//! Native lowering for the first narrow baseline tier.
//!
//! This module intentionally has a small allowlist. The legacy shim compiler
//! remains the fallback for every code block that cannot be represented by the
//! native value model below.

use std::collections::{BTreeSet, HashMap};

use crate::builtins::function::OrdinaryFunction;
use crate::object::internal_methods::InternalMethodCallContext;
use crate::object::shape::slot::SlotAttributes;
use crate::vm::{CodeBlock, IndexedKind, Instruction, InstructionIterator};
use crate::{Context, JsValue};

use super::{JIT_BREAK_BIT, JIT_GUARD_FAIL_BIT, JitBackend, JitExit, JitExitKind};

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
pub(super) fn compile(
    backend: &mut JitBackend,
    code: &CodeBlock,
    charge_instruction_budget: bool,
) -> Option<extern "C" fn(*mut Context) -> u64> {
    if !eligible(code) {
        return None;
    }

    let instructions = decode(code)?;
    let mode = select_mode(&instructions);
    let mut compiler =
        NativeCompiler::new(backend, code, instructions, mode, charge_instruction_budget)?;
    compiler.compile()
}

struct DecodedInstructions {
    instructions: Vec<(usize, usize, Instruction)>,
    pc_to_index: HashMap<usize, usize>,
}

fn decode(code: &CodeBlock) -> Option<DecodedInstructions> {
    let mut instructions = Vec::new();
    let mut pc_to_index = HashMap::new();
    let mut iterator = InstructionIterator::new(&code.bytecode);

    while let Some((pc, opcode, instruction)) = iterator.next() {
        if pc_to_index.insert(pc, instructions.len()).is_some() {
            return None;
        }
        instructions.push((pc, iterator.pc(), instruction));

        if !is_supported(opcode, &instructions.last().expect("just pushed").2) {
            return None;
        }
    }

    if instructions.is_empty() {
        return None;
    }

    // All branch targets must land on decoded instruction boundaries. This is
    // an allowlist check rather than an assumption about the current opcode
    // table; an omitted branch variant rejects native compilation safely.
    for (_, _, instruction) in &instructions {
        if let Some(target) = branch_target(instruction) {
            if !pc_to_index.contains_key(&target) {
                return None;
            }
        }
    }

    Some(DecodedInstructions {
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
        }

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

fn eligible(code: &CodeBlock) -> bool {
    code.is_ordinary() && code.handlers.is_empty() && code.register_count <= 128
}

fn is_supported(opcode: crate::vm::Opcode, instruction: &Instruction) -> bool {
    // Keep the opcode argument in the check so a future enum/decoder mismatch
    // cannot accidentally make an instruction look native by its fields alone.
    use crate::vm::Opcode;

    match (opcode, instruction) {
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

#[derive(Clone, Copy)]
struct Helpers {
    ptr: cranelift_codegen::ir::Type,
    guard: Helper,
    guard_argument_number: Helper,
    guard_stack_number: Helper,
    copy_argument_register: Helper,
    copy_register: Helper,
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
    store_i32: Helper,
    store_f64: Helper,
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
    helpers: Option<Helpers>,
    variables: Vec<Variable>,
    dirty: BTreeSet<usize>,
    charge_instruction_budget: bool,
}

impl<'a> NativeCompiler<'a> {
    fn new(
        backend: &'a mut JitBackend,
        code: &'a CodeBlock,
        instructions: DecodedInstructions,
        mode: NativeMode,
        charge_instruction_budget: bool,
    ) -> Option<Self> {
        let analysis = analyze_registers(&instructions, code.register_count as usize)?;
        Some(Self {
            backend,
            code,
            instructions,
            mode,
            analysis,
            current_instruction: 0,
            helpers: None,
            variables: Vec::new(),
            dirty: BTreeSet::new(),
            charge_instruction_budget,
        })
    }

    fn compile(&mut self) -> Option<extern "C" fn(*mut Context) -> u64> {
        let ptr = self.backend.module.target_config().pointer_type();
        let mut cctx = self.backend.module.make_context();
        let mut fctx = FunctionBuilderContext::new();

        cctx.func.signature.params.push(AbiParam::new(ptr));
        cctx.func.signature.returns.push(AbiParam::new(types::I64));

        let mut bcx = FunctionBuilder::new(&mut cctx.func, &mut fctx);
        self.variables = (0..self.code.register_count)
            .map(|_| bcx.declare_var(self.mode.value_type()))
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
        self.helpers = Some(helpers);

        let guard_ok = self.emit_entry_guard(&mut bcx, ctx_val, entry_deopt, helpers);
        if !guard_ok {
            return None;
        }

        bcx.ins().jump(code_blocks[0], &[]);

        bcx.switch_to_block(entry_deopt);
        self.emit_set_pc(&mut bcx, ctx_val, helpers, 0);
        let entry_deopt_status = bcx
            .ins()
            .iconst(types::I64, JitExit::encode(JitExitKind::Deopt, 0) as i64);
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
                self.emit_consume_instruction_budget(&mut bcx, ctx_val, helpers, pc, break_block);
            }

            if !self.emit_instruction(
                &mut bcx,
                ctx_val,
                helpers,
                pc,
                next_pc,
                &instruction,
                &code_blocks,
                break_block,
            ) {
                return None;
            }

            if fallthrough(&instruction) && !has_explicit_edges(&instruction) {
                let Some(next_index) = index
                    .checked_add(1)
                    .filter(|next| *next < code_blocks.len())
                else {
                    return None;
                };
                bcx.ins().jump(code_blocks[next_index], &[]);
            }
        }

        bcx.seal_all_blocks();
        bcx.finalize();

        let name = self.backend.next_fn_name("jit_native");
        let id = self
            .backend
            .module
            .declare_function(&name, Linkage::Export, &cctx.func.signature)
            .ok()?;
        self.backend.module.define_function(id, &mut cctx).ok()?;
        self.backend.module.clear_context(&mut cctx);
        self.backend.module.finalize_definitions().ok()?;

        let code_ptr = self.backend.module.get_finalized_function(id);
        // SAFETY: the signature is declared as `extern "C" fn(*mut Context) ->
        // u64`, and the backend owns the finalized code for the function's
        // lifetime.
        Some(unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*mut Context) -> u64>(code_ptr)
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
                jit_dense_array_guard as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32, types::I32],
                types::I64,
            ),
            dense_guard_f64: make(
                jit_dense_array_guard_f64 as *const () as usize,
                &[ptr, types::I32, types::F64, types::I32],
                types::I64,
            ),
            dense_i32: make(
                jit_dense_array_i32 as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32],
                types::I32,
            ),
            dense_f64: make(
                jit_dense_array_f64 as *const () as usize,
                &[ptr, types::I32, types::F64, types::I32],
                types::F64,
            ),
            named_guard: make(
                jit_named_property_guard as *const () as usize,
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            named_i32: make(
                jit_named_property_i32 as *const () as usize,
                &[ptr, types::I32, types::I32],
                types::I32,
            ),
            named_f64: make(
                jit_named_property_f64 as *const () as usize,
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
            increment_loop: make(jit_increment_loop as *const () as usize, &[ptr], types::I64),
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

    fn emit_consume_instruction_budget(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: Helpers,
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
        helpers: Helpers,
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

    fn emit_instruction(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: Helpers,
        pc: usize,
        next_pc: usize,
        instruction: &Instruction,
        blocks: &[Block],
        break_block: Block,
    ) -> bool {
        let register = |operand: crate::vm::opcode::RegisterOperand| usize::from(operand);

        match instruction {
            Instruction::GetArgument { index, dst } => {
                let index = bcx.ins().iconst(types::I32, u32::from(*index) as i64);
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
                            if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                            if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                let ic_index = bcx.ins().iconst(types::I32, u32::from(*ic_index) as i64);
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
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                let ic_index = bcx.ins().iconst(types::I32, u32::from(*ic_index) as i64);
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
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                    .iconst(types::I32, u32::from(*argument_count) as i64);
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
                if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
                    return false;
                }

                bcx.switch_to_block(called);
                let status = bcx.ins().iconst(
                    types::I64,
                    JitExit::encode(JitExitKind::Call, next_pc as u32) as i64,
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
                    let result = bcx.ins().iadd(lhs, rhs);
                    let lhs_sign = self.sign_bit(bcx, lhs);
                    let rhs_sign = self.sign_bit(bcx, rhs);
                    let result_sign = self.sign_bit(bcx, result);
                    let same_sign = bcx.ins().icmp(IntCC::Equal, lhs_sign, rhs_sign);
                    let changed_sign = bcx.ins().icmp(IntCC::NotEqual, result_sign, lhs_sign);
                    let overflow = bcx.ins().band(same_sign, changed_sign);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                    let result = bcx.ins().isub(lhs, rhs);
                    let lhs_sign = self.sign_bit(bcx, lhs);
                    let rhs_sign = self.sign_bit(bcx, rhs);
                    let result_sign = self.sign_bit(bcx, result);
                    let different_sign = bcx.ins().icmp(IntCC::NotEqual, lhs_sign, rhs_sign);
                    let changed_sign = bcx.ins().icmp(IntCC::NotEqual, result_sign, lhs_sign);
                    let overflow = bcx.ins().band(different_sign, changed_sign);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                    let lhs_wide = bcx.ins().sextend(types::I64, lhs);
                    let rhs_wide = bcx.ins().sextend(types::I64, rhs);
                    let wide_result = bcx.ins().imul(lhs_wide, rhs_wide);
                    let result = bcx.ins().ireduce(types::I32, wide_result);
                    let round_trip = bcx.ins().sextend(types::I64, result);
                    let overflow = bcx.ins().icmp(IntCC::NotEqual, wide_result, round_trip);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                    let new_value = bcx.ins().iadd(old_value, one);
                    let old_sign = self.sign_bit(bcx, old_value);
                    let new_sign = self.sign_bit(bcx, new_value);
                    let not_old_sign = bcx.ins().bnot(old_sign);
                    let max_overflow = bcx.ins().band(not_old_sign, new_sign);
                    let deopt = bcx.create_block();
                    let cont = bcx.create_block();
                    bcx.ins().brif(max_overflow, deopt, &[], cont, &[]);
                    bcx.switch_to_block(deopt);
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.increment_loop.address as i64);
                let status =
                    bcx.ins()
                        .call_indirect(helpers.increment_loop.signature, helper, &[ctx]);
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
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
                    if !self.emit_guard_deopt(bcx, ctx, helpers, pc) {
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
        self.dirty.insert(register);
        true
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
        helpers: Helpers,
        pc: usize,
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
        // `try_use_var` also validates that every value has a definition on
        // this path; an invalid map rejects native compilation.
        for register in &self.dirty {
            let Some(value) = self.use_register(bcx, *register) else {
                return false;
            };
            let helper = if self.mode == NativeMode::F64 {
                helpers.store_f64
            } else {
                helpers.store_i32
            };
            let register_value = bcx.ins().iconst(types::I32, *register as i64);
            let helper_address = bcx.ins().iconst(helpers.ptr, helper.address as i64);
            bcx.ins().call_indirect(
                helper.signature,
                helper_address,
                &[ctx, register_value, value],
            );
        }
        self.emit_set_pc(bcx, ctx, helpers, pc);
        let status = bcx.ins().iconst(
            types::I64,
            JitExit::encode(JitExitKind::Deopt, pc as u32) as i64,
        );
        bcx.ins().return_(&[status]);
        true
    }

    fn emit_set_pc(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: Helpers,
        pc: usize,
    ) {
        let helper = bcx.ins().iconst(helpers.ptr, helpers.set_pc.address as i64);
        let pc = bcx.ins().iconst(types::I32, pc as i64);
        bcx.ins()
            .call_indirect(helpers.set_pc.signature, helper, &[ctx, pc]);
    }
}

// The helper implementations are kept with the compiler so their ABI is
// reviewed together with the generated calls. Helpers return zero on success
// and a tagged/break status on failure.

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
            context.vm.jit_pending = Some(crate::vm::CompletionRecord::Throw(error));
            JIT_BREAK_BIT
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
    if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index > u32::MAX as f64 {
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
        Err(error) => {
            let mut error = crate::JsError::from(error);
            context.capture_error_backtrace(&mut error);
            context.vm.jit_pending = Some(crate::vm::CompletionRecord::Throw(error));
            return JIT_BREAK_BIT;
        }
    };

    match call.resolve(context) {
        Ok(_) => JitExit::encode(JitExitKind::Call, 0),
        Err(error) => {
            let mut error = crate::JsError::from(error);
            context.capture_error_backtrace(&mut error);
            context.vm.jit_pending = Some(crate::vm::CompletionRecord::Throw(error));
            JIT_BREAK_BIT
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

/// Charge one interpreter-visible loop iteration at a native backedge. This
/// is the initial native safepoint: the PC is written before this helper is
/// called, and failures are recorded in VM state rather than crossing the C
/// ABI as Rust values.
extern "C" fn jit_increment_loop(context: *mut Context) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    match context.consume_loop_iterations(1) {
        Ok(()) => 0,
        Err(error) => {
            let mut error = crate::JsError::from(error);
            context.capture_error_backtrace(&mut error);
            context.vm.jit_pending = Some(crate::vm::CompletionRecord::Throw(error));
            JIT_BREAK_BIT
        }
    }
}

extern "C" fn jit_handle_return(context: *mut Context) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    context.jit_handle_return()
}
