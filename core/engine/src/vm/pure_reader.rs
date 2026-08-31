//! Cached proofs and range summaries for small pure numeric calls.
//!
//! The accepted bytecode subset is deliberately narrow: a linear ordinary
//! function may read cached data properties from argument zero, combine i32
//! values with checked addition/subtraction, or apply one constant i32 offset
//! to its argument. The plans contain no source text or GC-managed pointers,
//! so they are safe to retain on the immutable [`CodeBlock`] and reuse from
//! both interpreter and JIT paths.

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
const MAX_PURE_PROPERTY_WRITES: usize = 8;
pub(crate) const PURE_PROPERTY_WRITE_GUARD_MISS: u8 = 1 << 0;
pub(crate) const PURE_METHOD_GUARD_MISS: u8 = 1 << 1;

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

#[derive(Clone, Copy, Debug)]
enum AffineSymbolicValue {
    Unset,
    Argument,
    Constant(i32),
    Step(i32),
}

#[derive(Clone, Copy, Debug)]
enum ReceiverAffineSymbolicValue {
    Unset,
    Argument,
    Receiver,
    StateBefore,
    Step(i32),
    StateAfter,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PureReceiverAffineStepPlan {
    read_ic_index: u32,
    write_ic_index: u32,
    return_ic_index: u32,
    argument_scale: i32,
}

#[derive(Clone, Copy, Debug)]
enum PureFunctionKind {
    Reader,
    AffineStep,
    ReceiverAffineStep(PureReceiverAffineStepPlan),
}

/// One cached, source-free proof for the mutually exclusive pure-function
/// subsets consumed by structural loop summaries. Affine offsets reuse a
/// constant node so this remains the same compact `(box, root, tag)` layout as
/// the original property-reader proof.
#[derive(Clone, Debug)]
pub(crate) struct PureFunctionPlan {
    nodes: Box<[PureReaderNode]>,
    root: u8,
    kind: PureFunctionKind,
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

/// A canonical `accumulator = step(accumulator)` loop whose first completed
/// iteration proves the current function and i32 representation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PureAffineLoopPlan {
    loop_iteration_pc: u32,
    loop_iteration_next_pc: u32,
    exit_pc: u32,
    function_binding_index: u32,
    function_ic_index: Option<u32>,
    index: usize,
    limit: usize,
    accumulator: usize,
}

/// A canonical loop that writes affine index values to a fixed set of own
/// data properties. The final source iteration is retained, so the plan only
/// skips writes whose values are guaranteed to be overwritten before exit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PurePropertyWriteLoopPlan {
    loop_iteration_pc: u32,
    loop_iteration_next_pc: u32,
    body_start_pc: u32,
    object_binding_index: u32,
    object_ic_index: Option<u32>,
    property_ic_indices: [u32; MAX_PURE_PROPERTY_WRITES],
    property_count: u8,
    index: usize,
    limit: usize,
}

/// A canonical `last = receiver.method(constant)` loop whose method advances
/// one own i32 data slot and returns that same slot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PureMethodLoopPlan {
    loop_iteration_pc: u32,
    loop_iteration_next_pc: u32,
    exit_pc: u32,
    receiver_binding_index: u32,
    receiver_binding_ic_index: Option<u32>,
    method_ic_index: u32,
    argument: i32,
    index: usize,
    limit: usize,
    accumulator: usize,
}

/// A statically proven loop shape. Reader and affine candidates use distinct
/// specialized opcodes because their runtime guards and tiering policy differ.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PureLoopPlan {
    Reader(PureReaderLoopPlan),
    Affine(PureAffineLoopPlan),
    PropertyWrite(PurePropertyWriteLoopPlan),
    Method(PureMethodLoopPlan),
}

impl PureFunctionPlan {
    fn parse_reader(code: &CodeBlock) -> Option<Self> {
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
                        kind: PureFunctionKind::Reader,
                    });
                }
                _ => return None,
            }
        }

        None
    }

    /// Evaluate the proven plan without allocating or invoking JavaScript.
    /// A miss is pre-effect and sends the caller through ordinary bytecode.
    fn evaluate_reader(&self, code: &CodeBlock, object: &JsObject) -> Option<i32> {
        if !matches!(self.kind, PureFunctionKind::Reader) {
            return None;
        }
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

impl PureFunctionPlan {
    pub(super) fn parse(code: &CodeBlock) -> Option<Self> {
        Self::parse_reader(code)
            .or_else(|| Self::parse_affine(code))
            .or_else(|| Self::parse_receiver_affine(code))
    }

    pub(super) fn reader_i32(&self, code: &CodeBlock, object: &JsObject) -> Option<i32> {
        self.evaluate_reader(code, object)
    }

    pub(super) fn affine_delta(&self) -> Option<i32> {
        if !matches!(self.kind, PureFunctionKind::AffineStep) {
            return None;
        }
        let PureReaderNode::Constant(delta) = self.nodes.get(usize::from(self.root))? else {
            return None;
        };
        Some(*delta)
    }

    pub(super) fn receiver_affine_step(&self) -> Option<PureReceiverAffineStepPlan> {
        let PureFunctionKind::ReceiverAffineStep(plan) = self.kind else {
            return None;
        };
        Some(plan)
    }

    fn parse_affine(code: &CodeBlock) -> Option<Self> {
        if !code.is_ordinary()
            || code.is_class_constructor()
            || !code.handlers.is_empty()
            || code.register_count > MAX_PURE_READER_REGISTERS
        {
            return None;
        }

        let mut registers = vec![AffineSymbolicValue::Unset; code.register_count as usize];
        let mut stack = Vec::new();
        let mut accumulator = None;
        let mut instruction_count = 0usize;
        let mut arithmetic_seen = false;

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
                    registers[dst] = AffineSymbolicValue::Argument;
                }
                Instruction::StoreZero { dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = AffineSymbolicValue::Constant(0);
                }
                Instruction::StoreOne { dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = AffineSymbolicValue::Constant(1);
                }
                Instruction::StoreInt8 { value, dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = AffineSymbolicValue::Constant(i32::from(value));
                }
                Instruction::StoreInt16 { value, dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = AffineSymbolicValue::Constant(i32::from(value));
                }
                Instruction::StoreInt32 { value, dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = AffineSymbolicValue::Constant(value);
                }
                Instruction::Move { src, dst } => {
                    let value = read(src)?;
                    if matches!(value, AffineSymbolicValue::Unset) {
                        return None;
                    }
                    let dst = destination(dst)?;
                    registers[dst] = value;
                }
                Instruction::Add { dst, lhs, rhs } if !arithmetic_seen => {
                    let ((AffineSymbolicValue::Argument, AffineSymbolicValue::Constant(delta))
                    | (AffineSymbolicValue::Constant(delta), AffineSymbolicValue::Argument)) =
                        (read(lhs)?, read(rhs)?)
                    else {
                        return None;
                    };
                    let dst = destination(dst)?;
                    registers[dst] = AffineSymbolicValue::Step(delta);
                    arithmetic_seen = true;
                }
                Instruction::Sub { dst, lhs, rhs } if !arithmetic_seen => {
                    let (AffineSymbolicValue::Argument, AffineSymbolicValue::Constant(constant)) =
                        (read(lhs)?, read(rhs)?)
                    else {
                        return None;
                    };
                    let dst = destination(dst)?;
                    registers[dst] = AffineSymbolicValue::Step(constant.checked_neg()?);
                    arithmetic_seen = true;
                }
                Instruction::PushFromRegister { src } => {
                    let value = read(src)?;
                    if matches!(value, AffineSymbolicValue::Unset) {
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
                    let AffineSymbolicValue::Step(delta) = read(src)? else {
                        return None;
                    };
                    accumulator = Some(delta);
                }
                Instruction::CheckReturn => {}
                Instruction::Return => {
                    if !arithmetic_seen || !stack.is_empty() {
                        return None;
                    }
                    let delta = accumulator?;
                    return Some(Self {
                        nodes: vec![PureReaderNode::Constant(delta)].into_boxed_slice(),
                        root: 0,
                        kind: PureFunctionKind::AffineStep,
                    });
                }
                _ => return None,
            }
        }

        None
    }

    fn parse_receiver_affine(code: &CodeBlock) -> Option<Self> {
        if !code.is_ordinary()
            || code.is_class_constructor()
            || !code.handlers.is_empty()
            || code.register_count > MAX_PURE_READER_REGISTERS
        {
            return None;
        }

        let mut registers = vec![ReceiverAffineSymbolicValue::Unset; code.register_count as usize];
        let mut stack = Vec::new();
        let mut state_read_ic = None;
        let mut state_write_ic = None;
        let mut state_return_ic = None;
        let mut argument_scale = None;
        let mut accumulator_set = false;
        let mut instruction_count = 0usize;

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
                    registers[dst] = ReceiverAffineSymbolicValue::Argument;
                }
                Instruction::Move { src, dst } => {
                    let value = read(src)?;
                    if matches!(value, ReceiverAffineSymbolicValue::Unset) {
                        return None;
                    }
                    let dst = destination(dst)?;
                    registers[dst] = value;
                }
                Instruction::This { dst } => {
                    let dst = destination(dst)?;
                    registers[dst] = ReceiverAffineSymbolicValue::Receiver;
                }
                Instruction::GetPropertyByName {
                    dst,
                    value,
                    ic_index,
                } if matches!(read(value)?, ReceiverAffineSymbolicValue::Receiver) => {
                    let dst = destination(dst)?;
                    let ic_index = u32::from(ic_index);
                    if state_write_ic.is_none() {
                        if state_read_ic.replace(ic_index).is_some() {
                            return None;
                        }
                        registers[dst] = ReceiverAffineSymbolicValue::StateBefore;
                    } else {
                        if state_return_ic.replace(ic_index).is_some() {
                            return None;
                        }
                        registers[dst] = ReceiverAffineSymbolicValue::StateAfter;
                    }
                }
                Instruction::Add { dst, lhs, rhs } if argument_scale.is_none() => {
                    if !matches!(
                        (read(lhs)?, read(rhs)?),
                        (
                            ReceiverAffineSymbolicValue::StateBefore,
                            ReceiverAffineSymbolicValue::Argument
                        ) | (
                            ReceiverAffineSymbolicValue::Argument,
                            ReceiverAffineSymbolicValue::StateBefore
                        )
                    ) {
                        return None;
                    }
                    let dst = destination(dst)?;
                    registers[dst] = ReceiverAffineSymbolicValue::Step(1);
                    argument_scale = Some(1);
                }
                Instruction::Sub { dst, lhs, rhs } if argument_scale.is_none() => {
                    if !matches!(
                        (read(lhs)?, read(rhs)?),
                        (
                            ReceiverAffineSymbolicValue::StateBefore,
                            ReceiverAffineSymbolicValue::Argument
                        )
                    ) {
                        return None;
                    }
                    let dst = destination(dst)?;
                    registers[dst] = ReceiverAffineSymbolicValue::Step(-1);
                    argument_scale = Some(-1);
                }
                Instruction::SetPropertyByName {
                    value,
                    object,
                    ic_index,
                } if matches!(read(object)?, ReceiverAffineSymbolicValue::Receiver) => {
                    let ReceiverAffineSymbolicValue::Step(scale) = read(value)? else {
                        return None;
                    };
                    if argument_scale != Some(scale)
                        || state_write_ic.replace(u32::from(ic_index)).is_some()
                    {
                        return None;
                    }
                }
                Instruction::PushFromRegister { src } => {
                    let value = read(src)?;
                    if matches!(value, ReceiverAffineSymbolicValue::Unset) {
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
                    if !matches!(read(src)?, ReceiverAffineSymbolicValue::StateAfter)
                        || accumulator_set
                    {
                        return None;
                    }
                    accumulator_set = true;
                }
                Instruction::CheckReturn => {}
                Instruction::Return => {
                    if !stack.is_empty() || !accumulator_set {
                        return None;
                    }
                    let plan = PureReceiverAffineStepPlan {
                        read_ic_index: state_read_ic?,
                        write_ic_index: state_write_ic?,
                        return_ic_index: state_return_ic?,
                        argument_scale: argument_scale?,
                    };
                    let read_name = &code.ic.get(plan.read_ic_index as usize)?.name;
                    let write_name = &code.ic.get(plan.write_ic_index as usize)?.name;
                    let return_name = &code.ic.get(plan.return_ic_index as usize)?.name;
                    if write_name != read_name || return_name != read_name {
                        return None;
                    }
                    return Some(Self {
                        nodes: Box::default(),
                        root: 0,
                        kind: PureFunctionKind::ReceiverAffineStep(plan),
                    });
                }
                _ => return None,
            }
        }

        None
    }
}

impl PureLoopPlan {
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
            if let Some(plan) =
                PureReaderLoopPlan::parse_at(code, &instructions, loop_iteration_index)
            {
                plans.push(Self::Reader(plan));
            } else if let Some(plan) =
                PureAffineLoopPlan::parse_at(code, &instructions, loop_iteration_index)
            {
                plans.push(Self::Affine(plan));
            } else if let Some(plan) =
                PureMethodLoopPlan::parse_at(code, &instructions, loop_iteration_index)
            {
                plans.push(Self::Method(plan));
            } else if let Some(plan) =
                PurePropertyWriteLoopPlan::parse_at(code, &instructions, loop_iteration_index)
            {
                plans.push(Self::PropertyWrite(plan));
            }
        }
        plans.into_boxed_slice()
    }
}

impl PureReaderLoopPlan {
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
        if reader_result == saved_sum {
            return None;
        }
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

impl PureAffineLoopPlan {
    fn parse_at(
        code: &CodeBlock,
        instructions: &[DecodedInstruction],
        loop_iteration_index: usize,
    ) -> Option<Self> {
        let preheader_index = loop_iteration_index.checked_sub(1)?;
        let increment_index = loop_iteration_index.checked_add(1)?;
        let comparison_index = loop_iteration_index.checked_add(2)?;
        let body_start_index = loop_iteration_index.checked_add(3)?;
        let function_load_index = loop_iteration_index.checked_add(4)?;
        let function_push_index = loop_iteration_index.checked_add(5)?;
        let argument_push_index = loop_iteration_index.checked_add(6)?;
        let call_index = loop_iteration_index.checked_add(7)?;
        let pop_index = loop_iteration_index.checked_add(8)?;
        let after_pop_index = loop_iteration_index.checked_add(9)?;

        let loop_iteration = instructions.get(loop_iteration_index)?;
        if !matches!(
            &loop_iteration.instruction,
            Instruction::IncrementLoopIteration
        ) {
            return None;
        }

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
        if usize::from(*lhs) != index || preheader_target.as_u32() as usize != comparison.pc {
            return None;
        }
        let limit = usize::from(*rhs);
        let exit_pc = address.as_u32() as usize;

        let body_start = instructions.get(body_start_index)?;
        if !matches!(
            &body_start.instruction,
            Instruction::PushFromRegister { .. }
        ) {
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

        if !matches!(
            &instructions.get(function_push_index)?.instruction,
            Instruction::PushFromRegister { src } if usize::from(*src) == function_register
        ) {
            return None;
        }
        let Instruction::PushFromRegister { src } =
            &instructions.get(argument_push_index)?.instruction
        else {
            return None;
        };
        let accumulator = usize::from(*src);
        if !matches!(
            &instructions.get(call_index)?.instruction,
            Instruction::Call { argument_count } if u32::from(*argument_count) == 1
        ) {
            return None;
        }
        let Instruction::PopIntoRegister { dst } = &instructions.get(pop_index)?.instruction else {
            return None;
        };
        let result = usize::from(*dst);

        let (back_edge_index, result_temporary) = if let Instruction::Move { src, dst } =
            &instructions.get(after_pop_index)?.instruction
        {
            if usize::from(*src) != result || usize::from(*dst) != accumulator {
                return None;
            }
            (after_pop_index.checked_add(1)?, Some(result))
        } else {
            if result != accumulator {
                return None;
            }
            (after_pop_index, None)
        };

        let Instruction::Jump {
            address: back_edge_address,
        } = &instructions.get(back_edge_index)?.instruction
        else {
            return None;
        };
        if back_edge_address.as_u32() as usize != loop_iteration.pc {
            return None;
        }

        let exit_index = instructions
            .iter()
            .position(|instruction| instruction.pc == exit_pc)?;
        if exit_index <= back_edge_index {
            return None;
        }
        for instruction_index in loop_iteration_index..back_edge_index {
            if instructions.get(instruction_index)?.next_pc
                != instructions.get(instruction_index + 1)?.pc
            {
                return None;
            }
        }

        let back_edge_pc = instructions.get(back_edge_index)?.pc;
        for (instruction_index, instruction) in instructions.iter().enumerate() {
            if instruction_targets_range(&instruction.instruction, body_start.pc, back_edge_pc) {
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

        let source_registers = [index, limit, accumulator];
        if source_registers
            .iter()
            .enumerate()
            .any(|(position, register)| source_registers[..position].contains(register))
            || source_registers.contains(&function_register)
        {
            return None;
        }

        let live_at_exit = continuation_live_registers(code, exit_pc)?;
        if live_at_exit.get(function_register).copied().unwrap_or(true)
            || result_temporary
                .is_some_and(|temporary| live_at_exit.get(temporary).copied().unwrap_or(true))
        {
            return None;
        }

        Some(Self {
            loop_iteration_pc: u32::try_from(loop_iteration.pc).ok()?,
            loop_iteration_next_pc: u32::try_from(loop_iteration.next_pc).ok()?,
            exit_pc: u32::try_from(exit_pc).ok()?,
            function_binding_index,
            function_ic_index,
            index,
            limit,
            accumulator,
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
        let accumulator = context.vm.get_register(self.accumulator).as_i32()?;
        let remaining = i64::from(limit)
            .checked_sub(i64::from(index))?
            .checked_sub(1)?;
        if remaining <= 0 {
            return None;
        }

        let function = binding_data_value(
            caller_code,
            self.function_binding_index,
            self.function_ic_index,
            context,
        )?;
        let function = function.as_object()?;
        let ordinary = function.downcast_ref::<crate::builtins::function::OrdinaryFunction>()?;
        let step_code = ordinary.codeblock();
        if !step_code.is_ordinary()
            || step_code.is_class_constructor()
            || step_code.has_binding_identifier()
            || step_code.has_function_scope()
        {
            return None;
        }
        #[cfg(feature = "trace")]
        if step_code.traceable() {
            return None;
        }
        let delta = step_code.pure_affine_step_delta()?;

        // The first source iteration proved the call, representation, and
        // runtime checks. A constant offset is monotone, so i32 endpoints also
        // prove that every skipped intermediate call remains an i32 operation.
        let reduced = i128::from(accumulator) + i128::from(delta) * i128::from(remaining);
        let reduced = i32::try_from(reduced).ok()?;
        let total_iterations = u64::try_from(remaining).ok()?.checked_add(1)?;

        context.consume_loop_iterations(total_iterations).ok()?;
        context.vm.set_register(self.accumulator, reduced.into());
        context.vm.set_register(self.index, limit.into());
        context.vm.frame_mut().pc = self.exit_pc;
        caller_code.mark_pure_range_loop_observed();
        #[cfg(test)]
        {
            context.vm.pure_affine_loop_reductions =
                context.vm.pure_affine_loop_reductions.saturating_add(1);
            context.vm.pure_affine_loop_calls_elided = context
                .vm
                .pure_affine_loop_calls_elided
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

impl PureMethodLoopPlan {
    fn parse_at(
        code: &CodeBlock,
        instructions: &[DecodedInstruction],
        loop_iteration_index: usize,
    ) -> Option<Self> {
        let preheader_index = loop_iteration_index.checked_sub(1)?;
        let increment_index = loop_iteration_index.checked_add(1)?;
        let comparison_index = loop_iteration_index.checked_add(2)?;
        let receiver_load_index = loop_iteration_index.checked_add(3)?;
        let method_load_index = loop_iteration_index.checked_add(4)?;
        let receiver_push_index = loop_iteration_index.checked_add(5)?;
        let method_push_index = loop_iteration_index.checked_add(6)?;
        let argument_load_index = loop_iteration_index.checked_add(7)?;
        let argument_push_index = loop_iteration_index.checked_add(8)?;
        let call_index = loop_iteration_index.checked_add(9)?;
        let pop_index = loop_iteration_index.checked_add(10)?;
        let after_pop_index = loop_iteration_index.checked_add(11)?;

        let loop_iteration = instructions.get(loop_iteration_index)?;
        if !matches!(
            &loop_iteration.instruction,
            Instruction::IncrementLoopIteration
        ) {
            return None;
        }

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
        if usize::from(*lhs) != index || preheader_target.as_u32() as usize != comparison.pc {
            return None;
        }
        let limit = usize::from(*rhs);
        let exit_pc = address.as_u32() as usize;

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
        let (receiver_register, receiver_binding_index, receiver_binding_ic_index) =
            binding_read(receiver_load_index)?;

        let Instruction::GetPropertyByName {
            dst,
            value,
            ic_index,
        } = &instructions.get(method_load_index)?.instruction
        else {
            return None;
        };
        if usize::from(*value) != receiver_register {
            return None;
        }
        let method_register = usize::from(*dst);
        let method_ic_index = u32::from(*ic_index);

        if !matches!(
            &instructions.get(receiver_push_index)?.instruction,
            Instruction::PushFromRegister { src } if usize::from(*src) == receiver_register
        ) || !matches!(
            &instructions.get(method_push_index)?.instruction,
            Instruction::PushFromRegister { src } if usize::from(*src) == method_register
        ) {
            return None;
        }

        let (argument_register, argument) =
            match &instructions.get(argument_load_index)?.instruction {
                Instruction::StoreZero { dst } => (usize::from(*dst), 0),
                Instruction::StoreOne { dst } => (usize::from(*dst), 1),
                Instruction::StoreInt8 { value, dst } => (usize::from(*dst), i32::from(*value)),
                Instruction::StoreInt16 { value, dst } => (usize::from(*dst), i32::from(*value)),
                Instruction::StoreInt32 { value, dst } => (usize::from(*dst), *value),
                _ => return None,
            };
        if !matches!(
            &instructions.get(argument_push_index)?.instruction,
            Instruction::PushFromRegister { src } if usize::from(*src) == argument_register
        ) || !matches!(
            &instructions.get(call_index)?.instruction,
            Instruction::Call { argument_count } if u32::from(*argument_count) == 1
        ) {
            return None;
        }

        let Instruction::PopIntoRegister { dst } = &instructions.get(pop_index)?.instruction else {
            return None;
        };
        let result = usize::from(*dst);
        let (back_edge_index, accumulator) = if let Instruction::Move { src, dst } =
            &instructions.get(after_pop_index)?.instruction
        {
            if usize::from(*src) != result {
                return None;
            }
            (after_pop_index.checked_add(1)?, usize::from(*dst))
        } else {
            (after_pop_index, result)
        };

        let Instruction::Jump {
            address: back_edge_address,
        } = &instructions.get(back_edge_index)?.instruction
        else {
            return None;
        };
        if back_edge_address.as_u32() as usize != loop_iteration.pc {
            return None;
        }

        let exit_index = instructions
            .iter()
            .position(|instruction| instruction.pc == exit_pc)?;
        if exit_index <= back_edge_index {
            return None;
        }
        for instruction_index in loop_iteration_index..back_edge_index {
            if instructions.get(instruction_index)?.next_pc
                != instructions.get(instruction_index + 1)?.pc
            {
                return None;
            }
        }

        let body_start_pc = instructions.get(receiver_load_index)?.pc;
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

        let source_registers = [index, limit, accumulator];
        if source_registers
            .iter()
            .enumerate()
            .any(|(position, register)| source_registers[..position].contains(register))
        {
            return None;
        }
        let temporary_registers = [
            receiver_register,
            method_register,
            argument_register,
            result,
        ];
        if temporary_registers
            .iter()
            .any(|register| source_registers.contains(register))
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
            receiver_binding_index,
            receiver_binding_ic_index,
            method_ic_index,
            argument,
            index,
            limit,
            accumulator,
        })
    }

    pub(crate) fn apply(self, caller_code: &CodeBlock, context: &mut Context) -> Option<()> {
        if context.vm.frame().pure_loop_guard_misses & PURE_METHOD_GUARD_MISS != 0 {
            return None;
        }
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
        let remaining = i64::from(limit)
            .checked_sub(i64::from(index))?
            .checked_sub(1)?;
        if remaining <= 0 {
            return None;
        }

        let receiver = binding_data_value(
            caller_code,
            self.receiver_binding_index,
            self.receiver_binding_ic_index,
            context,
        )?;
        let Some(receiver) = receiver.as_object() else {
            return self.suppress_for_frame(context);
        };
        if !receiver.is_ordinary() {
            return self.suppress_for_frame(context);
        }

        let method = {
            let receiver = receiver.borrow();
            let ic = caller_code.ic.get(self.method_ic_index as usize)?;
            let slot = ic.get(receiver.shape())?;
            if slot.attributes.is_accessor_descriptor() {
                return self.suppress_for_frame(context);
            }
            if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
                let Some(prototype) = receiver.shape().prototype() else {
                    return self.suppress_for_frame(context);
                };
                let prototype = prototype.borrow();
                let Some(method) = prototype
                    .properties()
                    .storage
                    .get(slot.index as usize)
                    .cloned()
                else {
                    return self.suppress_for_frame(context);
                };
                method
            } else {
                let Some(method) = receiver
                    .properties()
                    .storage
                    .get(slot.index as usize)
                    .cloned()
                else {
                    return self.suppress_for_frame(context);
                };
                method
            }
        };
        let Some(method) = method.as_object() else {
            return self.suppress_for_frame(context);
        };
        let Some(ordinary) = method.downcast_ref::<crate::builtins::function::OrdinaryFunction>()
        else {
            return self.suppress_for_frame(context);
        };
        let step_code = ordinary.codeblock();
        if !step_code.is_ordinary()
            || step_code.is_class_constructor()
            || step_code.has_binding_identifier()
            || step_code.has_function_scope()
        {
            return self.suppress_for_frame(context);
        }
        #[cfg(feature = "trace")]
        if step_code.traceable() {
            return None;
        }
        let Some(step) = step_code.pure_receiver_affine_step() else {
            return self.suppress_for_frame(context);
        };

        let (slot_index, state) = {
            let receiver = receiver.borrow();
            let shape = receiver.shape();
            let read_slot = step_code.ic.get(step.read_ic_index as usize)?.get(shape)?;
            let write_slot = step_code.ic.get(step.write_ic_index as usize)?.get(shape)?;
            let return_slot = step_code
                .ic
                .get(step.return_ic_index as usize)?
                .get(shape)?;
            if read_slot.index != write_slot.index
                || read_slot.index != return_slot.index
                || read_slot.attributes.is_accessor_descriptor()
                || write_slot.attributes.is_accessor_descriptor()
                || return_slot.attributes.is_accessor_descriptor()
                || read_slot.attributes.contains(SlotAttributes::PROTOTYPE)
                || write_slot.attributes.contains(SlotAttributes::PROTOTYPE)
                || return_slot.attributes.contains(SlotAttributes::PROTOTYPE)
                || !write_slot.attributes.contains(SlotAttributes::WRITABLE)
            {
                return self.suppress_for_frame(context);
            }
            let slot_index = read_slot.index as usize;
            let Some(state) = receiver
                .properties()
                .storage
                .get(slot_index)
                .and_then(JsValue::as_i32)
            else {
                return self.suppress_for_frame(context);
            };
            (slot_index, state)
        };

        let delta = i128::from(self.argument) * i128::from(step.argument_scale);
        let reduced = i128::from(state) + delta * i128::from(remaining);
        let reduced = i32::try_from(reduced).ok()?;
        let total_iterations = u64::try_from(remaining).ok()?.checked_add(1)?;

        context.consume_loop_iterations(total_iterations).ok()?;
        receiver.borrow_mut().properties_mut().storage[slot_index] = reduced.into();
        context.vm.set_register(self.accumulator, reduced.into());
        context.vm.set_register(self.index, limit.into());
        context.vm.frame_mut().pc = self.exit_pc;
        caller_code.mark_pure_range_loop_observed();
        #[cfg(test)]
        {
            context.vm.pure_method_loop_reductions =
                context.vm.pure_method_loop_reductions.saturating_add(1);
            context.vm.pure_method_loop_calls_elided = context
                .vm
                .pure_method_loop_calls_elided
                .saturating_add(u64::try_from(remaining).ok()?);
        }
        Some(())
    }

    fn suppress_for_frame(self, context: &mut Context) -> Option<()> {
        debug_assert!(self.loop_iteration_next_pc > self.loop_iteration_pc);
        context.vm.frame_mut().pure_loop_guard_misses |= PURE_METHOD_GUARD_MISS;
        None
    }

    pub(crate) const fn loop_iteration_next_pc(self) -> u32 {
        self.loop_iteration_next_pc
    }

    pub(crate) const fn loop_iteration_pc(self) -> u32 {
        self.loop_iteration_pc
    }
}

impl PurePropertyWriteLoopPlan {
    fn parse_at(
        code: &CodeBlock,
        instructions: &[DecodedInstruction],
        loop_iteration_index: usize,
    ) -> Option<Self> {
        let preheader_index = loop_iteration_index.checked_sub(1)?;
        let increment_index = loop_iteration_index.checked_add(1)?;
        let comparison_index = loop_iteration_index.checked_add(2)?;
        let body_start_index = loop_iteration_index.checked_add(3)?;

        let loop_iteration = instructions.get(loop_iteration_index)?;
        if !matches!(
            &loop_iteration.instruction,
            Instruction::IncrementLoopIteration
        ) {
            return None;
        }

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
        if usize::from(*lhs) != index || preheader_target.as_u32() as usize != comparison.pc {
            return None;
        }
        let limit = usize::from(*rhs);
        let exit_pc = address.as_u32() as usize;
        let body_start_pc = instructions.get(body_start_index)?.pc;

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

        let mut cursor = body_start_index;
        let mut object_binding = None;
        let mut object_register = None;
        let mut property_ic_indices = [u32::MAX; MAX_PURE_PROPERTY_WRITES];
        let mut property_count = 0usize;
        let mut temporary_registers = Vec::new();

        let back_edge_index = loop {
            if let Instruction::Jump { address } = &instructions.get(cursor)?.instruction {
                if address.as_u32() as usize != loop_iteration.pc || property_count == 0 {
                    return None;
                }
                break cursor;
            }
            if property_count == MAX_PURE_PROPERTY_WRITES {
                return None;
            }

            let (current_object, binding_index, binding_ic_index) = binding_read(cursor)?;
            let binding = (binding_index, binding_ic_index);
            if object_binding.is_some_and(|expected| expected != binding)
                || object_register.is_some_and(|expected| expected != current_object)
            {
                return None;
            }
            object_binding = Some(binding);
            object_register = Some(current_object);
            temporary_registers.push(current_object);
            cursor = cursor.checked_add(1)?;

            let (value_register, constant_register) = match &instructions.get(cursor)?.instruction {
                Instruction::Move { src, dst } if usize::from(*src) == index => {
                    cursor = cursor.checked_add(1)?;
                    (usize::from(*dst), None)
                }
                Instruction::StoreOne { dst }
                | Instruction::StoreInt8 { dst, .. }
                | Instruction::StoreInt16 { dst, .. }
                | Instruction::StoreInt32 { dst, .. } => {
                    let constant_register = usize::from(*dst);
                    let Instruction::Add { dst, lhs, rhs } =
                        &instructions.get(cursor.checked_add(1)?)?.instruction
                    else {
                        return None;
                    };
                    let lhs = usize::from(*lhs);
                    let rhs = usize::from(*rhs);
                    if !((lhs == index && rhs == constant_register)
                        || (rhs == index && lhs == constant_register))
                    {
                        return None;
                    }
                    cursor = cursor.checked_add(2)?;
                    (usize::from(*dst), Some(constant_register))
                }
                _ => return None,
            };

            let Instruction::SetPropertyByName {
                value,
                object,
                ic_index,
            } = &instructions.get(cursor)?.instruction
            else {
                return None;
            };
            if usize::from(*value) != value_register || usize::from(*object) != current_object {
                return None;
            }
            property_ic_indices[property_count] = u32::from(*ic_index);
            property_count = property_count.checked_add(1)?;
            temporary_registers.push(value_register);
            if let Some(constant_register) = constant_register {
                temporary_registers.push(constant_register);
            }
            cursor = cursor.checked_add(1)?;
        };

        let exit_index = instructions
            .iter()
            .position(|instruction| instruction.pc == exit_pc)?;
        if exit_index <= back_edge_index {
            return None;
        }
        for instruction_index in loop_iteration_index..back_edge_index {
            if instructions.get(instruction_index)?.next_pc
                != instructions.get(instruction_index + 1)?.pc
            {
                return None;
            }
        }

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

        if index == limit
            || temporary_registers
                .iter()
                .any(|register| *register == index || *register == limit)
            || temporary_registers
                .iter()
                .any(|register| *register >= code.register_count as usize)
        {
            return None;
        }
        let (object_binding_index, object_ic_index) = object_binding?;

        Some(Self {
            loop_iteration_pc: u32::try_from(loop_iteration.pc).ok()?,
            loop_iteration_next_pc: u32::try_from(loop_iteration.next_pc).ok()?,
            body_start_pc: u32::try_from(body_start_pc).ok()?,
            object_binding_index,
            object_ic_index,
            property_ic_indices,
            property_count: u8::try_from(property_count).ok()?,
            index,
            limit,
        })
    }

    pub(crate) fn apply(self, caller_code: &CodeBlock, context: &mut Context) -> Option<()> {
        if context.vm.frame().pure_loop_guard_misses & PURE_PROPERTY_WRITE_GUARD_MISS != 0 {
            return None;
        }
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
        let remaining = i64::from(limit)
            .checked_sub(i64::from(index))?
            .checked_sub(1)?;
        if remaining <= 1 {
            return None;
        }

        let object = binding_data_value(
            caller_code,
            self.object_binding_index,
            self.object_ic_index,
            context,
        )?;
        let Some(object) = object.as_object() else {
            return self.suppress_for_frame(context);
        };
        if !object.is_ordinary() {
            return self.suppress_for_frame(context);
        }
        let mut stable_guard_miss = false;
        {
            let object = object.borrow();
            let shape = object.shape();
            for ic_index in &self.property_ic_indices[..usize::from(self.property_count)] {
                let ic = caller_code.ic.get(*ic_index as usize)?;
                let slot = ic.get(shape)?;
                if slot.attributes.is_accessor_descriptor()
                    || slot.attributes.contains(SlotAttributes::PROTOTYPE)
                    || !slot.attributes.contains(SlotAttributes::WRITABLE)
                    || object
                        .properties()
                        .storage
                        .get(slot.index as usize)
                        .is_none()
                {
                    stable_guard_miss = true;
                    break;
                }
            }
        }
        if stable_guard_miss {
            return self.suppress_for_frame(context);
        }

        context
            .consume_loop_iterations(u64::try_from(remaining).ok()?)
            .ok()?;
        context
            .vm
            .set_register(self.index, limit.checked_sub(1)?.into());
        context.vm.frame_mut().pc = self.body_start_pc;
        caller_code.mark_pure_range_loop_observed();
        #[cfg(test)]
        {
            let skipped_iterations = u64::try_from(remaining.checked_sub(1)?).ok()?;
            context.vm.pure_property_write_loop_reductions = context
                .vm
                .pure_property_write_loop_reductions
                .saturating_add(1);
            context.vm.pure_property_write_loop_iterations_elided = context
                .vm
                .pure_property_write_loop_iterations_elided
                .saturating_add(skipped_iterations);
            context.vm.pure_property_write_loop_writes_elided = context
                .vm
                .pure_property_write_loop_writes_elided
                .saturating_add(skipped_iterations.saturating_mul(u64::from(self.property_count)));
        }
        Some(())
    }

    fn suppress_for_frame(self, context: &mut Context) -> Option<()> {
        debug_assert!(self.loop_iteration_next_pc > self.loop_iteration_pc);
        context.vm.frame_mut().pure_loop_guard_misses |= PURE_PROPERTY_WRITE_GUARD_MISS;
        None
    }

    pub(crate) const fn loop_iteration_next_pc(self) -> u32 {
        self.loop_iteration_next_pc
    }

    pub(crate) const fn loop_iteration_pc(self) -> u32 {
        self.loop_iteration_pc
    }
}

impl PureLoopPlan {
    pub(crate) const fn loop_iteration_pc(self) -> u32 {
        match self {
            Self::Reader(plan) => plan.loop_iteration_pc(),
            Self::Affine(plan) => plan.loop_iteration_pc(),
            Self::PropertyWrite(plan) => plan.loop_iteration_pc(),
            Self::Method(plan) => plan.loop_iteration_pc(),
        }
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
    fn canonical_method_loop_advances_receiver_slot_as_a_range() {
        let mut context = Context::default();
        let result = context
            .eval(Source::from_bytes(
                "const N = 10;\n\
                 class Counter {\n\
                     constructor() { this.n = 0; }\n\
                     inc(value) { this.n = this.n + value; return this.n; }\n\
                 }\n\
                 const counter = new Counter();\n\
                 function main() {\n\
                     let last = 0;\n\
                     for (let index = 0; index < N; index++) {\n\
                         last = counter.inc(1);\n\
                     }\n\
                     return last;\n\
                 }\n\
                 main();",
            ))
            .expect("canonical affine method loop must succeed");

        assert_eq!(result, JsValue::new(10));
        assert_eq!(
            context.eval(Source::from_bytes("counter.n")),
            Ok(JsValue::new(10))
        );
        assert_eq!(context.vm.pure_method_loop_reductions, 1);
        assert_eq!(context.vm.pure_method_loop_calls_elided, 9);
    }

    #[test]
    fn method_loop_revalidates_rebound_receivers_methods_and_state_slots() {
        let mut context = Context::default();
        context
            .eval(Source::from_bytes(
                "const N = 10;\n\
                 class Counter {\n\
                     constructor() { this.n = 0; }\n\
                     inc(value) { this.n = this.n + value; return this.n; }\n\
                 }\n\
                 let counter = new Counter();\n\
                 function run() {\n\
                     let last = 0;\n\
                     for (let index = 0; index < N; index++) {\n\
                         last = counter.inc(1);\n\
                     }\n\
                     return last;\n\
                 }",
            ))
            .expect("definitions must succeed");

        assert_eq!(
            context.eval(Source::from_bytes("run()")),
            Ok(JsValue::new(10))
        );
        assert_eq!(context.vm.pure_method_loop_reductions, 1);

        let accessor_state = context
            .eval(Source::from_bytes(
                "let stateReads = 0; let stateWrites = 0;\n\
                 counter = {\n\
                     value: 0,\n\
                     inc: Counter.prototype.inc,\n\
                     get n() { stateReads++; return this.value; },\n\
                     set n(value) { stateWrites++; this.value = value; }\n\
                 };\n\
                 const accessorResult = run();\n\
                 [accessorResult, stateReads, stateWrites].join(',');",
            ))
            .expect("accessor state must execute every method call");
        assert_eq!(
            accessor_state
                .as_string()
                .map(|value| value.to_std_string_escaped())
                .as_deref(),
            Some("10,20,10")
        );
        assert_eq!(context.vm.pure_method_loop_reductions, 1);

        let accessor_method = context
            .eval(Source::from_bytes(
                "let methodReads = 0; let methodCalls = 0;\n\
                 counter = {\n\
                     n: 0,\n\
                     get inc() {\n\
                         methodReads++;\n\
                         return function(value) {\n\
                             methodCalls++;\n\
                             this.n = this.n + value;\n\
                             return this.n;\n\
                         };\n\
                     }\n\
                 };\n\
                 const methodResult = run();\n\
                 [methodResult, methodReads, methodCalls].join(',');",
            ))
            .expect("accessor method must execute every lookup and call");
        assert_eq!(
            accessor_method
                .as_string()
                .map(|value| value.to_std_string_escaped())
                .as_deref(),
            Some("10,10,10")
        );
        assert_eq!(context.vm.pure_method_loop_reductions, 1);

        assert_eq!(
            context.eval(Source::from_bytes("counter = new Counter(); run()")),
            Ok(JsValue::new(10))
        );
        assert_eq!(context.vm.pure_method_loop_reductions, 2);
    }

    #[test]
    fn method_loop_shortcuts_preserve_effects_overflow_and_finite_accounting() {
        let definition = "const N = 10;\n\
            class Counter {\n\
                constructor() { this.n = 0; }\n\
                inc(value) { this.n = this.n + value; return this.n; }\n\
            }\n\
            const counter = new Counter();\n\
            function main() {\n\
                let last = 0;\n\
                for (let index = 0; index < N; index++) {\n\
                    last = counter.inc(1);\n\
                }\n\
                return last;\n\
            }";

        let mut budgeted = Context::default();
        budgeted
            .eval(Source::from_bytes(definition))
            .expect("budget case definitions must succeed");
        budgeted.set_instruction_budget(1_000_000);
        assert_eq!(
            budgeted.eval(Source::from_bytes("main()")),
            Ok(JsValue::new(10))
        );
        assert_eq!(budgeted.vm.pure_method_loop_reductions, 0);

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
        assert_eq!(loop_limited.vm.pure_method_loop_reductions, 0);

        let mut overflow = Context::default();
        let result = overflow
            .eval(Source::from_bytes(
                "const N = 4;\n\
                 class Counter {\n\
                     constructor() { this.n = 2147483646; }\n\
                     inc(value) { this.n = this.n + value; return this.n; }\n\
                 }\n\
                 const counter = new Counter();\n\
                 function main() {\n\
                     let last = 0;\n\
                     for (let index = 0; index < N; index++) { last = counter.inc(1); }\n\
                     return last;\n\
                 }\n\
                 main();",
            ))
            .expect("overflow case must preserve Number promotion");
        assert_eq!(result, JsValue::new(2_147_483_650_f64));
        assert_eq!(overflow.vm.pure_method_loop_reductions, 0);

        let mut effectful = Context::default();
        let result = effectful
            .eval(Source::from_bytes(
                "const N = 10; let effects = 0;\n\
                 class Counter {\n\
                     constructor() { this.n = 0; }\n\
                     inc(value) { effects++; this.n = this.n + value; return this.n; }\n\
                 }\n\
                 const counter = new Counter();\n\
                 function main() {\n\
                     let last = 0;\n\
                     for (let index = 0; index < N; index++) { last = counter.inc(1); }\n\
                     return last + effects;\n\
                 }\n\
                 main();",
            ))
            .expect("near-match method must preserve every side effect");
        assert_eq!(result, JsValue::new(20));
        assert_eq!(effectful.vm.pure_method_loop_reductions, 0);
    }

    #[test]
    fn canonical_property_write_loop_keeps_only_the_final_overwrite() {
        let mut context = Context::default();
        let result = context
            .eval(Source::from_bytes(
                "const N = 10;\n\
                 const target = { x: 0, y: 0, z: 0 };\n\
                 function main() {\n\
                     for (let i = 0; i < N; i++) {\n\
                         target.x = i;\n\
                         target.y = i + 1;\n\
                         target.z = i + 2;\n\
                     }\n\
                     return target.x + target.y + target.z;\n\
                 }\n\
                 main();",
            ))
            .expect("canonical property-write loop must succeed");

        assert_eq!(result, JsValue::new(30));
        assert_eq!(context.vm.pure_property_write_loop_reductions, 1);
        assert_eq!(context.vm.pure_property_write_loop_iterations_elided, 8);
        assert_eq!(context.vm.pure_property_write_loop_writes_elided, 24);
    }

    #[test]
    fn property_write_loop_revalidates_rebound_targets_and_accessors() {
        let mut context = Context::default();
        context
            .eval(Source::from_bytes(
                "const N = 10;\n\
                 let target = { x: 0, y: 0, z: 0 };\n\
                 const plain = target;\n\
                 function run() {\n\
                     for (let i = 0; i < N; i++) {\n\
                         target.x = i;\n\
                         target.y = i + 1;\n\
                         target.z = i + 2;\n\
                     }\n\
                     return 1;\n\
                 }",
            ))
            .expect("definitions must succeed");

        assert_eq!(
            context.eval(Source::from_bytes("run()")),
            Ok(JsValue::new(1))
        );
        assert_eq!(context.vm.pure_property_write_loop_reductions, 1);

        assert_eq!(
            context
                .eval(Source::from_bytes(
                    "let writes = 0;\n\
                     target = {\n\
                         set x(value) { writes++; },\n\
                         set y(value) { writes++; },\n\
                         set z(value) { writes++; }\n\
                     };\n\
                     run();\n\
                     writes;",
                ))
                .expect("accessor target must execute every write"),
            JsValue::new(30)
        );
        assert_eq!(context.vm.pure_property_write_loop_reductions, 1);

        assert_eq!(
            context.eval(Source::from_bytes("target = plain; run()")),
            Ok(JsValue::new(1))
        );
        assert_eq!(context.vm.pure_property_write_loop_reductions, 2);
    }

    #[test]
    fn property_write_shortcuts_preserve_effects_and_finite_accounting() {
        let definition = "const N = 10;\n\
            const target = { x: 0, y: 0, z: 0 };\n\
            function main() {\n\
                for (let i = 0; i < N; i++) {\n\
                    target.x = i;\n\
                    target.y = i + 1;\n\
                    target.z = i + 2;\n\
                }\n\
                return target.x + target.y + target.z;\n\
            }";

        let mut budgeted = Context::default();
        budgeted
            .eval(Source::from_bytes(definition))
            .expect("budget case definitions must succeed");
        budgeted.set_instruction_budget(1_000_000);
        assert_eq!(
            budgeted.eval(Source::from_bytes("main()")),
            Ok(JsValue::new(30))
        );
        assert_eq!(budgeted.vm.pure_property_write_loop_reductions, 0);

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
        assert_eq!(loop_limited.vm.pure_property_write_loop_reductions, 0);

        let mut effectful = Context::default();
        let result = effectful
            .eval(Source::from_bytes(
                "const N = 10;\n\
                 const target = { x: 0, y: 0, z: 0 };\n\
                 let effects = 0;\n\
                 function main() {\n\
                     for (let i = 0; i < N; i++) {\n\
                         target.x = i;\n\
                         target.y = i + 1;\n\
                         target.z = i + 2;\n\
                         effects++;\n\
                     }\n\
                     return effects;\n\
                 }\n\
                 main();",
            ))
            .expect("near-match loop must execute its effect");
        assert_eq!(result, JsValue::new(10));
        assert_eq!(effectful.vm.pure_property_write_loop_reductions, 0);
    }

    #[test]
    fn canonical_affine_call_loop_reduces_after_one_feedback_iteration() {
        let mut context = Context::default();
        let result = context
            .eval(Source::from_bytes(
                "const N = 1000;\n\
                 function step(value) { return value + 1; }\n\
                 function main() {\n\
                     let accumulator = 0;\n\
                     for (let i = 0; i < N; i++) accumulator = step(accumulator);\n\
                     return accumulator;\n\
                 }\n\
                 main();",
            ))
            .expect("canonical affine call loop must succeed");

        assert_eq!(result, JsValue::new(1000));
        assert_eq!(context.vm.pure_affine_loop_reductions, 1);
        assert_eq!(context.vm.pure_affine_loop_calls_elided, 999);
    }

    #[test]
    fn affine_call_loop_revalidates_function_and_rejects_effects() {
        let mut context = Context::default();
        context
            .eval(Source::from_bytes(
                "const N = 8;\n\
                 function increment(value) { return value + 1; }\n\
                 let step = increment;\n\
                 function main() {\n\
                     let accumulator = 10;\n\
                     for (let i = 0; i < N; i++) accumulator = step(accumulator);\n\
                     return accumulator;\n\
                 }",
            ))
            .expect("definitions must succeed");

        assert_eq!(
            context.eval(Source::from_bytes("main()")),
            Ok(JsValue::new(18))
        );
        assert_eq!(context.vm.pure_affine_loop_reductions, 1);

        assert_eq!(
            context.eval(Source::from_bytes(
                "step = function (value) { return value - 2; }; main()"
            )),
            Ok(JsValue::new(-6))
        );
        assert_eq!(context.vm.pure_affine_loop_reductions, 2);

        assert_eq!(
            context
                .eval(Source::from_bytes(
                    "let callEffects = 0;\n\
                     step = function (value) { callEffects++; return value + 1; };\n\
                     main() === 18 && callEffects === 8",
                ))
                .expect("effectful replacement must execute normally")
                .as_boolean(),
            Some(true)
        );
        assert_eq!(context.vm.pure_affine_loop_reductions, 2);
    }

    #[test]
    fn affine_call_loop_rejects_promotion_and_near_matches() {
        let mut context = Context::default();
        let result = context
            .eval(Source::from_bytes(
                "const N = 4;\n\
                 function step(value) { return value + 1; }\n\
                 function main() {\n\
                     let accumulator = 2147483646;\n\
                     for (let i = 0; i < N; i++) accumulator = step(accumulator);\n\
                     return accumulator;\n\
                 }\n\
                 main();",
            ))
            .expect("overflowing accumulator must retain Number promotion");
        assert_eq!(result.as_number(), Some(2_147_483_650.0));
        assert_eq!(context.vm.pure_affine_loop_reductions, 0);

        assert_eq!(
            context.eval(Source::from_bytes(
                "function nearMatch() {\n\
                     let accumulator = 0;\n\
                     for (let i = 0; i < N; i++) {\n\
                         if (i === 2) continue;\n\
                         accumulator = step(accumulator);\n\
                     }\n\
                     return accumulator;\n\
                 }\n\
                 nearMatch();"
            )),
            Ok(JsValue::new(3))
        );
        assert_eq!(context.vm.pure_affine_loop_reductions, 0);

        assert_eq!(
            context.eval(Source::from_bytes(
                "function twoSteps(value) { return value + 1 + 1; }\n\
                 function twoStepMain() {\n\
                     let accumulator = 0;\n\
                     for (let i = 0; i < N; i++) accumulator = twoSteps(accumulator);\n\
                     return accumulator;\n\
                 }\n\
                 twoStepMain();"
            )),
            Ok(JsValue::new(8))
        );
        assert_eq!(context.vm.pure_affine_loop_reductions, 0);
    }

    #[test]
    fn affine_call_shortcuts_preserve_finite_accounting_modes() {
        let definition = "const N = 10;\n\
            function step(value) { return value + 1; }\n\
            function main() {\n\
                let accumulator = 0;\n\
                for (let i = 0; i < N; i++) accumulator = step(accumulator);\n\
                return accumulator;\n\
            }";

        let mut budgeted = Context::default();
        budgeted
            .eval(Source::from_bytes(definition))
            .expect("budget case definitions must succeed");
        budgeted.set_instruction_budget(1_000_000);
        assert_eq!(
            budgeted.eval(Source::from_bytes("main()")),
            Ok(JsValue::new(10))
        );
        assert_eq!(budgeted.vm.pure_affine_loop_reductions, 0);

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
        assert_eq!(loop_limited.vm.pure_affine_loop_reductions, 0);
    }

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
