//! Structured currency output built from ICU4X locale patterns.

use core::fmt::Write as _;

use boa_icu_data::{
    CurrencyAccountingPattern, CurrencyAccountingPatterns, CurrencyAccountingPlaceholderOrder,
};
use fixed_decimal::Sign;
use icu_decimal::DecimalFormatter;
use icu_experimental::dimension::provider::currency::essentials::{
    CurrencyEssentials, PatternSelection, PlaceholderValue,
};
use icu_pattern::{DoublePlaceholderKey, DoublePlaceholderPattern, PatternItem};
use tinystr::TinyAsciiStr;
use writeable::{LengthHint, Part, PartsWrite, Writeable};

use super::{Currency, CurrencyDisplay, CurrencySign};

const CURRENCY: Part = Part {
    category: "number",
    value: "currency",
};

const MINUS_SIGN: Part = Part {
    category: "number",
    value: "minusSign",
};

struct CurrencyBody<'a, W> {
    number: &'a W,
    pattern: &'a DoublePlaceholderPattern,
    symbol: &'a str,
}

#[derive(Debug, Clone, Copy)]
enum AccountingPlaceholder {
    Number,
    Currency,
    MinusSign,
}

fn accounting_placeholders(
    order: CurrencyAccountingPlaceholderOrder,
) -> ([AccountingPlaceholder; 3], usize) {
    use AccountingPlaceholder::{Currency, MinusSign, Number};
    use CurrencyAccountingPlaceholderOrder::{
        CurrencyMinusNumber, CurrencyNumberMinus, MinusCurrencyNumber, MinusNumberCurrency,
        NumberCurrency, NumberCurrencyMinus, NumberMinusCurrency,
    };

    match order {
        NumberCurrency => ([Number, Currency, Number], 2),
        MinusCurrencyNumber => ([MinusSign, Currency, Number], 3),
        MinusNumberCurrency => ([MinusSign, Number, Currency], 3),
        CurrencyMinusNumber => ([Currency, MinusSign, Number], 3),
        CurrencyNumberMinus => ([Currency, Number, MinusSign], 3),
        NumberMinusCurrency => ([Number, MinusSign, Currency], 3),
        NumberCurrencyMinus => ([Number, Currency, MinusSign], 3),
        _ => unreachable!("unsupported accounting placeholder order"),
    }
}

struct AccountingCurrencyBody<'a, W> {
    number: &'a W,
    pattern: &'a CurrencyAccountingPattern<'a>,
    symbol: &'a str,
    minus_sign: &'a str,
}

impl<W: Writeable> AccountingCurrencyBody<'_, W> {
    fn write_value<S: core::fmt::Write + ?Sized>(
        &self,
        value: AccountingPlaceholder,
        sink: &mut S,
    ) -> core::fmt::Result {
        match value {
            AccountingPlaceholder::Number => self.number.write_to(sink),
            AccountingPlaceholder::Currency => sink.write_str(self.symbol),
            AccountingPlaceholder::MinusSign => sink.write_str(self.minus_sign),
        }
    }

    fn write_value_to_parts<S: PartsWrite + ?Sized>(
        &self,
        value: AccountingPlaceholder,
        sink: &mut S,
    ) -> core::fmt::Result {
        match value {
            AccountingPlaceholder::Number => self.number.write_to_parts(sink),
            AccountingPlaceholder::Currency => {
                sink.with_part(CURRENCY, |sink| sink.write_str(self.symbol))
            }
            AccountingPlaceholder::MinusSign => {
                sink.with_part(MINUS_SIGN, |sink| sink.write_str(self.minus_sign))
            }
        }
    }
}

impl<W: Writeable> Writeable for AccountingCurrencyBody<'_, W> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        let (values, len) = accounting_placeholders(self.pattern.order);
        sink.write_str(&self.pattern.prefix)?;
        self.write_value(values[0], sink)?;
        sink.write_str(&self.pattern.infix)?;
        self.write_value(values[1], sink)?;
        if len == 3 {
            sink.write_str(&self.pattern.second_infix)?;
            self.write_value(values[2], sink)?;
        }
        sink.write_str(&self.pattern.suffix)
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        let (values, len) = accounting_placeholders(self.pattern.order);
        sink.write_str(&self.pattern.prefix)?;
        self.write_value_to_parts(values[0], sink)?;
        sink.write_str(&self.pattern.infix)?;
        self.write_value_to_parts(values[1], sink)?;
        if len == 3 {
            sink.write_str(&self.pattern.second_infix)?;
            self.write_value_to_parts(values[2], sink)?;
        }
        sink.write_str(&self.pattern.suffix)
    }

    fn writeable_length_hint(&self) -> LengthHint {
        let (_, len) = accounting_placeholders(self.pattern.order);
        let mut hint = self.number.writeable_length_hint()
            + LengthHint::exact(self.symbol.len())
            + LengthHint::exact(self.pattern.prefix.len())
            + LengthHint::exact(self.pattern.infix.len())
            + LengthHint::exact(self.pattern.suffix.len());
        if len == 3 {
            hint += LengthHint::exact(self.minus_sign.len() + self.pattern.second_infix.len());
        }
        hint
    }
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
    currency_sign: CurrencySign,
    accounting_patterns: Option<&'a CurrencyAccountingPatterns<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CurrencyFormatterOptions {
    pub(super) currency: Currency,
    pub(super) display: CurrencyDisplay,
    pub(super) sign: CurrencySign,
}

impl<'a, W> FormattedCurrency<'a, W> {
    pub(super) fn new(
        number: W,
        essentials: &'a CurrencyEssentials<'a>,
        sign_formatter: &'a DecimalFormatter,
        sign: Sign,
        options: CurrencyFormatterOptions,
        accounting_patterns: Option<&'a CurrencyAccountingPatterns<'a>>,
    ) -> Self {
        Self {
            number,
            essentials,
            sign_formatter,
            sign,
            currency: options.currency.as_tinystr(),
            display: options.display,
            currency_sign: options.sign,
            accounting_patterns,
        }
    }

    fn symbol_and_pattern(&self) -> (&str, &DoublePlaceholderPattern, PatternSelection) {
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

        (symbol, pattern, selection)
    }

    fn body(&self) -> CurrencyBody<'_, W> {
        let (symbol, pattern, _) = self.symbol_and_pattern();
        CurrencyBody {
            number: &self.number,
            pattern,
            symbol,
        }
    }

    fn accounting_body(&self) -> Option<AccountingCurrencyBody<'_, W>> {
        if self.sign != Sign::Negative || self.currency_sign != CurrencySign::Accounting {
            return None;
        }
        let patterns = self.accounting_patterns?;
        let (symbol, _, selection) = self.symbol_and_pattern();
        let pattern = match selection {
            PatternSelection::Standard => patterns.standard.as_ref(),
            PatternSelection::StandardAlphaNextToNumber => patterns.alpha_next_to_number.as_ref(),
        }?;
        Some(AccountingCurrencyBody {
            number: &self.number,
            pattern,
            symbol,
            minus_sign: &patterns.minus_sign,
        })
    }
}

impl<W: Writeable> Writeable for FormattedCurrency<'_, W> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        if let Some(accounting) = self.accounting_body() {
            return accounting.write_to(sink);
        }
        self.sign_formatter
            .format_sign(self.sign, self.body())
            .write_to(sink)
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        if let Some(accounting) = self.accounting_body() {
            return accounting.write_to_parts(sink);
        }
        self.sign_formatter
            .format_sign(self.sign, self.body())
            .write_to_parts(sink)
    }

    fn writeable_length_hint(&self) -> LengthHint {
        if let Some(accounting) = self.accounting_body() {
            return accounting.writeable_length_hint();
        }
        self.sign_formatter
            .format_sign(self.sign, self.body())
            .writeable_length_hint()
    }
}
