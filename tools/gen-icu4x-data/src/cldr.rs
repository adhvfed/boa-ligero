//! CLDR data that ICU4X's source provider does not expose yet.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    error::Error,
    fs::{self, File},
    io::{self, BufWriter},
    path::PathBuf,
};

use boa_icu_data::NumberSpecialSymbols;
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
}

/// Supplemental number-format data indexed by its exact CLDR locale.
pub(crate) struct SupplementalNumberData {
    special_symbols: HashMap<DataLocale, NumberSpecialSymbols<'static>>,
}

impl SupplementalNumberData {
    pub(crate) fn load() -> Result<Self, Box<dyn Error>> {
        let archive_path = cldr_archive_path()?;
        let mut archive = ZipArchive::new(File::open(archive_path)?)?;
        let mut special_symbols = HashMap::new();

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
        }

        Ok(Self { special_symbols })
    }

    pub(crate) const fn special_symbols(
        &self,
    ) -> &HashMap<DataLocale, NumberSpecialSymbols<'static>> {
        &self.special_symbols
    }
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
