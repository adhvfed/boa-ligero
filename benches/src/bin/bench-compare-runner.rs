//! Boa-side counterpart to runner.mjs.
//!
//! Loads a JS script, evaluates it once, then calls `main()` `runs` times
//! after `warmup` warmup runs, and prints `elapsed_ns=<N>` on stdout.
//!
//! This mirrors the timing protocol in `tools/bench-compare/runner.mjs` so
//! Boa and node/bun numbers are directly comparable. When built with the
//! `jit` feature, `jit` mode reports a warm JIT measurement and a separate
//! first-call measurement that includes native compilation. The `osr-cold`
//! mode performs exactly one production-threshold call in a fresh process
//! context, with no earlier threshold override or native compilation. Both JIT
//! modes can write a bounded diagnostic snapshot after timing via
//! `--jit-diagnostics-out <path>`. The optional
//! `--jit-diagnostic-record-limit <count>` applies the same requested bound to
//! every detailed record kind; Boa still enforces its engine-owned hard cap.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use std::{env, fs, path::Path, process, time::Instant};

#[cfg(feature = "jit")]
use boa_engine::jit::JIT_DIAGNOSTIC_SCHEMA_VERSION;
use boa_engine::{
    Context, JsValue, Source, js_string, optimizer::OptimizerOptions, script::Script,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunnerMode {
    Interpreter,
    Jit,
    OsrCold,
}

impl RunnerMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "interp" => Ok(Self::Interpreter),
            "jit" => Ok(Self::Jit),
            "osr-cold" => Ok(Self::OsrCold),
            other => Err(format!(
                "unknown runner mode `{other}`; expected `interp`, `jit`, or `osr-cold`"
            )),
        }
    }
}

fn validate_mode_invocation(mode: RunnerMode, runs: usize, warmup: usize) -> Result<(), String> {
    if mode == RunnerMode::OsrCold && (runs != 1 || warmup != 0) {
        return Err("osr-cold mode requires exactly `runs=1` and `warmup=0`".to_owned());
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(2);
    }

    let script_path = &args[1];
    let runs: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let warmup: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    let (mode, diagnostic_options) = parse_runner_options(args.get(4..).unwrap_or_default())
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            print_usage();
            process::exit(2);
        });
    validate_mode_invocation(mode, runs, warmup).unwrap_or_else(|error| {
        eprintln!("{error}");
        print_usage();
        process::exit(2);
    });

    let code = fs::read_to_string(Path::new(script_path)).expect("read script");

    match mode {
        RunnerMode::Interpreter if diagnostic_options == JitDiagnosticOptions::default() => {
            run_interpreter(script_path, &code, runs, warmup);
        }
        RunnerMode::Interpreter => {
            eprintln!("--jit-diagnostics-out is only valid in a JIT mode");
            process::exit(2);
        }
        RunnerMode::Jit => {
            #[cfg(feature = "jit")]
            run_jit(
                script_path,
                &code,
                runs,
                warmup,
                diagnostic_options.output.map(Path::new),
                diagnostic_options.record_limit,
            );
            #[cfg(not(feature = "jit"))]
            {
                eprintln!("jit mode requires building bench-compare-runner with `--features jit`");
                process::exit(2);
            }
        }
        RunnerMode::OsrCold => {
            #[cfg(feature = "jit")]
            run_osr_cold(
                script_path,
                &code,
                diagnostic_options.output.map(Path::new),
                diagnostic_options.record_limit,
            );
            #[cfg(not(feature = "jit"))]
            {
                eprintln!(
                    "osr-cold mode requires building bench-compare-runner with `--features jit`"
                );
                process::exit(2);
            }
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: runner-boa <script.js> [runs] [warmup] [interp|jit|osr-cold] \
         [--jit-diagnostics-out <path>] \
         [--jit-diagnostic-record-limit <count>]"
    );
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct JitDiagnosticOptions<'a> {
    output: Option<&'a str>,
    record_limit: Option<usize>,
}

fn parse_runner_options(args: &[String]) -> Result<(RunnerMode, JitDiagnosticOptions<'_>), String> {
    let (mode, diagnostic_args) = match args.split_first() {
        Some((mode, rest)) if !mode.starts_with("--") => (RunnerMode::parse(mode)?, rest),
        _ => (RunnerMode::Interpreter, args),
    };
    let diagnostic_options = parse_jit_diagnostic_options(diagnostic_args)?;
    Ok((mode, diagnostic_options))
}

fn parse_jit_diagnostic_options(args: &[String]) -> Result<JitDiagnosticOptions<'_>, String> {
    let mut options = JitDiagnosticOptions::default();
    let mut remaining = args;

    while let Some((flag, rest)) = remaining.split_first() {
        let Some((value, tail)) = rest.split_first() else {
            return Err(format!("{flag} requires a value"));
        };

        match flag.as_str() {
            "--jit-diagnostics-out" if options.output.is_none() && !value.is_empty() => {
                options.output = Some(value);
            }
            "--jit-diagnostics-out" if options.output.is_some() => {
                return Err("--jit-diagnostics-out may be specified only once".to_owned());
            }
            "--jit-diagnostics-out" => {
                return Err("--jit-diagnostics-out requires a non-empty path".to_owned());
            }
            "--jit-diagnostic-record-limit" if options.record_limit.is_none() => {
                options.record_limit = Some(value.parse().map_err(|_| {
                    "--jit-diagnostic-record-limit requires an unsigned integer".to_owned()
                })?);
            }
            "--jit-diagnostic-record-limit" => {
                return Err("--jit-diagnostic-record-limit may be specified only once".to_owned());
            }
            _ => return Err(format!("unknown runner option `{flag}`")),
        }

        remaining = tail;
    }

    if options.record_limit.is_some() && options.output.is_none() {
        return Err("--jit-diagnostic-record-limit requires --jit-diagnostics-out".to_owned());
    }

    Ok(options)
}

fn run_interpreter(script_path: &str, code: &str, runs: usize, warmup: usize) {
    let context = &mut interpreter_context();
    context.set_optimizer_options(OptimizerOptions::empty());
    register_runtime(context);

    let script = parse_script(code, context);
    script.codeblock(context).unwrap();
    script.evaluate(context).unwrap();

    let function = main_function(context, script_path);

    // Warmup.
    for _ in 0..warmup {
        function.call(&JsValue::undefined(), &[], context).unwrap();
    }

    let start = Instant::now();
    let mut acc: i32 = 0;
    for _ in 0..runs {
        let v = function.call(&JsValue::undefined(), &[], context).unwrap();
        // Mix in some bit of the result to keep the measured result observable
        // even when this runner is used with a JIT-enabled Boa build.
        acc ^= v.to_i32(context).unwrap_or(0);
    }
    let elapsed_ns = start.elapsed().as_nanos();

    println!(
        "elapsed_ns={elapsed_ns} runs={runs} ns_per_run={} acc={acc} mode=interp",
        elapsed_ns / runs as u128
    );
}

fn interpreter_context() -> Context {
    let builder = Context::builder();
    #[cfg(feature = "jit")]
    let builder = builder.jit(false);
    builder.build().expect("build interpreter context")
}

fn register_runtime(context: &mut Context) {
    boa_runtime::register(
        boa_runtime::extensions::ConsoleExtension(boa_runtime::NullLogger),
        None,
        context,
    )
    .expect("register runtime");
}

fn parse_script(code: &str, context: &mut Context) -> Script {
    Script::parse(Source::from_bytes(code), None, context).unwrap()
}

fn main_function(context: &mut Context, script_path: &str) -> boa_engine::object::JsObject {
    context
        .global_object()
        .get(js_string!("main"), context)
        .unwrap_or_else(|_| panic!("no main in {script_path}"))
        .as_callable()
        .unwrap_or_else(|| panic!("main is not callable in {script_path}"))
}

#[cfg(feature = "jit")]
struct OsrColdSample {
    elapsed_ns: u128,
    acc: i32,
    stats: boa_engine::jit::JitStats,
    diagnostics: Option<boa_engine::jit::JitDiagnosticSnapshot>,
}

/// Execute one production-threshold call without any earlier native work in
/// this context. The process-level protocol launches this mode directly, so
/// Cranelift and the loop cache are cold when the timer begins.
#[cfg(feature = "jit")]
fn collect_osr_cold_sample(
    script_path: &str,
    code: &str,
    diagnostic_limits: Option<boa_engine::jit::JitDiagnosticLimits>,
) -> OsrColdSample {
    let context = &mut interpreter_context();
    context.set_optimizer_options(OptimizerOptions::empty());
    register_runtime(context);

    let script = parse_script(code, context);
    script.codeblock(context).unwrap();
    script.evaluate(context).unwrap();
    let function = main_function(context, script_path);
    if let Some(limits) = diagnostic_limits {
        context.enable_jit_diagnostics(limits);
    } else {
        context.enable_jit();
    }

    let start = Instant::now();
    let value = function.call(&JsValue::undefined(), &[], context).unwrap();
    let elapsed_ns = start.elapsed().as_nanos();
    let acc = value.to_i32(context).unwrap_or(0);
    let stats = context.jit_stats().expect("JIT stats");
    let diagnostics =
        diagnostic_limits.map(|_| context.jit_diagnostic_snapshot().expect("JIT diagnostics"));

    OsrColdSample {
        elapsed_ns,
        acc,
        stats,
        diagnostics,
    }
}

#[cfg(feature = "jit")]
fn run_osr_cold(
    script_path: &str,
    code: &str,
    diagnostics_out: Option<&Path>,
    diagnostic_record_limit: Option<usize>,
) {
    let diagnostic_limits = diagnostics_out.map(|_| jit_diagnostic_limits(diagnostic_record_limit));
    let sample = collect_osr_cold_sample(script_path, code, diagnostic_limits);
    let stats = sample.stats;
    let osr = stats.osr;
    let resources = stats.resources;
    let total_compile_time_ns = stats.compile_time_ns.saturating_add(osr.compile_time_ns);
    let function_native_entries = stats.native_entries.saturating_sub(osr.entries);

    println!(
        concat!(
            "elapsed_ns={} runs=1 ns_per_run={} acc={} mode=osr-cold ",
            "total_compile_time_ns={} function_compile_time_ns={} ",
            "compilations={} native_compilations={} shim_compilations={} ",
            "function_entries={} native_entries={} function_native_entries={} deopts={} ",
            "loop_backedges={} hotness_threshold_crossings={} ",
            "osr_cache_requests={} osr_cache_hits={} osr_cache_misses={} ",
            "osr_hotness_crossings={} osr_compile_attempts={} ",
            "osr_compilations={} osr_entries={} osr_entry_rejections={} ",
            "osr_continuations={} osr_deopts={} osr_compile_time_ns={} ",
            "osr_code_bytes={} osr_rejection_compiler_failure={} ",
            "osr_suppression_region_capacity={} osr_suppression_code_bytes={} ",
            "osr_suppression_compile_time={} resource_function_capacity={} ",
            "resource_oversized_functions={} resource_terminal_failure_hits={} ",
            "resource_call_target_capacity={} resource_code_bytes={} ",
            "resource_compile_time={} resource_slow_attempt={} ",
            "resource_payload_overrun_retirements={} ",
            "resource_compilation_failure_retirements={} resource_retained_code_bytes={} ",
            "resource_compile_time_ns={}"
        ),
        sample.elapsed_ns,
        sample.elapsed_ns,
        sample.acc,
        total_compile_time_ns,
        stats.compile_time_ns,
        stats.compilations,
        stats.native_compilations,
        stats.shim_compilations,
        stats.function_entries,
        stats.native_entries,
        function_native_entries,
        stats.deopts,
        stats.loop_backedges,
        stats.hotness_threshold_crossings,
        osr.cache_requests,
        osr.cache_hits,
        osr.cache_misses,
        osr.hotness_crossings,
        osr.compile_attempts,
        osr.compilations,
        osr.entries,
        osr.entry_rejections,
        osr.continuations,
        osr.deopts,
        osr.compile_time_ns,
        osr.code_bytes,
        osr.rejections.compiler_failure,
        osr.suppressions.region_capacity,
        osr.suppressions.code_bytes,
        osr.suppressions.compile_time,
        resources.function_capacity,
        resources.oversized_functions,
        resources.terminal_failure_hits,
        resources.call_target_capacity,
        resources.code_bytes,
        resources.compile_time,
        resources.slow_attempt,
        resources.payload_overrun_retirements,
        resources.compilation_failure_retirements,
        resources.retained_code_bytes,
        resources.compile_time_ns,
    );

    if let (Some(output), Some(snapshot)) = (diagnostics_out, sample.diagnostics) {
        let report = JitOsrColdDiagnosticReport {
            schema_version: JIT_DIAGNOSTIC_SCHEMA_VERSION,
            mode: "osr-cold",
            runs: 1,
            warmup: 0,
            sample: snapshot,
        };
        let json = serde_json::to_vec_pretty(&report).expect("serialize JIT diagnostics");
        fs::write(output, json).expect("write JIT diagnostics");
    }
}

#[cfg(feature = "jit")]
fn run_jit(
    script_path: &str,
    code: &str,
    runs: usize,
    warmup: usize,
    diagnostics_out: Option<&Path>,
    diagnostic_record_limit: Option<usize>,
) {
    // The cold sample starts from a fresh backend and lowers the first main()
    // entry immediately. Script parsing and top-level setup remain outside the
    // timer, matching the existing runner protocol; the reported duration
    // includes JIT compilation and the complete first call.
    let cold_context = &mut interpreter_context();
    cold_context.set_optimizer_options(OptimizerOptions::empty());
    register_runtime(cold_context);
    let cold_script = parse_script(code, cold_context);
    cold_script.codeblock(cold_context).unwrap();
    cold_script.evaluate(cold_context).unwrap();
    let cold_function = main_function(cold_context, script_path);
    if diagnostics_out.is_some() {
        cold_context.enable_jit_diagnostics(jit_diagnostic_limits(diagnostic_record_limit));
    } else {
        cold_context.enable_jit();
    }
    cold_context.set_jit_thresholds(boa_engine::jit::JitThresholds {
        function_entries: 1,
        loop_backedges: 1,
    });

    let cold_start = Instant::now();
    let cold_value = cold_function
        .call(&JsValue::undefined(), &[], cold_context)
        .unwrap();
    let cold_elapsed_ns = cold_start.elapsed().as_nanos();
    let cold_acc = cold_value.to_i32(cold_context).unwrap_or(0);
    let cold_stats = cold_context.jit_stats().expect("JIT stats");
    let cold_resources = cold_stats.resources;
    let cold_diagnostics = diagnostics_out.map(|_| {
        cold_context
            .jit_diagnostic_snapshot()
            .expect("JIT diagnostics")
    });

    // The warm sample uses the production thresholds and a fresh context so
    // compilation is not accidentally amortized by the cold sample.
    let context = &mut interpreter_context();
    context.set_optimizer_options(OptimizerOptions::empty());
    register_runtime(context);
    let script = parse_script(code, context);
    script.codeblock(context).unwrap();
    script.evaluate(context).unwrap();
    let function = main_function(context, script_path);
    if diagnostics_out.is_some() {
        context.enable_jit_diagnostics(jit_diagnostic_limits(diagnostic_record_limit));
    } else {
        context.enable_jit();
    }

    for _ in 0..warmup {
        function.call(&JsValue::undefined(), &[], context).unwrap();
    }

    let start = Instant::now();
    let mut acc: i32 = 0;
    for _ in 0..runs {
        let v = function.call(&JsValue::undefined(), &[], context).unwrap();
        acc ^= v.to_i32(context).unwrap_or(0);
    }
    let elapsed_ns = start.elapsed().as_nanos();
    let stats = context.jit_stats().expect("JIT stats");
    let resources = stats.resources;
    let diagnostics =
        diagnostics_out.map(|_| context.jit_diagnostic_snapshot().expect("JIT diagnostics"));

    println!(
        concat!(
            "elapsed_ns={} runs={} ns_per_run={} acc={} mode=jit ",
            "cold_elapsed_ns={} cold_ns_per_run={} cold_acc={} ",
            "cold_compile_time_ns={} cold_native_entries={} cold_deopts={} ",
            "cold_scheduler_call_exits={} cold_loop_backedges={} ",
            "cold_hotness_threshold_crossings={} cold_saturated_loop_backedges={} ",
            "cold_dormant_loop_frames={} ",
            "compile_time_ns={} compilations={} native_compilations={} ",
            "shim_compilations={} native_entries={} deopts={} ",
            "scheduler_call_exits={} loop_backedges={} hotness_threshold_crossings={} ",
            "saturated_loop_backedges={} dormant_loop_frames={} ",
            "cold_admission_denials={} admission_denials={} ",
            "cold_resource_function_capacity={} resource_function_capacity={} ",
            "cold_resource_code_bytes={} resource_code_bytes={} ",
            "cold_resource_compile_time={} resource_compile_time={} ",
            "cold_resource_slow_attempt={} resource_slow_attempt={} ",
            "cold_resource_payload_overrun_retirements={} ",
            "resource_payload_overrun_retirements={} ",
            "cold_resource_compilation_failure_retirements={} ",
            "resource_compilation_failure_retirements={} ",
            "cold_resource_retained_code_bytes={} resource_retained_code_bytes={} ",
            "cold_resource_compile_time_ns={} resource_compile_time_ns={}"
        ),
        elapsed_ns,
        runs,
        elapsed_ns / runs as u128,
        acc,
        cold_elapsed_ns,
        cold_elapsed_ns,
        cold_acc,
        cold_stats.compile_time_ns,
        cold_stats.native_entries,
        cold_stats.deopts,
        cold_stats.scheduler_call_exits,
        cold_stats.loop_backedges,
        cold_stats.hotness_threshold_crossings,
        cold_stats.saturated_loop_backedges,
        cold_stats.dormant_loop_frames,
        stats.compile_time_ns,
        stats.compilations,
        stats.native_compilations,
        stats.shim_compilations,
        stats.native_entries,
        stats.deopts,
        stats.scheduler_call_exits,
        stats.loop_backedges,
        stats.hotness_threshold_crossings,
        stats.saturated_loop_backedges,
        stats.dormant_loop_frames,
        cold_stats.admission_denials,
        stats.admission_denials,
        cold_resources.function_capacity,
        resources.function_capacity,
        cold_resources.code_bytes,
        resources.code_bytes,
        cold_resources.compile_time,
        resources.compile_time,
        cold_resources.slow_attempt,
        resources.slow_attempt,
        cold_resources.payload_overrun_retirements,
        resources.payload_overrun_retirements,
        cold_resources.compilation_failure_retirements,
        resources.compilation_failure_retirements,
        cold_resources.retained_code_bytes,
        resources.retained_code_bytes,
        cold_resources.compile_time_ns,
        resources.compile_time_ns,
    );

    if let Some(output) = diagnostics_out {
        let report = JitDiagnosticReport {
            schema_version: JIT_DIAGNOSTIC_SCHEMA_VERSION,
            runs,
            warmup,
            cold: cold_diagnostics.expect("cold diagnostics were requested"),
            warm: diagnostics.expect("warm diagnostics were requested"),
        };
        let json = serde_json::to_vec_pretty(&report).expect("serialize JIT diagnostics");
        fs::write(output, json).expect("write JIT diagnostics");
    }
}

#[cfg(feature = "jit")]
fn jit_diagnostic_limits(record_limit: Option<usize>) -> boa_engine::jit::JitDiagnosticLimits {
    let Some(record_limit) = record_limit else {
        return boa_engine::jit::JitDiagnosticLimits::default();
    };

    boa_engine::jit::JitDiagnosticLimits {
        compile_records: record_limit,
        admission_records: record_limit,
        exit_records: record_limit,
        call_records: record_limit,
        loop_records: record_limit,
        storage_records: record_limit,
    }
}

#[cfg(feature = "jit")]
#[derive(serde::Serialize)]
struct JitDiagnosticReport {
    schema_version: u32,
    runs: usize,
    warmup: usize,
    cold: boa_engine::jit::JitDiagnosticSnapshot,
    warm: boa_engine::jit::JitDiagnosticSnapshot,
}

#[cfg(feature = "jit")]
#[derive(serde::Serialize)]
struct JitOsrColdDiagnosticReport {
    schema_version: u32,
    mode: &'static str,
    runs: usize,
    warmup: usize,
    sample: boa_engine::jit::JitDiagnosticSnapshot,
}

#[cfg(test)]
mod tests {
    use super::{
        JitDiagnosticOptions, RunnerMode, parse_jit_diagnostic_options, parse_runner_options,
        validate_mode_invocation,
    };

    #[test]
    fn parses_and_validates_isolated_osr_mode() {
        assert_eq!(RunnerMode::parse("interp"), Ok(RunnerMode::Interpreter));
        assert_eq!(RunnerMode::parse("jit"), Ok(RunnerMode::Jit));
        assert_eq!(RunnerMode::parse("osr-cold"), Ok(RunnerMode::OsrCold));
        assert!(RunnerMode::parse("cold").is_err());

        assert_eq!(validate_mode_invocation(RunnerMode::OsrCold, 1, 0), Ok(()));
        assert!(validate_mode_invocation(RunnerMode::OsrCold, 2, 0).is_err());
        assert!(validate_mode_invocation(RunnerMode::OsrCold, 1, 1).is_err());
        assert_eq!(validate_mode_invocation(RunnerMode::Jit, 2, 1), Ok(()));
    }

    #[test]
    fn runner_options_allow_the_documented_default_mode() {
        assert_eq!(
            parse_runner_options(&[]),
            Ok((RunnerMode::Interpreter, JitDiagnosticOptions::default()))
        );
        assert_eq!(
            parse_runner_options(&["jit".to_owned()]),
            Ok((RunnerMode::Jit, JitDiagnosticOptions::default()))
        );
        assert!(parse_runner_options(&["unknown".to_owned()]).is_err());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn isolated_osr_sample_uses_production_thresholds_without_function_jit() {
        let source = r#"
            function main() {
                Math.abs(300);
                let total = 0.5;
                for (let i = 0; i < 300; i++) {
                    total = total + i;
                }
                return total;
            }
        "#;
        let sample = super::collect_osr_cold_sample("isolated-osr.js", source, None);

        assert_eq!(sample.acc, 44_850);
        assert_eq!(sample.stats.function_entries, 1);
        assert_eq!(sample.stats.compilations, 0);
        assert_eq!(sample.stats.native_entries, 1);
        assert_eq!(sample.stats.osr.compile_attempts, 1);
        assert_eq!(sample.stats.osr.compilations, 1);
        assert_eq!(sample.stats.osr.entries, 1);
        assert_eq!(
            sample
                .stats
                .native_entries
                .saturating_sub(sample.stats.osr.entries),
            0
        );
        assert_eq!(sample.stats.osr.continuations, 1);
        assert_eq!(sample.stats.osr.entry_rejections, 0);
        assert_eq!(sample.stats.osr.deopts, 0);
        assert!(sample.diagnostics.is_none());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn isolated_osr_diagnostic_sample_is_bounded_and_zero_drop() {
        let source = r#"
            function main() {
                Math.abs(300);
                let total = 0.5;
                for (let i = 0; i < 300; i++) {
                    total = total + i;
                }
                return total;
            }
        "#;
        let limits = boa_engine::jit::JitDiagnosticLimits::default();
        let sample = super::collect_osr_cold_sample("isolated-osr.js", source, Some(limits));
        let diagnostics = sample.diagnostics.expect("diagnostic snapshot");

        assert_eq!(diagnostics.limits, limits);
        assert_eq!(diagnostics.osr.compilations, 1);
        assert_eq!(diagnostics.osr.entries, 1);
        assert_eq!(diagnostics.dropped_compile_records, 0);
        assert_eq!(diagnostics.dropped_admission_records, 0);
        assert_eq!(diagnostics.dropped_exit_records, 0);
        assert_eq!(diagnostics.dropped_call_observations, 0);
        assert_eq!(diagnostics.dropped_loop_observations, 0);
        assert_eq!(diagnostics.dropped_storage_observations, 0);
    }

    #[cfg(feature = "jit")]
    #[test]
    fn resource_saturation_fixture_reaches_production_function_and_loop_limits() {
        let source = include_str!("../../scripts/microbench/jit-resource-saturation.js");
        let sample = super::collect_osr_cold_sample("jit-resource-saturation.js", source, None);

        assert_eq!(sample.acc, 616_416);
        assert!(sample.stats.hotness_threshold_crossings >= 192);
        assert!(sample.stats.osr.cache_requests >= 64);
        assert!(sample.stats.resources.retained_code_bytes > 0);
        assert!(sample.stats.resources.compile_time_ns > 0);
        assert_eq!(sample.stats.resources.payload_overrun_retirements, 0);
        assert_eq!(sample.stats.resources.compilation_failure_retirements, 0);
    }

    #[test]
    fn parses_optional_jit_diagnostics() {
        assert_eq!(
            parse_jit_diagnostic_options(&[]),
            Ok(JitDiagnosticOptions::default())
        );
        assert_eq!(
            parse_jit_diagnostic_options(&[
                "--jit-diagnostics-out".to_owned(),
                "profile.json".to_owned(),
            ]),
            Ok(JitDiagnosticOptions {
                output: Some("profile.json"),
                record_limit: None,
            })
        );
        assert_eq!(
            parse_jit_diagnostic_options(&[
                "--jit-diagnostic-record-limit".to_owned(),
                "4096".to_owned(),
                "--jit-diagnostics-out".to_owned(),
                "profile.json".to_owned(),
            ]),
            Ok(JitDiagnosticOptions {
                output: Some("profile.json"),
                record_limit: Some(4096),
            })
        );
    }

    #[test]
    fn rejects_invalid_jit_diagnostic_options() {
        let cases = [
            vec!["--unknown".to_owned()],
            vec!["--jit-diagnostics-out".to_owned(), String::new()],
            vec![
                "--jit-diagnostic-record-limit".to_owned(),
                "not-a-number".to_owned(),
            ],
            vec![
                "--jit-diagnostic-record-limit".to_owned(),
                "4096".to_owned(),
            ],
            vec![
                "--jit-diagnostics-out".to_owned(),
                "one.json".to_owned(),
                "--jit-diagnostics-out".to_owned(),
                "two.json".to_owned(),
            ],
        ];

        for args in cases {
            assert!(
                parse_jit_diagnostic_options(&args).is_err(),
                "accepted {args:?}"
            );
        }
    }
}
