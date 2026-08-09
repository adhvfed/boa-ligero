//! ECMA-402 additions to ICU4X's source-data inventory.

use std::collections::BTreeSet;

use boa_icu_data::{BoaCurrencyAccountingPatternsV1, BoaNumberSpecialSymbolsV1};
use icu_decimal::provider::DecimalDigitsV1;
use icu_provider::{
    DataError, DataErrorKind, DataIdentifierCow, DataMarker, DataMarkerAttributes, DataMarkerInfo,
    DataPayload, DataRequest, DataResponse, DataResponseMetadata, DynamicDataProvider,
    IterableDynamicDataProvider,
    dynutil::UpcastDataPayload,
    export::{ExportMarker, ExportableProvider},
};
use icu_provider_source::SourceDataProvider;

use crate::cldr::SupplementalNumberData;

/// ECMA-402 simple digit mappings not guaranteed to appear in ICU4X's locale-derived inventory.
///
/// All systems except `hanidec` use ten consecutive Unicode scalar values.
const SIMPLE_DIGIT_ZEROES: &[(&str, char)] = &[
    ("adlm", '\u{1E950}'),
    ("ahom", '\u{11730}'),
    ("arab", '\u{660}'),
    ("arabext", '\u{6F0}'),
    ("bali", '\u{1B50}'),
    ("beng", '\u{9E6}'),
    ("bhks", '\u{11C50}'),
    ("brah", '\u{11066}'),
    ("cakm", '\u{11136}'),
    ("cham", '\u{AA50}'),
    ("deva", '\u{966}'),
    ("diak", '\u{11950}'),
    ("fullwide", '\u{FF10}'),
    ("gara", '\u{10D40}'),
    ("gong", '\u{11DA0}'),
    ("gonm", '\u{11D50}'),
    ("gujr", '\u{AE6}'),
    ("gukh", '\u{16130}'),
    ("guru", '\u{A66}'),
    ("hmng", '\u{16B50}'),
    ("hmnp", '\u{1E140}'),
    ("java", '\u{A9D0}'),
    ("kali", '\u{A900}'),
    ("kawi", '\u{11F50}'),
    ("khmr", '\u{17E0}'),
    ("knda", '\u{CE6}'),
    ("krai", '\u{16D70}'),
    ("lana", '\u{1A80}'),
    ("lanatham", '\u{1A90}'),
    ("laoo", '\u{ED0}'),
    ("latn", '0'),
    ("lepc", '\u{1C40}'),
    ("limb", '\u{1946}'),
    ("mathbold", '\u{1D7CE}'),
    ("mathdbl", '\u{1D7D8}'),
    ("mathmono", '\u{1D7F6}'),
    ("mathsanb", '\u{1D7EC}'),
    ("mathsans", '\u{1D7E2}'),
    ("mlym", '\u{D66}'),
    ("modi", '\u{11650}'),
    ("mong", '\u{1810}'),
    ("mroo", '\u{16A60}'),
    ("mtei", '\u{ABF0}'),
    ("mymr", '\u{1040}'),
    ("mymrepka", '\u{116DA}'),
    ("mymrpao", '\u{116D0}'),
    ("mymrshan", '\u{1090}'),
    ("mymrtlng", '\u{A9F0}'),
    ("nagm", '\u{1E4F0}'),
    ("newa", '\u{11450}'),
    ("nkoo", '\u{7C0}'),
    ("olck", '\u{1C50}'),
    ("onao", '\u{1E5F1}'),
    ("orya", '\u{B66}'),
    ("osma", '\u{104A0}'),
    ("outlined", '\u{1CCF0}'),
    ("rohg", '\u{10D30}'),
    ("saur", '\u{A8D0}'),
    ("segment", '\u{1FBF0}'),
    ("shrd", '\u{111D0}'),
    ("sind", '\u{112F0}'),
    ("sinh", '\u{DE6}'),
    ("sora", '\u{110F0}'),
    ("sund", '\u{1BB0}'),
    ("sunu", '\u{11BF0}'),
    ("takr", '\u{116C0}'),
    ("talu", '\u{19D0}'),
    ("tamldec", '\u{BE6}'),
    ("telu", '\u{C66}'),
    ("thai", '\u{E50}'),
    ("tibt", '\u{F20}'),
    ("tirh", '\u{114D0}'),
    ("tnsa", '\u{16AC0}'),
    ("tols", '\u{11DE0}'),
    ("vaii", '\u{A620}'),
    ("wara", '\u{118E0}'),
    ("wcho", '\u{1E2F0}'),
];

const HANIDEC_DIGITS: [char; 10] = ['〇', '一', '二', '三', '四', '五', '六', '七', '八', '九'];

fn simple_digits(identifier: &str) -> Option<[char; 10]> {
    if identifier == "hanidec" {
        return Some(HANIDEC_DIGITS);
    }
    let (_, zero) = SIMPLE_DIGIT_ZEROES
        .binary_search_by_key(&identifier, |(name, _)| *name)
        .ok()
        .and_then(|index| SIMPLE_DIGIT_ZEROES.get(index))?;
    Some(std::array::from_fn(|offset| {
        let offset = u32::try_from(offset).expect("a decimal digit offset fits in u32");
        char::from_u32(*zero as u32 + offset)
            .expect("simple numbering systems contain consecutive Unicode scalar values")
    }))
}

pub(crate) struct Ecma402SourceProvider<'a> {
    inner: &'a SourceDataProvider,
    supplemental_numbers: &'a SupplementalNumberData,
}

impl<'a> Ecma402SourceProvider<'a> {
    pub(crate) const fn new(
        inner: &'a SourceDataProvider,
        supplemental_numbers: &'a SupplementalNumberData,
    ) -> Self {
        Self {
            inner,
            supplemental_numbers,
        }
    }
}

impl DynamicDataProvider<ExportMarker> for Ecma402SourceProvider<'_> {
    fn load_data(
        &self,
        marker: DataMarkerInfo,
        request: DataRequest<'_>,
    ) -> Result<DataResponse<ExportMarker>, DataError> {
        if marker.id == BoaNumberSpecialSymbolsV1::INFO.id {
            let Some(symbols) = self
                .supplemental_numbers
                .special_symbols()
                .get(request.id.locale)
            else {
                return Err(DataErrorKind::IdentifierNotFound.with_req(marker, request));
            };
            let payload = DataPayload::<BoaNumberSpecialSymbolsV1>::from_owned(symbols.clone());
            return Ok(DataResponse {
                metadata: DataResponseMetadata::default(),
                payload: <ExportMarker as UpcastDataPayload<BoaNumberSpecialSymbolsV1>>::upcast(
                    payload,
                ),
            });
        }
        if marker.id == BoaCurrencyAccountingPatternsV1::INFO.id {
            let Some(patterns) = self
                .supplemental_numbers
                .accounting_patterns()
                .get(request.id.locale)
            else {
                return Err(DataErrorKind::IdentifierNotFound.with_req(marker, request));
            };
            let payload =
                DataPayload::<BoaCurrencyAccountingPatternsV1>::from_owned(patterns.clone());
            return Ok(DataResponse {
                metadata: DataResponseMetadata::default(),
                payload:
                    <ExportMarker as UpcastDataPayload<BoaCurrencyAccountingPatternsV1>>::upcast(
                        payload,
                    ),
            });
        }

        match self.inner.load_data(marker, request) {
            Err(error)
                if error.kind == DataErrorKind::IdentifierNotFound
                    && marker.id == DecimalDigitsV1::INFO.id =>
            {
                let Some(digits) = simple_digits(request.id.marker_attributes.as_str()) else {
                    return Err(error);
                };
                let payload = DataPayload::<DecimalDigitsV1>::from_owned(digits);
                Ok(DataResponse {
                    metadata: DataResponseMetadata::default(),
                    payload: <ExportMarker as UpcastDataPayload<DecimalDigitsV1>>::upcast(payload),
                })
            }
            result => result,
        }
    }
}

impl IterableDynamicDataProvider<ExportMarker> for Ecma402SourceProvider<'_> {
    fn iter_ids_for_marker(
        &self,
        marker: DataMarkerInfo,
    ) -> Result<BTreeSet<DataIdentifierCow<'_>>, DataError> {
        if marker.id == BoaNumberSpecialSymbolsV1::INFO.id {
            return Ok(self
                .supplemental_numbers
                .special_symbols()
                .keys()
                .copied()
                .map(DataIdentifierCow::from_locale)
                .collect());
        }
        if marker.id == BoaCurrencyAccountingPatternsV1::INFO.id {
            return Ok(self
                .supplemental_numbers
                .accounting_patterns()
                .keys()
                .copied()
                .map(DataIdentifierCow::from_locale)
                .collect());
        }

        let mut identifiers = self.inner.iter_ids_for_marker(marker)?;
        if marker.id == DecimalDigitsV1::INFO.id {
            identifiers.extend(
                SIMPLE_DIGIT_ZEROES
                    .iter()
                    .map(|(identifier, _)| *identifier)
                    .chain(["hanidec"])
                    .map(|identifier| {
                        DataIdentifierCow::from_marker_attributes_owned(
                            DataMarkerAttributes::try_from_str(identifier)
                                .expect("numbering system identifiers are valid ICU attributes")
                                .to_owned(),
                        )
                    }),
            );
        }
        Ok(identifiers)
    }
}

impl ExportableProvider for Ecma402SourceProvider<'_> {
    fn supported_markers(&self) -> BTreeSet<DataMarkerInfo> {
        let mut markers = self.inner.supported_markers();
        markers.insert(BoaNumberSpecialSymbolsV1::INFO);
        markers.insert(BoaCurrencyAccountingPatternsV1::INFO);
        markers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_explicit_decimal_digits() {
        let source = SourceDataProvider::new();
        let supplemental = SupplementalNumberData::load().unwrap();
        let provider = Ecma402SourceProvider::new(&source, &supplemental);
        let identifiers = provider.iter_ids_for_marker(DecimalDigitsV1::INFO).unwrap();
        assert!(
            identifiers
                .iter()
                .any(|id| id.marker_attributes.as_str() == "ahom")
        );
        provider
            .load_data(
                DecimalDigitsV1::INFO,
                DataRequest {
                    id: icu_provider::DataIdentifierBorrowed::for_marker_attributes(
                        DataMarkerAttributes::try_from_str("ahom").unwrap(),
                    ),
                    ..DataRequest::default()
                },
            )
            .unwrap();
    }
}
