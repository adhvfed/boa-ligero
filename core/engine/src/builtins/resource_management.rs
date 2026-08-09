//! Shared runtime primitives for explicit resource management.

use crate::{Context, JsError, JsResult, JsValue, object::JsFunction, symbol::JsSymbol};
use boa_gc::{Finalize, Trace};

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
    #[unsafe_ignore_trace]
    call: DisposableCall,
}

impl DisposableResource {
    /// Creates a resource that invokes `value[Symbol.dispose]()`.
    pub(crate) fn sync(value: JsValue, context: &mut Context) -> JsResult<Option<Self>> {
        if value.is_null_or_undefined() {
            return Ok(None);
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
                call: DisposableCall::Use,
            });
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
            call: DisposableCall::Use,
        })
    }

    /// Creates a resource for `DisposableStack.prototype.adopt`.
    pub(crate) fn adopt(value: JsValue, method: JsFunction, asynchronous: bool) -> Self {
        Self {
            value,
            method: Some(method),
            await_result: asynchronous,
            call: DisposableCall::Adopt,
        }
    }

    /// Creates a resource for `DisposableStack.prototype.defer`.
    pub(crate) fn defer(method: JsFunction, asynchronous: bool) -> Self {
        Self {
            value: JsValue::undefined(),
            method: Some(method),
            await_result: asynchronous,
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
