//! Explicit resource-management opcodes.

use crate::{
    Context, JsError, JsResult,
    builtins::resource_management::{
        AsyncDisposal, DisposableResource, begin_async_disposal, suppress_error,
    },
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

/// Adds an asynchronous disposable resource to the innermost scope.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AddAsyncDisposableResource;

impl AddAsyncDisposableResource {
    #[inline(always)]
    pub(crate) fn operation(value: RegisterOperand, context: &mut Context) -> JsResult<()> {
        let value = context.vm.get_register(value.into()).clone();
        let resource = DisposableResource::asynchronous(value, context)?;
        context.vm.frame_mut().disposable_resources.push(resource);
        Ok(())
    }
}

impl Operation for AddAsyncDisposableResource {
    const NAME: &'static str = "AddAsyncDisposableResource";
    const INSTRUCTION: &'static str = "INST - AddAsyncDisposableResource";
    const COST: u8 = 6;
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

        for resource in resources.into_iter().rev() {
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

/// Begins disposing a scope that can contain asynchronous resources.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DisposeResourcesAsync;

impl DisposeResourcesAsync {
    #[inline(always)]
    pub(crate) fn operation(
        (has_error, error, result, needs_await): (
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let completion = context
            .vm
            .get_register(has_error.into())
            .to_boolean()
            .then(|| JsError::from_opaque(context.vm.get_register(error.into()).clone()));
        let resources = context.vm.frame_mut().disposable_resources.take_scope();

        match begin_async_disposal(resources, completion, context)? {
            AsyncDisposal::Complete => {
                context
                    .vm
                    .set_register(result.into(), crate::JsValue::undefined());
                context.vm.set_register(needs_await.into(), false.into());
            }
            AsyncDisposal::Pending(promise) => {
                context.vm.set_register(result.into(), promise.into());
                context.vm.set_register(needs_await.into(), true.into());
            }
        }
        Ok(())
    }
}

impl Operation for DisposeResourcesAsync {
    const NAME: &'static str = "DisposeResourcesAsync";
    const INSTRUCTION: &'static str = "INST - DisposeResourcesAsync";
    const COST: u8 = 10;
}

impl Operation for DisposeResources {
    const NAME: &'static str = "DisposeResources";
    const INSTRUCTION: &'static str = "INST - DisposeResources";
    const COST: u8 = 8;
}
