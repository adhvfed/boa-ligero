use indoc::indoc;

use std::rc::Rc;

use crate::{
    Context, JsString, Source, TestAction, context::HostHooks, js_string, run_test_actions,
};

use super::{UtcOffset, parse_offset_time_zone_identifier};

// Intl.DateTimeFormat tests

#[cfg(feature = "intl_bundled")]
#[test]
fn dtf_basic() {
    run_test_actions([
        TestAction::run(indoc! {"
            // Setup date
            const date = new Date(Date.UTC(2020, 11, 20, 3, 23, 16, 738));

            let formatter = new Intl.DateTimeFormat('en-US');
            let result = formatter.format(date);
        "}),
        TestAction::assert_eq("result === '12/20/20'", true),
    ]);
    run_test_actions([
        TestAction::run(indoc! {"
            // Setup date
            const date = new Date(Date.UTC(2020, 11, 20, 3, 23, 16, 738));

            let formatter = new Intl.DateTimeFormat('en-US', { dateStyle: 'full' });
            let result = formatter.format(date);
        "}),
        TestAction::assert_eq("result === 'Sunday, December 20, 2020'", true),
    ]);
    run_test_actions([
        TestAction::run(indoc! {"
            // Setup date
            const date = new Date(Date.UTC(2020, 11, 20, 3, 23, 16, 738));

            let formatter = new Intl.DateTimeFormat('en-GB');
            let result = formatter.format(date);
        "}),
        TestAction::assert_eq("result === '20/12/2020'", true),
    ]);
    run_test_actions([
        TestAction::run(indoc! {"
            // Setup date
            const date = new Date(Date.UTC(2020, 11, 20, 3, 23, 16, 738));

            let formatter = new Intl.DateTimeFormat('en-GB', {
                dateStyle: 'full',
                timeStyle: 'long',
            });
            let result = formatter.format(date);
        "}),
        TestAction::assert_eq("result === 'Sunday, 20 December 2020 at 03:23:16'", true),
    ]);
    run_test_actions([
        TestAction::run(indoc! {"
            // Setup date
            const date = new Date(Date.UTC(2020, 11, 20, 3, 23, 16, 738));

            let formatter = new Intl.DateTimeFormat('en-GB', {
                dateStyle: 'full',
                timeStyle: 'long',
                timeZone: 'Australia/Sydney',
            });
            let result = formatter.format(date);
        "}),
        TestAction::assert_eq("result === 'Sunday, 20 December 2020 at 14:23:16'", true),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn dtf_uses_the_host_timezone_when_time_zone_is_omitted() {
    struct HelsinkiHooks;

    impl HostHooks for HelsinkiHooks {
        fn local_timezone_identifier(&self) -> JsString {
            JsString::from("Europe/Helsinki")
        }
    }

    let mut context = Context::builder()
        .host_hooks(Rc::new(HelsinkiHooks))
        .build()
        .expect("context");
    let result = context
        .eval(Source::from_bytes(
            r#"
            const date = new Date('2020-12-20T03:23:16.738Z');
            const formatter = new Intl.DateTimeFormat('en-GB', {
                hour: '2-digit',
                minute: '2-digit',
                hour12: false,
            });
            `${formatter.resolvedOptions().timeZone}|${formatter.format(date)}`;
            "#,
        ))
        .expect("format in host time zone")
        .to_string(&mut context)
        .expect("string result")
        .to_std_string_escaped();

    assert_eq!(result, "Europe/Helsinki|05:23");
}

#[cfg(feature = "intl_bundled")]
#[test]
fn date_to_locale_string() {
    run_test_actions([
        TestAction::run(indoc! {"
            // Setup date
            const date = new Date(Date.UTC(2021, 3, 12, 6, 7));

            let result = date.toLocaleString('en-US', { dateStyle: 'short' });
        "}),
        TestAction::assert_eq("result === '4/12/21'", true),
    ]);
    run_test_actions([
        TestAction::run(indoc! {"
            // Setup date
            const date = new Date(Date.UTC(2021, 3, 12, 6, 7));

            let result = date.toLocaleString('en-US', { timeStyle: 'short' });
        "}),
        TestAction::assert_eq("result === '6:07\u{202f}AM'", true),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn dtf_ctor_observable_behavior() {
    run_test_actions([
        TestAction::run(indoc! {"
            const expected = [];

            const proxyConstructor = new Proxy(Intl.DateTimeFormat, {
              get(target, prop) {
                if (prop === 'prototype') {
                  expected.push('prototype-access');
                }
                return target[prop];
              }
            });

            try {
              new proxyConstructor('en', { timeZone: 'Invalid/Zone' });
            } catch (e) {
              expected.push('error-thrown');
            }
        "}),
        TestAction::assert_eq("expected.length === 2", true),
        TestAction::assert_eq("expected[0] === 'prototype-access'", true),
        TestAction::assert_eq("expected[1] === 'error-thrown'", true),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn resolved_options_preserve_components_and_object_identity() {
    run_test_actions([
        TestAction::run(indoc! {"
            const formatter = new Intl.DateTimeFormat('en-US', {
                year: 'numeric',
                month: 'long',
                day: '2-digit',
                hour: 'numeric',
                hourCycle: 'h24',
            });
            const first = formatter.resolvedOptions();
            const second = formatter.resolvedOptions();
        "}),
        TestAction::assert_eq("first !== second", true),
        TestAction::assert_eq("first.year === 'numeric'", true),
        TestAction::assert_eq("first.month === 'long'", true),
        TestAction::assert_eq("first.day === '2-digit'", true),
        TestAction::assert_eq("first.hour === 'numeric'", true),
        TestAction::assert_eq("first.hourCycle === 'h24'", true),
        TestAction::assert_eq("first.hour12 === false", true),
    ]);
}

#[test]
fn ecma402_offset_time_zone_grammar() {
    for (identifier, seconds) in [
        ("+00", 0),
        ("-00:00", 0),
        ("+23", 23 * 60 * 60),
        ("-2359", -(23 * 60 * 60 + 59 * 60)),
        ("+01:03", 63 * 60),
    ] {
        assert_eq!(
            parse_offset_time_zone_identifier(identifier).map(UtcOffset::to_seconds),
            Some(seconds),
            "{identifier}"
        );
    }

    for identifier in [
        "+3",
        "+24",
        "+23:0",
        "+2400",
        "+15:60",
        "+15:59:00",
        "−05",
        "Z",
    ] {
        assert!(
            parse_offset_time_zone_identifier(identifier).is_none(),
            "{identifier}"
        );
    }
}

#[test]
fn format_to_parts_splits_the_formatted_date_by_field() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            const date = new Date(Date.UTC(2026, 7, 24, 15, 4, 5));
            const long = new Intl.DateTimeFormat("en-US", {
              year: "numeric", month: "long", day: "numeric",
              hour: "numeric", minute: "2-digit", timeZone: "UTC",
            });
            const short = new Intl.DateTimeFormat("en-US", { timeZone: "UTC" });
            const types = (f) => f.formatToParts(date).map(p => p.type).join(",");
            const joined = (f) => f.formatToParts(date).map(p => p.value).join("");
        "#}),
        // Numeric fields arrive through a decimal formatter that adds its own
        // annotations; reporting the innermost one would call a year a literal.
        TestAction::assert_eq(
            "types(long)",
            js_string!(
                "month,literal,day,literal,year,literal,hour,literal,minute,literal,dayPeriod"
            ),
        ),
        TestAction::assert_eq("types(short)", js_string!("month,literal,day,literal,year")),
        // The parts must reconstruct exactly what `format` produces.
        TestAction::assert("joined(long) === long.format(date)"),
        TestAction::assert("joined(short) === short.format(date)"),
        // Field order follows the locale, not the option order.
        TestAction::assert_eq(
            "new Intl.DateTimeFormat('de-DE', { dateStyle: 'long', timeZone: 'UTC' })\
               .formatToParts(date).map(p => p.type).join(',')",
            js_string!("day,literal,month,literal,year"),
        ),
        // Every entry is a plain `{ type, value }` pair of strings.
        TestAction::assert(
            "long.formatToParts(date).every(p => \
               typeof p.type === 'string' && typeof p.value === 'string' && \
               Object.keys(p).length === 2)",
        ),
        TestAction::assert_eq("Intl.DateTimeFormat.prototype.formatToParts.length", 1),
    ]);
}

#[test]
fn format_to_parts_rejects_a_non_finite_date() {
    run_test_actions([TestAction::assert_native_error(
        "new Intl.DateTimeFormat('en-US').formatToParts(NaN)",
        crate::JsNativeErrorKind::Range,
        "formatted date cannot be NaN",
    )]);
}
