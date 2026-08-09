use crate::{JsNativeErrorKind, TestAction, run_test_actions};
use indoc::indoc;

#[test]
fn let_is_block_scoped() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            {
              let bar = "bar";
            }
            bar;
        "#},
        JsNativeErrorKind::Reference,
        "bar is not defined",
    )]);
}

#[test]
fn const_is_block_scoped() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            {
            const bar = "bar";
            }
            bar;
        "#},
        JsNativeErrorKind::Reference,
        "bar is not defined",
    )]);
}

#[test]
fn var_not_block_scoped() {
    run_test_actions([TestAction::assert(indoc! {r#"
            {
              var bar = "bar";
            }
            bar == "bar";
        "#})]);
}

#[test]
fn functions_use_declaration_scope() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            function foo() {
                bar;
            }
            {
                let bar = "bar";
                foo();
            }
        "#},
        JsNativeErrorKind::Reference,
        "bar is not defined",
    )]);
}

#[test]
fn set_outer_var_in_block_scope() {
    run_test_actions([TestAction::assert(indoc! {r#"
            var bar;
            {
                bar = "foo";
            }
            bar == "foo";
        "#})]);
}

#[test]
fn set_outer_let_in_block_scope() {
    run_test_actions([TestAction::assert(indoc! {r#"
            let bar;
            {
                bar = "foo";
            }
            bar == "foo";
        "#})]);
}

#[test]
fn strict_global_update_does_not_recreate_a_deleted_binding() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            var updateCount = 0;
            Object.defineProperty(this, "deletedDuringUpdate", {
                configurable: true,
                get() {
                    delete this.deletedDuringUpdate;
                    return 2;
                }
            });
        "#}),
        TestAction::assert_native_error(
            indoc! {r#"
                (function() {
                    "use strict";
                    updateCount++;
                    deletedDuringUpdate ^= 3;
                    updateCount++;
                })()
            "#},
            JsNativeErrorKind::Reference,
            "cannot assign to uninitialized global property `deletedDuringUpdate`",
        ),
        TestAction::assert_eq("updateCount", 1),
        TestAction::assert("!('deletedDuringUpdate' in this)"),
    ]);
}
