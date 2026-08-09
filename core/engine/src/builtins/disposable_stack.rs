//! Boa's implementation of the ECMAScript `DisposableStack` object.

use crate::{
    Context, JsArgs, JsData, JsError, JsResult, JsString, JsValue, NativeFunction,
    builtins::{BuiltInBuilder, BuiltInConstructor, BuiltInObject, IntrinsicObject},
    context::intrinsics::{Intrinsics, StandardConstructor, StandardConstructors},
    error::JsNativeError,
    js_error, js_string,
    object::{
        FunctionObjectBuilder, JsFunction, JsObject,
        internal_methods::get_prototype_from_constructor,
    },
    property::Attribute,
    realm::Realm,
    string::StaticJsStrings,
    symbol::JsSymbol,
};
use boa_gc::{Finalize, Trace};

/// A synchronous disposable resource record.
#[derive(Debug, Clone, Trace, Finalize)]
struct DisposableResource {
    value: JsValue,
    method: JsFunction,
}

/// The internal data of a `DisposableStack` instance.
#[derive(Debug, Default, Trace, Finalize, JsData)]
pub(crate) struct DisposableStack {
    disposed: bool,
    resources: Vec<DisposableResource>,
}

impl IntrinsicObject for DisposableStack {
    fn init(realm: &Realm) {
        let attributes = Attribute::WRITABLE | Attribute::NON_ENUMERABLE | Attribute::CONFIGURABLE;
        let dispose = BuiltInBuilder::callable(realm, Self::dispose)
            .name(js_string!("dispose"))
            .length(0)
            .build();
        let get_disposed = BuiltInBuilder::callable(realm, Self::get_disposed)
            .name(js_string!("get disposed"))
            .length(0)
            .build();

        BuiltInBuilder::from_standard_constructor::<Self>(realm)
            .property(js_string!("dispose"), dispose.clone(), attributes)
            .property(JsSymbol::dispose(), dispose, attributes)
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

impl BuiltInObject for DisposableStack {
    const NAME: JsString = StaticJsStrings::DISPOSABLE_STACK;
    const ATTRIBUTE: Attribute = Attribute::WRITABLE.union(Attribute::CONFIGURABLE);
}

impl BuiltInConstructor for DisposableStack {
    const CONSTRUCTOR_ARGUMENTS: usize = 0;
    const PROTOTYPE_STORAGE_SLOTS: usize = 9;
    const CONSTRUCTOR_STORAGE_SLOTS: usize = 0;

    const STANDARD_CONSTRUCTOR: fn(&StandardConstructors) -> &StandardConstructor =
        StandardConstructors::disposable_stack;

    fn constructor(
        new_target: &JsValue,
        _: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        if new_target.is_undefined() {
            return Err(js_error!(TypeError: "DisposableStack constructor requires 'new'"));
        }

        let prototype = get_prototype_from_constructor(
            new_target,
            StandardConstructors::disposable_stack,
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

impl DisposableStack {
    fn require(this: &JsValue, method: &'static str) -> JsResult<JsObject<Self>> {
        this.as_object()
            .and_then(|object| object.downcast::<Self>().ok())
            .ok_or_else(|| {
                JsNativeError::typ()
                    .with_message(format!(
                        "DisposableStack.prototype.{method} called on incompatible receiver"
                    ))
                    .into()
            })
    }

    fn ensure_pending(stack: &JsObject<Self>) -> JsResult<()> {
        if stack.borrow().data().disposed {
            return Err(JsNativeError::reference()
                .with_message("DisposableStack is already disposed")
                .into());
        }
        Ok(())
    }

    /// `get DisposableStack.prototype.disposed`.
    fn get_disposed(this: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "disposed")?;
        Ok(stack.borrow().data().disposed.into())
    }

    /// `DisposableStack.prototype.use(value)`.
    fn r#use(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "use")?;
        Self::ensure_pending(&stack)?;

        let value = args.get_or_undefined(0);
        if value.is_null_or_undefined() {
            return Ok(value.clone());
        }
        if !value.is_object() {
            return Err(js_error!(TypeError: "DisposableStack.prototype.use requires an object"));
        }

        let method = value
            .get_method(JsSymbol::dispose(), context)?
            .ok_or_else(|| js_error!(TypeError: "value does not have a callable Symbol.dispose"))?;
        let method = JsFunction::from_object(method)
            .expect("GetMethod must return a callable function object");

        stack
            .borrow_mut()
            .data_mut()
            .resources
            .push(DisposableResource {
                value: value.clone(),
                method,
            });
        Ok(value.clone())
    }

    /// `DisposableStack.prototype.adopt(value, onDispose)`.
    fn adopt(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "adopt")?;
        Self::ensure_pending(&stack)?;

        let value = args.get_or_undefined(0).clone();
        let on_dispose = args
            .get_or_undefined(1)
            .as_function()
            .ok_or_else(|| js_error!(TypeError: "onDispose must be callable"))?;
        let method = FunctionObjectBuilder::new(
            context.realm(),
            NativeFunction::from_copy_closure_with_captures(
                |_, _, captures, context| {
                    captures.1.call(
                        &JsValue::undefined(),
                        std::slice::from_ref(&captures.0),
                        context,
                    )
                },
                (value.clone(), on_dispose),
            ),
        )
        .name(js_string!())
        .length(0)
        .build();

        stack
            .borrow_mut()
            .data_mut()
            .resources
            .push(DisposableResource {
                value: JsValue::undefined(),
                method,
            });
        Ok(value)
    }

    /// `DisposableStack.prototype.defer(onDispose)`.
    fn defer(this: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "defer")?;
        Self::ensure_pending(&stack)?;

        let method = args
            .get_or_undefined(0)
            .as_function()
            .ok_or_else(|| js_error!(TypeError: "onDispose must be callable"))?;
        stack
            .borrow_mut()
            .data_mut()
            .resources
            .push(DisposableResource {
                value: JsValue::undefined(),
                method,
            });
        Ok(JsValue::undefined())
    }

    /// `DisposableStack.prototype.move()`.
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
                .disposable_stack()
                .prototype(),
            moved,
        )
        .upcast()
        .into())
    }

    /// `DisposableStack.prototype.dispose()`.
    fn dispose(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let stack = Self::require(this, "dispose")?;
        let mut resources = {
            let mut stack = stack.borrow_mut();
            let stack = stack.data_mut();
            if stack.disposed {
                return Ok(JsValue::undefined());
            }
            stack.disposed = true;
            std::mem::take(&mut stack.resources)
        };

        let mut completion: Option<JsError> = None;
        while let Some(resource) = resources.pop() {
            if let Err(error) = resource.method.call(&resource.value, &[], context) {
                completion = Some(match completion {
                    None => error,
                    Some(suppressed) => Self::suppress(error, suppressed, context)?,
                });
            }
        }

        match completion {
            Some(error) => Err(error),
            None => Ok(JsValue::undefined()),
        }
    }

    fn suppress(error: JsError, suppressed: JsError, context: &mut Context) -> JsResult<JsError> {
        let error = error.into_opaque(context)?;
        let suppressed = suppressed.into_opaque(context)?;
        let constructor = context
            .intrinsics()
            .constructors()
            .suppressed_error()
            .constructor();
        let object = constructor.construct(&[error, suppressed], None, context)?;
        Ok(JsError::from_opaque(object.into()))
    }
}

#[cfg(test)]
mod tests {
    use crate::{JsNativeErrorKind, TestAction, run_test_actions};
    use boa_macros::js_str;

    #[test]
    fn disposable_stack_lifecycle() {
        run_test_actions([
            TestAction::assert_eq("DisposableStack.length", 0),
            TestAction::assert_eq("DisposableStack.name", js_str!("DisposableStack")),
            TestAction::assert(
                "DisposableStack.prototype.dispose === DisposableStack.prototype[Symbol.dispose]",
            ),
            TestAction::run(
                "var disposalLog = [];
                var stack = new DisposableStack();
                var resource = { [Symbol.dispose]() { disposalLog.push('use'); } };
                stack.defer(() => disposalLog.push('defer'));
                stack.adopt('value', value => disposalLog.push(value));
                stack.use(resource);",
            ),
            TestAction::assert("!stack.disposed"),
            TestAction::assert("stack.use(null) === null"),
            TestAction::assert("stack.use(undefined) === undefined"),
            TestAction::assert_eq("stack.dispose()", crate::JsValue::undefined()),
            TestAction::assert("stack.disposed"),
            TestAction::assert_eq("disposalLog.join(',')", js_str!("use,value,defer")),
            TestAction::assert_eq("stack.dispose()", crate::JsValue::undefined()),
            TestAction::assert_native_error(
                "stack.defer(() => {})",
                JsNativeErrorKind::Reference,
                "DisposableStack is already disposed",
            ),
        ]);
    }

    #[test]
    fn disposable_stack_suppresses_multiple_errors() {
        run_test_actions([
            TestAction::run(
                "var first = {};
                var second = {};
                var errors = new DisposableStack();
                errors.defer(() => { throw first; });
                errors.defer(() => { throw second; });
                var disposalError;
                try { errors.dispose(); } catch (error) { disposalError = error; }",
            ),
            TestAction::assert("disposalError instanceof SuppressedError"),
            TestAction::assert("disposalError.error === first"),
            TestAction::assert("disposalError.suppressed === second"),
        ]);
    }
}
