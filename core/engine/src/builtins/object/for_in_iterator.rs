//! This module implements the `ForInIterator` object.
//!
//! More information:
//!  - [ECMAScript reference][spec]
//!
//! [spec]: https://tc39.es/ecma262/#sec-for-in-iterator-objects

use crate::{
    Context, JsData, JsResult, JsString, JsValue,
    builtins::{BuiltInBuilder, IntrinsicObject, iterable::create_iter_result_object},
    context::intrinsics::Intrinsics,
    error::JsNativeError,
    js_string,
    object::{JsObject, internal_methods::InternalMethodPropertyContext},
    property::PropertyKey,
    realm::Realm,
};
use boa_gc::{Finalize, Trace};
use rustc_hash::FxHashSet;
use std::collections::VecDeque;

/// The `ForInIterator` object represents an iteration over some specific object.
/// It implements the iterator protocol.
///
/// More information:
///  - [ECMAScript reference][spec]
///
/// [spec]: https://tc39.es/ecma262/#sec-for-in-iterator-objects
#[derive(Debug, Clone, Finalize, Trace, JsData)]
pub(crate) struct ForInIterator {
    object: JsValue,
    visited_keys: FxHashSet<JsString>,
    remaining_keys: VecDeque<JsString>,
    object_was_visited: bool,
}

impl IntrinsicObject for ForInIterator {
    fn init(realm: &Realm) {
        BuiltInBuilder::with_intrinsic::<Self>(realm)
            .prototype(realm.intrinsics().constructors().iterator().prototype())
            .static_method(Self::next, js_string!("next"), 0)
            .build();
    }

    fn get(intrinsics: &Intrinsics) -> JsObject {
        intrinsics.objects().iterator_prototypes().for_in()
    }
}

impl ForInIterator {
    fn new(object: JsValue) -> Self {
        Self {
            object,
            visited_keys: FxHashSet::default(),
            remaining_keys: VecDeque::default(),
            object_was_visited: false,
        }
    }

    /// `CreateForInIterator( object )`
    ///
    /// Creates a new iterator over the given object.
    ///
    /// Returns the iterator object and its `next` method as a pair,
    /// avoiding the need to look up `next` through the prototype chain.
    ///
    /// More information:
    ///  - [ECMA reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-createforiniterator
    pub(crate) fn create_for_in_iterator(
        object: JsValue,
        context: &Context,
    ) -> (JsObject, JsValue) {
        let prototypes = context.intrinsics().objects().iterator_prototypes();
        let prototype = prototypes.for_in();
        let next_method = prototype
            .borrow()
            .properties()
            .get_own_named_data_property_value(&js_string!("next").into())
            .cloned()
            .expect("%ForInIteratorPrototype%.next must be initialized");
        let iterator = prototypes
            .for_in_instance()
            .create(Self::new(object), Vec::new());

        (iterator, next_method)
    }

    /// %ForInIteratorPrototype%.next( )
    ///
    /// Gets the next result in the object.
    ///
    /// More information:
    ///  - [ECMA reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-%foriniteratorprototype%.next
    pub(crate) fn next(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let object = this.as_object();
        let mut iterator = object
            .as_ref()
            .and_then(|o| o.downcast_mut::<Self>())
            .ok_or_else(|| JsNativeError::typ().with_message("`this` is not a ForInIterator"))?;
        let mut object = iterator.object.to_object(context)?;
        loop {
            if !iterator.object_was_visited {
                let keys = object
                    .__own_property_keys__(&mut InternalMethodPropertyContext::new(context))?;
                for k in keys {
                    match k {
                        PropertyKey::String(ref k) => {
                            iterator.remaining_keys.push_back(k.clone());
                        }
                        PropertyKey::Index(i) => {
                            iterator.remaining_keys.push_back(i.get().into());
                        }
                        PropertyKey::Symbol(_) => {}
                    }
                }
                iterator.object_was_visited = true;
            }
            while let Some(r) = iterator.remaining_keys.pop_front() {
                if !iterator.visited_keys.contains(&r)
                    && let Some(desc) = object.__get_own_property__(
                        &PropertyKey::from(r.clone()),
                        &mut InternalMethodPropertyContext::new(context),
                    )?
                {
                    iterator.visited_keys.insert(r.clone());
                    if desc.expect_enumerable() {
                        return Ok(create_iter_result_object(JsValue::new(r), false, context));
                    }
                }
            }
            let proto = object.prototype().clone();
            match proto {
                Some(o) => {
                    object = o;
                }
                _ => {
                    return Ok(create_iter_result_object(
                        JsValue::undefined(),
                        true,
                        context,
                    ));
                }
            }
            iterator.object = JsValue::new(object.clone());
            iterator.object_was_visited = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterators_share_intrinsic_prototype_and_next_method() {
        let context = Context::default();
        let source = JsObject::with_null_proto();

        let (first, first_next) =
            ForInIterator::create_for_in_iterator(source.clone().into(), &context);
        let (second, second_next) = ForInIterator::create_for_in_iterator(source.into(), &context);

        let prototype = context
            .intrinsics()
            .objects()
            .iterator_prototypes()
            .for_in();
        let iterator_prototype = context.intrinsics().constructors().iterator().prototype();

        assert!(first_next.strict_equals(&second_next));
        assert_eq!(
            first.borrow().properties().shape.to_addr_usize(),
            second.borrow().properties().shape.to_addr_usize()
        );
        assert!(first.borrow().properties().shape.is_shared());
        assert!(
            first
                .prototype()
                .is_some_and(|actual| JsObject::equals(&actual, &prototype))
        );
        assert!(
            second
                .prototype()
                .is_some_and(|actual| JsObject::equals(&actual, &prototype))
        );
        assert!(
            prototype
                .prototype()
                .is_some_and(|actual| JsObject::equals(&actual, &iterator_prototype))
        );
    }
}
