use boa_ast::scope::BindingLocatorScope;

use crate::{
    Context, JsResult, JsValue,
    environments::Environment,
    error::JsNativeError,
    object::{internal_methods::InternalMethodPropertyContext, shape::slot::SlotAttributes},
    property::PropertyKey,
    vm::{
        BindingReference,
        opcode::{IndexOperand, Operation, RegisterOperand},
    },
};

/// `GetName` implements the Opcode Operation for `Opcode::GetName`
///
/// Operation:
///  - Find a binding on the environment chain and store its value in dst.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetName;

impl GetName {
    #[inline(always)]
    pub(crate) fn operation(
        (value, index): (RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        // Fast path: when the active environment is a plain declarative one
        // (no `with`, no eval-induced poisoning) the bytecode-resolved
        // locator already points at the correct binding, so we can skip
        // both the `BindingLocator` clone (which costs a `JsString`
        // refcount inc per access) and the `find_runtime_binding` walk.
        // Falls through to the slow path for object envs, uninitialised
        // bindings, or global-object lookups so spec error messages and
        // semantics are preserved.
        if context.binding_locator_stable() {
            let bindings_idx = usize::from(index);
            let (scope, binding_index) = {
                let b = &context.vm.frame().code_block.bindings[bindings_idx];
                (b.scope(), b.binding_index())
            };
            let result_opt = match scope {
                BindingLocatorScope::Stack(env_index) => {
                    match context.environment_expect(env_index) {
                        Environment::Declarative(env) => env.get(binding_index),
                        Environment::Object(_) => None,
                    }
                }
                BindingLocatorScope::GlobalDeclarative => {
                    context.vm.frame().realm.environment().get(binding_index)
                }
                BindingLocatorScope::GlobalObject => None,
            };
            if let Some(result) = result_opt {
                context.vm.set_register(value.into(), result);
                return Ok(());
            }
        }

        let mut binding_locator =
            context.vm.frame().code_block.bindings[usize::from(index)].clone();
        context.find_runtime_binding(&mut binding_locator)?;
        let result = context.get_binding(&binding_locator)?.ok_or_else(|| {
            let name = binding_locator.name().to_std_string_escaped();
            JsNativeError::reference().with_message(format!("{name} is not defined"))
        })?;
        context.vm.set_register(value.into(), result);
        Ok(())
    }
}

impl Operation for GetName {
    const NAME: &'static str = "GetName";
    const INSTRUCTION: &'static str = "INST - GetName";
    const COST: u8 = 4;
}

/// `GetNameGlobal` implements the Opcode Operation for `Opcode::GetNameGlobal`
///
/// Operation:
///  - Find a binding in the global object and store its value in dst.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetNameGlobal;

impl GetNameGlobal {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, index, ic_index): (RegisterOperand, IndexOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let binding_index = usize::from(index);

        // A global-object locator is already final while the active
        // environment is a plain, unpoisoned declarative environment. In
        // that common case, cloning the locator and calling
        // `find_runtime_binding` only repeats the resolver's early return.
        // Keep the check here (rather than caching the result permanently) so
        // direct eval and `with` still take the existing slow path when they
        // make runtime resolution observable.
        if context.binding_locator_stable()
            && context.vm.frame().code_block.bindings[binding_index].is_global()
        {
            return Self::get_global(dst, ic_index, context);
        }

        let mut binding_locator = context.vm.frame().code_block.bindings[binding_index].clone();
        context.find_runtime_binding(&mut binding_locator)?;

        if binding_locator.is_global() {
            return Self::get_global(dst, ic_index, context);
        }

        let result = context.get_binding(&binding_locator)?.ok_or_else(|| {
            let name = binding_locator.name().to_std_string_escaped();
            JsNativeError::reference().with_message(format!("{name} is not defined"))
        })?;

        context.vm.set_register(dst.into(), result);
        Ok(())
    }

    /// Read a binding known to resolve to the global object.
    #[inline(always)]
    fn get_global(
        dst: RegisterOperand,
        ic_index: IndexOperand,
        context: &mut Context,
    ) -> JsResult<()> {
        let object = context.global_object();

        let ic = &context.vm.frame().code_block().ic[usize::from(ic_index)];

        let object_borrowed = object.borrow();
        if let Some(slot) = ic.get(object_borrowed.shape()) {
            let mut result = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
                let prototype = object_borrowed
                    .shape()
                    .prototype()
                    .expect("prototype should have value");
                let prototype = prototype.borrow();
                prototype.properties().storage[slot.index as usize].clone()
            } else {
                object_borrowed.properties().storage[slot.index as usize].clone()
            };

            drop(object_borrowed);
            if slot.attributes.has_get() && result.is_object() {
                result = result.as_object().expect("should contain getter").call(
                    &object.clone().into(),
                    &[],
                    context,
                )?;
            }
            context.vm.set_register(dst.into(), result);
            return Ok(());
        }

        drop(object_borrowed);

        let name = ic.name.clone();
        let key: PropertyKey = name.clone().into();

        let context = &mut InternalMethodPropertyContext::new(context);
        let Some(result) = object.__try_get__(&key, object.clone().into(), context)? else {
            let name = name.to_std_string_escaped();
            return Err(JsNativeError::reference()
                .with_message(format!("{name} is not defined"))
                .into());
        };

        // Cache the property.
        let slot = *context.slot();
        if slot.is_cacheable() {
            let ic = &context.vm.frame().code_block.ic[usize::from(ic_index)];
            let object_borrowed = object.borrow();
            let shape = object_borrowed.shape();
            ic.set(shape, slot);
        }

        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetNameGlobal {
    const NAME: &'static str = "GetNameGlobal";
    const INSTRUCTION: &'static str = "INST - GetNameGlobal";
    const COST: u8 = 4;
}

/// `GetNameGlobalAndLocator` is the global-object version of
/// [`GetNameAndLocator`]. It keeps the reference in compact form so the
/// matching `SetNameByLocator` can use the same global property IC.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetNameGlobalAndLocator;

impl GetNameGlobalAndLocator {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, index, ic_index): (RegisterOperand, IndexOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let binding_index = usize::from(index);
        let ic_index = u32::from(ic_index);

        if context.binding_locator_stable()
            && context.vm.frame().code_block.bindings[binding_index].is_global()
        {
            GetNameGlobal::get_global(dst, ic_index.into(), context)?;
            context
                .vm
                .frame_mut()
                .binding_stack
                .push(BindingReference::Global { ic_index });
            return Ok(());
        }

        let mut binding_locator = context.vm.frame().code_block.bindings[binding_index].clone();
        context.find_runtime_binding(&mut binding_locator)?;
        let result = context.get_binding(&binding_locator)?.ok_or_else(|| {
            let name = binding_locator.name().to_std_string_escaped();
            JsNativeError::reference().with_message(format!("{name} is not defined"))
        })?;

        context
            .vm
            .frame_mut()
            .binding_stack
            .push(BindingReference::Locator(binding_locator));
        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetNameGlobalAndLocator {
    const NAME: &'static str = "GetNameGlobalAndLocator";
    const INSTRUCTION: &'static str = "INST - GetNameGlobalAndLocator";
    const COST: u8 = 4;
}

/// `GetLocator` implements the Opcode Operation for `Opcode::GetLocator`
///
/// Operation:
///  - Find a binding on the environment and set the `current_binding` of the current frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetLocator;

impl GetLocator {
    #[inline(always)]
    pub(crate) fn operation(index: IndexOperand, context: &mut Context) -> JsResult<()> {
        let mut binding_locator =
            context.vm.frame().code_block.bindings[usize::from(index)].clone();
        context.find_runtime_binding(&mut binding_locator)?;

        context
            .vm
            .frame_mut()
            .binding_stack
            .push(BindingReference::Locator(binding_locator));

        Ok(())
    }
}

/// `GetLocatorGlobal` captures a global-object binding for an assignment whose
/// right-hand side may mutate the environment chain.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetLocatorGlobal;

impl GetLocatorGlobal {
    #[inline(always)]
    pub(crate) fn operation(
        (index, ic_index): (IndexOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let binding_index = usize::from(index);
        let ic_index = u32::from(ic_index);

        if context.binding_locator_stable()
            && context.vm.frame().code_block.bindings[binding_index].is_global()
        {
            context
                .vm
                .frame_mut()
                .binding_stack
                .push(BindingReference::Global { ic_index });
            return Ok(());
        }

        let mut binding_locator = context.vm.frame().code_block.bindings[binding_index].clone();
        context.find_runtime_binding(&mut binding_locator)?;
        context
            .vm
            .frame_mut()
            .binding_stack
            .push(BindingReference::Locator(binding_locator));
        Ok(())
    }
}

impl Operation for GetLocatorGlobal {
    const NAME: &'static str = "GetLocatorGlobal";
    const INSTRUCTION: &'static str = "INST - GetLocatorGlobal";
    const COST: u8 = 4;
}

impl Operation for GetLocator {
    const NAME: &'static str = "GetLocator";
    const INSTRUCTION: &'static str = "INST - GetLocator";
    const COST: u8 = 4;
}

/// `GetNameAndLocator` implements the Opcode Operation for `Opcode::GetNameAndLocator`
///
/// Operation:
///  - Find a binding on the environment chain and store its value in dst, setting the
///    `current_binding` of the current frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetNameAndLocator;

impl GetNameAndLocator {
    #[inline(always)]
    pub(crate) fn operation(
        (value, index): (RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let mut binding_locator =
            context.vm.frame().code_block.bindings[usize::from(index)].clone();
        context.find_runtime_binding(&mut binding_locator)?;
        let result = context.get_binding(&binding_locator)?.ok_or_else(|| {
            let name = binding_locator.name().to_std_string_escaped();
            JsNativeError::reference().with_message(format!("{name} is not defined"))
        })?;

        context
            .vm
            .frame_mut()
            .binding_stack
            .push(BindingReference::Locator(binding_locator));
        context.vm.set_register(value.into(), result);
        Ok(())
    }
}

impl Operation for GetNameAndLocator {
    const NAME: &'static str = "GetNameAndLocator";
    const INSTRUCTION: &'static str = "INST - GetNameAndLocator";
    const COST: u8 = 4;
}

/// `GetNameOrUndefined` implements the Opcode Operation for `Opcode::GetNameOrUndefined`
///
/// Operation:
///  - Find a binding on the environment chain and store its value in dst. If the binding does not exist, store undefined.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetNameOrUndefined;

impl GetNameOrUndefined {
    #[inline(always)]
    pub(crate) fn operation(
        (value, index): (RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let mut binding_locator =
            context.vm.frame().code_block.bindings[usize::from(index)].clone();

        let is_global = binding_locator.is_global();

        context.find_runtime_binding(&mut binding_locator)?;

        let result = if let Some(value) = context.get_binding(&binding_locator)? {
            value
        } else if is_global {
            JsValue::undefined()
        } else {
            let name = binding_locator.name().to_std_string_escaped();
            return Err(JsNativeError::reference()
                .with_message(format!("{name} is not defined"))
                .into());
        };

        context.vm.set_register(value.into(), result);
        Ok(())
    }
}

impl Operation for GetNameOrUndefined {
    const NAME: &'static str = "GetNameOrUndefined";
    const INSTRUCTION: &'static str = "INST - GetNameOrUndefined";
    const COST: u8 = 4;
}
