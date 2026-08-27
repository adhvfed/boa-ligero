use std::{cell::Cell, fmt, str::FromStr};

use boa_gc::{Finalize, Trace};
use fixed_decimal::{Decimal, FloatPrecision, Sign};
use icu_decimal::{
    DecimalFormatter, DecimalFormatterPreferences, options::DecimalFormatterOptions,
    preferences::NumberingSystem, provider::DecimalDigitsV1,
};
use icu_experimental::relativetime::{
    RelativeTimeFormatter as NativeRelativeTimeFormatter, RelativeTimeFormatterOptions,
    RelativeTimeFormatterPreferences, options::Numeric as NativeNumeric,
    provider::LongSecondRelativeV1,
};
use icu_locale::{
    LanguageIdentifier, Locale,
    extensions::unicode::{self, Value},
};
use icu_provider::{
    DataMarker, DataMarkerAttributes, DynamicDataProvider, buf::BufferMarker,
    prelude::icu_locale_core::preferences::LocalePreferences,
};
use writeable::Writeable;

use crate::{
    Context, JsArgs, JsData, JsNativeError, JsObject, JsResult, JsString, JsSymbol, JsValue,
    builtins::{
        BuiltInBuilder, BuiltInConstructor, BuiltInObject, IntrinsicObject,
        options::{OptionType, ParsableOptionType, get_option},
    },
    context::{
        icu::IntlProvider,
        intrinsics::{Intrinsics, StandardConstructor, StandardConstructors},
    },
    js_string,
    object::{ObjectInitializer, internal_methods::get_prototype_from_constructor},
    property::Attribute,
    realm::Realm,
    string::StaticJsStrings,
};

use super::{
    Service, ServicePreferences,
    locale::{
        canonicalize_locale_list, filter_locales, resolve_locale, validate_extension_attribute,
    },
    options::{IntlOptions, coerce_options_to_object},
    parts::{FormattedPart, PartsCollector, UnmarkedStyle, relative_time_parts_into_js_array},
};

#[cfg(test)]
mod tests;

#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
enum RelativeTimeStyle {
    #[default]
    Long,
    Short,
    Narrow,
}

impl RelativeTimeStyle {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
            Self::Narrow => "narrow",
        }
    }
}

impl FromStr for RelativeTimeStyle {
    type Err = RelativeTimeOptionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "long" => Ok(Self::Long),
            "short" => Ok(Self::Short),
            "narrow" => Ok(Self::Narrow),
            _ => Err(RelativeTimeOptionError("style")),
        }
    }
}

impl ParsableOptionType for RelativeTimeStyle {}

#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
enum RelativeTimeNumeric {
    #[default]
    Always,
    Auto,
}

impl RelativeTimeNumeric {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Auto => "auto",
        }
    }

    const fn to_native(self) -> NativeNumeric {
        match self {
            Self::Always => NativeNumeric::Always,
            Self::Auto => NativeNumeric::Auto,
        }
    }
}

impl FromStr for RelativeTimeNumeric {
    type Err = RelativeTimeOptionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "always" => Ok(Self::Always),
            "auto" => Ok(Self::Auto),
            _ => Err(RelativeTimeOptionError("numeric")),
        }
    }
}

impl ParsableOptionType for RelativeTimeNumeric {}

/// ECMA-402 accepts every syntactically valid Unicode locale type for the
/// `numberingSystem` option, even when the provider does not implement it.
/// ICU4X's `NumberingSystem` can represent only known systems, so retain the
/// supported value and otherwise let locale resolution fall back normally.
struct NumberingSystemOption(Option<NumberingSystem>);

impl OptionType for NumberingSystemOption {
    fn from_value(value: JsValue, context: &mut Context) -> JsResult<Self> {
        let source = value.to_string(context)?.to_std_string_escaped();
        let well_formed = source.split('-').all(|subtag| {
            (3..=8).contains(&subtag.len())
                && subtag.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        let unicode_value = well_formed
            .then(|| Value::try_from_str(&source).ok())
            .flatten();
        let Some(unicode_value) = unicode_value else {
            return Err(JsNativeError::range()
                .with_message(format!("provided numbering system `{source}` is invalid"))
                .into());
        };
        Ok(Self(NumberingSystem::try_from(unicode_value).ok()))
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum RelativeTimeUnit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
}

impl RelativeTimeUnit {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Quarter => "quarter",
            Self::Month => "month",
            Self::Week => "week",
            Self::Day => "day",
            Self::Hour => "hour",
            Self::Minute => "minute",
            Self::Second => "second",
        }
    }

    fn from_value(value: &JsValue, context: &mut Context) -> JsResult<Self> {
        let value = value.to_string(context)?.to_std_string_escaped();
        match value.as_str() {
            "year" | "years" => Ok(Self::Year),
            "quarter" | "quarters" => Ok(Self::Quarter),
            "month" | "months" => Ok(Self::Month),
            "week" | "weeks" => Ok(Self::Week),
            "day" | "days" => Ok(Self::Day),
            "hour" | "hours" => Ok(Self::Hour),
            "minute" | "minutes" => Ok(Self::Minute),
            "second" | "seconds" => Ok(Self::Second),
            _ => Err(JsNativeError::range()
                .with_message(format!("provided relative time unit `{value}` is invalid"))
                .into()),
        }
    }
}

#[derive(Debug)]
struct RelativeTimeOptionError(&'static str);

impl fmt::Display for RelativeTimeOptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provided string was not a valid {} option",
            self.0
        )
    }
}

impl ServicePreferences for RelativeTimeFormatterPreferences {
    fn validate(&mut self, _id: &LanguageIdentifier, provider: &IntlProvider) {
        self.numbering_system = self.numbering_system.take().filter(|numbering_system| {
            let attributes = DataMarkerAttributes::from_str_or_panic(numbering_system.as_str());
            validate_extension_attribute::<DecimalDigitsV1>(attributes, provider)
        });
    }

    impl_service_preferences!(numbering_system);
}

#[derive(Debug, Trace, Finalize, JsData)]
#[boa_gc(unsafe_empty_trace)]
pub(crate) struct RelativeTimeFormat {
    locale: Locale,
    numbering_system: NumberingSystem,
    style: RelativeTimeStyle,
    numeric: RelativeTimeNumeric,
}

impl Service for RelativeTimeFormat {
    type LangMarker = LongSecondRelativeV1;
    type Preferences = RelativeTimeFormatterPreferences;
}

impl IntrinsicObject for RelativeTimeFormat {
    fn init(realm: &Realm) {
        BuiltInBuilder::from_standard_constructor::<Self>(realm)
            .static_method(
                Self::supported_locales_of,
                js_string!("supportedLocalesOf"),
                1,
            )
            .property(
                JsSymbol::to_string_tag(),
                js_string!("Intl.RelativeTimeFormat"),
                Attribute::CONFIGURABLE,
            )
            .method(Self::resolved_options, js_string!("resolvedOptions"), 0)
            .method(Self::format, js_string!("format"), 2)
            .method(Self::format_to_parts, js_string!("formatToParts"), 2)
            .build();
    }

    fn get(intrinsics: &Intrinsics) -> JsObject {
        Self::STANDARD_CONSTRUCTOR(intrinsics.constructors()).constructor()
    }
}

impl BuiltInObject for RelativeTimeFormat {
    const NAME: JsString = StaticJsStrings::RELATIVE_TIME_FORMAT;
}

impl BuiltInConstructor for RelativeTimeFormat {
    const CONSTRUCTOR_ARGUMENTS: usize = 0;
    const PROTOTYPE_STORAGE_SLOTS: usize = 4;
    const CONSTRUCTOR_STORAGE_SLOTS: usize = 1;

    const STANDARD_CONSTRUCTOR: fn(&StandardConstructors) -> &StandardConstructor =
        StandardConstructors::relative_time_format;

    fn constructor(
        new_target: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        if new_target.is_undefined() {
            return Err(JsNativeError::typ()
                .with_message("cannot call `Intl.RelativeTimeFormat` constructor without `new`")
                .into());
        }

        let requested_locales = canonicalize_locale_list(args.get_or_undefined(0), context)?;
        let options = coerce_options_to_object(args.get_or_undefined(1), context)?;
        let matcher =
            get_option(&options, js_string!("localeMatcher"), context)?.unwrap_or_default();
        let numbering_system =
            get_option::<NumberingSystemOption>(&options, js_string!("numberingSystem"), context)?
                .and_then(|option| option.0);
        let style: RelativeTimeStyle =
            get_option(&options, js_string!("style"), context)?.unwrap_or_default();
        let numeric: RelativeTimeNumeric =
            get_option(&options, js_string!("numeric"), context)?.unwrap_or_default();

        let mut intl_options = IntlOptions {
            matcher,
            preferences: {
                let mut preferences = RelativeTimeFormatterPreferences::default();
                preferences.numbering_system = numbering_system;
                preferences
            },
        };
        let locale = resolve_locale::<Self>(
            requested_locales,
            &mut intl_options,
            context.intl_provider(),
        )?;

        let mut native_options = RelativeTimeFormatterOptions::default();
        native_options.numeric = numeric.to_native();
        let preferences = intl_options.preferences;
        let numbering_system = resolved_numbering_system(
            context.intl_provider().erased_provider(),
            preferences,
            native_options,
        )?;

        let prototype = get_prototype_from_constructor(
            new_target,
            StandardConstructors::relative_time_format,
            context,
        )?;
        Ok(JsObject::from_proto_and_data_with_shared_shape(
            context.root_shape(),
            prototype,
            Self {
                locale,
                numbering_system,
                style,
                numeric,
            },
        )
        .into())
    }
}

impl RelativeTimeFormat {
    fn supported_locales_of(
        _: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let requested = canonicalize_locale_list(args.get_or_undefined(0), context)?;
        filter_locales::<Self>(requested, args.get_or_undefined(1), context).map(JsValue::from)
    }

    fn format(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let object = require_relative_time_format(this, "format")?;
        let relative_time = object
            .downcast_ref::<Self>()
            .expect("relative time format type was checked");
        let value = finite_decimal(args.get_or_undefined(0), context)?;
        let unit = RelativeTimeUnit::from_value(args.get_or_undefined(1), context)?;
        let formatter = relative_time.formatter(unit, context)?;
        Ok(js_string!(formatter.format(value).write_to_string()).into())
    }

    fn format_to_parts(
        this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let object = require_relative_time_format(this, "formatToParts")?;
        let relative_time = object
            .downcast_ref::<Self>()
            .expect("relative time format type was checked");
        let value = finite_decimal(args.get_or_undefined(0), context)?;
        let unit = RelativeTimeUnit::from_value(args.get_or_undefined(1), context)?;
        let formatter = relative_time.formatter(unit, context)?;
        let relative_time_text = formatter
            .format(value.clone())
            .write_to_string()
            .into_owned();

        // ICU4X's experimental relative-time Writeable currently marks the
        // whole numeric phrase as a literal. ECMA-402 instead preserves the
        // decimal formatter's integer/group/decimal/fraction parts and adds
        // the canonical unit to each numeric part. Reuse the same locale and
        // numbering-system preferences as the relative-time formatter, then
        // splice those structured number parts into the localized phrase.
        let decimal = DecimalFormatter::try_new_with_buffer_provider(
            context.intl_provider().erased_provider(),
            DecimalFormatterPreferences::from(&relative_time.locale),
            DecimalFormatterOptions::default(),
        )
        .map_err(|error| JsNativeError::typ().with_message(error.to_string()))?;
        let mut absolute = value;
        absolute.set_sign(Sign::None);
        let formatted_number = decimal.format(&absolute);
        let number = formatted_number.write_to_string().into_owned();
        let mut number_parts = PartsCollector::new(UnmarkedStyle::Literal);
        formatted_number
            .write_to_parts(&mut number_parts)
            .map_err(|error| JsNativeError::typ().with_message(error.to_string()))?;

        let parts = if let Some(start) = relative_time_text.find(&number) {
            let end = start + number.len();
            let mut parts = Vec::with_capacity(number_parts.parts.len() + 2);
            if start != 0 {
                parts.push(FormattedPart {
                    kind: "literal",
                    value: relative_time_text[..start].to_owned(),
                });
            }
            parts.extend(number_parts.parts);
            if end != relative_time_text.len() {
                parts.push(FormattedPart {
                    kind: "literal",
                    value: relative_time_text[end..].to_owned(),
                });
            }
            parts
        } else {
            vec![FormattedPart {
                kind: "literal",
                value: relative_time_text,
            }]
        };
        relative_time_parts_into_js_array(parts, unit.as_str(), context)
    }

    fn resolved_options(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let object = require_relative_time_format(this, "resolvedOptions")?;
        let relative_time = object
            .downcast_ref::<Self>()
            .expect("relative time format type was checked");
        let mut options = ObjectInitializer::new(context);
        options.property(
            js_string!("locale"),
            js_string!(relative_time.locale.to_string()),
            Attribute::all(),
        );
        options.property(
            js_string!("style"),
            js_string!(relative_time.style.as_str()),
            Attribute::all(),
        );
        options.property(
            js_string!("numeric"),
            js_string!(relative_time.numeric.as_str()),
            Attribute::all(),
        );
        options.property(
            js_string!("numberingSystem"),
            js_string!(relative_time.numbering_system.as_str()),
            Attribute::all(),
        );
        Ok(options.build().into())
    }

    fn formatter(
        &self,
        unit: RelativeTimeUnit,
        context: &mut Context,
    ) -> JsResult<NativeRelativeTimeFormatter> {
        let preferences = RelativeTimeFormatterPreferences::from(&self.locale);
        let mut options = RelativeTimeFormatterOptions::default();
        options.numeric = self.numeric.to_native();
        new_formatter(
            context.intl_provider().erased_provider(),
            preferences,
            options,
            self.style,
            unit,
        )
    }
}

fn require_relative_time_format(this: &JsValue, method: &str) -> JsResult<JsObject> {
    let object = this.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message(format!(
            "`{method}` can only be called on an `Intl.RelativeTimeFormat` object"
        ))
    })?;
    if !object.is::<RelativeTimeFormat>() {
        return Err(JsNativeError::typ()
            .with_message(format!(
                "`{method}` can only be called on an `Intl.RelativeTimeFormat` object"
            ))
            .into());
    }
    Ok(object)
}

fn finite_decimal(value: &JsValue, context: &mut Context) -> JsResult<Decimal> {
    let value = value.to_number(context)?;
    if !value.is_finite() {
        return Err(JsNativeError::range()
            .with_message("relative time value must be finite")
            .into());
    }
    Decimal::try_from_f64(value, FloatPrecision::RoundTrip).map_err(|error| {
        JsNativeError::range()
            .with_message(error.to_string())
            .into()
    })
}

fn resolved_numbering_system(
    provider: &dyn DynamicDataProvider<BufferMarker>,
    preferences: RelativeTimeFormatterPreferences,
    options: RelativeTimeFormatterOptions,
) -> JsResult<NumberingSystem> {
    struct RequestInspector<'a> {
        inner: &'a dyn DynamicDataProvider<BufferMarker>,
        numbering_system: Cell<Option<Box<DataMarkerAttributes>>>,
    }

    impl DynamicDataProvider<BufferMarker> for RequestInspector<'_> {
        fn load_data(
            &self,
            marker: icu_provider::DataMarkerInfo,
            request: icu_provider::DataRequest<'_>,
        ) -> Result<icu_provider::DataResponse<BufferMarker>, icu_provider::DataError> {
            if marker.id == DecimalDigitsV1::INFO.id {
                self.numbering_system
                    .set(Some(request.id.marker_attributes.to_owned()));
            }
            self.inner.load_data(marker, request)
        }
    }

    let inspector = RequestInspector {
        inner: provider,
        numbering_system: Cell::new(None),
    };
    NativeRelativeTimeFormatter::try_new_long_second_with_buffer_provider(
        &inspector,
        preferences,
        options,
    )
    .map_err(|error| JsNativeError::typ().with_message(error.to_string()))?;

    let numbering_system = inspector
        .numbering_system
        .into_inner()
        .and_then(|attributes| Value::try_from_str(&attributes).ok())
        .and_then(|value| NumberingSystem::try_from(value).ok())
        .ok_or_else(|| {
            JsNativeError::typ()
                .with_message("could not obtain resolved numbering system from Intl provider")
        })?;
    Ok(numbering_system)
}

fn new_formatter(
    provider: &dyn DynamicDataProvider<BufferMarker>,
    preferences: RelativeTimeFormatterPreferences,
    options: RelativeTimeFormatterOptions,
    style: RelativeTimeStyle,
    unit: RelativeTimeUnit,
) -> JsResult<NativeRelativeTimeFormatter> {
    let result = match (style, unit) {
        (RelativeTimeStyle::Long, RelativeTimeUnit::Year) => {
            NativeRelativeTimeFormatter::try_new_long_year_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Long, RelativeTimeUnit::Quarter) => {
            NativeRelativeTimeFormatter::try_new_long_quarter_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Long, RelativeTimeUnit::Month) => {
            NativeRelativeTimeFormatter::try_new_long_month_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Long, RelativeTimeUnit::Week) => {
            NativeRelativeTimeFormatter::try_new_long_week_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Long, RelativeTimeUnit::Day) => {
            NativeRelativeTimeFormatter::try_new_long_day_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Long, RelativeTimeUnit::Hour) => {
            NativeRelativeTimeFormatter::try_new_long_hour_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Long, RelativeTimeUnit::Minute) => {
            NativeRelativeTimeFormatter::try_new_long_minute_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Long, RelativeTimeUnit::Second) => {
            NativeRelativeTimeFormatter::try_new_long_second_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Year) => {
            NativeRelativeTimeFormatter::try_new_short_year_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Quarter) => {
            NativeRelativeTimeFormatter::try_new_short_quarter_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Month) => {
            NativeRelativeTimeFormatter::try_new_short_month_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Week) => {
            NativeRelativeTimeFormatter::try_new_short_week_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Day) => {
            NativeRelativeTimeFormatter::try_new_short_day_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Hour) => {
            NativeRelativeTimeFormatter::try_new_short_hour_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Minute) => {
            NativeRelativeTimeFormatter::try_new_short_minute_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Short, RelativeTimeUnit::Second) => {
            NativeRelativeTimeFormatter::try_new_short_second_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Year) => {
            NativeRelativeTimeFormatter::try_new_narrow_year_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Quarter) => {
            NativeRelativeTimeFormatter::try_new_narrow_quarter_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Month) => {
            NativeRelativeTimeFormatter::try_new_narrow_month_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Week) => {
            NativeRelativeTimeFormatter::try_new_narrow_week_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Day) => {
            NativeRelativeTimeFormatter::try_new_narrow_day_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Hour) => {
            NativeRelativeTimeFormatter::try_new_narrow_hour_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Minute) => {
            NativeRelativeTimeFormatter::try_new_narrow_minute_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
        (RelativeTimeStyle::Narrow, RelativeTimeUnit::Second) => {
            NativeRelativeTimeFormatter::try_new_narrow_second_with_buffer_provider(
                provider,
                preferences,
                options,
            )
        }
    };
    result.map_err(|error| JsNativeError::typ().with_message(error.to_string()).into())
}
