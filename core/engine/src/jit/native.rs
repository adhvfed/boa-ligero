//! Native lowering for the first narrow baseline tier.
//!
//! This module intentionally has a small allowlist. The legacy shim compiler
//! remains the fallback for every code block that cannot be represented by the
//! native value model below.

use std::collections::{BTreeSet, HashMap};

use boa_ast::scope::BindingLocatorScope;

use crate::builtins::function::OrdinaryFunction;
use crate::builtins::number::f64_to_int32;
use crate::object::internal_methods::InternalMethodCallContext;
use crate::object::shape::slot::SlotAttributes;
use crate::vm::{CodeBlock, IndexedKind, Instruction, InstructionIterator};
use crate::{Context, JsObject, JsValue};

use super::{
    JIT_BREAK_BIT, JIT_GUARD_FAIL_BIT, JitBackend, JitCacheKey, JitCompileBlockerKind,
    JitEntryPoint, JitExit, JitExitKind, JitExitReason, JitModuleFailureStage,
    JitOsrRejectionReason, JitOsrRepresentation, MAX_FUNCTION_BYTECODE_INSTRUCTIONS,
};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{AbiParam, Block, InstBuilder, StackSlotData, StackSlotKind, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{Linkage, Module};

const JIT_SCAN_DENSE_FAIL_BIT: u64 = 1 << 60;
const JIT_SCAN_MATCH_BIT: u64 = 1 << 59;
const JIT_SUM_APPLIED_BIT: u64 = 1 << 59;
const JIT_SCAN_COUNT_SHIFT: u32 = 32;
const JIT_SCAN_COUNT_MASK: u64 = (1 << 27) - 1;
const JIT_GLOBAL_DECLARATIVE_IC: u32 = u32::MAX;

/// Compile an ordinary numeric code block to native code.
///
/// The current native subset is deliberately conservative: primitive values
/// use an `i32` or `f64` specialization, while a small set of boxed reads and
/// calls stays in traced VM storage across helper boundaries. Returning `None`
/// is a normal eligibility result; the caller uses the legacy shim compiler.
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
            let region_excluded = matches!(
                instruction,
                Instruction::Call { .. }
                    | Instruction::BitAnd { .. }
                    | Instruction::BitOr { .. }
                    | Instruction::BitXor { .. }
                    | Instruction::GetLengthProperty { .. }
                    | Instruction::GetPropertyByName { .. }
                    | Instruction::GetPropertyByNameWithThis { .. }
                    | Instruction::GetPropertyByValue { .. }
                    | Instruction::GetPropertyByValuePush { .. }
                    | Instruction::SetPropertyByName { .. }
            );
            if region_excluded || !is_supported(code, opcode, instruction) {
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
        Instruction::IncrementLoopIteration
        | Instruction::PureReaderLoopIteration
        | Instruction::PureAffineLoopIteration
        | Instruction::PurePropertyWriteLoopIteration
        | Instruction::PureMethodLoopIteration
        | Instruction::PureGlobalAffineLoopIteration
        | Instruction::PureIndexedReaderLoopIteration
        | Instruction::Jump { .. } => Ok((Vec::new(), None)),
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

#[derive(Clone, Copy)]
pub(super) struct NativeCompileOptions {
    pub(super) accounting: NativeAccounting,
    pub(super) diagnostics: NativeDiagnostics,
}

#[derive(Clone, Copy)]
pub(super) struct NativeAccounting {
    pub(super) instruction_budget: bool,
    pub(super) loop_iterations: bool,
}

#[derive(Clone, Copy)]
pub(super) struct NativeDiagnostics {
    pub(super) collect_metadata: bool,
    pub(super) instrument_storage: bool,
}

pub(super) fn compile(
    backend: &mut JitBackend,
    code: &CodeBlock,
    options: NativeCompileOptions,
) -> NativeCompileResult {
    let eligibility_blocker = eligibility_blocker(code);
    if let Some(kind) = eligibility_blocker {
        let bytecode_instructions = if options.diagnostics.collect_metadata {
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
        options.diagnostics.collect_metadata,
        MAX_FUNCTION_BYTECODE_INSTRUCTIONS,
    ) {
        Ok(instructions) => instructions,
        Err(rejection) => return NativeCompileResult::Rejected(rejection),
    };
    let bytecode_instructions = instructions.instructions.len();
    let profile = instructions.static_profile();
    let mode = select_mode(&instructions);
    let Some(mut compiler) = NativeCompiler::new(backend, code, instructions, mode, options) else {
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
                Instruction::GetNameGlobal { .. }
                    | Instruction::GetLengthProperty { .. }
                    | Instruction::GetPropertyByName { .. }
                    | Instruction::GetPropertyByNameWithThis { .. }
                    | Instruction::GetPropertyByValue { .. }
                    | Instruction::GetPropertyByValuePush { .. }
                    | Instruction::SetPropertyByName { .. }
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
    let contains_float_literal = instructions.instructions.iter().any(|(_, _, instruction)| {
        matches!(
            instruction,
            Instruction::StoreFloat { .. } | Instruction::StoreDouble { .. }
        )
    });
    let contains_argument = instructions
        .instructions
        .iter()
        .any(|(_, _, instruction)| matches!(instruction, Instruction::GetArgument { .. }));
    let contains_bitwise = instructions.instructions.iter().any(|(_, _, instruction)| {
        matches!(
            instruction,
            Instruction::BitOr { .. } | Instruction::BitXor { .. }
        )
    });
    let bitwise_coercions_are_proven_i32 =
        !contains_bitwise
            || (!contains_argument
                && instructions.instructions.iter().enumerate().all(
                    |(index, (_, _, instruction))| match instruction {
                        Instruction::BitOr { .. } => {
                            index.checked_sub(2).is_some_and(|arithmetic_index| {
                                i32_arithmetic_coercion(
                                    instructions,
                                    arithmetic_index,
                                    &instructions.instructions[arithmetic_index].2,
                                )
                            })
                        }
                        Instruction::BitXor { .. } => false,
                        _ => true,
                    },
                ));

    if contains_float_literal || !bitwise_coercions_are_proven_i32 {
        NativeMode::F64
    } else {
        NativeMode::I32
    }
}

fn small_exact_integer_definition(instruction: &Instruction, register: usize) -> bool {
    let destination = |operand: crate::vm::opcode::RegisterOperand| usize::from(operand);
    match instruction {
        Instruction::StoreZero { dst }
        | Instruction::StoreOne { dst }
        | Instruction::StoreInt8 { dst, .. }
        | Instruction::StoreInt16 { dst, .. } => destination(*dst) == register,
        Instruction::StoreInt32 { dst, value } => {
            destination(*dst) == register && value.unsigned_abs() <= 1 << 21
        }
        _ => false,
    }
}

fn i32_constant_definition(instruction: &Instruction) -> Option<(usize, i32)> {
    let destination = |operand: crate::vm::opcode::RegisterOperand| usize::from(operand);
    match instruction {
        Instruction::StoreZero { dst } => Some((destination(*dst), 0)),
        Instruction::StoreOne { dst } => Some((destination(*dst), 1)),
        Instruction::StoreInt8 { dst, value } => Some((destination(*dst), i32::from(*value))),
        Instruction::StoreInt16 { dst, value } => Some((destination(*dst), i32::from(*value))),
        Instruction::StoreInt32 { dst, value } => Some((destination(*dst), *value)),
        _ => None,
    }
}

fn i32_arithmetic_coercion(
    instructions: &DecodedInstructions,
    arithmetic_index: usize,
    instruction: &Instruction,
) -> bool {
    let register = |operand: crate::vm::opcode::RegisterOperand| usize::from(operand);
    let (dst, multiplication_operands) = match instruction {
        Instruction::Add { dst, .. } | Instruction::Sub { dst, .. } => (register(*dst), None),
        Instruction::Mul { dst, lhs, rhs } => {
            (register(*dst), Some((register(*lhs), register(*rhs))))
        }
        _ => return false,
    };

    let Some((_, _, Instruction::StoreZero { dst: zero })) =
        instructions.instructions.get(arithmetic_index + 1)
    else {
        return false;
    };
    let zero = register(*zero);
    let Some((_, _, Instruction::BitOr { lhs, rhs, .. })) =
        instructions.instructions.get(arithmetic_index + 2)
    else {
        return false;
    };
    let lhs = register(*lhs);
    let rhs = register(*rhs);
    if !((lhs == dst && rhs == zero) || (rhs == dst && lhs == zero)) {
        return false;
    }

    let Some((mul_lhs, mul_rhs)) = multiplication_operands else {
        return true;
    };
    let Some((_, _, previous)) = arithmetic_index
        .checked_sub(1)
        .and_then(|index| instructions.instructions.get(index))
    else {
        return false;
    };
    small_exact_integer_definition(previous, mul_lhs)
        || small_exact_integer_definition(previous, mul_rhs)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterKind {
    Numeric,
    Boolean,
    Boxed,
}

#[derive(Clone, Copy, Debug)]
struct RegisterDefinition {
    source: Option<usize>,
    kind: RegisterKind,
    /// This value comes from a dynamically typed VM source. If it is used as
    /// an ordinary call argument, keep the traced `JsValue` in the VM register
    /// instead of guessing a numeric native representation.
    box_when_passed: bool,
}

struct RegisterAnalysis {
    before: Vec<Vec<RegisterKind>>,
    after: Vec<Vec<RegisterKind>>,
    /// Registers whose current values are used on at least one path after each
    /// instruction. Safepoints materialize only this set instead of every
    /// definition the compiler has ever seen.
    live_after: Vec<BTreeSet<usize>>,
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
            box_when_passed: false,
        })
        .collect();
    let mut current: Vec<usize> = (0..register_count).collect();
    let mut before_ids = Vec::with_capacity(instructions.instructions.len());
    let mut after_ids = Vec::with_capacity(instructions.instructions.len());
    let mut targets = Vec::with_capacity(instructions.instructions.len());
    let call_pushes = call_push_operands(instructions)?;

    for (index, (_, _, instruction)) in instructions.instructions.iter().enumerate() {
        before_ids.push(current.clone());

        for register in object_operands(instruction) {
            let definition = current.get(register).copied()?;
            mark_definition(definition, &mut definitions);
        }
        if call_pushes.boxed.contains(&index)
            && let Instruction::PushFromRegister { src } = instruction
        {
            let definition = current.get(usize::from(*src)).copied()?;
            mark_definition(definition, &mut definitions);
        } else if call_pushes.arguments.contains(&index)
            && let Instruction::PushFromRegister { src } = instruction
        {
            let definition = current.get(usize::from(*src)).copied()?;
            if definitions.get(definition)?.box_when_passed {
                mark_definition(definition, &mut definitions);
            }
        }

        let mut target = None;
        if let Some((register, source, kind, box_when_passed)) =
            output_definition(instruction, &current, &definitions)
        {
            if register >= current.len() {
                return None;
            }
            let definition = definitions.len();
            definitions.push(RegisterDefinition {
                source,
                kind,
                box_when_passed,
            });
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

    let live_after = analyze_register_liveness(instructions, register_count, &targets)?;
    Some(RegisterAnalysis {
        before: before_ids.iter().map(|ids| kinds(ids)).collect(),
        after: after_ids.iter().map(|ids| kinds(ids)).collect(),
        live_after,
        targets,
    })
}

fn analyze_register_liveness(
    instructions: &DecodedInstructions,
    register_count: usize,
    definitions: &[Option<usize>],
) -> Option<Vec<BTreeSet<usize>>> {
    let mut uses = Vec::with_capacity(instructions.instructions.len());
    let mut successors = Vec::with_capacity(instructions.instructions.len());

    for (index, (_, next_pc, instruction)) in instructions.instructions.iter().enumerate() {
        let instruction_uses: BTreeSet<usize> = register_uses(instruction).into_iter().collect();
        if instruction_uses
            .iter()
            .any(|register| *register >= register_count)
        {
            return None;
        }
        uses.push(instruction_uses);

        let mut instruction_successors = Vec::with_capacity(2);
        if let Some(target_pc) = branch_target(instruction) {
            instruction_successors.push(*instructions.pc_to_index.get(&target_pc)?);
        }
        if fallthrough(instruction) {
            if let Some(next) = instructions.pc_to_index.get(next_pc) {
                instruction_successors.push(*next);
            } else if index + 1 < instructions.instructions.len() {
                return None;
            }
        }
        instruction_successors.sort_unstable();
        instruction_successors.dedup();
        successors.push(instruction_successors);
    }

    let mut live_before = vec![BTreeSet::new(); instructions.instructions.len()];
    let mut live_after = vec![BTreeSet::new(); instructions.instructions.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for index in (0..instructions.instructions.len()).rev() {
            let mut after = BTreeSet::new();
            for successor in &successors[index] {
                after.extend(live_before[*successor].iter().copied());
            }
            let mut before = after.clone();
            if let Some(definition) = definitions.get(index).copied().flatten() {
                before.remove(&definition);
            }
            before.extend(uses[index].iter().copied());
            if after != live_after[index] || before != live_before[index] {
                live_after[index] = after;
                live_before[index] = before;
                changed = true;
            }
        }
    }
    Some(live_after)
}

fn register_uses(instruction: &Instruction) -> Vec<usize> {
    let register = |value: crate::vm::opcode::RegisterOperand| usize::from(value);
    match instruction {
        Instruction::Move { src, .. }
        | Instruction::Inc { src, .. }
        | Instruction::PushFromRegister { src }
        | Instruction::SetAccumulator { src } => vec![register(*src)],
        Instruction::Add { lhs, rhs, .. }
        | Instruction::Sub { lhs, rhs, .. }
        | Instruction::Div { lhs, rhs, .. }
        | Instruction::Mul { lhs, rhs, .. }
        | Instruction::BitOr { lhs, rhs, .. }
        | Instruction::BitXor { lhs, rhs, .. }
        | Instruction::JumpIfNotLessThan { lhs, rhs, .. }
        | Instruction::JumpIfNotLessThanOrEqual { lhs, rhs, .. }
        | Instruction::JumpIfNotGreaterThan { lhs, rhs, .. }
        | Instruction::JumpIfNotGreaterThanOrEqual { lhs, rhs, .. }
        | Instruction::JumpIfNotEqual { lhs, rhs, .. } => {
            vec![register(*lhs), register(*rhs)]
        }
        Instruction::StrictEq { lhs, rhs, .. } => vec![register(*lhs), register(*rhs)],
        Instruction::JumpIfFalse { value, .. }
        | Instruction::GetLengthProperty { value, .. }
        | Instruction::GetPropertyByName { value, .. } => vec![register(*value)],
        Instruction::GetPropertyByNameWithThis {
            receiver, value, ..
        } => vec![register(*receiver), register(*value)],
        Instruction::GetPropertyByValue {
            receiver,
            object,
            key,
            ..
        }
        | Instruction::GetPropertyByValuePush {
            receiver,
            object,
            key,
            ..
        } => vec![register(*receiver), register(*object), register(*key)],
        Instruction::SetPropertyByName { value, object, .. } => {
            vec![register(*value), register(*object)]
        }
        _ => Vec::new(),
    }
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
        Instruction::StrictEq { lhs, rhs, .. } => {
            vec![usize::from(*lhs), usize::from(*rhs)]
        }
        Instruction::SetPropertyByName { object, .. } => vec![usize::from(*object)],
        _ => Vec::new(),
    }
}

struct CallPushOperands {
    boxed: BTreeSet<usize>,
    arguments: BTreeSet<usize>,
}

/// Identify the two boxed prologue operands (`this`, function) and the
/// argument operands for each supported call-convention group.
///
/// Arguments may have arbitrary register-only bytecode between their
/// `PushFromRegister` instructions, so adjacency to `Call` is not a valid role
/// test. Walk backward to recover all `argument_count + 2` pushes. The first
/// two are always boxed. Binding-sourced dynamic values in the remaining
/// argument positions are boxed too, while known numeric definitions remain
/// in SSA and use the numeric push helper.
///
/// A nested call, stack pop, or control-flow edge before the complete group is
/// ambiguous without full value-stack dataflow. Reject that shape instead of
/// guessing which register contains the callable object.
fn call_push_operands(instructions: &DecodedInstructions) -> Option<CallPushOperands> {
    let mut boxed_pushes = BTreeSet::new();
    let mut argument_pushes = BTreeSet::new();
    for (call_index, (_, _, instruction)) in instructions.instructions.iter().enumerate() {
        let Instruction::Call { argument_count } = instruction else {
            continue;
        };
        let required_pushes = usize::from(*argument_count).checked_add(2)?;
        let mut group = Vec::with_capacity(required_pushes);
        let mut previous = call_index;
        while group.len() < required_pushes {
            let push_index = previous.checked_sub(1)?;
            let candidate = &instructions.instructions[push_index].2;
            match candidate {
                Instruction::PushFromRegister { .. } => group.push(push_index),
                Instruction::Call { .. }
                | Instruction::PopIntoRegister { .. }
                | Instruction::Jump { .. }
                | Instruction::JumpIfNotLessThan { .. }
                | Instruction::JumpIfNotLessThanOrEqual { .. }
                | Instruction::JumpIfNotGreaterThan { .. }
                | Instruction::JumpIfNotGreaterThanOrEqual { .. }
                | Instruction::JumpIfNotEqual { .. }
                | Instruction::Return => return None,
                _ => {}
            }
            previous = push_index;
        }
        group.reverse();
        boxed_pushes.extend(group.iter().copied().take(2));
        argument_pushes.extend(group.into_iter().skip(2));
    }
    Some(CallPushOperands {
        boxed: boxed_pushes,
        arguments: argument_pushes,
    })
}

fn output_definition(
    instruction: &Instruction,
    current: &[usize],
    definitions: &[RegisterDefinition],
) -> Option<(usize, Option<usize>, RegisterKind, bool)> {
    let numeric = |register: usize| (register, None, RegisterKind::Numeric, false);
    let dynamic = |register: usize| (register, None, RegisterKind::Numeric, true);
    let boolean = |register: usize| (register, None, RegisterKind::Boolean, false);
    let boxed = |register: usize| (register, None, RegisterKind::Boxed, false);
    let moved = |dst: usize, src: usize| {
        let source = current.get(src).copied();
        let definition = source.and_then(|source| definitions.get(source));
        (
            dst,
            source,
            definition.map_or(RegisterKind::Boxed, |definition| definition.kind),
            definition.is_some_and(|definition| definition.box_when_passed),
        )
    };

    match instruction {
        Instruction::This { dst } => Some(boxed(usize::from(*dst))),
        Instruction::GetName { dst, .. } | Instruction::GetNameGlobal { dst, .. } => {
            Some(dynamic(usize::from(*dst)))
        }
        Instruction::GetArgument { dst, .. }
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
        | Instruction::BitOr { dst, .. }
        | Instruction::BitXor { dst, .. }
        | Instruction::Inc { dst, .. }
        | Instruction::PopIntoRegister { dst }
        | Instruction::GetPropertyByName { dst, .. }
        | Instruction::GetLengthProperty { dst, .. }
        | Instruction::GetPropertyByNameWithThis { dst, .. }
        | Instruction::GetPropertyByValue { dst, .. }
        | Instruction::GetPropertyByValuePush { dst, .. } => Some(numeric(usize::from(*dst))),
        Instruction::StrictEq { dst, .. } => Some(boolean(usize::from(*dst))),
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
        (Opcode::GetNameGlobal, Instruction::GetNameGlobal { binding_index, .. }) => code
            .bindings
            .get(usize::from(*binding_index))
            .is_some_and(|binding| binding.scope() == BindingLocatorScope::GlobalObject),
        (Opcode::This, Instruction::This { .. })
        | (Opcode::GetArgument, Instruction::GetArgument { .. })
        | (Opcode::StoreZero, Instruction::StoreZero { .. })
        | (Opcode::StoreOne, Instruction::StoreOne { .. })
        | (Opcode::StoreInt8, Instruction::StoreInt8 { .. })
        | (Opcode::StoreInt16, Instruction::StoreInt16 { .. })
        | (Opcode::StoreInt32, Instruction::StoreInt32 { .. })
        | (Opcode::StoreFloat, Instruction::StoreFloat { .. })
        | (Opcode::StoreDouble, Instruction::StoreDouble { .. })
        | (Opcode::Move, Instruction::Move { .. })
        | (Opcode::GetLengthProperty, Instruction::GetLengthProperty { .. })
        | (Opcode::GetPropertyByName, Instruction::GetPropertyByName { .. })
        | (Opcode::GetPropertyByNameWithThis, Instruction::GetPropertyByNameWithThis { .. })
        | (Opcode::GetPropertyByValue, Instruction::GetPropertyByValue { .. })
        | (Opcode::SetPropertyByName, Instruction::SetPropertyByName { .. })
        | (Opcode::Call, Instruction::Call { .. })
        | (Opcode::Add, Instruction::Add { .. })
        | (Opcode::Sub, Instruction::Sub { .. })
        | (Opcode::Div, Instruction::Div { .. })
        | (Opcode::Mul, Instruction::Mul { .. })
        | (Opcode::BitOr, Instruction::BitOr { .. })
        | (Opcode::BitXor, Instruction::BitXor { .. })
        | (Opcode::StrictEq, Instruction::StrictEq { .. })
        | (Opcode::Inc, Instruction::Inc { .. })
        | (Opcode::Jump, Instruction::Jump { .. })
        | (Opcode::JumpIfNotLessThan, Instruction::JumpIfNotLessThan { .. })
        | (Opcode::JumpIfNotLessThanOrEqual, Instruction::JumpIfNotLessThanOrEqual { .. })
        | (Opcode::JumpIfNotGreaterThan, Instruction::JumpIfNotGreaterThan { .. })
        | (Opcode::JumpIfNotGreaterThanOrEqual, Instruction::JumpIfNotGreaterThanOrEqual { .. })
        | (Opcode::JumpIfNotEqual, Instruction::JumpIfNotEqual { .. })
        | (Opcode::JumpIfFalse, Instruction::JumpIfFalse { .. })
        | (Opcode::IncrementLoopIteration, Instruction::IncrementLoopIteration)
        | (Opcode::PureReaderLoopIteration, Instruction::PureReaderLoopIteration)
        | (Opcode::PushFromRegister, Instruction::PushFromRegister { .. })
        | (Opcode::PopIntoRegister, Instruction::PopIntoRegister { .. })
        | (Opcode::SetAccumulator, Instruction::SetAccumulator { .. })
        | (Opcode::CheckReturn, Instruction::CheckReturn)
        | (Opcode::Return, Instruction::Return) => true,
        (Opcode::PureAffineLoopIteration, Instruction::PureAffineLoopIteration)
        | (Opcode::PurePropertyWriteLoopIteration, Instruction::PurePropertyWriteLoopIteration)
        | (Opcode::PureMethodLoopIteration, Instruction::PureMethodLoopIteration)
        | (Opcode::PureGlobalAffineLoopIteration, Instruction::PureGlobalAffineLoopIteration)
        | (Opcode::PureIndexedReaderLoopIteration, Instruction::PureIndexedReaderLoopIteration) => {
            !code.pure_range_loop_observed()
        }
        _ => false,
    }
}

fn branch_target(instruction: &Instruction) -> Option<usize> {
    match instruction {
        Instruction::Jump { address }
        | Instruction::JumpIfFalse { address, .. }
        | Instruction::JumpIfNotLessThan { address, .. }
        | Instruction::JumpIfNotLessThanOrEqual { address, .. }
        | Instruction::JumpIfNotGreaterThan { address, .. }
        | Instruction::JumpIfNotGreaterThanOrEqual { address, .. }
        | Instruction::JumpIfNotEqual { address, .. } => Some(address.as_u32() as usize),
        _ => None,
    }
}

fn fallthrough(instruction: &Instruction) -> bool {
    !matches!(instruction, Instruction::Jump { .. } | Instruction::Return)
}

fn has_explicit_edges(instruction: &Instruction) -> bool {
    matches!(
        instruction,
        Instruction::Jump { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::JumpIfNotLessThan { .. }
            | Instruction::JumpIfNotLessThanOrEqual { .. }
            | Instruction::JumpIfNotGreaterThan { .. }
            | Instruction::JumpIfNotGreaterThanOrEqual { .. }
            | Instruction::JumpIfNotEqual { .. }
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
    copy_global_object_binding_register: Helper,
    guard_argument_number: Helper,
    guard_stack_number: Helper,
    copy_argument_register: Helper,
    copy_this_register: Helper,
    copy_register: Helper,
    get_register_i32: Helper,
    get_register_f64: Helper,
    push_register: Helper,
    set_return_register: Helper,
    get_argument_i32: Helper,
    get_argument_f64: Helper,
    dense_i32_guarded: Helper,
    dense_f64_guarded: Helper,
    dense_boxed_i32_guarded: Helper,
    dense_boxed_f64_guarded: Helper,
    indexed_scan_step_i32_guarded: Helper,
    indexed_wrapping_sum_i32_guarded: Helper,
    indexed_wrapping_sum_f64_guarded: Helper,
    wrapping_affine_range_i32: Helper,
    pure_reader_range_i32_guarded: Helper,
    named_boxed_guarded: Helper,
    named_i32_guarded: Helper,
    named_f64_guarded: Helper,
    bit_or_f64: Helper,
    bit_xor_f64: Helper,
    strict_eq: Helper,
    call_ordinary: Helper,
    set_property_by_name: Helper,
    set_pc: Helper,
    store_i32_if_defined: Helper,
    store_f64_if_defined: Helper,
    store_bool_i32_if_defined: Helper,
    store_bool_f64_if_defined: Helper,
    push_i32: Helper,
    push_f64: Helper,
    push_bool_i32: Helper,
    push_bool_f64: Helper,
    pop_i32: Helper,
    pop_f64: Helper,
    set_return_i32: Helper,
    set_return_f64: Helper,
    set_return_bool_i32: Helper,
    set_return_bool_f64: Helper,
    increment_loop: Helper,
    handle_return: Helper,
    consume_instruction_budget: Helper,
    refund_instruction_budget: Helper,
}

#[derive(Clone, Copy)]
struct IndexedScanStepFusion {
    compare_index: usize,
    object_move_index: usize,
    key_move_index: usize,
    property_index: usize,
    property_pc: usize,
    property_ic_index: u32,
    property_object_dst: usize,
    property_key_dst: usize,
    strict_eq_index: usize,
    branch_index: usize,
    index: usize,
    other: usize,
    other_binding_index: Option<u32>,
    other_load_index: Option<usize>,
}

#[derive(Clone, Copy)]
struct IndexedWrappingSumFusion {
    object_load_index: usize,
    object_pc: usize,
    key_move_index: usize,
    property_index: usize,
    property_pc: usize,
    add_index: usize,
    zero_index: usize,
    bit_or_index: usize,
    result_move_index: usize,
    object_binding_index: u32,
    property_ic_index: u32,
    object_dst: usize,
    key_dst: usize,
    index: usize,
    limit: usize,
    sum: usize,
}

#[derive(Clone, Copy)]
struct WrappingAffineLoopFusion {
    body_start_index: usize,
    back_edge_index: usize,
    index: usize,
    limit: usize,
    accumulator: usize,
    multiplier: i32,
    offset: i32,
}

#[derive(Clone, Copy)]
struct PureReaderLoopFusion {
    body_start_index: usize,
    back_edge_index: usize,
    body_pc: usize,
    function_binding_index: u32,
    function_ic_index: u32,
    object_binding_index: u32,
    object_ic_index: u32,
    index: usize,
    limit: usize,
    sum: usize,
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
    /// Encoded scan-step results produced by a dominating length-property
    /// block and consumed by its comparison and equality branch.
    fused_scan_step_results: HashMap<usize, cranelift_codegen::ir::Value>,
    /// Pure bytecodes already performed by a fused bulk helper.
    fused_instruction_skips: BTreeSet<usize>,
    options: NativeCompileOptions,
}

impl<'a> NativeCompiler<'a> {
    fn new(
        backend: &'a mut JitBackend,
        code: &'a CodeBlock,
        instructions: DecodedInstructions,
        mode: NativeMode,
        options: NativeCompileOptions,
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
            fused_scan_step_results: HashMap::new(),
            fused_instruction_skips: BTreeSet::new(),
            options,
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

            if self.options.accounting.instruction_budget {
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
                &[ptr, types::I32, types::I32],
                types::I64,
            ),
            copy_global_declarative_binding_register: make(
                jit_copy_global_declarative_binding_register as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            copy_global_object_binding_register: make(
                jit_copy_global_object_binding_register as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32, types::I32],
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
            copy_this_register: make(
                jit_copy_this_register as *const () as usize,
                &[ptr, types::I32],
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
            dense_i32_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_dense_array_i32_guarded as *const () as usize
                } else {
                    jit_dense_array_i32_guarded as *const () as usize
                },
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            dense_f64_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_dense_array_f64_guarded as *const () as usize
                } else {
                    jit_dense_array_f64_guarded as *const () as usize
                },
                &[ptr, types::I32, types::F64, types::I32, ptr],
                types::I64,
            ),
            dense_boxed_i32_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_dense_array_boxed_i32_guarded as *const () as usize
                } else {
                    jit_dense_array_boxed_i32_guarded as *const () as usize
                },
                &[ptr, types::I32, types::I32, types::I32, types::I32],
                types::I64,
            ),
            dense_boxed_f64_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_dense_array_boxed_f64_guarded as *const () as usize
                } else {
                    jit_dense_array_boxed_f64_guarded as *const () as usize
                },
                &[ptr, types::I32, types::F64, types::I32, types::I32],
                types::I64,
            ),
            indexed_scan_step_i32_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_indexed_scan_step_i32_guarded as *const () as usize
                } else {
                    jit_indexed_scan_step_i32_guarded as *const () as usize
                },
                &[
                    ptr,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
            indexed_wrapping_sum_i32_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_indexed_wrapping_sum_i32_guarded as *const () as usize
                } else {
                    jit_indexed_wrapping_sum_i32_guarded as *const () as usize
                },
                &[
                    ptr,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
            indexed_wrapping_sum_f64_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_indexed_wrapping_sum_f64_guarded as *const () as usize
                } else {
                    jit_indexed_wrapping_sum_f64_guarded as *const () as usize
                },
                &[
                    ptr,
                    types::I32,
                    types::F64,
                    types::F64,
                    types::F64,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
            wrapping_affine_range_i32: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_wrapping_affine_range_i32 as *const () as usize
                } else {
                    jit_wrapping_affine_range_i32 as *const () as usize
                },
                &[
                    ptr,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I32,
            ),
            pure_reader_range_i32_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_pure_reader_range_i32_guarded as *const () as usize
                } else {
                    jit_pure_reader_range_i32_guarded as *const () as usize
                },
                &[
                    ptr,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
            named_boxed_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_named_property_boxed_guarded as *const () as usize
                } else {
                    jit_named_property_boxed_guarded as *const () as usize
                },
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            named_i32_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_named_property_i32_guarded as *const () as usize
                } else {
                    jit_named_property_i32_guarded as *const () as usize
                },
                &[ptr, types::I32, types::I32],
                types::I64,
            ),
            named_f64_guarded: make(
                if self.options.diagnostics.instrument_storage {
                    jit_diagnostic_named_property_f64_guarded as *const () as usize
                } else {
                    jit_named_property_f64_guarded as *const () as usize
                },
                &[ptr, types::I32, types::I32, ptr],
                types::I64,
            ),
            bit_or_f64: make(
                jit_bit_or_f64 as *const () as usize,
                &[types::F64, types::F64],
                types::F64,
            ),
            bit_xor_f64: make(
                jit_bit_xor_f64 as *const () as usize,
                &[types::F64, types::F64],
                types::F64,
            ),
            strict_eq: make(
                jit_strict_eq as *const () as usize,
                &[ptr, types::I32, types::I32],
                types::I32,
            ),
            call_ordinary: make(
                jit_call_ordinary as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            set_property_by_name: make(
                jit_set_property_by_name as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32],
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
            store_bool_i32_if_defined: make(
                jit_store_bool_i32_if_defined as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            store_bool_f64_if_defined: make(
                jit_store_bool_f64_if_defined as *const () as usize,
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
            push_bool_i32: make(
                jit_push_bool_i32 as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            push_bool_f64: make(
                jit_push_bool_f64 as *const () as usize,
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
            set_return_bool_i32: make(
                jit_set_return_bool_i32 as *const () as usize,
                &[ptr, types::I32],
                types::I64,
            ),
            set_return_bool_f64: make(
                jit_set_return_bool_f64 as *const () as usize,
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
        let charge_instruction_budget = bcx.ins().iconst(
            types::I32,
            i64::from(self.options.accounting.instruction_budget),
        );
        let charge_loop_iterations = bcx.ins().iconst(
            types::I32,
            i64::from(self.options.accounting.loop_iterations),
        );
        let result = bcx.ins().call_indirect(
            helpers.guard.signature,
            guard,
            &[ctx, charge_instruction_budget, charge_loop_iterations],
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
        if self
            .fused_instruction_skips
            .remove(&self.current_instruction)
        {
            return true;
        }

        match instruction {
            Instruction::This { dst } => {
                let dst = register(*dst);
                if self.defined_register_kind(dst) != RegisterKind::Boxed
                    || !self.emit_materialize_live_dirty_registers(bcx, ctx, helpers)
                {
                    return false;
                }
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.copy_this_register.address as i64);
                let dst = bcx.ins().iconst(types::I32, dst as i64);
                let status = bcx.ins().call_indirect(
                    helpers.copy_this_register.signature,
                    helper,
                    &[ctx, dst],
                );
                let status = bcx.inst_results(status)[0];
                let break_mask = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
                let is_break = bcx.ins().band(status, break_mask);
                let cont = bcx.create_block();
                bcx.ins()
                    .brif(is_break, break_block, &[status.into()], cont, &[]);
                bcx.switch_to_block(cont);
            }
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
                if !self.emit_binding_read_result(bcx, ctx, helpers, pc, dst, guard) {
                    return false;
                }
            }
            Instruction::GetNameGlobal {
                dst,
                binding_index,
                ic_index,
            } => {
                let dst = register(*dst);
                let binding_index = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*binding_index)));
                let ic_index = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*ic_index)));
                let dst_value = bcx.ins().iconst(types::I32, dst as i64);
                let representation = match (self.defined_register_kind(dst), self.mode) {
                    (RegisterKind::Boxed, _) => 2,
                    (_, NativeMode::F64) => 1,
                    (_, NativeMode::I32) => 0,
                };
                let representation = bcx.ins().iconst(types::I32, representation);
                let helper = bcx.ins().iconst(
                    helpers.ptr,
                    helpers.copy_global_object_binding_register.address as i64,
                );
                let guard = bcx.ins().call_indirect(
                    helpers.copy_global_object_binding_register.signature,
                    helper,
                    &[ctx, binding_index, ic_index, dst_value, representation],
                );
                let guard = bcx.inst_results(guard)[0];
                if !self.emit_binding_read_result(bcx, ctx, helpers, pc, dst, guard) {
                    return false;
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
            Instruction::GetLengthProperty {
                dst,
                value,
                ic_index,
            }
            | Instruction::GetPropertyByName {
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
                let object_register = usize::from(*value);
                if self.register_kind(object_register) != RegisterKind::Boxed {
                    return false;
                }
                let dst = register(*dst);
                let scan_step = matches!(instruction, Instruction::GetLengthProperty { .. })
                    .then(|| self.indexed_scan_step_fusion(dst, object_register))
                    .flatten();
                if let Some(fusion) = scan_step {
                    let Some(index) = self.use_register(bcx, fusion.index) else {
                        return false;
                    };
                    let helper = bcx.ins().iconst(
                        helpers.ptr,
                        helpers.indexed_scan_step_i32_guarded.address as i64,
                    );
                    let object = bcx.ins().iconst(types::I32, object_register as i64);
                    let length_ic = bcx
                        .ins()
                        .iconst(types::I32, i64::from(u32::from(*ic_index)));
                    let property_ic = bcx
                        .ins()
                        .iconst(types::I32, i64::from(fusion.property_ic_index));
                    let other = bcx.ins().iconst(types::I32, fusion.other as i64);
                    let other_binding = bcx.ins().iconst(
                        types::I32,
                        i64::from(fusion.other_binding_index.unwrap_or(u32::MAX)),
                    );
                    let property_object_dst = bcx
                        .ins()
                        .iconst(types::I32, fusion.property_object_dst as i64);
                    let property_key_dst =
                        bcx.ins().iconst(types::I32, fusion.property_key_dst as i64);
                    self.emit_set_pc(bcx, ctx, helpers, next_pc);
                    let result = bcx.ins().call_indirect(
                        helpers.indexed_scan_step_i32_guarded.signature,
                        helper,
                        &[
                            ctx,
                            object,
                            index,
                            length_ic,
                            property_ic,
                            other,
                            other_binding,
                            property_object_dst,
                            property_key_dst,
                        ],
                    );
                    let result = bcx.inst_results(result)[0];

                    let named_deopt = bcx.create_block();
                    let dense_check = bcx.create_block();
                    let dense_deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    let named_fail_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                    let named_failed = bcx.ins().band(result, named_fail_mask);
                    bcx.ins()
                        .brif(named_failed, named_deopt, &[], dense_check, &[]);

                    bcx.switch_to_block(named_deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::NamedProperty) {
                        return false;
                    }

                    bcx.switch_to_block(dense_check);
                    let dense_fail_mask =
                        bcx.ins().iconst(types::I64, JIT_SCAN_DENSE_FAIL_BIT as i64);
                    let dense_failed = bcx.ins().band(result, dense_fail_mask);
                    bcx.ins().brif(dense_failed, dense_deopt, &[], cont, &[]);

                    bcx.switch_to_block(dense_deopt);
                    let failure_index = bcx.ins().ireduce(types::I32, result);
                    if !self.define_register(bcx, fusion.index, failure_index) {
                        return false;
                    }
                    if !self.emit_guard_deopt_preserving_vm_registers(
                        bcx,
                        ctx,
                        helpers,
                        fusion.property_pc,
                        JitExitReason::DenseElement,
                        &[fusion.property_object_dst, fusion.property_key_dst],
                    ) {
                        return false;
                    }

                    bcx.switch_to_block(cont);
                    let scan_index = bcx.ins().ireduce(types::I32, result);
                    if !self.define_register(bcx, fusion.index, scan_index) {
                        return false;
                    }
                    if self
                        .fused_scan_step_results
                        .insert(fusion.compare_index, result)
                        .is_some()
                        || self
                            .fused_scan_step_results
                            .insert(fusion.branch_index, result)
                            .is_some()
                    {
                        return false;
                    }
                    for index in [
                        fusion.object_move_index,
                        fusion.key_move_index,
                        fusion.property_index,
                        fusion.strict_eq_index,
                    ] {
                        if !self.fused_instruction_skips.insert(index) {
                            return false;
                        }
                    }
                    if let Some(index) = fusion.other_load_index
                        && !self.fused_instruction_skips.insert(index)
                    {
                        return false;
                    }
                    return true;
                }

                let object = bcx.ins().iconst(types::I32, object_register as i64);
                let ic_index = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*ic_index)));
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let boxed = self.defined_register_kind(dst) == RegisterKind::Boxed;
                let f64_output = (!boxed && self.mode == NativeMode::F64).then(|| {
                    bcx.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        3,
                    ))
                });
                let guarded_value = if boxed {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.named_boxed_guarded.address as i64);
                    let dst_value = bcx.ins().iconst(types::I32, dst as i64);
                    bcx.ins().call_indirect(
                        helpers.named_boxed_guarded.signature,
                        helper,
                        &[ctx, object, ic_index, dst_value],
                    )
                } else if self.mode == NativeMode::F64 {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.named_f64_guarded.address as i64);
                    let output = bcx.ins().stack_addr(
                        helpers.ptr,
                        f64_output.expect("F64 mode has an output slot"),
                        0,
                    );
                    bcx.ins().call_indirect(
                        helpers.named_f64_guarded.signature,
                        helper,
                        &[ctx, object, ic_index, output],
                    )
                } else {
                    let guard_helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.named_i32_guarded.address as i64);
                    bcx.ins().call_indirect(
                        helpers.named_i32_guarded.signature,
                        guard_helper,
                        &[ctx, object, ic_index],
                    )
                };
                let guarded_value = bcx.inst_results(guarded_value)[0];
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                if boxed || self.mode == NativeMode::F64 {
                    bcx.ins().brif(guarded_value, cont, &[], deopt, &[]);
                } else {
                    let fail_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                    let failed = bcx.ins().band(guarded_value, fail_mask);
                    bcx.ins().brif(failed, deopt, &[], cont, &[]);
                }
                bcx.switch_to_block(deopt);
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::NamedProperty) {
                    return false;
                }
                bcx.switch_to_block(cont);
                if boxed {
                    // The helper copied a traced `JsValue` directly between
                    // VM registers. Generic post-instruction bookkeeping
                    // clears any stale native definedness for this boxed dst.
                } else if self.mode == NativeMode::F64 {
                    let result = bcx.ins().stack_load(
                        types::F64,
                        f64_output.expect("F64 mode has an output slot"),
                        0,
                    );
                    if !self.define_register(bcx, dst, result) {
                        return false;
                    }
                } else {
                    let value = bcx.ins().ireduce(types::I32, guarded_value);
                    if !self.define_register(bcx, dst, value) {
                        return false;
                    }
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
                let boxed = self.defined_register_kind(dst) == RegisterKind::Boxed;
                let f64_output = (!boxed && self.mode == NativeMode::F64).then(|| {
                    bcx.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        3,
                    ))
                });
                let guarded_value = if boxed {
                    let dst_value = bcx.ins().iconst(types::I32, dst as i64);
                    let helper = if self.mode == NativeMode::F64 {
                        helpers.dense_boxed_f64_guarded
                    } else {
                        helpers.dense_boxed_i32_guarded
                    };
                    let helper_address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
                    bcx.ins().call_indirect(
                        helper.signature,
                        helper_address,
                        &[ctx, object, key, ic_index, dst_value],
                    )
                } else if self.mode == NativeMode::F64 {
                    let helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.dense_f64_guarded.address as i64);
                    let output = bcx.ins().stack_addr(
                        helpers.ptr,
                        f64_output.expect("F64 mode has an output slot"),
                        0,
                    );
                    bcx.ins().call_indirect(
                        helpers.dense_f64_guarded.signature,
                        helper,
                        &[ctx, object, key, ic_index, output],
                    )
                } else {
                    let guard_helper = bcx
                        .ins()
                        .iconst(helpers.ptr, helpers.dense_i32_guarded.address as i64);
                    bcx.ins().call_indirect(
                        helpers.dense_i32_guarded.signature,
                        guard_helper,
                        &[ctx, object, key, ic_index],
                    )
                };
                let guarded_value = bcx.inst_results(guarded_value)[0];
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                if boxed || self.mode == NativeMode::F64 {
                    bcx.ins().brif(guarded_value, cont, &[], deopt, &[]);
                } else {
                    let fail_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                    let failed = bcx.ins().band(guarded_value, fail_mask);
                    bcx.ins().brif(failed, deopt, &[], cont, &[]);
                }
                bcx.switch_to_block(deopt);
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::DenseElement) {
                    return false;
                }
                bcx.switch_to_block(cont);
                if boxed {
                    // The helper copied the traced value directly into the VM
                    // register. Generic bookkeeping clears stale native state.
                } else if self.mode == NativeMode::F64 {
                    let result = bcx.ins().stack_load(
                        types::F64,
                        f64_output.expect("F64 mode has an output slot"),
                        0,
                    );
                    if !self.define_register(bcx, dst, result) {
                        return false;
                    }
                } else {
                    let value = bcx.ins().ireduce(types::I32, guarded_value);
                    if !self.define_register(bcx, dst, value) {
                        return false;
                    }
                }
            }
            Instruction::SetPropertyByName {
                value,
                object,
                ic_index,
            } => {
                // The canonical setter reads its operands from the VM register
                // file. The assigned value is commonly dead immediately after
                // this instruction, so publish it in addition to the values
                // that a re-entrant setter can observe after returning.
                if !self.emit_materialize_live_dirty_registers_with(
                    bcx,
                    ctx,
                    helpers,
                    &[usize::from(*value)],
                ) {
                    return false;
                }
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.set_property_by_name.address as i64);
                let value = bcx.ins().iconst(types::I32, usize::from(*value) as i64);
                let object = bcx.ins().iconst(types::I32, usize::from(*object) as i64);
                let ic_index = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*ic_index)));
                let status = bcx.ins().call_indirect(
                    helpers.set_property_by_name.signature,
                    helper,
                    &[ctx, value, object, ic_index],
                );
                let status = bcx.inst_results(status)[0];
                let break_mask = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
                let is_break = bcx.ins().band(status, break_mask);
                let cont = bcx.create_block();
                bcx.ins()
                    .brif(is_break, break_block, &[status.into()], cont, &[]);
                bcx.switch_to_block(cont);
            }
            Instruction::Call { argument_count } => {
                // `function_call` can allocate, invoke host code, trigger GC,
                // and execute nested frames. Publish dirty primitives that are
                // live across the call so the VM owns the complete observable
                // caller state throughout that safepoint. Boxed registers
                // already live in traced VM slots.
                if !self.emit_materialize_live_dirty_registers(bcx, ctx, helpers) {
                    return false;
                }
                // The helper leaves the calling-convention stack untouched on
                // a non-ordinary callee. That makes this a pre-effect guard
                // exit: the interpreter can re-execute the Call opcode with
                // its normal generic-call semantics.
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.call_ordinary.address as i64);
                let argument_count = bcx
                    .ins()
                    .iconst(types::I32, i64::from(u32::from(*argument_count)));
                let status = bcx.ins().call_indirect(
                    helpers.call_ordinary.signature,
                    helper,
                    &[ctx, argument_count],
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
                let transition_check = bcx.create_block();
                bcx.ins()
                    .brif(guard_failed, deopt, &[], transition_check, &[]);

                bcx.switch_to_block(deopt);
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::CallTarget) {
                    return false;
                }

                bcx.switch_to_block(transition_check);
                let has_transition = bcx.ins().icmp_imm(IntCC::NotEqual, status, 0);
                let transition = bcx.create_block();
                let called = bcx.create_block();
                bcx.ins().brif(has_transition, transition, &[], called, &[]);

                bcx.switch_to_block(transition);
                bcx.ins().return_(&[status]);

                bcx.switch_to_block(called);
            }
            Instruction::BitOr { dst, lhs, rhs } | Instruction::BitXor { dst, lhs, rhs } => {
                let Some(lhs) = self.use_register(bcx, register(*lhs)) else {
                    return false;
                };
                let Some(rhs) = self.use_register(bcx, register(*rhs)) else {
                    return false;
                };
                let result = match self.mode {
                    NativeMode::I32 => match instruction {
                        Instruction::BitOr { .. } => bcx.ins().bor(lhs, rhs),
                        Instruction::BitXor { .. } => bcx.ins().bxor(lhs, rhs),
                        _ => unreachable!("the enclosing pattern is a binary bitwise opcode"),
                    },
                    NativeMode::F64 => {
                        let helper = match instruction {
                            Instruction::BitOr { .. } => helpers.bit_or_f64,
                            Instruction::BitXor { .. } => helpers.bit_xor_f64,
                            _ => unreachable!("the enclosing pattern is a binary bitwise opcode"),
                        };
                        let helper_address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
                        let call =
                            bcx.ins()
                                .call_indirect(helper.signature, helper_address, &[lhs, rhs]);
                        bcx.inst_results(call)[0]
                    }
                };
                if !self.define_register(bcx, register(*dst), result) {
                    return false;
                }
            }
            Instruction::StrictEq { dst, lhs, rhs } => {
                if self.register_kind(usize::from(*lhs)) != RegisterKind::Boxed
                    || self.register_kind(usize::from(*rhs)) != RegisterKind::Boxed
                {
                    return false;
                }
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.strict_eq.address as i64);
                let lhs = bcx.ins().iconst(types::I32, usize::from(*lhs) as i64);
                let rhs = bcx.ins().iconst(types::I32, usize::from(*rhs) as i64);
                let call =
                    bcx.ins()
                        .call_indirect(helpers.strict_eq.signature, helper, &[ctx, lhs, rhs]);
                let result = bcx.inst_results(call)[0];
                let result = if self.mode == NativeMode::F64 {
                    bcx.ins().fcvt_from_uint(types::F64, result)
                } else {
                    result
                };
                if !self.define_register(bcx, register(*dst), result) {
                    return false;
                }
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
                } else if self.i32_arithmetic_wraps(instruction) {
                    bcx.ins().iadd(lhs, rhs)
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
                } else if self.i32_arithmetic_wraps(instruction) {
                    bcx.ins().isub(lhs, rhs)
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
                } else if self.i32_arithmetic_wraps(instruction) {
                    bcx.ins().imul(lhs, rhs)
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
            Instruction::JumpIfFalse { address, value } => {
                if let Some(result) = self
                    .fused_scan_step_results
                    .remove(&self.current_instruction)
                {
                    let match_mask = bcx.ins().iconst(types::I64, JIT_SCAN_MATCH_BIT as i64);
                    let is_match = bcx.ins().band(result, match_mask);
                    let Some(target) = self.target_block(*address, blocks) else {
                        return false;
                    };
                    let Some(fallthrough) = self.next_block(next_pc, blocks) else {
                        return false;
                    };
                    bcx.ins().brif(is_match, fallthrough, &[], target, &[]);
                    return true;
                }
                let Some(value) = self.use_register(bcx, register(*value)) else {
                    return false;
                };
                let Some(target) = self.target_block(*address, blocks) else {
                    return false;
                };
                let Some(fallthrough) = self.next_block(next_pc, blocks) else {
                    return false;
                };
                let is_true = if self.mode == NativeMode::F64 {
                    let zero = bcx.ins().f64const(0.0);
                    bcx.ins().fcmp(FloatCC::NotEqual, value, zero)
                } else {
                    bcx.ins().icmp_imm(IntCC::NotEqual, value, 0)
                };
                bcx.ins().brif(is_true, fallthrough, &[], target, &[]);
            }
            Instruction::JumpIfNotLessThan { address, lhs, rhs } => {
                if let Some(fusion) = self.pure_reader_loop_fusion(register(*lhs), register(*rhs)) {
                    for instruction_index in fusion.body_start_index..fusion.back_edge_index {
                        if !self.fused_instruction_skips.insert(instruction_index) {
                            return false;
                        }
                    }

                    let Some(index) = self.use_register(bcx, fusion.index) else {
                        return false;
                    };
                    let Some(limit) = self.use_register(bcx, fusion.limit) else {
                        return false;
                    };
                    let Some(sum) = self.use_register(bcx, fusion.sum) else {
                        return false;
                    };
                    let Some(target) = self.target_block(*address, blocks) else {
                        return false;
                    };

                    let apply = bcx.create_block();
                    let has_iterations = bcx.ins().icmp(IntCC::SignedLessThan, index, limit);
                    bcx.ins().brif(has_iterations, apply, &[], target, &[]);
                    bcx.switch_to_block(apply);

                    let guarded = helpers.pure_reader_range_i32_guarded;
                    let helper = bcx.ins().iconst(helpers.ptr, guarded.address as i64);
                    let function_binding = bcx
                        .ins()
                        .iconst(types::I32, i64::from(fusion.function_binding_index));
                    let function_ic = bcx
                        .ins()
                        .iconst(types::I32, i64::from(fusion.function_ic_index));
                    let object_binding = bcx
                        .ins()
                        .iconst(types::I32, i64::from(fusion.object_binding_index));
                    let object_ic = bcx
                        .ins()
                        .iconst(types::I32, i64::from(fusion.object_ic_index));
                    let result = bcx.ins().call_indirect(
                        guarded.signature,
                        helper,
                        &[
                            ctx,
                            function_binding,
                            function_ic,
                            object_binding,
                            object_ic,
                            index,
                            limit,
                            sum,
                        ],
                    );
                    let result = bcx.inst_results(result)[0];

                    let deopt = bcx.create_block();
                    let applied_check = bcx.create_block();
                    let guard_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                    let guard_failed = bcx.ins().band(result, guard_mask);
                    bcx.ins().brif(guard_failed, deopt, &[], applied_check, &[]);

                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(
                        bcx,
                        ctx,
                        helpers,
                        fusion.body_pc,
                        JitExitReason::CallTarget,
                    ) {
                        return false;
                    }

                    bcx.switch_to_block(applied_check);
                    let applied_mask = bcx.ins().iconst(types::I64, JIT_SUM_APPLIED_BIT as i64);
                    let was_applied = bcx.ins().band(result, applied_mask);
                    let applied = bcx.create_block();
                    bcx.ins().brif(was_applied, applied, &[], target, &[]);

                    bcx.switch_to_block(applied);
                    let reduced_sum = bcx.ins().ireduce(types::I32, result);
                    if !self.define_register(bcx, fusion.sum, reduced_sum)
                        || !self.define_register(bcx, fusion.index, limit)
                    {
                        return false;
                    }
                    bcx.ins().jump(target, &[]);
                    return true;
                }
                if let Some(fusion) =
                    self.wrapping_affine_loop_fusion(register(*lhs), register(*rhs))
                {
                    for instruction_index in fusion.body_start_index..fusion.back_edge_index {
                        if !self.fused_instruction_skips.insert(instruction_index) {
                            return false;
                        }
                    }

                    let Some(index) = self.use_register(bcx, fusion.index) else {
                        return false;
                    };
                    let Some(limit) = self.use_register(bcx, fusion.limit) else {
                        return false;
                    };
                    let Some(accumulator) = self.use_register(bcx, fusion.accumulator) else {
                        return false;
                    };
                    let Some(target) = self.target_block(*address, blocks) else {
                        return false;
                    };

                    let apply = bcx.create_block();
                    let has_iterations = bcx.ins().icmp(IntCC::SignedLessThan, index, limit);
                    bcx.ins().brif(has_iterations, apply, &[], target, &[]);
                    bcx.switch_to_block(apply);

                    let helper = bcx.ins().iconst(
                        helpers.ptr,
                        helpers.wrapping_affine_range_i32.address as i64,
                    );
                    let multiplier = bcx.ins().iconst(types::I32, i64::from(fusion.multiplier));
                    let offset = bcx.ins().iconst(types::I32, i64::from(fusion.offset));
                    let result = bcx.ins().call_indirect(
                        helpers.wrapping_affine_range_i32.signature,
                        helper,
                        &[ctx, index, limit, accumulator, multiplier, offset],
                    );
                    let result = bcx.inst_results(result)[0];
                    if !self.define_register(bcx, fusion.accumulator, result)
                        || !self.define_register(bcx, fusion.index, limit)
                    {
                        return false;
                    }
                    bcx.ins().jump(target, &[]);
                    return true;
                }
                if let Some(fusion) =
                    self.indexed_wrapping_sum_fusion(register(*lhs), register(*rhs))
                {
                    for index in [
                        fusion.object_load_index,
                        fusion.key_move_index,
                        fusion.property_index,
                        fusion.add_index,
                        fusion.zero_index,
                        fusion.bit_or_index,
                        fusion.result_move_index,
                    ] {
                        if !self.fused_instruction_skips.insert(index) {
                            return false;
                        }
                    }

                    let Some(index) = self.use_register(bcx, fusion.index) else {
                        return false;
                    };
                    let Some(limit) = self.use_register(bcx, fusion.limit) else {
                        return false;
                    };
                    let Some(sum) = self.use_register(bcx, fusion.sum) else {
                        return false;
                    };
                    let Some(target) = self.target_block(*address, blocks) else {
                        return false;
                    };
                    let sum_helper = match self.mode {
                        NativeMode::I32 => helpers.indexed_wrapping_sum_i32_guarded,
                        NativeMode::F64 => helpers.indexed_wrapping_sum_f64_guarded,
                    };
                    let helper = bcx.ins().iconst(helpers.ptr, sum_helper.address as i64);
                    let object_binding = bcx
                        .ins()
                        .iconst(types::I32, i64::from(fusion.object_binding_index));
                    let property_ic = bcx
                        .ins()
                        .iconst(types::I32, i64::from(fusion.property_ic_index));
                    let object_dst = bcx.ins().iconst(types::I32, fusion.object_dst as i64);
                    let key_dst = bcx.ins().iconst(types::I32, fusion.key_dst as i64);
                    self.emit_set_pc(bcx, ctx, helpers, fusion.object_pc);
                    let result = bcx.ins().call_indirect(
                        sum_helper.signature,
                        helper,
                        &[
                            ctx,
                            object_binding,
                            index,
                            limit,
                            sum,
                            property_ic,
                            object_dst,
                            key_dst,
                        ],
                    );
                    let result = bcx.inst_results(result)[0];

                    let binding_deopt = bcx.create_block();
                    let dense_check = bcx.create_block();
                    let dense_deopt = bcx.create_block();
                    let applied_check = bcx.create_block();
                    let applied = bcx.create_block();
                    let binding_fail_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                    let binding_failed = bcx.ins().band(result, binding_fail_mask);
                    bcx.ins()
                        .brif(binding_failed, binding_deopt, &[], dense_check, &[]);

                    bcx.switch_to_block(binding_deopt);
                    if !self.emit_guard_deopt(
                        bcx,
                        ctx,
                        helpers,
                        fusion.object_pc,
                        JitExitReason::BindingRead,
                    ) {
                        return false;
                    }

                    bcx.switch_to_block(dense_check);
                    let dense_fail_mask =
                        bcx.ins().iconst(types::I64, JIT_SCAN_DENSE_FAIL_BIT as i64);
                    let dense_failed = bcx.ins().band(result, dense_fail_mask);
                    bcx.ins()
                        .brif(dense_failed, dense_deopt, &[], applied_check, &[]);

                    bcx.switch_to_block(dense_deopt);
                    if !self.emit_guard_deopt_preserving_vm_registers(
                        bcx,
                        ctx,
                        helpers,
                        fusion.property_pc,
                        JitExitReason::DenseElement,
                        &[fusion.object_dst, fusion.key_dst],
                    ) {
                        return false;
                    }

                    bcx.switch_to_block(applied_check);
                    let applied_mask = bcx.ins().iconst(types::I64, JIT_SUM_APPLIED_BIT as i64);
                    let was_applied = bcx.ins().band(result, applied_mask);
                    bcx.ins().brif(was_applied, applied, &[], target, &[]);

                    bcx.switch_to_block(applied);
                    let reduced_sum = bcx.ins().ireduce(types::I32, result);
                    let reduced_sum = if self.mode == NativeMode::F64 {
                        bcx.ins().fcvt_from_sint(types::F64, reduced_sum)
                    } else {
                        reduced_sum
                    };
                    if !self.define_register(bcx, fusion.sum, reduced_sum)
                        || !self.define_register(bcx, fusion.index, limit)
                    {
                        return false;
                    }
                    bcx.ins().jump(target, &[]);
                    return true;
                }
                if let Some(result) = self
                    .fused_scan_step_results
                    .remove(&self.current_instruction)
                {
                    let match_mask = bcx.ins().iconst(types::I64, JIT_SCAN_MATCH_BIT as i64);
                    let is_match = bcx.ins().band(result, match_mask);
                    let Some(target) = self.target_block(*address, blocks) else {
                        return false;
                    };
                    let Some(fallthrough) = self.next_block(next_pc, blocks) else {
                        return false;
                    };
                    bcx.ins().brif(is_match, fallthrough, &[], target, &[]);
                    return true;
                }
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
            Instruction::IncrementLoopIteration
            | Instruction::PureReaderLoopIteration
            | Instruction::PureAffineLoopIteration
            | Instruction::PurePropertyWriteLoopIteration
            | Instruction::PureMethodLoopIteration
            | Instruction::PureGlobalAffineLoopIteration
            | Instruction::PureIndexedReaderLoopIteration => {
                if !self.options.accounting.loop_iterations {
                    return true;
                }
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
                    let helper = match (self.register_kind(src), self.mode) {
                        (RegisterKind::Numeric, NativeMode::I32) => helpers.push_i32,
                        (RegisterKind::Numeric, NativeMode::F64) => helpers.push_f64,
                        (RegisterKind::Boolean, NativeMode::I32) => helpers.push_bool_i32,
                        (RegisterKind::Boolean, NativeMode::F64) => helpers.push_bool_f64,
                        (RegisterKind::Boxed, _) => unreachable!("handled above"),
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
                    let helper = match (self.register_kind(src), self.mode) {
                        (RegisterKind::Numeric, NativeMode::I32) => helpers.set_return_i32,
                        (RegisterKind::Numeric, NativeMode::F64) => helpers.set_return_f64,
                        (RegisterKind::Boolean, NativeMode::I32) => helpers.set_return_bool_i32,
                        (RegisterKind::Boolean, NativeMode::F64) => helpers.set_return_bool_f64,
                        (RegisterKind::Boxed, _) => unreachable!("handled above"),
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

    /// Whether this arithmetic result is consumed immediately by an exact
    /// `| 0` coercion. In the i32 specialization, wrapping add/sub already
    /// produce the ECMAScript `ToInt32` result because two i32 inputs have an
    /// exactly representable Number sum. Multiplication is equivalent only
    /// when a nearby integer constant keeps the Number product below 2^53.
    ///
    /// Keep the proof deliberately local: the bytecompiler emits the coercion
    /// as arithmetic, `StoreZero`, `BitOr`. Anything else retains the ordinary
    /// overflow deopt instead of relying on wider data-flow assumptions.
    fn i32_arithmetic_wraps(&self, instruction: &Instruction) -> bool {
        i32_arithmetic_coercion(&self.instructions, self.current_instruction, instruction)
    }

    /// Match the bytecompiler's canonical `sum += reader(object)` loop.
    ///
    /// The caller proof owns the complete call-convention sequence and loop
    /// maintenance tail. The callee is deliberately resolved and proved at
    /// runtime: a mutable global may name a different function or object on
    /// every entry, and only the currently installed ordinary function may
    /// authorize the reduction. The guarded helper accepts a linear i32
    /// expression over cached data properties and otherwise exits before the
    /// first body bytecode.
    fn pure_reader_loop_fusion(&self, index: usize, limit: usize) -> Option<PureReaderLoopFusion> {
        if self.mode != NativeMode::I32
            || self.options.accounting.instruction_budget
            || self.options.accounting.loop_iterations
        {
            return None;
        }

        let body_start_index = self.current_instruction.checked_add(1)?;
        let this_push_index = self.current_instruction.checked_add(2)?;
        let function_load_index = self.current_instruction.checked_add(3)?;
        let function_push_index = self.current_instruction.checked_add(4)?;
        let object_load_index = self.current_instruction.checked_add(5)?;
        let object_push_index = self.current_instruction.checked_add(6)?;
        let call_index = self.current_instruction.checked_add(7)?;
        let pop_index = self.current_instruction.checked_add(8)?;
        let add_index = self.current_instruction.checked_add(9)?;
        let result_move_index = self.current_instruction.checked_add(10)?;
        let back_edge_index = self.current_instruction.checked_add(11)?;

        let (body_pc, _, Instruction::Move { src, dst }) =
            self.instructions.instructions.get(body_start_index)?
        else {
            return None;
        };
        let sum = usize::from(*src);
        let saved_sum = usize::from(*dst);

        let (_, _, Instruction::PushFromRegister { src: this_register }) =
            self.instructions.instructions.get(this_push_index)?
        else {
            return None;
        };
        let this_register = usize::from(*this_register);

        let binding_read = |instruction_index: usize| {
            let (_, _, instruction) = self.instructions.instructions.get(instruction_index)?;
            match instruction {
                Instruction::GetName { dst, binding_index } => {
                    let binding_index = u32::from(*binding_index);
                    self.code
                        .bindings
                        .get(binding_index as usize)
                        .filter(|binding| {
                            binding.scope() == BindingLocatorScope::GlobalDeclarative
                        })?;
                    Some((usize::from(*dst), binding_index, JIT_GLOBAL_DECLARATIVE_IC))
                }
                Instruction::GetNameGlobal {
                    dst,
                    binding_index,
                    ic_index,
                } => {
                    let binding_index = u32::from(*binding_index);
                    self.code
                        .bindings
                        .get(binding_index as usize)
                        .filter(|binding| binding.scope() == BindingLocatorScope::GlobalObject)?;
                    Some((usize::from(*dst), binding_index, u32::from(*ic_index)))
                }
                _ => None,
            }
        };
        let (function_register, function_binding_index, function_ic_index) =
            binding_read(function_load_index)?;
        let (object_register, object_binding_index, object_ic_index) =
            binding_read(object_load_index)?;

        let (_, _, Instruction::PushFromRegister { src }) =
            self.instructions.instructions.get(function_push_index)?
        else {
            return None;
        };
        if usize::from(*src) != function_register {
            return None;
        }
        let (_, _, Instruction::PushFromRegister { src }) =
            self.instructions.instructions.get(object_push_index)?
        else {
            return None;
        };
        if usize::from(*src) != object_register {
            return None;
        }
        let (_, _, Instruction::Call { argument_count }) =
            self.instructions.instructions.get(call_index)?
        else {
            return None;
        };
        if u32::from(*argument_count) != 1 {
            return None;
        }
        let (_, _, Instruction::PopIntoRegister { dst }) =
            self.instructions.instructions.get(pop_index)?
        else {
            return None;
        };
        let reader_result = usize::from(*dst);

        let (_, _, Instruction::Add { dst, lhs, rhs }) =
            self.instructions.instructions.get(add_index)?
        else {
            return None;
        };
        let add_result = usize::from(*dst);
        let lhs = usize::from(*lhs);
        let rhs = usize::from(*rhs);
        if !((lhs == saved_sum && rhs == reader_result)
            || (rhs == saved_sum && lhs == reader_result))
        {
            return None;
        }
        let (_, _, Instruction::Move { src, dst }) =
            self.instructions.instructions.get(result_move_index)?
        else {
            return None;
        };
        if usize::from(*src) != add_result || usize::from(*dst) != sum {
            return None;
        }

        let (
            _,
            _,
            Instruction::Jump {
                address: back_edge_address,
            },
        ) = self.instructions.instructions.get(back_edge_index)?
        else {
            return None;
        };
        let loop_iteration_index = self.current_instruction.checked_sub(2)?;
        let index_increment_index = self.current_instruction.checked_sub(1)?;
        let (loop_iteration_pc, loop_iteration_next_pc, loop_iteration) =
            self.instructions.instructions.get(loop_iteration_index)?;
        if !matches!(
            loop_iteration,
            Instruction::IncrementLoopIteration
                | Instruction::PureReaderLoopIteration
                | Instruction::PureAffineLoopIteration
                | Instruction::PurePropertyWriteLoopIteration
                | Instruction::PureMethodLoopIteration
                | Instruction::PureGlobalAffineLoopIteration
                | Instruction::PureIndexedReaderLoopIteration
        ) || self
            .instructions
            .pc_to_index
            .get(loop_iteration_next_pc)
            .copied()
            != Some(index_increment_index)
            || back_edge_address.as_u32() as usize != *loop_iteration_pc
        {
            return None;
        }
        let (_, increment_next_pc, Instruction::Inc { src, dst }) =
            self.instructions.instructions.get(index_increment_index)?
        else {
            return None;
        };
        if usize::from(*src) != index
            || usize::from(*dst) != index
            || self
                .instructions
                .pc_to_index
                .get(increment_next_pc)
                .copied()
                != Some(self.current_instruction)
        {
            return None;
        }

        let (_, _, Instruction::JumpIfNotLessThan { address, .. }) = self
            .instructions
            .instructions
            .get(self.current_instruction)?
        else {
            return None;
        };
        if self
            .instructions
            .pc_to_index
            .get(&(address.as_u32() as usize))
            .copied()
            .is_none_or(|exit_index| exit_index <= back_edge_index)
        {
            return None;
        }
        for instruction_index in self.current_instruction..back_edge_index {
            let (_, next_pc, _) = self.instructions.instructions.get(instruction_index)?;
            if self.instructions.pc_to_index.get(next_pc).copied()? != instruction_index + 1 {
                return None;
            }
        }
        for instruction_index in body_start_index..=back_edge_index {
            let (pc, _, _) = self.instructions.instructions.get(instruction_index)?;
            if self
                .instructions
                .instructions
                .iter()
                .any(|(_, _, instruction)| branch_target(instruction) == Some(*pc))
            {
                return None;
            }
        }

        let source_registers = [index, limit, sum];
        if source_registers
            .iter()
            .enumerate()
            .any(|(position, register)| source_registers[..position].contains(register))
        {
            return None;
        }
        let temporary_registers = [
            saved_sum,
            function_register,
            object_register,
            reader_result,
            add_result,
        ];
        if source_registers
            .iter()
            .any(|source| temporary_registers.contains(source))
        {
            return None;
        }

        let kind_before = |instruction_index: usize, register: usize| {
            self.analysis
                .before
                .get(instruction_index)
                .and_then(|kinds| kinds.get(register))
                .copied()
        };
        let kind_after = |instruction_index: usize, register: usize| {
            self.analysis
                .after
                .get(instruction_index)
                .and_then(|kinds| kinds.get(register))
                .copied()
        };
        if source_registers.iter().any(|register| {
            kind_before(self.current_instruction, *register) != Some(RegisterKind::Numeric)
        }) || kind_before(this_push_index, this_register) != Some(RegisterKind::Boxed)
            || kind_after(body_start_index, saved_sum) != Some(RegisterKind::Numeric)
            || kind_after(function_load_index, function_register) != Some(RegisterKind::Boxed)
            || kind_after(object_load_index, object_register) != Some(RegisterKind::Boxed)
            || kind_after(pop_index, reader_result) != Some(RegisterKind::Numeric)
            || kind_after(add_index, add_result) != Some(RegisterKind::Numeric)
            || kind_after(result_move_index, sum) != Some(RegisterKind::Numeric)
        {
            return None;
        }

        let live_after_result = self.analysis.live_after.get(result_move_index)?;
        if temporary_registers
            .iter()
            .any(|temporary| live_after_result.contains(temporary))
        {
            return None;
        }

        Some(PureReaderLoopFusion {
            body_start_index,
            back_edge_index,
            body_pc: *body_pc,
            function_binding_index,
            function_ic_index,
            object_binding_index,
            object_ic_index,
            index,
            limit,
            sum,
        })
    }

    /// Match a pure wrapping-affine integer loop of the canonical form:
    ///
    /// `acc = (acc + index) | 0;`
    /// `acc = (acc * multiplier) | 0;`
    /// `acc = (acc - offset) | 0;`
    ///
    /// Repeated applications are one affine transform over the ring of
    /// 32-bit integers, so a logarithmic matrix exponentiation can replace the
    /// complete range. The matcher owns the entire body and maintenance tail;
    /// any side effect, alternate entry, live temporary, or runtime accounting
    /// keeps the ordinary native loop.
    fn wrapping_affine_loop_fusion(
        &self,
        index: usize,
        limit: usize,
    ) -> Option<WrappingAffineLoopFusion> {
        if self.mode != NativeMode::I32
            || self.options.accounting.instruction_budget
            || self.options.accounting.loop_iterations
        {
            return None;
        }

        let body_start_index = self.current_instruction.checked_add(1)?;
        let add_zero_index = self.current_instruction.checked_add(2)?;
        let add_or_index = self.current_instruction.checked_add(3)?;
        let add_move_index = self.current_instruction.checked_add(4)?;
        let multiplier_index = self.current_instruction.checked_add(5)?;
        let multiply_index = self.current_instruction.checked_add(6)?;
        let multiply_zero_index = self.current_instruction.checked_add(7)?;
        let multiply_or_index = self.current_instruction.checked_add(8)?;
        let multiply_move_index = self.current_instruction.checked_add(9)?;
        let offset_index = self.current_instruction.checked_add(10)?;
        let subtract_index = self.current_instruction.checked_add(11)?;
        let subtract_zero_index = self.current_instruction.checked_add(12)?;
        let subtract_or_index = self.current_instruction.checked_add(13)?;
        let subtract_move_index = self.current_instruction.checked_add(14)?;
        let back_edge_index = self.current_instruction.checked_add(15)?;

        let (_, _, Instruction::Add { dst, lhs, rhs }) =
            self.instructions.instructions.get(body_start_index)?
        else {
            return None;
        };
        let add_dst = usize::from(*dst);
        let add_lhs = usize::from(*lhs);
        let add_rhs = usize::from(*rhs);
        let accumulator = if add_lhs == index {
            add_rhs
        } else if add_rhs == index {
            add_lhs
        } else {
            return None;
        };

        let (_, _, Instruction::StoreZero { dst: add_zero }) =
            self.instructions.instructions.get(add_zero_index)?
        else {
            return None;
        };
        let add_zero = usize::from(*add_zero);
        let (_, _, Instruction::BitOr { dst, lhs, rhs }) =
            self.instructions.instructions.get(add_or_index)?
        else {
            return None;
        };
        let add_or = usize::from(*dst);
        if add_dst == add_zero
            || !((usize::from(*lhs) == add_dst && usize::from(*rhs) == add_zero)
                || (usize::from(*rhs) == add_dst && usize::from(*lhs) == add_zero))
        {
            return None;
        }
        let (_, _, Instruction::Move { src, dst }) =
            self.instructions.instructions.get(add_move_index)?
        else {
            return None;
        };
        if usize::from(*src) != add_or || usize::from(*dst) != accumulator {
            return None;
        }

        let (_, _, multiplier_instruction) =
            self.instructions.instructions.get(multiplier_index)?;
        let (multiplier_register, multiplier) = i32_constant_definition(multiplier_instruction)?;
        let (_, _, Instruction::Mul { dst, lhs, rhs }) =
            self.instructions.instructions.get(multiply_index)?
        else {
            return None;
        };
        let multiply_dst = usize::from(*dst);
        if !((usize::from(*lhs) == accumulator && usize::from(*rhs) == multiplier_register)
            || (usize::from(*rhs) == accumulator && usize::from(*lhs) == multiplier_register))
        {
            return None;
        }
        let (_, _, Instruction::StoreZero { dst: multiply_zero }) =
            self.instructions.instructions.get(multiply_zero_index)?
        else {
            return None;
        };
        let multiply_zero = usize::from(*multiply_zero);
        let (_, _, Instruction::BitOr { dst, lhs, rhs }) =
            self.instructions.instructions.get(multiply_or_index)?
        else {
            return None;
        };
        let multiply_or = usize::from(*dst);
        if multiply_dst == multiply_zero
            || !((usize::from(*lhs) == multiply_dst && usize::from(*rhs) == multiply_zero)
                || (usize::from(*rhs) == multiply_dst && usize::from(*lhs) == multiply_zero))
        {
            return None;
        }
        let (_, _, Instruction::Move { src, dst }) =
            self.instructions.instructions.get(multiply_move_index)?
        else {
            return None;
        };
        if usize::from(*src) != multiply_or || usize::from(*dst) != accumulator {
            return None;
        }

        let (_, _, offset_instruction) = self.instructions.instructions.get(offset_index)?;
        let (offset_register, offset) = i32_constant_definition(offset_instruction)?;
        let (_, _, Instruction::Sub { dst, lhs, rhs }) =
            self.instructions.instructions.get(subtract_index)?
        else {
            return None;
        };
        let subtract_dst = usize::from(*dst);
        if usize::from(*lhs) != accumulator || usize::from(*rhs) != offset_register {
            return None;
        }
        let (_, _, Instruction::StoreZero { dst: subtract_zero }) =
            self.instructions.instructions.get(subtract_zero_index)?
        else {
            return None;
        };
        let subtract_zero = usize::from(*subtract_zero);
        let (_, _, Instruction::BitOr { dst, lhs, rhs }) =
            self.instructions.instructions.get(subtract_or_index)?
        else {
            return None;
        };
        let subtract_or = usize::from(*dst);
        if subtract_dst == subtract_zero
            || !((usize::from(*lhs) == subtract_dst && usize::from(*rhs) == subtract_zero)
                || (usize::from(*rhs) == subtract_dst && usize::from(*lhs) == subtract_zero))
        {
            return None;
        }
        let (_, _, Instruction::Move { src, dst }) =
            self.instructions.instructions.get(subtract_move_index)?
        else {
            return None;
        };
        if usize::from(*src) != subtract_or || usize::from(*dst) != accumulator {
            return None;
        }

        let (_, _, Instruction::Jump { address }) =
            self.instructions.instructions.get(back_edge_index)?
        else {
            return None;
        };
        let comparison_entry_index = self
            .current_instruction
            .checked_sub(1)
            .filter(|reload_index| {
                let Some((_, reload_next_pc, Instruction::GetName { dst, binding_index })) =
                    self.instructions.instructions.get(*reload_index)
                else {
                    return false;
                };
                usize::from(*dst) == limit
                    && self
                        .code
                        .bindings
                        .get(usize::from(*binding_index))
                        .is_some_and(|binding| {
                            binding.scope() == BindingLocatorScope::GlobalDeclarative
                        })
                    && self.instructions.pc_to_index.get(reload_next_pc).copied()
                        == Some(self.current_instruction)
            })
            .unwrap_or(self.current_instruction);
        let loop_iteration_index = comparison_entry_index.checked_sub(2)?;
        let index_increment_index = comparison_entry_index.checked_sub(1)?;
        let (loop_iteration_pc, loop_iteration_next_pc, loop_iteration) =
            self.instructions.instructions.get(loop_iteration_index)?;
        if !matches!(
            loop_iteration,
            Instruction::IncrementLoopIteration
                | Instruction::PureReaderLoopIteration
                | Instruction::PureAffineLoopIteration
                | Instruction::PurePropertyWriteLoopIteration
                | Instruction::PureMethodLoopIteration
                | Instruction::PureGlobalAffineLoopIteration
                | Instruction::PureIndexedReaderLoopIteration
        ) || self
            .instructions
            .pc_to_index
            .get(loop_iteration_next_pc)
            .copied()
            != Some(index_increment_index)
            || address.as_u32() as usize != *loop_iteration_pc
        {
            return None;
        }
        let (_, increment_next_pc, Instruction::Inc { src, dst }) =
            self.instructions.instructions.get(index_increment_index)?
        else {
            return None;
        };
        if usize::from(*src) != index
            || usize::from(*dst) != index
            || self
                .instructions
                .pc_to_index
                .get(increment_next_pc)
                .copied()
                != Some(comparison_entry_index)
        {
            return None;
        }

        let (_, _, Instruction::JumpIfNotLessThan { address, .. }) = self
            .instructions
            .instructions
            .get(self.current_instruction)?
        else {
            return None;
        };
        if self
            .instructions
            .pc_to_index
            .get(&(address.as_u32() as usize))
            .copied()
            .is_none_or(|exit_index| exit_index <= back_edge_index)
        {
            return None;
        }
        for instruction_index in self.current_instruction..back_edge_index {
            let (_, next_pc, _) = self.instructions.instructions.get(instruction_index)?;
            if self.instructions.pc_to_index.get(next_pc).copied()? != instruction_index + 1 {
                return None;
            }
        }
        for instruction_index in body_start_index..=back_edge_index {
            let (pc, _, _) = self.instructions.instructions.get(instruction_index)?;
            if self
                .instructions
                .instructions
                .iter()
                .any(|(_, _, instruction)| branch_target(instruction) == Some(*pc))
            {
                return None;
            }
        }

        let source_registers = [index, limit, accumulator];
        if source_registers[0] == source_registers[1]
            || source_registers[0] == source_registers[2]
            || source_registers[1] == source_registers[2]
        {
            return None;
        }
        let temporary_registers = [
            add_dst,
            add_zero,
            add_or,
            multiplier_register,
            multiply_dst,
            multiply_zero,
            multiply_or,
            offset_register,
            subtract_dst,
            subtract_zero,
            subtract_or,
        ];
        let reloads_limit = comparison_entry_index != self.current_instruction;
        if temporary_registers.iter().any(|temporary| {
            *temporary == index
                || *temporary == accumulator
                || (*temporary == limit && !reloads_limit)
        }) {
            return None;
        }

        let kind_before = |instruction_index: usize, register: usize| {
            self.analysis
                .before
                .get(instruction_index)
                .and_then(|kinds| kinds.get(register))
                .copied()
        };
        if source_registers.iter().any(|register| {
            kind_before(self.current_instruction, *register) != Some(RegisterKind::Numeric)
        }) {
            return None;
        }
        for instruction_index in body_start_index..back_edge_index {
            if let Some(target) = self
                .analysis
                .targets
                .get(instruction_index)
                .copied()
                .flatten()
                && self
                    .analysis
                    .after
                    .get(instruction_index)
                    .and_then(|kinds| kinds.get(target))
                    .copied()
                    != Some(RegisterKind::Numeric)
            {
                return None;
            }
        }

        let dead_after = |instruction_index: usize, registers: &[usize]| {
            self.analysis
                .live_after
                .get(instruction_index)
                .is_some_and(|live| !registers.iter().any(|register| live.contains(register)))
        };
        if !dead_after(add_or_index, &[add_dst, add_zero])
            || !dead_after(add_move_index, &[add_or])
            || !dead_after(multiply_index, &[multiplier_register])
            || !dead_after(multiply_or_index, &[multiply_dst, multiply_zero])
            || !dead_after(multiply_move_index, &[multiply_or])
            || !dead_after(subtract_index, &[offset_register])
            || !dead_after(subtract_or_index, &[subtract_dst, subtract_zero])
            || !dead_after(subtract_move_index, &[subtract_or])
        {
            return None;
        }

        Some(WrappingAffineLoopFusion {
            body_start_index,
            back_edge_index,
            index,
            limit,
            accumulator,
            multiplier,
            offset,
        })
    }

    /// Match the bytecompiler's canonical wrapping i32 indexed reduction:
    ///
    /// `bounds branch -> global object load -> key move -> indexed load ->
    /// add -> zero -> bit-or -> accumulator move -> loop back-edge`.
    ///
    /// The complete loop body and maintenance tail are proven pure before a
    /// helper may replace multiple iterations. Metered code remains on the
    /// ordinary lowering so every bytecode and loop iteration stays visible.
    fn indexed_wrapping_sum_fusion(
        &self,
        index: usize,
        limit: usize,
    ) -> Option<IndexedWrappingSumFusion> {
        if self.options.accounting.instruction_budget || self.options.accounting.loop_iterations {
            return None;
        }
        let object_load_index = self.current_instruction.checked_add(1)?;
        let key_move_index = self.current_instruction.checked_add(2)?;
        let property_index = self.current_instruction.checked_add(3)?;
        let add_index = self.current_instruction.checked_add(4)?;
        let zero_index = self.current_instruction.checked_add(5)?;
        let bit_or_index = self.current_instruction.checked_add(6)?;
        let result_move_index = self.current_instruction.checked_add(7)?;
        let back_edge_index = self.current_instruction.checked_add(8)?;

        let (object_pc, _, object_load) = self.instructions.instructions.get(object_load_index)?;
        let Instruction::GetName {
            dst: object_dst,
            binding_index: object_binding_index,
        } = object_load
        else {
            return None;
        };
        let object_dst = usize::from(*object_dst);
        let object_binding_index = u32::from(*object_binding_index);
        if self
            .code
            .bindings
            .get(object_binding_index as usize)
            .is_none_or(|binding| binding.scope() != BindingLocatorScope::GlobalDeclarative)
        {
            return None;
        }

        let (_, _, key_move) = self.instructions.instructions.get(key_move_index)?;
        let Instruction::Move {
            src: key_source,
            dst: key_dst,
        } = key_move
        else {
            return None;
        };
        let key_dst = usize::from(*key_dst);
        if usize::from(*key_source) != index {
            return None;
        }

        let (property_pc, _, property) = self.instructions.instructions.get(property_index)?;
        let Instruction::GetPropertyByValue {
            dst: property_dst,
            key,
            receiver,
            object,
            ic_index: property_ic_index,
        } = property
        else {
            return None;
        };
        let property_dst = usize::from(*property_dst);
        if usize::from(*key) != key_dst
            || usize::from(*receiver) != object_dst
            || usize::from(*object) != object_dst
        {
            return None;
        }

        let (_, _, add) = self.instructions.instructions.get(add_index)?;
        let Instruction::Add { dst, lhs, rhs } = add else {
            return None;
        };
        let add_dst = usize::from(*dst);
        let sum = usize::from(*lhs);
        if usize::from(*rhs) != property_dst {
            return None;
        }

        let (_, _, zero) = self.instructions.instructions.get(zero_index)?;
        let Instruction::StoreZero { dst: zero_dst } = zero else {
            return None;
        };
        let zero_dst = usize::from(*zero_dst);
        if zero_dst != property_dst {
            return None;
        }

        let (_, _, bit_or) = self.instructions.instructions.get(bit_or_index)?;
        let Instruction::BitOr { dst, lhs, rhs } = bit_or else {
            return None;
        };
        let bit_or_dst = usize::from(*dst);
        let bit_lhs = usize::from(*lhs);
        let bit_rhs = usize::from(*rhs);
        if !((bit_lhs == add_dst && bit_rhs == zero_dst)
            || (bit_rhs == add_dst && bit_lhs == zero_dst))
        {
            return None;
        }

        let (_, _, result_move) = self.instructions.instructions.get(result_move_index)?;
        let Instruction::Move {
            src: result_source,
            dst: result_dst,
        } = result_move
        else {
            return None;
        };
        if usize::from(*result_source) != bit_or_dst || usize::from(*result_dst) != sum {
            return None;
        }

        let (_, _, back_edge) = self.instructions.instructions.get(back_edge_index)?;
        let Instruction::Jump {
            address: back_edge_address,
        } = back_edge
        else {
            return None;
        };
        let loop_iteration_index = self.current_instruction.checked_sub(2)?;
        let index_increment_index = self.current_instruction.checked_sub(1)?;
        let (loop_iteration_pc, loop_iteration_next_pc, loop_iteration) =
            self.instructions.instructions.get(loop_iteration_index)?;
        if !matches!(
            loop_iteration,
            Instruction::IncrementLoopIteration
                | Instruction::PureReaderLoopIteration
                | Instruction::PureAffineLoopIteration
                | Instruction::PurePropertyWriteLoopIteration
                | Instruction::PureMethodLoopIteration
                | Instruction::PureGlobalAffineLoopIteration
                | Instruction::PureIndexedReaderLoopIteration
        ) || self
            .instructions
            .pc_to_index
            .get(loop_iteration_next_pc)
            .copied()
            != Some(index_increment_index)
            || back_edge_address.as_u32() as usize != *loop_iteration_pc
        {
            return None;
        }
        let (_, increment_next_pc, increment) =
            self.instructions.instructions.get(index_increment_index)?;
        let Instruction::Inc {
            src: increment_src,
            dst: increment_dst,
        } = increment
        else {
            return None;
        };
        if usize::from(*increment_src) != index
            || usize::from(*increment_dst) != index
            || self
                .instructions
                .pc_to_index
                .get(increment_next_pc)
                .copied()
                != Some(self.current_instruction)
        {
            return None;
        }
        for instruction_index in self.current_instruction..back_edge_index {
            let (_, next_pc, _) = self.instructions.instructions.get(instruction_index)?;
            if self.instructions.pc_to_index.get(next_pc).copied()? != instruction_index + 1 {
                return None;
            }
        }
        for instruction_index in object_load_index..=back_edge_index {
            let (pc, _, _) = self.instructions.instructions.get(instruction_index)?;
            if self
                .instructions
                .instructions
                .iter()
                .any(|(_, _, instruction)| branch_target(instruction) == Some(*pc))
            {
                return None;
            }
        }

        let source_registers = [index, limit, sum];
        let temporary_registers = [object_dst, key_dst, property_dst, add_dst, bit_or_dst];
        if source_registers
            .iter()
            .enumerate()
            .any(|(position, register)| source_registers[..position].contains(register))
            || temporary_registers
                .iter()
                .enumerate()
                .any(|(position, register)| temporary_registers[..position].contains(register))
            || source_registers.iter().any(|source| {
                temporary_registers
                    .iter()
                    .any(|temporary| source == temporary)
            })
        {
            return None;
        }
        let kind_before = |instruction_index: usize, register: usize| {
            self.analysis
                .before
                .get(instruction_index)
                .and_then(|kinds| kinds.get(register))
                .copied()
        };
        let kind_after = |instruction_index: usize, register: usize| {
            self.analysis
                .after
                .get(instruction_index)
                .and_then(|kinds| kinds.get(register))
                .copied()
        };
        if kind_before(self.current_instruction, index) != Some(RegisterKind::Numeric)
            || kind_before(self.current_instruction, limit) != Some(RegisterKind::Numeric)
            || kind_before(self.current_instruction, sum) != Some(RegisterKind::Numeric)
            || kind_after(object_load_index, object_dst) != Some(RegisterKind::Boxed)
            || kind_after(key_move_index, key_dst) != Some(RegisterKind::Numeric)
            || kind_after(property_index, property_dst) != Some(RegisterKind::Numeric)
            || kind_after(add_index, add_dst) != Some(RegisterKind::Numeric)
            || kind_after(zero_index, zero_dst) != Some(RegisterKind::Numeric)
            || kind_after(bit_or_index, bit_or_dst) != Some(RegisterKind::Numeric)
            || kind_after(result_move_index, sum) != Some(RegisterKind::Numeric)
        {
            return None;
        }

        if self
            .analysis
            .live_after
            .get(property_index)?
            .contains(&object_dst)
            || self
                .analysis
                .live_after
                .get(property_index)?
                .contains(&key_dst)
            || self
                .analysis
                .live_after
                .get(add_index)?
                .contains(&property_dst)
            || self
                .analysis
                .live_after
                .get(bit_or_index)?
                .contains(&add_dst)
            || self
                .analysis
                .live_after
                .get(bit_or_index)?
                .contains(&zero_dst)
            || self
                .analysis
                .live_after
                .get(result_move_index)?
                .contains(&bit_or_dst)
        {
            return None;
        }
        Some(IndexedWrappingSumFusion {
            object_load_index,
            object_pc: *object_pc,
            key_move_index,
            property_index,
            property_pc: *property_pc,
            add_index,
            zero_index,
            bit_or_index,
            result_move_index,
            object_binding_index,
            property_ic_index: u32::from(*property_ic_index),
            object_dst,
            key_dst,
            index,
            limit,
            sum,
        })
    }

    /// Match the bytecompiler's canonical indexed identity-scan step:
    ///
    /// `length load -> bounds branch -> object/key setup -> indexed load ->
    /// strict equality -> equality branch`.
    ///
    /// Every interior block must have exactly the linear predecessor, and all
    /// elided temporaries must die at their canonical consumer. Budgeted code
    /// keeps the ordinary bytecode lowering because it must be resumable
    /// between every instruction.
    fn indexed_scan_step_fusion(
        &self,
        length_dst: usize,
        object: usize,
    ) -> Option<IndexedScanStepFusion> {
        if self.options.accounting.instruction_budget
            || self.options.accounting.loop_iterations
            || self.mode != NativeMode::I32
        {
            return None;
        }

        let compare_index = self.current_instruction.checked_add(1)?;
        let object_move_index = self.current_instruction.checked_add(2)?;
        let key_move_index = self.current_instruction.checked_add(3)?;
        let property_index = self.current_instruction.checked_add(4)?;
        let strict_or_other_index = self.current_instruction.checked_add(5)?;

        let (_, _, compare) = self.instructions.instructions.get(compare_index)?;
        let Instruction::JumpIfNotLessThan {
            lhs: index,
            rhs: length,
            ..
        } = compare
        else {
            return None;
        };
        let index = usize::from(*index);
        if usize::from(*length) != length_dst {
            return None;
        }

        let (_, _, object_move) = self.instructions.instructions.get(object_move_index)?;
        let (property_object_dst, object_binding_index) = match object_move {
            Instruction::Move {
                src: object_source,
                dst,
            } if usize::from(*object_source) == object => (usize::from(*dst), None),
            Instruction::GetName { dst, binding_index } => {
                let binding_index = u32::from(*binding_index);
                let previous_index = self.current_instruction.checked_sub(1)?;
                let (_, _, previous) = self.instructions.instructions.get(previous_index)?;
                let Instruction::GetName {
                    dst: previous_dst,
                    binding_index: previous_binding,
                } = previous
                else {
                    return None;
                };
                if usize::from(*previous_dst) != object
                    || u32::from(*previous_binding) != binding_index
                    || self
                        .code
                        .bindings
                        .get(binding_index as usize)
                        .is_none_or(|binding| {
                            binding.scope() != BindingLocatorScope::GlobalDeclarative
                        })
                {
                    return None;
                }
                (usize::from(*dst), Some(binding_index))
            }
            _ => return None,
        };

        let (_, _, key_move) = self.instructions.instructions.get(key_move_index)?;
        let Instruction::Move {
            src: key_source,
            dst: property_key_dst,
        } = key_move
        else {
            return None;
        };
        let property_key_dst = usize::from(*property_key_dst);
        if usize::from(*key_source) != index {
            return None;
        }

        let (property_pc, _, property) = self.instructions.instructions.get(property_index)?;
        let Instruction::GetPropertyByValue {
            dst: property_dst,
            key,
            receiver,
            object: property_object,
            ic_index: property_ic_index,
        } = property
        else {
            return None;
        };
        let property_dst = usize::from(*property_dst);
        if usize::from(*key) != property_key_dst
            || usize::from(*receiver) != property_object_dst
            || usize::from(*property_object) != property_object_dst
        {
            return None;
        }

        let (_, _, strict_or_other) = self.instructions.instructions.get(strict_or_other_index)?;
        let (strict_eq_index, expected_other, other_binding_index, other_load_index) =
            match strict_or_other {
                Instruction::StrictEq { .. } if object_binding_index.is_none() => {
                    (strict_or_other_index, None, None, None)
                }
                Instruction::GetName { dst, binding_index }
                    if object_binding_index.is_some()
                        && self
                            .code
                            .bindings
                            .get(usize::from(*binding_index))
                            .is_some_and(|binding| {
                                binding.scope() == BindingLocatorScope::GlobalDeclarative
                            }) =>
                {
                    (
                        strict_or_other_index.checked_add(1)?,
                        Some(usize::from(*dst)),
                        Some(u32::from(*binding_index)),
                        Some(strict_or_other_index),
                    )
                }
                _ => return None,
            };
        let branch_index = strict_eq_index.checked_add(1)?;

        for instruction_index in self.current_instruction..branch_index {
            let (_, next_pc, _) = self.instructions.instructions.get(instruction_index)?;
            if self.instructions.pc_to_index.get(next_pc).copied()? != instruction_index + 1 {
                return None;
            }
        }
        for instruction_index in compare_index..=branch_index {
            let (pc, _, _) = self.instructions.instructions.get(instruction_index)?;
            if self
                .instructions
                .instructions
                .iter()
                .any(|(_, _, instruction)| branch_target(instruction) == Some(*pc))
            {
                return None;
            }
        }

        let (_, _, strict_eq) = self.instructions.instructions.get(strict_eq_index)?;
        let Instruction::StrictEq { dst, lhs, rhs } = strict_eq else {
            return None;
        };
        let strict_eq_dst = usize::from(*dst);
        let lhs = usize::from(*lhs);
        let rhs = usize::from(*rhs);
        let other = match (lhs == property_dst, rhs == property_dst) {
            (true, false) => rhs,
            (false, true) => lhs,
            _ => return None,
        };
        if expected_other.is_some_and(|expected| expected != other) {
            return None;
        }

        let (_, _, branch) = self.instructions.instructions.get(branch_index)?;
        let Instruction::JumpIfFalse {
            value,
            address: false_address,
        } = branch
        else {
            return None;
        };
        if usize::from(*value) != strict_eq_dst {
            return None;
        }

        // A bulk scan skips all preceding non-matching iterations. Prove that
        // their false path contains only the bytecompiler's loop-maintenance
        // tail; otherwise calls, assignments, or any other user-visible work
        // between the comparison and back-edge would be skipped as well.
        let loop_iteration_index = self
            .current_instruction
            .checked_sub(if object_binding_index.is_some() { 3 } else { 2 })?;
        let index_increment_index = loop_iteration_index.checked_add(1)?;
        let (loop_iteration_pc, loop_iteration_next_pc, loop_iteration) =
            self.instructions.instructions.get(loop_iteration_index)?;
        if !matches!(
            loop_iteration,
            Instruction::IncrementLoopIteration
                | Instruction::PureReaderLoopIteration
                | Instruction::PureAffineLoopIteration
                | Instruction::PurePropertyWriteLoopIteration
                | Instruction::PureMethodLoopIteration
                | Instruction::PureGlobalAffineLoopIteration
                | Instruction::PureIndexedReaderLoopIteration
        ) || self
            .instructions
            .pc_to_index
            .get(loop_iteration_next_pc)
            .copied()
            != Some(index_increment_index)
        {
            return None;
        }
        let (_, increment_next_pc, increment) =
            self.instructions.instructions.get(index_increment_index)?;
        let Instruction::Inc {
            src: increment_src,
            dst: increment_dst,
        } = increment
        else {
            return None;
        };
        let increment_successor = if object_binding_index.is_some() {
            self.current_instruction.checked_sub(1)?
        } else {
            self.current_instruction
        };
        if usize::from(*increment_src) != index
            || usize::from(*increment_dst) != index
            || self
                .instructions
                .pc_to_index
                .get(increment_next_pc)
                .copied()
                != Some(increment_successor)
        {
            return None;
        }
        let false_index = self
            .instructions
            .pc_to_index
            .get(&(false_address.as_u32() as usize))
            .copied()?;
        let (_, _, false_instruction) = self.instructions.instructions.get(false_index)?;
        let Instruction::Jump { address: back_edge } = false_instruction else {
            return None;
        };
        if back_edge.as_u32() as usize != *loop_iteration_pc {
            return None;
        }

        if object_binding_index.is_some() {
            let registers = [
                object,
                index,
                length_dst,
                property_object_dst,
                property_key_dst,
            ];
            if registers
                .iter()
                .enumerate()
                .any(|(position, register)| registers[..position].contains(register))
                || property_dst != object
                || other != property_object_dst
                || strict_eq_dst != length_dst
            {
                return None;
            }
        } else {
            let source_registers = [object, index, other];
            let temporary_registers = [
                length_dst,
                property_object_dst,
                property_key_dst,
                property_dst,
            ];
            if source_registers.iter().any(|source| {
                temporary_registers
                    .iter()
                    .any(|temporary| source == temporary)
            }) || temporary_registers
                .iter()
                .enumerate()
                .any(|(position, register)| temporary_registers[..position].contains(register))
                || (strict_eq_dst != length_dst
                    && (source_registers.contains(&strict_eq_dst)
                        || temporary_registers.contains(&strict_eq_dst)))
            {
                return None;
            }
        }

        let kind_before = |instruction_index: usize, register: usize| {
            self.analysis
                .before
                .get(instruction_index)
                .and_then(|kinds| kinds.get(register))
                .copied()
        };
        let kind_after = |instruction_index: usize, register: usize| {
            self.analysis
                .after
                .get(instruction_index)
                .and_then(|kinds| kinds.get(register))
                .copied()
        };
        let other_is_boxed = other_load_index.map_or_else(
            || kind_before(self.current_instruction, other) == Some(RegisterKind::Boxed),
            |instruction_index| kind_after(instruction_index, other) == Some(RegisterKind::Boxed),
        );
        if kind_before(self.current_instruction, object) != Some(RegisterKind::Boxed)
            || kind_before(self.current_instruction, index) != Some(RegisterKind::Numeric)
            || !other_is_boxed
            || kind_after(self.current_instruction, length_dst) != Some(RegisterKind::Numeric)
            || kind_after(object_move_index, property_object_dst) != Some(RegisterKind::Boxed)
            || kind_after(key_move_index, property_key_dst) != Some(RegisterKind::Numeric)
            || kind_after(property_index, property_dst) != Some(RegisterKind::Boxed)
            || kind_after(strict_eq_index, strict_eq_dst) != Some(RegisterKind::Boolean)
        {
            return None;
        }

        if self
            .analysis
            .live_after
            .get(compare_index)?
            .contains(&length_dst)
            || self
                .analysis
                .live_after
                .get(property_index)?
                .contains(&property_object_dst)
            || self
                .analysis
                .live_after
                .get(property_index)?
                .contains(&property_key_dst)
            || self
                .analysis
                .live_after
                .get(strict_eq_index)?
                .contains(&property_dst)
            || (other_load_index.is_some()
                && self
                    .analysis
                    .live_after
                    .get(strict_eq_index)?
                    .contains(&other))
            || self
                .analysis
                .live_after
                .get(branch_index)?
                .contains(&strict_eq_dst)
        {
            return None;
        }

        Some(IndexedScanStepFusion {
            compare_index,
            object_move_index,
            key_move_index,
            property_index,
            property_pc: *property_pc,
            property_ic_index: u32::from(*property_ic_index),
            property_object_dst,
            property_key_dst,
            strict_eq_index,
            branch_index,
            index,
            other,
            other_binding_index,
            other_load_index,
        })
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

    /// Finish a binding helper that copied the authoritative value into a VM
    /// register. Boxed values stay rooted there; numeric specializations load
    /// the already-checked value into SSA only after the pre-effect guard.
    fn emit_binding_read_result(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        pc: usize,
        dst: usize,
        guard: cranelift_codegen::ir::Value,
    ) -> bool {
        let deopt = bcx.create_block();
        let cont = bcx.create_block();
        bcx.ins().brif(guard, cont, &[], deopt, &[]);
        bcx.switch_to_block(deopt);
        if !self.emit_guard_deopt(bcx, ctx, helpers, pc, JitExitReason::BindingRead) {
            return false;
        }
        bcx.switch_to_block(cont);

        if self.defined_register_kind(dst) != RegisterKind::Boxed {
            let dst_value = bcx.ins().iconst(types::I32, dst as i64);
            let load_helper = if self.mode == NativeMode::F64 {
                helpers.get_register_f64
            } else {
                helpers.get_register_i32
            };
            let load_address = bcx.ins().iconst(helpers.ptr, load_helper.address as i64);
            let value =
                bcx.ins()
                    .call_indirect(load_helper.signature, load_address, &[ctx, dst_value]);
            let value = bcx.inst_results(value)[0];
            if !self.define_register(bcx, dst, value) {
                return false;
            }
        }
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
        self.emit_guard_deopt_preserving_vm_registers(bcx, ctx, helpers, pc, reason, &[])
    }

    /// Exit at a helper-reconstructed bytecode while retaining VM operands
    /// that helper already wrote. A stale native temporary for one of those
    /// registers must not overwrite the authoritative replay value.
    #[allow(clippy::too_many_arguments)]
    fn emit_guard_deopt_preserving_vm_registers(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        pc: usize,
        reason: JitExitReason,
        preserved_vm_registers: &[usize],
    ) -> bool {
        // Budgeted native entries have charged this bytecode already, but a
        // guard exit asks the interpreter to execute the same bytecode. Refund
        // that charge so the interpreter remains the single owner of it.
        if self.options.accounting.instruction_budget {
            let helper = bcx.ins().iconst(
                helpers.ptr,
                helpers.refund_instruction_budget.address as i64,
            );
            bcx.ins()
                .call_indirect(helpers.refund_instruction_budget.signature, helper, &[ctx]);
        }

        let materialized = if preserved_vm_registers.is_empty() {
            self.emit_materialize_dirty_registers(bcx, ctx, helpers)
        } else {
            let registers: Vec<usize> = self
                .dirty
                .iter()
                .copied()
                .filter(|register| !preserved_vm_registers.contains(register))
                .collect();
            self.emit_materialize_registers(bcx, ctx, helpers, &registers)
        };
        if !materialized {
            return false;
        }
        self.emit_set_pc(bcx, ctx, helpers, pc);
        let status = bcx.ins().iconst(
            types::I64,
            JitExit::encode_with_reason(JitExitKind::Deopt, reason, pc as u32) as i64,
        );
        bcx.ins().return_(&[status]);
        true
    }

    /// Materialize every primitive definition that may be live on the current
    /// path into its VM register.
    ///
    /// A dirty register is not necessarily defined on every path: a definition
    /// the path branched around still puts the register in `self.dirty`, and
    /// `try_use_var` silently materializes a declared-but-undefined variable as
    /// zero. The companion definedness flag follows the same control flow, so
    /// the store helper updates the VM only when the native value is real.
    fn emit_materialize_dirty_registers(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
    ) -> bool {
        let registers: Vec<usize> = self.dirty.iter().copied().collect();
        self.emit_materialize_registers(bcx, ctx, helpers, &registers)
    }

    /// Materialize only dirty primitives that remain live after the current
    /// instruction. This is the safepoint path for a successful call: dead
    /// temporaries and a destination overwritten by the following pop are not
    /// observable while the callee runs.
    fn emit_materialize_live_dirty_registers(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
    ) -> bool {
        self.emit_materialize_live_dirty_registers_with(bcx, ctx, helpers, &[])
    }

    /// Materialize live dirty primitives plus operands consumed by a helper.
    ///
    /// Helpers that implement the current bytecode read their operands from
    /// the VM register file even when those values die at the instruction.
    /// Re-entrant helpers additionally require all values live afterwards to
    /// be visible to nested execution.
    fn emit_materialize_live_dirty_registers_with(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        required: &[usize],
    ) -> bool {
        let Some(live_after) = self.analysis.live_after.get(self.current_instruction) else {
            return false;
        };
        let mut registers: Vec<usize> = self.dirty.intersection(live_after).copied().collect();
        for &register in required {
            if self.dirty.contains(&register) && !registers.contains(&register) {
                registers.push(register);
            }
        }
        self.emit_materialize_registers(bcx, ctx, helpers, &registers)
    }

    fn emit_materialize_registers(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: &Helpers,
        registers: &[usize],
    ) -> bool {
        for &register in registers {
            let Some(value) = self.use_register(bcx, register) else {
                return false;
            };
            let Some(defined) = self.use_defined_flag(bcx, register) else {
                return false;
            };
            let helper = match (self.register_kind(register), self.mode) {
                (RegisterKind::Numeric, NativeMode::I32) => helpers.store_i32_if_defined,
                (RegisterKind::Numeric, NativeMode::F64) => helpers.store_f64_if_defined,
                (RegisterKind::Boolean, NativeMode::I32) => helpers.store_bool_i32_if_defined,
                (RegisterKind::Boolean, NativeMode::F64) => helpers.store_bool_f64_if_defined,
                (RegisterKind::Boxed, _) => return false,
            };
            let register_value = bcx.ins().iconst(types::I32, register as i64);
            let helper_address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
            bcx.ins().call_indirect(
                helper.signature,
                helper_address,
                &[ctx, register_value, value, defined],
            );
        }
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
            Instruction::IncrementLoopIteration
            | Instruction::PureReaderLoopIteration
            | Instruction::PureAffineLoopIteration
            | Instruction::PurePropertyWriteLoopIteration
            | Instruction::PureMethodLoopIteration
            | Instruction::PureGlobalAffineLoopIteration
            | Instruction::PureIndexedReaderLoopIteration => {
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

extern "C" fn jit_guard(
    context: *mut Context,
    charge_instruction_budget: u32,
    charge_loop_iterations: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let budget_mode_matches =
        context.instruction_budget_remaining.is_some() == (charge_instruction_budget != 0);
    let loop_mode_matches = (context.vm.runtime_limits.loop_iteration_limit() != u64::MAX)
        == (charge_loop_iterations != 0);
    if context.vm.frame().construct() || !budget_mode_matches || !loop_mode_matches {
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

    if !value_matches_native_representation(&value, representation) {
        return 0;
    }

    context.vm.set_register(register as usize, value);
    1
}

/// Copy a warm global-object binding into a VM register without retaining any
/// realm, object, shape, slot, or value in generated code. Accessors and IC
/// misses replay the authoritative `GetNameGlobal` operation in the
/// interpreter.
extern "C" fn jit_copy_global_object_binding_register(
    context: *mut Context,
    binding_index: u32,
    ic_index: u32,
    register: u32,
    representation: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    if !context.binding_locator_stable() {
        return 0;
    }

    let binding_is_global = context
        .vm
        .frame()
        .code_block
        .bindings
        .get(binding_index as usize)
        .is_some_and(|binding| binding.scope() == BindingLocatorScope::GlobalObject);
    if !binding_is_global {
        return 0;
    }

    let object = context.global_object();
    let Some(value) = cached_named_property_value(context, &object, ic_index) else {
        return 0;
    };
    if !value_matches_native_representation(&value, representation) {
        return 0;
    }

    context.vm.set_register(register as usize, value);
    1
}

fn value_matches_native_representation(value: &JsValue, representation: u32) -> bool {
    match representation {
        0 => value.as_i32().is_some(),
        1 => value.as_number().is_some(),
        2 => true,
        _ => false,
    }
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

extern "C" fn jit_copy_this_register(context: *mut Context, register: u32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    match crate::vm::opcode::This::operation(register.into(), context) {
        Ok(()) => 0,
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

extern "C" fn jit_dense_array_i32_guarded(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some((kind, value)) = dense_array_value(context, register, index, ic_index) else {
        return JIT_GUARD_FAIL_BIT;
    };
    if kind != IndexedKind::DenseI32 {
        return JIT_GUARD_FAIL_BIT;
    }
    let Some(value) = value.as_i32() else {
        return JIT_GUARD_FAIL_BIT;
    };
    // Every successful payload occupies only the low 32 bits, so it cannot
    // overlap the bit-61 guard-failure tag even when the i32 is negative.
    u64::from(value as u32)
}

extern "C" fn jit_dense_array_f64_guarded(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
    output: *mut f64,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some((kind, value)) = dense_array_value_f64(context, register, index, ic_index) else {
        return 0;
    };
    if !matches!(kind, IndexedKind::DenseI32 | IndexedKind::DenseF64) {
        return 0;
    }
    let Some(value) = value.as_number() else {
        return 0;
    };
    // SAFETY: generated code passes an aligned eight-byte stack slot owned by
    // the active native frame and reads it only when this helper returns 1.
    unsafe { output.write(value) };
    1
}

extern "C" fn jit_dense_array_boxed_i32_guarded(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
    dst: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some((_kind, value)) = dense_array_value(context, register, index, ic_index) else {
        return 0;
    };
    context.vm.set_register(dst as usize, value);
    1
}

extern "C" fn jit_dense_array_boxed_f64_guarded(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
    dst: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some((_kind, value)) = dense_array_value_f64(context, register, index, ic_index) else {
        return 0;
    };
    context.vm.set_register(dst as usize, value);
    1
}

type IndexedScanResult = Result<(Option<u32>, u64), (u32, u64)>;

fn dense_array_index_of_at(
    context: &Context,
    register: u32,
    start: u32,
    end: u32,
    ic_index: u32,
    other: &JsValue,
) -> Option<IndexedScanResult> {
    let value = context.vm.get_register(register as usize);
    let object = value.as_object_borrowed()?;
    let object = object.borrow();
    let ic = context
        .vm
        .frame()
        .code_block()
        .element_ic
        .get(ic_index as usize)?;
    ic.matches(object.shape())?;
    Some(
        object
            .properties()
            .index_of_contiguous_own_data(start, end, other),
    )
}

fn encode_indexed_scan_result(index: i32, scanned: u64, matched: bool) -> u64 {
    u64::from(index as u32)
        | (scanned.min(JIT_SCAN_COUNT_MASK) << JIT_SCAN_COUNT_SHIFT)
        | if matched { JIT_SCAN_MATCH_BIT } else { 0 }
}

fn materialize_indexed_scan_property_operands(
    context: &mut Context,
    register: u32,
    index: i32,
    property_object_dst: u32,
    property_key_dst: u32,
) {
    let object = context.vm.get_register(register as usize).clone();
    context
        .vm
        .set_register(property_object_dst as usize, object);
    context
        .vm
        .set_register(property_key_dst as usize, JsValue::from(index));
}

fn global_declarative_binding_value(context: &Context, binding_index: u32) -> Option<JsValue> {
    if !context.binding_locator_stable() {
        return None;
    }
    let frame = context.vm.frame();
    let binding = frame.code_block.bindings.get(binding_index as usize)?;
    if binding.scope() != BindingLocatorScope::GlobalDeclarative {
        return None;
    }
    frame.realm.environment().get(binding.binding_index())
}

/// Resolve one compile-time global binding through the active caller frame,
/// accepting only ordinary data-property reads for global-object bindings.
/// The `u32::MAX` IC sentinel denotes a global-declarative locator.
fn global_binding_data_value(
    context: &Context,
    binding_index: u32,
    ic_index: u32,
) -> Option<JsValue> {
    if ic_index == JIT_GLOBAL_DECLARATIVE_IC {
        return global_declarative_binding_value(context, binding_index);
    }
    if !context.binding_locator_stable() {
        return None;
    }

    let code = context.vm.frame().code_block().clone();
    if code
        .bindings
        .get(binding_index as usize)
        .is_none_or(|binding| binding.scope() != BindingLocatorScope::GlobalObject)
    {
        return None;
    }
    let global = context.global_object();
    cached_named_data_property_value(&code, &global, ic_index)
}

fn encode_pure_reader_range_result(sum: i32, iterations: u64, applied: bool) -> u64 {
    u64::from(sum as u32)
        | (iterations.min(JIT_SCAN_COUNT_MASK) << JIT_SCAN_COUNT_SHIFT)
        | if applied { JIT_SUM_APPLIED_BIT } else { 0 }
}

extern "C" fn jit_pure_reader_range_i32_guarded(
    context: *mut Context,
    function_binding_index: u32,
    function_ic_index: u32,
    object_binding_index: u32,
    object_ic_index: u32,
    index: i32,
    limit: i32,
    sum: i32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    if index >= limit {
        return encode_pure_reader_range_result(sum, 0, false);
    }

    // Preserve the source lookup order: callable first, argument second.
    let Some(function) =
        global_binding_data_value(context, function_binding_index, function_ic_index)
    else {
        return JIT_GUARD_FAIL_BIT;
    };
    let Some(argument) = global_binding_data_value(context, object_binding_index, object_ic_index)
    else {
        return JIT_GUARD_FAIL_BIT;
    };
    let Some(function_object) = function.as_object() else {
        return JIT_GUARD_FAIL_BIT;
    };
    let Some(ordinary) = function_object.downcast_ref::<OrdinaryFunction>() else {
        return JIT_GUARD_FAIL_BIT;
    };
    if !ordinary.codeblock().is_ordinary() || ordinary.codeblock().is_class_constructor() {
        return JIT_GUARD_FAIL_BIT;
    }

    // `function_call` checks these limits after the caller has pushed `this`,
    // the function, and one argument. A failed check must be replayed by the
    // ordinary Call opcode so it creates the exact catchable RangeError.
    if context.check_runtime_limits().is_err()
        || context.runtime_limits().stack_size_limit() <= context.vm.stack.len().saturating_add(3)
    {
        return JIT_GUARD_FAIL_BIT;
    }

    let Some(object) = argument.as_object() else {
        return JIT_GUARD_FAIL_BIT;
    };
    let Some(reader_value) = ordinary.codeblock().pure_reader_i32(&object) else {
        return JIT_GUARD_FAIL_BIT;
    };

    let iterations = i64::from(limit) - i64::from(index);
    let reduced = i128::from(sum) + i128::from(reader_value) * i128::from(iterations);
    let Ok(reduced) = i32::try_from(reduced) else {
        // Repeated addition would leave the i32 specialization. Resume before
        // the first call and let the ordinary overflow deopt preserve Number
        // promotion and every later iteration.
        return JIT_GUARD_FAIL_BIT;
    };
    encode_pure_reader_range_result(reduced, iterations as u64, true)
}

extern "C" fn jit_diagnostic_pure_reader_range_i32_guarded(
    context: *mut Context,
    function_binding_index: u32,
    function_ic_index: u32,
    object_binding_index: u32,
    object_ic_index: u32,
    index: i32,
    limit: i32,
    sum: i32,
) -> u64 {
    let result = jit_pure_reader_range_i32_guarded(
        context,
        function_binding_index,
        function_ic_index,
        object_binding_index,
        object_ic_index,
        index,
        limit,
        sum,
    );
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrows ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result & JIT_GUARD_FAIL_BIT != 0 {
        counters.pure_reader_guard_misses = counters.pure_reader_guard_misses.saturating_add(1);
        return result;
    }
    if result & JIT_SUM_APPLIED_BIT == 0 {
        return result;
    }
    let calls = (result >> JIT_SCAN_COUNT_SHIFT) & JIT_SCAN_COUNT_MASK;
    counters.pure_reader_range_hits = counters.pure_reader_range_hits.saturating_add(1);
    counters.pure_reader_calls_elided = counters.pure_reader_calls_elided.saturating_add(calls);
    result
}

fn dense_array_wrapping_sum_i32(
    context: &Context,
    value: &JsValue,
    start: u32,
    end: u32,
    initial: i32,
    ic_index: u32,
) -> Option<i32> {
    let object = value.as_object_borrowed()?;
    let object = object.borrow();
    let ic = context
        .vm
        .frame()
        .code_block()
        .element_ic
        .get(ic_index as usize)?;
    if ic.matches(object.shape())? != IndexedKind::DenseI32 {
        return None;
    }
    object
        .properties()
        .wrapping_sum_contiguous_i32(start, end, initial)
}

fn materialize_global_indexed_property_operands(
    context: &mut Context,
    object: &JsValue,
    key: JsValue,
    object_dst: u32,
    key_dst: u32,
) {
    context.vm.set_register(object_dst as usize, object.clone());
    context.vm.set_register(key_dst as usize, key);
}

fn encode_indexed_sum_result(sum: i32, scanned: u64, applied: bool) -> u64 {
    u64::from(sum as u32)
        | (scanned.min(JIT_SCAN_COUNT_MASK) << JIT_SCAN_COUNT_SHIFT)
        | if applied { JIT_SUM_APPLIED_BIT } else { 0 }
}

extern "C" fn jit_indexed_wrapping_sum_i32_guarded(
    context: *mut Context,
    object_binding_index: u32,
    index: i32,
    limit: i32,
    sum: i32,
    property_ic_index: u32,
    object_dst: u32,
    key_dst: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    if index >= limit {
        return encode_indexed_sum_result(sum, 0, false);
    }
    let Some(object) = global_declarative_binding_value(context, object_binding_index) else {
        return JIT_GUARD_FAIL_BIT;
    };
    let (Ok(start), Ok(end)) = (u32::try_from(index), u32::try_from(limit)) else {
        materialize_global_indexed_property_operands(
            context,
            &object,
            JsValue::from(index),
            object_dst,
            key_dst,
        );
        return JIT_SCAN_DENSE_FAIL_BIT | u64::from(sum as u32);
    };
    let Some(reduced) =
        dense_array_wrapping_sum_i32(context, &object, start, end, sum, property_ic_index)
    else {
        materialize_global_indexed_property_operands(
            context,
            &object,
            JsValue::from(index),
            object_dst,
            key_dst,
        );
        return JIT_SCAN_DENSE_FAIL_BIT | u64::from(sum as u32);
    };
    encode_indexed_sum_result(reduced, u64::from(end - start), true)
}

extern "C" fn jit_indexed_wrapping_sum_f64_guarded(
    context: *mut Context,
    object_binding_index: u32,
    index: f64,
    limit: f64,
    sum: f64,
    property_ic_index: u32,
    object_dst: u32,
    key_dst: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    if index.partial_cmp(&limit) != Some(std::cmp::Ordering::Less) {
        return 0;
    }
    let Some(object) = global_declarative_binding_value(context, object_binding_index) else {
        return JIT_GUARD_FAIL_BIT;
    };
    let exact_u32 = |value: f64| {
        (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= f64::from(u32::MAX))
            .then_some(value as u32)
    };
    let initial = f64_to_int32(sum);
    let exact_initial = sum == 0.0 || f64::from(initial).to_bits() == sum.to_bits();
    let (Some(start), Some(end)) = (exact_u32(index), exact_u32(limit)) else {
        materialize_global_indexed_property_operands(
            context,
            &object,
            JsValue::from(index),
            object_dst,
            key_dst,
        );
        return JIT_SCAN_DENSE_FAIL_BIT;
    };
    if !exact_initial {
        materialize_global_indexed_property_operands(
            context,
            &object,
            JsValue::from(index),
            object_dst,
            key_dst,
        );
        return JIT_SCAN_DENSE_FAIL_BIT;
    }
    let Some(reduced) =
        dense_array_wrapping_sum_i32(context, &object, start, end, initial, property_ic_index)
    else {
        materialize_global_indexed_property_operands(
            context,
            &object,
            JsValue::from(index),
            object_dst,
            key_dst,
        );
        return JIT_SCAN_DENSE_FAIL_BIT;
    };
    encode_indexed_sum_result(reduced, u64::from(end - start), true)
}

type WrappingMatrix3 = [[u32; 3]; 3];

fn wrapping_matrix_multiply(left: WrappingMatrix3, right: WrappingMatrix3) -> WrappingMatrix3 {
    let cell = |row: usize, column: usize| {
        left[row][0]
            .wrapping_mul(right[0][column])
            .wrapping_add(left[row][1].wrapping_mul(right[1][column]))
            .wrapping_add(left[row][2].wrapping_mul(right[2][column]))
    };
    [
        [cell(0, 0), cell(0, 1), cell(0, 2)],
        [cell(1, 0), cell(1, 1), cell(1, 2)],
        [cell(2, 0), cell(2, 1), cell(2, 2)],
    ]
}

fn wrapping_matrix_vector(matrix: WrappingMatrix3, vector: [u32; 3]) -> [u32; 3] {
    let row = |index: usize| {
        matrix[index][0]
            .wrapping_mul(vector[0])
            .wrapping_add(matrix[index][1].wrapping_mul(vector[1]))
            .wrapping_add(matrix[index][2].wrapping_mul(vector[2]))
    };
    [row(0), row(1), row(2)]
}

pub(super) fn wrapping_affine_range_i32(
    start: i32,
    end: i32,
    initial: i32,
    multiplier: i32,
    offset: i32,
) -> i32 {
    let Ok(mut iterations) = u64::try_from(i64::from(end) - i64::from(start)) else {
        return initial;
    };
    if iterations == 0 {
        return initial;
    }

    // One source iteration transforms [accumulator, index, 1]. All entries
    // use wrapping u32 operations, exactly matching the three ECMAScript
    // `| 0` coercions over the two's-complement ring.
    let multiplier = multiplier as u32;
    let mut power = [
        [multiplier, multiplier, (offset as u32).wrapping_neg()],
        [0, 1, 1],
        [0, 0, 1],
    ];
    let mut transform = [[1, 0, 0], [0, 1, 0], [0, 0, 1]];
    while iterations != 0 {
        if iterations & 1 != 0 {
            transform = wrapping_matrix_multiply(power, transform);
        }
        power = wrapping_matrix_multiply(power, power);
        iterations >>= 1;
    }

    wrapping_matrix_vector(transform, [initial as u32, start as u32, 1])[0] as i32
}

extern "C" fn jit_wrapping_affine_range_i32(
    _context: *mut Context,
    start: i32,
    end: i32,
    initial: i32,
    multiplier: i32,
    offset: i32,
) -> i32 {
    wrapping_affine_range_i32(start, end, initial, multiplier, offset)
}

extern "C" fn jit_diagnostic_wrapping_affine_range_i32(
    context: *mut Context,
    start: i32,
    end: i32,
    initial: i32,
    multiplier: i32,
    offset: i32,
) -> i32 {
    let result = wrapping_affine_range_i32(start, end, initial, multiplier, offset);
    let iterations = u64::try_from(i64::from(end) - i64::from(start)).unwrap_or(0);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the pure range calculation retains no reference into it.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if iterations != 0 {
        counters.affine_range_hits = counters.affine_range_hits.saturating_add(1);
        counters.affine_iterations = counters.affine_iterations.saturating_add(iterations);
    }
    result
}

extern "C" fn jit_indexed_scan_step_i32_guarded(
    context: *mut Context,
    register: u32,
    index: i32,
    length_ic_index: u32,
    property_ic_index: u32,
    other: u32,
    other_binding_index: u32,
    property_object_dst: u32,
    property_key_dst: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let global_other = if other_binding_index == u32::MAX {
        None
    } else {
        let Some(value) = global_declarative_binding_value(context, other_binding_index) else {
            return JIT_GUARD_FAIL_BIT;
        };
        Some(value)
    };
    let other = global_other
        .as_ref()
        .unwrap_or_else(|| context.vm.get_register(other as usize));
    let Some(length) =
        named_property_value(context, register, length_ic_index).and_then(|value| value.as_i32())
    else {
        return JIT_GUARD_FAIL_BIT;
    };
    if index >= length {
        return encode_indexed_scan_result(index, 0, false);
    }

    let Ok(start) = u32::try_from(index) else {
        // The interpreter resumes at `GetPropertyByValue`, after the two Move
        // bytecodes elided by this helper. Publish those exact operands only
        // on the cold guard-failure path.
        materialize_indexed_scan_property_operands(
            context,
            register,
            index,
            property_object_dst,
            property_key_dst,
        );
        return JIT_SCAN_DENSE_FAIL_BIT | u64::from(index as u32);
    };
    let Some(result) = dense_array_index_of_at(
        context,
        register,
        start,
        length as u32,
        property_ic_index,
        other,
    ) else {
        materialize_indexed_scan_property_operands(
            context,
            register,
            index,
            property_object_dst,
            property_key_dst,
        );
        return JIT_SCAN_DENSE_FAIL_BIT | u64::from(index as u32);
    };

    match result {
        Ok((Some(found), scanned)) => encode_indexed_scan_result(found as i32, scanned, true),
        Ok((None, scanned)) => encode_indexed_scan_result(length, scanned, false),
        Err((failed, scanned)) => {
            let failed = failed as i32;
            materialize_indexed_scan_property_operands(
                context,
                register,
                failed,
                property_object_dst,
                property_key_dst,
            );
            JIT_SCAN_DENSE_FAIL_BIT | encode_indexed_scan_result(failed, scanned, false)
        }
    }
}

extern "C" fn jit_diagnostic_dense_array_i32_guarded(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
) -> u64 {
    let result = jit_dense_array_i32_guarded(context, register, index, ic_index);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result & JIT_GUARD_FAIL_BIT != 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
    } else {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
        counters.dense_loads = counters.dense_loads.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_dense_array_f64_guarded(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
    output: *mut f64,
) -> u64 {
    let result = jit_dense_array_f64_guarded(context, register, index, ic_index, output);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
    } else {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
        counters.dense_loads = counters.dense_loads.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_dense_array_boxed_i32_guarded(
    context: *mut Context,
    register: u32,
    index: i32,
    ic_index: u32,
    dst: u32,
) -> u64 {
    let result = jit_dense_array_boxed_i32_guarded(context, register, index, ic_index, dst);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
    } else {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
        counters.dense_loads = counters.dense_loads.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_dense_array_boxed_f64_guarded(
    context: *mut Context,
    register: u32,
    index: f64,
    ic_index: u32,
    dst: u32,
) -> u64 {
    let result = jit_dense_array_boxed_f64_guarded(context, register, index, ic_index, dst);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
    } else {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
        counters.dense_loads = counters.dense_loads.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_indexed_scan_step_i32_guarded(
    context: *mut Context,
    register: u32,
    index: i32,
    length_ic_index: u32,
    property_ic_index: u32,
    other: u32,
    other_binding_index: u32,
    property_object_dst: u32,
    property_key_dst: u32,
) -> u64 {
    let result = jit_indexed_scan_step_i32_guarded(
        context,
        register,
        index,
        length_ic_index,
        property_ic_index,
        other,
        other_binding_index,
        property_object_dst,
        property_key_dst,
    );
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrows ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result & JIT_GUARD_FAIL_BIT != 0 {
        counters.named_guard_misses = counters.named_guard_misses.saturating_add(1);
        return result;
    }
    counters.named_guard_hits = counters.named_guard_hits.saturating_add(1);
    counters.named_loads = counters.named_loads.saturating_add(1);
    let scanned = (result >> JIT_SCAN_COUNT_SHIFT) & JIT_SCAN_COUNT_MASK;
    counters.dense_loads = counters.dense_loads.saturating_add(scanned);
    if result & JIT_SCAN_DENSE_FAIL_BIT != 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
    } else if scanned != 0 {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_indexed_wrapping_sum_i32_guarded(
    context: *mut Context,
    object_binding_index: u32,
    index: i32,
    limit: i32,
    sum: i32,
    property_ic_index: u32,
    object_dst: u32,
    key_dst: u32,
) -> u64 {
    let result = jit_indexed_wrapping_sum_i32_guarded(
        context,
        object_binding_index,
        index,
        limit,
        sum,
        property_ic_index,
        object_dst,
        key_dst,
    );
    if result & JIT_GUARD_FAIL_BIT != 0 {
        return result;
    }
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrows ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result & JIT_SCAN_DENSE_FAIL_BIT != 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
        return result;
    }
    let scanned = (result >> JIT_SCAN_COUNT_SHIFT) & JIT_SCAN_COUNT_MASK;
    counters.dense_loads = counters.dense_loads.saturating_add(scanned);
    if scanned != 0 {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_indexed_wrapping_sum_f64_guarded(
    context: *mut Context,
    object_binding_index: u32,
    index: f64,
    limit: f64,
    sum: f64,
    property_ic_index: u32,
    object_dst: u32,
    key_dst: u32,
) -> u64 {
    let result = jit_indexed_wrapping_sum_f64_guarded(
        context,
        object_binding_index,
        index,
        limit,
        sum,
        property_ic_index,
        object_dst,
        key_dst,
    );
    if result & JIT_GUARD_FAIL_BIT != 0 {
        return result;
    }
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrows ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result & JIT_SCAN_DENSE_FAIL_BIT != 0 {
        counters.dense_guard_misses = counters.dense_guard_misses.saturating_add(1);
        return result;
    }
    let scanned = (result >> JIT_SCAN_COUNT_SHIFT) & JIT_SCAN_COUNT_MASK;
    counters.dense_loads = counters.dense_loads.saturating_add(scanned);
    if scanned != 0 {
        counters.dense_guard_hits = counters.dense_guard_hits.saturating_add(1);
    }
    result
}

fn named_property_value(context: &Context, register: u32, ic_index: u32) -> Option<JsValue> {
    let value = context.vm.get_register(register as usize);
    let object = value.as_object_borrowed()?;
    cached_named_property_value(context, &object, ic_index)
}

fn cached_named_property_value(
    context: &Context,
    object: &JsObject,
    ic_index: u32,
) -> Option<JsValue> {
    cached_named_property_value_from_code(context.vm.frame().code_block(), object, ic_index, false)
}

fn cached_named_data_property_value(
    code: &CodeBlock,
    object: &JsObject,
    ic_index: u32,
) -> Option<JsValue> {
    cached_named_property_value_from_code(code, object, ic_index, true)
}

fn cached_named_property_value_from_code(
    code: &CodeBlock,
    object: &JsObject,
    ic_index: u32,
    data_only: bool,
) -> Option<JsValue> {
    let object = object.borrow();
    let ic = code.ic.get(ic_index as usize)?;
    let slot = ic.get(object.shape())?;
    if if data_only {
        slot.attributes.is_accessor_descriptor()
    } else {
        slot.attributes.has_get()
    } {
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

extern "C" fn jit_named_property_boxed_guarded(
    context: *mut Context,
    register: u32,
    ic_index: u32,
    dst: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some(value) = named_property_value(context, register, ic_index) else {
        return 0;
    };
    context.vm.set_register(dst as usize, value);
    1
}

extern "C" fn jit_named_property_f64_guarded(
    context: *mut Context,
    register: u32,
    ic_index: u32,
    output: *mut f64,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some(value) =
        named_property_value(context, register, ic_index).and_then(|value| value.as_number())
    else {
        return 0;
    };
    // SAFETY: generated code passes an aligned eight-byte stack slot owned by
    // the active native frame and reads it only when this helper returns 1.
    unsafe { output.write(value) };
    1
}

extern "C" fn jit_named_property_i32_guarded(
    context: *mut Context,
    register: u32,
    ic_index: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    let Some(value) =
        named_property_value(context, register, ic_index).and_then(|value| value.as_i32())
    else {
        return JIT_GUARD_FAIL_BIT;
    };
    // Every successful payload occupies only the low 32 bits, so it cannot
    // overlap the bit-61 guard-failure tag even when the i32 is negative.
    u64::from(value as u32)
}

extern "C" fn jit_diagnostic_named_property_f64_guarded(
    context: *mut Context,
    register: u32,
    ic_index: u32,
    output: *mut f64,
) -> u64 {
    let result = jit_named_property_f64_guarded(context, register, ic_index, output);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.named_guard_misses = counters.named_guard_misses.saturating_add(1);
    } else {
        counters.named_guard_hits = counters.named_guard_hits.saturating_add(1);
        counters.named_loads = counters.named_loads.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_named_property_boxed_guarded(
    context: *mut Context,
    register: u32,
    ic_index: u32,
    dst: u32,
) -> u64 {
    let result = jit_named_property_boxed_guarded(context, register, ic_index, dst);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result == 0 {
        counters.named_guard_misses = counters.named_guard_misses.saturating_add(1);
    } else {
        counters.named_guard_hits = counters.named_guard_hits.saturating_add(1);
        counters.named_loads = counters.named_loads.saturating_add(1);
    }
    result
}

extern "C" fn jit_diagnostic_named_property_i32_guarded(
    context: *mut Context,
    register: u32,
    ic_index: u32,
) -> u64 {
    let result = jit_named_property_i32_guarded(context, register, ic_index);
    // SAFETY: generated code receives an exclusively borrowed live context,
    // and the delegated helper's borrow ended before this update.
    let counters = unsafe { &mut (*context).vm.jit_native_storage };
    if result & JIT_GUARD_FAIL_BIT != 0 {
        counters.named_guard_misses = counters.named_guard_misses.saturating_add(1);
    } else {
        counters.named_guard_hits = counters.named_guard_hits.saturating_add(1);
        counters.named_loads = counters.named_loads.saturating_add(1);
    }
    result
}

extern "C" fn jit_bit_or_f64(lhs: f64, rhs: f64) -> f64 {
    f64::from(f64_to_int32(lhs) | f64_to_int32(rhs))
}

extern "C" fn jit_bit_xor_f64(lhs: f64, rhs: f64) -> f64 {
    f64::from(f64_to_int32(lhs) ^ f64_to_int32(rhs))
}

extern "C" fn jit_strict_eq(context: *mut Context, lhs: u32, rhs: u32) -> i32 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    i32::from(
        context
            .vm
            .get_register(lhs as usize)
            .strict_equals(context.vm.get_register(rhs as usize)),
    )
}

extern "C" fn jit_set_property_by_name(
    context: *mut Context,
    value: u32,
    object: u32,
    ic_index: u32,
) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    match crate::vm::opcode::SetPropertyByName::operation(
        (value.into(), object.into(), ic_index.into()),
        context,
    ) {
        Ok(()) => 0,
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

/// Enter a prepared call-free leaf from an already-native caller.
///
/// `None` leaves the callee frame installed for interpreter completion after
/// an entry guard or arithmetic deopt. Every `Some` value is the final status
/// the ordinary-call helper must return to generated code.
pub(super) fn call_prepared_leaf(
    context: &mut Context,
    caller_depth: usize,
    caller_code_id: u64,
    continuation_pc: u32,
) -> Option<u64> {
    if context.instruction_budget_remaining().is_some()
        || context.runtime_limits().loop_iteration_limit() != u64::MAX
        || context.active_jit_observes_interpreted_sites
    {
        return None;
    }
    let entry = context
        .vm
        .frame()
        .code_block
        .jit_leaf_entry(context.active_jit_backend_id)?;
    let status = entry(std::ptr::from_mut(context));
    if status & JIT_BREAK_BIT != 0 {
        return Some(status);
    }

    match JitExit::decode(status) {
        Some(JitExit {
            kind: JitExitKind::Return,
            reason: JitExitReason::Return,
            ..
        }) if context.vm.frames.len() == caller_depth
            && context.vm.frame().code_block.debug_id == caller_code_id
            && context.vm.frame().pc == continuation_pc =>
        {
            context
                .vm
                .record_native_leaf_entry(context.active_jit_backend_id);
            Some(0)
        }
        Some(JitExit {
            kind: JitExitKind::Deopt | JitExitKind::EntryRejected,
            ..
        }) => None,
        _ => {
            let mut error =
                crate::error::PanicError::new("invalid prepared JIT leaf continuation metadata")
                    .into();
            context.capture_error_backtrace(&mut error);
            Some(jit_break(
                context,
                crate::vm::CompletionRecord::Throw(error),
                JitExitKind::Completion,
                JitExitReason::Unknown,
                continuation_pc,
            ))
        }
    }
}

extern "C" fn jit_call_ordinary(context: *mut Context, argument_count: u32) -> u64 {
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
    if !ordinary.codeblock().is_ordinary() || ordinary.codeblock().is_class_constructor() {
        return JIT_GUARD_FAIL_BIT;
    }

    let caller_depth = context.vm.frames.len();
    let caller_code_id = context.vm.frame().code_block.debug_id;
    let continuation_pc = context.vm.frame().pc;

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
        Ok(true) => 0,
        Ok(false) => {
            if let Some(status) =
                call_prepared_leaf(context, caller_depth, caller_code_id, continuation_pc)
            {
                return status;
            }

            // An unprepared leaf or a native entry guard/deopt completes in
            // the interpreter from the current callee frame.
            match context.run_interpreter_until_frame_depth(caller_depth) {
                std::ops::ControlFlow::Continue(())
                    if context.vm.frames.len() == caller_depth
                        && context.vm.frame().code_block.debug_id == caller_code_id
                        && context.vm.frame().pc == continuation_pc =>
                {
                    0
                }
                std::ops::ControlFlow::Continue(())
                    if context.vm.frames.len() < caller_depth && !context.vm.frames.is_empty() =>
                {
                    JitExit::encode_with_reason(
                        JitExitKind::Call,
                        JitExitReason::Scheduler,
                        context.vm.frame().pc,
                    )
                }
                std::ops::ControlFlow::Continue(()) => {
                    let mut error = crate::error::PanicError::new(
                        "invalid JIT ordinary-call continuation metadata",
                    )
                    .into();
                    context.capture_error_backtrace(&mut error);
                    jit_break(
                        context,
                        crate::vm::CompletionRecord::Throw(error),
                        JitExitKind::Completion,
                        JitExitReason::Unknown,
                        continuation_pc,
                    )
                }
                std::ops::ControlFlow::Break(record) => jit_break(
                    context,
                    record,
                    JitExitKind::Completion,
                    JitExitReason::Exception,
                    continuation_pc,
                ),
            }
        }
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

extern "C" fn jit_store_bool_i32_if_defined(
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
        .set_register(register as usize, JsValue::new(value != 0));
    0
}

extern "C" fn jit_store_bool_f64_if_defined(
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
        .set_register(register as usize, JsValue::new(value != 0.0));
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

extern "C" fn jit_push_bool_i32(context: *mut Context, value: i32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.stack.push(JsValue::new(value != 0));
    0
}

extern "C" fn jit_push_bool_f64(context: *mut Context, value: f64) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.stack.push(JsValue::new(value != 0.0));
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

extern "C" fn jit_set_return_bool_i32(context: *mut Context, value: i32) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.set_return_value(JsValue::new(value != 0));
    0
}

extern "C" fn jit_set_return_bool_f64(context: *mut Context, value: f64) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.vm.set_return_value(JsValue::new(value != 0.0));
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
