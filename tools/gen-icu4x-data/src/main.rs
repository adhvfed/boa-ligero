#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use std::path::Path;
use std::{error::Error, fs::File};

use icu_provider_export::blob_exporter::BlobExporter;
use icu_provider_export::prelude::*;
use icu_provider_source::{CoverageLevel, SourceDataProvider};

mod cldr;
mod source;
use source::Ecma402SourceProvider;

/// Path to the directory where the exported data lives.
const EXPORT_PATH: &str = "core/icu_provider/data";

const EXPERIMENTAL_MARKERS: &[DataMarkerInfo] = &[
    icu_experimental::dimension::provider::currency::essentials::CurrencyEssentialsV1::INFO,
    icu_experimental::dimension::provider::currency::fractions::CurrencyFractionsV1::INFO,
    icu_experimental::dimension::provider::percent::PercentEssentialsV1::INFO,
    icu_experimental::dimension::provider::units::display_names::UnitsDisplayNamesV1::INFO,
    icu_experimental::dimension::provider::units::essentials::UnitsEssentialsV1::INFO,
];

/// Units accepted by ECMA-402's `IsSanctionedSimpleUnitIdentifier` operation.
const SANCTIONED_SIMPLE_UNITS: &[&str] = &[
    "acre",
    "bit",
    "byte",
    "celsius",
    "centimeter",
    "day",
    "degree",
    "fahrenheit",
    "fluid-ounce",
    "foot",
    "gallon",
    "gigabit",
    "gigabyte",
    "gram",
    "hectare",
    "hour",
    "inch",
    "kilobit",
    "kilobyte",
    "kilogram",
    "kilometer",
    "liter",
    "megabit",
    "megabyte",
    "meter",
    "microsecond",
    "mile",
    "mile-scandinavian",
    "milliliter",
    "millimeter",
    "millisecond",
    "minute",
    "month",
    "nanosecond",
    "ounce",
    "percent",
    "petabyte",
    "pound",
    "second",
    "stone",
    "terabit",
    "terabyte",
    "week",
    "yard",
    "year",
];

fn is_sanctioned_unit_attribute(attributes: &str) -> bool {
    let Some((width, unit)) = attributes.split_once('-') else {
        return false;
    };
    if !matches!(width, "short" | "narrow" | "long") {
        return false;
    }

    let (numerator, denominator) = unit.split_once("-per-").unwrap_or((unit, ""));
    SANCTIONED_SIMPLE_UNITS.binary_search(&numerator).is_ok()
        && (denominator.is_empty() || SANCTIONED_SIMPLE_UNITS.binary_search(&denominator).is_ok())
}

/// List of services used by `Intl` components.
///
/// This must be kept in sync with the list of implemented services for `Intl`.
const SERVICES: &[(&str, &[DataMarkerInfo])] = &[
    ("icu_casemap", icu_casemap::provider::MARKERS),
    ("icu_collator", icu_collator::provider::MARKERS),
    ("icu_datetime", icu_datetime::provider::MARKERS),
    ("icu_time", icu_time::provider::MARKERS),
    ("icu_decimal", icu_decimal::provider::MARKERS),
    ("icu_experimental", EXPERIMENTAL_MARKERS),
    ("icu_list", icu_list::provider::MARKERS),
    ("icu_locale", icu_locale::provider::MARKERS),
    ("icu_normalizer", icu_normalizer::provider::MARKERS),
    ("icu_plurals", icu_plurals::provider::MARKERS),
    ("icu_segmenter", icu_segmenter::provider::MARKERS),
    ("boa_icu_data", boa_icu_data::MARKERS),
];

fn export_for_service(
    service: &str,
    markers: &[DataMarkerInfo],
    provider: &Ecma402SourceProvider<'_>,
    driver: ExportDriver,
) -> Result<(), Box<dyn Error>> {
    log::info!("Generating ICU4X data for service `{service}` with markers: {markers:#?}");

    let export_path = Path::new(EXPORT_PATH);
    let export_file = export_path.join(format!("{service}.postcard"));

    driver.with_markers(markers.iter().copied()).export(
        provider,
        BlobExporter::new_with_sink(Box::new(File::create(export_file)?)),
    )?;

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    simple_logger::SimpleLogger::new()
        .env()
        .with_level(log::LevelFilter::Info)
        .init()?;

    // Removal will throw an error if the directory doesn't exist, hence
    // why we can ignore the error.
    let _unused = std::fs::remove_dir_all(EXPORT_PATH);
    std::fs::create_dir_all(EXPORT_PATH)?;

    let source = SourceDataProvider::new();
    let locales = source
        .locales_for_coverage_levels([CoverageLevel::Modern])?
        .into_iter()
        .map(DataLocaleFamily::with_descendants)
        .chain([
            // test262 assumes the en-US locale does not fallback.
            // Required by https://github.com/tc39/test262/blob/a073f479f80b336256b7fc4e04700c827293e2fe/test/intl402/ListFormat/prototype/resolvedOptions/type.js
            DataLocaleFamily::single(locale!("en-US").into()),
            // test262 uses the Manx locale.
            // Required by https://github.com/tc39/test262/blob/a073f479f80b336256b7fc4e04700c827293e2fe/test/intl402/PluralRules/prototype/resolvedOptions/plural-categories-order.js
            DataLocaleFamily::with_descendants(locale!("gv").into()),
        ]);

    let driver = ExportDriver::new(
        locales,
        DeduplicationStrategy::None.into(),
        LocaleFallbacker::try_new_unstable(&source)?,
    )
    .with_additional_collations([String::from("search*")])
    .with_marker_attributes_filter("units", |attributes| {
        is_sanctioned_unit_attribute(attributes.as_str())
    })
    .with_recommended_segmenter_models();
    let supplemental_number_data = cldr::SupplementalNumberData::load()?;
    let provider = Ecma402SourceProvider::new(&source, &supplemental_number_data);
    for (service, keys) in SERVICES {
        export_for_service(service, keys, &provider, driver.clone())?;
    }

    Ok(())
}
