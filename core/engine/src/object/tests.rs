use crate::{JsNativeErrorKind, JsObject, TestAction, run_test_actions};
use indoc::indoc;

#[test]
fn ordinary_has_instance_nonobject_prototype() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            function C() {}
            C.prototype = 1
            String instanceof C
        "#},
        JsNativeErrorKind::Type,
        "function has non-object prototype in instanceof check",
    )]);
}

#[test]
fn object_properties_return_order() {
    run_test_actions([
        TestAction::run_harness(),
        TestAction::run(indoc! {r#"
                var o = {
                    p1: 'v1',
                    p2: 'v2',
                    p3: 'v3',
                };
                o.p4 = 'v4';
                o[2] = 'iv2';
                o[0] = 'iv0';
                o[1] = 'iv1';
                delete o.p1;
                delete o.p3;
                o.p1 = 'v1';
            "#}),
        TestAction::assert(r#"arrayEquals(Object.keys(o), [ "0", "1", "2", "p2", "p4", "p1" ])"#),
        TestAction::assert(
            r#"arrayEquals(Object.values(o), [ "iv0", "iv1", "iv2", "v2", "v4", "v1" ])"#,
        ),
    ]);
}

#[test]
fn dense_readonly_indexed_properties_preserve_descriptors_and_transitions() {
    run_test_actions([
        TestAction::run_harness(),
        TestAction::run(indoc! {r#"
            const values = {};
            for (let index = 0; index < 3; index++) {
                Object.defineProperty(values, index, {
                    value: `value-${index}`,
                    writable: false,
                    enumerable: true,
                    configurable: true,
                });
            }
        "#}),
        TestAction::assert("values[1] === 'value-1'"),
        TestAction::assert("Object.getOwnPropertyDescriptor(values, 1).writable === false"),
        TestAction::assert("Object.getOwnPropertyDescriptor(values, 1).enumerable"),
        TestAction::assert("Object.getOwnPropertyDescriptor(values, 1).configurable"),
        TestAction::assert("arrayEquals(Object.keys(values), ['0', '1', '2'])"),
        TestAction::assert(
            "Object.defineProperty(values, 1, { value: 'changed' })[1] === 'changed'",
        ),
        TestAction::assert("delete values[1]"),
        TestAction::assert("!(1 in values)"),
        TestAction::assert("values[2] === 'value-2'"),
        TestAction::assert("Object.defineProperty(values, 0, { writable: true })[0] === 'value-0'"),
        TestAction::assert("Object.getOwnPropertyDescriptor(values, 0).writable"),
    ]);
}

#[test]
fn weak_js_object_does_not_keep_object_alive() {
    let weak = {
        let object = JsObject::with_null_proto();
        let weak = object.downgrade();
        assert!(weak.upgrade().is_some());
        weak
    };

    boa_gc::force_collect();
    assert!(weak.upgrade().is_none());
}
