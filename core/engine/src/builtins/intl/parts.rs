//! Bridges ICU4X's structured `Writeable` output to ECMA-402 part objects.

use crate::{
    Context, JsExpect, JsResult, JsValue,
    builtins::{Array, OrdinaryObject},
    js_string,
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FormattedPart {
    pub(crate) kind: &'static str,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeSource {
    Start,
    End,
    Shared,
}

impl RangeSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "startRange",
            Self::End => "endRange",
            Self::Shared => "shared",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RangePart {
    pub(crate) kind: &'static str,
    pub(crate) value: String,
    pub(crate) source: RangeSource,
}

impl RangePart {
    pub(crate) fn new(part: FormattedPart, source: RangeSource) -> Self {
        Self {
            kind: part.kind,
            value: part.value,
            source,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum UnmarkedStyle {
    /// Drop text outside an ICU part.
    Ignore,
    /// Treat text outside an ICU part as an ECMA-402 literal.
    Literal,
    /// Split compact-number affixes into whitespace literals and compact values.
    Compact,
}

#[derive(Debug)]
pub(crate) struct PartsCollector {
    pub(crate) parts: Vec<FormattedPart>,
    active_part: Option<&'static str>,
    unmarked_style: UnmarkedStyle,
}

impl PartsCollector {
    pub(crate) const fn new(unmarked_style: UnmarkedStyle) -> Self {
        Self {
            parts: Vec::new(),
            active_part: None,
            unmarked_style,
        }
    }

    fn push(&mut self, kind: &'static str, value: &str) {
        if value.is_empty() {
            return;
        }

        if let Some(last) = self.parts.last_mut()
            && last.kind == kind
        {
            last.value.push_str(value);
        } else {
            self.parts.push(FormattedPart {
                kind,
                value: value.to_owned(),
            });
        }
    }

    fn push_unmarked(&mut self, value: &str) {
        match self.unmarked_style {
            UnmarkedStyle::Ignore => {}
            UnmarkedStyle::Literal => self.push("literal", value),
            UnmarkedStyle::Compact => {
                let mut start = 0;
                let mut whitespace = None;
                for (index, character) in value.char_indices() {
                    let is_whitespace = character.is_whitespace();
                    if whitespace.is_some_and(|current| current != is_whitespace) {
                        self.push(
                            if whitespace == Some(true) {
                                "literal"
                            } else {
                                "compact"
                            },
                            &value[start..index],
                        );
                        start = index;
                    }
                    whitespace = Some(is_whitespace);
                }

                self.push(
                    if whitespace == Some(true) {
                        "literal"
                    } else {
                        "compact"
                    },
                    &value[start..],
                );
            }
        }
    }

    pub(crate) fn into_js_array(self, context: &mut Context) -> JsResult<JsValue> {
        parts_into_js_array(
            self.parts
                .into_iter()
                .map(|part| (part.kind, part.value, None)),
            context,
        )
    }
}

pub(crate) fn range_parts_into_js_array(
    parts: Vec<RangePart>,
    context: &mut Context,
) -> JsResult<JsValue> {
    parts_into_js_array(
        parts
            .into_iter()
            .map(|part| (part.kind, part.value, Some(part.source.as_str()))),
        context,
    )
}

fn parts_into_js_array(
    parts: impl IntoIterator<Item = (&'static str, String, Option<&'static str>)>,
    context: &mut Context,
) -> JsResult<JsValue> {
    let result = Array::array_create(0, None, context)
        .js_expect("creating an empty array with the default prototype must not fail")?;

    for (index, (kind, value, source)) in parts.into_iter().enumerate() {
        let object = context
            .intrinsics()
            .templates()
            .ordinary_object()
            .create(OrdinaryObject, vec![]);
        object
            .create_data_property_or_throw(js_string!("type"), js_string!(kind), context)
            .js_expect("creating a property on a fresh ordinary object must not fail")?;
        object
            .create_data_property_or_throw(js_string!("value"), js_string!(value), context)
            .js_expect("creating a property on a fresh ordinary object must not fail")?;
        if let Some(source) = source {
            object
                .create_data_property_or_throw(js_string!("source"), js_string!(source), context)
                .js_expect("creating a property on a fresh ordinary object must not fail")?;
        }
        result
            .create_data_property_or_throw(index, object, context)
            .js_expect("creating an indexed property on a fresh array must not fail")?;
    }

    Ok(result.into())
}

impl core::fmt::Write for PartsCollector {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        if let Some(kind) = self.active_part {
            self.push(kind, value);
        } else {
            self.push_unmarked(value);
        }
        Ok(())
    }
}

impl writeable::PartsWrite for PartsCollector {
    type SubPartsWrite = Self;

    fn with_part(
        &mut self,
        part: writeable::Part,
        mut write: impl FnMut(&mut Self::SubPartsWrite) -> core::fmt::Result,
    ) -> core::fmt::Result {
        let previous = self.active_part.replace(part.value);
        let result = write(self);
        self.active_part = previous;
        result
    }
}

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use super::{FormattedPart, PartsCollector, UnmarkedStyle};

    #[test]
    fn unmarked_text_policies_preserve_boundaries() {
        let mut ignored = PartsCollector::new(UnmarkedStyle::Ignore);
        ignored.write_str("ignored").unwrap();
        assert!(ignored.parts.is_empty());

        let mut literal = PartsCollector::new(UnmarkedStyle::Literal);
        literal.write_str("two ").unwrap();
        literal.write_str("writes").unwrap();
        assert_eq!(
            literal.parts,
            [FormattedPart {
                kind: "literal",
                value: "two writes".to_owned(),
            }]
        );

        let mut compact = PartsCollector::new(UnmarkedStyle::Compact);
        compact.write_str("\u{a0}million").unwrap();
        assert_eq!(
            compact.parts,
            [
                FormattedPart {
                    kind: "literal",
                    value: "\u{a0}".to_owned(),
                },
                FormattedPart {
                    kind: "compact",
                    value: "million".to_owned(),
                },
            ]
        );
    }
}
