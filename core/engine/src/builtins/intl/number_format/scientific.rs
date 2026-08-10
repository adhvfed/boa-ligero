//! Structured scientific-notation output built on ICU4X decimal formatting.

use core::fmt::Write as _;

use fixed_decimal::Decimal;
use icu_decimal::{DecimalFormatter, FormattedDecimal};
use writeable::{LengthHint, Part, PartsWrite, Writeable};

const EXPONENT_SEPARATOR: Part = Part {
    category: "number",
    value: "exponentSeparator",
};

pub(super) struct FormattedScientific<'a> {
    significand: FormattedDecimal<'a>,
    exponent: Decimal,
    exponent_formatter: &'a DecimalFormatter,
    exponent_separator: &'a str,
}

impl<'a> FormattedScientific<'a> {
    pub(super) fn new(
        significand: FormattedDecimal<'a>,
        exponent: i16,
        exponent_formatter: &'a DecimalFormatter,
        exponent_separator: &'a str,
    ) -> Self {
        Self {
            significand,
            exponent: Decimal::from(exponent),
            exponent_formatter,
            exponent_separator,
        }
    }
}

struct ExponentParts<'a, S: ?Sized> {
    sink: &'a mut S,
}

impl<S: core::fmt::Write + ?Sized> core::fmt::Write for ExponentParts<'_, S> {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        self.sink.write_str(value)
    }
}

impl<S: PartsWrite + ?Sized> PartsWrite for ExponentParts<'_, S> {
    type SubPartsWrite = S::SubPartsWrite;

    fn with_part(
        &mut self,
        part: Part,
        write: impl FnMut(&mut Self::SubPartsWrite) -> core::fmt::Result,
    ) -> core::fmt::Result {
        let part = match part.value {
            "integer" => Part {
                category: "number",
                value: "exponentInteger",
            },
            "minusSign" => Part {
                category: "number",
                value: "exponentMinusSign",
            },
            "plusSign" => Part {
                category: "number",
                value: "exponentPlusSign",
            },
            _ => part,
        };
        self.sink.with_part(part, write)
    }
}

impl Writeable for FormattedScientific<'_> {
    fn write_to<W: core::fmt::Write + ?Sized>(&self, sink: &mut W) -> core::fmt::Result {
        self.significand.write_to(sink)?;
        sink.write_str(self.exponent_separator)?;
        self.exponent_formatter
            .format(&self.exponent)
            .write_to(sink)
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        self.significand.write_to_parts(sink)?;
        sink.with_part(EXPONENT_SEPARATOR, |sink| {
            sink.write_str(self.exponent_separator)
        })?;
        self.exponent_formatter
            .format(&self.exponent)
            .write_to_parts(&mut ExponentParts { sink })
    }

    fn writeable_length_hint(&self) -> LengthHint {
        self.significand.writeable_length_hint()
            + LengthHint::exact(self.exponent_separator.len())
            + self
                .exponent_formatter
                .format(&self.exponent)
                .writeable_length_hint()
    }
}
