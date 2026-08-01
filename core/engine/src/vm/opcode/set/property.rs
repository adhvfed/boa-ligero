use crate::JsExpect;
use crate::JsValue;
use crate::value::JsVariant;
use crate::vm::opcode::{IndexOperand, RegisterOperand};
use crate::{
    Context, JsNativeError, JsResult,
    builtins::function::set_function_name,
    object::{
        IndexedProperties, internal_methods::InternalMethodPropertyContext,
        shape::slot::SlotAttributes,
    },
    property::{PropertyDescriptor, PropertyKey},
    vm::{IndexedKind, opcode::Operation},
};
use boa_macros::js_str;

/// IC-hit fast path for [`set_by_name`] that writes a plain data slot through
/// a borrowed target, skipping the `Gc::clone` of the target object the
/// `get_register().clone()` at the call sites would otherwise perform. The
/// `value` is moved into the slot (it has to be owned to be stored regardless).
///
/// Returns `Ok(())` when the write happened; `Err(value)` hands the value back
/// so the caller can take the slow path (accessor slot, IC miss, primitive
/// target, or missing prototype) without re-cloning it.
#[inline]
fn try_ic_write_no_setter(
    target: &JsValue,
    value: JsValue,
    context: &Context,
    ic_index: u32,
) -> Result<(), JsValue> {
    let Some(obj) = target.as_object_borrowed() else {
        return Err(value);
    };
    let ic = &context.vm.frame().code_block().ic[ic_index as usize];
    let object_borrowed = obj.borrow();
    let Some(slot) = ic.get(object_borrowed.shape()) else {
        return Err(value);
    };
    // Accessor slots invoke a setter that re-enters the VM with `&mut Context`,
    // which the borrows we hold preclude — hand those to the slow path.
    if slot.attributes.is_accessor_descriptor() {
        return Err(value);
    }
    let slot_index = slot.index as usize;
    if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
        let Some(prototype) = object_borrowed.shape().prototype() else {
            return Err(value);
        };
        drop(object_borrowed);
        prototype.borrow_mut().properties_mut().storage[slot_index] = value;
    } else {
        drop(object_borrowed);
        obj.borrow_mut().properties_mut().storage[slot_index] = value;
    }
    Ok(())
}

fn set_by_name(
    value: RegisterOperand,
    value_object: &JsValue,
    receiver: &JsValue,
    index: IndexOperand,
    context: &mut Context,
) -> JsResult<()> {
    let value = context.vm.get_register(value.into()).clone();

    // Fast path: if the target is already an object, write into the IC-cached
    // data slot through a non-owning borrow — skips the `to_object` clone
    // (`Gc::clone` on the inner object) the slow path performs.
    //
    // Only handles plain (non-accessor) data slots: accessor writes call
    // a setter which re-enters the VM with `&mut Context`, and the source
    // register could be reassigned across that re-entry. The owned-object
    // slow path below keeps the object alive across the setter invocation.
    if let Some(obj) = value_object.as_object_borrowed() {
        let ic = &context.vm.frame().code_block().ic[usize::from(index)];
        let object_borrowed = obj.borrow();
        if let Some(slot) = ic.get(object_borrowed.shape())
            && !slot.attributes.is_accessor_descriptor()
        {
            let slot_index = slot.index as usize;
            if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
                let prototype = object_borrowed
                    .shape()
                    .prototype()
                    .expect("prototype should have value");
                drop(object_borrowed);
                let mut prototype = prototype.borrow_mut();
                prototype.properties_mut().storage[slot_index] = value;
            } else {
                drop(object_borrowed);
                let mut object_mut = obj.borrow_mut();
                object_mut.properties_mut().storage[slot_index] = value;
            }
            return Ok(());
        }
        drop(object_borrowed);
    }

    let object = value_object.to_object(context)?;

    let ic = &context.vm.frame().code_block().ic[usize::from(index)];

    let object_borrowed = object.borrow();
    if let Some(slot) = ic.get(object_borrowed.shape()) {
        let slot_index = slot.index as usize;

        if slot.attributes.is_accessor_descriptor() {
            let result = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
                let prototype = object_borrowed
                    .shape()
                    .prototype()
                    .expect("prototype should have value");
                let prototype = prototype.borrow();

                prototype.properties().storage[slot_index + 1].clone()
            } else {
                object_borrowed.properties().storage[slot_index + 1].clone()
            };

            drop(object_borrowed);
            if slot.attributes.has_set() && result.is_object() {
                result.as_object().expect("should contain getter").call(
                    receiver,
                    std::slice::from_ref(&value),
                    context,
                )?;
            }
        } else if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
            let prototype = object_borrowed
                .shape()
                .prototype()
                .expect("prototype should have value");
            drop(object_borrowed);
            let mut prototype = prototype.borrow_mut();

            prototype.properties_mut().storage[slot_index] = value.clone();
        } else {
            drop(object_borrowed);
            let mut object_borrowed = object.borrow_mut();
            object_borrowed.properties_mut().storage[slot_index] = value.clone();
        }
        return Ok(());
    }
    drop(object_borrowed);

    let name: PropertyKey = ic.name.clone().into();

    let context = &mut InternalMethodPropertyContext::new(context);
    let succeeded = object.__set__(name.clone(), value.clone(), receiver.clone(), context)?;
    if !succeeded && context.vm.frame().code_block.strict() {
        return Err(JsNativeError::typ()
            .with_message(format!("cannot set non-writable property: {name}"))
            .into());
    }

    // Cache the property.
    let slot = *context.slot();
    if succeeded && slot.is_cacheable() {
        let ic = &context.vm.frame().code_block.ic[usize::from(index)];
        let object_borrowed = object.borrow();
        let shape = object_borrowed.shape();
        ic.set(shape, slot);
    }

    Ok(())
}

/// `SetPropertyByName` implements the Opcode Operation for `Opcode::SetPropertyByName`
///
/// Operation:
///  - Sets a property by name of an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetPropertyByName;

impl SetPropertyByName {
    #[inline(always)]
    pub(crate) fn operation(
        (value, object, index): (RegisterOperand, RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        // Fast path: write through a borrowed target, no `Gc::clone` of it.
        let value_clone = context.vm.get_register(value.into()).clone();
        let pending = {
            let target = context.vm.get_register(object.into());
            try_ic_write_no_setter(target, value_clone, context, index.into())
        };
        if pending.is_ok() {
            return Ok(());
        }
        // Slow path: accessor/IC-miss/primitive target. `set_by_name`
        // re-reads the value register (one extra clone on this cold path).
        let object = context.vm.get_register(object.into()).clone();
        set_by_name(value, &object, &object, index, context)
    }
}

impl Operation for SetPropertyByNameWithThis {
    const NAME: &'static str = "SetPropertyByNameWithThis";
    const INSTRUCTION: &'static str = "INST - SetPropertyByNameWithThis";
    const COST: u8 = 4;
}

/// `SetPropertyByNameWithThis` implements the Opcode Operation for `Opcode::SetPropertyByNameWithThis`
///
/// Operation:
///  - Sets a property by name of an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetPropertyByNameWithThis;

impl SetPropertyByNameWithThis {
    #[inline(always)]
    pub(crate) fn operation(
        (value, receiver, object, index): (
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            IndexOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        // Fast path: no getter/setter ⇒ `receiver` is unused, so skip both
        // the target and receiver clones on the borrowed IC-hit write.
        let value_clone = context.vm.get_register(value.into()).clone();
        let pending = {
            let target = context.vm.get_register(object.into());
            try_ic_write_no_setter(target, value_clone, context, index.into())
        };
        if pending.is_ok() {
            return Ok(());
        }
        let value_object = context.vm.get_register(object.into()).clone();
        let receiver = context.vm.get_register(receiver.into()).clone();
        set_by_name(value, &value_object, &receiver, index, context)
    }
}

impl Operation for SetPropertyByName {
    const NAME: &'static str = "SetPropertyByName";
    const INSTRUCTION: &'static str = "INST - SetPropertyByName";
    const COST: u8 = 4;
}

/// `SetPropertyByValue` implements the Opcode Operation for `Opcode::SetPropertyByValue`
///
/// Operation:
///  - Sets a property by value of an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetPropertyByValue;

/// Map the current `IndexedProperties` kind to an [`IndexedKind`] discriminant,
/// or `None` if the storage is sparse.
#[inline]
fn indexed_properties_write_kind(props: &IndexedProperties) -> Option<IndexedKind> {
    match props {
        IndexedProperties::DenseI32(_) => Some(IndexedKind::DenseI32),
        IndexedProperties::DenseF64(_) => Some(IndexedKind::DenseF64),
        IndexedProperties::DenseElement(_) => Some(IndexedKind::DenseElement),
        IndexedProperties::SparseElement(_) | IndexedProperties::SparseProperty(_) => None,
    }
}

impl SetPropertyByValue {
    #[inline(always)]
    pub(crate) fn operation(
        (value, key, receiver, object, ic_index): (
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            IndexOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        // --- Element-access IC fast path (set) ---
        //
        // When the key register holds a non-negative integer and the object
        // register holds an object whose shape matches the element IC, try
        // `set_dense_property` directly — skipping `to_object()`, `is_array()`,
        // and `to_property_key`.
        //
        // `set_dense_property` returns `false` on out-of-bounds or sparse
        // storage; in those cases we fall through to the slow path (which
        // handles all the edge cases correctly). A false-positive IC hit
        // (same shape, storage meanwhile went sparse) is a performance miss,
        // not a correctness bug: `set_dense_property` will simply return `false`
        // and we deopt correctly.
        {
            let key_val = context.vm.get_register(key.into());
            if let JsVariant::Integer32(raw_key) = key_val.variant()
                && raw_key >= 0
            {
                let obj_val = context.vm.get_register(object.into());
                if let Some(obj_ref) = obj_val.as_object_borrowed() {
                    let obj_borrow = obj_ref.borrow();
                    let ic = &context.vm.frame().code_block().element_ic[usize::from(ic_index)];
                    if ic.matches(obj_borrow.shape()).is_some() && obj_borrow.extensible {
                        drop(obj_borrow);
                        let val = context.vm.get_register(value.into()).clone();
                        if obj_ref
                            .borrow_mut()
                            .properties_mut()
                            .set_dense_property(raw_key as u32, &val)
                        {
                            return Ok(());
                        }
                        // Out-of-bounds or dense→sparse: fall through to slow path.
                    }
                }
            }
        }

        let value = context.vm.get_register(value.into()).clone();
        let key = context.vm.get_register(key.into()).clone();
        let receiver = context.vm.get_register(receiver.into()).clone();
        let object = context.vm.get_register(object.into()).clone();
        let object = object.to_object(context)?;

        let key = key.to_property_key(context)?;

        // Existing fast path — reached when the IC missed or the key is not
        // an integer (e.g. a computed string key).
        'fast_path: {
            if object.is_array()
                && let PropertyKey::Index(index) = &key
            {
                let mut object_borrowed = object.borrow_mut();

                // Cannot modify if not extensible.
                if !object_borrowed.extensible {
                    break 'fast_path;
                }

                if object_borrowed
                    .properties_mut()
                    .set_dense_property(index.get(), &value)
                {
                    // Seed the IC so future same-site same-shape writes can
                    // hit the fast path above without calling to_object.
                    let kind = indexed_properties_write_kind(
                        &object_borrowed.properties().indexed_properties,
                    );
                    drop(object_borrowed);
                    if let Some(kind) = kind {
                        let ic = &context.vm.frame().code_block().element_ic[usize::from(ic_index)];
                        ic.seed(object.borrow().shape(), kind);
                    }
                    return Ok(());
                }
            }
        }

        // Slow path:
        let succeeded = object.__set__(
            key.clone(),
            value.clone(),
            receiver.clone(),
            &mut context.into(),
        )?;
        if !succeeded && context.vm.frame().code_block.strict() {
            return Err(JsNativeError::typ()
                .with_message(format!("cannot set non-writable property: {key}"))
                .into());
        }

        Ok(())
    }
}

impl Operation for SetPropertyByValue {
    const NAME: &'static str = "SetPropertyByValue";
    const INSTRUCTION: &'static str = "INST - SetPropertyByValue";
    const COST: u8 = 4;
}

/// `SetPropertyGetterByName` implements the Opcode Operation for `Opcode::SetPropertyGetterByName`
///
/// Operation:
///  - Sets a getter property by name of an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetPropertyGetterByName;

impl SetPropertyGetterByName {
    #[inline(always)]
    pub(crate) fn operation(
        (object, value, index): (RegisterOperand, RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let object = context.vm.get_register(object.into()).clone();
        let value = context.vm.get_register(value.into()).clone();
        let name = context
            .vm
            .frame()
            .code_block()
            .constant_string(index.into())
            .into();

        let object = object.to_object(context)?;
        let set = object
            .__get_own_property__(&name, &mut InternalMethodPropertyContext::new(context))?
            .as_ref()
            .and_then(PropertyDescriptor::set)
            .cloned();
        object.__define_own_property__(
            &name,
            PropertyDescriptor::builder()
                .maybe_get(Some(value.clone()))
                .maybe_set(set)
                .enumerable(true)
                .configurable(true)
                .build(),
            &mut InternalMethodPropertyContext::new(context),
        )?;
        Ok(())
    }
}

impl Operation for SetPropertyGetterByName {
    const NAME: &'static str = "SetPropertyGetterByName";
    const INSTRUCTION: &'static str = "INST - SetPropertyGetterByName";
    const COST: u8 = 4;
}

/// `SetPropertyGetterByValue` implements the Opcode Operation for `Opcode::SetPropertyGetterByValue`
///
/// Operation:
///  - Sets a getter property by value of an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetPropertyGetterByValue;

impl SetPropertyGetterByValue {
    #[inline(always)]
    pub(crate) fn operation(
        (value, key, object): (RegisterOperand, RegisterOperand, RegisterOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let value = context.vm.get_register(value.into()).clone();
        let key = context.vm.get_register(key.into()).clone();
        let object = context.vm.get_register(object.into()).clone();
        let object = object.to_object(context)?;
        let name = key.to_property_key(context)?;

        let set = object
            .__get_own_property__(&name, &mut InternalMethodPropertyContext::new(context))?
            .as_ref()
            .and_then(PropertyDescriptor::set)
            .cloned();
        object.__define_own_property__(
            &name,
            PropertyDescriptor::builder()
                .maybe_get(Some(value.clone()))
                .maybe_set(set)
                .enumerable(true)
                .configurable(true)
                .build(),
            &mut InternalMethodPropertyContext::new(context),
        )?;
        Ok(())
    }
}

impl Operation for SetPropertyGetterByValue {
    const NAME: &'static str = "SetPropertyGetterByValue";
    const INSTRUCTION: &'static str = "INST - SetPropertyGetterByValue";
    const COST: u8 = 4;
}

/// `SetPropertySetterByName` implements the Opcode Operation for `Opcode::SetPropertySetterByName`
///
/// Operation:
///  - Sets a setter property by name of an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetPropertySetterByName;

impl SetPropertySetterByName {
    #[inline(always)]
    pub(crate) fn operation(
        (object, value, index): (RegisterOperand, RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let object = context.vm.get_register(object.into()).clone();
        let value = context.vm.get_register(value.into()).clone();
        let name = context
            .vm
            .frame()
            .code_block()
            .constant_string(index.into())
            .into();

        let object = object.to_object(context)?;

        let get = object
            .__get_own_property__(&name, &mut InternalMethodPropertyContext::new(context))?
            .as_ref()
            .and_then(PropertyDescriptor::get)
            .cloned();
        object.__define_own_property__(
            &name,
            PropertyDescriptor::builder()
                .maybe_set(Some(value.clone()))
                .maybe_get(get)
                .enumerable(true)
                .configurable(true)
                .build(),
            &mut InternalMethodPropertyContext::new(context),
        )?;
        Ok(())
    }
}

impl Operation for SetPropertySetterByName {
    const NAME: &'static str = "SetPropertySetterByName";
    const INSTRUCTION: &'static str = "INST - SetPropertySetterByName";
    const COST: u8 = 4;
}

/// `SetPropertySetterByValue` implements the Opcode Operation for `Opcode::SetPropertySetterByValue`
///
/// Operation:
///  - Sets a setter property by value of an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetPropertySetterByValue;

impl SetPropertySetterByValue {
    #[inline(always)]
    pub(crate) fn operation(
        (value, key, object): (RegisterOperand, RegisterOperand, RegisterOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let value = context.vm.get_register(value.into()).clone();
        let key = context.vm.get_register(key.into()).clone();
        let object = context.vm.get_register(object.into()).clone();

        let object = object.to_object(context)?;
        let name = key.to_property_key(context)?;

        let get = object
            .__get_own_property__(&name, &mut InternalMethodPropertyContext::new(context))?
            .as_ref()
            .and_then(PropertyDescriptor::get)
            .cloned();
        object.__define_own_property__(
            &name,
            PropertyDescriptor::builder()
                .maybe_set(Some(value.clone()))
                .maybe_get(get)
                .enumerable(true)
                .configurable(true)
                .build(),
            &mut InternalMethodPropertyContext::new(context),
        )?;
        Ok(())
    }
}

impl Operation for SetPropertySetterByValue {
    const NAME: &'static str = "SetPropertySetterByValue";
    const INSTRUCTION: &'static str = "INST - SetPropertySetterByValue";
    const COST: u8 = 4;
}

/// `SetFunctionName` implements the Opcode Operation for `Opcode::SetFunctionName`
///
/// Operation:
///  - Sets the name of a function object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetFunctionName;

impl SetFunctionName {
    #[inline(always)]
    pub(crate) fn operation(
        (function, name, prefix): (RegisterOperand, RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let function = context.vm.get_register(function.into()).clone();
        let name = context.vm.get_register(name.into()).clone();
        let name = match name.variant() {
            JsVariant::String(name) => PropertyKey::from(name.clone()),
            JsVariant::Symbol(name) => PropertyKey::from(name.clone()),
            _ => unreachable!(),
        };

        let prefix = match u32::from(prefix) {
            1 => Some(js_str!("get")),
            2 => Some(js_str!("set")),
            _ => None,
        };

        set_function_name(
            &function
                .as_object()
                .js_expect("function is not an object")?,
            &name,
            prefix,
            context,
        )?;
        Ok(())
    }
}

impl Operation for SetFunctionName {
    const NAME: &'static str = "SetFunctionName";
    const INSTRUCTION: &'static str = "INST - SetFunctionName";
    const COST: u8 = 4;
}
