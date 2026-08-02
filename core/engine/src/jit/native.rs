//! Native lowering for the first narrow baseline tier.
//!
//! This module intentionally has a small allowlist. The legacy shim compiler
//! remains the fallback for every code block that cannot be represented by the
//! native value model below.

use std::collections::{BTreeSet, HashMap};

use crate::vm::{CodeBlock, Instruction, InstructionIterator};
use crate::{Context, JsValue};

use super::{JIT_BREAK_BIT, JIT_GUARD_FAIL_BIT, JitBackend, JitExit, JitExitKind};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, Block, InstBuilder, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{Linkage, Module};

/// Compile an ordinary integer code block to native code.
///
/// The current native subset is deliberately conservative: all register
/// values are `i32`, object/boxed operations are rejected, and the VM stack is
/// materialized only at helper/exit boundaries. Returning `None` is a normal
/// eligibility result; the caller uses the legacy shim compiler.
pub(super) fn compile(
    backend: &mut JitBackend,
    code: &CodeBlock,
) -> Option<extern "C" fn(*mut Context) -> u64> {
    if !eligible(code) {
        return None;
    }

    let instructions = decode(code)?;
    let mut compiler = NativeCompiler::new(backend, code, instructions)?;
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
        | (Opcode::Move, Instruction::Move { .. })
        | (Opcode::Add, Instruction::Add { .. })
        | (Opcode::Sub, Instruction::Sub { .. })
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
    !matches!(instruction, Instruction::Jump { .. } | Instruction::Return)
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
    get_argument_i32: Helper,
    set_pc: Helper,
    store_i32: Helper,
    push_i32: Helper,
    pop_i32: Helper,
    set_return_i32: Helper,
    increment_loop: Helper,
    handle_return: Helper,
}

struct NativeCompiler<'a> {
    backend: &'a mut JitBackend,
    code: &'a CodeBlock,
    instructions: DecodedInstructions,
    helpers: Option<Helpers>,
    variables: Vec<Variable>,
    dirty: BTreeSet<usize>,
}

impl<'a> NativeCompiler<'a> {
    fn new(
        backend: &'a mut JitBackend,
        code: &'a CodeBlock,
        instructions: DecodedInstructions,
    ) -> Option<Self> {
        Some(Self {
            backend,
            code,
            instructions,
            helpers: None,
            variables: Vec::new(),
            dirty: BTreeSet::new(),
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
        let mut make = |address: usize, params: &[cranelift_codegen::ir::Type]| {
            let mut signature = self.backend.module.make_signature();
            for param in params {
                signature.params.push(AbiParam::new(*param));
            }
            signature.returns.push(AbiParam::new(types::I64));
            Helper {
                address,
                signature: bcx.import_signature(signature),
            }
        };

        Helpers {
            ptr,
            guard: make(jit_guard as *const () as usize, &[ptr]),
            get_argument_i32: make(
                jit_get_argument_i32 as *const () as usize,
                &[ptr, types::I32],
            ),
            set_pc: make(jit_set_pc as *const () as usize, &[ptr, types::I32]),
            store_i32: make(
                jit_store_i32 as *const () as usize,
                &[ptr, types::I32, types::I32],
            ),
            push_i32: make(jit_push_i32 as *const () as usize, &[ptr, types::I32]),
            pop_i32: make(jit_pop_i32 as *const () as usize, &[ptr]),
            set_return_i32: make(jit_set_return_i32 as *const () as usize, &[ptr, types::I32]),
            increment_loop: make(jit_increment_loop as *const () as usize, &[ptr]),
            handle_return: make(jit_handle_return as *const () as usize, &[ptr]),
        }
    }

    fn emit_entry_guard(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        entry_deopt: Block,
        helpers: Helpers,
    ) -> bool {
        let guard = bcx.ins().iconst(helpers.ptr, helpers.guard.address as i64);
        let result = bcx
            .ins()
            .call_indirect(helpers.guard.signature, guard, &[ctx]);
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
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.get_argument_i32.address as i64);
                let index = bcx.ins().iconst(types::I32, u32::from(*index) as i64);
                let result = bcx.ins().call_indirect(
                    helpers.get_argument_i32.signature,
                    helper,
                    &[ctx, index],
                );
                let result = bcx.inst_results(result)[0];
                let fail_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                let failed = bcx.ins().band(result, fail_mask);
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                bcx.ins().brif(failed, deopt, &[], cont, &[]);

                bcx.switch_to_block(deopt);
                if !self.emit_deopt(bcx, ctx, helpers, pc) {
                    return false;
                }
                bcx.switch_to_block(cont);
                let value = bcx.ins().ireduce(types::I32, result);
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreZero { dst } => {
                let value = bcx.ins().iconst(types::I32, 0);
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreOne { dst } => {
                let value = bcx.ins().iconst(types::I32, 1);
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreInt8 { dst, value } => {
                let value = bcx.ins().iconst(types::I32, i64::from(*value));
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreInt16 { dst, value } => {
                let value = bcx.ins().iconst(types::I32, i64::from(*value));
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::StoreInt32 { dst, value } => {
                let value = bcx.ins().iconst(types::I32, i64::from(*value));
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::Move { dst, src } => {
                let Some(value) = self.use_register(bcx, register(*src)) else {
                    return false;
                };
                if !self.define_register(bcx, register(*dst), value) {
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
                if !self.emit_deopt(bcx, ctx, helpers, pc) {
                    return false;
                }
                bcx.switch_to_block(cont);
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
                if !self.emit_deopt(bcx, ctx, helpers, pc) {
                    return false;
                }
                bcx.switch_to_block(cont);
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
                if !self.emit_deopt(bcx, ctx, helpers, pc) {
                    return false;
                }
                bcx.switch_to_block(cont);
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
                let Some(value) = self.use_register(bcx, register(*src)) else {
                    return false;
                };
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.push_i32.address as i64);
                bcx.ins()
                    .call_indirect(helpers.push_i32.signature, helper, &[ctx, value]);
            }
            Instruction::PopIntoRegister { dst } => {
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.pop_i32.address as i64);
                let result = bcx
                    .ins()
                    .call_indirect(helpers.pop_i32.signature, helper, &[ctx]);
                let result = bcx.inst_results(result)[0];
                let guard_mask = bcx.ins().iconst(types::I64, JIT_GUARD_FAIL_BIT as i64);
                let failed = bcx.ins().band(result, guard_mask);
                let deopt = bcx.create_block();
                let cont = bcx.create_block();
                bcx.ins().brif(failed, deopt, &[], cont, &[]);
                bcx.switch_to_block(deopt);
                if !self.emit_deopt(bcx, ctx, helpers, pc) {
                    return false;
                }
                bcx.switch_to_block(cont);
                let value = bcx.ins().ireduce(types::I32, result);
                if !self.define_register(bcx, register(*dst), value) {
                    return false;
                }
            }
            Instruction::SetAccumulator { src } => {
                let Some(value) = self.use_register(bcx, register(*src)) else {
                    return false;
                };
                self.emit_set_pc(bcx, ctx, helpers, next_pc);
                let helper = bcx
                    .ins()
                    .iconst(helpers.ptr, helpers.set_return_i32.address as i64);
                bcx.ins()
                    .call_indirect(helpers.set_return_i32.signature, helper, &[ctx, value]);
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
        self.variables
            .get(register)
            .and_then(|variable| bcx.try_use_var(*variable).ok())
    }

    fn define_register(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        register: usize,
        value: cranelift_codegen::ir::Value,
    ) -> bool {
        let Some(variable) = self.variables.get(register) else {
            return false;
        };
        bcx.def_var(*variable, value);
        self.dirty.insert(register);
        true
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
        condition: IntCC,
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
        let condition = bcx.ins().icmp(condition, lhs, rhs);
        bcx.ins().brif(condition, target, &[], next, &[]);
        true
    }

    fn emit_deopt(
        &self,
        bcx: &mut FunctionBuilder<'_>,
        ctx: cranelift_codegen::ir::Value,
        helpers: Helpers,
        pc: usize,
    ) -> bool {
        // Dirty values are materialized before returning to the interpreter.
        // `try_use_var` also validates that every value has a definition on
        // this path; an invalid map rejects native compilation.
        for register in &self.dirty {
            let Some(value) = self.use_register(bcx, *register) else {
                return false;
            };
            let helper = bcx
                .ins()
                .iconst(helpers.ptr, helpers.store_i32.address as i64);
            let register_value = bcx.ins().iconst(types::I32, *register as i64);
            bcx.ins().call_indirect(
                helpers.store_i32.signature,
                helper,
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

extern "C" fn jit_guard(context: *mut Context) -> u64 {
    // SAFETY: generated code receives an exclusively borrowed live context.
    let context = unsafe { &mut *context };
    if context.vm.frame().construct() || context.instruction_budget_remaining.is_some() {
        return 0;
    }
    1
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

extern "C" fn jit_push_i32(context: *mut Context, value: i32) -> u64 {
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

extern "C" fn jit_set_return_i32(context: *mut Context, value: i32) -> u64 {
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
