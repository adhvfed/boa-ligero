use boa_string::StaticJsStrings;

use crate::{
    Context, JsResult, JsValue, js_string,
    object::{
        IndexedProperties, internal_methods::InternalMethodPropertyContext,
        shape::slot::SlotAttributes,
    },
    property::PropertyKey,
    vm::{
        DenseKind,
        opcode::{IndexOperand, Operation, RegisterOperand},
    },
};

/// IC-hit fast path for [`get_by_name`] that runs without cloning the source
/// or receiver `JsValue`. Returns `Some(value)` when the inline cache hits on
/// a non-accessor property — the common case for monomorphic property reads —
/// and `None` for every situation that needs the slow path (IC miss,
/// primitive source, getter on the slot, missing prototype).
///
/// Splitting this off as a `Context`-borrowing helper lets the call sites
/// keep `source`/`receiver` as borrowed `&JsValue` from `vm.get_register()`
/// instead of cloning the underlying `Gc<JsObject>`. The previous
/// `get_register().clone()` was one ref-count inc + one matching dec per
/// property read — pure overhead on an IC hit since the value is only ever
/// borrowed through to `shape()` and `properties().storage[..]`.
#[inline]
fn try_ic_read_no_getter(source: &JsValue, context: &Context, ic_index: u32) -> Option<JsValue> {
    let obj = source.as_object_borrowed()?;
    let ic = &context.vm.frame().code_block().ic[ic_index as usize];
    let object_borrowed = obj.borrow();
    let slot = ic.get(object_borrowed.shape())?;
    // Getter slots need `&mut Context` to invoke `.call(...)`. The borrows
    // we hold on `context.vm` (through `source`/`obj`) preclude that, so
    // hand getter cases to the slow path (which clones once, freeing the
    // borrow, and then calls the getter).
    if slot.attributes.has_get() {
        return None;
    }
    let result = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
        let prototype = object_borrowed.shape().prototype()?;
        let prototype = prototype.borrow();
        prototype.properties().storage[slot.index as usize].clone()
    } else {
        object_borrowed.properties().storage[slot.index as usize].clone()
    };
    Some(result)
}

fn get_by_name<const LENGTH: bool>(
    (dst, source, receiver, index): (RegisterOperand, &JsValue, &JsValue, IndexOperand),
    context: &mut Context,
) -> JsResult<()> {
    if LENGTH {
        if let Some(object) = source.as_object()
            && object.is_array()
        {
            let value = object.borrow().properties().storage[0].clone();
            context.vm.set_register(dst.into(), value);
            return Ok(());
        } else if let Some(string) = source.as_string() {
            // NOTE: Since we’re using the prototype returned directly by `base_class()`,
            //       we need to handle string primitives separately due to the
            //       string exotic internal methods.
            context
                .vm
                .set_register(dst.into(), (string.len() as u32).into());
            return Ok(());
        }
    }

    // Fast path: if the receiver is already an object, try the IC without
    // materialising a fresh `JsObject` clone. `as_object_borrowed` returns
    // a non-owning view, so there is no `Gc` refcount inc/dec pair for an
    // IC hit. On miss we fall through to the slow path which has to
    // materialise the prototype anyway when invoking the ordinary internal
    // method.
    if let Some(obj) = source.as_object_borrowed() {
        let ic = &context.vm.frame().code_block().ic[usize::from(index)];
        let object_borrowed = obj.borrow();
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
                    receiver,
                    &[],
                    context,
                )?;
            }
            context.vm.set_register(dst.into(), result);
            return Ok(());
        }
        drop(object_borrowed);
    }

    // Slow path: primitives need `base_class()` to find their prototype
    // (Number.prototype, String.prototype, etc.) without wrapping; object
    // values reach this when the IC missed.
    let object = source.base_class(context)?;

    let ic = &context.vm.frame().code_block().ic[usize::from(index)];

    let key: PropertyKey = ic.name.clone().into();

    let context = &mut InternalMethodPropertyContext::new(context);
    let result = object.__get__(&key, receiver.clone(), context)?;

    // Cache the property.
    let slot = *context.slot();
    if slot.is_cacheable() {
        let ic = &context.vm.frame().code_block.ic[usize::from(index)];
        let object_borrowed = object.borrow();
        let shape = object_borrowed.shape();
        ic.set(shape, slot);
    }

    context.vm.set_register(dst.into(), result);
    Ok(())
}

/// Map the current indexed storage to the element-read IC's discriminant.
#[inline]
fn indexed_properties_read_kind(props: &IndexedProperties) -> DenseKind {
    match props {
        IndexedProperties::DenseI32(_) => DenseKind::DenseI32,
        IndexedProperties::DenseF64(_) => DenseKind::DenseF64,
        IndexedProperties::DenseElement(_) => DenseKind::DenseElement,
        IndexedProperties::SparseElement(_) | IndexedProperties::SparseProperty(_) => {
            DenseKind::SparseData
        }
    }
}

fn get_by_value<const PUSH_KEY: bool>(
    (dst, key, receiver, object, ic_index): (
        RegisterOperand,
        RegisterOperand,
        RegisterOperand,
        RegisterOperand,
        IndexOperand,
    ),
    context: &mut Context,
) -> JsResult<()> {
    // --- Element-access IC fast path ---
    //
    // When the key register already holds a non-negative integer and the
    // object register holds an object whose shape matches the element IC,
    // read the own indexed data directly — skipping internal-method dispatch,
    // `base_class()` (potential Gc refcount), and `to_property_key`.
    //
    // Safety of the deopt: if the property is absent or became an accessor
    // since the IC was seeded, we simply fall through to the slow path, which
    // produces the correct result. A false-positive IC hit (same shape but
    // now sparse storage) is a performance miss, not a correctness bug.
    {
        let key_val = context.vm.get_register(key.into());
        // Integers stored as JsVariant::Integer32. Non-negative i32 is a
        // valid u32 array index candidate; skip non-integers and negatives.
        if let crate::value::JsVariant::Integer32(raw_key) = key_val.variant()
            && raw_key >= 0
        {
            let base_val = context.vm.get_register(object.into());
            if let Some(obj_ref) = base_val.as_object_borrowed() {
                let obj_borrow = obj_ref.borrow();
                let ic = &context.vm.frame().code_block().element_ic[usize::from(ic_index)];
                // Shape guard: fused address-equality + liveness check.
                if ic.matches(obj_borrow.shape()).is_some()
                    && let Some(element) = obj_borrow
                        .properties()
                        .get_indexed_data_property(raw_key as u32)
                {
                    drop(obj_borrow);
                    // For `GetPropertyByValuePush` we must also store
                    // the key back. The register already holds the
                    // correct integer value, so nothing extra is needed
                    // (the caller placed the key there).
                    context.vm.set_register(dst.into(), element);
                    return Ok(());
                }
            }
        }
    }

    // --- General path: key conversion + property lookup ---
    let key_value = context.vm.get_register(key.into()).clone();
    let base = context.vm.get_register(object.into()).clone();
    let object = base.base_class(context)?;
    let key_value = key_value.to_property_key(context)?;

    // Fast paths for string indexing and `length`:
    //
    // NOTE: Since we’re using the prototype returned directly by `base_class()`,
    //       we need to handle string primitives separately due to the
    //       string exotic internal methods.
    match &key_value {
        PropertyKey::Index(index) => {
            if object.uses_ordinary_property_reads() {
                let object_borrowed = object.borrow();
                if let Some(element) = object_borrowed
                    .properties()
                    .get_indexed_data_property(index.get())
                {
                    // Seed the element IC so the next execution of this site
                    // can hit the fast path above, skipping base_class/to_property_key.
                    let kind = indexed_properties_read_kind(
                        &object_borrowed.properties().indexed_properties,
                    );
                    let shape = object_borrowed.shape().clone();
                    drop(object_borrowed);
                    let ic = &context.vm.frame().code_block().element_ic[usize::from(ic_index)];
                    ic.seed(&shape, kind);

                    if PUSH_KEY {
                        context.vm.set_register(key.into(), key_value.into());
                    }
                    context.vm.set_register(dst.into(), element);
                    return Ok(());
                }
            } else if let Some(string) = base.as_string() {
                let value = string
                    .code_unit_at(index.get() as usize)
                    .map_or_else(JsValue::undefined, |char| {
                        js_string!([char].as_slice()).into()
                    });

                if PUSH_KEY {
                    context.vm.set_register(key.into(), key_value.into());
                }
                context.vm.set_register(dst.into(), value);
                return Ok(());
            }
        }
        PropertyKey::String(string) if *string == StaticJsStrings::LENGTH => {
            if let Some(string) = base.as_string() {
                let value = string.len().into();

                if PUSH_KEY {
                    context.vm.set_register(key.into(), key_value.into());
                }
                context.vm.set_register(dst.into(), value);
                return Ok(());
            }
        }
        _ => {}
    }

    let receiver = context.vm.get_register(receiver.into());

    // Slow path: generic internal method lookup.
    let result = object.__get__(
        &key_value,
        receiver.clone(),
        &mut InternalMethodPropertyContext::new(context),
    )?;

    if PUSH_KEY {
        context.vm.set_register(key.into(), key_value.into());
    }
    context.vm.set_register(dst.into(), result);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GetLengthProperty;

impl GetLengthProperty {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, object, index): (RegisterOperand, RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        // IC-hit no-getter fast path on the borrowed register value, skipping
        // the `JsValue::clone` (which is a `Gc::clone` for object sources).
        // Array/string `.length` intrinsics are handled by the slow path —
        // they require a wider match than the IC's shape check.
        if let Some(result) = {
            let source = context.vm.get_register(object.into());
            try_ic_read_no_getter(source, context, index.into())
        } {
            context.vm.set_register(dst.into(), result);
            return Ok(());
        }
        let object = context.vm.get_register(object.into()).clone();
        get_by_name::<true>((dst, &object, &object, index), context)
    }
}

impl Operation for GetLengthProperty {
    const NAME: &'static str = "GetLengthProperty";
    const INSTRUCTION: &'static str = "INST - GetLengthProperty";
    const COST: u8 = 4;
}

/// `GetPropertyByName` implements the Opcode Operation for `Opcode::GetPropertyByName`
///
/// Operation:
///  - Get a property by name from an object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByName;

impl GetPropertyByName {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, object, index): (RegisterOperand, RegisterOperand, IndexOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        // IC-hit no-getter fast path: borrow the source register, read the
        // slot, and write the result — no `Gc::clone` of the source.
        if let Some(result) = {
            let source = context.vm.get_register(object.into());
            try_ic_read_no_getter(source, context, index.into())
        } {
            context.vm.set_register(dst.into(), result);
            return Ok(());
        }
        let object = context.vm.get_register(object.into()).clone();
        get_by_name::<false>((dst, &object, &object, index), context)
    }
}

impl Operation for GetPropertyByName {
    const NAME: &'static str = "GetPropertyByName";
    const INSTRUCTION: &'static str = "INST - GetPropertyByName";
    const COST: u8 = 4;
}

/// `GetPropertyByNameWithThis` implements the Opcode Operation for `Opcode::GetPropertyByNameWithThis`
///
/// Operation:
///  - Get a property by name from an object with this.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByNameWithThis;

impl GetPropertyByNameWithThis {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, receiver, value, index): (
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            IndexOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        // IC-hit no-getter fast path: no need to clone `receiver` either,
        // since `receiver` is only consumed when invoking a getter (which
        // forces the slow path).
        if let Some(result) = {
            let source = context.vm.get_register(value.into());
            try_ic_read_no_getter(source, context, index.into())
        } {
            context.vm.set_register(dst.into(), result);
            return Ok(());
        }
        let receiver = context.vm.get_register(receiver.into()).clone();
        let object = context.vm.get_register(value.into()).clone();
        get_by_name::<false>((dst, &object, &receiver, index), context)
    }
}

impl Operation for GetPropertyByNameWithThis {
    const NAME: &'static str = "GetPropertyByNameWithThis";
    const INSTRUCTION: &'static str = "INST - GetPropertyByNameWithThis";
    const COST: u8 = 4;
}

/// `GetPropertyByValue` implements the Opcode Operation for `Opcode::GetPropertyByValue`
///
/// Operation:
///  - Get a property by value from an object and store it in dst.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByValue;

impl GetPropertyByValue {
    #[inline(always)]
    pub(crate) fn operation(
        args: (
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            IndexOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        get_by_value::<false>(args, context)
    }
}

impl Operation for GetPropertyByValue {
    const NAME: &'static str = "GetPropertyByValue";
    const INSTRUCTION: &'static str = "INST - GetPropertyByValue";
    const COST: u8 = 4;
}

/// `GetPropertyByValuePush` implements the Opcode Operation for `Opcode::GetPropertyByValuePush`
///
/// Operation:
///  - Get a property by value from an object and store the key and value in registers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByValuePush;

impl GetPropertyByValuePush {
    #[inline(always)]
    pub(crate) fn operation(
        args: (
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            RegisterOperand,
            IndexOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        get_by_value::<true>(args, context)
    }
}

impl Operation for GetPropertyByValuePush {
    const NAME: &'static str = "GetPropertyByValuePush";
    const INSTRUCTION: &'static str = "INST - GetPropertyByValuePush";
    const COST: u8 = 4;
}
