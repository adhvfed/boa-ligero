use boa_macros::js_str;
use indoc::indoc;

use crate::{JsNativeErrorKind, TestAction, run_test_actions};

#[test]
// https://github.com/boa-dev/boa/issues/2317
fn fun_block_eval_2317() {
    run_test_actions([
        TestAction::assert_eq(
            indoc! {r#"
                (function(y){
                    {
                        eval("var x = 'inner';");
                    }
                    return y + x;
                })("arg");
            "#},
            js_str!("arginner"),
        ),
        TestAction::assert_eq(
            indoc! {r#"
                (function(y = "default"){
                    {
                        eval("var x = 'inner';");
                    }
                    return y + x;
                })();
            "#},
            js_str!("defaultinner"),
        ),
    ]);
}

#[test]
fn global_name_fast_path_preserves_dynamic_resolution() {
    run_test_actions([TestAction::assert(indoc! {r#"
        var value = 1;
        var ok = true;

        function read() {
            return value;
        }

        function write(next) {
            value = next;
            return value;
        }

        if (read() !== 1) {
            ok = false;
        }
        if (write(2) !== 2 || read() !== 2) {
            ok = false;
        }

        if ((function (object) {
            with (object) {
                return value;
            }
        })({ value: 3 }) !== 3) {
            ok = false;
        }

        function evalRead() {
            eval("var value = 4;");
            return value;
        }

        function evalWrite() {
            eval("var value = 5;");
            value = 6;
            return value;
        }

        ok && evalRead() === 4 && evalWrite() === 6 && value === 2;
    "#})]);
}

#[test]
// https://github.com/boa-dev/boa/issues/2719
fn with_env_not_panic() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            with({ p1:1,  }) {k[oa>>2]=d;}
            {
            let a12345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890 = 1,
                b = "";
            }
        "#},
        JsNativeErrorKind::Reference,
        "k is not defined",
    )]);
}

#[test]
// https://github.com/boa-dev/boa/issues/4350
fn indirect_eval_function_var_binding_4350() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            var t = [];

            var s1 = `
            function core() { t.push(1) }

            core.prototype.a = function () { t.push(2) }
            core.prototype.b = function () { t.push(3) }
            `;
            var s2 = `
            function core() { t.push(1) }

            core.prototype.a = function () { t.push(2) }
            core.prototype.b = function () { t.push(3) }
            var core = new core();
            `;
            var s3 = `
            function core() { t.push(1) }
            var core = new core();
            `;

            function run_ctx(s) {
                (1,eval)(s);
            }

            function test() {
                run_ctx(s1);
                var core1 = new core();

                run_ctx(s2);
                var core2 = core;

                run_ctx(s3);
                var core3 = core;
                return [core1, core2, core3].toString();
            }

            test();
        "#},
        js_str!("[object Object],[object Object],[object Object]"),
    )]);
}
