//! Structured unit output built from ICU4X locale patterns.

use core::fmt::Write as _;

use icu_experimental::dimension::provider::units::{
    display_names::{UnitsDisplayNames, UnitsDisplayNamesV1},
    essentials::UnitsEssentialsV1,
};
use icu_locale::Locale;
use icu_pattern::{
    DoublePlaceholderKey, DoublePlaceholderPattern, ParserOptions, PatternItem,
    SinglePlaceholderPattern,
};
use icu_plurals::{PluralCategory, PluralOperands, PluralRules, PluralRulesPreferences};
use icu_provider::{
    DataError, DataErrorKind, DataIdentifierBorrowed, DataMarkerAttributes, DataPayload,
    DataProvider, DataRequest, DryDataProvider,
};
use writeable::{LengthHint, Part, PartsWrite, Writeable};

use super::{Unit, UnitDisplay};
use crate::context::icu::IntlProvider;

const UNIT: Part = Part {
    category: "number",
    value: "unit",
};

#[derive(Debug)]
enum UnitPatterns {
    Direct(DataPayload<UnitsDisplayNamesV1>),
    Compound {
        numerator: DataPayload<UnitsDisplayNamesV1>,
        denominator: DataPayload<UnitsDisplayNamesV1>,
        per: Box<DoublePlaceholderPattern>,
    },
}

/// Locale data required to apply a unit style to an already-formatted number.
#[derive(Debug)]
pub(super) struct UnitData {
    patterns: UnitPatterns,
    plural_rules: PluralRules,
    other_operands: PluralOperands,
}

impl UnitData {
    pub(super) fn try_new(
        provider: &IntlProvider,
        locale: &Locale,
        unit: &Unit,
        display: UnitDisplay,
    ) -> Result<Self, DataError> {
        let data_locale = locale.into();
        let load_display_name = |identifier: &str| {
            let attribute = format!("{}-{identifier}", display.as_str());
            let attributes = DataMarkerAttributes::try_from_str(&attribute)
                .map_err(|_| DataError::custom("invalid unit data marker attribute"))?;
            let mut metadata = icu_provider::DataRequestMetadata::default();
            metadata.silent = true;
            let request = DataRequest {
                id: DataIdentifierBorrowed::for_marker_attributes_and_locale(
                    attributes,
                    &data_locale,
                ),
                metadata,
            };
            DryDataProvider::<UnitsDisplayNamesV1>::dry_load(provider, request)?;
            let response = DataProvider::<UnitsDisplayNamesV1>::load(provider, request)?;
            Ok::<_, DataError>(response.payload)
        };

        let patterns = match load_display_name(&unit.identifier()) {
            Ok(direct) => UnitPatterns::Direct(direct),
            Err(error) if error.kind == DataErrorKind::IdentifierNotFound => {
                let Some(denominator_identifier) = unit.denominator() else {
                    return Err(error);
                };
                let numerator = load_display_name(unit.numerator())?;
                let denominator = load_display_name(denominator_identifier)?;

                let width = DataMarkerAttributes::try_from_str(display.as_str())
                    .map_err(|_| DataError::custom("invalid unit width data marker attribute"))?;
                let essentials = DataProvider::<UnitsEssentialsV1>::load(
                    provider,
                    DataRequest {
                        id: DataIdentifierBorrowed::for_marker_attributes_and_locale(
                            width,
                            &data_locale,
                        ),
                        ..DataRequest::default()
                    },
                )?;
                let per = DoublePlaceholderPattern::try_from_str(
                    &essentials.payload.get().per,
                    ParserOptions::default(),
                )
                .map_err(|_| DataError::custom("invalid unit composition pattern"))?;

                UnitPatterns::Compound {
                    numerator,
                    denominator,
                    per,
                }
            }
            Err(error) => return Err(error),
        };

        let plural_rules = PluralRules::try_new_cardinal_with_buffer_provider(
            provider.erased_provider(),
            PluralRulesPreferences::from(locale),
        )?;
        // Special numeric values always use the `other` plural form. Keep a real
        // operand for that category because the packed ICU data deliberately
        // exposes selection through plural rules rather than category indexing.
        let other_operands = (0_u32..=200)
            .find(|value| plural_rules.category_for(*value) == PluralCategory::Other)
            .map(PluralOperands::from)
            .unwrap_or_default();

        Ok(Self {
            patterns,
            plural_rules,
            other_operands,
        })
    }

    fn operands(&self, operands: Option<PluralOperands>) -> PluralOperands {
        operands.unwrap_or(self.other_operands)
    }
}

fn is_unit_spacing(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '\u{061C}' // ARABIC LETTER MARK
                | '\u{200E}' // LEFT-TO-RIGHT MARK
                | '\u{200F}' // RIGHT-TO-LEFT MARK
        )
}

fn split_leading_spacing(value: &str) -> (&str, &str) {
    let boundary = value
        .char_indices()
        .find_map(|(index, character)| (!is_unit_spacing(character)).then_some(index))
        .unwrap_or(value.len());
    value.split_at(boundary)
}

fn split_trailing_spacing(value: &str) -> (&str, &str) {
    let boundary = value
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            (!is_unit_spacing(character)).then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    value.split_at(boundary)
}

fn write_unit_literal<S: PartsWrite + ?Sized>(
    literal: &str,
    before_number: bool,
    sink: &mut S,
) -> core::fmt::Result {
    let (unit, spacing) = if before_number {
        split_trailing_spacing(literal)
    } else {
        let (spacing, unit) = split_leading_spacing(literal);
        if !spacing.is_empty() {
            sink.write_str(spacing)?;
        }
        return sink.with_part(UNIT, |sink| sink.write_str(unit));
    };

    sink.with_part(UNIT, |sink| sink.write_str(unit))?;
    sink.write_str(spacing)
}

struct SelectedUnit<'a, W> {
    number: &'a W,
    pattern: &'a SinglePlaceholderPattern,
}

impl<W: Writeable> Writeable for SelectedUnit<'_, W> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        for item in self.pattern.iter() {
            match item {
                PatternItem::Literal(literal) => sink.write_str(literal)?,
                PatternItem::Placeholder(_) => self.number.write_to(sink)?,
            }
        }
        Ok(())
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        let mut before_number = true;
        for item in self.pattern.iter() {
            match item {
                PatternItem::Literal(literal) => {
                    write_unit_literal(literal, before_number, sink)?;
                }
                PatternItem::Placeholder(_) => {
                    self.number.write_to_parts(sink)?;
                    before_number = false;
                }
            }
        }
        Ok(())
    }

    fn writeable_length_hint(&self) -> LengthHint {
        let mut hint = self.number.writeable_length_hint();
        for item in self.pattern.iter() {
            if let PatternItem::Literal(literal) = item {
                hint += LengthHint::exact(literal.len());
            }
        }
        hint
    }
}

struct UnitName<'a> {
    pattern: &'a SinglePlaceholderPattern,
}

impl UnitName<'_> {
    fn write_literals<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        for item in self.pattern.iter() {
            if let PatternItem::Literal(literal) = item {
                sink.write_str(literal.trim_matches(is_unit_spacing))?;
            }
        }
        Ok(())
    }
}

impl Writeable for UnitName<'_> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        self.write_literals(sink)
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        sink.with_part(UNIT, |sink| self.write_literals(sink))
    }

    fn writeable_length_hint(&self) -> LengthHint {
        LengthHint::undefined()
    }
}

pub(super) struct FormattedUnit<'a, W> {
    number: W,
    data: &'a UnitData,
    operands: Option<PluralOperands>,
}

impl<'a, W> FormattedUnit<'a, W> {
    pub(super) const fn new(
        number: W,
        data: &'a UnitData,
        operands: Option<PluralOperands>,
    ) -> Self {
        Self {
            number,
            data,
            operands,
        }
    }

    fn direct(&self, data: &'a UnitsDisplayNames<'a>) -> SelectedUnit<'_, W> {
        SelectedUnit {
            number: &self.number,
            pattern: data
                .patterns
                .get(self.data.operands(self.operands), &self.data.plural_rules),
        }
    }
}

impl<W: Writeable> Writeable for FormattedUnit<'_, W> {
    fn write_to<S: core::fmt::Write + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        match &self.data.patterns {
            UnitPatterns::Direct(data) => self.direct(data.get()).write_to(sink),
            UnitPatterns::Compound {
                numerator,
                denominator,
                per,
            } => {
                let numerator = self.direct(numerator.get());
                let denominator = UnitName {
                    pattern: denominator
                        .get()
                        .patterns
                        .get(1.into(), &self.data.plural_rules),
                };
                for item in per.iter() {
                    match item {
                        PatternItem::Literal(literal) => sink.write_str(literal)?,
                        PatternItem::Placeholder(DoublePlaceholderKey::Place0) => {
                            numerator.write_to(sink)?;
                        }
                        PatternItem::Placeholder(DoublePlaceholderKey::Place1) => {
                            denominator.write_to(sink)?;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn write_to_parts<S: PartsWrite + ?Sized>(&self, sink: &mut S) -> core::fmt::Result {
        match &self.data.patterns {
            UnitPatterns::Direct(data) => self.direct(data.get()).write_to_parts(sink),
            UnitPatterns::Compound {
                numerator,
                denominator,
                per,
            } => {
                let numerator = self.direct(numerator.get());
                let denominator = UnitName {
                    pattern: denominator
                        .get()
                        .patterns
                        .get(1.into(), &self.data.plural_rules),
                };
                for item in per.iter() {
                    match item {
                        PatternItem::Literal(literal) => {
                            sink.with_part(UNIT, |sink| sink.write_str(literal))?;
                        }
                        PatternItem::Placeholder(DoublePlaceholderKey::Place0) => {
                            numerator.write_to_parts(sink)?;
                        }
                        PatternItem::Placeholder(DoublePlaceholderKey::Place1) => {
                            denominator.write_to_parts(sink)?;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn writeable_length_hint(&self) -> LengthHint {
        LengthHint::undefined()
    }
}
