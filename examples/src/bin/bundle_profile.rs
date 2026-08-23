//! Samples where parse and compile time goes on a real JavaScript bundle.
//!
//! `bundle_bench` reports *how long* the two phases take;this reports *where*
//! that time is spent, which is what a parser or compiler change has to be
//! aimed at. `perf` needs privileges some development hosts do not have, so
//! sampling happens in-process via `pprof`, which uses `setitimer(ITIMER_PROF)`.
//!
//! Run with:
//! `cargo run --release -p boa_examples --bin bundle_profile -- bundle.js [iterations] [out.svg]`

use std::{env, fs, path::PathBuf, time::Instant};

use boa_engine::{Context, Source, optimizer::OptimizerOptions, script::Script};

fn main() {
    let mut args = env::args_os().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: bundle_profile <bundle.js> [iterations] [out.svg] [phase]");
        return;
    };
    let iterations = args
        .next()
        .map(|v| {
            v.to_string_lossy()
                .parse::<usize>()
                .expect("iterations must be a positive integer")
        })
        .unwrap_or(20);
    let out = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bundle-profile.svg"));
    // `parse`, `compile`, or `all` (the default).
    let phase = args
        .next()
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| "all".to_owned());

    let source = fs::read(&path).expect("failed to read bundle");

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(4999)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .expect("start sampling profiler");

    // `BOA_PROFILE_SCRIPT=1` executes the file instead of only parsing and
    // compiling it, so the profile covers the interpreter and the collector.
    let execute = env::var("BOA_PROFILE_SCRIPT").is_ok();

    // `BOA_PROFILE_OPTIMIZER=off` measures parse without the parse-time
    // constant-folding/strength-reduction walk; `stats` prints what that walk
    // actually achieved on this input.
    let optimizer = env::var("BOA_PROFILE_OPTIMIZER").unwrap_or_default();

    let started = Instant::now();
    for _ in 0..iterations {
        let mut context = Context::default();
        match optimizer.as_str() {
            "off" => context.set_optimizer_options(OptimizerOptions::empty()),
            "stats" => context.set_optimizer_options(
                OptimizerOptions::OPTIMIZE_ALL | OptimizerOptions::STATISTICS,
            ),
            _ => {}
        }
        let script = Script::parse(Source::from_bytes(&source), None, &mut context)
            .expect("failed to parse bundle");
        if phase != "parse" {
            script
                .codeblock(&mut context)
                .expect("failed to compile bundle");
        }
        if execute {
            script.evaluate(&mut context).expect("failed to run script");
            context.run_jobs().expect("failed to drain jobs");
        }
    }
    let elapsed = started.elapsed();

    let gc = boa_gc::stats();
    eprintln!(
        "bundle_profile: gc collections={} bytes_allocated={} threshold={}",
        gc.collections, gc.bytes_allocated, gc.threshold
    );
    eprintln!(
        "bundle_profile: {} bytes x {iterations} ({phase}) in {:.1} ms ({:.2} ms/iter)",
        source.len(),
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e3 / iterations as f64,
    );

    match guard.report().build() {
        Ok(report) => {
            let f = fs::File::create(&out)
                .unwrap_or_else(|e| panic!("cannot create {}: {e}", out.display()));
            report.flamegraph(f).expect("write flamegraph");

            // Folded stacks alongside the SVG: self-time per frame is what
            // tells you which function to change, and it cannot be read off a
            // flamegraph's cumulative bars without reconstructing the tree.
            let folded_path = out.with_extension("folded");
            let mut folded = String::new();
            for (frames, count) in &report.data {
                let mut stack: Vec<String> = frames
                    .frames
                    .iter()
                    .rev()
                    .map(|symbols| {
                        symbols
                            .first()
                            .map_or_else(|| "??".to_owned(), |symbol| symbol.name())
                    })
                    .collect();
                stack.retain(|name| name != "??");
                folded.push_str(&stack.join(";"));
                folded.push(' ');
                folded.push_str(&count.to_string());
                folded.push('\n');
            }
            fs::write(&folded_path, folded).expect("write folded stacks");
            eprintln!(
                "bundle_profile: wrote {} and {}",
                out.display(),
                folded_path.display()
            );
        }
        Err(e) => eprintln!("bundle_profile: report failed: {e}"),
    }
}
