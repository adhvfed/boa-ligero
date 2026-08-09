//! CLDR data that ICU4X's source provider does not expose yet.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    error::Error,
    fs::{self, File},
    io::{self, BufWriter},
    path::PathBuf,
};

use boa_icu_data::{
    CurrencyAccountingPattern, CurrencyAccountingPatterns, CurrencyAccountingPlaceholderOrder,
    NumberSpecialSymbols,
};
use icu_provider::DataLocale;
use icu_provider_source::SourceDataProvider;
use serde::Deserialize;
use zip::ZipArchive;

const CLDR_NUMBERS_PREFIX: &str = "cldr-numbers-full/main/";
const NUMBERS_SUFFIX: &str = "/numbers.json";

#[derive(Deserialize)]
struct NumbersResource {
    main: BTreeMap<String, LocaleResource>,
}

#[derive(Deserialize)]
struct LocaleResource {
    numbers: Numbers,
}

#[derive(Deserialize)]
struct Numbers {
    #[serde(rename = "defaultNumberingSystem")]
    default_numbering_system: String,
    #[serde(flatten)]
    sections: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct Symbols {
    infinity: String,
    nan: String,
    #[serde(rename = "minusSign")]
    minus_sign: String,
}

#[derive(Deserialize)]
struct CurrencyFormats {
    accounting: String,
    #[serde(rename = "accounting-alphaNextToNumber")]
    accounting_alpha_next_to_number: Option<String>,
}

/// Supplemental number-format data indexed by its exact CLDR locale.
pub(crate) struct SupplementalNumberData {
    special_symbols: HashMap<DataLocale, NumberSpecialSymbols<'static>>,
    accounting_patterns: HashMap<DataLocale, CurrencyAccountingPatterns<'static>>,
}

impl SupplementalNumberData {
    pub(crate) fn load() -> Result<Self, Box<dyn Error>> {
        let archive_path = cldr_archive_path()?;
        let mut archive = ZipArchive::new(File::open(archive_path)?)?;
        let mut special_symbols = HashMap::new();
        let mut accounting_patterns = HashMap::new();

        for index in 0..archive.len() {
            let file_name = archive.by_index_raw(index)?.name().to_owned();
            let Some(locale_name) = file_name
                .strip_prefix(CLDR_NUMBERS_PREFIX)
                .and_then(|name| name.strip_suffix(NUMBERS_SUFFIX))
            else {
                continue;
            };

            let resource: NumbersResource = serde_json::from_reader(archive.by_index(index)?)?;
            let Some(locale_resource) = resource.main.get(locale_name) else {
                return Err(
                    format!("CLDR numbers resource is missing locale {locale_name}").into(),
                );
            };
            let numbers = &locale_resource.numbers;
            let key = format!("symbols-numberSystem-{}", numbers.default_numbering_system);
            let Some(symbols) = numbers.sections.get(&key) else {
                return Err(format!("CLDR locale {locale_name} is missing {key}").into());
            };
            let symbols: Symbols = serde_json::from_value(symbols.clone())?;
            let currency_key = format!(
                "currencyFormats-numberSystem-{}",
                numbers.default_numbering_system
            );
            let Some(currency_formats) = numbers.sections.get(&currency_key) else {
                return Err(format!("CLDR locale {locale_name} is missing {currency_key}").into());
            };
            let currency_formats: CurrencyFormats =
                serde_json::from_value(currency_formats.clone())?;
            let locale = if locale_name == "root" {
                DataLocale::default()
            } else {
                DataLocale::try_from_str(locale_name)?
            };
            special_symbols.insert(
                locale,
                NumberSpecialSymbols {
                    infinity: Cow::Owned(symbols.infinity),
                    nan: Cow::Owned(symbols.nan),
                },
            );
            let alpha_pattern = currency_formats
                .accounting_alpha_next_to_number
                .as_deref()
                .unwrap_or(&currency_formats.accounting);
            accounting_patterns.insert(
                locale,
                CurrencyAccountingPatterns {
                    standard: negative_accounting_pattern(&currency_formats.accounting)?,
                    alpha_next_to_number: negative_accounting_pattern(alpha_pattern)?,
                    minus_sign: Cow::Owned(symbols.minus_sign),
                },
            );
        }

        Ok(Self {
            special_symbols,
            accounting_patterns,
        })
    }

    pub(crate) const fn special_symbols(
        &self,
    ) -> &HashMap<DataLocale, NumberSpecialSymbols<'static>> {
        &self.special_symbols
    }

    pub(crate) const fn accounting_patterns(
        &self,
    ) -> &HashMap<DataLocale, CurrencyAccountingPatterns<'static>> {
        &self.accounting_patterns
    }
}

#[derive(Debug)]
enum AccountingPatternItem {
    Literal(String),
    Number,
    Currency,
    MinusSign,
}

fn negative_accounting_pattern(
    pattern: &str,
) -> Result<Option<CurrencyAccountingPattern<'static>>, Box<dyn Error>> {
    let Some(negative) = split_negative_subpattern(pattern) else {
        return Ok(None);
    };
    let items = parse_accounting_subpattern(negative)?;
    let mut literals = vec![String::new()];
    let mut placeholders = Vec::new();
    for item in items {
        let placeholder = match item {
            AccountingPatternItem::Literal(literal) => {
                literals
                    .last_mut()
                    .expect("accounting patterns always have a literal slot")
                    .push_str(&literal);
                continue;
            }
            AccountingPatternItem::Number => AccountingPlaceholder::Number,
            AccountingPatternItem::Currency => AccountingPlaceholder::Currency,
            AccountingPatternItem::MinusSign => AccountingPlaceholder::MinusSign,
        };
        placeholders.push(placeholder);
        literals.push(String::new());
    }

    let order = match placeholders.as_slice() {
        [
            AccountingPlaceholder::Number,
            AccountingPlaceholder::Currency,
        ] => CurrencyAccountingPlaceholderOrder::NumberCurrency,
        [
            AccountingPlaceholder::Currency,
            AccountingPlaceholder::Number,
        ] => CurrencyAccountingPlaceholderOrder::CurrencyNumber,
        [
            AccountingPlaceholder::MinusSign,
            AccountingPlaceholder::Currency,
            AccountingPlaceholder::Number,
        ] => CurrencyAccountingPlaceholderOrder::MinusCurrencyNumber,
        [
            AccountingPlaceholder::MinusSign,
            AccountingPlaceholder::Number,
            AccountingPlaceholder::Currency,
        ] => CurrencyAccountingPlaceholderOrder::MinusNumberCurrency,
        [
            AccountingPlaceholder::Currency,
            AccountingPlaceholder::MinusSign,
            AccountingPlaceholder::Number,
        ] => CurrencyAccountingPlaceholderOrder::CurrencyMinusNumber,
        [
            AccountingPlaceholder::Currency,
            AccountingPlaceholder::Number,
            AccountingPlaceholder::MinusSign,
        ] => CurrencyAccountingPlaceholderOrder::CurrencyNumberMinus,
        [
            AccountingPlaceholder::Number,
            AccountingPlaceholder::MinusSign,
            AccountingPlaceholder::Currency,
        ] => CurrencyAccountingPlaceholderOrder::NumberMinusCurrency,
        [
            AccountingPlaceholder::Number,
            AccountingPlaceholder::Currency,
            AccountingPlaceholder::MinusSign,
        ] => CurrencyAccountingPlaceholderOrder::NumberCurrencyMinus,
        _ => {
            return Err(format!(
                "unsupported accounting placeholder order in pattern {negative:?}: {placeholders:?}"
            )
            .into());
        }
    };
    let mut literals = literals.into_iter();
    let prefix = literals.next().unwrap_or_default();
    let infix = literals.next().unwrap_or_default();
    let (second_infix, suffix) = if placeholders.len() == 3 {
        (
            literals.next().unwrap_or_default(),
            literals.next().unwrap_or_default(),
        )
    } else {
        (String::new(), literals.next().unwrap_or_default())
    };
    Ok(Some(CurrencyAccountingPattern {
        prefix: Cow::Owned(prefix),
        infix: Cow::Owned(infix),
        second_infix: Cow::Owned(second_infix),
        suffix: Cow::Owned(suffix),
        order,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountingPlaceholder {
    Number,
    Currency,
    MinusSign,
}

fn split_negative_subpattern(pattern: &str) -> Option<&str> {
    let mut quoted = false;
    let mut chars = pattern.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if character == '\'' {
            if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                chars.next();
            } else {
                quoted = !quoted;
            }
        } else if character == ';' && !quoted {
            return pattern.get(index + character.len_utf8()..);
        }
    }
    None
}

fn parse_accounting_subpattern(
    pattern: &str,
) -> Result<Vec<AccountingPatternItem>, Box<dyn Error>> {
    fn push_literal(items: &mut Vec<AccountingPatternItem>, character: char) {
        if let Some(AccountingPatternItem::Literal(literal)) = items.last_mut() {
            literal.push(character);
        } else {
            items.push(AccountingPatternItem::Literal(character.to_string()));
        }
    }

    let mut items = Vec::new();
    let mut chars = pattern.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        if character == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                push_literal(&mut items, '\'');
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if quoted {
            push_literal(&mut items, character);
            continue;
        }

        match character {
            '¤' => {
                while chars.peek() == Some(&'¤') {
                    chars.next();
                }
                items.push(AccountingPatternItem::Currency);
            }
            '-' => items.push(AccountingPatternItem::MinusSign),
            '#' | '0' | '@' => {
                while chars
                    .peek()
                    .is_some_and(|next| matches!(next, '#' | '0' | '@' | '.' | ',' | 'E' | '+'))
                {
                    chars.next();
                }
                items.push(AccountingPatternItem::Number);
            }
            _ => push_literal(&mut items, character),
        }
    }
    if quoted {
        return Err("unclosed quote in CLDR accounting pattern".into());
    }
    Ok(items)
}

fn cldr_archive_path() -> Result<PathBuf, Box<dyn Error>> {
    let tag = SourceDataProvider::TESTED_CLDR_TAG;
    let relative_path = format!(
        "github.com/unicode-org/cldr-json/releases/download/{tag}/cldr-{tag}-json-full.zip"
    );
    let cache_root = std::env::var_os("ICU4X_SOURCE_CACHE").map_or_else(
        || std::env::temp_dir().join("icu4x-source-cache"),
        PathBuf::from,
    );
    let archive_path = cache_root.join(relative_path);
    if archive_path.exists() {
        return Ok(archive_path);
    }

    let url = format!(
        "https://github.com/unicode-org/cldr-json/releases/download/{tag}/cldr-{tag}-json-full.zip"
    );
    log::info!("Downloading supplemental number-format data from {url}");
    let response = ureq::get(&url)
        .call()
        .map_err(|error| io::Error::other(format!("failed to download CLDR: {error}")))?;
    let mut reader = response.into_body().into_reader();
    fs::create_dir_all(
        archive_path
            .parent()
            .expect("the CLDR cache path has a parent"),
    )?;
    io::copy(
        &mut reader,
        &mut BufWriter::new(File::create(&archive_path)?),
    )?;
    Ok(archive_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_typed_accounting_patterns() {
        let data = SupplementalNumberData::load().unwrap();
        let en = data
            .accounting_patterns()
            .get(&DataLocale::try_from_str("en").unwrap())
            .unwrap();
        let standard = en.standard.as_ref().unwrap();
        assert_eq!(
            standard.order,
            CurrencyAccountingPlaceholderOrder::CurrencyNumber
        );
        assert_eq!((&*standard.prefix, &*standard.suffix), ("(", ")"));

        let pt = data
            .accounting_patterns()
            .get(&DataLocale::try_from_str("pt").unwrap())
            .unwrap();
        assert!(pt.standard.is_none());
    }
}
