//! Cached proof and evaluation for small pure numeric property readers.
//!
//! The accepted bytecode subset is deliberately narrow: a linear ordinary
//! function may read cached data properties from argument zero, combine i32
//! values with checked addition/subtraction, and return the result.  The plan
//! contains no source text or GC-managed pointers, so it is safe to retain on
//! the immutable [`CodeBlock`] and reuse from both interpreter and JIT paths.

use crate::{
    Context, JsObject, JsValue,
    object::shape::slot::SlotAttributes,
    vm::{CodeBlock, Instruction, InstructionIterator, Opcode},
};
use boa_ast::scope::BindingLocatorScope;

pub(super) const MAX_PURE_READER_INSTRUCTIONS: usize = 64;
const MAX_PURE_READER_PROPERTIES: usize = 8;
const MAX_PURE_READER_REGISTERS: u32 = 128;
const MAX_PURE_READER_LOOP_CODE: usize = 512;
const MAX_PURE_READER_LOOPS: usize = 8;
const MAX_PURE_READER_CONTINUATION: usize = 16;

#[derive(Clone, Copy, Debug)]
enum PureReaderNode {
    Constant(i32),
    Property(u32),
    Add { lhs: u8, rhs: u8 },
    Sub { lhs: u8, rhs: u8 },
}

#[derive(Clone, Copy, Debug)]
enum SymbolicValue {
    Unset,
    Argument,
    Node(u8),
}

/// A source-free proof that a code block is a small pure i32 property reader.
#[derive(Clone, Debug)]
pub(crate) struct PureReaderPlan {
    nodes: Box<[PureReaderNode]>,
    root: u8,
}

#[derive(Clone)]
struct DecodedInstruction {
    pc: usize,
    next_pc: usize,
    instruction: Instruction,
}

/// A canonical `sum += reader(object)` loop whose first completed iteration
/// proves all runtime feedback needed to reduce the rest of its i32 range.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PureReaderLoopPlan {
    loop_iteration_pc: u32,
    loop_iteration_next_pc: u32,
    exit_pc: u32,
    function_binding_index: u32,
    function_ic_index: Option<u32>,
    object_binding_index: u32,
    object_ic_index: Option<u32>,
    index: usize,
    limit: usize,
    sum: usize,
}

impl PureReaderPlan {
    pub(super) fn parse(code: &CodeBlock) -> Option<Self> {
        if !code.is_ordinary()
            || code.is_class_constructor()
            || !code.handlers.is_empty()
            || code.register_count > MAX_PURE_READER_REGISTERS
        {
            return None;
        }

        let mut registers = vec![SymbolicValue::Unset; code.register_count as usize];
        let mut stack = Vec::new();
        let mut nodes = Vec::new();
        let mut accumulator = None;
        let mut property_reads = 0usize;
        let mut instruction_count = 0usize;

        let mut push_node = |node| {
            if nodes.len() >= MAX_PURE_READER_INSTRUCTIONS {
                return None;
            }
            let index = u8::try_from(nodes.len()).ok()?;
            nodes.push(node);
            Some(SymbolicValue::Node(index))
        };

        for (_, _, instruction) in InstructionIterator::new(&code.bytecode) {
            instruction_count = instruction_count.checked_add(1)?;
            if instruction_count > MAX_PURE_READER_INSTRUCTIONS {
                return None;
            }

            let read = |register: crate::vm::opcode::RegisterOperand| {
                registers.get(usize::from(register)).copied()
            };
            let destination = |register: crate::vm::opcode::RegisterOperand| {
                usize::from(register)
                    .lt(&registers.len())
                    .then_some(usize::from(register))
            };

            match instruction {
                Instruction::GetArgument { index, dst } if u32::from(index) == 0 => {
                    let dst = destination(dst)?;
                    registers[dst] = SymbolicValue::Argument;
                }
                Instruction::StoreZero { dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Constant(0))?;
                }
                Instruction::StoreOne { dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Constant(1))?;
                }
                Instruction::StoreInt8 { value, dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Constant(i32::from(value)))?;
                }
                Instruction::StoreInt16 { value, dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Constant(i32::from(value)))?;
                }
                Instruction::StoreInt32 { value, dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Constant(value))?;
                }
                Instruction::Move { src, dst } => {
                    let value = read(src)?;
                    if matches!(value, SymbolicValue::Unset) {
                        return None;
                    }
                    let dst = destination(dst)?;
                    registers[dst] = value;
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
                } => {
                    if !matches!(read(value)?, SymbolicValue::Argument) {
                        return None;
                    }
                    property_reads = property_reads.checked_add(1)?;
                    if property_reads > MAX_PURE_READER_PROPERTIES {
                        return None;
                    }
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Property(u32::from(ic_index)))?;
                }
                Instruction::Add { dst, lhs, rhs } => {
                    let (SymbolicValue::Node(lhs), SymbolicValue::Node(rhs)) =
                        (read(lhs)?, read(rhs)?)
                    else {
                        return None;
                    };
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Add { lhs, rhs })?;
                }
                Instruction::Sub { dst, lhs, rhs } => {
                    let (SymbolicValue::Node(lhs), SymbolicValue::Node(rhs)) =
                        (read(lhs)?, read(rhs)?)
                    else {
                        return None;
                    };
                    let dst = destination(dst)?;
                    registers[dst] = push_node(PureReaderNode::Sub { lhs, rhs })?;
                }
                Instruction::PushFromRegister { src } => {
                    let value = read(src)?;
                    if matches!(value, SymbolicValue::Unset) {
                        return None;
                    }
                    stack.push(value);
                }
                Instruction::PopIntoRegister { dst } => {
                    let value = stack.pop()?;
                    let dst = destination(dst)?;
                    registers[dst] = value;
                }
                Instruction::SetAccumulator { src } => {
                    let SymbolicValue::Node(value) = read(src)? else {
                        return None;
                    };
                    accumulator = Some(value);
                }
                Instruction::CheckReturn => {}
                Instruction::Return => {
                    if property_reads == 0 || !stack.is_empty() {
                        return None;
                    }
                    return Some(Self {
                        nodes: nodes.into_boxed_slice(),
                        root: accumulator?,
                    });
                }
                _ => return None,
            }
        }

        None
    }

    /// Evaluate the proven plan without allocating or invoking JavaScript.
    /// A miss is pre-effect and sends the caller through ordinary bytecode.
    pub(super) fn evaluate(&self, code: &CodeBlock, object: &JsObject) -> Option<i32> {
        if !object.uses_ordinary_property_reads() {
            return None;
        }

        let object = object.borrow();
        let mut values = [0i32; MAX_PURE_READER_INSTRUCTIONS];

        for (index, node) in self.nodes.iter().copied().enumerate() {
            values[index] = match node {
                PureReaderNode::Constant(value) => value,
                PureReaderNode::Property(ic_index) => {
                    let ic = code.ic.get(ic_index as usize)?;
                    let slot = ic.get(object.shape())?;
                    if slot.attributes.is_accessor_descriptor() {
                        return None;
                    }
                    if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
                        let prototype = object.shape().prototype()?;
                        let prototype = prototype.borrow();
                        prototype
                            .properties()
                            .storage
                            .get(slot.index as usize)?
                            .as_i32()?
                    } else {
                        object
                            .properties()
                            .storage
                            .get(slot.index as usize)?
                            .as_i32()?
                    }
                }
                PureReaderNode::Add { lhs, rhs } => {
                    values[usize::from(lhs)].checked_add(values[usize::from(rhs)])?
                }
                PureReaderNode::Sub { lhs, rhs } => {
                    values[usize::from(lhs)].checked_sub(values[usize::from(rhs)])?
                }
            };
        }

        self.nodes.get(usize::from(self.root))?;
        Some(values[usize::from(self.root)])
    }
}

impl PureReaderLoopPlan {
    pub(super) fn parse_all(code: &CodeBlock) -> Box<[Self]> {
        // Most functions contain no loops. Avoid allocating and decoding an
        // instruction vector unless the byte stream could contain the loop
        // maintenance opcode. Operand bytes can cause a harmless false
        // positive, but a real opcode cannot cause a false negative.
        if !code.handlers.is_empty()
            || !code
                .bytecode
                .bytes
                .contains(&Opcode::IncrementLoopIteration.encode())
        {
            return Box::default();
        }

        let mut instructions = Vec::new();
        let mut iterator = InstructionIterator::new(&code.bytecode);
        while let Some((pc, _, instruction)) = iterator.next() {
            if instructions.len() == MAX_PURE_READER_LOOP_CODE {
                return Box::default();
            }
            instructions.push(DecodedInstruction {
                pc,
                next_pc: iterator.pc(),
                instruction,
            });
        }

        let mut plans = Vec::new();
        for loop_iteration_index in 0..instructions.len() {
            if plans.len() == MAX_PURE_READER_LOOPS {
                break;
            }
            if let Some(plan) = Self::parse_at(code, &instructions, loop_iteration_index) {
                plans.push(plan);
            }
        }
        plans.into_boxed_slice()
    }

    fn parse_at(
        code: &CodeBlock,
        instructions: &[DecodedInstruction],
        loop_iteration_index: usize,
    ) -> Option<Self> {
        let preheader_index = loop_iteration_index.checked_sub(1)?;
        let increment_index = loop_iteration_index.checked_add(1)?;
        let comparison_index = loop_iteration_index.checked_add(2)?;
        let body_start_index = loop_iteration_index.checked_add(3)?;
        let this_push_index = loop_iteration_index.checked_add(4)?;
        let function_load_index = loop_iteration_index.checked_add(5)?;
        let function_push_index = loop_iteration_index.checked_add(6)?;
        let object_load_index = loop_iteration_index.checked_add(7)?;
        let object_push_index = loop_iteration_index.checked_add(8)?;
        let call_index = loop_iteration_index.checked_add(9)?;
        let pop_index = loop_iteration_index.checked_add(10)?;
        let add_index = loop_iteration_index.checked_add(11)?;
        let result_move_index = loop_iteration_index.checked_add(12)?;
        let back_edge_index = loop_iteration_index.checked_add(13)?;

        let loop_iteration = instructions.get(loop_iteration_index)?;
        if !matches!(
            &loop_iteration.instruction,
            Instruction::IncrementLoopIteration
        ) {
            return None;
        }

        // This optimization runs from the maintenance opcode, so prove that
        // initial entry jumps over it and only the canonical latch can reach
        // it. Otherwise a warmed function entered at maintenance could be
        // mistaken for a just-completed first body iteration.
        let preheader = instructions.get(preheader_index)?;
        let Instruction::Jump {
            address: preheader_target,
        } = &preheader.instruction
        else {
            return None;
        };
        if preheader.next_pc != loop_iteration.pc {
            return None;
        }

        let increment = instructions.get(increment_index)?;
        let Instruction::Inc { src, dst } = &increment.instruction else {
            return None;
        };
        let index = usize::from(*src);
        if usize::from(*dst) != index {
            return None;
        }

        let comparison = instructions.get(comparison_index)?;
        let Instruction::JumpIfNotLessThan { address, lhs, rhs } = &comparison.instruction else {
            return None;
        };
        if usize::from(*lhs) != index {
            return None;
        }
        if preheader_target.as_u32() as usize != comparison.pc {
            return None;
        }
        let limit = usize::from(*rhs);
        let exit_pc = address.as_u32() as usize;
        let exit_index = instructions
            .iter()
            .position(|instruction| instruction.pc == exit_pc)?;
        if exit_index <= back_edge_index {
            return None;
        }

        let body_start = instructions.get(body_start_index)?;
        let Instruction::Move { src, dst } = &body_start.instruction else {
            return None;
        };
        let sum = usize::from(*src);
        let saved_sum = usize::from(*dst);

        let this_push = instructions.get(this_push_index)?;
        if !matches!(&this_push.instruction, Instruction::PushFromRegister { .. }) {
            return None;
        }

        let binding_read = |instruction_index: usize| {
            let instruction = &instructions.get(instruction_index)?.instruction;
            match instruction {
                Instruction::GetName { dst, binding_index } => {
                    let binding_index = u32::from(*binding_index);
                    code.bindings
                        .get(binding_index as usize)
                        .filter(|binding| {
                            binding.scope() == BindingLocatorScope::GlobalDeclarative
                        })?;
                    Some((usize::from(*dst), binding_index, None))
                }
                Instruction::GetNameGlobal {
                    dst,
                    binding_index,
                    ic_index,
                } => {
                    let binding_index = u32::from(*binding_index);
                    code.bindings
                        .get(binding_index as usize)
                        .filter(|binding| binding.scope() == BindingLocatorScope::GlobalObject)?;
                    Some((usize::from(*dst), binding_index, Some(u32::from(*ic_index))))
                }
                _ => None,
            }
        };
        let (function_register, function_binding_index, function_ic_index) =
            binding_read(function_load_index)?;
        let (object_register, object_binding_index, object_ic_index) =
            binding_read(object_load_index)?;

        let function_push = instructions.get(function_push_index)?;
        if !matches!(
            &function_push.instruction,
            Instruction::PushFromRegister { src } if usize::from(*src) == function_register
        ) {
            return None;
        }
        let object_push = instructions.get(object_push_index)?;
        if !matches!(
            &object_push.instruction,
            Instruction::PushFromRegister { src } if usize::from(*src) == object_register
        ) {
            return None;
        }
        if !matches!(
            &instructions.get(call_index)?.instruction,
            Instruction::Call { argument_count } if u32::from(*argument_count) == 1
        ) {
            return None;
        }

        let Instruction::PopIntoRegister { dst } = &instructions.get(pop_index)?.instruction else {
            return None;
        };
        let reader_result = usize::from(*dst);
        let Instruction::Add { dst, lhs, rhs } = &instructions.get(add_index)?.instruction else {
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
        let Instruction::Move { src, dst } = &instructions.get(result_move_index)?.instruction
        else {
            return None;
        };
        if usize::from(*src) != add_result || usize::from(*dst) != sum {
            return None;
        }

        let Instruction::Jump {
            address: back_edge_address,
        } = &instructions.get(back_edge_index)?.instruction
        else {
            return None;
        };
        if back_edge_address.as_u32() as usize != loop_iteration.pc {
            return None;
        }

        for instruction_index in loop_iteration_index..back_edge_index {
            if instructions.get(instruction_index)?.next_pc
                != instructions.get(instruction_index + 1)?.pc
            {
                return None;
            }
        }

        let body_start_pc = body_start.pc;
        let back_edge_pc = instructions.get(back_edge_index)?.pc;
        for (instruction_index, instruction) in instructions.iter().enumerate() {
            if instruction_targets_range(&instruction.instruction, body_start_pc, back_edge_pc) {
                return None;
            }
            if instruction_index != back_edge_index
                && instruction_targets_range(
                    &instruction.instruction,
                    loop_iteration.pc,
                    loop_iteration.pc,
                )
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

        let live_at_exit = continuation_live_registers(code, exit_pc)?;
        if temporary_registers
            .iter()
            .any(|temporary| live_at_exit.get(*temporary).copied().unwrap_or(true))
        {
            return None;
        }

        Some(Self {
            loop_iteration_pc: u32::try_from(loop_iteration.pc).ok()?,
            loop_iteration_next_pc: u32::try_from(loop_iteration.next_pc).ok()?,
            exit_pc: u32::try_from(exit_pc).ok()?,
            function_binding_index,
            function_ic_index,
            object_binding_index,
            object_ic_index,
            index,
            limit,
            sum,
        })
    }

    pub(crate) fn apply(self, caller_code: &CodeBlock, context: &mut Context) -> Option<()> {
        if context.instruction_budget_remaining().is_some()
            || context.runtime_limits().loop_iteration_limit() != u64::MAX
        {
            return None;
        }
        #[cfg(feature = "jit")]
        if context.active_jit_observes_interpreted_sites {
            return None;
        }
        #[cfg(feature = "trace")]
        if context.vm.trace || caller_code.traceable() {
            return None;
        }

        let index = context.vm.get_register(self.index).as_i32()?;
        let limit = context.vm.get_register(self.limit).as_i32()?;
        let sum = context.vm.get_register(self.sum).as_i32()?;
        let remaining = i64::from(limit)
            .checked_sub(i64::from(index))?
            .checked_sub(1)?;
        if remaining <= 0 {
            return None;
        }

        // Preserve the source lookup order: callable first, argument second.
        let function = binding_data_value(
            caller_code,
            self.function_binding_index,
            self.function_ic_index,
            context,
        )?;
        let argument = binding_data_value(
            caller_code,
            self.object_binding_index,
            self.object_ic_index,
            context,
        )?;
        let function = function.as_object()?;
        let ordinary = function.downcast_ref::<crate::builtins::function::OrdinaryFunction>()?;
        let reader_code = ordinary.codeblock();
        if !reader_code.is_ordinary()
            || reader_code.is_class_constructor()
            || reader_code.has_binding_identifier()
            || reader_code.has_function_scope()
        {
            return None;
        }
        #[cfg(feature = "trace")]
        if reader_code.traceable() {
            return None;
        }
        let argument = argument.as_object()?;
        let reader_value = reader_code.pure_reader_i32(&argument)?;

        // The first source iteration already passed the ordinary call-stack,
        // recursion, and runtime-limit checks. The proven reader cannot alter
        // any of those guards, so the remaining calls have the same outcome.
        let reduced = i128::from(sum) + i128::from(reader_value) * i128::from(remaining);
        let reduced = i32::try_from(reduced).ok()?;
        let total_iterations = u64::try_from(remaining).ok()?.checked_add(1)?;

        // Although the disabled loop limit cannot reject, retaining the
        // frame's wrapping count makes later diagnostics and limit checks
        // indistinguishable from executing every maintenance opcode.
        context.consume_loop_iterations(total_iterations).ok()?;
        context.vm.set_register(self.sum, reduced.into());
        context.vm.set_register(self.index, limit.into());
        context.vm.frame_mut().pc = self.exit_pc;
        #[cfg(test)]
        {
            context.vm.pure_reader_loop_reductions =
                context.vm.pure_reader_loop_reductions.saturating_add(1);
            context.vm.pure_reader_loop_calls_elided = context
                .vm
                .pure_reader_loop_calls_elided
                .saturating_add(u64::try_from(remaining).ok()?);
        }
        Some(())
    }

    pub(crate) const fn loop_iteration_next_pc(self) -> u32 {
        self.loop_iteration_next_pc
    }

    pub(crate) const fn loop_iteration_pc(self) -> u32 {
        self.loop_iteration_pc
    }
}

fn instruction_targets_range(instruction: &Instruction, start: usize, end: usize) -> bool {
    let in_range = |address: crate::vm::opcode::Address| {
        let address = address.as_u32() as usize;
        (start..=end).contains(&address)
    };
    match instruction {
        Instruction::LogicalAnd { address, .. }
        | Instruction::LogicalOr { address, .. }
        | Instruction::Coalesce { address, .. }
        | Instruction::Jump { address }
        | Instruction::JumpIfTrue { address, .. }
        | Instruction::JumpIfFalse { address, .. }
        | Instruction::JumpIfNotUndefined { address, .. }
        | Instruction::JumpIfNullOrUndefined { address, .. }
        | Instruction::JumpIfNotLessThan { address, .. }
        | Instruction::JumpIfNotLessThanOrEqual { address, .. }
        | Instruction::JumpIfNotGreaterThan { address, .. }
        | Instruction::JumpIfNotGreaterThanOrEqual { address, .. }
        | Instruction::JumpIfNotEqual { address, .. }
        | Instruction::Case { address, .. }
        | Instruction::TemplateLookup { address, .. } => in_range(*address),
        Instruction::JumpTable { addresses, .. } => addresses.iter().copied().any(in_range),
        _ => false,
    }
}

fn continuation_live_registers(code: &CodeBlock, resume_pc: usize) -> Option<Vec<bool>> {
    let mut epilogue = Vec::new();
    let mut found_resume = false;
    let mut found_return = false;
    for (pc, _, instruction) in InstructionIterator::new(&code.bytecode) {
        if !found_resume {
            if pc < resume_pc {
                continue;
            }
            if pc != resume_pc {
                return None;
            }
            found_resume = true;
        }
        if epilogue.len() == MAX_PURE_READER_CONTINUATION {
            return None;
        }
        let (uses, definition, returns) = match instruction {
            Instruction::PushFromRegister { src } | Instruction::SetAccumulator { src } => {
                (vec![usize::from(src)], None, false)
            }
            Instruction::PopIntoRegister { dst } => (Vec::new(), Some(usize::from(dst)), false),
            Instruction::CheckReturn => (Vec::new(), None, false),
            Instruction::Return => (Vec::new(), None, true),
            _ => return None,
        };
        if uses
            .iter()
            .any(|register| *register >= code.register_count as usize)
            || definition.is_some_and(|register| register >= code.register_count as usize)
        {
            return None;
        }
        epilogue.push((uses, definition));
        if returns {
            found_return = true;
            break;
        }
    }
    if !found_resume || !found_return {
        return None;
    }

    let mut live = vec![false; code.register_count as usize];
    for (uses, definition) in epilogue.into_iter().rev() {
        if let Some(definition) = definition {
            live[definition] = false;
        }
        for register in uses {
            live[register] = true;
        }
    }
    Some(live)
}

fn binding_data_value(
    code: &CodeBlock,
    binding_index: u32,
    ic_index: Option<u32>,
    context: &Context,
) -> Option<JsValue> {
    if !context.binding_locator_stable() {
        return None;
    }
    let binding = code.bindings.get(binding_index as usize)?;
    match (binding.scope(), ic_index) {
        (BindingLocatorScope::GlobalDeclarative, None) => context
            .vm
            .frame()
            .realm
            .environment()
            .get(binding.binding_index()),
        (BindingLocatorScope::GlobalObject, Some(ic_index)) => {
            let global = context.global_object();
            cached_named_data_property_value(code, &global, ic_index)
        }
        _ => None,
    }
}

fn cached_named_data_property_value(
    code: &CodeBlock,
    object: &JsObject,
    ic_index: u32,
) -> Option<JsValue> {
    let object = object.borrow();
    let ic = code.ic.get(ic_index as usize)?;
    let slot = ic.get(object.shape())?;
    if slot.attributes.is_accessor_descriptor() {
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

#[cfg(test)]
mod tests {
    use crate::{
        Context, JsValue, Source,
        error::{EngineError, RuntimeLimitError},
    };

    #[test]
    fn canonical_reader_loop_reduces_after_one_feedback_iteration() {
        let mut context = Context::default();
        let result = context
            .eval(Source::from_bytes(
                "const N = 1000;\n\
                 const object = { x: 1, y: 2, z: 3 };\n\
                 function read(o) { return o.x + o.y + o.z; }\n\
                 function main() {\n\
                     let sum = 0;\n\
                     for (let i = 0; i < N; i++) sum += read(object);\n\
                     return sum;\n\
                 }\n\
                 main();",
            ))
            .expect("canonical pure reader loop must succeed");

        assert_eq!(result, JsValue::new(6000));
        assert_eq!(context.vm.pure_reader_loop_reductions, 1);
        assert_eq!(context.vm.pure_reader_loop_calls_elided, 999);
    }

    #[test]
    fn reader_loop_revalidates_bindings_shapes_and_accessors() {
        let mut context = Context::default();
        context
            .eval(Source::from_bytes(
                "const N = 8;\n\
                 function pureRead(o) { return o.x + o.y + o.z; }\n\
                 let reader = pureRead;\n\
                 let object = { x: 1, y: 2, z: 3 };\n\
                 function main() {\n\
                     let sum = 0;\n\
                     for (let i = 0; i < N; i++) sum += reader(object);\n\
                     return sum;\n\
                 }",
            ))
            .expect("definitions must succeed");

        assert_eq!(
            context.eval(Source::from_bytes("main()")),
            Ok(JsValue::new(48))
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 1);

        assert_eq!(
            context.eval(Source::from_bytes(
                "object = { x: 10, y: 20, z: 30 }; main()"
            )),
            Ok(JsValue::new(480))
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 2);

        assert_eq!(
            context
                .eval(Source::from_bytes(
                    "let getterEffects = 0;\n\
                     object = { get x() { getterEffects++; return 1; }, y: 2, z: 3 };\n\
                     main() === 48 && getterEffects === 8",
                ))
                .expect("accessor case must execute normally")
                .as_boolean(),
            Some(true)
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 2);

        assert_eq!(
            context
                .eval(Source::from_bytes(
                    "let callEffects = 0;\n\
                     object = { x: 1, y: 2, z: 3 };\n\
                     reader = function (o) { callEffects++; return o.x + o.y + o.z; };\n\
                     main() === 48 && callEffects === 8",
                ))
                .expect("effectful replacement must execute normally")
                .as_boolean(),
            Some(true)
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 2);
    }

    #[test]
    fn reader_loop_rejects_number_promotion_proxies_and_near_matches() {
        let mut context = Context::default();
        let result = context
            .eval(Source::from_bytes(
                "const N = 4;\n\
                 function read(o) { return o.x + o.y + o.z; }\n\
                 let object = { x: 2147483647, y: 0, z: 0 };\n\
                 function main() {\n\
                     let sum = 0;\n\
                     for (let i = 0; i < N; i++) sum += read(object);\n\
                     return sum;\n\
                 }\n\
                 main();",
            ))
            .expect("overflowing sum must retain Number promotion");
        assert_eq!(result.as_number(), Some(8_589_934_588.0));
        assert_eq!(context.vm.pure_reader_loop_reductions, 0);

        assert_eq!(
            context
                .eval(Source::from_bytes(
                    "let proxyEffects = 0;\n\
                     object = new Proxy({ x: 1, y: 2, z: 3 }, {\n\
                         get(target, key, receiver) {\n\
                             proxyEffects++;\n\
                             return Reflect.get(target, key, receiver);\n\
                         }\n\
                     });\n\
                     main() === 24 && proxyEffects === 12",
                ))
                .expect("proxy reads must execute normally")
                .as_boolean(),
            Some(true)
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 0);

        assert_eq!(
            context
                .eval(Source::from_bytes(
                    "object = { x: 1, y: 2, z: 3 };\n\
                     function nearMatch() {\n\
                         let sum = 0;\n\
                         for (let i = 0; i < N; i++) {\n\
                             if (i === 2) continue;\n\
                             sum += read(object);\n\
                         }\n\
                         return sum;\n\
                     }\n\
                     nearMatch();",
                ))
                .expect("noncanonical control flow must execute normally"),
            JsValue::new(18)
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 0);
    }

    #[test]
    fn reader_shortcuts_preserve_finite_accounting_modes() {
        let definition = "const N = 10;\n\
            const object = { x: 1, y: 2, z: 3 };\n\
            function read(o) { return o.x + o.y + o.z; }\n\
            function main() {\n\
                let sum = 0;\n\
                for (let i = 0; i < N; i++) sum += read(object);\n\
                return sum;\n\
            }";

        let mut budgeted = Context::default();
        budgeted
            .eval(Source::from_bytes(definition))
            .expect("budget case definitions must succeed");
        budgeted.set_instruction_budget(1_000_000);
        assert_eq!(
            budgeted.eval(Source::from_bytes("main()")),
            Ok(JsValue::new(60))
        );
        assert_eq!(budgeted.vm.pure_reader_loop_reductions, 0);

        let mut loop_limited = Context::default();
        loop_limited
            .eval(Source::from_bytes(definition))
            .expect("loop-limit case definitions must succeed");
        loop_limited
            .runtime_limits_mut()
            .set_loop_iteration_limit(3);
        let error = loop_limited
            .eval(Source::from_bytes("main()"))
            .expect_err("finite loop limits must execute every source charge");
        assert_eq!(
            error.as_engine(),
            Some(&EngineError::RuntimeLimit(RuntimeLimitError::LoopIteration))
        );
        assert_eq!(loop_limited.vm.pure_reader_loop_reductions, 0);
    }

    #[test]
    fn cached_reader_proof_survives_gc_without_retaining_shapes() {
        let mut context = Context::default();
        assert_eq!(
            context.eval(Source::from_bytes(
                "const N = 8;\n\
                 function read(o) { return o.x + o.y + o.z; }\n\
                 var object = { x: 1, y: 2, z: 3 };\n\
                 function main() {\n\
                     let sum = 0;\n\
                     for (let i = 0; i < N; i++) sum += read(object);\n\
                     return sum;\n\
                 }\n\
                 main();"
            )),
            Ok(JsValue::new(48))
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 1);

        context
            .eval(Source::from_bytes("object = null"))
            .expect("old receiver must be releasable");
        boa_gc::force_collect();

        assert_eq!(
            context.eval(Source::from_bytes(
                "object = { x: 10, y: 20, z: 30, fresh: true }; main();"
            )),
            Ok(JsValue::new(480))
        );
        assert_eq!(context.vm.pure_reader_loop_reductions, 2);
    }
}
