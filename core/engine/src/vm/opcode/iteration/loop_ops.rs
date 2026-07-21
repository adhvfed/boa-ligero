use crate::error::RuntimeLimitError;
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
        let max = context.vm.runtime_limits.loop_iteration_limit();
        let frame = context.vm.frame_mut();
        let previous_iteration_count = frame.loop_iteration_count;

        // `loop_iteration_count` is the number of iterations already entered,
        // so reject the next one once the configured maximum has been reached.
        // Keep `u64::MAX` as the documented disabled sentinel instead of
        // turning its final (theoretical) increment into a limit error.
        if max != u64::MAX && previous_iteration_count >= max {
            return Err(RuntimeLimitError::LoopIteration.into());
        }

        frame.loop_iteration_count = previous_iteration_count.wrapping_add(1);
        Ok(())
    }
}

impl Operation for IncrementLoopIteration {
    const NAME: &'static str = "IncrementLoopIteration";
    const INSTRUCTION: &'static str = "INST - IncrementLoopIteration";
    const COST: u8 = 3;
}
