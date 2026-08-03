//! Boa-side counterpart to runner.mjs.
//!
//! Loads a JS script, evaluates it once, then calls `main()` `runs` times
//! after `warmup` warmup runs, and prints `elapsed_ns=<N>` on stdout.
//!
//! This mirrors the timing protocol in `tools/bench-compare/runner.mjs` so
//! Boa and node/bun numbers are directly comparable. When built with the
//! `jit` feature, a fourth `jit` mode reports a warm JIT measurement and a
//! separate first-call measurement that includes native compilation. JIT mode
//! can also write a bounded diagnostic snapshot after timing via
//! `--jit-diagnostics-out <path>`.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use std::{env, fs, path::Path, process, time::Instant};

#[cfg(feature = "jit")]
use boa_engine::jit::JIT_DIAGNOSTIC_SCHEMA_VERSION;
use boa_engine::{
    Context, JsValue, Source, js_string, optimizer::OptimizerOptions, script::Script,
};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(2);
    }

    let script_path = &args[1];
    let runs: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let warmup: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mode = args
        .get(4)
        .filter(|mode| !mode.is_empty())
        .map(String::as_str)
        .unwrap_or("interp");
    let diagnostics_out = parse_jit_diagnostics_output(&args[5..]).unwrap_or_else(|error| {
        eprintln!("{error}");
        print_usage();
        process::exit(2);
    });

    let code = fs::read_to_string(Path::new(script_path)).expect("read script");

    match mode {
        "interp" if diagnostics_out.is_none() => {
            run_interpreter(script_path, &code, runs, warmup);
        }
        "interp" => {
            eprintln!("--jit-diagnostics-out is only valid in jit mode");
            process::exit(2);
        }
        "jit" => {
            #[cfg(feature = "jit")]
            run_jit(
                script_path,
                &code,
                runs,
                warmup,
                diagnostics_out.map(Path::new),
            );
            #[cfg(not(feature = "jit"))]
            {
                eprintln!("jit mode requires building bench-compare-runner with `--features jit`");
                process::exit(2);
            }
        }
        other => {
            eprintln!("unknown runner mode `{other}`; expected `interp` or `jit`");
            process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: runner-boa <script.js> [runs] [warmup] [interp|jit] \
         [--jit-diagnostics-out <path>]"
    );
}

fn parse_jit_diagnostics_output(args: &[String]) -> Result<Option<&str>, &'static str> {
    match args {
        [] => Ok(None),
        [flag, path] if flag == "--jit-diagnostics-out" && !path.is_empty() => {
            Ok(Some(path.as_str()))
        }
        [flag, _] if flag != "--jit-diagnostics-out" => Err("unknown runner option"),
        [flag, _] if flag == "--jit-diagnostics-out" => {
            Err("--jit-diagnostics-out requires a non-empty path")
        }
        _ => Err("expected --jit-diagnostics-out followed by one path"),
    }
}

fn run_interpreter(script_path: &str, code: &str, runs: usize, warmup: usize) {
    let context = &mut Context::default();
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
fn run_jit(
    script_path: &str,
    code: &str,
    runs: usize,
    warmup: usize,
    diagnostics_out: Option<&Path>,
) {
    // The cold sample starts from a fresh backend and lowers the first main()
    // entry immediately. Script parsing and top-level setup remain outside the
    // timer, matching the existing runner protocol; the reported duration
    // includes JIT compilation and the complete first call.
    let cold_context = &mut Context::default();
    cold_context.set_optimizer_options(OptimizerOptions::empty());
    register_runtime(cold_context);
    let cold_script = parse_script(code, cold_context);
    cold_script.codeblock(cold_context).unwrap();
    cold_script.evaluate(cold_context).unwrap();
    let cold_function = main_function(cold_context, script_path);
    if diagnostics_out.is_some() {
        cold_context.enable_jit_diagnostics(boa_engine::jit::JitDiagnosticLimits::default());
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
    let cold_diagnostics = diagnostics_out.map(|_| {
        cold_context
            .jit_diagnostic_snapshot()
            .expect("JIT diagnostics")
    });

    // The warm sample uses the production thresholds and a fresh context so
    // compilation is not accidentally amortized by the cold sample.
    let context = &mut Context::default();
    context.set_optimizer_options(OptimizerOptions::empty());
    register_runtime(context);
    let script = parse_script(code, context);
    script.codeblock(context).unwrap();
    script.evaluate(context).unwrap();
    let function = main_function(context, script_path);
    if diagnostics_out.is_some() {
        context.enable_jit_diagnostics(boa_engine::jit::JitDiagnosticLimits::default());
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
            "cold_admission_denials={} admission_denials={}"
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
#[derive(serde::Serialize)]
struct JitDiagnosticReport {
    schema_version: u32,
    runs: usize,
    warmup: usize,
    cold: boa_engine::jit::JitDiagnosticSnapshot,
    warm: boa_engine::jit::JitDiagnosticSnapshot,
}

#[cfg(test)]
mod tests {
    use super::parse_jit_diagnostics_output;

    #[test]
    fn parses_optional_jit_diagnostics_output() {
        assert_eq!(parse_jit_diagnostics_output(&[]), Ok(None));
        assert_eq!(
            parse_jit_diagnostics_output(&[
                "--jit-diagnostics-out".to_owned(),
                "profile.json".to_owned(),
            ]),
            Ok(Some("profile.json"))
        );
        assert!(parse_jit_diagnostics_output(&["--unknown".to_owned()]).is_err());
        assert!(
            parse_jit_diagnostics_output(&["--jit-diagnostics-out".to_owned(), String::new(),])
                .is_err()
        );
    }
}
