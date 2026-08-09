//! Shared runtime primitives for explicit resource management.

use crate::{
    Context, JsArgs, JsError, JsResult, JsValue, NativeFunction, js_string,
    object::{FunctionObjectBuilder, JsFunction, builtins::JsPromise},
    symbol::JsSymbol,
};
use boa_gc::{Finalize, Gc, GcRefCell, Trace};

/// How a resource's disposer is invoked.
#[derive(Debug, Clone, Copy)]
enum DisposableCall {
    Use,
    Adopt,
    Defer,
}

/// A resource and the operation that releases it.
///
/// This is the common representation used by disposable stacks and lexical
/// resource declarations. An absent method is meaningful: an asynchronous
/// nullish resource still introduces an explicit await during disposal.
#[derive(Debug, Clone, Trace, Finalize)]
pub(crate) struct DisposableResource {
    value: JsValue,
    method: Option<JsFunction>,
    await_result: bool,
    needs_await: bool,
    #[unsafe_ignore_trace]
    call: DisposableCall,
}

impl DisposableResource {
    /// Creates a resource that invokes `value[Symbol.dispose]()`.
    pub(crate) fn sync(value: JsValue, context: &mut Context) -> JsResult<Option<Self>> {
        if value.is_null_or_undefined() {
            return Ok(None);
        }
        if !value.is_object() {
            return Err(crate::js_error!(TypeError: "disposable resource must be an object"));
        }

        let method = value.get_method(JsSymbol::dispose(), context)?.ok_or_else(
            || crate::js_error!(TypeError: "value does not have a callable Symbol.dispose"),
        )?;

        Ok(Some(Self {
            value,
            method: Some(
                JsFunction::from_object(method)
                    .expect("GetMethod must return a callable function object"),
            ),
            await_result: false,
            needs_await: false,
            call: DisposableCall::Use,
        }))
    }

    /// Creates a resource that prefers `Symbol.asyncDispose` and falls back to
    /// `Symbol.dispose`.
    pub(crate) fn asynchronous(value: JsValue, context: &mut Context) -> JsResult<Self> {
        if value.is_null_or_undefined() {
            return Ok(Self {
                value,
                method: None,
                await_result: true,
                needs_await: true,
                call: DisposableCall::Use,
            });
        }
        if !value.is_object() {
            return Err(crate::js_error!(TypeError: "disposable resource must be an object"));
        }

        let (method, await_result) =
            if let Some(method) = value.get_method(JsSymbol::async_dispose(), context)? {
                (method, true)
            } else {
                let method = value.get_method(JsSymbol::dispose(), context)?.ok_or_else(
                || crate::js_error!(TypeError: "value does not have a callable disposal method"),
            )?;
                (method, false)
            };

        Ok(Self {
            value,
            method: Some(
                JsFunction::from_object(method)
                    .expect("GetMethod must return a callable function object"),
            ),
            await_result,
            needs_await: true,
            call: DisposableCall::Use,
        })
    }

    /// Creates a resource for `DisposableStack.prototype.adopt`.
    pub(crate) fn adopt(value: JsValue, method: JsFunction, asynchronous: bool) -> Self {
        Self {
            value,
            method: Some(method),
            await_result: asynchronous,
            needs_await: asynchronous,
            call: DisposableCall::Adopt,
        }
    }

    /// Creates a resource for `DisposableStack.prototype.defer`.
    pub(crate) fn defer(method: JsFunction, asynchronous: bool) -> Self {
        Self {
            value: JsValue::undefined(),
            method: Some(method),
            await_result: asynchronous,
            needs_await: asynchronous,
            call: DisposableCall::Defer,
        }
    }

    /// Invokes this resource's disposer.
    pub(crate) fn invoke(&self, context: &mut Context) -> JsResult<JsValue> {
        let Some(method) = &self.method else {
            return Ok(JsValue::undefined());
        };

        let result = match self.call {
            DisposableCall::Use => method.call(&self.value, &[], context),
            DisposableCall::Adopt => method.call(
                &JsValue::undefined(),
                std::slice::from_ref(&self.value),
                context,
            ),
            DisposableCall::Defer => method.call(&JsValue::undefined(), &[], context),
        }?;

        if self.await_result {
            Ok(result)
        } else {
            Ok(JsValue::undefined())
        }
    }

    /// Whether disposal must suspend on this resource's result.
    pub(crate) const fn needs_await(&self) -> bool {
        self.needs_await
    }
}

/// Frame-local storage for nested lexical resource scopes.
#[derive(Debug, Default, Clone, Trace, Finalize)]
pub(crate) struct DisposableResourceStack {
    resources: Vec<DisposableResource>,
    #[unsafe_ignore_trace]
    scopes: Vec<usize>,
}

impl DisposableResourceStack {
    /// Starts a nested resource scope.
    pub(crate) fn begin_scope(&mut self) {
        self.scopes.push(self.resources.len());
    }

    /// Adds a resource to the innermost scope.
    pub(crate) fn push(&mut self, resource: DisposableResource) {
        assert!(!self.scopes.is_empty(), "resource scope must be active");
        self.resources.push(resource);
    }

    /// Removes and returns the innermost scope in disposal order.
    pub(crate) fn take_scope(&mut self) -> Vec<DisposableResource> {
        let start = self
            .scopes
            .pop()
            .expect("disposal bytecode must balance resource scopes");
        self.resources.split_off(start)
    }
}

#[derive(Debug, Trace, Finalize)]
struct AsyncDisposalState {
    resources: Vec<DisposableResource>,
    completion: Option<JsError>,
}

/// The initial result of asynchronous disposal.
pub(crate) enum AsyncDisposal {
    /// No asynchronous resource was reached, so disposal completed inline.
    Complete,
    /// Disposal suspended at an asynchronous resource.
    Pending(JsPromise),
}

/// Starts disposal and runs synchronous resources inline until the first
/// asynchronous resource requires suspension.
pub(crate) fn begin_async_disposal(
    resources: Vec<DisposableResource>,
    completion: Option<JsError>,
    context: &mut Context,
) -> JsResult<AsyncDisposal> {
    let state = Gc::new(GcRefCell::new(AsyncDisposalState {
        resources,
        completion,
    }));
    match resume_async_disposal(state, context)? {
        Some(promise) => Ok(AsyncDisposal::Pending(promise)),
        None => Ok(AsyncDisposal::Complete),
    }
}

fn resume_async_disposal(
    state: Gc<GcRefCell<AsyncDisposalState>>,
    context: &mut Context,
) -> JsResult<Option<JsPromise>> {
    loop {
        let Some(resource) = state.borrow_mut().resources.pop() else {
            return match state.borrow().completion.clone() {
                Some(error) => Err(error),
                None => Ok(None),
            };
        };

        match resource.invoke(context) {
            Ok(result) if resource.needs_await() => {
                let promise = JsPromise::resolve(result, context)?;
                let on_fulfilled = async_disposal_handler(state.clone(), false, context);
                let on_rejected = async_disposal_handler(state, true, context);
                return Ok(Some(promise.then_intrinsic(
                    Some(on_fulfilled),
                    Some(on_rejected),
                    context,
                )));
            }
            Ok(_) => {}
            Err(error) => record_async_error(&state, error, context)?,
        }
    }
}

fn async_disposal_handler(
    state: Gc<GcRefCell<AsyncDisposalState>>,
    rejected: bool,
    context: &mut Context,
) -> JsFunction {
    #[derive(Trace, Finalize)]
    struct Captures {
        state: Gc<GcRefCell<AsyncDisposalState>>,
        #[unsafe_ignore_trace]
        rejected: bool,
    }

    FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            |_, args, captures, context| {
                if captures.rejected {
                    record_async_error(
                        &captures.state,
                        JsError::from_opaque(args.get_or_undefined(0).clone()),
                        context,
                    )?;
                }
                Ok(resume_async_disposal(captures.state.clone(), context)?
                    .map_or_else(JsValue::undefined, Into::into))
            },
            Captures { state, rejected },
        ),
    )
    .name(js_string!())
    .length(1)
    .build()
}

fn record_async_error(
    state: &Gc<GcRefCell<AsyncDisposalState>>,
    error: JsError,
    context: &mut Context,
) -> JsResult<()> {
    let suppressed = state.borrow_mut().completion.take();
    state.borrow_mut().completion = Some(match suppressed {
        None => error,
        Some(suppressed) => suppress_error(error, suppressed, context)?,
    });
    Ok(())
}

/// Combines a disposal failure with the completion it replaces.
pub(crate) fn suppress_value(
    error: JsValue,
    suppressed: JsValue,
    context: &mut Context,
) -> JsResult<JsValue> {
    let constructor = context
        .intrinsics()
        .constructors()
        .suppressed_error()
        .constructor();
    Ok(constructor
        .construct(&[error, suppressed], None, context)?
        .into())
}

/// Combines opaque or native errors without losing their JavaScript values.
pub(crate) fn suppress_error(
    error: JsError,
    suppressed: JsError,
    context: &mut Context,
) -> JsResult<JsError> {
    let error = error.into_opaque(context)?;
    let suppressed = suppressed.into_opaque(context)?;
    Ok(JsError::from_opaque(suppress_value(
        error, suppressed, context,
    )?))
}
