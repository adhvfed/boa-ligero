# boa_icu_provider

`gen-icu4x-data` generates the [ICU4X](https://github.com/unicode-org/icu4x) data provider
for `boa_icu_provider`.

## Datagen

To regenerate the data:

```bash
$ cargo run --release -p gen-icu4x-data --bin gen-icu4x-data
```

For a dependency migration, generate into a staging directory first:

```bash
cargo run --release -p gen-icu4x-data --bin gen-icu4x-data -- --output-dir /tmp/boa-icu-data
```

Install the complete generated set together, then run the bundled-provider and
engine Intl tests. Experimental marker schemas can change between ICU releases;
old blobs must not be mixed with new consumers. The experimental marker catalog
is shared through `boa_icu_data::EXPERIMENTAL_MARKERS`. ICU4X 2.4 exposes locale
fallback markers through a separate `icu_locale_fallback` service; include its
blob alongside the locale data. The separate `icu_calendar` blob supplies
calendar preferences and calendar-specific data required by date formatting.

Boa's supplemental unit marker includes all ECMA-402 sanctioned units. ICU4X 2.4's
categorized inventory omits some required categories, including digital units
and temperature, so the generator reads the complete CLDR inventory and uses
ICU's pattern parser and plural-pattern representation.
