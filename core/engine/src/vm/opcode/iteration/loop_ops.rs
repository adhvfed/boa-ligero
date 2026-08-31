use crate::{Context, JsResult, vm::opcode::Operation};

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
