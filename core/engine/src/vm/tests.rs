use crate::error::{EngineError, RuntimeLimitError};
use crate::vm::CallFrame;
use crate::vm::call_frame::CallFrameLocation;
use crate::vm::source_info::SourcePath;
use crate::{
    Context, JsNativeErrorKind, JsValue, NativeFunction, TestAction,
    builtins::function::OrdinaryFunction, js_string, property::Attribute, run_test_actions,
    run_test_actions_with,
};
use boa_ast::Position;
use boa_macros::js_str;
use boa_parser::Source;
use indoc::indoc;

const EMOTION_HASH_SOURCE: &str = r#"function emotionHash(e){for(var t,r=0,n=0,i=e.length;i>=4;++n,i-=4)t=(65535&(t=255&e.charCodeAt(n)|(255&e.charCodeAt(++n))<<8|(255&e.charCodeAt(++n))<<16|(255&e.charCodeAt(++n))<<24))*1540483477+((t>>>16)*59797<<16),t^=t>>>24,r=(65535&t)*1540483477+((t>>>16)*59797<<16)^(65535&r)*1540483477+((r>>>16)*59797<<16);switch(i){case 3:r^=(255&e.charCodeAt(n+2))<<16;case 2:r^=(255&e.charCodeAt(n+1))<<8;case 1:r^=255&e.charCodeAt(n),r=(65535&r)*1540483477+((r>>>16)*59797<<16)}return r^=r>>>13,r=(65535&r)*1540483477+((r>>>16)*59797<<16),((r^r>>>15)>>>0).toString(36)}"#;

const EMOTION_HASH_BLOCK_SOURCE: &str = r#"function emotionHash(e) {
  for (var t, r = 0, n = 0, i = e.length; i >= 4; ++n, i -= 4) {
    t = (65535 & (t = 255 & e.charCodeAt(n) |
      (255 & e.charCodeAt(++n)) << 8 |
      (255 & e.charCodeAt(++n)) << 16 |
      (255 & e.charCodeAt(++n)) << 24)) * 0x5bd1e995 + ((t >>> 16) * 59797 << 16);
    t ^= t >>> 24;
    r = (65535 & t) * 0x5bd1e995 + ((t >>> 16) * 59797 << 16) ^
      (65535 & r) * 0x5bd1e995 + ((r >>> 16) * 59797 << 16);
  }
  switch (i) {
    case 3: r ^= (255 & e.charCodeAt(n + 2)) << 16;
    case 2: r ^= (255 & e.charCodeAt(n + 1)) << 8;
    case 1:
      r ^= 255 & e.charCodeAt(n);
      r = (65535 & r) * 0x5bd1e995 + ((r >>> 16) * 59797 << 16);
  }
  r ^= r >>> 13;
  r = (65535 & r) * 0x5bd1e995 + ((r >>> 16) * 59797 << 16);
  return ((r ^ r >>> 15) >>> 0).toString(36);
}"#;

#[test]
fn typeof_string() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            const a = "hello";
            typeof a;
        "#},
        js_str!("string"),
    )]);
}

#[test]
fn typeof_number() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let a = 1234;
            typeof a;
        "#},
        js_str!("number"),
    )]);
}

#[test]
fn basic_op() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            const a = 1;
            const b = 2;
            a + b
        "#},
        3,
    )]);
}

#[test]
fn primitive_char_code_at_and_imul_calls_use_guarded_native_fast_paths() {
    let mut context = Context::default();
    let result = context
        .eval(Source::from_bytes(
            r#"JSON.stringify(["AZ".charCodeAt(1), Math.imul(-7, 6)])"#,
        ))
        .expect("canonical primitive built-ins must evaluate");
    assert_eq!(result, js_string!("[90,-42]").into());
    assert_eq!(context.vm.native_builtin_fast_calls, 2);

    let patched = context
        .eval(Source::from_bytes(
            r#"
                String.prototype.charCodeAt = function () { return 77; };
                Math.imul = function () { return 81; };
                JSON.stringify(["AZ".charCodeAt(1), Math.imul(-7, 6)]);
            "#,
        ))
        .expect("monkey-patched built-ins must evaluate normally");
    assert_eq!(patched, js_string!("[77,81]").into());
    assert_eq!(
        context.vm.native_builtin_fast_calls, 2,
        "resolved replacements must bypass the pointer-identity fast paths"
    );
}

#[test]
fn native_builtin_fast_paths_fall_back_for_javascript_coercions() {
    let mut context = Context::default();
    let result = context
        .eval(Source::from_bytes(
            r#"JSON.stringify(["AZ".charCodeAt("1"), "AZ".charCodeAt(9), Math.imul("-7", 6)])"#,
        ))
        .expect("coercing built-in calls must evaluate through the generic path");
    assert_eq!(result, js_string!("[90,null,-42]").into());
    assert_eq!(context.vm.native_builtin_fast_calls, 0);
}

#[test]
fn canonical_emotion_hash_uses_exact_guarded_call_summary() {
    let call = r#"JSON.stringify([emotionHash(""),emotionHash("a"),emotionHash("abcd"),emotionHash("abcdefg"),emotionHash("😀 style")])"#;

    let mut summarized = Context::default();
    summarized
        .eval(Source::from_bytes(EMOTION_HASH_SOURCE))
        .expect("define canonical hash");
    let summarized_result = summarized
        .eval(Source::from_bytes(call))
        .expect("summarize canonical hashes");
    assert_eq!(summarized.vm.emotion_hash_fast_calls, 5);

    let mut interpreted = Context::default();
    interpreted
        .eval(Source::from_bytes(EMOTION_HASH_SOURCE))
        .expect("define interpreted hash");
    interpreted
        .eval(Source::from_bytes(
            "const originalCharCodeAt = String.prototype.charCodeAt; \
             String.prototype.charCodeAt = function (index) { \
                 return originalCharCodeAt.call(this, index); \
             };",
        ))
        .expect("install equivalent wrapper");
    let interpreted_result = interpreted
        .eval(Source::from_bytes(call))
        .expect("interpret wrapped hashes");
    assert_eq!(interpreted.vm.emotion_hash_fast_calls, 0);
    assert_eq!(summarized_result, interpreted_result);
}

#[test]
fn emotion_hash_summary_honors_builtin_replacements_and_runtime_limits() {
    let mut patched = Context::default();
    patched
        .eval(Source::from_bytes(EMOTION_HASH_SOURCE))
        .expect("define canonical hash");
    let result = patched
        .eval(Source::from_bytes(
            "Number.prototype.toString = function () { return 'patched'; }; \
             emotionHash('abcd')",
        ))
        .expect("patched toString must execute");
    assert_eq!(result, js_string!("patched").into());
    assert_eq!(patched.vm.emotion_hash_fast_calls, 0);

    let mut limited = Context::default();
    limited
        .eval(Source::from_bytes(EMOTION_HASH_SOURCE))
        .expect("define canonical hash");
    limited.runtime_limits_mut().set_loop_iteration_limit(1);
    let error = limited
        .eval(Source::from_bytes("emotionHash('abcdefgh')"))
        .expect_err("the ordinary loop path must enforce its per-frame limit");
    assert_eq!(
        error.as_engine(),
        Some(&EngineError::RuntimeLimit(RuntimeLimitError::LoopIteration))
    );
    assert_eq!(limited.vm.emotion_hash_fast_calls, 0);
}

#[test]
fn emotion_hash_summary_preserves_exact_instruction_accounting() {
    for source in [EMOTION_HASH_SOURCE, EMOTION_HASH_BLOCK_SOURCE] {
        let mut summarized = Context::default();
        summarized
            .eval(Source::from_bytes(source))
            .expect("define summarized hash");
        summarized.set_instruction_budget(100_000);
        let summarized_result = summarized
            .eval(Source::from_bytes("emotionHash('abcdefghijk')"))
            .expect("summarized hash must fit");
        let summarized_remaining = summarized.instruction_budget_remaining();
        assert_eq!(summarized.vm.emotion_hash_fast_calls, 1);

        let mut interpreted = Context::default();
        interpreted
            .eval(Source::from_bytes(source))
            .expect("define interpreted hash");
        let global = interpreted.global_object();
        let function = global
            .get(js_string!("emotionHash"), &mut interpreted)
            .expect("read interpreted hash");
        let object = function
            .as_object()
            .expect("hash must be a function object");
        let ordinary = object
            .downcast_ref::<OrdinaryFunction>()
            .expect("hash must be ordinary");
        ordinary
            .codeblock()
            .pure_function_plan
            .set(None)
            .expect("proof cache must be cold before the first call");
        interpreted.set_instruction_budget(100_000);
        let interpreted_result = interpreted
            .eval(Source::from_bytes("emotionHash('abcdefghijk')"))
            .expect("interpreted hash must fit");

        assert_eq!(summarized_result, interpreted_result);
        assert_eq!(
            summarized_remaining,
            interpreted.instruction_budget_remaining()
        );
    }
}

#[test]
fn position() {
    let context = &mut Context::default();
    context
        .register_global_callable(
            js_string!("check_stack"),
            2,
            NativeFunction::from_copy_closure(|_, _, context| {
                let frame = context.stack_trace().collect::<Vec<&CallFrame>>();

                assert_eq!(frame.len(), 4);
                assert_eq!(
                    frame[0].position(),
                    CallFrameLocation {
                        function_name: js_string!("myOtherFunction"),
                        path: SourcePath::None,
                        position: Some(Position::new(2, 16))
                    }
                );
                assert_eq!(
                    frame[1].position(),
                    CallFrameLocation {
                        function_name: js_string!("<eval>"),
                        path: SourcePath::Eval,
                        position: Some(Position::new(1, 16))
                    }
                );
                assert_eq!(
                    frame[2].position(),
                    CallFrameLocation {
                        function_name: js_string!("myFunction"),
                        path: SourcePath::None,
                        position: Some(Position::new(5, 9))
                    }
                );
                assert_eq!(
                    frame[3].position(),
                    CallFrameLocation {
                        function_name: js_string!("<main>"),
                        path: SourcePath::None,
                        position: Some(Position::new(8, 11))
                    }
                );
                Ok(JsValue::undefined())
            }),
        )
        .expect("Could not register function");
    run_test_actions_with(
        [TestAction::run(indoc! {r#"
            const myOtherFunction = () => {
                check_stack();
            };
            function myFunction() {
                eval("myOtherFunction()");
            }

            myFunction();
        "#})],
        context,
    );
}

#[test]
fn try_catch_finally_from_init() {
    // the initialisation of the array here emits a PopOnReturnAdd op
    //
    // here we test that the stack is not popped more than intended due to multiple catches in the
    // same function, which could lead to VM stack corruption
    run_test_actions([TestAction::assert_opaque_error(
        indoc! {r#"
            try {
                [(() => {throw "h";})()];
            } catch (x) {
                throw "h";
            } finally {
            }
        "#},
        js_str!("h"),
    )]);
}

#[test]
fn multiple_catches() {
    // see explanation on `try_catch_finally_from_init`
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
                try {
                    [(() => {throw "h";})()];
                } catch (x) {
                    throw "h";
                }
            } catch (y) {
            }
        "#},
        JsValue::undefined(),
    )]);
}

#[test]
fn use_last_expr_try_block() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
                19;
                7.5;
                "Hello!";
            } catch (y) {
                14;
                "Bye!"
            }
        "#},
        js_str!("Hello!"),
    )]);
}

#[test]
fn use_last_expr_catch_block() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
                throw Error("generic error");
                19;
                7.5;
            } catch (y) {
                14;
                "Hello!";
            }
        "#},
        js_str!("Hello!"),
    )]);
}

#[test]
fn no_use_last_expr_finally_block() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
            } catch (y) {
            } finally {
                "Unused";
            }
        "#},
        JsValue::undefined(),
    )]);
}

#[test]
fn finally_block_binding_env() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let buf = "Hey hey";
            try {
            } catch (y) {
            } finally {
                let x = " people";
                buf += x;
            }
            buf
        "#},
        js_str!("Hey hey people"),
    )]);
}

#[test]
fn run_super_method_in_object() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let proto = {
                m() { return "super"; }
            };
            let obj = {
                v() { return super.m(); }
            };
            Object.setPrototypeOf(obj, proto);
            obj.v();
        "#},
        js_str!("super"),
    )]);
}

#[test]
fn get_reference_by_super() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            var fromA, fromB;
            var A = { fromA: 'a', fromB: 'a' };
            var B = { fromB: 'b' };
            Object.setPrototypeOf(B, A);
            var obj = {
                fromA: 'c',
                fromB: 'c',
                method() {
                    fromA = (() => { return super.fromA; })();
                    fromB = (() => { return super.fromB; })();
                }
            };
            Object.setPrototypeOf(obj, B);
            obj.method();
            fromA + fromB
        "#},
        js_str!("ab"),
    )]);
}

#[test]
fn super_call_constructor_null() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            class A extends Object {
                constructor() {
                    Object.setPrototypeOf(A, null);
                    super(A);
                }
            }
            new A();
        "#},
        JsNativeErrorKind::Type,
        "super constructor object must be constructor",
    )]);
}

#[test]
fn super_call_get_constructor_before_arguments_execution() {
    run_test_actions([TestAction::assert(indoc! {r#"
        class A extends Object {
            constructor() {
                super(Object.setPrototypeOf(A, null));
            }
        }
        new A() instanceof A;
    "#})]);
}

#[test]
fn order_of_execution_in_assignment() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                let i = 0;
                let array = [[]];

                array[i++][i++] = i++;
            "#}),
        TestAction::assert_eq("i", 3),
        TestAction::assert_eq("array.length", 1),
        TestAction::assert_eq("array[0].length", 2),
    ]);
}

#[test]
fn order_of_execution_in_assignment_with_comma_expressions() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let result = "";
            function f(i) {
                result += i;
            }
            let a = [[]];
            (f(1), a)[(f(2), 0)][(f(3), 0)] = (f(4), 123);
            result
        "#},
        js_str!("1234"),
    )]);
}

#[test]
fn loop_runtime_limit() {
    run_test_actions([
        TestAction::assert_eq(
            indoc! {r#"
                for (let i = 0; i < 20; ++i) { }
            "#},
            JsValue::undefined(),
        ),
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_loop_iteration_limit(10);
        }),
        TestAction::assert_runtime_limit_error(
            indoc! {r#"
                for (let i = 0; i < 20; ++i) { }
            "#},
            RuntimeLimitError::LoopIteration,
        ),
        TestAction::assert_eq(
            indoc! {r#"
                for (let i = 0; i < 10; ++i) { }
            "#},
            JsValue::undefined(),
        ),
        TestAction::assert_runtime_limit_error(
            indoc! {r#"
                for (let i = 0; i < 11; ++i) { }
            "#},
            RuntimeLimitError::LoopIteration,
        ),
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_loop_iteration_limit(0);
        }),
        TestAction::assert_eq(
            indoc! {r#"
                for (let i = 0; i < 0; ++i) { }
            "#},
            JsValue::undefined(),
        ),
        TestAction::assert_runtime_limit_error(
            indoc! {r#"
                for (let i = 0; i < 1; ++i) { }
            "#},
            RuntimeLimitError::LoopIteration,
        ),
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_loop_iteration_limit(10);
        }),
        TestAction::assert_runtime_limit_error(
            indoc! {r#"
                while (1) { }
            "#},
            RuntimeLimitError::LoopIteration,
        ),
    ]);
}

#[test]
fn native_loop_iteration_batches_enforce_the_exact_remaining_limit() {
    let mut context = Context::default();
    context.runtime_limits_mut().set_loop_iteration_limit(3);

    context
        .check_loop_iterations(3)
        .expect("checking an exact batch must not consume it");
    context
        .consume_loop_iterations(2)
        .expect("a batch within the remaining limit must succeed");
    context
        .consume_loop_iterations(1)
        .expect("a batch ending exactly at the limit must succeed");

    let error = context
        .consume_loop_iterations(1)
        .expect_err("the first iteration beyond the limit must fail");
    assert_eq!(
        error.as_engine(),
        Some(&EngineError::RuntimeLimit(RuntimeLimitError::LoopIteration))
    );
}

#[test]
fn instruction_budget_stops_straight_line_execution() {
    let mut context = Context::builder()
        .instruction_budget(8)
        .build()
        .expect("context creation must succeed");

    let source = Source::from_bytes(indoc! {r#"
        let total = 0;
        total += 1;
        total += 2;
        total += 3;
        total += 4;
        total += 5;
        total += 6;
        total += 7;
        total += 8;
        total;
    "#})
    .with_path(std::path::Path::new("instruction-budget.js"));
    let error = context
        .eval(source)
        .expect_err("straight-line bytecode must consume the finite budget");

    assert_eq!(error.as_engine(), Some(&EngineError::NoInstructionsRemain));
    assert!(
        error
            .to_string()
            .contains("at <main> (instruction-budget.js:"),
        "instruction termination must retain its source frame: {error}"
    );
    assert_eq!(context.instruction_budget_remaining(), Some(0));
}

#[test]
fn instruction_budget_exhaustion_is_uncatchable() {
    let mut context = Context::builder()
        .instruction_budget(0)
        .build()
        .expect("context creation must succeed");

    let error = context
        .eval(Source::from_bytes(
            "try { globalThis.caught = false; } catch { globalThis.caught = true; }",
        ))
        .expect_err("engine errors must escape ECMAScript exception handlers");

    assert_eq!(error.as_engine(), Some(&EngineError::NoInstructionsRemain));
}

#[test]
fn instruction_budget_is_persistent_resettable_and_optional() {
    let mut context = Context::default();
    assert_eq!(context.instruction_budget_remaining(), None);

    context.set_instruction_budget(1_000);
    context
        .eval(Source::from_bytes("function nested() { return 40 + 2; }"))
        .expect("the definition must fit in the budget");
    let after_definition = context
        .instruction_budget_remaining()
        .expect("the budget must remain enabled");

    assert_eq!(
        context
            .eval(Source::from_bytes("nested()"))
            .expect("the nested call must fit in the shared budget"),
        JsValue::new(42)
    );
    assert!(
        context.instruction_budget_remaining().unwrap() < after_definition,
        "nested execution must consume the existing context-wide budget"
    );

    context.set_instruction_budget(0);
    let error = context
        .eval(Source::from_bytes("nested()"))
        .expect_err("resetting to zero must stop the next task");
    assert_eq!(error.as_engine(), Some(&EngineError::NoInstructionsRemain));

    context.clear_instruction_budget();
    assert_eq!(context.instruction_budget_remaining(), None);
    assert_eq!(
        context
            .eval(Source::from_bytes("nested()"))
            .expect("clearing the budget must restore unlimited execution"),
        JsValue::new(42)
    );
}

#[test]
fn instruction_budget_exhaustion_unwinds_nested_frames() {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(
            "function burn() { let total = 0; total += 1; total += 2; total += 3; \
             total += 4; total += 5; total += 6; total += 7; total += 8; } \
             function outer() { return burn(); }",
        ))
        .expect("function definitions must succeed without a budget");
    let baseline_frame_depth = context.vm.frames.len();

    context.set_instruction_budget(12);
    let error = context
        .eval(Source::from_bytes("outer()"))
        .expect_err("the nested call must exhaust its budget");
    assert_eq!(error.as_engine(), Some(&EngineError::NoInstructionsRemain));
    assert_eq!(
        context.vm.frames.len(),
        baseline_frame_depth,
        "an uncatchable budget termination must unwind every nested frame"
    );

    context.set_instruction_budget(1_000);
    assert_eq!(
        context
            .eval(Source::from_bytes("40 + 2"))
            .expect("the context must remain usable after budget termination"),
        JsValue::new(42)
    );
}

#[test]
fn recursion_runtime_limit() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            function factorial(n) {
                if (n == 0) {
                    return 1;
                }

                return n * factorial(n - 1);
            }
        "#}),
        TestAction::assert_eq("factorial(8)", JsValue::new(40_320)),
        TestAction::assert_eq("factorial(11)", JsValue::new(39_916_800)),
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_recursion_limit(10);
        }),
        TestAction::assert_native_error(
            "factorial(11)",
            JsNativeErrorKind::Range,
            "Maximum call stack size exceeded",
        ),
        TestAction::assert_eq("factorial(8)", JsValue::new(40_320)),
        TestAction::assert_native_error(
            indoc! {r#"
                function x() {
                    x()
                }

                x()
            "#},
            JsNativeErrorKind::Range,
            "Maximum call stack size exceeded",
        ),
    ]);
}

#[test]
fn arguments_object_constructor_valid_index() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let args;
            function F(a = 1) {
                args = arguments;
            }
            new F();
            typeof args
        "#},
        js_str!("object"),
    )]);
}

#[test]
fn empty_return_values() {
    run_test_actions([
        TestAction::run(indoc! {r#"do {{}} while (false);"#}),
        TestAction::run(indoc! {r#"do try {{}} catch {} while (false);"#}),
        TestAction::run(indoc! {r#"do {} while (false);"#}),
        TestAction::run(indoc! {r#"do try {{}{}} catch {} while (false);"#}),
        TestAction::run(indoc! {r#"do {{}{}} while (false);"#}),
        TestAction::run(indoc! {r#"do {;{}} while (false);"#}),
        TestAction::run(indoc! {r#"do {e: {}} while (false);"#}),
        TestAction::run(indoc! {r#"do {e: ;} while (false);"#}),
        TestAction::run(indoc! {r#"do { break } while (false);"#}),
        TestAction::run(indoc! {r#"while (true) a: break"#}),
        TestAction::run(indoc! {r#"while (true) a: {"a"; break};"#}),
        TestAction::run(indoc! {r#"do {"a";{}} while (false);"#}),
        TestAction::run(indoc! {r#"
            switch (false) {
                default: {}
            }
        "#}),
        TestAction::run(indoc! {r#"
            switch (false) {
                default: {}{}
            }
        "#}),
        TestAction::run(indoc! {r#"
            switch (false) {
                default: ;{}{}
            }
        "#}),
    ]);
}

#[test]
fn truncate_environments_on_non_caught_native_error() {
    let source = "with (new Proxy({}, {has: p => false})) {a}";
    run_test_actions([
        TestAction::assert_native_error(source, JsNativeErrorKind::Reference, "a is not defined"),
        TestAction::assert_native_error(source, JsNativeErrorKind::Reference, "a is not defined"),
    ]);
}

#[test]
fn super_construction_with_parameter_expression() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            class Person {
                constructor(name) {
                    this.name = name;
                }
            }

            class Student extends Person {
                constructor(name = 'unknown') {
                    super(name);
                }
            }
        "#}),
        TestAction::assert_eq("new Student().name", js_str!("unknown")),
        TestAction::assert_eq("new Student('Jack').name", js_str!("Jack")),
    ]);
}

#[test]
fn cross_context_function_call() {
    let context1 = &mut Context::default();
    let result = context1.eval(Source::from_bytes(indoc! {r"
        var global = 100;

        (function x() {
            return global;
        })
    "}));

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(result.is_callable());

    let context2 = &mut Context::default();

    context2
        .register_global_property(js_string!("func"), result, Attribute::all())
        .unwrap();

    let result = context2.eval(Source::from_bytes("func()"));

    assert_eq!(result, Ok(JsValue::new(100)));
}

// See: https://github.com/boa-dev/boa/issues/1848
#[test]
fn long_object_chain_gc_trace_stack_overflow() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            let old = {};
            for (let i = 0; i < 100000; i++) {
                old = { old };
            }
        "#}),
        TestAction::inspect_context(|_| boa_gc::force_collect()),
    ]);
}

// See: https://github.com/boa-dev/boa/issues/4515
//
// The recursion limit must terminate the thenable `then`-getter recursion.
// Since call-depth overflow is a catchable `RangeError`, the async-generator
// machinery converts it into a promise rejection (matching real engines)
// instead of bubbling an engine error out of `evaluate`, so the assertion is
// that evaluation completes with a promise value rather than hanging or
// overflowing the native stack.
#[test]
fn recursion_in_async_gen_terminates_into_promise_rejection() {
    run_test_actions([
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_recursion_limit(128);
        }),
        TestAction::assert_with_op(
            indoc! {r#"
                async function* f() {}
                f().return({
                  get then() {
                    this.then;
                  },
                });
            "#},
            |value, _context| value.is_object(),
        ),
    ]);
}

#[test]
fn recursion_in_setter_throws_catchable_range_error() {
    run_test_actions([
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_recursion_limit(128);
        }),
        TestAction::assert_native_error(
            indoc! {r#"
                const obj = {
                  set x(value) {
                    this.x = value;
                  },
                };
                obj.x = 1;
            "#},
            JsNativeErrorKind::Range,
            "Maximum call stack size exceeded",
        ),
    ]);
}

/// Builds a context with an explicit recursion budget and host-frame weighting.
fn limited_context(recursion_limit: usize, host_frame_cost: usize) -> Context {
    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_recursion_limit(recursion_limit);
    context
        .runtime_limits_mut()
        .set_host_frame_cost(host_frame_cost);
    context
}

/// Plain JS recursion must be charged 1:1 no matter how expensive host
/// re-entries are declared to be: the weighting exists to price the *native*
/// stack, and a plain call never nests a native frame chain.
#[test]
fn host_frame_cost_leaves_plain_js_recursion_unweighted() {
    for cost in [1_usize, 16] {
        let mut context = limited_context(200, cost);
        run_test_actions_with(
            [TestAction::assert_eq(
                indoc! {r#"
                    let depth = 0;
                    function go() { depth++; go(); }
                    try { go() } catch (e) {}
                    depth;
                "#},
                JsValue::from(199),
            )],
            &mut context,
        );
    }
}

/// A setter that assigns through itself recurses via a *host* re-entry, so
/// raising `host_frame_cost` must make it trip proportionally sooner while the
/// budget itself stays put.
#[test]
fn host_frame_cost_charges_host_reentry_proportionally() {
    // Each level costs the callee's own frame plus `host_frame_cost`, and the
    // level that exhausts the budget still runs its body before recursing.
    const BUDGET: usize = 200;
    for cost in [1_usize, 4, 16] {
        let expected = BUDGET.div_ceil(cost + 1);
        let mut context = limited_context(BUDGET, cost);
        run_test_actions_with(
            [TestAction::assert_eq(
                indoc! {r#"
                    let depth = 0;
                    const obj = { set x(value) { depth++; this.x = value; } };
                    try { obj.x = 1 } catch (e) {}
                    depth;
                "#},
                JsValue::from(i32::try_from(expected).unwrap()),
            )],
            &mut context,
        );
    }
}
