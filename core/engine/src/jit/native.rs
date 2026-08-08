//! Native lowering for the first narrow baseline tier.
//!
//! This module intentionally has a small allowlist. The legacy shim compiler
//! remains the fallback for every code block that cannot be represented by the
//! native value model below.

use std::collections::{BTreeSet, HashMap};

use boa_ast::scope::BindingLocatorScope;

use crate::builtins::function::OrdinaryFunction;
use crate::object::internal_methods::InternalMethodCallContext;
use crate::object::shape::slot::SlotAttributes;
use crate::vm::{CodeBlock, IndexedKind, Instruction, InstructionIterator};
use crate::{Context, JsValue};

use super::{
    JIT_BREAK_BIT, JIT_GUARD_FAIL_BIT, JitBackend, JitCacheKey, JitCompileBlockerKind,
    JitEntryPoint, JitExit, JitExitKind, JitExitReason, JitModuleFailureStage,
    JitOsrRejectionReason, JitOsrRepresentation, MAX_FUNCTION_BYTECODE_INSTRUCTIONS,
};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{AbiParam, Block, InstBuilder, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{Linkage, Module};

/// Compile an ordinary numeric code block to native code.
///
/// The current native subset is deliberately conservative: all register
/// values are either `i32` or `f64` for a whole specialization, object/boxed
/// operations are rejected, and the VM stack is materialized only at
/// helper/exit boundaries. Returning `None` is a normal eligibility result;
/// the caller uses the legacy shim compiler.
pub(super) enum NativeCompileResult {
    Compiled {
        entry: extern "C" fn(*mut Context) -> u64,
        profile: NativeStaticProfile,
        code_bytes: usize,
    },
    Rejected(NativeRejection),
    ModuleFailure(JitModuleFailureStage),
}

/// Source-free static shape of a code block accepted by the native baseline.
///
/// These counters describe decoded bytecode, not runtime execution. They are
/// carried into opt-in diagnostics so admission policy can be calibrated from
/// browser workloads without retaining source text or property names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct NativeStaticProfile {
    pub(super) bytecode_instructions: u32,
    pub(super) backward_branches: u32,
    pub(super) call_instructions: u32,
    pub(super) property_instructions: u32,
}

pub(super) struct NativeRejection {
    pub(super) kind: JitCompileBlockerKind,
    pub(super) first_blocking_opcode: Option<crate::vm::Opcode>,
    pub(super) first_blocking_pc: Option<u32>,
    pub(super) supported_prefix_instructions: u32,
    pub(super) bytecode_instructions: u32,
}

/// Return the source-free static shape used by context-tier admission.
///
/// This performs no code generation. Unsupported or otherwise ineligible
/// bodies stay on the ordinary interpreter path; the private differential-test
/// seam retains its complete-semantics shim fallback.
pub(super) fn admission_profile(
    code: &CodeBlock,
    collect_diagnostic_metadata: bool,
) -> Result<NativeStaticProfile, NativeRejection> {
    if let Some(kind) = eligibility_blocker(code) {
        let bytecode_instructions = if collect_diagnostic_metadata {
            match decode(code, true, MAX_FUNCTION_BYTECODE_INSTRUCTIONS) {
                Ok(instructions) => instructions.instructions.len(),
                Err(rejection) => rejection.bytecode_instructions as usize,
            }
        } else {
            0
        };
        return Err(NativeRejection::new(
            kind,
            None,
            None,
            0,
            bytecode_instructions,
        ));
    }
    decode(
        code,
        collect_diagnostic_metadata,
        MAX_FUNCTION_BYTECODE_INSTRUCTIONS,
    )
    .map(|instructions| instructions.static_profile())
}

/// Apply the conservative static screen for a first loop-OSR candidate.
///
/// This intentionally does not perform live-state analysis or promise that a
/// nonzero-PC entry can be compiled. It answers the narrower profiling
/// question: whether the decoded range from an observed header through its
/// backedge contains only the Phase 1 numeric/control-flow subset and none of
/// the calls or property helpers excluded from the first OSR ABI review.
pub(super) fn loop_admission_profile(
    code: &CodeBlock,
    header_pc: u32,
    backedge_pc: u32,
) -> Result<NativeStaticProfile, NativeRejection> {
    if let Some(kind) = eligibility_blocker(code) {
        return Err(NativeRejection::new(kind, None, None, 0, 0));
    }

    let mut region = Vec::new();
    let mut found_header = false;
    let mut found_backedge = false;
    let mut iterator = InstructionIterator::new(&code.bytecode);
    while let Some((pc, opcode, instruction)) = iterator.next() {
        let pc = pc as u32;
        found_header |= pc == header_pc;
        found_backedge |= pc == backedge_pc;
        if pc >= header_pc && pc <= backedge_pc {
            region.push((pc as usize, iterator.pc(), instruction));
            let instruction = &region.last().expect("just pushed").2;
            let property_or_call = matches!(
                instruction,
                Instruction::Call { .. }
                    | Instruction::GetLengthProperty { .. }
                    | Instruction::GetPropertyByName { .. }
                    | Instruction::GetPropertyByNameWithThis { .. }
                    | Instruction::GetPropertyByValue { .. }
                    | Instruction::GetPropertyByValuePush { .. }
            );
            if property_or_call || !is_supported(code, opcode, instruction) {
                return Err(NativeRejection::new(
                    JitCompileBlockerKind::UnsupportedOpcode,
                    Some(opcode),
                    Some(pc),
                    region.len() - 1,
                    region.len(),
                ));
            }
        }
    }

    if !found_header || !found_backedge || region.is_empty() {
        return Err(NativeRejection::new(
            JitCompileBlockerKind::InvalidBranchTarget,
            None,
            Some(if found_header { backedge_pc } else { header_pc }),
            0,
            region.len(),
        ));
    }
    let (_, _, backedge) = region.last().expect("nonempty region");
    if branch_target(backedge) != Some(header_pc as usize) {
        return Err(NativeRejection::new(
            JitCompileBlockerKind::InvalidBranchTarget,
            Some(crate::vm::Opcode::decode(
                code.bytecode.bytes[backedge_pc as usize],
            )),
            Some(backedge_pc),
            region.len().saturating_sub(1),
            region.len(),
        ));
    }

    Ok(DecodedInstructions {
        pc_to_index: region
            .iter()
            .enumerate()
            .map(|(index, (pc, _, _))| (*pc, index))
            .collect(),
        instructions: region,
    }
    .static_profile())
}

const MAX_LOOP_REGION_INSTRUCTIONS: usize = 128;
const MAX_LOOP_CONTINUATION_INSTRUCTIONS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoopPlanRejection {
    IneligibleCodeBlock(JitCompileBlockerKind),
    InvalidBoundary,
    RegionTooLarge,
    UnsupportedRegionOpcode,
    InvalidControlFlow,
    UnsupportedContinuation,
    RepresentationMismatch,
    UnprovenValue,
}

impl From<LoopPlanRejection> for JitOsrRejectionReason {
    fn from(reason: LoopPlanRejection) -> Self {
        match reason {
            LoopPlanRejection::IneligibleCodeBlock(_) => Self::IneligibleCodeBlock,
            LoopPlanRejection::InvalidBoundary => Self::InvalidBoundary,
            LoopPlanRejection::RegionTooLarge => Self::RegionTooLarge,
            LoopPlanRejection::UnsupportedRegionOpcode => Self::UnsupportedRegionOpcode,
            LoopPlanRejection::InvalidControlFlow => Self::InvalidControlFlow,
            LoopPlanRejection::UnsupportedContinuation => Self::UnsupportedContinuation,
            LoopPlanRejection::RepresentationMismatch => Self::RepresentationMismatch,
            LoopPlanRejection::UnprovenValue => Self::UnprovenValue,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoopEntrySource {
    VmRegister,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoopExitSource {
    NativeValue,
    PreservedVmValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LoopEntryValue {
    pub(super) register: u32,
    pub(super) representation: JitOsrRepresentation,
    pub(super) source: LoopEntrySource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LoopExitValue {
    pub(super) register: u32,
    pub(super) source: LoopExitSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LoopExitMap {
    pub(super) from_pc: u32,
    pub(super) resume_pc: u32,
    pub(super) materialize: Vec<LoopExitValue>,
}

/// A source-free, side-effect-free proof for one canonical numeric loop.
///
/// This is deliberately not executable yet. The scheduler and compiler slices
/// consume this immutable plan only after the planner has proved every entry
/// value and the single path-specific continuation map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LoopRegionPlan {
    pub(super) key: JitCacheKey,
    pub(super) instruction_pcs: Vec<u32>,
    pub(super) entry: Vec<LoopEntryValue>,
    pub(super) exits: Vec<LoopExitMap>,
    /// Per region instruction, the registers a mid-region exit must write back
    /// to the VM frame before the interpreter resumes at that same bytecode.
    ///
    /// Every other register is deliberately left alone: it either holds no
    /// native definition on any path that reaches the instruction, or it is
    /// dead there, so the VM frame already owns the value the interpreter will
    /// observe. Writing those registers would replace live non-numeric frame
    /// values with a numeric zero.
    pub(super) available: Vec<Vec<u32>>,
    pub(super) requires_f64: bool,
}

#[derive(Clone)]
struct LoopDecodedInstruction {
    pc: usize,
    next_pc: usize,
    instruction: Instruction,
}

/// Prove the first loop-OSR shape without compiling or touching VM state.
///
/// The first shape has one unconditional canonical latch and one conditional
/// forward continuation. Its continuation is intentionally restricted to the
/// bytecompiler's register-to-return epilogue so liveness cannot silently
/// guess at an unmodelled opcode.
pub(super) fn plan_loop_region(
    code: &CodeBlock,
    header_pc: u32,
    backedge_pc: u32,
    representation: JitOsrRepresentation,
    budgeted: bool,
    diagnostic: bool,
) -> Result<LoopRegionPlan, LoopPlanRejection> {
    if let Some(kind) = eligibility_blocker(code) {
        return Err(LoopPlanRejection::IneligibleCodeBlock(kind));
    }
    if header_pc >= backedge_pc {
        return Err(LoopPlanRejection::InvalidBoundary);
    }

    let region = decode_loop_region(code, header_pc, backedge_pc)?;
    let by_pc: HashMap<usize, usize> = region
        .iter()
        .enumerate()
        .map(|(index, instruction)| (instruction.pc, index))
        .collect();
    if !matches!(
        &region.last().ok_or(LoopPlanRejection::InvalidBoundary)?.instruction,
        Instruction::Jump { address } if address.as_u32() == header_pc
    ) {
        return Err(LoopPlanRejection::InvalidControlFlow);
    }

    let mut external_exit = None;
    let mut requires_f64 = false;
    let mut uses = Vec::with_capacity(region.len());
    let mut defs = Vec::with_capacity(region.len());
    let mut successors = Vec::with_capacity(region.len());

    for (region_index, decoded_instruction) in region.iter().enumerate() {
        let (instruction_uses, instruction_def) = loop_use_def(
            &decoded_instruction.instruction,
            code.register_count as usize,
        )?;
        requires_f64 |= matches!(
            decoded_instruction.instruction,
            Instruction::StoreFloat { .. } | Instruction::StoreDouble { .. }
        );
        uses.push(instruction_uses);
        defs.push(instruction_def);

        let mut instruction_successors = Vec::with_capacity(2);
        match &decoded_instruction.instruction {
            Instruction::Jump { address } => {
                if decoded_instruction.pc != backedge_pc as usize || address.as_u32() != header_pc {
                    return Err(LoopPlanRejection::InvalidControlFlow);
                }
                instruction_successors.push(0);
            }
            instruction if is_loop_conditional_branch(instruction) => {
                let target_pc =
                    branch_target(instruction).ok_or(LoopPlanRejection::InvalidControlFlow)?;
                let fallthrough = by_pc
                    .get(&decoded_instruction.next_pc)
                    .copied()
                    .ok_or(LoopPlanRejection::InvalidControlFlow)?;
                if let Some(&target) = by_pc.get(&target_pc) {
                    if target <= region_index {
                        return Err(LoopPlanRejection::InvalidControlFlow);
                    }
                    instruction_successors.push(target);
                } else {
                    if target_pc <= backedge_pc as usize || external_exit.is_some() {
                        return Err(LoopPlanRejection::InvalidControlFlow);
                    }
                    external_exit = Some((decoded_instruction.pc, target_pc));
                }
                instruction_successors.push(fallthrough);
            }
            _ => {
                let next = by_pc
                    .get(&decoded_instruction.next_pc)
                    .copied()
                    .ok_or(LoopPlanRejection::InvalidControlFlow)?;
                instruction_successors.push(next);
            }
        }
        successors.push(instruction_successors);
    }

    let (exit_from_pc, resume_pc) = external_exit.ok_or(LoopPlanRejection::InvalidControlFlow)?;
    if requires_f64 && representation != JitOsrRepresentation::F64 {
        return Err(LoopPlanRejection::RepresentationMismatch);
    }
    let exit_live = continuation_live_in(code, resume_pc, code.register_count as usize)?;

    let exit_instruction_index = *by_pc
        .get(&exit_from_pc)
        .ok_or(LoopPlanRejection::InvalidControlFlow)?;
    let mut live_in = vec![BTreeSet::new(); region.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for index in (0..region.len()).rev() {
            let mut live_out = BTreeSet::new();
            for successor in &successors[index] {
                live_out.extend(live_in[*successor].iter().copied());
            }
            if index == exit_instruction_index {
                live_out.extend(exit_live.iter().copied());
            }
            if let Some(definition) = defs[index] {
                live_out.remove(&definition);
            }
            live_out.extend(uses[index].iter().copied());
            if live_out != live_in[index] {
                live_in[index] = live_out;
                changed = true;
            }
        }
    }

    let used_or_defined: BTreeSet<usize> = uses
        .iter()
        .flat_map(|registers| registers.iter().copied())
        .chain(defs.iter().flatten().copied())
        .collect();
    let defined: BTreeSet<usize> = defs.iter().flatten().copied().collect();
    let entry_registers: BTreeSet<usize> =
        live_in[0].intersection(&used_or_defined).copied().collect();

    let entry = entry_registers
        .iter()
        .map(|register| LoopEntryValue {
            register: *register as u32,
            representation,
            source: LoopEntrySource::VmRegister,
        })
        .collect();
    let mut materialize = Vec::with_capacity(exit_live.len());
    for register in exit_live {
        let source = if defined.contains(&register) {
            if !live_in[exit_instruction_index].contains(&register)
                || (!entry_registers.contains(&register)
                    && !definitely_defined_before(
                        &successors,
                        &defs,
                        exit_instruction_index,
                        register,
                    ))
            {
                return Err(LoopPlanRejection::UnprovenValue);
            }
            LoopExitSource::NativeValue
        } else {
            LoopExitSource::PreservedVmValue
        };
        materialize.push(LoopExitValue {
            register: register as u32,
            source,
        });
    }

    // A mid-region exit resumes the interpreter at the very bytecode it left,
    // so the only registers it may write back are the ones that are live there
    // *and* carry a native definition. A register that is live at the exit but
    // never defined by the region has no native definition to write; a register
    // the region defines but that is dead at the exit has nothing the
    // interpreter can observe. Both are left in the VM frame untouched.
    //
    // Every register in this set provably has a definition reaching the
    // instruction: if it were only defined on some paths, the undefined path
    // would make it live at the region entry, which would have placed it in
    // `entry_registers` and given it a guarded prologue load.
    let available = live_in
        .iter()
        .map(|live| {
            live.iter()
                .filter(|register| entry_registers.contains(register) || defined.contains(register))
                .map(|register| *register as u32)
                .collect()
        })
        .collect();

    Ok(LoopRegionPlan {
        key: JitCacheKey::loop_region(
            code.debug_id,
            header_pc,
            backedge_pc,
            representation,
            budgeted,
            diagnostic,
        ),
        instruction_pcs: region
            .iter()
            .map(|instruction| instruction.pc as u32)
            .collect(),
        entry,
        exits: vec![LoopExitMap {
            from_pc: exit_from_pc as u32,
            resume_pc: resume_pc as u32,
            materialize,
        }],
        available,
        requires_f64,
    })
}

#[derive(Clone, Copy)]
pub(super) struct CompiledLoopRegion {
    pub(super) entry: extern "C" fn(*mut Context) -> u64,
    pub(super) code_bytes: usize,
}

// returned once per loop-region compile attempt, not stored in bulk, so the
// larger `Compiled` variant doesn't cost anything worth boxing for
#[allow(variant_size_differences)]
pub(super) enum LoopNativeCompileResult {
    Compiled(CompiledLoopRegion),
    Rejected(JitOsrRejectionReason),
    ModuleFailure(JitModuleFailureStage),
}

/// Compile one already-proven loop region for the post-backedge scheduler.
///
/// The immutable plan is revalidated against the live `CodeBlock` before any
/// Cranelift function is declared. A failure returns a bounded source-free
/// reason and never exposes a callable entry.
pub(super) fn compile_loop_region(
    backend: &mut JitBackend,
    code: &CodeBlock,
    plan: &LoopRegionPlan,
) -> LoopNativeCompileResult {
    let JitEntryPoint::Loop {
        header_pc,
        backedge_pc,
        representation,
    } = plan.key.entry_point
    else {
        return LoopNativeCompileResult::Rejected(JitOsrRejectionReason::Lowering);
    };
    let expected = match plan_loop_region(
        code,
        header_pc,
        backedge_pc,
        representation,
        plan.key.budgeted,
        plan.key.diagnostic,
    ) {
        Ok(expected) => expected,
        Err(reason) => return LoopNativeCompileResult::Rejected(reason.into()),
    };
    if &expected != plan {
        return LoopNativeCompileResult::Rejected(JitOsrRejectionReason::Lowering);
    }

    let region = match decode_loop_region(code, header_pc, backedge_pc) {
        Ok(region) => region,
        Err(reason) => return LoopNativeCompileResult::Rejected(reason.into()),
    };
    let mode = match representation {
        JitOsrRepresentation::I32 if !plan.requires_f64 => NativeMode::I32,
        JitOsrRepresentation::F64 => NativeMode::F64,
        JitOsrRepresentation::I32 => {
            return LoopNativeCompileResult::Rejected(
                JitOsrRejectionReason::RepresentationMismatch,
            );
        }
    };
    let mut compiler = LoopRegionCompiler::new(backend, code, plan, region, mode);
    match compiler.compile() {
        Ok(Some(artifact)) => LoopNativeCompileResult::Compiled(artifact),
        Ok(None) => LoopNativeCompileResult::Rejected(JitOsrRejectionReason::Lowering),
        Err(stage) => LoopNativeCompileResult::ModuleFailure(stage),
    }
}

fn decode_loop_region(
    code: &CodeBlock,
    header_pc: u32,
    backedge_pc: u32,
) -> Result<Vec<LoopDecodedInstruction>, LoopPlanRejection> {
    let mut region = Vec::new();
    let mut iterator = InstructionIterator::new(&code.bytecode);
    while let Some((pc, _, instruction)) = iterator.next() {
        if pc > backedge_pc as usize {
            break;
        }
        if pc >= header_pc as usize {
            if region.len() == MAX_LOOP_REGION_INSTRUCTIONS {
                return Err(LoopPlanRejection::RegionTooLarge);
            }
            region.push(LoopDecodedInstruction {
                pc,
                next_pc: iterator.pc(),
                instruction,
            });
        }
    }
    if region.first().map(|instruction| instruction.pc) != Some(header_pc as usize)
        || region.last().map(|instruction| instruction.pc) != Some(backedge_pc as usize)
    {
        return Err(LoopPlanRejection::InvalidBoundary);
    }
    Ok(region)
}

fn loop_use_def(
    instruction: &Instruction,
    register_count: usize,
) -> Result<(Vec<usize>, Option<usize>), LoopPlanRejection> {
    let register = |value: usize| {
        (value < register_count)
            .then_some(value)
            .ok_or(LoopPlanRejection::UnprovenValue)
    };
    let unary = |src: usize, dst: usize| Ok((vec![register(src)?], Some(register(dst)?)));
    let binary = |lhs: usize, rhs: usize, dst: usize| {
        Ok((vec![register(lhs)?, register(rhs)?], Some(register(dst)?)))
    };
    let comparison = |lhs: usize, rhs: usize| Ok((vec![register(lhs)?, register(rhs)?], None));

    match instruction {
        Instruction::StoreZero { dst }
        | Instruction::StoreOne { dst }
        | Instruction::StoreInt8 { dst, .. }
        | Instruction::StoreInt16 { dst, .. }
        | Instruction::StoreInt32 { dst, .. }
        | Instruction::StoreFloat { dst, .. }
        | Instruction::StoreDouble { dst, .. } => {
            Ok((Vec::new(), Some(register(usize::from(*dst))?)))
        }
        Instruction::Move { src, dst } | Instruction::Inc { src, dst } => {
            unary(usize::from(*src), usize::from(*dst))
        }
        Instruction::Add { lhs, rhs, dst }
        | Instruction::Sub { lhs, rhs, dst }
        | Instruction::Mul { lhs, rhs, dst } => {
            binary(usize::from(*lhs), usize::from(*rhs), usize::from(*dst))
        }
        Instruction::JumpIfNotLessThan { lhs, rhs, .. }
        | Instruction::JumpIfNotLessThanOrEqual { lhs, rhs, .. }
        | Instruction::JumpIfNotGreaterThan { lhs, rhs, .. }
        | Instruction::JumpIfNotGreaterThanOrEqual { lhs, rhs, .. }
        | Instruction::JumpIfNotEqual { lhs, rhs, .. } => {
            comparison(usize::from(*lhs), usize::from(*rhs))
        }
        Instruction::IncrementLoopIteration | Instruction::Jump { .. } => Ok((Vec::new(), None)),
        _ => Err(LoopPlanRejection::UnsupportedRegionOpcode),
    }
}

fn is_loop_conditional_branch(instruction: &Instruction) -> bool {
    matches!(
        instruction,
        Instruction::JumpIfNotLessThan { .. }
            | Instruction::JumpIfNotLessThanOrEqual { .. }
            | Instruction::JumpIfNotGreaterThan { .. }
            | Instruction::JumpIfNotGreaterThanOrEqual { .. }
            | Instruction::JumpIfNotEqual { .. }
    )
}

fn continuation_live_in(
    code: &CodeBlock,
    resume_pc: usize,
    register_count: usize,
) -> Result<BTreeSet<usize>, LoopPlanRejection> {
    let mut epilogue = Vec::new();
    let mut found_resume = false;
    let mut found_return = false;
    for (pc, _, instruction) in InstructionIterator::new(&code.bytecode) {
        if !found_resume {
            if pc < resume_pc {
                continue;
            }
            if pc != resume_pc {
                return Err(LoopPlanRejection::UnsupportedContinuation);
            }
            found_resume = true;
        }
        if epilogue.len() == MAX_LOOP_CONTINUATION_INSTRUCTIONS {
            return Err(LoopPlanRejection::UnsupportedContinuation);
        }
        let (uses, definition, returns) = match &instruction {
            Instruction::PushFromRegister { src } | Instruction::SetAccumulator { src } => {
                (vec![usize::from(*src)], None, false)
            }
            Instruction::PopIntoRegister { dst } => (Vec::new(), Some(usize::from(*dst)), false),
            Instruction::CheckReturn => (Vec::new(), None, false),
            Instruction::Return => (Vec::new(), None, true),
            _ => return Err(LoopPlanRejection::UnsupportedContinuation),
        };
        if uses.iter().any(|register| *register >= register_count)
            || definition.is_some_and(|register| register >= register_count)
        {
            return Err(LoopPlanRejection::UnprovenValue);
        }
        epilogue.push((uses, definition));
        if returns {
            found_return = true;
            break;
        }
    }
    if !found_resume || !found_return {
        return Err(LoopPlanRejection::UnsupportedContinuation);
    }

    let mut live = BTreeSet::new();
    for (uses, definition) in epilogue.into_iter().rev() {
        if let Some(definition) = definition {
            live.remove(&definition);
        }
        live.extend(uses);
    }
    Ok(live)
}

fn definitely_defined_before(
    successors: &[Vec<usize>],
    defs: &[Option<usize>],
    exit_index: usize,
    register: usize,
) -> bool {
    let mut definitely_defined = vec![false; successors.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for index in 0..successors.len() {
            let incoming = if index == 0 {
                false
            } else {
                let predecessors: Vec<usize> = successors
                    .iter()
                    .enumerate()
                    .filter_map(|(predecessor, targets)| {
                        targets.contains(&index).then_some(predecessor)
                    })
                    .collect();
                !predecessors.is_empty()
                    && predecessors
                        .iter()
                        .all(|predecessor| definitely_defined[*predecessor])
            };
            let after = incoming || defs[index] == Some(register);
            if after != definitely_defined[index] {
                definitely_defined[index] = after;
                changed = true;
            }
        }
    }
    definitely_defined.get(exit_index).copied().unwrap_or(false)
}

impl NativeRejection {
    fn new(
        kind: JitCompileBlockerKind,
        first_blocking_opcode: Option<crate::vm::Opcode>,
        first_blocking_pc: Option<u32>,
        supported_prefix_instructions: usize,
        bytecode_instructions: usize,
    ) -> Self {
        Self {
            kind,
            first_blocking_opcode,
            first_blocking_pc,
            supported_prefix_instructions: supported_prefix_instructions as u32,
            bytecode_instructions: bytecode_instructions as u32,
        }
    }
}

pub(super) fn compile(
    backend: &mut JitBackend,
    code: &CodeBlock,
    charge_instruction_budget: bool,
    collect_diagnostic_metadata: bool,
    instrument_storage: bool,
) -> NativeCompileResult {
    let eligibility_blocker = eligibility_blocker(code);
    if let Some(kind) = eligibility_blocker {
        let bytecode_instructions = if collect_diagnostic_metadata {
            match decode(code, true, MAX_FUNCTION_BYTECODE_INSTRUCTIONS) {
                Ok(instructions) => instructions.instructions.len(),
                Err(rejection) => rejection.bytecode_instructions as usize,
            }
        } else {
            0
        };
        return NativeCompileResult::Rejected(NativeRejection::new(
            kind,
            None,
            None,
            0,
            bytecode_instructions,
        ));
    }

    let instructions = match decode(
        code,
        collect_diagnostic_metadata,
        MAX_FUNCTION_BYTECODE_INSTRUCTIONS,
    ) {
        Ok(instructions) => instructions,
        Err(rejection) => return NativeCompileResult::Rejected(rejection),
    };
    let bytecode_instructions = instructions.instructions.len();
    let profile = instructions.static_profile();
    let mode = select_mode(&instructions);
    let Some(mut compiler) = NativeCompiler::new(
        backend,
        code,
        instructions,
        mode,
        charge_instruction_budget,
        instrument_storage,
    ) else {
        return NativeCompileResult::Rejected(NativeRejection::new(
            JitCompileBlockerKind::RegisterAnalysis,
            None,
            None,
            0,
            bytecode_instructions,
        ));
    };
    match compiler.compile() {
        Ok(Some((entry, code_bytes))) => NativeCompileResult::Compiled {
            entry,
            profile,
            code_bytes,
        },
        Ok(None) => {
            let (pc, opcode) = compiler.current_instruction_identity();
            NativeCompileResult::Rejected(NativeRejection::new(
                JitCompileBlockerKind::Lowering,
                opcode,
                pc,
                compiler.current_instruction,
                bytecode_instructions,
            ))
        }
        Err(stage) => NativeCompileResult::ModuleFailure(stage),
    }
}

struct DecodedInstructions {
    instructions: Vec<(usize, usize, Instruction)>,
    pc_to_index: HashMap<usize, usize>,
}

impl DecodedInstructions {
    fn static_profile(&self) -> NativeStaticProfile {
        let mut profile = NativeStaticProfile {
            bytecode_instructions: self.instructions.len() as u32,
            ..NativeStaticProfile::default()
        };

        for (pc, _, instruction) in &self.instructions {
            if branch_target(instruction).is_some_and(|target| target < *pc) {
                profile.backward_branches = profile.backward_branches.saturating_add(1);
            }
            if matches!(instruction, Instruction::Call { .. }) {
                profile.call_instructions = profile.call_instructions.saturating_add(1);
            }
            if matches!(
                instruction,
                Instruction::GetLengthProperty { .. }
                    | Instruction::GetPropertyByName { .. }
                    | Instruction::GetPropertyByNameWithThis { .. }
                    | Instruction::GetPropertyByValue { .. }
                    | Instruction::GetPropertyByValuePush { .. }
            ) {
                profile.property_instructions = profile.property_instructions.saturating_add(1);
            }
        }

        profile
    }
}

fn decode(
    code: &CodeBlock,
    collect_diagnostic_metadata: bool,
    instruction_limit: usize,
) -> Result<DecodedInstructions, NativeRejection> {
    let mut instructions = Vec::new();
    let mut pc_to_index = HashMap::new();
    let mut iterator = InstructionIterator::new(&code.bytecode);
    let mut first_unsupported = None;

    while let Some((pc, opcode, instruction)) = iterator.next() {
        if instructions.len() >= instruction_limit {
            return Err(NativeRejection::new(
                JitCompileBlockerKind::InstructionLimit,
                Some(opcode),
                Some(pc as u32),
                instructions.len(),
                instructions.len().saturating_add(1),
            ));
        }
        if pc_to_index.insert(pc, instructions.len()).is_some() {
            return Err(NativeRejection::new(
                JitCompileBlockerKind::DuplicateInstructionBoundary,
                Some(opcode),
                Some(pc as u32),
                instructions.len(),
                instructions.len(),
            ));
        }
        instructions.push((pc, iterator.pc(), instruction));

        if !is_supported(code, opcode, &instructions.last().expect("just pushed").2) {
            let supported_prefix = instructions.len() - 1;
            if !collect_diagnostic_metadata {
                return Err(NativeRejection::new(
                    JitCompileBlockerKind::UnsupportedOpcode,
                    Some(opcode),
                    Some(pc as u32),
                    supported_prefix,
                    instructions.len(),
                ));
            }
            if first_unsupported.is_none() {
                first_unsupported = Some((pc, opcode, supported_prefix));
            }
        }
    }

    if instructions.is_empty() {
        return Err(NativeRejection::new(
            JitCompileBlockerKind::EmptyCodeBlock,
            None,
            None,
            0,
            0,
        ));
    }

    if let Some((pc, opcode, supported_prefix)) = first_unsupported {
        return Err(NativeRejection::new(
            JitCompileBlockerKind::UnsupportedOpcode,
            Some(opcode),
            Some(pc as u32),
            supported_prefix,
            instructions.len(),
        ));
    }

    // All branch targets must land on decoded instruction boundaries. This is
    // an allowlist check rather than an assumption about the current opcode
    // table; an omitted branch variant rejects native compilation safely.
    for (pc, _, instruction) in &instructions {
        if let Some(target) = branch_target(instruction)
            && !pc_to_index.contains_key(&target)
        {
            let opcode = crate::vm::Opcode::decode(code.bytecode.bytes[*pc]);
            return Err(NativeRejection::new(
                JitCompileBlockerKind::InvalidBranchTarget,
                Some(opcode),
                Some(*pc as u32),
                instructions.len(),
                instructions.len(),
            ));
        }
    }

    Ok(DecodedInstructions {
        instructions,
        pc_to_index,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeMode {
    I32,
    F64,
}

impl NativeMode {
    fn value_type(self) -> cranelift_codegen::ir::Type {
        match self {
            Self::I32 => types::I32,
            Self::F64 => types::F64,
        }
    }
}

fn select_mode(instructions: &DecodedInstructions) -> NativeMode {
    if instructions.instructions.iter().any(|(_, _, instruction)| {
        matches!(
            instruction,
            Instruction::StoreFloat { .. } | Instruction::StoreDouble { .. }
        )
    }) {
        NativeMode::F64
    } else {
        NativeMode::I32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterKind {
    Numeric,
    Boxed,
}

#[derive(Clone, Copy, Debug)]
struct RegisterDefinition {
    source: Option<usize>,
    kind: RegisterKind,
}

struct RegisterAnalysis {
    before: Vec<Vec<RegisterKind>>,
    after: Vec<Vec<RegisterKind>>,
    /// The single register each instruction rewrites, if any. Recorded here so
    /// the definedness bookkeeping in the compiler cannot drift from
    /// `output_definition`.
    targets: Vec<Option<usize>>,
}

fn analyze_registers(
    instructions: &DecodedInstructions,
    register_count: usize,
) -> Option<RegisterAnalysis> {
    // Register numbers are reused by the bytecode allocator. Track the
    // current definition instead of assigning one type to the whole register
    // number; otherwise an object argument can incorrectly poison a later
    // integer definition in the same register.
    let mut definitions: Vec<RegisterDefinition> = (0..register_count)
        .map(|_| RegisterDefinition {
            source: None,
            kind: RegisterKind::Numeric,
        })
        .collect();
    let mut current: Vec<usize> = (0..register_count).collect();
    let mut before_ids = Vec::with_capacity(instructions.instructions.len());
    let mut after_ids = Vec::with_capacity(instructions.instructions.len());
    let mut targets = Vec::with_capacity(instructions.instructions.len());
    let call_pushes = call_push_operands(instructions);

    for (index, (_, _, instruction)) in instructions.instructions.iter().enumerate() {
        before_ids.push(current.clone());

        for register in object_operands(instruction) {
            let definition = current.get(register).copied()?;
            mark_definition(definition, &mut definitions);
        }
        if call_pushes.contains(&index)
            && let Instruction::PushFromRegister { src } = instruction
        {
            let definition = current.get(usize::from(*src)).copied()?;
            mark_definition(definition, &mut definitions);
        }

        let mut target = None;
        if let Some((register, source, kind)) =
            output_definition(instruction, &current, &definitions)
        {
            if register >= current.len() {
                return None;
            }
            let definition = definitions.len();
            definitions.push(RegisterDefinition { source, kind });
            if let Some(current_definition) = current.get_mut(register) {
                *current_definition = definition;
            }
            target = Some(register);
        }
        targets.push(target);

        after_ids.push(current.clone());
    }

    let kinds = |ids: &[usize]| {
        ids.iter()
            .map(|definition| {
                definitions
                    .get(*definition)
                    .map_or(RegisterKind::Boxed, |definition| definition.kind)
            })
            .collect()
    };

    Some(RegisterAnalysis {
        before: before_ids.iter().map(|ids| kinds(ids)).collect(),
        after: after_ids.iter().map(|ids| kinds(ids)).collect(),
        targets,
    })
}

fn mark_definition(definition: usize, definitions: &mut [RegisterDefinition]) {
    let Some(definition_info) = definitions.get_mut(definition) else {
        return;
    };
    if definition_info.kind == RegisterKind::Boxed {
        return;
    }
    definition_info.kind = RegisterKind::Boxed;
    if let Some(source) = definition_info.source {
        mark_definition(source, definitions);
    }
}

fn object_operands(instruction: &Instruction) -> Vec<usize> {
    match instruction {
        Instruction::GetLengthProperty { value, .. }
        | Instruction::GetPropertyByName { value, .. } => vec![usize::from(*value)],
        Instruction::GetPropertyByNameWithThis {
            receiver, value, ..
        } => vec![usize::from(*receiver), usize::from(*value)],
        Instruction::GetPropertyByValue {
            receiver, object, ..
        }
        | Instruction::GetPropertyByValuePush {
            receiver, object, ..
        } => vec![usize::from(*receiver), usize::from(*object)],
        _ => Vec::new(),
    }
}

fn call_push_operands(instructions: &DecodedInstructions) -> BTreeSet<usize> {
    let mut call_pushes = BTreeSet::new();
    for (index, (_, _, instruction)) in instructions.instructions.iter().enumerate() {
        if !matches!(instruction, Instruction::Call { .. }) {
            continue;
        }

        let mut previous = index;
        while let Some(push_index) = previous.checked_sub(1) {
            let Instruction::PushFromRegister { .. } = &instructions.instructions[push_index].2
            else {
                break;
            };
            call_pushes.insert(push_index);
            previous = push_index;
        }
    }
    call_pushes
}

fn output_definition(
    instruction: &Instruction,
    current: &[usize],
    definitions: &[RegisterDefinition],
) -> Option<(usize, Option<usize>, RegisterKind)> {
    let numeric = |register: usize| (register, None, RegisterKind::Numeric);
    let boxed = |register: usize| (register, None, RegisterKind::Boxed);
    let moved = |dst: usize, src: usize| {
        let kind = definitions
            .get(current.get(src).copied().unwrap_or(usize::MAX))
            .map_or(RegisterKind::Boxed, |definition| definition.kind);
        (dst, current.get(src).copied(), kind)
    };

    match instruction {
        Instruction::GetArgument { dst, .. }
        | Instruction::GetName { dst, .. }
        | Instruction::StoreZero { dst }
        | Instruction::StoreOne { dst }
        | Instruction::StoreInt8 { dst, .. }
        | Instruction::StoreInt16 { dst, .. }
        | Instruction::StoreInt32 { dst, .. }
        | Instruction::StoreFloat { dst, .. }
        | Instruction::StoreDouble { dst, .. }
        | Instruction::Add { dst, .. }
        | Instruction::Sub { dst, .. }
        | Instruction::Div { dst, .. }
        | Instruction::Mul { dst, .. }
        | Instruction::Inc { dst, .. }
        | Instruction::PopIntoRegister { dst }
        | Instruction::GetPropertyByName { dst, .. }
        | Instruction::GetLengthProperty { dst, .. }
        | Instruction::GetPropertyByNameWithThis { dst, .. }
        | Instruction::GetPropertyByValue { dst, .. }
        | Instruction::GetPropertyByValuePush { dst, .. } => Some(numeric(usize::from(*dst))),
        Instruction::Move { dst, src } => Some(moved(usize::from(*dst), usize::from(*src))),
        Instruction::GetFunction { dst, .. } | Instruction::StoreNewArray { dst } => {
            Some(boxed(usize::from(*dst)))
        }
        _ => None,
    }
}

fn eligibility_blocker(code: &CodeBlock) -> Option<JitCompileBlockerKind> {
    if !code.is_ordinary() {
        Some(JitCompileBlockerKind::FunctionKind)
    } else if !code.handlers.is_empty() {
        Some(JitCompileBlockerKind::ExceptionHandlers)
    } else if code.register_count > 128 {
        Some(JitCompileBlockerKind::RegisterLimit)
    } else {
        None
    }
}

fn is_supported(code: &CodeBlock, opcode: crate::vm::Opcode, instruction: &Instruction) -> bool {
    // Keep the opcode argument in the check so a future enum/decoder mismatch
    // cannot accidentally make an instruction look native by its fields alone.
    use crate::vm::Opcode;

    match (opcode, instruction) {
        (Opcode::GetName, Instruction::GetName { binding_index, .. }) => code
            .bindings
            .get(usize::from(*binding_index))
            .is_some_and(|binding| binding.scope() == BindingLocatorScope::GlobalDeclarative),
        (Opcode::GetArgument, Instruction::GetArgument { .. })
        | (Opcode::StoreZero, Instruction::StoreZero { .. })
        | (Opcode::StoreOne, Instruction::StoreOne { .. })
        | (Opcode::StoreInt8, Instruction::StoreInt8 { .. })
        | (Opcode::StoreInt16, Instruction::StoreInt16 { .. })
        | (Opcode::StoreInt32, Instruction::StoreInt32 { .. })
        | (Opcode::StoreFloat, Instruction::StoreFloat { .. })
        | (Opcode::StoreDouble, Instruction::StoreDouble { .. })
        | (Opcode::Move, Instruction::Move { .. })
        | (Opcode::GetPropertyByName, Instruction::GetPropertyByName { .. })
        | (Opcode::GetPropertyByNameWithThis, Instruction::GetPropertyByNameWithThis { .. })
        | (Opcode::GetPropertyByValue, Instruction::GetPropertyByValue { .. })
        | (Opcode::Call, Instruction::Call { .. })
        | (Opcode::Add, Instruction::Add { .. })
        | (Opcode::Sub, Instruction::Sub { .. })
        | (Opcode::Div, Instruction::Div { .. })
        | (Opcode::Mul, Instruction::Mul { .. })
        | (Opcode::Inc, Instruction::Inc { .. })
        | (Opcode::Jump, Instruction::Jump { .. })
        | (Opcode::JumpIfNotLessThan, Instruction::JumpIfNotLessThan { .. })
        | (Opcode::JumpIfNotLessThanOrEqual, Instruction::JumpIfNotLessThanOrEqual { .. })
        | (Opcode::JumpIfNotGreaterThan, Instruction::JumpIfNotGreaterThan { .. })
        | (Opcode::JumpIfNotGreaterThanOrEqual, Instruction::JumpIfNotGreaterThanOrEqual { .. })
        | (Opcode::JumpIfNotEqual, Instruction::JumpIfNotEqual { .. })
        | (Opcode::IncrementLoopIteration, Instruction::IncrementLoopIteration)
        | (Opcode::PushFromRegister, Instruction::PushFromRegister { .. })
        | (Opcode::PopIntoRegister, Instruction::PopIntoRegister { .. })
        | (Opcode::SetAccumulator, Instruction::SetAccumulator { .. })
        | (Opcode::CheckReturn, Instruction::CheckReturn)
        | (Opcode::Return, Instruction::Return) => true,
        _ => false,
    }
}

fn branch_target(instruction: &Instruction) -> Option<usize> {
    match instruction {
        Instruction::Jump { address }
        | Instruction::JumpIfNotLessThan { address, .. }
        | Instruction::JumpIfNotLessThanOrEqual { address, .. }
        | Instruction::JumpIfNotGreaterThan { address, .. }
        | Instruction::JumpIfNotGreaterThanOrEqual { address, .. }
        | Instruction::JumpIfNotEqual { address, .. } => Some(address.as_u32() as usize),
        _ => None,
    }
}

fn fallthrough(instruction: &Instruction) -> bool {
    !matches!(
        instruction,
        Instruction::Jump { .. } | Instruction::Call { .. } | Instruction::Return
    )
}

fn has_explicit_edges(instruction: &Instruction) -> bool {
    matches!(
        instruction,
        Instruction::Jump { .. }
            | Instruction::JumpIfNotLessThan { .. }
            | Instruction::JumpIfNotLessThanOrEqual { .. }
            | Instruction::JumpIfNotGreaterThan { .. }
            | Instruction::JumpIfNotGreaterThanOrEqual { .. }
            | Instruction::JumpIfNotEqual { .. }
            | Instruction::Call { .. }
            | Instruction::Return
    )
}

#[derive(Clone, Copy)]
struct Helper {
    address: usize,
    signature: cranelift_codegen::ir::SigRef,
}

struct Helpers {
    ptr: cranelift_codegen::ir::Type,
    guard: Helper,
    copy_global_declarative_binding_register: Helper,
    guard_argument_number: Helper,
    guard_stack_number: Helper,
    copy_argument_register: Helper,
    copy_register: Helper,
    get_register_i32: Helper,
    get_register_f64: Helper,
    push_register: Helper,
    set_return_register: Helper,
    get_argument_i32: Helper,
    get_argument_f64: Helper,
    dense_guard: Helper,
    dense_guard_f64: Helper,
    dense_i32: Helper,
    dense_f64: Helper,
    named_guard: Helper,
    named_i32: Helper,
    named_f64: Helper,
    call_ordinary: Helper,
    set_pc: Helper,
    store_i32_if_defined: Helper,
    store_f64_if_defined: Helper,
    push_i32: Helper,
    push_f64: Helper,
    pop_i32: Helper,
    pop_f64: Helper,
    set_return_i32: Helper,
    set_return_f64: Helper,
    increment_loop: Helper,
    handle_return: Helper,
    consume_instruction_budget: Helper,
    refund_instruction_budget: Helper,
}

struct NativeCompiler<'a> {
    backend: &'a mut JitBackend,
    code: &'a CodeBlock,
    instructions: DecodedInstructions,
    mode: NativeMode,
    analysis: RegisterAnalysis,
    current_instruction: usize,
    variables: Vec<Variable>,
    /// One companion per register, carrying whether `variables` holds the
    /// register's current value on the path being executed.
    defined_flags: Vec<Variable>,
    flag_defined: Option<cranelift_codegen::ir::Value>,
    flag_undefined: Option<cranelift_codegen::ir::Value>,
    dirty: BTreeSet<usize>,
    charge_instruction_budget: bool,
    instrument_storage: bool,
}

impl<'a> NativeCompiler<'a> {
    fn new(
        backend: &'a mut JitBackend,
        code: &'a CodeBlock,
        instructions: DecodedInstructions,
        mode: NativeMode,
        charge_instruction_budget: bool,
        instrument_storage: bool,
    ) -> Option<Self> {
        let analysis = analyze_registers(&instructions, code.register_count as usize)?;
        Some(Self {
            backend,
            code,
            instructions,
            mode,
            analysis,
            current_instruction: 0,
            variables: Vec::new(),
            defined_flags: Vec::new(),
            flag_defined: None,
            flag_undefined: None,
            dirty: BTreeSet::new(),
            charge_instruction_budget,
            instrument_storage,
        })
    }

    fn compile(
        &mut self,
    ) -> Result<Option<(extern "C" fn(*mut Context) -> u64, usize)>, JitModuleFailureStage> {
        let ptr = self.backend.module.target_config().pointer_type();
        let mut cctx = self.backend.module.make_context();
        let mut fctx = FunctionBuilderContext::new();

        cctx.func.signature.params.push(AbiParam::new(ptr));
        cctx.func.signature.returns.push(AbiParam::new(types::I64));

        let mut bcx = FunctionBuilder::new(&mut cctx.func, &mut fctx);
        self.variables = (0..self.code.register_count)
            .map(|_| bcx.declare_var(self.mode.value_type()))
            .collect();
        self.defined_flags = (0..self.code.register_count)
            .map(|_| bcx.declare_var(types::I32))
            .collect();

        let code_blocks: Vec<Block> = self
            .instructions
            .instructions
            .iter()
            .map(|_| bcx.create_block())
            .collect();

        let entry = bcx.create_block();
        let entry_deopt = bcx.create_block();
        let break_block = bcx.create_block();

        let ctx_val = {
            bcx.append_block_params_for_function_params(entry);
            bcx.switch_to_block(entry);
            bcx.block_params(entry)[0]
        };

        let helpers = self.build_helpers(&mut bcx, ptr);

        // A single pair of constants defined in the entry block keeps the flags
        // free wherever definedness agrees across a join: Cranelift drops the
        // block parameter when every predecessor yields the same value, so a
        // flag only becomes a real phi where the paths genuinely disagree.
        let flag_defined = bcx.ins().iconst(types::I32, 1);
        let flag_undefined = bcx.ins().iconst(types::I32, 0);
        self.flag_defined = Some(flag_defined);
        self.flag_undefined = Some(flag_undefined);
        for register in 0..self.variables.len() {
            self.clear_defined_flag(&mut bcx, register);
        }

        let guard_ok = self.emit_entry_guard(&mut bcx, ctx_val, entry_deopt, &helpers);
        if !guard_ok {
            return Ok(None);
        }

        bcx.ins().jump(code_blocks[0], &[]);

        bcx.switch_to_block(entry_deopt);
        self.emit_set_pc(&mut bcx, ctx_val, &helpers, 0);
        let entry_deopt_status = bcx.ins().iconst(
            types::I64,
            JitExit::encode_with_reason(JitExitKind::Deopt, JitExitReason::EntryGuard, 0) as i64,
        );
        bcx.ins().return_(&[entry_deopt_status]);

        bcx.append_block_param(break_block, types::I64);
        bcx.switch_to_block(break_block);
        let break_status = bcx.block_params(break_block)[0];
        bcx.ins().return_(&[break_status]);

        for index in 0..self.instructions.instructions.len() {
            let (pc, next_pc, instruction) = {
                let (pc, next_pc, instruction) = &self.instructions.instructions[index];
                (*pc, *next_pc, instruction.clone())
            };
            let block = code_blocks[index];
            bcx.switch_to_block(block);
            self.current_instruction = index;

            if self.charge_instruction_budget {
                self.emit_consume_instruction_budget(&mut bcx, ctx_val, &helpers, pc, break_block);
            }

            if !self.emit_instruction(
                &mut bcx,
                ctx_val,
                &helpers,
                pc,
                next_pc,
                &instruction,
                &code_blocks,
                break_block,
            ) {
                return Ok(None);
            }

            // A definition the lowering could not keep in a Cranelift variable
            // went straight to the VM frame, so the frame is now the owner.
            if let Some(Some(target)) = self.analysis.targets.get(index).copied()
                && self.defined_register_kind(target) == RegisterKind::Boxed
                && !self.clear_defined_flag(&mut bcx, target)
            {
                return Ok(None);
            }

            if fallthrough(&instruction) && !has_explicit_edges(&instruction) {
                let Some(next_index) = index
                    .checked_add(1)
                    .filter(|next| *next < code_blocks.len())
                else {
                    return Ok(None);
                };
                bcx.ins().jump(code_blocks[next_index], &[]);
            }
        }

        bcx.seal_all_blocks();
        bcx.finalize();

        let name = self.backend.next_fn_name("jit_native");
        self.backend
            .before_module_stage(JitModuleFailureStage::NativeDeclare)?;
        let id = self
            .backend
            .module
            .declare_function(&name, Linkage::Export, &cctx.func.signature)
            .map_err(|_| JitModuleFailureStage::NativeDeclare)?;
        self.backend
            .before_module_stage(JitModuleFailureStage::NativeDefine)?;
        self.backend
            .module
            .define_function(id, &mut cctx)
            .map_err(|_| JitModuleFailureStage::NativeDefine)?;
        self.backend
            .before_module_stage(JitModuleFailureStage::NativeCompiledCode)?;
        let code_bytes = cctx
            .compiled_code()
            .ok_or(JitModuleFailureStage::NativeCompiledCode)?
            .code_buffer()
            .len();
        self.backend.module.clear_context(&mut cctx);
        self.backend
            .before_module_stage(JitModuleFailureStage::NativeFinalize)?;
        self.backend
            .module
            .finalize_definitions()
            .map_err(|_| JitModuleFailureStage::NativeFinalize)?;

        let code_ptr = self.backend.module.get_finalized_function(id);
        // SAFETY: the signature is declared as `extern "C" fn(*mut Context) ->
        // u64`, and the backend owns the finalized code for the function's
        // lifetime.
        let entry = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*mut Context) -> u64>(code_ptr)
        };
        Ok(Some((entry, code_bytes)))
    }

    fn current_instruction_identity(&self) -> (Option<u32>, Option<crate::vm::Opcode>) {
        self.instructions
            .instructions
            .get(self.current_instruction)
            .map_or((None, None), |(pc, _, _)| {
                let opcode = crate::vm::Opcode::decode(self.code.bytecode.bytes[*pc]);
                (Some(*pc as u32), Some(opcode))
            })
    }

    fn build_helpers(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ptr: cranelift_codegen::ir::Type,
    ) -> Helpers {
        let mut make = |address: usize,
                        params: &[cranelift_codegen::ir::Type],
                        result: cranelift_codegen::ir::Type| {
            let mut signature = self.backend.module.make_signature();
            for param in params {
                signature.params.push(AbiParam::new(*param));
            }
            signature.returns.push(AbiParam::new(result));
            Helper {
                address,
                signature: bcx.import_signature(signature),
            }
        };

        Helpers {
            ptr,
            guard: make(
                jit_guard as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            copy_global_declarative_binding_register: make(
                jit_copy_global_declarative_binding_register as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            guard_argument_number: make(
                jit_guard_argument_number as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            guard_stack_number: make(
                jit_guard_stack_number as *const () as usize,
                &[ptr],
                types::I64,
            ),
            copy_argument_register: make(
                jit_copy_argument_register as *const () as usize,
                &[ptr, types::I32, types::I32],
                types::I64,
            ),
            copy_register: make(
                jit_copy_register as *const () as usize,
                &[ptr, types::I32, types::I32],
                types::I64,
            ),
            get_register_i32: make(
                jit_get_register_i32 as *const () as usize,
                &[ptr, types::I32],
                types::I32,
            ),
            get_register_f64: make(
                jit_get_register_f64 as *const () as usize,
                &[ptr, types::I32],
                types::F64,
            ),
            push_register: make(
                jit_push_register as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            set_return_register: make(
                jit_set_return_register as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            get_argument_i32: make(
                jit_get_argument_i32 as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            get_argument_f64: make(
                jit_get_argument_f64 as *const () as usize,
                &[ptr, types::I32],
                types::F64,
            ),
            dense_guard: make(
                if self.instrument_storage {
                    jit_diagnostic_dense_array_guard as *const () as usize
                } else {
                    jit_dense_array_guard as *const () as usize
                },
                &[ptr, types::I32, types::I32, types::I32, types::I32],
                types::I64,
            ),
            dense_guard_f64: make(
                if self.instrument_storage {
                    jit_diagnostic_dense_array_guard_f64 as *const () as usize
                } else {
                    jit_dense_array_guard_f64 as *const () as usize
                },
                &[ptr, types::I32, types::F64, types::I32],
                types::I64,
            ),
            dense_i32: make(
                if self.instrument_storage {
                    jit_diagnostic_dense_array_i32 as *const () as usize
                } else {
                    jit_dense_array_i32 as *const () as usize
                },
                &[ptr, types::I32, types::I32, types::I32],
                types::I32,
            ),
            dense_f64: make(
                if self.instrument_storage {
                    jit_diagnostic_dense_array_f64 as *const () as usize
                } else {
                    jit_dense_array_f64 as *const () as usize
                },
                &[ptr, types::I32, types::F64, types::I32],
                types::F64,
            ),
            named_guard: make(
                if self.instrument_storage {
                    jit_diagnostic_named_property_guard as *const () as usize
                } else {
                    jit_named_property_guard as *const () as usize
                },
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            named_i32: make(
                if self.instrument_storage {
                    jit_diagnostic_named_property_i32 as *const () as usize
                } else {
                    jit_named_property_i32 as *const () as usize
                },
                &[ptr, types::I32, types::I32],
                types::I32,
            ),
            named_f64: make(
                if self.instrument_storage {
                    jit_diagnostic_named_property_f64 as *const () as usize
                } else {
                    jit_named_property_f64 as *const () as usize
                },
                &[ptr, types::I32, types::I32],
                types::F64,
            ),
            call_ordinary: make(
                jit_call_ordinary as *const () as usize,
                &[ptr, types::I32, types::I64],
                types::I64,
            ),
            set_pc: make(
                jit_set_pc as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            store_i32_if_defined: make(
                jit_store_i32_if_defined as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            store_f64_if_defined: make(
                jit_store_f64_if_defined as *const () as usize,
                &[ptr, types::I32, types::F64, types::I32],
                types::I64,
            ),
            push_i32: make(
                jit_push_i32 as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            push_f64: make(
                jit_push_f64 as *const () as usize,
                &[ptr, types::F64],
                types::I64,
            ),
            pop_i32: make(jit_pop_i32 as *const () as usize, &[ptr], types::I64),
            pop_f64: make(jit_pop_f64 as *const () as usize, &[ptr], types::F64),
            set_return_i32: make(
                jit_set_return_i32 as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            set_return_f64: make(
                jit_set_return_f64 as *const () as usize,
                &[ptr, types::F64],
                types::I64,
            ),
            increment_loop: make(
                jit_increment_loop as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            handle_return: make(jit_handle_return as *const () as usize, &[ptr], types::I64),
            consume_instruction_budget: make(
                jit_consume_instruction_budget as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            refund_instruction_budget: make(
                jit_refund_instruction_budget as *const () as usize,
                &[ptr],
                types::I64,
            ),
        }
    }

    // kept as a method for a uniform `self.emit_*` dispatch style with the
    // sibling emitters below that do read `self`
    #[allow(clippy::unused_self)]
    fn emit_consume_instruction_budget(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        pc: usize,
        break_block: Block,
    ) {
        let helper = bcx.ins().iconst(
            helpers.ptr,
            helpers.consume_instruction_budget.address as i64,
        );
        let pc = bcx.ins().iconst(types::I32, pc as i64);
        let status = bcx.ins().call_indirect(
            helpers.consume_instruction_budget.signature,
            helper,
            &[ctx, pc],
        );
        let status = bcx.inst_results(status)[0];
        let break_mask = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
        let failed = bcx.ins().band(status, break_mask);
        let continuation = bcx.create_block();
        bcx.ins()
            .brif(failed, break_block, &[status.into()], continuation, &[]);
        bcx.switch_to_block(continuation);
    }

    fn emit_entry_guard(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        entry_deopt: Block,
        helpers: &Helpers,
    ) -> bool {
        let guard = bcx.ins().iconst(helpers.ptr, helpers.guard.address as i64);
        let charge_instruction_budget = bcx
            .ins()
            .iconst(types::I32, i64::from(self.charge_instruction_budget));
        let result = bcx.ins().call_indirect(
            helpers.guard.signature,
            guard,
            &[ctx, charge_instruction_budget],
        );
        let result = bcx.inst_results(result)[0];
        let native_entry = bcx.create_block();
        bcx.ins().brif(result, native_entry, &[], entry_deopt, &[]);
        bcx.switch_to_block(native_entry);
        true
    }

    // the instruction dispatch loop threads codegen context (bcx/ctx/helpers)
    // plus per-instruction addressing through every emit_* call; bundling
    // that into a context struct is a real follow-up, not a lint dodge
    #[allow(clippy::too_many_arguments)]
    fn emit_instruction(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        pc: usize,
        next_pc: usize,
        instruction: &Instruction,
        blocks: &[Block],
        break_block: Block,
    ) -> bool {
        let register = |operand: crate::vm::opcode::RegisterOperand| usize::from(operand);

        match instruction {
            Instruction::GetName { dst, binding_index } => {
                let dst = register(*dst);
                let binding_index = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*binding_index)));
                let dst_value = bcx.ins().iconst(types::I32, dst as i64);
                let representation = match (self.defined_register_kind(dst), self.mode) {
                    (RegisterKind::Boxed, _) => 2,
                    (_, NativeMode::F64) => 1,
                    (_, NativeMode::I32) => 0,
                };
                let representation = bcx.ins().iconst(types::I32, representation);
                let helper = bcx.ins().iconst(
                    helpers.ptr,
                    helpers.copy_global_declarative_binding_register.address as i64,
                );
                let guard = bcx.ins().call_indirect(
                    helpers.copy_global_declarative_binding_register.signature,
                    helper,
                    &[ctx, binding_index, dst_value, representation],
                );
                let guard = bcx.inst_results(guard)[0];
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                bcx.ins().brif(guard, cont, &[], deopt, &[]);
                bcx.switch_to_block(deopt);
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::BindingRead) {
                    return false;
                }
                bcx.switch_to_block(cont);

                if self.defined_register_kind(dst) != RegisterKind::Boxed {
                    let load_helper = if self.mode == NativeMode::F64 {
                        helpers.get_register_f64
                    } else {
                        helpers.get_register_i32
                    };
                    let load_address = bcx.ins().iconst(helpers.ptr, load_helper.address as i64);
                    let value = bcx.ins().call_indirect(
                        load_helper.signature,
                        load_address,
                        &[ctx, dst_value],
                    );
                    let value = bcx.inst_results(value)[0];
                    if !self.define_register(bcx, dst, value) {
                        return false;
                    }
                }
            }
            Instruction::GetArgument { index, dst } => {
                let index = bcx.ins().iconst(types::I32, i64::from(u32::from(*index)));
                let dst = register(*dst);
                if self.defined_register_kind(dst) == RegisterKind::Boxed {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.copy_argument_register.address as i64);
                    let dst_value = bcx.ins().iconst(types::I32, dst as i64);
                    bcx.ins().call_indirect(
                        helpers.copy_argument_register.signature,
                        helper,
                        &[ctx, index, dst_value],
                    );
                } else {
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();

                    match self.mode {
                        NativeMode::I32 => {
                            let helper = bcx
                                .ins()
                                .iconst(helpers.ptr, helpers.get_argument_i32.address as i64);
                            let result = bcx.ins().call_indirect(
                                helpers.get_argument_i32.signature,
                                helper,
                                &[ctx, index],
                            );
                            let result = bcx.inst_results(result)[0];
                            let fail_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                            let failed = bcx.ins().band(result, fail_mask);
                            bcx.ins().brif(failed, deopt, &[], cont, &[]);

                            bcx.switch_to_block(deopt);
                            if !self.emit_guard_deopt(
                                bcx,
                                ctx,
                                helpers,
                                pc,
                                JitExitReason::ArgumentType,
                            ) {
                                return false;
                            }
                            bcx.switch_to_block(cont);
                            let value = bcx.ins().ireduce(types::I32, result);
                            if !self.define_register(bcx, dst, value) {
                                return false;
                            }
                        }
                        NativeMode::F64 => {
                            let helper = bcx
                                .ins()
                                .iconst(helpers.ptr, helpers.guard_argument_number.address as i64);
                            let guard = bcx.ins().call_indirect(
                                helpers.guard_argument_number.signature,
                                helper,
                                &[ctx, index],
                            );
                            let guard = bcx.inst_results(guard)[0];
                            bcx.ins().brif(guard, cont, &[], deopt, &[]);

                            bcx.switch_to_block(deopt);
                            if !self.emit_guard_deopt(
                                bcx,
                                ctx,
                                helpers,
                                pc,
                                JitExitReason::ArgumentType,
                            ) {
                                return false;
                            }
                            bcx.switch_to_block(cont);
                            let helper = bcx
                                .ins()
                                .iconst(helpers.ptr, helpers.get_argument_f64.address as i64);
                            let result = bcx.ins().call_indirect(
                                helpers.get_argument_f64.signature,
                                helper,
                                &[ctx, index],
                            );
                            let value = bcx.inst_results(result)[0];
                            if !self.define_register(bcx, dst, value) {
                                return false;
                            }
                        }
                    }
                }
            }
            Instruction::StoreZero { dst } => {
                let value = self.constant_f64_or_i32(bcx, 0.0, 0);
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreOne { dst } => {
                let value = self.constant_f64_or_i32(bcx, 1.0, 1);
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreInt8 { dst, value } => {
                let value = self.constant_f64_or_i32(bcx, f64::from(*value), i64::from(*value));
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreInt16 { dst, value } => {
                let value = self.constant_f64_or_i32(bcx, f64::from(*value), i64::from(*value));
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreInt32 { dst, value } => {
                let value = self.constant_f64_or_i32(bcx, f64::from(*value), i64::from(*value));
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreFloat { dst, value } => {
                if self.mode != NativeMode::F64 {
                    return false;
                }
                let value = bcx.ins().f64const(f64::from(*value));
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreDouble { dst, value } => {
                if self.mode != NativeMode::F64 {
                    return false;
                }
                let value = bcx.ins().f64const(*value);
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::Move { dst, src } => {
                let dst = register(*dst);
                let src = register(*src);
                if self.register_kind(src) == RegisterKind::Boxed {
                    if self.defined_register_kind(dst) != RegisterKind::Boxed {
                        return false;
                    }
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.copy_register.address as i64);
                    let dst_value = bcx.ins().iconst(types::I32, dst as i64);
                    let src_value = bcx.ins().iconst(types::I32, src as i64);
                    bcx.ins().call_indirect(
                        helpers.copy_register.signature,
                        helper,
                        &[ctx, dst_value, src_value],
                    );
                } else {
                    let Some(value) = self.use_register(bcx, src) else {
                        return false;
                    };
                    if !self.define_register(bcx, dst, value) {
                        return false;
                    }
                }
            }
            Instruction::GetPropertyByName {
                dst,
                value,
                ic_index,
            }
            | Instruction::GetPropertyByNameWithThis {
                dst,
                value,
                ic_index,
                ..
            } => {
                if self.register_kind(usize::from(*value)) != RegisterKind::Boxed {
                    return false;
                }
                let dst = register(*dst);
                let object = bcx.ins().iconst(types::I32, usize::from(*value) as i64);
                let ic_index = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*ic_index)));
                let mode = bcx
                    .ins()
                    .iconst(types::I32, i64::from(self.mode == NativeMode::F64));
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let guard_helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.named_guard.address as i64);
                let guard = bcx.ins().call_indirect(
                    helpers.named_guard.signature,
                    guard_helper,
                    &[ctx, object, ic_index, mode],
                );
                let guard = bcx.inst_results(guard)[0];
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                bcx.ins().brif(guard, cont, &[], deopt, &[]);
                bcx.switch_to_block(deopt);
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::NamedProperty) {
                    return false;
                }
                bcx.switch_to_block(cont);
                let load_helper = if self.mode == NativeMode::F64 {
                    helpers.named_f64
                } else {
                    helpers.named_i32
                };
                let load_address = bcx.ins().iconst(helpers.ptr, load_helper.address as i64);
                let result = bcx.ins().call_indirect(
                    load_helper.signature,
                    load_address,
                    &[ctx, object, ic_index],
                );
                let result = bcx.inst_results(result)[0];
                if !self.define_register(bcx, dst, result) {
                    return false;
                }
            }
            Instruction::GetPropertyByValue {
                dst,
                key,
                object,
                ic_index,
                ..
            } => {
                let object = usize::from(*object);
                if self.register_kind(object) != RegisterKind::Boxed {
                    return false;
                }
                let Some(key) = self.use_register(bcx, register(*key)) else {
                    return false;
                };
                let dst = register(*dst);
                let object = bcx.ins().iconst(types::I32, object as i64);
                let ic_index = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*ic_index)));
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let guard = if self.mode == NativeMode::F64 {
                    let guard_helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.dense_guard_f64.address as i64);
                    bcx.ins().call_indirect(
                        helpers.dense_guard_f64.signature,
                        guard_helper,
                        &[ctx, object, key, ic_index],
                    )
                } else {
                    let mode = bcx.ins().iconst(types::I32, 0);
                    let guard_helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.dense_guard.address as i64);
                    bcx.ins().call_indirect(
                        helpers.dense_guard.signature,
                        guard_helper,
                        &[ctx, object, key, ic_index, mode],
                    )
                };
                let guard = bcx.inst_results(guard)[0];
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                bcx.ins().brif(guard, cont, &[], deopt, &[]);
                bcx.switch_to_block(deopt);
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::DenseElement) {
                    return false;
                }
                bcx.switch_to_block(cont);
                let result = if self.mode == NativeMode::F64 {
                    let load_helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.dense_f64.address as i64);
                    bcx.ins().call_indirect(
                        helpers.dense_f64.signature,
                        load_helper,
                        &[ctx, object, key, ic_index],
                    )
                } else {
                    let load_helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.dense_i32.address as i64);
                    bcx.ins().call_indirect(
                        helpers.dense_i32.signature,
                        load_helper,
                        &[ctx, object, key, ic_index],
                    )
                };
                let result = bcx.inst_results(result)[0];
                if !self.define_register(bcx, dst, result) {
                    return false;
                }
            }
            Instruction::Call { argument_count } => {
                let Some(expected_target) = self.backend.call_target(self.code, pc) else {
                    return false;
                };
                // The helper leaves the calling-convention stack untouched on
                // a non-ordinary or different ordinary callee. That makes
                // this a real guard exit: the interpreter can re-execute the
                // Call opcode with its normal generic-call semantics.
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.call_ordinary.address as i64);
                let argument_count = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*argument_count)));
                let expected_target = bcx.ins().iconst(types::I64, expected_target as i64);
                let status = bcx.ins().call_indirect(
                    helpers.call_ordinary.signature,
                    helper,
                    &[ctx, argument_count, expected_target],
                );
                let status = bcx.inst_results(status)[0];

                let break_mask = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
                let is_break = bcx.ins().band(status, break_mask);
                let guard_check = bcx.create_block();
                bcx.ins()
                    .brif(is_break, break_block, &[status.into()], guard_check, &[]);

                bcx.switch_to_block(guard_check);
                let guard_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                let guard_failed = bcx.ins().band(status, guard_mask);
                let deopt = bcx.create_block();
                let called = bcx.create_block();
                bcx.ins().brif(guard_failed, deopt, &[], called, &[]);

                bcx.switch_to_block(deopt);
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::CallTarget) {
                    return false;
                }

                bcx.switch_to_block(called);
                let status = bcx.ins().iconst(
                    types::I64,
                    JitExit::encode_with_reason(
                        JitExitKind::Call,
                        JitExitReason::Scheduler,
                        next_pc as u32,
                    ) as i64,
                );
                bcx.ins().return_(&[status]);
            }
            Instruction::Add { dst, lhs, rhs } => {
                let Some(lhs) = self.use_register(bcx, register(*lhs)) else {
                    return false;
                };
                let Some(rhs) = self.use_register(bcx, register(*rhs)) else {
                    return false;
                };
                let result = if self.mode == NativeMode::F64 {
                    bcx.ins().fadd(lhs, rhs)
                } else {
                    let (result, overflow) = bcx.ins().sadd_overflow(lhs, rhs);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::IntegerOverflow)
                    {
                        return false;
                    }
                    bcx.switch_to_block(cont);
                    result
                };
                if !self.define_register(bcx, register(*dst), result) {
                    return false;
                }
            }
            Instruction::Sub { dst, lhs, rhs } => {
                let Some(lhs) = self.use_register(bcx, register(*lhs)) else {
                    return false;
                };
                let Some(rhs) = self.use_register(bcx, register(*rhs)) else {
                    return false;
                };
                let result = if self.mode == NativeMode::F64 {
                    bcx.ins().fsub(lhs, rhs)
                } else {
                    let (result, overflow) = bcx.ins().ssub_overflow(lhs, rhs);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::IntegerOverflow)
                    {
                        return false;
                    }
                    bcx.switch_to_block(cont);
                    result
                };
                if !self.define_register(bcx, register(*dst), result) {
                    return false;
                }
            }
            Instruction::Mul { dst, lhs, rhs } => {
                let Some(lhs) = self.use_register(bcx, register(*lhs)) else {
                    return false;
                };
                let Some(rhs) = self.use_register(bcx, register(*rhs)) else {
                    return false;
                };
                let result = if self.mode == NativeMode::F64 {
                    bcx.ins().fmul(lhs, rhs)
                } else {
                    let (result, overflow) = bcx.ins().smul_overflow(lhs, rhs);
                    let zero = bcx.ins().iconst(types::I32, 0);
                    let is_zero = bcx.ins().icmp(IntCC::Equal, result, zero);
                    let lhs_negative = self.sign_bit(bcx, lhs);
                    let rhs_negative = self.sign_bit(bcx, rhs);
                    let signs_differ = bcx.ins().bxor(lhs_negative, rhs_negative);
                    let negative_zero = bcx.ins().band(is_zero, signs_differ);
                    let overflow = bcx.ins().bor(overflow, negative_zero);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::IntegerOverflow)
                    {
                        return false;
                    }
                    bcx.switch_to_block(cont);
                    result
                };
                if !self.define_register(bcx, register(*dst), result) {
                    return false;
                }
            }
            Instruction::Div { dst, lhs, rhs } => {
                if self.mode != NativeMode::F64 {
                    return false;
                }
                let Some(lhs) = self.use_register(bcx, register(*lhs)) else {
                    return false;
                };
                let Some(rhs) = self.use_register(bcx, register(*rhs)) else {
                    return false;
                };
                let result = bcx.ins().fdiv(lhs, rhs);
                if !self.define_register(bcx, register(*dst), result) {
                    return false;
                }
            }
            Instruction::Inc { dst, src } => {
                let source = register(*src);
                let destination = register(*dst);
                let Some(old_value) = self.use_register(bcx, source) else {
                    return false;
                };
                let new_value = if self.mode == NativeMode::F64 {
                    let one = bcx.ins().f64const(1.0);
                    bcx.ins().fadd(old_value, one)
                } else {
                    let one = bcx.ins().iconst(types::I32, 1);
                    let (new_value, max_overflow) = bcx.ins().sadd_overflow(old_value, one);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(max_overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::IntegerOverflow)
                    {
                        return false;
                    }
                    bcx.switch_to_block(cont);
                    new_value
                };
                if !self.define_register(bcx, destination, new_value) {
                    return false;
                }
            }
            Instruction::Jump { address } => {
                let Some(target) = self.target_block(*address, blocks) else {
                    return false;
                };
                bcx.ins().jump(target, &[]);
            }
            Instruction::JumpIfNotLessThan { address, lhs, rhs } => {
                if !self.emit_compare_branch(
                    bcx,
                    register(*lhs),
                    register(*rhs),
                    IntCC::SignedGreaterThanOrEqual,
                    FloatCC::UnorderedOrGreaterThanOrEqual,
                    *address,
                    next_pc,
                    blocks,
                ) {
                    return false;
                }
            }
            Instruction::JumpIfNotLessThanOrEqual { address, lhs, rhs } => {
                if !self.emit_compare_branch(
                    bcx,
                    register(*lhs),
                    register(*rhs),
                    IntCC::SignedGreaterThan,
                    FloatCC::UnorderedOrGreaterThan,
                    *address,
                    next_pc,
                    blocks,
                ) {
                    return false;
                }
            }
            Instruction::JumpIfNotGreaterThan { address, lhs, rhs } => {
                if !self.emit_compare_branch(
                    bcx,
                    register(*lhs),
                    register(*rhs),
                    IntCC::SignedLessThanOrEqual,
                    FloatCC::UnorderedOrLessThanOrEqual,
                    *address,
                    next_pc,
                    blocks,
                ) {
                    return false;
                }
            }
            Instruction::JumpIfNotGreaterThanOrEqual { address, lhs, rhs } => {
                if !self.emit_compare_branch(
                    bcx,
                    register(*lhs),
                    register(*rhs),
                    IntCC::SignedLessThan,
                    FloatCC::UnorderedOrLessThan,
                    *address,
                    next_pc,
                    blocks,
                ) {
                    return false;
                }
            }
            Instruction::JumpIfNotEqual { address, lhs, rhs } => {
                if !self.emit_compare_branch(
                    bcx,
                    register(*lhs),
                    register(*rhs),
                    IntCC::NotEqual,
                    FloatCC::NotEqual,
                    *address,
                    next_pc,
                    blocks,
                ) {
                    return false;
                }
            }
            Instruction::IncrementLoopIteration => {
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.increment_loop.address as i64);
                let next_pc = bcx.ins().iconst(types::I32, next_pc as i64);
                let status = bcx.ins().call_indirect(
                    helpers.increment_loop.signature,
                    helper,
                    &[ctx, next_pc],
                );
                let status = bcx.inst_results(status)[0];
                let break_mask = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
                let failed = bcx.ins().band(status, break_mask);
                let continuation = bcx.create_block();
                bcx.ins()
                    .brif(failed, break_block, &[status.into()], continuation, &[]);
                // The continuation is filled by the generic fallthrough jump
                // below after switching to it.
                bcx.switch_to_block(continuation);
            }
            Instruction::PushFromRegister { src } => {
                let src = register(*src);
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                if self.register_kind(src) == RegisterKind::Boxed {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.push_register.address as i64);
                    let src_value = bcx.ins().iconst(types::I32, src as i64);
                    bcx.ins().call_indirect(
                        helpers.push_register.signature,
                        helper,
                        &[ctx, src_value],
                    );
                } else {
                    let Some(value) = self.use_register(bcx, src) else {
                        return false;
                    };
                    let helper = if self.mode == NativeMode::F64 {
                        helpers.push_f64
                    } else {
                        helpers.push_i32
                    };
                    let helper_address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
                    bcx.ins()
                        .call_indirect(helper.signature, helper_address, &[ctx, value]);
                }
            }
            Instruction::PopIntoRegister { dst } => {
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                if self.mode == NativeMode::F64 {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.guard_stack_number.address as i64);
                    let guard = bcx.ins().call_indirect(
                        helpers.guard_stack_number.signature,
                        helper,
                        &[ctx],
                    );
                    let guard = bcx.inst_results(guard)[0];
                    bcx.ins().brif(guard, cont, &[], deopt, &[]);

                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::StackType) {
                        return false;
                    }
                    bcx.switch_to_block(cont);
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.pop_f64.address as i64);
                    let result = bcx
                        .ins()
                        .call_indirect(helpers.pop_f64.signature, helper, &[ctx]);
                    let value = bcx.inst_results(result)[0];
                    if !self.define_register(bcx, register(*dst), value) {
                        return false;
                    }
                } else {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.pop_i32.address as i64);
                    let result = bcx
                        .ins()
                        .call_indirect(helpers.pop_i32.signature, helper, &[ctx]);
                    let result = bcx.inst_results(result)[0];
                    let guard_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                    let failed = bcx.ins().band(result, guard_mask);
                    bcx.ins().brif(failed, deopt, &[], cont, &[]);

                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::StackType) {
                        return false;
                    }
                    bcx.switch_to_block(cont);
                    let value = bcx.ins().ireduce(types::I32, result);
                    if !self.define_register(bcx, register(*dst), value) {
                        return false;
                    }
                }
            }
            Instruction::SetAccumulator { src } => {
                let src = register(*src);
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                if self.register_kind(src) == RegisterKind::Boxed {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.set_return_register.address as i64);
                    let src_value = bcx.ins().iconst(types::I32, src as i64);
                    bcx.ins().call_indirect(
                        helpers.set_return_register.signature,
                        helper,
                        &[ctx, src_value],
                    );
                } else {
                    let Some(value) = self.use_register(bcx, src) else {
                        return false;
                    };
                    let helper = if self.mode == NativeMode::F64 {
                        helpers.set_return_f64
                    } else {
                        helpers.set_return_i32
                    };
                    let helper_address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
                    bcx.ins()
                        .call_indirect(helper.signature, helper_address, &[ctx, value]);
                }
            }
            Instruction::CheckReturn => {
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
            }
            Instruction::Return => {
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.handle_return.address as i64);
                let status =
                    bcx.ins()
                        .call_indirect(helpers.handle_return.signature, helper, &[ctx]);
                let status = bcx.inst_results(status)[0];
                bcx.ins().return_(&[status]);
            }
            _ => return false,
        }

        true
    }

    fn use_register(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        register: usize,
    ) -> Option<cranelift_codegen::ir::Value> {
        if self.register_kind(register) == RegisterKind::Boxed {
            return None;
        }
        self.variables
            .get(register)
            .and_then(|variable| bcx.try_use_var(*variable).ok())
    }

    fn register_kind(&self, register: usize) -> RegisterKind {
        self.analysis
            .before
            .get(self.current_instruction)
            .and_then(|kinds| kinds.get(register))
            .copied()
            .unwrap_or(RegisterKind::Boxed)
    }

    fn defined_register_kind(&self, register: usize) -> RegisterKind {
        self.analysis
            .after
            .get(self.current_instruction)
            .and_then(|kinds| kinds.get(register))
            .copied()
            .unwrap_or(RegisterKind::Boxed)
    }

    fn define_register(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        register: usize,
        value: cranelift_codegen::ir::Value,
    ) -> bool {
        if self.defined_register_kind(register) == RegisterKind::Boxed {
            return false;
        }
        let Some(variable) = self.variables.get(register) else {
            return false;
        };
        bcx.def_var(*variable, value);
        let Some(flag_defined) = self.flag_defined else {
            return false;
        };
        let Some(flag) = self.defined_flags.get(register) else {
            return false;
        };
        bcx.def_var(*flag, flag_defined);
        self.dirty.insert(register);
        true
    }

    /// Record that the VM frame, not the Cranelift variable, now owns the
    /// register. Boxed definitions write the frame directly, so a stale native
    /// value must never be flushed over them at a later guard exit.
    fn clear_defined_flag(&self, bcx: &mut FunctionBuilder<'_>, register: usize) -> bool {
        let (Some(flag_undefined), Some(flag)) =
            (self.flag_undefined, self.defined_flags.get(register))
        else {
            return false;
        };
        bcx.def_var(*flag, flag_undefined);
        true
    }

    fn use_defined_flag(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        register: usize,
    ) -> Option<cranelift_codegen::ir::Value> {
        self.defined_flags
            .get(register)
            .and_then(|flag| bcx.try_use_var(*flag).ok())
    }

    fn constant_f64_or_i32(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        f64_value: f64,
        i32_value: i64,
    ) -> cranelift_codegen::ir::Value {
        match self.mode {
            NativeMode::I32 => bcx.ins().iconst(types::I32, i32_value),
            NativeMode::F64 => bcx.ins().f64const(f64_value),
        }
    }

    // kept as a method for a uniform `self.emit_*` dispatch style, see
    // emit_consume_instruction_budget above
    #[allow(clippy::unused_self)]
    fn sign_bit(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        value: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let zero = bcx.ins().iconst(types::I32, 0);
        bcx.ins().icmp(IntCC::SignedLessThan, value, zero)
    }

    fn target_block(&self, address: crate::vm::opcode::Address, blocks: &[Block]) -> Option<Block> {
        self.instructions
            .pc_to_index
            .get(&(address.as_u32() as usize))
            .and_then(|index| blocks.get(*index))
            .copied()
    }

    fn next_block(&self, next_pc: usize, blocks: &[Block]) -> Option<Block> {
        self.instructions
            .pc_to_index
            .get(&next_pc)
            .and_then(|index| blocks.get(*index))
            .copied()
    }

    // see emit_instruction above
    #[allow(clippy::too_many_arguments)]
    fn emit_compare_branch(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        lhs_register: usize,
        rhs_register: usize,
        int_condition: IntCC,
        float_condition: FloatCC,
        target: crate::vm::opcode::Address,
        next_pc: usize,
        blocks: &[Block],
    ) -> bool {
        let Some(lhs) = self.use_register(bcx, lhs_register) else {
            return false;
        };
        let Some(rhs) = self.use_register(bcx, rhs_register) else {
            return false;
        };
        let Some(target) = self.target_block(target, blocks) else {
            return false;
        };
        let Some(next) = self.next_block(next_pc, blocks) else {
            return false;
        };
        let condition = match self.mode {
            NativeMode::I32 => bcx.ins().icmp(int_condition, lhs, rhs),
            NativeMode::F64 => bcx.ins().fcmp(float_condition, lhs, rhs),
        };
        bcx.ins().brif(condition, target, &[], next, &[]);
        true
    }

    /// Exit before the current bytecode has made any JavaScript-visible
    /// change. This invariant lets budgeted entries refund their native charge
    /// before the interpreter executes the same bytecode.
    fn emit_guard_deopt(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        pc: usize,
        reason: JitExitReason,
    ) -> bool {
        // Budgeted native entries have charged this bytecode already, but a
        // guard exit asks the interpreter to execute the same bytecode. Refund
        // that charge so the interpreter remains the single owner of it.
        if self.charge_instruction_budget {
            let helper = bcx.ins().iconst(
                helpers.ptr,
                helpers.refund_instruction_budget.address as i64,
            );
            bcx.ins()
                .call_indirect(helpers.refund_instruction_budget.signature, helper, &[ctx]);
        }

        // Dirty values are materialized before returning to the interpreter.
        //
        // A dirty register is not necessarily *live natively* on the path that
        // reached this exit: a definition the path branched around still puts
        // the register in `self.dirty`, and `try_use_var` cannot tell us so —
        // it only rejects *undeclared* variables, and silently materializes a
        // declared-but-undefined variable as zero. Writing that zero back would
        // replace whatever the VM frame still holds. Each register therefore
        // carries a companion flag that the same control flow keeps in sync, and
        // the store helper honours it.
        let registers: Vec<usize> = self.dirty.iter().copied().collect();
        for register in registers {
            let Some(value) = self.use_register(bcx, register) else {
                return false;
            };
            let Some(defined) = self.use_defined_flag(bcx, register) else {
                return false;
            };
            let helper = if self.mode == NativeMode::F64 {
                helpers.store_f64_if_defined
            } else {
                helpers.store_i32_if_defined
            };
            let register_value = bcx.ins().iconst(types::I32, register as i64);
            let helper_address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
            bcx.ins().call_indirect(
                helper.signature,
                helper_address,
                &[ctx, register_value, value, defined],
            );
        }
        self.emit_set_pc(bcx, ctx, helpers, pc);
        let status = bcx.ins().iconst(
            types::I64,
            JitExit::encode_with_reason(JitExitKind::Deopt, reason, pc as u32) as i64,
        );
        bcx.ins().return_(&[status]);
        true
    }

    // kept as a method for a uniform `self.emit_*` dispatch style, see
    // emit_consume_instruction_budget above
    #[allow(clippy::unused_self)]
    fn emit_set_pc(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        pc: usize,
    ) {
        let helper = bcx.ins().iconst(helpers.ptr, helpers.set_pc.address as i64);
        let pc = bcx.ins().iconst(types::I32, pc as i64);
        bcx.ins()
            .call_indirect(helpers.set_pc.signature, helper, &[ctx, pc]);
    }
}

#[derive(Clone, Copy)]
struct LoopHelpers {
    ptr: cranelift_codegen::ir::Type,
    entry_guard: Helper,
    register_guard: Helper,
    get_register_i32: Helper,
    get_register_f64: Helper,
    set_pc: Helper,
    store_i32: Helper,
    store_f64: Helper,
    increment_loop: Helper,
    consume_instruction_budget: Helper,
    refund_instruction_budget: Helper,
}

struct LoopRegionCompiler<'a> {
    backend: &'a mut JitBackend,
    code: &'a CodeBlock,
    plan: &'a LoopRegionPlan,
    region: Vec<LoopDecodedInstruction>,
    pc_to_index: HashMap<usize, usize>,
    mode: NativeMode,
    variables: Vec<Variable>,
}

impl<'a> LoopRegionCompiler<'a> {
    fn new(
        backend: &'a mut JitBackend,
        code: &'a CodeBlock,
        plan: &'a LoopRegionPlan,
        region: Vec<LoopDecodedInstruction>,
        mode: NativeMode,
    ) -> Self {
        let pc_to_index = region
            .iter()
            .enumerate()
            .map(|(index, instruction)| (instruction.pc, index))
            .collect();
        Self {
            backend,
            code,
            plan,
            region,
            pc_to_index,
            mode,
            variables: Vec::new(),
        }
    }

    fn compile(&mut self) -> Result<Option<CompiledLoopRegion>, JitModuleFailureStage> {
        let JitEntryPoint::Loop {
            header_pc,
            representation,
            ..
        } = self.plan.key.entry_point
        else {
            return Ok(None);
        };
        let ptr = self.backend.module.target_config().pointer_type();
        let mut cctx = self.backend.module.make_context();
        let mut fctx = FunctionBuilderContext::new();
        cctx.func.signature.params.push(AbiParam::new(ptr));
        cctx.func.signature.returns.push(AbiParam::new(types::I64));

        let mut bcx = FunctionBuilder::new(&mut cctx.func, &mut fctx);
        self.variables = (0..self.code.register_count)
            .map(|_| bcx.declare_var(self.mode.value_type()))
            .collect();
        let code_blocks: Vec<Block> = self.region.iter().map(|_| bcx.create_block()).collect();
        let entry = bcx.create_block();
        let frame_rejected = bcx.create_block();
        let representation_rejected = bcx.create_block();
        let loop_exit = bcx.create_block();

        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        let ctx = bcx.block_params(entry)[0];
        let helpers = self.build_helpers(&mut bcx, ptr);
        self.emit_frame_guard(&mut bcx, ctx, &helpers, header_pc, frame_rejected);

        for entry_value in &self.plan.entry {
            if entry_value.representation != representation
                || entry_value.source != LoopEntrySource::VmRegister
            {
                return Ok(None);
            }
            let register = entry_value.register as usize;
            let guard = bcx
                .ins()
                .iconst(helpers.ptr, helpers.register_guard.address as i64);
            let register_value = bcx.ins().iconst(types::I32, register as i64);
            let representation_value = bcx.ins().iconst(
                types::I32,
                i64::from(representation == JitOsrRepresentation::F64),
            );
            let matches = bcx.ins().call_indirect(
                helpers.register_guard.signature,
                guard,
                &[ctx, register_value, representation_value],
            );
            let matches = bcx.inst_results(matches)[0];
            let load = bcx.create_block();
            bcx.ins()
                .brif(matches, load, &[], representation_rejected, &[]);
            bcx.switch_to_block(load);

            let helper = match self.mode {
                NativeMode::I32 => helpers.get_register_i32,
                NativeMode::F64 => helpers.get_register_f64,
            };
            let address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
            let value = bcx
                .ins()
                .call_indirect(helper.signature, address, &[ctx, register_value]);
            let value = bcx.inst_results(value)[0];
            let Some(()) = self.define_register(&mut bcx, register, value) else {
                return Ok(None);
            };
        }
        let Some(first_block) = code_blocks.first() else {
            return Ok(None);
        };
        bcx.ins().jump(*first_block, &[]);

        bcx.switch_to_block(frame_rejected);
        let status = bcx.ins().iconst(
            types::I64,
            JitExit::encode_with_reason(
                JitExitKind::EntryRejected,
                JitExitReason::EntryGuard,
                header_pc,
            ) as i64,
        );
        bcx.ins().return_(&[status]);

        bcx.switch_to_block(representation_rejected);
        let status = bcx.ins().iconst(
            types::I64,
            JitExit::encode_with_reason(
                JitExitKind::EntryRejected,
                JitExitReason::ArgumentType,
                header_pc,
            ) as i64,
        );
        bcx.ins().return_(&[status]);

        for index in 0..self.region.len() {
            let instruction = self.region[index].clone();
            bcx.switch_to_block(code_blocks[index]);
            if self.plan.key.budgeted
                && self
                    .emit_budget_guard(&mut bcx, ctx, &helpers, instruction.pc, index)
                    .is_none()
            {
                return Ok(None);
            }
            let Some(()) = self.emit_instruction(
                &mut bcx,
                ctx,
                &helpers,
                &instruction,
                index,
                &code_blocks,
                loop_exit,
            ) else {
                return Ok(None);
            };
            if fallthrough(&instruction.instruction)
                && !has_explicit_edges(&instruction.instruction)
            {
                let Some(next_block) = code_blocks.get(index + 1) else {
                    return Ok(None);
                };
                bcx.ins().jump(*next_block, &[]);
            }
        }

        bcx.switch_to_block(loop_exit);
        let Some(exit) = self.plan.exits.first() else {
            return Ok(None);
        };
        let Some(()) = self.emit_exact_materialization(&mut bcx, ctx, &helpers, &exit.materialize)
        else {
            return Ok(None);
        };
        Self::emit_set_pc(&mut bcx, ctx, &helpers, exit.resume_pc);
        let status = bcx.ins().iconst(
            types::I64,
            JitExit::encode_with_reason(
                JitExitKind::Continuation,
                JitExitReason::LoopExit,
                exit.resume_pc,
            ) as i64,
        );
        bcx.ins().return_(&[status]);

        bcx.seal_all_blocks();
        bcx.finalize();

        let name = self.backend.next_fn_name("jit_loop");
        self.backend
            .before_module_stage(JitModuleFailureStage::LoopDeclare)?;
        let id = self
            .backend
            .module
            .declare_function(&name, Linkage::Export, &cctx.func.signature)
            .map_err(|_| JitModuleFailureStage::LoopDeclare)?;
        self.backend
            .before_module_stage(JitModuleFailureStage::LoopDefine)?;
        self.backend
            .module
            .define_function(id, &mut cctx)
            .map_err(|_| JitModuleFailureStage::LoopDefine)?;
        self.backend
            .before_module_stage(JitModuleFailureStage::LoopCompiledCode)?;
        let code_bytes = cctx
            .compiled_code()
            .ok_or(JitModuleFailureStage::LoopCompiledCode)?
            .code_buffer()
            .len();
        self.backend.module.clear_context(&mut cctx);
        self.backend
            .before_module_stage(JitModuleFailureStage::LoopFinalize)?;
        self.backend
            .module
            .finalize_definitions()
            .map_err(|_| JitModuleFailureStage::LoopFinalize)?;
        let code_ptr = self.backend.module.get_finalized_function(id);
        // SAFETY: this function was declared with the exact Context-pointer to
        // u64 C ABI, and the owning backend outlives the returned entry.
        let entry = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*mut Context) -> u64>(code_ptr)
        };
        Ok(Some(CompiledLoopRegion { entry, code_bytes }))
    }

    fn build_helpers(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ptr: cranelift_codegen::ir::Type,
    ) -> LoopHelpers {
        let mut make = |address: usize,
                        params: &[cranelift_codegen::ir::Type],
                        result: cranelift_codegen::ir::Type| {
            let mut signature = self.backend.module.make_signature();
            for param in params {
                signature.params.push(AbiParam::new(*param));
            }
            signature.returns.push(AbiParam::new(result));
            Helper {
                address,
                signature: bcx.import_signature(signature),
            }
        };
        LoopHelpers {
            ptr,
            entry_guard: make(
                jit_loop_entry_guard as *const () as usize,
                &[
                    ptr,
                    types::I64,
                    types::I64,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
            register_guard: make(
                jit_loop_register_guard as *const () as usize,
                &[ptr, types::I32, types::I32],
                types::I64,
            ),
            get_register_i32: make(
                jit_get_register_i32 as *const () as usize,
                &[ptr, types::I32],
                types::I32,
            ),
            get_register_f64: make(
                jit_get_register_f64 as *const () as usize,
                &[ptr, types::I32],
                types::F64,
            ),
            set_pc: make(
                jit_set_pc as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            store_i32: make(
                jit_store_i32 as *const () as usize,
                &[ptr, types::I32, types::I32],
                types::I64,
            ),
            store_f64: make(
                jit_store_f64 as *const () as usize,
                &[ptr, types::I32, types::F64],
                types::I64,
            ),
            increment_loop: make(
                jit_increment_loop as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            consume_instruction_budget: make(
                jit_consume_instruction_budget as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            refund_instruction_budget: make(
                jit_refund_instruction_budget as *const () as usize,
                &[ptr],
                types::I64,
            ),
        }
    }

    fn emit_frame_guard(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        header_pc: u32,
        rejected: Block,
    ) {
        let helper = bcx
            .ins()
            .iconst(helpers.ptr, helpers.entry_guard.address as i64);
        let backend_id = bcx.ins().iconst(types::I64, self.backend.id as i64);
        let code_id = bcx.ins().iconst(types::I64, self.plan.key.code_id as i64);
        let header = bcx.ins().iconst(types::I32, i64::from(header_pc));
        let budgeted = bcx
            .ins()
            .iconst(types::I32, i64::from(self.plan.key.budgeted));
        let registers = bcx
            .ins()
            .iconst(types::I32, i64::from(self.code.register_count));
        let result = bcx.ins().call_indirect(
            helpers.entry_guard.signature,
            helper,
            &[ctx, backend_id, code_id, header, budgeted, registers],
        );
        let result = bcx.inst_results(result)[0];
        let accepted = bcx.create_block();
        bcx.ins().brif(result, accepted, &[], rejected, &[]);
        bcx.switch_to_block(accepted);
    }

    fn emit_budget_guard(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        pc: usize,
        index: usize,
    ) -> Option<()> {
        let helper = bcx.ins().iconst(
            helpers.ptr,
            helpers.consume_instruction_budget.address as i64,
        );
        let pc_value = bcx.ins().iconst(types::I32, pc as i64);
        let status = bcx.ins().call_indirect(
            helpers.consume_instruction_budget.signature,
            helper,
            &[ctx, pc_value],
        );
        let status = bcx.inst_results(status)[0];
        let break_mask = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
        let failed = bcx.ins().band(status, break_mask);
        let failed_block = bcx.create_block();
        let continuation = bcx.create_block();
        bcx.ins().brif(failed, failed_block, &[], continuation, &[]);
        bcx.switch_to_block(failed_block);
        self.emit_available_materialization(bcx, ctx, helpers, index)?;
        bcx.ins().return_(&[status]);
        bcx.switch_to_block(continuation);
        Some(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_instruction(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        decoded: &LoopDecodedInstruction,
        index: usize,
        blocks: &[Block],
        loop_exit: Block,
    ) -> Option<()> {
        let register = |operand: crate::vm::opcode::RegisterOperand| usize::from(operand);
        match &decoded.instruction {
            Instruction::StoreZero { dst } => {
                let value = self.constant(bcx, 0.0, 0);
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::StoreOne { dst } => {
                let value = self.constant(bcx, 1.0, 1);
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::StoreInt8 { dst, value } => {
                let value = self.constant(bcx, f64::from(*value), i64::from(*value));
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::StoreInt16 { dst, value } => {
                let value = self.constant(bcx, f64::from(*value), i64::from(*value));
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::StoreInt32 { dst, value } => {
                let value = self.constant(bcx, f64::from(*value), i64::from(*value));
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::StoreFloat { dst, value } if self.mode == NativeMode::F64 => {
                let value = bcx.ins().f64const(f64::from(*value));
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::StoreDouble { dst, value } if self.mode == NativeMode::F64 => {
                let value = bcx.ins().f64const(*value);
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::Move { dst, src } => {
                let value = self.use_register(bcx, register(*src))?;
                self.define_register(bcx, register(*dst), value)?;
            }
            Instruction::Add { dst, lhs, rhs } => {
                let lhs = self.use_register(bcx, register(*lhs))?;
                let rhs = self.use_register(bcx, register(*rhs))?;
                let result = match self.mode {
                    NativeMode::F64 => bcx.ins().fadd(lhs, rhs),
                    NativeMode::I32 => {
                        let (result, overflow) = bcx.ins().sadd_overflow(lhs, rhs);
                        self.emit_overflow_guard(bcx, ctx, helpers, decoded.pc, index, overflow)?;
                        result
                    }
                };
                self.define_register(bcx, register(*dst), result)?;
            }
            Instruction::Sub { dst, lhs, rhs } => {
                let lhs = self.use_register(bcx, register(*lhs))?;
                let rhs = self.use_register(bcx, register(*rhs))?;
                let result = match self.mode {
                    NativeMode::F64 => bcx.ins().fsub(lhs, rhs),
                    NativeMode::I32 => {
                        let (result, overflow) = bcx.ins().ssub_overflow(lhs, rhs);
                        self.emit_overflow_guard(bcx, ctx, helpers, decoded.pc, index, overflow)?;
                        result
                    }
                };
                self.define_register(bcx, register(*dst), result)?;
            }
            Instruction::Mul { dst, lhs, rhs } => {
                let lhs = self.use_register(bcx, register(*lhs))?;
                let rhs = self.use_register(bcx, register(*rhs))?;
                let result = match self.mode {
                    NativeMode::F64 => bcx.ins().fmul(lhs, rhs),
                    NativeMode::I32 => {
                        let (result, overflow) = bcx.ins().smul_overflow(lhs, rhs);
                        let zero = bcx.ins().iconst(types::I32, 0);
                        let is_zero = bcx.ins().icmp(IntCC::Equal, result, zero);
                        let lhs_negative = Self::sign_bit(bcx, lhs);
                        let rhs_negative = Self::sign_bit(bcx, rhs);
                        let signs_differ = bcx.ins().bxor(lhs_negative, rhs_negative);
                        let negative_zero = bcx.ins().band(is_zero, signs_differ);
                        let overflow = bcx.ins().bor(overflow, negative_zero);
                        self.emit_overflow_guard(bcx, ctx, helpers, decoded.pc, index, overflow)?;
                        result
                    }
                };
                self.define_register(bcx, register(*dst), result)?;
            }
            Instruction::Inc { dst, src } => {
                let old = self.use_register(bcx, register(*src))?;
                let result = match self.mode {
                    NativeMode::F64 => {
                        let one = bcx.ins().f64const(1.0);
                        bcx.ins().fadd(old, one)
                    }
                    NativeMode::I32 => {
                        let one = bcx.ins().iconst(types::I32, 1);
                        let (result, overflow) = bcx.ins().sadd_overflow(old, one);
                        self.emit_overflow_guard(bcx, ctx, helpers, decoded.pc, index, overflow)?;
                        result
                    }
                };
                self.define_register(bcx, register(*dst), result)?;
            }
            Instruction::Jump { address } => {
                bcx.ins().jump(self.target_block(*address, blocks)?, &[]);
            }
            Instruction::JumpIfNotLessThan { address, lhs, rhs } => self.emit_compare(
                bcx,
                register(*lhs),
                register(*rhs),
                IntCC::SignedGreaterThanOrEqual,
                FloatCC::UnorderedOrGreaterThanOrEqual,
                *address,
                decoded.next_pc,
                blocks,
                loop_exit,
            )?,
            Instruction::JumpIfNotLessThanOrEqual { address, lhs, rhs } => self.emit_compare(
                bcx,
                register(*lhs),
                register(*rhs),
                IntCC::SignedGreaterThan,
                FloatCC::UnorderedOrGreaterThan,
                *address,
                decoded.next_pc,
                blocks,
                loop_exit,
            )?,
            Instruction::JumpIfNotGreaterThan { address, lhs, rhs } => self.emit_compare(
                bcx,
                register(*lhs),
                register(*rhs),
                IntCC::SignedLessThanOrEqual,
                FloatCC::UnorderedOrLessThanOrEqual,
                *address,
                decoded.next_pc,
                blocks,
                loop_exit,
            )?,
            Instruction::JumpIfNotGreaterThanOrEqual { address, lhs, rhs } => self.emit_compare(
                bcx,
                register(*lhs),
                register(*rhs),
                IntCC::SignedLessThan,
                FloatCC::UnorderedOrLessThan,
                *address,
                decoded.next_pc,
                blocks,
                loop_exit,
            )?,
            Instruction::JumpIfNotEqual { address, lhs, rhs } => self.emit_compare(
                bcx,
                register(*lhs),
                register(*rhs),
                IntCC::NotEqual,
                FloatCC::NotEqual,
                *address,
                decoded.next_pc,
                blocks,
                loop_exit,
            )?,
            Instruction::IncrementLoopIteration => {
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.increment_loop.address as i64);
                let next_pc = bcx.ins().iconst(types::I32, decoded.next_pc as i64);
                let status = bcx.ins().call_indirect(
                    helpers.increment_loop.signature,
                    helper,
                    &[ctx, next_pc],
                );
                let status = bcx.inst_results(status)[0];
                let break_mask = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
                let failed = bcx.ins().band(status, break_mask);
                let failed_block = bcx.create_block();
                let continuation = bcx.create_block();
                bcx.ins().brif(failed, failed_block, &[], continuation, &[]);
                bcx.switch_to_block(failed_block);
                self.emit_available_materialization(bcx, ctx, helpers, index)?;
                bcx.ins().return_(&[status]);
                bcx.switch_to_block(continuation);
            }
            _ => return None,
        }
        Some(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_compare(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        lhs: usize,
        rhs: usize,
        int_condition: IntCC,
        float_condition: FloatCC,
        target: crate::vm::opcode::Address,
        next_pc: usize,
        blocks: &[Block],
        loop_exit: Block,
    ) -> Option<()> {
        let lhs = self.use_register(bcx, lhs)?;
        let rhs = self.use_register(bcx, rhs)?;
        let condition = match self.mode {
            NativeMode::I32 => bcx.ins().icmp(int_condition, lhs, rhs),
            NativeMode::F64 => bcx.ins().fcmp(float_condition, lhs, rhs),
        };
        let target_pc = target.as_u32();
        let target = if self.plan.exits.first()?.resume_pc == target_pc {
            loop_exit
        } else {
            self.target_block(target, blocks)?
        };
        let next = self
            .pc_to_index
            .get(&next_pc)
            .and_then(|index| blocks.get(*index))
            .copied()?;
        bcx.ins().brif(condition, target, &[], next, &[]);
        Some(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_overflow_guard(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        pc: usize,
        index: usize,
        overflow: cranelift_codegen::ir::Value,
    ) -> Option<()> {
        let deopt = bcx.create_block();
        let continuation = bcx.create_block();
        bcx.ins().brif(overflow, deopt, &[], continuation, &[]);
        bcx.switch_to_block(deopt);
        if self.plan.key.budgeted {
            let helper = bcx.ins().iconst(
                helpers.ptr,
                helpers.refund_instruction_budget.address as i64,
            );
            bcx.ins()
                .call_indirect(helpers.refund_instruction_budget.signature, helper, &[ctx]);
        }
        self.emit_available_materialization(bcx, ctx, helpers, index)?;
        Self::emit_set_pc(bcx, ctx, helpers, pc as u32);
        let status = bcx.ins().iconst(
            types::I64,
            JitExit::encode_with_reason(
                JitExitKind::Deopt,
                JitExitReason::IntegerOverflow,
                pc as u32,
            ) as i64,
        );
        bcx.ins().return_(&[status]);
        bcx.switch_to_block(continuation);
        Some(())
    }

    /// Write back the registers the planner proved a mid-region exit owns.
    ///
    /// The set is taken from the plan rather than from the Cranelift variable
    /// map: `try_use_var` only rejects *undeclared* variables, and a declared
    /// variable with no definition on the current path is silently materialized
    /// as zero. Iterating the variable map would therefore store an integer
    /// zero over every frame register the region never defined.
    fn emit_available_materialization(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        index: usize,
    ) -> Option<()> {
        for register in self.plan.available.get(index)? {
            let register = *register as usize;
            let value = self.use_register(bcx, register)?;
            self.emit_store(bcx, ctx, helpers, register, value);
        }
        Some(())
    }

    fn emit_exact_materialization(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        values: &[LoopExitValue],
    ) -> Option<()> {
        for value in values {
            match value.source {
                LoopExitSource::PreservedVmValue => {}
                LoopExitSource::NativeValue => {
                    let native = self.use_register(bcx, value.register as usize)?;
                    self.emit_store(bcx, ctx, helpers, value.register as usize, native);
                }
            }
        }
        Some(())
    }

    fn emit_store(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        register: usize,
        value: cranelift_codegen::ir::Value,
    ) {
        let helper = match self.mode {
            NativeMode::I32 => helpers.store_i32,
            NativeMode::F64 => helpers.store_f64,
        };
        let address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
        let register = bcx.ins().iconst(types::I32, register as i64);
        bcx.ins()
            .call_indirect(helper.signature, address, &[ctx, register, value]);
    }

    fn emit_set_pc(
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &LoopHelpers,
        pc: u32,
    ) {
        let helper = bcx.ins().iconst(helpers.ptr, helpers.set_pc.address as i64);
        let pc = bcx.ins().iconst(types::I32, i64::from(pc));
        bcx.ins()
            .call_indirect(helpers.set_pc.signature, helper, &[ctx, pc]);
    }

    fn target_block(&self, address: crate::vm::opcode::Address, blocks: &[Block]) -> Option<Block> {
        self.pc_to_index
            .get(&(address.as_u32() as usize))
            .and_then(|index| blocks.get(*index))
            .copied()
    }

    fn use_register(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        register: usize,
    ) -> Option<cranelift_codegen::ir::Value> {
        self.variables
            .get(register)
            .and_then(|variable| bcx.try_use_var(*variable).ok())
    }

    fn define_register(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        register: usize,
        value: cranelift_codegen::ir::Value,
    ) -> Option<()> {
        bcx.def_var(*self.variables.get(register)?, value);
        Some(())
    }

    fn constant(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        f64_value: f64,
        i32_value: i64,
    ) -> cranelift_codegen::ir::Value {
        match self.mode {
            NativeMode::I32 => bcx.ins().iconst(types::I32, i32_value),
            NativeMode::F64 => bcx.ins().f64const(f64_value),
        }
    }

    fn sign_bit(
        bcx: &mut FunctionBuilder<'_>,
        value: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let zero = bcx.ins().iconst(types::I32, 0);
        bcx.ins().icmp(IntCC::SignedLessThan, value, zero)
    }
}

// The helper implementations are kept with the compiler so their ABI is
// reviewed together with the generated calls. Helpers return zero on success
// and a tagged/break status on failure.

fn jit_break(
    context: &mut Context,
    record: crate::vm::CompletionRecord,
    kind: JitExitKind,
    reason: JitExitReason,
    pc: u32,
) -> u64 {
    context.vm.jit_exit_pending = Some(JitExit { kind, reason, pc });
    context.vm.jit_pending = Some(record);
    JIT_BREAK_BIT
}

extern "C" fn jit_guard(context: *mut Context, charge_instruction_budget: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let budget_mode_matches =
        context.instruction_budget_remaining.is_some() == (charge_instruction_budget != 0);
    if context.vm.frame().construct() || !budget_mode_matches {
        return 0;
    }
    1
}

/// Validate the immutable frame identity owned by one typed loop artifact.
/// Register values are checked separately so a representation miss can carry
/// a distinct entry-rejection reason. Every access here is bounds checked;
/// generated code never reaches the unchecked VM register helpers until this
/// guard and all per-register guards have succeeded.
extern "C" fn jit_loop_entry_guard(
    context: *mut Context,
    backend_id: u64,
    code_id: u64,
    header_pc: u32,
    charge_instruction_budget: u32,
    register_count: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let frame = context.vm.frame();
    let budget_mode_matches =
        context.instruction_budget_remaining.is_some() == (charge_instruction_budget != 0);
    let register_range_exists = register_count == frame.code_block.register_count
        && (register_count == 0
            || context
                .vm
                .stack
                .get_register(frame, register_count as usize - 1)
                .is_some());
    u64::from(
        context.active_jit_backend_id == backend_id
            && frame.code_block.debug_id == code_id
            && frame.pc == header_pc
            && !frame.construct()
            && budget_mode_matches
            && register_range_exists,
    )
}

extern "C" fn jit_loop_register_guard(
    context: *mut Context,
    register: u32,
    representation: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let frame = context.vm.frame();
    let Some(value) = context.vm.stack.get_register(frame, register as usize) else {
        return 0;
    };
    u64::from(match representation {
        0 => value.as_i32().is_some(),
        1 => value.as_number().is_some(),
        _ => false,
    })
}

/// Charge one bytecode instruction before native lowering executes it.
/// Failures stay in VM state because Rust values cannot unwind through the C
/// ABI used by generated code.
extern "C" fn jit_consume_instruction_budget(context: *mut Context, pc: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    match context.consume_instruction_budget() {
        Ok(()) => 0,
        Err(error) => {
            context.vm.frame_mut().pc = pc;
            let mut error = crate::JsError::from(error);
            context.capture_error_backtrace(&mut error);
            jit_break(
                context,
                crate::vm::CompletionRecord::Throw(error),
                JitExitKind::Budget,
                JitExitReason::RuntimeLimit,
                pc,
            )
        }
    }
}

/// Return the current bytecode's native charge before a guard exit lets the
/// interpreter execute that same bytecode.
extern "C" fn jit_refund_instruction_budget(context: *mut Context) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    if let Some(remaining) = &mut context.instruction_budget_remaining {
        *remaining = remaining.saturating_add(1);
    }
    0
}

/// Copy a compile-time global-declarative binding into a VM register when the
/// active environment still makes that locator exact and the current value
/// matches the native representation. No environment or value is retained by
/// generated code; every entry reads through the current frame and realm.
extern "C" fn jit_copy_global_declarative_binding_register(
    context: *mut Context,
    binding_index: u32,
    register: u32,
    representation: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    if !context.binding_locator_stable() {
        return 0;
    }

    let value = {
        let frame = context.vm.frame();
        let Some(binding) = frame.code_block.bindings.get(binding_index as usize) else {
            return 0;
        };
        if binding.scope() != BindingLocatorScope::GlobalDeclarative {
            return 0;
        }
        let Some(value) = frame.realm.environment().get(binding.binding_index()) else {
            return 0;
        };
        value
    };

    let representation_matches = match representation {
        0 => value.as_i32().is_some(),
        1 => value.as_number().is_some(),
        2 => true,
        _ => false,
    };
    if !representation_matches {
        return 0;
    }

    context.vm.set_register(register as usize, value);
    1
}

extern "C" fn jit_guard_argument_number(context: *mut Context, index: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    u64::from(
        context
            .vm
            .stack
            .get_argument(context.vm.frame(), index as usize)
            .is_some_and(JsValue::is_number),
    )
}

extern "C" fn jit_get_register_i32(context: *mut Context, register: u32) -> i32 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context
        .vm
        .get_register(register as usize)
        .as_i32()
        .unwrap_or_default()
}

extern "C" fn jit_get_register_f64(context: *mut Context, register: u32) -> f64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context
        .vm
        .get_register(register as usize)
        .as_number()
        .unwrap_or_default()
}

extern "C" fn jit_guard_stack_number(context: *mut Context) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    u64::from(context.vm.stack.jit_top_is_number())
}

extern "C" fn jit_copy_argument_register(context: *mut Context, index: u32, register: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let value = context
        .vm
        .stack
        .get_argument(context.vm.frame(), index as usize)
        .cloned()
        .unwrap_or_else(JsValue::undefined);
    context.vm.set_register(register as usize, value);
    0
}

extern "C" fn jit_copy_register(context: *mut Context, dst: u32, src: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let value = context.vm.get_register(src as usize).clone();
    context.vm.set_register(dst as usize, value);
    0
}

extern "C" fn jit_push_register(context: *mut Context, register: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let value = context.vm.get_register(register as usize).clone();
    context.vm.stack.push(value);
    0
}

extern "C" fn jit_set_return_register(context: *mut Context, register: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let value = context.vm.get_register(register as usize).clone();
    context.vm.set_return_value(value);
    0
}

fn dense_array_value(
    context: &Context,
    register: u32,
    index: i32,
    ic_index: u32,
) -> Option<(IndexedKind, JsValue)> {
    let index = u32::try_from(index).ok()?;
    dense_array_value_at(context, register, index, ic_index)
}

fn dense_array_value_at(
    context: &Context,
    register: u32,
    index: u32,
    ic_index: u32,
) -> Option<(IndexedKind, JsValue)> {
    let value = context.vm.get_register(register as usize);
    let object = value.as_object_borrowed()?;
    let object = object.borrow();
    let ic = context
        .vm
        .frame()
        .code_block()
        .element_ic
        .get(ic_index as usize)?;
    let kind = ic.matches(object.shape())?;
    let value = object.properties().get_indexed_data_property(index)?;
    Some((kind, value))
}

fn dense_array_value_f64(
    context: &Context,
    register: u32,
    index: f64,
    ic_index: u32,
) -> Option<(IndexedKind, JsValue)> {
    if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index > f64::from(u32::MAX) {
        return None;
    }
    dense_array_value_at(context, register, index as u32, ic_index)
}

extern "C" fn jit_dense_array_guard(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
    mode: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some((kind, value)) = dense_array_value(context, register, index, ic_index) else {
        return 0;
    };
    let kind_ok = if mode == 0 {
        kind == IndexedKind::DenseI32 && value.as_i32().is_some()
    } else {
        matches!(kind, IndexedKind::DenseI32 | IndexedKind::DenseF64) && value.as_number().is_some()
    };
    u64::from(kind_ok)
}

extern "C" fn jit_dense_array_i32(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
) -> i32 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    dense_array_value(context, register, index, ic_index)
        .and_then(|(_, value)| value.as_i32())
        .unwrap_or_default()
}

extern "C" fn jit_dense_array_guard_f64(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some((kind, value)) = dense_array_value_f64(context, register, index, ic_index) else {
        return 0;
    };
    u64::from(
        matches!(kind, IndexedKind::DenseI32 | IndexedKind::DenseF64)
            && value.as_number().is_some(),
    )
}

extern "C" fn jit_dense_array_f64(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
) -> f64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    dense_array_value_f64(context, register, index, ic_index)
        .and_then(|(_, value)| value.as_number())
        .unwrap_or(0.0)
}

extern "C" fn jit_diagnostic_dense_array_guard(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
    mode: u32,
) -> u64 {
    let result = jit_dense_array_guard(context, register, index, ic_index, mode);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
    } else {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_dense_array_guard_f64(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
) -> u64 {
    let result = jit_dense_array_guard_f64(context, register, index, ic_index);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
    } else {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_dense_array_i32(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
) -> i32 {
    let result = jit_dense_array_i32(context, register, index, ic_index);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    counters.dense_loads = counters.dense_loads.saturating_add(1);
    result
}

extern "C" fn jit_diagnostic_dense_array_f64(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
) -> f64 {
    let result = jit_dense_array_f64(context, register, index, ic_index);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    counters.dense_loads = counters.dense_loads.saturating_add(1);
    result
}

fn named_property_value(context: &Context, register: u32, ic_index: u32) -> Option<JsValue> {
    let value = context.vm.get_register(register as usize);
    let object = value.as_object_borrowed()?;
    let object = object.borrow();
    let ic = context.vm.frame().code_block().ic.get(ic_index as usize)?;
    let slot = ic.get(object.shape())?;
    if slot.attributes.has_get() {
        return None;
    }
    if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
        let prototype = object.shape().prototype()?;
        let prototype = prototype.borrow();
        prototype
            .properties()
            .storage
            .get(slot.index as usize)
            .cloned()
    } else {
        object
            .properties()
            .storage
            .get(slot.index as usize)
            .cloned()
    }
}

extern "C" fn jit_named_property_guard(
    context: *mut Context,
    register: u32,
    ic_index: u32,
    mode: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some(value) = named_property_value(context, register, ic_index) else {
        return 0;
    };
    u64::from(if mode == 0 {
        value.as_i32().is_some()
    } else {
        value.as_number().is_some()
    })
}

extern "C" fn jit_named_property_i32(context: *mut Context, register: u32, ic_index: u32) -> i32 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    named_property_value(context, register, ic_index)
        .and_then(|value| value.as_i32())
        .unwrap_or_default()
}

extern "C" fn jit_named_property_f64(context: *mut Context, register: u32, ic_index: u32) -> f64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    named_property_value(context, register, ic_index)
        .and_then(|value| value.as_number())
        .unwrap_or(0.0)
}

extern "C" fn jit_diagnostic_named_property_guard(
    context: *mut Context,
    register: u32,
    ic_index: u32,
    mode: u32,
) -> u64 {
    let result = jit_named_property_guard(context, register, ic_index, mode);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.named_guard_misses = counters.named_guard_misses.saturating_add(1);
    } else {
        counters.named_guard_hits = counters.named_guard_hits.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_named_property_i32(
    context: *mut Context,
    register: u32,
    ic_index: u32,
) -> i32 {
    let result = jit_named_property_i32(context, register, ic_index);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    counters.named_loads = counters.named_loads.saturating_add(1);
    result
}

extern "C" fn jit_diagnostic_named_property_f64(
    context: *mut Context,
    register: u32,
    ic_index: u32,
) -> f64 {
    let result = jit_named_property_f64(context, register, ic_index);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    counters.named_loads = counters.named_loads.saturating_add(1);
    result
}

extern "C" fn jit_call_ordinary(
    context: *mut Context,
    argument_count: u32,
    expected_target: u64,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let argument_count = argument_count as usize;
    let function = context
        .vm
        .stack
        .calling_convention_get_function(argument_count)
        .clone();
    let Some(object) = function.as_object() else {
        return JIT_GUARD_FAIL_BIT;
    };
    let Some(ordinary) = object.downcast_ref::<OrdinaryFunction>() else {
        return JIT_GUARD_FAIL_BIT;
    };
    if !ordinary.codeblock().is_ordinary()
        || ordinary.codeblock().is_class_constructor()
        || ordinary.codeblock().debug_id != expected_target
    {
        return JIT_GUARD_FAIL_BIT;
    }

    let call = crate::builtins::function::function_call(
        &object,
        argument_count,
        &mut InternalMethodCallContext::new(context),
    );
    let call = match call {
        Ok(call) => call,
        Err(mut error) => {
            context.capture_error_backtrace(&mut error);
            let pc = context.vm.frame().pc;
            return jit_break(
                context,
                crate::vm::CompletionRecord::Throw(error),
                JitExitKind::Completion,
                JitExitReason::Exception,
                pc,
            );
        }
    };

    match call.resolve(context) {
        Ok(_) => JitExit::encode_with_reason(
            JitExitKind::Call,
            JitExitReason::Scheduler,
            context.vm.frame().pc,
        ),
        Err(mut error) => {
            context.capture_error_backtrace(&mut error);
            let pc = context.vm.frame().pc;
            jit_break(
                context,
                crate::vm::CompletionRecord::Throw(error),
                JitExitKind::Completion,
                JitExitReason::Exception,
                pc,
            )
        }
    }
}

extern "C" fn jit_get_argument_i32(context: *mut Context, index: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some(value) = context
        .vm
        .stack
        .get_argument(context.vm.frame(), index as usize)
        .and_then(JsValue::as_i32)
    else {
        return JIT_GUARD_FAIL_BIT;
    };
    u64::from(value as u32)
}

extern "C" fn jit_get_argument_f64(context: *mut Context, index: u32) -> f64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context
        .vm
        .stack
        .get_argument(context.vm.frame(), index as usize)
        .and_then(JsValue::as_number)
        .unwrap_or(0.0)
}

extern "C" fn jit_set_pc(context: *mut Context, pc: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.frame_mut().pc = pc;
    0
}

extern "C" fn jit_store_i32(context: *mut Context, register: u32, value: i32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context
        .vm
        .set_register(register as usize, JsValue::new(value));
    0
}

extern "C" fn jit_store_f64(context: *mut Context, register: u32, value: f64) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context
        .vm
        .set_register(register as usize, JsValue::new(value));
    0
}

/// Write back a register only when the taken path actually defined it.
///
/// The function tier accumulates one dirty set for the whole body, so a guard
/// exit can be reached on a path that branched around a register's native
/// definition. `defined` is the companion flag the generated code keeps in step
/// with that control flow; a zero leaves the VM frame's own value in place.
extern "C" fn jit_store_i32_if_defined(
    context: *mut Context,
    register: u32,
    value: i32,
    defined: u32,
) -> u64 {
    if defined == 0 {
        return 0;
    }
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context
        .vm
        .set_register(register as usize, JsValue::new(value));
    0
}

extern "C" fn jit_store_f64_if_defined(
    context: *mut Context,
    register: u32,
    value: f64,
    defined: u32,
) -> u64 {
    if defined == 0 {
        return 0;
    }
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context
        .vm
        .set_register(register as usize, JsValue::new(value));
    0
}

extern "C" fn jit_push_i32(context: *mut Context, value: i32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.stack.push(JsValue::new(value));
    0
}

extern "C" fn jit_push_f64(context: *mut Context, value: f64) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.stack.push(JsValue::new(value));
    0
}

extern "C" fn jit_pop_i32(context: *mut Context) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some(value) = context.vm.stack.jit_pop_i32() else {
        return JIT_GUARD_FAIL_BIT;
    };
    u64::from(value as u32)
}

extern "C" fn jit_pop_f64(context: *mut Context) -> f64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.stack.jit_pop_f64().unwrap_or(0.0)
}

extern "C" fn jit_set_return_i32(context: *mut Context, value: i32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.set_return_value(JsValue::new(value));
    0
}

extern "C" fn jit_set_return_f64(context: *mut Context, value: f64) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.set_return_value(JsValue::new(value));
    0
}

/// Publish the next bytecode PC and charge one interpreter-visible loop
/// iteration at a native backedge. Combining both safepoint operations avoids
/// a second Rust helper transition on every native loop iteration. Failures
/// remain recorded in VM state rather than crossing the C ABI as Rust values.
extern "C" fn jit_increment_loop(context: *mut Context, next_pc: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.frame_mut().pc = next_pc;
    match context.consume_loop_iterations(1) {
        Ok(()) => 0,
        Err(mut error) => {
            context.capture_error_backtrace(&mut error);
            let pc = context.vm.frame().pc;
            jit_break(
                context,
                crate::vm::CompletionRecord::Throw(error),
                JitExitKind::Budget,
                JitExitReason::RuntimeLimit,
                pc,
            )
        }
    }
}

extern "C" fn jit_handle_return(context: *mut Context) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.jit_handle_return()
}
