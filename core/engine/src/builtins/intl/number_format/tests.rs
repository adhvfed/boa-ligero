use crate::{
    builtins::intl::number_format::{
        IntlMathematicalValue, RoundingIncrement, SpecialValue,
        js_string_to_intl_mathematical_value,
    },
    builtins::intl::parts::{FormattedPart, PartsCollector, UnmarkedStyle},
    js_string,
};
use fixed_decimal::Decimal;
use fixed_decimal::RoundingIncrement::*;
use writeable::Writeable;

#[cfg(feature = "intl_bundled")]
use crate::{TestAction, run_test_actions};

#[cfg(feature = "intl_bundled")]
#[test]
fn numbering_system_preferences_reach_the_formatter() {
    run_test_actions([TestAction::assert_eq(
        r#"
            const fromExtension = new Intl.NumberFormat("en-u-nu-arab").resolvedOptions();
            const formatter = new Intl.NumberFormat("en-u-nu-latn", {
                numberingSystem: "arab"
            });
            const fromOption = formatter.resolvedOptions();
            fromExtension.locale === "en-u-nu-arab"
                && fromExtension.numberingSystem === "arab"
                && fromOption.locale === "en"
                && fromOption.numberingSystem === "arab"
                && formatter.format(123) === "١٢٣";
        "#,
        true,
    )]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn scientific_notation_formats_values_and_parts() {
    run_test_actions([
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { notation: "engineering" }).format(0.000345)"#,
            js_string!("345E-6"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { notation: "scientific" }).format(0.000345)"#,
            js_string!("3.45E-4"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { notation: "scientific", maximumSignificantDigits: 3 }).format(999.9)"#,
            js_string!("1E3"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { notation: "scientific", minimumSignificantDigits: 2, maximumSignificantDigits: 3 }).format(999.9)"#,
            js_string!("1.0E3"),
        ),
        TestAction::assert_eq(
            r#"JSON.stringify(new Intl.NumberFormat("en-US", { notation: "scientific" }).formatToParts(0.0345))"#,
            js_string!(
                r#"[{"type":"integer","value":"3"},{"type":"decimal","value":"."},{"type":"fraction","value":"45"},{"type":"exponentSeparator","value":"E"},{"type":"exponentMinusSign","value":"-"},{"type":"exponentInteger","value":"2"}]"#
            ),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn significant_digit_padding_uses_rounded_magnitude() {
    run_test_actions([
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { minimumSignificantDigits: 2, maximumSignificantDigits: 3 }).format(9.999)"#,
            js_string!("10"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { minimumSignificantDigits: 2, maximumSignificantDigits: 3 }).format(0.9999)"#,
            js_string!("1.0"),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn currency_fraction_defaults_come_from_cldr() {
    run_test_actions([
        TestAction::assert_eq(
            r#"
                const currencyOptions = currency => new Intl.NumberFormat("en-US", {
                    style: "currency",
                    currency
                }).resolvedOptions();
                JSON.stringify([
                    currencyOptions("USD").minimumFractionDigits,
                    currencyOptions("JPY").minimumFractionDigits,
                    currencyOptions("KWD").minimumFractionDigits,
                    currencyOptions("CLF").minimumFractionDigits,
                    currencyOptions("XTS").minimumFractionDigits,
                ]);
            "#,
            js_string!("[2,0,3,4,2]"),
        ),
        TestAction::assert_eq(
            r#"
                const notationOptions = notation => new Intl.NumberFormat("en-US", {
                    style: "currency",
                    currency: "JPY",
                    notation
                }).resolvedOptions();
                JSON.stringify([
                    notationOptions("scientific").minimumFractionDigits,
                    notationOptions("scientific").maximumFractionDigits,
                    notationOptions("compact").minimumFractionDigits,
                    notationOptions("compact").maximumFractionDigits,
                ]);
            "#,
            js_string!("[0,3,0,0]"),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn currency_style_uses_localized_symbols_and_patterns() {
    run_test_actions([
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" }).format(-1234.5)"#,
            js_string!("-$1,234.50"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("de-DE", { style: "currency", currency: "USD" }).format(-1234.5)"#,
            js_string!("-1.234,50\u{a0}$"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("ko-KR", { style: "currency", currency: "USD" }).format(1234.5)"#,
            js_string!("US$1,234.50"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("ko-KR", { style: "currency", currency: "USD", currencyDisplay: "narrowSymbol" }).format(1234.5)"#,
            js_string!("$1,234.50"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", currencyDisplay: "code" }).format(1234.5)"#,
            js_string!("USD\u{a0}1,234.50"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "currency", currency: "ZZZ" }).format(1)"#,
            js_string!("ZZZ\u{a0}1.00"),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn currency_style_preserves_structured_parts() {
    run_test_actions([TestAction::assert_eq(
        r#"JSON.stringify(new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" }).formatToParts(-1234.5))"#,
        js_string!(
            r#"[{"type":"minusSign","value":"-"},{"type":"currency","value":"$"},{"type":"integer","value":"1"},{"type":"group","value":","},{"type":"integer","value":"234"},{"type":"decimal","value":"."},{"type":"fraction","value":"50"}]"#
        ),
    )]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn percent_style_scales_values_and_uses_locale_patterns() {
    run_test_actions([
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "percent" }).format(0.2)"#,
            js_string!("20%"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("tr", { style: "percent" }).format(0.2)"#,
            js_string!("%20"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("de-DE", { style: "percent" }).format(0.2)"#,
            js_string!("20\u{a0}%"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "percent", signDisplay: "always" }).format(0.2)"#,
            js_string!("+20%"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "percent", signDisplay: "negative" }).format(-0)"#,
            js_string!("0%"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "percent", notation: "scientific" }).format(0.0123)"#,
            js_string!("1E0%"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-u-nu-arab", { style: "percent" }).format(0.2)"#,
            js_string!("٢٠%"),
        ),
        TestAction::assert_eq(
            r#"
                const formatter = new Intl.NumberFormat("en-US", {
                    style: "percent",
                    signDisplay: "always"
                });
                JSON.stringify([
                    formatter.format(NaN),
                    formatter.format(Infinity),
                    formatter.format(-Infinity),
                ]);
            "#,
            js_string!(r#"["+NaN%","+∞%","-∞%"]"#),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn percent_style_preserves_structured_parts() {
    run_test_actions([
        TestAction::assert_eq(
            r#"JSON.stringify(new Intl.NumberFormat("en-US", { style: "percent" }).formatToParts(-123))"#,
            js_string!(
                r#"[{"type":"minusSign","value":"-"},{"type":"integer","value":"12"},{"type":"group","value":","},{"type":"integer","value":"300"},{"type":"percentSign","value":"%"}]"#
            ),
        ),
        TestAction::assert_eq(
            r#"JSON.stringify(new Intl.NumberFormat("de-DE", { style: "percent" }).formatToParts(0.2))"#,
            js_string!(
                r#"[{"type":"integer","value":"20"},{"type":"literal","value":" "},{"type":"percentSign","value":"%"}]"#
            ),
        ),
        TestAction::assert_eq(
            r#"JSON.stringify(new Intl.NumberFormat("en-US", { style: "percent", signDisplay: "always" }).formatToParts(NaN))"#,
            js_string!(
                r#"[{"type":"plusSign","value":"+"},{"type":"nan","value":"NaN"},{"type":"percentSign","value":"%"}]"#
            ),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn unit_style_uses_localized_patterns_and_plural_forms() {
    run_test_actions([
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "unit", unit: "meter", unitDisplay: "long" }).format(1)"#,
            js_string!("1 meter"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "unit", unit: "meter", unitDisplay: "long" }).format(2)"#,
            js_string!("2 meters"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "unit", unit: "kilometer-per-hour", unitDisplay: "long" }).format(-987)"#,
            js_string!("-987 kilometers per hour"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("de-DE", { style: "unit", unit: "kilometer-per-hour", unitDisplay: "long" }).format(987)"#,
            js_string!("987 Kilometer pro Stunde"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("ja-JP", { style: "unit", unit: "kilometer-per-hour", unitDisplay: "long" }).format(-987)"#,
            js_string!("時速 -987 キロメートル"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("ko-KR", { style: "unit", unit: "kilometer-per-hour", unitDisplay: "long" }).format(987)"#,
            js_string!("시속 987킬로미터"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("zh-TW", { style: "unit", unit: "kilometer-per-hour", unitDisplay: "long" }).format(987)"#,
            js_string!("每小時 987 公里"),
        ),
        TestAction::assert_eq(
            r#"new Intl.NumberFormat("en-US", { style: "unit", unit: "percent" }).format(12)"#,
            js_string!("12%"),
        ),
        TestAction::assert_eq(
            r#"
                const formatter = new Intl.NumberFormat("en-US", {
                    style: "unit",
                    unit: "meter",
                    unitDisplay: "long"
                });
                JSON.stringify([
                    formatter.format(NaN),
                    formatter.format(Infinity),
                    formatter.format(-Infinity),
                ]);
            "#,
            js_string!(r#"["NaN meters","∞ meters","-∞ meters"]"#),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn unit_style_preserves_structured_parts() {
    run_test_actions([
        TestAction::assert_eq(
            r#"JSON.stringify(new Intl.NumberFormat("en-US", { style: "unit", unit: "kilometer-per-hour", unitDisplay: "long" }).formatToParts(-987))"#,
            js_string!(
                r#"[{"type":"minusSign","value":"-"},{"type":"integer","value":"987"},{"type":"literal","value":" "},{"type":"unit","value":"kilometers per hour"}]"#
            ),
        ),
        TestAction::assert_eq(
            r#"JSON.stringify(new Intl.NumberFormat("en-US", { style: "unit", unit: "percent" }).formatToParts(-12))"#,
            js_string!(
                r#"[{"type":"minusSign","value":"-"},{"type":"integer","value":"12"},{"type":"unit","value":"%"}]"#
            ),
        ),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn compound_units_fall_back_to_locale_composition() {
    run_test_actions([TestAction::assert_eq(
        r#"new Intl.NumberFormat("en-US", { style: "unit", unit: "acre-per-bit", unitDisplay: "long" }).format(2)"#,
        js_string!("2 acres per bit"),
    )]);
}

#[test]
fn special_number_parts_are_tagged() {
    let mut parts = PartsCollector::new(UnmarkedStyle::Ignore);
    SpecialValue::Infinity.write_to_parts(&mut parts).unwrap();
    assert_eq!(
        parts.parts,
        [FormattedPart {
            kind: "infinity",
            value: "∞".to_owned(),
        }]
    );
}

#[test]
fn intl_mathematical_value_preserves_special_values() {
    assert!(matches!(
        IntlMathematicalValue::try_from_f64(f64::NAN),
        Ok(IntlMathematicalValue::NaN)
    ));
    assert!(matches!(
        IntlMathematicalValue::try_from_f64(f64::INFINITY),
        Ok(IntlMathematicalValue::Infinity { negative: false })
    ));
    assert!(matches!(
        IntlMathematicalValue::try_from_f64(f64::NEG_INFINITY),
        Ok(IntlMathematicalValue::Infinity { negative: true })
    ));

    assert!(matches!(
        js_string_to_intl_mathematical_value(&js_string!("not a number")),
        IntlMathematicalValue::NaN
    ));
    assert!(matches!(
        js_string_to_intl_mathematical_value(&js_string!("-Infinity")),
        IntlMathematicalValue::Infinity { negative: true }
    ));

    let IntlMathematicalValue::Finite(value) =
        js_string_to_intl_mathematical_value(&js_string!("0x2a"))
    else {
        panic!("a valid numeric string must produce a finite value");
    };
    assert_eq!(value, Decimal::from(42));
}

#[test]
fn u16_to_rounding_increment_sunny_day() {
    #[rustfmt::skip]
    let valid_cases: [(u16, RoundingIncrement); 15] = [
        // Singles
        (1, RoundingIncrement::from_parts(MultiplesOf1, 0).unwrap()),
        (2, RoundingIncrement::from_parts(MultiplesOf2, 0).unwrap()),
        (5, RoundingIncrement::from_parts(MultiplesOf5, 0).unwrap()),
        // Tens
        (10, RoundingIncrement::from_parts(MultiplesOf1, 1).unwrap()),
        (20, RoundingIncrement::from_parts(MultiplesOf2, 1).unwrap()),
        (25, RoundingIncrement::from_parts(MultiplesOf25, 0).unwrap()),
        (50, RoundingIncrement::from_parts(MultiplesOf5, 1).unwrap()),
        // Hundreds
        (100, RoundingIncrement::from_parts(MultiplesOf1, 2).unwrap()),
        (200, RoundingIncrement::from_parts(MultiplesOf2, 2).unwrap()),
        (250, RoundingIncrement::from_parts(MultiplesOf25, 1).unwrap()),
        (500, RoundingIncrement::from_parts(MultiplesOf5, 2).unwrap()),
        // Thousands
        (1000, RoundingIncrement::from_parts(MultiplesOf1, 3).unwrap()),
        (2000, RoundingIncrement::from_parts(MultiplesOf2, 3).unwrap()),
        (2500, RoundingIncrement::from_parts(MultiplesOf25, 2).unwrap()),
        (5000, RoundingIncrement::from_parts(MultiplesOf5, 3).unwrap()),
    ];

    for (num, increment) in valid_cases {
        assert_eq!(RoundingIncrement::from_u16(num), Some(increment));
        assert_eq!(increment.to_u16(), num);
    }
}

#[test]
fn u16_to_rounding_increment_rainy_day() {
    const INVALID_CASES: [u16; 9] = [0, 4, 6, 24, 10000, 65535, 7373, 140, 1500];

    for num in INVALID_CASES {
        assert!(RoundingIncrement::from_u16(num).is_none());
    }
}
