use crate::{
    builtins::intl::number_format::{
        IntlMathematicalValue, NumberPart, PartsCollector, RoundingIncrement, SpecialValue,
        js_string_to_intl_mathematical_value,
    },
    js_string,
};
use fixed_decimal::Decimal;
use fixed_decimal::RoundingIncrement::*;
use writeable::Writeable;

#[test]
fn number_parts_preserve_special_and_compact_boundaries() {
    let mut parts = PartsCollector::new(false);
    SpecialValue::Infinity.write_to_parts(&mut parts).unwrap();
    assert_eq!(
        parts.parts,
        [NumberPart {
            kind: "infinity",
            value: "∞".to_owned(),
        }]
    );

    let mut parts = PartsCollector::new(true);
    core::fmt::Write::write_str(&mut parts, "\u{a0}million").unwrap();
    assert_eq!(
        parts.parts,
        [
            NumberPart {
                kind: "literal",
                value: "\u{a0}".to_owned(),
            },
            NumberPart {
                kind: "compact",
                value: "million".to_owned(),
            },
        ]
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
