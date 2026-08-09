//! Structured percent output built from ICU4X locale patterns.

use core::fmt::Write as _;

use fixed_decimal::Sign;
use icu_experimental::dimension::provider::percent::PercentEssentials;
use icu_pattern::{DoublePlaceholderKey, PatternItem};
use writeable::{LengthHint, Part, PartsWrite, Writeable};

const PERCENT_SIGN: Part = Part {
    category: "number",
    value: "percentSign",
};
const PLUS_SIGN: Part = Part {
    category: "number",
    value: "plusSign",
};
const MINUS_SIGN: Part = Part {
    category: "number",
    value: "minusSign",
};

pub(super) struct FormattedPercent<'a, W> {
    number: W,
    essentials: &'a PercentEssentials<'a>,
    sign: Sign,
}

impl<'a, W> FormattedPercent<'a, W> {
    pub(super) const fn new(number: W, essentials: &'a PercentEssentials<'a>, sign: Sign) -> Self {
        Self {
            number,
            essentials,
            sign,
        }
    }

    fn sign_string(&self) -> &str {
        match self.sign {
            Sign::Positive => &self.essentials.plus_sign,
            Sign::Negative => &self.essentials.minus_sign,
            Sign::None => "",
        }
    }
}

fn is_percent_spacing(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '\u{061C}' // ARABIC LETTER MARK
                | '\u{200E}' // LEFT-TO-RIGHT MARK
                | '\u{200F}' // RIGHT-TO-LEFT MARK
        )
}

fn write_percent_literal<S: PartsWrite + ?Sized>(literal: &str, sink: &mut S) -> core::fmt::Result {
    let mut start = 0;
    let mut spacing = literal.chars().next().is_some_and(is_percent_spacing);

    for (index, character) in literal.char_indices() {
        let current_spacing = is_percent_spacing(character);
        if current_spacing == spacing {
            continue;
        }

        let segment = &literal[start..index];
        if spacing {
            sink.write_str(segment)?;
        } else {
            sink.with_part(PERCENT_SIGN, |sink| sink.write_str(segment))?;
        }
        start = index;
        spacing = current_spacing;
    }

    let segment = &literal[start..];
    if spacing {
        sink.write_str(segment)
    } else {
        sink.with_part(PERCENT_SIGN, |sink| sink.write_str(segment))
    }
}

impl<W: Writeable> Writeable for FormattedPercent<'_, W> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        if self.sign == Sign::None {
            for item in self.essentials.unsigned_pattern.iter() {
                match item {
                    PatternItem::Literal(literal) => sink.write_str(literal)?,
                    PatternItem::Placeholder(_) => self.number.write_to(sink)?,
                }
            }
        } else {
            for item in self.essentials.signed_pattern.iter() {
                match item {
                    PatternItem::Literal(literal) => sink.write_str(literal)?,
                    PatternItem::Placeholder(DoublePlaceholderKey::Place0) => {
                        self.number.write_to(sink)?;
                    }
                    PatternItem::Placeholder(DoublePlaceholderKey::Place1) => {
                        sink.write_str(self.sign_string())?;
                    }
                }
            }
        }
        Ok(())
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        if self.sign == Sign::None {
            for item in self.essentials.unsigned_pattern.iter() {
                match item {
                    PatternItem::Literal(literal) => write_percent_literal(literal, sink)?,
                    PatternItem::Placeholder(_) => self.number.write_to_parts(sink)?,
                }
            }
        } else {
            for item in self.essentials.signed_pattern.iter() {
                match item {
                    PatternItem::Literal(literal) => write_percent_literal(literal, sink)?,
                    PatternItem::Placeholder(DoublePlaceholderKey::Place0) => {
                        self.number.write_to_parts(sink)?;
                    }
                    PatternItem::Placeholder(DoublePlaceholderKey::Place1) => {
                        let part = if self.sign == Sign::Negative {
                            MINUS_SIGN
                        } else {
                            PLUS_SIGN
                        };
                        sink.with_part(part, |sink| sink.write_str(self.sign_string()))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn writeable_length_hint(&self) -> LengthHint {
        let mut hint = self.number.writeable_length_hint();
        if self.sign == Sign::None {
            for item in self.essentials.unsigned_pattern.iter() {
                if let PatternItem::Literal(literal) = item {
                    hint += LengthHint::exact(literal.len());
                }
            }
        } else {
            hint += LengthHint::exact(self.sign_string().len());
            for item in self.essentials.signed_pattern.iter() {
                if let PatternItem::Literal(literal) = item {
                    hint += LengthHint::exact(literal.len());
                }
            }
        }
        hint
    }
}
