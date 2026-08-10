//! Locale-sensitive assembly of ECMA-402 number ranges.

use super::Style;
use crate::builtins::intl::parts::{FormattedPart, RangePart, RangeSource};

pub(super) struct RangeFormatter<'a> {
    separator: &'a str,
    approximately_sign: &'a str,
    style: Style,
}

impl<'a> RangeFormatter<'a> {
    pub(super) const fn new(separator: &'a str, approximately_sign: &'a str, style: Style) -> Self {
        Self {
            separator,
            approximately_sign,
            style,
        }
    }

    pub(super) fn format(
        &self,
        start: Vec<FormattedPart>,
        end: Vec<FormattedPart>,
    ) -> Vec<RangePart> {
        if formatted_values_equal(&start, &end) {
            return self.format_approximately(start);
        }

        self.format_distinct(start, end)
    }

    fn format_approximately(&self, mut parts: Vec<FormattedPart>) -> Vec<RangePart> {
        if !parts.iter().any(is_numeric_part) {
            return parts
                .into_iter()
                .map(|part| RangePart::new(part, RangeSource::Shared))
                .collect();
        }

        let mut result = Vec::with_capacity(parts.len() + 1);
        let approximately_sign = RangePart {
            kind: "approximatelySign",
            value: self.approximately_sign.to_owned(),
            source: RangeSource::Shared,
        };
        let insertion_index = if self.style == Style::Currency {
            let leading = leading_affix_len(&parts);
            if parts[..leading].iter().all(|part| part.kind == "literal") {
                leading
            } else {
                0
            }
        } else {
            0
        };
        let trailing_parts = parts.split_off(insertion_index);
        result.extend(
            parts
                .into_iter()
                .map(|part| RangePart::new(part, RangeSource::Shared)),
        );
        result.push(approximately_sign);
        result.extend(
            trailing_parts
                .into_iter()
                .map(|part| RangePart::new(part, RangeSource::Shared)),
        );
        result
    }

    fn format_distinct(
        &self,
        mut start: Vec<FormattedPart>,
        mut end: Vec<FormattedPart>,
    ) -> Vec<RangePart> {
        let prefix_limit = leading_affix_len(&start).min(leading_affix_len(&end));
        let prefix_len = common_prefix_len(&start[..prefix_limit], &end[..prefix_limit]);
        let suffix_limit = trailing_affix_len(&start).min(trailing_affix_len(&end));
        let suffix_len = common_suffix_len(
            &start[start.len() - suffix_limit..],
            &end[end.len() - suffix_limit..],
        );
        let common_prefix = &start[..prefix_len];
        let common_suffix = &start[start.len() - suffix_len..];
        let prefix_has_sign = contains_kind(common_prefix, &["plusSign", "minusSign"]);
        let prefix_has_literal = contains_kind(common_prefix, &["literal"]);
        let prefix_has_decorated_sign = common_prefix.iter().any(|part| {
            matches!(part.kind, "plusSign" | "minusSign") && part.value.chars().count() > 1
        });
        let prefix_has_currency = contains_kind(common_prefix, &["currency"]);
        let suffix_has_currency = contains_kind(common_suffix, &["currency"]);
        let suffix_has_unit = contains_kind(common_suffix, &["unit"]);
        let suffix_has_literal = contains_kind(common_suffix, &["literal"]);
        let suffix_has_percent = contains_kind(common_suffix, &["percentSign"])
            || common_suffix.iter().any(is_percent_unit);
        let collapse_prefix = prefix_len > 0
            && ((prefix_has_sign
                && (matches!(self.style, Style::Currency | Style::Percent)
                    || prefix_has_literal
                    || prefix_has_decorated_sign
                    || suffix_has_percent))
                || (self.style == Style::Currency
                    && prefix_has_literal
                    && (prefix_has_currency || suffix_has_currency)));
        let collapse_suffix = suffix_len > 0
            && (suffix_has_currency
                || (suffix_has_unit && !suffix_has_percent)
                || (suffix_has_percent && (collapse_prefix || suffix_has_literal))
                || (self.style == Style::Currency && collapse_prefix));

        let shared_prefix = if collapse_prefix {
            start.drain(..prefix_len).collect()
        } else {
            Vec::new()
        };
        if collapse_prefix {
            end.drain(..prefix_len);
        }

        let shared_suffix = if collapse_suffix {
            let suffix = start.split_off(start.len() - suffix_len);
            end.truncate(end.len() - suffix_len);
            suffix
        } else {
            Vec::new()
        };

        let separator =
            if has_common_affix(&start, &end) && !self.separator.starts_with(char::is_whitespace) {
                format!(" {} ", self.separator.trim())
            } else {
                self.separator.to_owned()
            };

        let mut result = Vec::with_capacity(
            shared_prefix.len() + start.len() + 1 + end.len() + shared_suffix.len(),
        );
        result.extend(
            shared_prefix
                .into_iter()
                .map(|part| RangePart::new(part, RangeSource::Shared)),
        );
        result.extend(
            start
                .into_iter()
                .map(|part| RangePart::new(part, RangeSource::Start)),
        );
        result.push(RangePart {
            kind: "literal",
            value: separator,
            source: RangeSource::Shared,
        });
        result.extend(
            end.into_iter()
                .map(|part| RangePart::new(part, RangeSource::End)),
        );
        result.extend(
            shared_suffix
                .into_iter()
                .map(|part| RangePart::new(part, RangeSource::Shared)),
        );
        result
    }
}

fn formatted_values_equal(start: &[FormattedPart], end: &[FormattedPart]) -> bool {
    start
        .iter()
        .flat_map(|part| part.value.chars())
        .eq(end.iter().flat_map(|part| part.value.chars()))
}

fn is_numeric_part(part: &FormattedPart) -> bool {
    matches!(
        part.kind,
        "integer"
            | "group"
            | "decimal"
            | "fraction"
            | "compact"
            | "exponentSeparator"
            | "exponentMinusSign"
            | "exponentPlusSign"
            | "exponentInteger"
            | "nan"
            | "infinity"
    )
}

fn contains_kind(parts: &[FormattedPart], kinds: &[&str]) -> bool {
    parts.iter().any(|part| kinds.contains(&part.kind))
}

fn is_percent_unit(part: &FormattedPart) -> bool {
    part.kind == "unit"
        && part
            .value
            .chars()
            .any(|character| matches!(character, '%' | '٪'))
}

fn has_common_affix(start: &[FormattedPart], end: &[FormattedPart]) -> bool {
    let prefix_limit = leading_affix_len(start).min(leading_affix_len(end));
    if common_prefix_len(&start[..prefix_limit], &end[..prefix_limit]) > 0 {
        return true;
    }

    let suffix_limit = trailing_affix_len(start).min(trailing_affix_len(end));
    common_suffix_len(
        &start[start.len() - suffix_limit..],
        &end[end.len() - suffix_limit..],
    ) > 0
}

fn leading_affix_len(parts: &[FormattedPart]) -> usize {
    parts
        .iter()
        .position(is_numeric_part)
        .unwrap_or(parts.len())
}

fn trailing_affix_len(parts: &[FormattedPart]) -> usize {
    parts
        .iter()
        .rposition(is_numeric_part)
        .map_or(parts.len(), |index| parts.len() - index - 1)
}

fn common_prefix_len(start: &[FormattedPart], end: &[FormattedPart]) -> usize {
    start
        .iter()
        .zip(end)
        .take_while(|(start, end)| start == end)
        .count()
}

fn common_suffix_len(start: &[FormattedPart], end: &[FormattedPart]) -> usize {
    start
        .iter()
        .rev()
        .zip(end.iter().rev())
        .take_while(|(start, end)| start == end)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(kind: &'static str, value: &str) -> FormattedPart {
        FormattedPart {
            kind,
            value: value.to_owned(),
        }
    }

    #[test]
    fn equal_ranges_are_approximate_and_shared() {
        let parts = RangeFormatter::new("–", "~", Style::Decimal)
            .format(vec![part("integer", "3")], vec![part("integer", "3")]);
        assert_eq!(
            parts,
            [
                RangePart {
                    kind: "approximatelySign",
                    value: "~".to_owned(),
                    source: RangeSource::Shared,
                },
                RangePart {
                    kind: "integer",
                    value: "3".to_owned(),
                    source: RangeSource::Shared,
                },
            ]
        );
    }

    #[test]
    fn collapses_shared_suffixes_without_losing_part_boundaries() {
        let start = vec![
            part("integer", "3"),
            part("literal", "\u{a0}"),
            part("currency", "€"),
        ];
        let end = vec![
            part("integer", "5"),
            part("literal", "\u{a0}"),
            part("currency", "€"),
        ];
        let parts = RangeFormatter::new(" - ", "~", Style::Currency).format(start, end);
        assert_eq!(
            parts.iter().map(|part| &*part.value).collect::<String>(),
            "3 - 5\u{a0}€"
        );
        assert_eq!(parts.last().unwrap().source, RangeSource::Shared);
    }

    #[test]
    fn retains_repeated_prefix_currency_when_signs_are_not_shared() {
        let start = vec![part("currency", "$"), part("integer", "3")];
        let end = vec![part("currency", "$"), part("integer", "5")];
        let parts = RangeFormatter::new("–", "~", Style::Currency).format(start, end);
        assert_eq!(
            parts.iter().map(|part| &*part.value).collect::<String>(),
            "$3 – $5"
        );
    }

    #[test]
    fn never_collapses_identical_fractional_digits_as_an_affix() {
        let start = vec![
            part("integer", "3"),
            part("decimal", ","),
            part("fraction", "00"),
            part("literal", "\u{a0}"),
            part("currency", "€"),
        ];
        let end = vec![
            part("integer", "5"),
            part("decimal", ","),
            part("fraction", "00"),
            part("literal", "\u{a0}"),
            part("currency", "€"),
        ];
        let parts = RangeFormatter::new(" - ", "~", Style::Currency).format(start, end);
        assert_eq!(
            parts.iter().map(|part| &*part.value).collect::<String>(),
            "3,00 - 5,00\u{a0}€"
        );
    }

    #[test]
    fn repeats_plain_signs_with_a_spaced_separator() {
        let start = vec![part("minusSign", "-"), part("integer", "3")];
        let end = vec![part("minusSign", "-"), part("integer", "5")];
        let parts = RangeFormatter::new("–", "~", Style::Decimal).format(start, end);
        assert_eq!(
            parts.iter().map(|part| &*part.value).collect::<String>(),
            "-3 – -5"
        );
        assert_eq!(parts[0].source, RangeSource::Start);
        assert_eq!(parts[3].source, RangeSource::End);
    }

    #[test]
    fn repeats_unit_signs_while_collapsing_the_unit() {
        let start = vec![
            part("minusSign", "-"),
            part("integer", "3"),
            part("literal", " "),
            part("unit", "meters"),
        ];
        let end = vec![
            part("minusSign", "-"),
            part("integer", "5"),
            part("literal", " "),
            part("unit", "meters"),
        ];
        let parts = RangeFormatter::new("–", "~", Style::Unit).format(start, end);
        assert_eq!(
            parts.iter().map(|part| &*part.value).collect::<String>(),
            "-3 – -5 meters"
        );
        assert_eq!(parts.last().unwrap().source, RangeSource::Shared);
    }

    #[test]
    fn collapses_signs_that_carry_directional_marks() {
        let start = vec![part("minusSign", "\u{61c}-"), part("integer", "٣")];
        let end = vec![part("minusSign", "\u{61c}-"), part("integer", "٥")];
        let parts = RangeFormatter::new("–", "~", Style::Decimal).format(start, end);
        assert_eq!(
            parts.iter().map(|part| &*part.value).collect::<String>(),
            "\u{61c}-٣–٥"
        );
        assert_eq!(parts[0].source, RangeSource::Shared);
    }

    #[test]
    fn collapses_percent_signs_and_accounting_wrappers() {
        let percent = RangeFormatter::new("–", "~", Style::Percent).format(
            vec![
                part("minusSign", "-"),
                part("integer", "300"),
                part("percentSign", "%"),
            ],
            vec![
                part("minusSign", "-"),
                part("integer", "500"),
                part("percentSign", "%"),
            ],
        );
        assert_eq!(
            percent.iter().map(|part| &*part.value).collect::<String>(),
            "-300–500%"
        );

        let positive_percent = RangeFormatter::new("–", "~", Style::Percent).format(
            vec![part("integer", "300"), part("percentSign", "%")],
            vec![part("integer", "500"), part("percentSign", "%")],
        );
        assert_eq!(
            positive_percent
                .iter()
                .map(|part| &*part.value)
                .collect::<String>(),
            "300% – 500%"
        );

        let unit_percent = RangeFormatter::new("–", "~", Style::Unit).format(
            vec![
                part("minusSign", "-"),
                part("integer", "3"),
                part("unit", "%"),
            ],
            vec![
                part("minusSign", "-"),
                part("integer", "5"),
                part("unit", "%"),
            ],
        );
        assert_eq!(
            unit_percent
                .iter()
                .map(|part| &*part.value)
                .collect::<String>(),
            "-3–5%"
        );

        let accounting = RangeFormatter::new("–", "~", Style::Currency).format(
            vec![
                part("literal", "("),
                part("currency", "$"),
                part("integer", "3"),
                part("literal", ")"),
            ],
            vec![
                part("literal", "("),
                part("currency", "$"),
                part("integer", "5"),
                part("literal", ")"),
            ],
        );
        assert_eq!(
            accounting
                .iter()
                .map(|part| &*part.value)
                .collect::<String>(),
            "($3–5)"
        );
    }

    #[test]
    fn does_not_add_approximation_to_non_numeric_output() {
        let parts = RangeFormatter::new("–", "~", Style::Unit)
            .format(vec![part("unit", "meter")], vec![part("unit", "meter")]);
        assert_eq!(
            parts,
            [RangePart::new(part("unit", "meter"), RangeSource::Shared)]
        );
    }
}
