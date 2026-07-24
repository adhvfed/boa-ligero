//! Measures repeated parse and compile time for a local JavaScript bundle.
//!
//! Run with:
//! `cargo run --release --example bundle_bench -- path/to/bundle.js [iterations]`

use std::{
    env, fs,
    path::PathBuf,
    process,
    time::{Duration, Instant},
};

use boa_engine::{Context, Source, script::Script};

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn main() {
    let mut args = env::args_os().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: bundle_bench <bundle.js> [iterations]");
        process::exit(2);
    };
    let iterations = args
        .next()
        .map(|value| {
            value
                .to_string_lossy()
                .parse::<usize>()
                .expect("iterations must be a positive integer")
        })
        .unwrap_or(15);
    assert!(iterations > 0, "iterations must be positive");

    let source = fs::read(&path).expect("failed to read bundle");
    let mut parse_samples = Vec::with_capacity(iterations);
    let mut compile_samples = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        // A browser document starts with a fresh realm and interner. Reusing
        // one here would make later iterations unrealistically benefit from
        // all strings interned by the first parse.
        let mut context = Context::default();
        let start = Instant::now();
        let script = Script::parse(Source::from_bytes(&source), None, &mut context)
            .expect("failed to parse bundle");
        parse_samples.push(start.elapsed());

        let start = Instant::now();
        script
            .codeblock(&mut context)
            .expect("failed to compile bundle");
        compile_samples.push(start.elapsed());
    }

    let parse = median(&mut parse_samples);
    let compile = median(&mut compile_samples);
    println!(
        "{} bytes: parse={parse:.2?}, compile={compile:.2?}, total={:.2?}",
        source.len(),
        parse + compile
    );
}
