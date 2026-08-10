//! Supplemental ICU4X data schemas used by Boa's ECMA-402 implementation.
//!
//! ICU4X does not yet expose every piece of CLDR number-format data required by
//! ECMA-402. Keeping those schemas in a small, engine-independent crate lets
//! both Boa's bundled provider and custom providers supply the missing data.

#![no_std]
#![allow(single_use_lifetimes, unused_crate_dependencies)]

extern crate alloc;

use alloc::borrow::Cow;

use icu_provider::prelude::*;
use yoke::Yokeable;
use zerofrom::ZeroFrom;

icu_provider::data_marker!(
    /// Locale- and numbering-system-specific symbols used by number formatting.
    BoaNumberSpecialSymbolsV2,
    "boa/number/special/symbols/v2",
    NumberSpecialSymbols<'static>,
    #[cfg(feature = "datagen")]
    attributes_domain = "numbering_system"
);

/// Locale- and numbering-system-specific symbols used by number formatting.
#[derive(Debug, Clone, PartialEq, Yokeable, ZeroFrom)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "datagen", derive(serde::Serialize, databake::Bake))]
#[cfg_attr(feature = "datagen", databake(path = boa_icu_data))]
pub struct NumberSpecialSymbols<'data> {
    /// The localized representation of positive infinity.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub infinity: Cow<'data, str>,

    /// The localized representation of `NaN`.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub nan: Cow<'data, str>,

    /// The localized separator between a significand and its exponent.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub exponential: Cow<'data, str>,

    /// The localized sign used to mark an approximate number.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub approximately_sign: Cow<'data, str>,

    /// The locale-specific infix placed between distinct range endpoints.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub range_separator: Cow<'data, str>,
}

icu_provider::data_struct!(
    NumberSpecialSymbols<'_>,
    #[cfg(feature = "datagen")]
);

icu_provider::data_marker!(
    /// Locale-specific negative currency patterns for accounting notation.
    BoaCurrencyAccountingPatternsV1,
    "boa/currency/accounting/patterns/v1",
    CurrencyAccountingPatterns<'static>,
);

/// Locale-specific negative currency patterns for accounting notation.
///
/// The patterns retain typed `number`, `currency`, and optional `minusSign`
/// placeholders so formatting parts remain structured.
#[derive(Debug, Clone, PartialEq, Yokeable, ZeroFrom)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "datagen", derive(serde::Serialize, databake::Bake))]
#[cfg_attr(feature = "datagen", databake(path = boa_icu_data))]
pub struct CurrencyAccountingPatterns<'data> {
    /// The explicit negative accounting pattern for symbolic currency displays.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub standard: Option<CurrencyAccountingPattern<'data>>,

    /// The explicit negative accounting pattern used when a currency display
    /// adjacent to the number begins or ends with a letter.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub alpha_next_to_number: Option<CurrencyAccountingPattern<'data>>,

    /// The localized minus sign substituted for the `minusSign` placeholder.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub minus_sign: Cow<'data, str>,
}

/// A negative accounting pattern split around its typed placeholders.
#[derive(Debug, Clone, PartialEq, Yokeable, ZeroFrom)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "datagen", derive(serde::Serialize, databake::Bake))]
#[cfg_attr(feature = "datagen", databake(path = boa_icu_data))]
pub struct CurrencyAccountingPattern<'data> {
    /// Text before the first placeholder.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub prefix: Cow<'data, str>,

    /// Text between the first and second placeholders.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub infix: Cow<'data, str>,

    /// Text between the second and optional third placeholders.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub second_infix: Cow<'data, str>,

    /// Text after the last placeholder.
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub suffix: Cow<'data, str>,

    /// The placeholder order in this pattern.
    pub order: CurrencyAccountingPlaceholderOrder,
}

/// Placeholder orders supported by CLDR negative accounting patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Yokeable, ZeroFrom)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "datagen", derive(serde::Serialize, databake::Bake))]
#[cfg_attr(feature = "datagen", databake(path = boa_icu_data))]
#[non_exhaustive]
pub enum CurrencyAccountingPlaceholderOrder {
    /// Number, then currency.
    NumberCurrency,
    /// Currency, then number.
    CurrencyNumber,
    /// Minus sign, currency, then number.
    MinusCurrencyNumber,
    /// Minus sign, number, then currency.
    MinusNumberCurrency,
    /// Currency, minus sign, then number.
    CurrencyMinusNumber,
    /// Currency, number, then minus sign.
    CurrencyNumberMinus,
    /// Number, minus sign, then currency.
    NumberMinusCurrency,
    /// Number, currency, then minus sign.
    NumberCurrencyMinus,
}

icu_provider::data_struct!(
    CurrencyAccountingPatterns<'_>,
    #[cfg(feature = "datagen")]
);

/// All supplemental markers currently defined by this crate.
pub const MARKERS: &[DataMarkerInfo] = &[
    BoaNumberSpecialSymbolsV2::INFO,
    BoaCurrencyAccountingPatternsV1::INFO,
];
