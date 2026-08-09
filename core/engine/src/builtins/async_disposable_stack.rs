//! Boa's implementation of the ECMAScript `AsyncDisposableStack` object.

use crate::{
    Context, JsArgs, JsData, JsResult, JsString, JsValue,
    builtins::{
        BuiltInBuilder, BuiltInConstructor, BuiltInObject, IntrinsicObject,
        resource_management::{AsyncDisposal, DisposableResource, begin_async_disposal},
    },
    context::intrinsics::{Intrinsics, StandardConstructor, StandardConstructors},
    error::JsNativeError,
    js_error, js_string,
    object::{JsObject, builtins::JsPromise, internal_methods::get_prototype_from_constructor},
    property::Attribute,
    realm::Realm,
    string::StaticJsStrings,
    symbol::JsSymbol,
};
use boa_gc::{Finalize, Trace};

/// The internal data of an `AsyncDisposableStack` instance.
#[derive(Debug, Default, Trace, Finalize, JsData)]
pub(crate) struct AsyncDisposableStack {
    disposed: bool,
    resources: Vec<DisposableResource>,
}

impl IntrinsicObject for AsyncDisposableStack {
    fn init(realm: &Realm) {
        let attributes = Attribute::WRITABLE | Attribute::NON_ENUMERABLE | Attribute::CONFIGURABLE;
        let dispose_async = BuiltInBuilder::callable(realm, Self::dispose_async)
            .name(js_string!("disposeAsync"))
            .length(0)
            .build();
        let get_disposed = BuiltInBuilder::callable(realm, Self::get_disposed)
            .name(js_string!("get disposed"))
            .length(0)
            .build();

        BuiltInBuilder::from_standard_constructor::<Self>(realm)
            .property(
                js_string!("disposeAsync"),
                dispose_async.clone(),
                attributes,
            )
            .property(JsSymbol::async_dispose(), dispose_async, attributes)
            .method(Self::r#use, js_string!("use"), 1)
            .method(Self::adopt, js_string!("adopt"), 2)
            .method(Self::defer, js_string!("defer"), 1)
            .method(Self::r#move, js_string!("move"), 0)
            .accessor(
                js_string!("disposed"),
                Some(get_disposed),
                None,
                Attribute::NON_ENUMERABLE | Attribute::CONFIGURABLE,
            )
            .property(
                JsSymbol::to_string_tag(),
                Self::NAME,
                Attribute::NON_ENUMERABLE | Attribute::CONFIGURABLE,
            )
            .build();
    }

    fn get(intrinsics: &Intrinsics) -> JsObject {
        Self::STANDARD_CONSTRUCTOR(intrinsics.constructors()).constructor()
    }
}

impl BuiltInObject for AsyncDisposableStack {
    const NAME: JsString = StaticJsStrings::ASYNC_DISPOSABLE_STACK;
    const ATTRIBUTE: Attribute = Attribute::WRITABLE.union(Attribute::CONFIGURABLE);
}

impl BuiltInConstructor for AsyncDisposableStack {
    const CONSTRUCTOR_ARGUMENTS: usize = 0;
    const PROTOTYPE_STORAGE_SLOTS: usize = 9;
    const CONSTRUCTOR_STORAGE_SLOTS: usize = 0;

    const STANDARD_CONSTRUCTOR: fn(&StandardConstructors) -> &StandardConstructor =
        StandardConstructors::async_disposable_stack;

    fn constructor(
        new_target: &JsValue,
        _: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        if new_target.is_undefined() {
            return Err(js_error!(TypeError: "AsyncDisposableStack constructor requires 'new'"));
        }

        let prototype = get_prototype_from_constructor(
            new_target,
            StandardConstructors::async_disposable_stack,
            context,
        )?;
        Ok(JsObject::from_proto_and_data_with_shared_shape(
            context.root_shape(),
            prototype,
            Self::default(),
        )
        .upcast()
        .into())
    }
}

impl AsyncDisposableStack {
    fn require(this: &JsValue, method: &'static str) -> JsResult<JsObject<Self>> {
        this.as_object()
            .and_then(|object| object.downcast::<Self>().ok())
            .ok_or_else(|| {
                JsNativeError::typ()
                    .with_message(format!(
                        "AsyncDisposableStack.prototype.{method} called on incompatible receiver"
                    ))
                    .into()
            })
    }

    fn ensure_pending(stack: &JsObject<Self>) -> JsResult<()> {
        if stack.borrow().data().disposed {
            return Err(JsNativeError::reference()
                .with_message("AsyncDisposableStack is already disposed")
                .into());
        }
        Ok(())
    }

    fn get_disposed(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "disposed")?;
        Ok(stack.borrow().data().disposed.into())
    }

    fn r#use(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "use")?;
        Self::ensure_pending(&stack)?;

        let value = args.get_or_undefined(0);
        let resource = if value.is_null_or_undefined() {
            DisposableResource::asynchronous(value.clone(), context)?
        } else {
            if !value.is_object() {
                return Err(
                    js_error!(TypeError: "AsyncDisposableStack.prototype.use requires an object"),
                );
            }

            DisposableResource::asynchronous(value.clone(), context)?
        };

        stack.borrow_mut().data_mut().resources.push(resource);
        Ok(value.clone())
    }

    fn adopt(this: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "adopt")?;
        Self::ensure_pending(&stack)?;

        let value = args.get_or_undefined(0).clone();
        let method = args
            .get_or_undefined(1)
            .as_function()
            .ok_or_else(|| js_error!(TypeError: "onDisposeAsync must be callable"))?;
        stack
            .borrow_mut()
            .data_mut()
            .resources
            .push(DisposableResource::adopt(value.clone(), method, true));
        Ok(value)
    }

    fn defer(this: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "defer")?;
        Self::ensure_pending(&stack)?;

        let method = args
            .get_or_undefined(0)
            .as_function()
            .ok_or_else(|| js_error!(TypeError: "onDisposeAsync must be callable"))?;
        stack
            .borrow_mut()
            .data_mut()
            .resources
            .push(DisposableResource::defer(method, true));
        Ok(JsValue::undefined())
    }

    fn r#move(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "move")?;
        Self::ensure_pending(&stack)?;

        let resources = {
            let mut stack = stack.borrow_mut();
            let stack = stack.data_mut();
            stack.disposed = true;
            std::mem::take(&mut stack.resources)
        };
        let moved = Self {
            disposed: false,
            resources,
        };
        Ok(JsObject::from_proto_and_data_with_shared_shape(
            context.root_shape(),
            context
                .intrinsics()
                .constructors()
                .async_disposable_stack()
                .prototype(),
            moved,
        )
        .upcast()
        .into())
    }

    fn dispose_async(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let stack = match Self::require(this, "disposeAsync") {
            Ok(stack) => stack,
            Err(error) => return Ok(JsPromise::reject(error, context)?.into()),
        };
        let resources = {
            let mut stack = stack.borrow_mut();
            let stack = stack.data_mut();
            if stack.disposed {
                return Ok(JsPromise::resolve(JsValue::undefined(), context)?.into());
            }
            stack.disposed = true;
            std::mem::take(&mut stack.resources)
        };

        if resources.is_empty() {
            return Ok(JsPromise::resolve(JsValue::undefined(), context)?.into());
        }

        match begin_async_disposal(resources, None, context) {
            Ok(AsyncDisposal::Pending(promise)) => Ok(promise.into()),
            Ok(AsyncDisposal::Complete) => {
                Ok(JsPromise::resolve(JsValue::undefined(), context)?.into())
            }
            Err(error) => Ok(JsPromise::reject(error, context)?.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{TestAction, run_test_actions};
    use boa_macros::js_str;

    #[test]
    fn disposal_starts_synchronously_and_resumes_in_order() {
        run_test_actions([
            TestAction::run(
                "var disposalLog = [];
                var releaseFirst;
                var first = new Promise(resolve => { releaseFirst = resolve; });
                var stack = new AsyncDisposableStack();
                stack.defer(() => { disposalLog.push('bottom'); });
                stack.defer(() => { disposalLog.push('top'); return first; });
                var disposal = stack.disposeAsync().then(() => disposalLog.push('done'));",
            ),
            TestAction::assert_eq("disposalLog.join(',')", js_str!("top")),
            TestAction::run("releaseFirst();"),
            TestAction::inspect_context(|context| context.run_jobs().unwrap()),
            TestAction::assert_eq("disposalLog.join(',')", js_str!("top,bottom,done")),
        ]);
    }

    #[test]
    fn sync_fallback_does_not_await_its_return_value() {
        run_test_actions([
            TestAction::run(
                "var fallbackLog = [];
                var never = new Promise(() => {});
                var stack = new AsyncDisposableStack();
                stack.use({ [Symbol.dispose]() { fallbackLog.push('dispose'); return never; } });
                stack.disposeAsync().then(() => fallbackLog.push('done'));",
            ),
            TestAction::assert_eq("fallbackLog.join(',')", js_str!("dispose")),
            TestAction::inspect_context(|context| context.run_jobs().unwrap()),
            TestAction::assert_eq("fallbackLog.join(',')", js_str!("dispose,done")),
        ]);
    }
}
