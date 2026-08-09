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
    /// Locale-specific spellings of non-finite numbers.
    BoaNumberSpecialSymbolsV1,
    "boa/number/special/symbols/v1",
    NumberSpecialSymbols<'static>,
);

/// Locale-specific spellings of non-finite numbers used by number formatting.
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
}

icu_provider::data_struct!(
    NumberSpecialSymbols<'_>,
    #[cfg(feature = "datagen")]
);

/// All supplemental markers currently defined by this crate.
pub const MARKERS: &[DataMarkerInfo] = &[BoaNumberSpecialSymbolsV1::INFO];
