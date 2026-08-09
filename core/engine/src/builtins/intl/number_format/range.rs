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

    fn format_approximately(&self, parts: Vec<FormattedPart>) -> Vec<RangePart> {
        let mut result = Vec::with_capacity(parts.len() + 1);
        result.push(RangePart {
            kind: "approximatelySign",
            value: self.approximately_sign.to_owned(),
            source: RangeSource::Shared,
        });
        result.extend(
            parts
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
        let collapse_prefix = prefix_len > 0
            && start[..prefix_len]
                .iter()
                .any(|part| matches!(part.kind, "plusSign" | "minusSign"));
        let collapse_suffix = suffix_len > 0
            && start[start.len() - suffix_len..]
                .iter()
                .any(|part| matches!(part.kind, "currency" | "unit"));

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

        let repeat_affix =
            self.style != Style::Decimal && shared_prefix.is_empty() && shared_suffix.is_empty();
        let separator = if repeat_affix && !self.separator.starts_with(' ') {
            format!(" {} ", self.separator)
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
            | "exponentInteger"
            | "nan"
            | "infinity"
    )
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
}
