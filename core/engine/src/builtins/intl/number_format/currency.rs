//! Structured currency output built from ICU4X locale patterns.

use core::fmt::Write as _;

use fixed_decimal::Sign;
use icu_decimal::DecimalFormatter;
use icu_experimental::dimension::provider::currency::essentials::{
    CurrencyEssentials, PatternSelection, PlaceholderValue,
};
use icu_pattern::{DoublePlaceholderKey, DoublePlaceholderPattern, PatternItem};
use tinystr::TinyAsciiStr;
use writeable::{LengthHint, Part, PartsWrite, Writeable};

use super::{Currency, CurrencyDisplay};

const CURRENCY: Part = Part {
    category: "number",
    value: "currency",
};

struct CurrencyBody<'a, W> {
    number: &'a W,
    pattern: &'a DoublePlaceholderPattern,
    symbol: &'a str,
}

impl<W: Writeable> Writeable for CurrencyBody<'_, W> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        for item in self.pattern.iter() {
            match item {
                PatternItem::Literal(literal) => sink.write_str(literal)?,
                PatternItem::Placeholder(DoublePlaceholderKey::Place0) => {
                    self.number.write_to(sink)?;
                }
                PatternItem::Placeholder(DoublePlaceholderKey::Place1) => {
                    sink.write_str(self.symbol)?;
                }
            }
        }
        Ok(())
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        for item in self.pattern.iter() {
            match item {
                PatternItem::Literal(literal) => sink.write_str(literal)?,
                PatternItem::Placeholder(DoublePlaceholderKey::Place0) => {
                    self.number.write_to_parts(sink)?;
                }
                PatternItem::Placeholder(DoublePlaceholderKey::Place1) => {
                    sink.with_part(CURRENCY, |sink| sink.write_str(self.symbol))?;
                }
            }
        }
        Ok(())
    }

    fn writeable_length_hint(&self) -> LengthHint {
        let mut hint = self.number.writeable_length_hint() + LengthHint::exact(self.symbol.len());
        for item in self.pattern.iter() {
            if let PatternItem::Literal(literal) = item {
                hint += LengthHint::exact(literal.len());
            }
        }
        hint
    }
}

pub(super) struct FormattedCurrency<'a, W> {
    number: W,
    essentials: &'a CurrencyEssentials<'a>,
    sign_formatter: &'a DecimalFormatter,
    sign: Sign,
    currency: TinyAsciiStr<3>,
    display: CurrencyDisplay,
}

impl<'a, W> FormattedCurrency<'a, W> {
    pub(super) fn new(
        number: W,
        essentials: &'a CurrencyEssentials<'a>,
        sign_formatter: &'a DecimalFormatter,
        sign: Sign,
        currency: Currency,
        display: CurrencyDisplay,
    ) -> Self {
        Self {
            number,
            essentials,
            sign_formatter,
            sign,
            currency: currency.as_tinystr(),
            display,
        }
    }

    fn symbol_and_pattern(&self) -> (&str, &DoublePlaceholderPattern) {
        let config = self
            .essentials
            .pattern_config_map
            .get_copied(&self.currency.to_unvalidated())
            .unwrap_or(self.essentials.default_pattern_config);

        let (placeholder, selection) = match self.display {
            CurrencyDisplay::Symbol => (
                config.short_placeholder_value,
                config.short_pattern_selection,
            ),
            CurrencyDisplay::NarrowSymbol => (
                config.narrow_placeholder_value,
                config.narrow_pattern_selection,
            ),
            // ICU4X's essentials provider does not contain pluralized currency names.
            // The ISO code is the required fallback when localized display data is absent.
            CurrencyDisplay::Code | CurrencyDisplay::Name => (
                Some(PlaceholderValue::ISO),
                PatternSelection::StandardAlphaNextToNumber,
            ),
        };

        let symbol = match placeholder {
            Some(PlaceholderValue::Index(index)) => self
                .essentials
                .placeholders
                .get(index.into())
                .unwrap_or(self.currency.as_str()),
            Some(PlaceholderValue::ISO) | None => self.currency.as_str(),
        };

        let pattern = match selection {
            PatternSelection::Standard => &*self.essentials.standard_pattern,
            PatternSelection::StandardAlphaNextToNumber => {
                &*self.essentials.standard_alpha_next_to_number_pattern
            }
        };

        (symbol, pattern)
    }

    fn body(&self) -> CurrencyBody<'_, W> {
        let (symbol, pattern) = self.symbol_and_pattern();
        CurrencyBody {
            number: &self.number,
            pattern,
            symbol,
        }
    }
}

impl<W: Writeable> Writeable for FormattedCurrency<'_, W> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        self.sign_formatter
            .format_sign(self.sign, self.body())
            .write_to(sink)
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        self.sign_formatter
            .format_sign(self.sign, self.body())
            .write_to_parts(sink)
    }

    fn writeable_length_hint(&self) -> LengthHint {
        self.sign_formatter
            .format_sign(self.sign, self.body())
            .writeable_length_hint()
    }
}
