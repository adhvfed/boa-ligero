use crate::{
    Context, JsResult,
    vm::{
        opcode::Operation,
        pure_reader::{
            PURE_GLOBAL_AFFINE_GUARD_MISS, PURE_METHOD_GUARD_MISS, PURE_PROPERTY_WRITE_GUARD_MISS,
        },
    },
};

/// `IncrementLoopIteration` implements the Opcode Operation for `Opcode::IncrementLoopIteration`.
///
/// Operation:
///  - Increment loop iteration count.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IncrementLoopIteration;

impl IncrementLoopIteration {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        context.consume_loop_iterations(1)
    }
}

impl Operation for IncrementLoopIteration {
    const NAME: &'static str = "IncrementLoopIteration";
    const INSTRUCTION: &'static str = "INST - IncrementLoopIteration";
    const COST: u8 = 3;
}

/// Loop-maintenance opcode installed only on a statically proven canonical
/// pure-reader loop. Ordinary loops retain [`IncrementLoopIteration`] and pay
/// no plan lookup or guard branch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PureReaderLoopIteration;

impl PureReaderLoopIteration {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        let plan = context
            .vm
            .frame()
            .code_block()
            .pure_reader_loop_plan(context.vm.frame().pc);
        if let Some(plan) = plan {
            let code = context.vm.frame().code_block.clone();
            if plan.apply(&code, context).is_some() {
                return Ok(());
            }
        }
        context.consume_loop_iterations(1)
    }
}

impl Operation for PureReaderLoopIteration {
    const NAME: &'static str = "PureReaderLoopIteration";
    const INSTRUCTION: &'static str = "INST - PureReaderLoopIteration";
    const COST: u8 = 3;
}

/// Loop-maintenance opcode installed only on a canonical single-argument call
/// recurrence. Before a runtime affine proof succeeds, the native tier may
/// lower it as ordinary maintenance; an observed range summary keeps the code
/// block in the faster interpreter path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PureAffineLoopIteration;

impl PureAffineLoopIteration {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        let plan = context
            .vm
            .frame()
            .code_block()
            .pure_affine_loop_plan(context.vm.frame().pc);
        if let Some(plan) = plan {
            let code = context.vm.frame().code_block.clone();
            if plan.apply(&code, context).is_some() {
                return Ok(());
            }
        }
        context.consume_loop_iterations(1)
    }
}

impl Operation for PureAffineLoopIteration {
    const NAME: &'static str = "PureAffineLoopIteration";
    const INSTRUCTION: &'static str = "INST - PureAffineLoopIteration";
    const COST: u8 = 3;
}

/// Loop-maintenance opcode installed on a canonical fixed-property write loop.
/// A successful guard skips overwritten middle iterations but executes the
/// final source body normally.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PurePropertyWriteLoopIteration;

impl PurePropertyWriteLoopIteration {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        if context.vm.frame().pure_loop_guard_misses & PURE_PROPERTY_WRITE_GUARD_MISS != 0 {
            return context.consume_loop_iterations(1);
        }
        let plan = context
            .vm
            .frame()
            .code_block()
            .pure_property_write_loop_plan(context.vm.frame().pc);
        if let Some(plan) = plan {
            let code = context.vm.frame().code_block.clone();
            if plan.apply(&code, context).is_some() {
                return Ok(());
            }
        }
        context.consume_loop_iterations(1)
    }
}

impl Operation for PurePropertyWriteLoopIteration {
    const NAME: &'static str = "PurePropertyWriteLoopIteration";
    const INSTRUCTION: &'static str = "INST - PurePropertyWriteLoopIteration";
    const COST: u8 = 3;
}

/// Loop-maintenance opcode installed on a canonical constant-argument method
/// recurrence. A successful guard advances the receiver's proven data slot in
/// one checked range step.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PureMethodLoopIteration;

impl PureMethodLoopIteration {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        if context.vm.frame().pure_loop_guard_misses & PURE_METHOD_GUARD_MISS != 0 {
            return context.consume_loop_iterations(1);
        }
        let plan = context
            .vm
            .frame()
            .code_block()
            .pure_method_loop_plan(context.vm.frame().pc);
        if let Some(plan) = plan {
            let code = context.vm.frame().code_block.clone();
            if plan.apply(&code, context).is_some() {
                return Ok(());
            }
        }
        context.consume_loop_iterations(1)
    }
}

impl Operation for PureMethodLoopIteration {
    const NAME: &'static str = "PureMethodLoopIteration";
    const INSTRUCTION: &'static str = "INST - PureMethodLoopIteration";
    const COST: u8 = 3;
}

/// Loop-maintenance opcode installed on a canonical no-argument call whose
/// callee advances one own global-object data slot by a constant i32 offset.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PureGlobalAffineLoopIteration;

impl PureGlobalAffineLoopIteration {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        if context.vm.frame().pure_loop_guard_misses & PURE_GLOBAL_AFFINE_GUARD_MISS != 0 {
            return context.consume_loop_iterations(1);
        }
        let plan = context
            .vm
            .frame()
            .code_block()
            .pure_global_affine_loop_plan(context.vm.frame().pc);
        if let Some(plan) = plan {
            let code = context.vm.frame().code_block.clone();
            if plan.apply(&code, context).is_some() {
                return Ok(());
            }
        }
        context.consume_loop_iterations(1)
    }
}

impl Operation for PureGlobalAffineLoopIteration {
    const NAME: &'static str = "PureGlobalAffineLoopIteration";
    const INSTRUCTION: &'static str = "INST - PureGlobalAffineLoopIteration";
    const COST: u8 = 3;
}
