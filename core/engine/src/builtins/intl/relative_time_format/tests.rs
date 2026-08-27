use crate::{TestAction, run_test_actions};

#[cfg(feature = "intl_bundled")]
#[test]
fn formats_numeric_and_named_relative_times() {
    run_test_actions([
        TestAction::run(
            r#"
            const always = new Intl.RelativeTimeFormat('en');
            const automatic = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
            const numericResult = always.format(-2, 'days');
            const namedResult = automatic.format(-1, 'day');
            "#,
        ),
        TestAction::assert_eq("numericResult === '2 days ago'", true),
        TestAction::assert_eq("namedResult === 'yesterday'", true),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn format_to_parts_carries_the_canonical_unit() {
    run_test_actions([
        TestAction::run(
            r#"
            const formatter = new Intl.RelativeTimeFormat('en');
            const parts = formatter.formatToParts(-2, 'days');
            "#,
        ),
        TestAction::assert_eq("parts.length", 2),
        TestAction::assert_eq("parts[0].type === 'integer'", true),
        TestAction::assert_eq("parts[0].value === '2'", true),
        TestAction::assert_eq("parts[0].unit === 'day'", true),
        TestAction::assert_eq("parts[1].type === 'literal'", true),
        TestAction::assert_eq("parts[1].value === ' days ago'", true),
        TestAction::assert_eq("'unit' in parts[1]", false),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn formats_norwegian_short_style_and_reports_resolved_options() {
    run_test_actions([
        TestAction::run(
            r#"
            const formatter = new Intl.RelativeTimeFormat('nb-NO', { style: 'short' });
            const result = formatter.format(-3, 'hour');
            const options = formatter.resolvedOptions();
            "#,
        ),
        TestAction::assert_eq("result === 'for 3 t siden'", true),
        TestAction::assert_eq("options.locale === 'nb'", true),
        TestAction::assert_eq("options.style === 'short'", true),
        TestAction::assert_eq("options.numeric === 'always'", true),
        TestAction::assert_eq("options.numberingSystem === 'latn'", true),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn bundled_data_covers_every_unit_and_style() {
    run_test_actions([
        TestAction::run(
            r#"
            const units = ['year', 'quarter', 'month', 'week', 'day', 'hour', 'minute', 'second'];
            const styles = ['long', 'short', 'narrow'];
            let covered = 0;
            for (const style of styles) {
              const formatter = new Intl.RelativeTimeFormat('nb-NO', { style });
              for (const unit of units) {
                const formatted = formatter.format(-2, unit);
                const parts = formatter.formatToParts(-2, unit);
                if (formatted.length > 0 &&
                    parts.map(part => part.value).join('') === formatted &&
                    parts.some(part => part.type === 'integer' && part.unit === unit)) {
                  covered++;
                }
              }
            }
            "#,
        ),
        TestAction::assert_eq("covered", 24),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn unsupported_but_well_formed_numbering_system_falls_back() {
    run_test_actions([
        TestAction::run(
            r#"
            const options = new Intl.RelativeTimeFormat('en', {
              numberingSystem: '1234abcd-abc123',
            }).resolvedOptions();
            "#,
        ),
        TestAction::assert_eq("options.numberingSystem === 'latn'", true),
    ]);
}

#[cfg(feature = "intl_bundled")]
#[test]
fn rejects_invalid_calls_options_values_and_units() {
    run_test_actions([
        TestAction::run(
            r#"
            const errors = [];
            for (const callback of [
              () => Intl.RelativeTimeFormat('en'),
              () => new Intl.RelativeTimeFormat('en', { style: 'wide' }),
              () => new Intl.RelativeTimeFormat('en', { numeric: 'sometimes' }),
              () => new Intl.RelativeTimeFormat('en', { numberingSystem: 'latn-ca' }),
              () => new Intl.RelativeTimeFormat('en').format(Infinity, 'day'),
              () => new Intl.RelativeTimeFormat('en').format(1, 'fortnight'),
            ]) {
              try { callback(); errors.push('missing'); }
              catch (error) { errors.push(error.name); }
            }
            "#,
        ),
        TestAction::assert_eq(
            "errors.join(',') === 'TypeError,RangeError,RangeError,RangeError,RangeError,RangeError'",
            true,
        ),
    ]);
}
