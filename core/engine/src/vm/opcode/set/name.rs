use boa_ast::scope::{BindingLocator, BindingLocatorScope};

use crate::{
    Context, JsError, JsExpect, JsNativeError, JsResult, JsValue,
    environments::Environment,
    object::{internal_methods::InternalMethodPropertyContext, shape::slot::SlotAttributes},
    property::PropertyKey,
    vm::{
        BindingReference,
        opcode::{IndexOperand, Operation, RegisterOperand},
    },
};

/// Try to write `value` into the binding at `binding_index_in_code_block`
/// without cloning the [`BindingLocator`].
///
/// Returns [`None`] on a successful fast-path write. Returns
/// `Some(value)` (giving ownership back) when the caller must fall back to
/// the full [`SetName`] slow path because the binding lives in an object
/// environment, is uninitialised, sits on the global object (the
/// global-object write path is owned by [`SetNameGlobal`]), or the active
/// environment isn't a plain declarative one.
#[inline]
fn try_set_binding_fast(
    context: &mut Context,
    binding_index_in_code_block: usize,
    value: JsValue,
) -> Option<JsValue> {
    if !context.binding_locator_stable() {
        return Some(value);
    }
    let (scope, binding_index) = {
        let b = &context.vm.frame().code_block.bindings[binding_index_in_code_block];
        (b.scope(), b.binding_index())
    };
    match scope {
        BindingLocatorScope::Stack(env_index) => {
            if let Environment::Declarative(env) = context.environment_expect(env_index) {
                // `env.get` clones the stored value just to test initialisation
                // — fine for the hot path because the values written through
                // SetName are typically primitives (no GC ops on clone).
                if env.get(binding_index).is_some() {
                    env.set(binding_index, value);
                    return None;
                }
            }
            Some(value)
        }
        BindingLocatorScope::GlobalDeclarative => {
            let env = context.vm.frame().realm.environment();
            if env.get(binding_index).is_some() {
                env.set(binding_index, value);
                return None;
            }
            Some(value)
        }
        BindingLocatorScope::GlobalObject => Some(value),
    }
}

/// `ThrowMutateImmutable` implements the Opcode Operation for `Opcode::ThrowMutateImmutable`
///
/// Operation:
///  - Throws an error because the binding access is illegal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ThrowMutateImmutable;

impl ThrowMutateImmutable {
    #[inline(always)]
    pub(crate) fn operation(index: IndexOperand, context: &mut Context) -> JsError {
        let name = context
            .vm
            .frame()
            .code_block()
            .constant_string(index.into());

        JsNativeError::typ()
            .with_message(format!(
                "cannot mutate an immutable binding '{}'",
                name.to_std_string_escaped()
            ))
            .into()
    }
}

impl Operation for ThrowMutateImmutable {
    const NAME: &'static str = "ThrowMutateImmutable";
    const INSTRUCTION: &'static str = "INST - ThrowMutateImmutable";
    const COST: u8 = 2;
}

/// `SetName` implements the Opcode Operation for `Opcode::SetName`
///
/// Operation:
///  - Find a binding on the environment chain and assign its value.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetName;

impl SetName {
    #[inline(always)]
    pub(crate) fn operation(
        (value, index): (RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let value = context.vm.get_register(value.into()).clone();

        // Fast path: writes into a plain declarative environment skip the
        // `BindingLocator` clone and the `find_runtime_binding` walk. The
        // helper returns ownership of `value` back when it could not take
        // the fast path, so the slow path below can consume it.
        let Some(value) = try_set_binding_fast(context, usize::from(index), value) else {
            return Ok(());
        };

        let code_block = context.vm.frame().code_block();
        let mut binding_locator = code_block.bindings[usize::from(index)].clone();
        let strict = code_block.strict();

        context.find_runtime_binding(&mut binding_locator)?;

        verify_initialized(&binding_locator, context)?;

        context.set_binding(&binding_locator, value.clone(), strict)?;

        Ok(())
    }
}

impl Operation for SetName {
    const NAME: &'static str = "SetName";
    const INSTRUCTION: &'static str = "INST - SetName";
    const COST: u8 = 4;
}

/// `SetNameGlobal` implements the Opcode Operation for `Opcode::SetNameGlobal`.
///
/// Mirrors [`super::super::get::GetNameGlobal`] for the write path: on an IC
/// hit, write the value directly into the global object's property storage
/// without going through the ordinary `[[Set]]` internal method. On a miss
/// (or whenever the binding resolves outside the global object, e.g. through
/// a `with` env), fall through to the ordinary [`SetName`] path so the IC is
/// also seeded for the next call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetNameGlobal;

impl SetNameGlobal {
    #[inline(always)]
    pub(crate) fn operation(
        (value, index, ic_index): (RegisterOperand, IndexOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let binding_index = usize::from(index);

        // As in `GetNameGlobal`, the compile-time global-object locator is
        // final while the active environment is plain and unpoisoned. Avoid
        // cloning it and repeating the resolver's early return on every write.
        if context.binding_locator_stable()
            && context.vm.frame().code_block.bindings[binding_index].is_global()
        {
            return Self::set_global(value, ic_index, context);
        }

        let mut binding_locator = context.vm.frame().code_block.bindings[binding_index].clone();
        context.find_runtime_binding(&mut binding_locator)?;

        // Fast path: binding still resolves on the global object and the IC
        // remembers the slot. Skip `is_initialized_binding` because an IC hit
        // implies the property already exists on the global.
        if binding_locator.is_global() {
            return Self::set_global(value, ic_index, context);
        }

        // Binding now lives outside the global object (a `with` scope rebound
        // it). Defer to the ordinary slow path so the spec semantics hold.
        let value = context.vm.get_register(value.into()).clone();
        let strict = context.vm.frame().code_block().strict();
        verify_initialized(&binding_locator, context)?;
        context.set_binding(&binding_locator, value, strict)?;
        Ok(())
    }

    /// Write a binding known to resolve to the global object.
    #[inline(always)]
    pub(super) fn set_global(
        value: RegisterOperand,
        ic_index: IndexOperand,
        context: &mut Context,
    ) -> JsResult<()> {
        let value = context.vm.get_register(value.into()).clone();
        let object = context.global_object();

        let ic = &context.vm.frame().code_block().ic[usize::from(ic_index)];
        let object_borrowed = object.borrow();

        if let Some(slot) = ic.get(object_borrowed.shape()) {
            // Accessor or prototype-bound slots take the cold path so the
            // setter and prototype chain semantics are preserved exactly.
            if !slot
                .attributes
                .intersects(SlotAttributes::PROTOTYPE | SlotAttributes::GET | SlotAttributes::SET)
                && slot.attributes.contains(SlotAttributes::WRITABLE)
            {
                let slot_index = slot.index as usize;
                drop(object_borrowed);
                let mut object_mut = object.borrow_mut();
                object_mut.properties_mut().storage[slot_index] = value;
                return Ok(());
            }
        }

        drop(object_borrowed);

        // Slow path: missing IC entry, accessor slot, or prototype slot.
        // Run the ordinary `[[Set]]` so the cache can be seeded for a
        // subsequent fast path.
        let strict = context.vm.frame().code_block().strict();
        let key: PropertyKey = ic.name.clone().into();
        let receiver = object.clone().into();

        let context_inner = &mut InternalMethodPropertyContext::new(context);
        let succeeded = object.__set__(key.clone(), value.clone(), receiver, context_inner)?;
        if !succeeded && strict {
            return Err(JsNativeError::typ()
                .with_message(format!("cannot set non-writable property: {key}"))
                .into());
        }

        let slot = *context_inner.slot();
        if succeeded && slot.is_cacheable() {
            let ic = &context.vm.frame().code_block.ic[usize::from(ic_index)];
            let object_borrowed = object.borrow();
            let shape = object_borrowed.shape();
            ic.set(shape, slot);
        }
        Ok(())
    }
}

impl Operation for SetNameGlobal {
    const NAME: &'static str = "SetNameGlobal";
    const INSTRUCTION: &'static str = "INST - SetNameGlobal";
    const COST: u8 = 4;
}

/// `SetNameByLocator` implements the Opcode Operation for `Opcode::SetNameByLocator`
///
/// Operation:
///  - Assigns a value to the binding pointed by the `current_binding` of the current frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetNameByLocator;

impl SetNameByLocator {
    #[inline(always)]
    pub(crate) fn operation(value: RegisterOperand, context: &mut Context) -> JsResult<()> {
        let binding = context
            .vm
            .frame_mut()
            .binding_stack
            .pop()
            .js_expect("locator should have been popped before")?;

        match binding {
            BindingReference::Global { ic_index } => {
                SetNameGlobal::set_global(value, ic_index.into(), context)
            }
            BindingReference::Locator(binding_locator) => {
                let strict = context.vm.frame().code_block.strict();
                let value = context.vm.get_register(value.into()).clone();

                verify_initialized(&binding_locator, context)?;
                context.set_binding(&binding_locator, value, strict)?;

                Ok(())
            }
        }
    }
}

impl Operation for SetNameByLocator {
    const NAME: &'static str = "SetNameByLocator";
    const INSTRUCTION: &'static str = "INST - SetNameByLocator";
    const COST: u8 = 4;
}

/// Checks that the binding pointed by `locator` exists and is initialized.
fn verify_initialized(locator: &BindingLocator, context: &mut Context) -> JsResult<()> {
    if !context.is_initialized_binding(locator)? {
        let key = locator.name();
        let strict = context.vm.frame().code_block.strict();

        let message = match locator.scope() {
            BindingLocatorScope::GlobalObject if strict => Some(format!(
                "cannot assign to uninitialized global property `{}`",
                key.to_std_string_escaped()
            )),
            BindingLocatorScope::GlobalObject => None,
            BindingLocatorScope::GlobalDeclarative => Some(format!(
                "cannot assign to uninitialized binding `{}`",
                key.to_std_string_escaped()
            )),
            BindingLocatorScope::Stack(index) => match context.environment_expect(index) {
                Environment::Declarative(_) => Some(format!(
                    "cannot assign to uninitialized binding `{}`",
                    key.to_std_string_escaped()
                )),
                Environment::Object(_) if strict => Some(format!(
                    "cannot assign to uninitialized property `{}`",
                    key.to_std_string_escaped()
                )),
                Environment::Object(_) => None,
            },
        };

        if let Some(message) = message {
            return Err(JsNativeError::reference().with_message(message).into());
        }
    }

    Ok(())
}
