use crate::error::RuntimeLimitError;
use crate::{Context, JsResult, vm::opcode::Operation};

/// `IncrementLoopIteration` implements the Opcode Operation for `Opcode::IncrementLoopIteration`.
///
/// Operation:
///  - Increment loop iteration count.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IncrementLoopIteration;

impl IncrementLoopIteration {
    /// Charges `count` iterations against the current frame's loop limit.
    #[inline]
    pub(crate) fn consume_iterations(count: u64, context: &mut Context) -> JsResult<()> {
        let max = context.vm.runtime_limits.loop_iteration_limit();
        let frame = context.vm.frame_mut();
        let previous_iteration_count = frame.loop_iteration_count;

        // Keep `u64::MAX` as the documented disabled sentinel. For finite
        // limits, subtraction avoids overflowing while checking the batch.
        if max != u64::MAX && count > max.saturating_sub(previous_iteration_count) {
            return Err(RuntimeLimitError::LoopIteration.into());
        }

        frame.loop_iteration_count = previous_iteration_count.wrapping_add(count);
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        Self::consume_iterations(1, context)
    }
}

impl Operation for IncrementLoopIteration {
    const NAME: &'static str = "IncrementLoopIteration";
    const INSTRUCTION: &'static str = "INST - IncrementLoopIteration";
    const COST: u8 = 3;
}
