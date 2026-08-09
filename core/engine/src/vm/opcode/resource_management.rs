//! Explicit resource-management opcodes.

use crate::{
    Context, JsError, JsResult,
    builtins::resource_management::{DisposableResource, suppress_error},
    vm::opcode::{Operation, RegisterOperand},
};

/// Starts a nested lexical resource scope.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CreateDisposableResourceScope;

impl CreateDisposableResourceScope {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) {
        context.vm.frame_mut().disposable_resources.begin_scope();
    }
}

impl Operation for CreateDisposableResourceScope {
    const NAME: &'static str = "CreateDisposableResourceScope";
    const INSTRUCTION: &'static str = "INST - CreateDisposableResourceScope";
    const COST: u8 = 1;
}

/// Adds a synchronous disposable resource to the innermost scope.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AddDisposableResource;

impl AddDisposableResource {
    #[inline(always)]
    pub(crate) fn operation(value: RegisterOperand, context: &mut Context) -> JsResult<()> {
        let value = context.vm.get_register(value.into()).clone();
        let Some(resource) = DisposableResource::sync(value, context)? else {
            return Ok(());
        };
        context.vm.frame_mut().disposable_resources.push(resource);
        Ok(())
    }
}

impl Operation for AddDisposableResource {
    const NAME: &'static str = "AddDisposableResource";
    const INSTRUCTION: &'static str = "INST - AddDisposableResource";
    const COST: u8 = 5;
}

/// Disposes the innermost resource scope.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DisposeResources;

impl DisposeResources {
    #[inline(always)]
    pub(crate) fn operation(
        (has_error, error): (RegisterOperand, RegisterOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let mut completion = context
            .vm
            .get_register(has_error.into())
            .to_boolean()
            .then(|| JsError::from_opaque(context.vm.get_register(error.into()).clone()));
        let resources = context.vm.frame_mut().disposable_resources.take_scope();

        for resource in resources {
            if let Err(error) = resource.invoke(context) {
                completion = Some(match completion {
                    None => error,
                    Some(suppressed) => suppress_error(error, suppressed, context)?,
                });
            }
        }

        completion.map_or(Ok(()), Err)
    }
}

impl Operation for DisposeResources {
    const NAME: &'static str = "DisposeResources";
    const INSTRUCTION: &'static str = "INST - DisposeResources";
    const COST: u8 = 8;
}
