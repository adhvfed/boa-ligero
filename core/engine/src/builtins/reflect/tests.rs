use crate::{JsValue, TestAction, error::RuntimeLimitError, run_test_actions};
use boa_macros::js_str;
use indoc::indoc;

#[test]
fn apply() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                var called = {};
                function f(n) { called.result = n };
                Reflect.apply(f, undefined, [42]);
            "#}),
        TestAction::assert_eq("called.result", 42),
    ]);
}

#[test]
fn construct() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                var called = {};
                function f(n) { called.result = n };
                Reflect.construct(f, [42]);
            "#}),
        TestAction::assert_eq("called.result", 42),
    ]);
}

#[test]
fn array_like_argument_lists_respect_loop_iteration_limit() {
    run_test_actions([
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_loop_iteration_limit(3);
        }),
        TestAction::assert_eq(
            "Reflect.apply((a, b, c) => a + b + c, undefined, {\n\
                0: 1, 1: 2, 2: 3, length: 3\n\
            })",
            6,
        ),
        TestAction::assert_runtime_limit_error(
            "Reflect.apply(() => {}, undefined, { length: 4 })",
            RuntimeLimitError::LoopIteration,
        ),
        TestAction::assert_runtime_limit_error(
            "Reflect.construct(function () {}, { length: 4 })",
            RuntimeLimitError::LoopIteration,
        ),
        TestAction::assert_runtime_limit_error(
            "Function.prototype.apply.call(() => {}, undefined, { length: 4 })",
            RuntimeLimitError::LoopIteration,
        ),
    ]);
}

#[test]
fn define_property() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                let obj = {};
                Reflect.defineProperty(obj, 'p', { value: 42 });
            "#}),
        TestAction::assert_eq("obj.p", 42),
    ]);
}

#[test]
fn delete_property() {
    run_test_actions([
        TestAction::run("let obj = { p: 42 };"),
        TestAction::assert("Reflect.deleteProperty(obj, 'p')"),
        TestAction::assert_eq("obj.p", JsValue::undefined()),
    ]);
}

#[test]
fn get() {
    run_test_actions([
        TestAction::run("let obj = { p: 42 };"),
        TestAction::assert_eq("Reflect.get(obj, 'p')", 42),
    ]);
}

#[test]
fn get_own_property_descriptor() {
    run_test_actions([
        TestAction::run("let obj = { p: 42 };"),
        TestAction::assert_eq("Reflect.getOwnPropertyDescriptor(obj, 'p').value", 42),
    ]);
}

#[test]
fn get_prototype_of() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                function F() { this.p = 42 };
                let f = new F();
            "#}),
        TestAction::assert_eq("Reflect.getPrototypeOf(f).constructor.name", js_str!("F")),
    ]);
}

#[test]
fn has() {
    run_test_actions([
        TestAction::run("let obj = { p: 42 };"),
        TestAction::assert("Reflect.has(obj, 'p')"),
        TestAction::assert("!Reflect.has(obj, 'p2')"),
    ]);
}

#[test]
fn is_extensible() {
    run_test_actions([
        TestAction::run("let obj = { p: 42 };"),
        TestAction::assert("Reflect.isExtensible(obj)"),
    ]);
}

#[test]
fn own_keys() {
    run_test_actions([
        TestAction::run_harness(),
        TestAction::run("let obj = { p: 42 };"),
        TestAction::assert(indoc! {r#"
                arrayEquals(
                    Reflect.ownKeys(obj),
                    ["p"]
                )
            "#}),
    ]);
}

#[test]
fn prevent_extensions() {
    run_test_actions([
        TestAction::run("let obj = { p: 42 };"),
        TestAction::assert("Reflect.preventExtensions(obj)"),
        TestAction::assert("!Reflect.isExtensible(obj)"),
    ]);
}

#[test]
fn set() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                let obj = {};
                Reflect.set(obj, 'p', 42);
            "#}),
        TestAction::assert_eq("obj.p", 42),
    ]);
}

#[test]
fn set_prototype_of() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                function F() { this.p = 42 };
                let obj = {}
                Reflect.setPrototypeOf(obj, F);
            "#}),
        TestAction::assert_eq("Reflect.getPrototypeOf(obj).name", js_str!("F")),
    ]);
}
