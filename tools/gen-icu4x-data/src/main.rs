#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use std::path::{Path, PathBuf};
use std::{error::Error, fs::File};

use clap::Parser;
use icu_provider_export::blob_exporter::BlobExporter;
use icu_provider_export::prelude::*;
use icu_provider_source::{CoverageLevel, SourceDataProvider};

mod cldr;
mod source;
use source::Ecma402SourceProvider;

/// Path to the directory where the exported data lives.
const EXPORT_PATH: &str = "core/icu_provider/data";

#[derive(Debug, Parser)]
struct Args {
    /// Write generated blobs to a staging directory before installing them.
    #[arg(long, default_value = EXPORT_PATH)]
    output_dir: PathBuf,
    /// Regenerate only one service blob, preserving all other generated data.
    #[arg(long)]
    service: Option<String>,
}

use boa_icu_data::EXPERIMENTAL_MARKERS;

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
    ("icu_calendar", icu_calendar::provider::MARKERS),
    ("icu_collator", icu_collator::provider::MARKERS),
    ("icu_datetime", icu_datetime::provider::MARKERS),
    ("icu_time", icu_time::provider::MARKERS),
    ("icu_decimal", icu_decimal::provider::MARKERS),
    ("icu_experimental", EXPERIMENTAL_MARKERS),
    ("icu_list", icu_list::provider::MARKERS),
    ("icu_locale", icu_locale::provider::MARKERS),
    (
        "icu_locale_fallback",
        icu_locale_fallback::provider::MARKERS,
    ),
    ("icu_normalizer", icu_normalizer::provider::MARKERS),
    ("icu_plurals", icu_plurals::provider::MARKERS),
    ("icu_segmenter", icu_segmenter::provider::MARKERS),
    ("boa_icu_data", boa_icu_data::MARKERS),
];

fn export_for_service(
    service: &str,
    export_path: &Path,
    markers: &[DataMarkerInfo],
    provider: &Ecma402SourceProvider<'_>,
    driver: ExportDriver,
) -> Result<(), Box<dyn Error>> {
    log::info!("Generating ICU4X data for service `{service}` with markers: {markers:#?}");

    let export_file = export_path.join(format!("{service}.postcard"));

    driver.with_markers(markers.iter().copied()).export(
        provider,
        BlobExporter::new_with_sink(Box::new(File::create(export_file)?)),
    )?;

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    if let Some(service) = &args.service
        && !SERVICES.iter().any(|(candidate, _)| candidate == service)
    {
        return Err(format!("unknown ICU data service `{service}`").into());
    }

    simple_logger::SimpleLogger::new()
        .env()
        .with_level(log::LevelFilter::Info)
        .init()?;

    std::fs::create_dir_all(&args.output_dir)?;

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
            // ICU's pt-PT number-range separator is more specific than the
            // generic Portuguese CLDR range pattern.
            DataLocaleFamily::single(locale!("pt-PT").into()),
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
        if args
            .service
            .as_deref()
            .is_some_and(|requested| requested != *service)
        {
            continue;
        }
        export_for_service(service, &args.output_dir, keys, &provider, driver.clone())?;
    }

    Ok(())
}
