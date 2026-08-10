#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { accessSync, constants, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  distribution,
  geometricMean,
  parseResultLine,
  performanceTargetFailures,
} from "./stats.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../..");
const SCRIPT_DIR = join(REPO, "benches/scripts/microbench");
const NODE_RUNNER = join(HERE, "runner.mjs");
const BOA_RUNNER = join(REPO, "target/release/bench-compare-runner");
const SUITE_PATH = join(HERE, "suite.json");

function positiveInteger(raw, flag) {
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value <= 0) throw new Error(`${flag} requires a positive integer`);
  return value;
}

function nonNegativeInteger(raw, flag) {
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${flag} requires a non-negative integer`);
  }
  return value;
}

function finiteNumber(raw, flag) {
  const value = Number(raw);
  if (!Number.isFinite(value) || value < 0) throw new Error(`${flag} requires a non-negative number`);
  return value;
}

function parseArgs(argv) {
  const options = {
    runs: process.env.RUNS ? positiveInteger(process.env.RUNS, "RUNS") : null,
    warmup: process.env.WARMUP ? nonNegativeInteger(process.env.WARMUP, "WARMUP") : null,
    samples: process.env.SAMPLES ? positiveInteger(process.env.SAMPLES, "SAMPLES") : null,
    maxCvPct: 5,
    boaJit: process.env.BOA_JIT === "1",
    failNoisy: false,
    enforceTargets: false,
    binding: false,
    filters: [],
    json: null,
  };

  for (let index = 2; index < argv.length; index++) {
    const argument = argv[index];
    if (argument === "--runs") options.runs = positiveInteger(argv[++index], argument);
    else if (argument === "--warmup") options.warmup = nonNegativeInteger(argv[++index], argument);
    else if (argument === "--samples") options.samples = positiveInteger(argv[++index], argument);
    else if (argument === "--max-cv-pct") options.maxCvPct = finiteNumber(argv[++index], argument);
    else if (argument === "--json") options.json = argv[++index];
    else if (argument === "--boa-jit") options.boaJit = true;
    else if (argument === "--fail-noisy") options.failNoisy = true;
    else if (argument === "--enforce-targets") options.enforceTargets = true;
    else if (argument === "--binding") options.binding = true;
    else if (argument === "--help" || argument === "-h") {
      console.log(`usage: node tools/bench-compare/compare.mjs [filters...] [options]

  --runs N         main() calls per process sample (default 50)
  --warmup N       untimed warmup calls per process sample (default 20)
  --samples N      independent process samples per engine (default 7)
  --boa-jit        include Boa's explicit tiered/JIT mode
  --json PATH      write the complete machine-readable report
  --max-cv-pct N   noisy-sample threshold (default 5)
  --fail-noisy     fail when any engine/workload exceeds the CV threshold
  --enforce-targets fail when a selected workload or the complete suite misses its parity target
  --binding        9 samples, 200 runs, 80 warmups, Boa JIT, fail noisy, enforce targets

RUNS, WARMUP, SAMPLES, and BOA_JIT remain supported for the old shell workflow.`);
      process.exit(0);
    } else if (argument.startsWith("--")) throw new Error(`unknown option: ${argument}`);
    else options.filters.push(argument.replace(/\.js$/, ""));
  }

  if (options.binding) {
    options.runs = 200;
    options.warmup = 80;
    options.samples = 9;
    options.boaJit = true;
    options.failNoisy = true;
    options.enforceTargets = true;
  }
  options.runs ??= 50;
  options.warmup ??= 20;
  options.samples ??= 7;
  return options;
}

function executable(path) {
  try {
    accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function engines(options) {
  if (!executable(BOA_RUNNER)) {
    throw new Error(
      "missing release Boa runner; run `cargo build --release -p boa_benches --bin bench-compare-runner --features jit`",
    );
  }
  const definitions = [
    { name: "v8", command: process.execPath, prefix: [NODE_RUNNER] },
    { name: "v8-jitless", command: process.execPath, prefix: ["--jitless", NODE_RUNNER] },
    { name: "boa", command: BOA_RUNNER, prefix: [], mode: "interp" },
  ];
  if (options.boaJit) definitions.push({ name: "boa-jit", command: BOA_RUNNER, prefix: [], mode: "jit" });
  return definitions;
}

function loadSuite(filters) {
  const suite = JSON.parse(readFileSync(SUITE_PATH, "utf8"));
  if (
    suite.schema_version !== 1 ||
    typeof suite.benchmarks !== "object" ||
    typeof suite.targets !== "object"
  ) {
    throw new Error("unsupported or malformed benchmark suite manifest");
  }
  for (const [name, target] of Object.entries(suite.targets)) {
    if (
      !Number.isFinite(target.geomean_max) ||
      target.geomean_max <= 0 ||
      !Number.isFinite(target.workload_max) ||
      target.workload_max <= 0
    ) {
      throw new Error(`malformed performance target: ${name}`);
    }
  }

  const discovered = readdirSync(SCRIPT_DIR)
    .filter((file) => file.endsWith(".js") && !file.startsWith("_"))
    .map((file) => basename(file, ".js"))
    .sort();
  const missingMetadata = discovered.filter((name) => !(name in suite.benchmarks));
  const missingScript = Object.keys(suite.benchmarks).filter((name) => !discovered.includes(name));
  if (missingMetadata.length || missingScript.length) {
    throw new Error(
      `suite manifest mismatch; missing metadata=[${missingMetadata}], missing scripts=[${missingScript}]`,
    );
  }

  const enabled = discovered
    .filter((name) => suite.benchmarks[name].enabled !== false)
    .map((name) => ({ name, path: join(SCRIPT_DIR, `${name}.js`), ...suite.benchmarks[name] }));
  return {
    targets: suite.targets,
    headlineWorkloadCount: enabled.filter((benchmark) => benchmark.headline).length,
    benchmarks: enabled
      .filter(
        (benchmark) =>
          filters.length === 0 || filters.some((filter) => benchmark.name.includes(filter)),
      ),
  };
}

function rotate(values, count) {
  const offset = count % values.length;
  return [...values.slice(offset), ...values.slice(0, offset)];
}

function sampleOrder(definitions, sample) {
  const order = rotate(definitions, sample);
  return sample % 2 === 0 ? order : order.reverse();
}

function runEngine(engine, benchmark, options) {
  const arguments_ = [...engine.prefix, benchmark.path, String(options.runs), String(options.warmup)];
  if (engine.mode) arguments_.push(engine.mode);
  const result = spawnSync(engine.command, arguments_, {
    cwd: REPO,
    encoding: "utf8",
    timeout: 5 * 60 * 1000,
    maxBuffer: 16 * 1024 * 1024,
  });
  if (result.error) throw new Error(`${engine.name}/${benchmark.name}: ${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(
      `${engine.name}/${benchmark.name} exited ${result.status}: ${(result.stderr || result.stdout).trim()}`,
    );
  }
  return parseResultLine(result.stdout);
}

function gitRevision() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: REPO, encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : null;
}

function ratio(numerator, denominator) {
  return numerator && denominator ? numerator.p50 / denominator.p50 : null;
}

function formatRatio(value) {
  return value == null ? "-" : `${value.toFixed(2)}x`;
}

function maxOrNull(values) {
  return values.length === 0 ? null : Math.max(...values);
}

function printBenchmark(result, includeJit) {
  const boa = result.engines.boa.distribution;
  const v8 = result.engines.v8.distribution;
  const jitless = result.engines["v8-jitless"].distribution;
  const jit = result.engines["boa-jit"]?.distribution;
  const marker = result.headline ? "*" : " ";
  const columns = [
    `${marker} ${result.name}`.padEnd(31),
    Math.round(boa.p50).toString().padStart(12),
    Math.round(jitless.p50).toString().padStart(12),
    formatRatio(result.ratios.boa_to_v8_jitless).padStart(12),
  ];
  if (includeJit) {
    columns.push(Math.round(jit.p50).toString().padStart(12));
    columns.push(Math.round(v8.p50).toString().padStart(12));
    columns.push(formatRatio(result.ratios.boa_jit_to_v8).padStart(12));
  }
  console.log(columns.join(" "));
}

function main() {
  const options = parseArgs(process.argv);
  const definitions = engines(options);
  const suite = loadSuite(options.filters);
  const benchmarks = suite.benchmarks;
  if (benchmarks.length === 0) throw new Error("no benchmarks matched the requested filters");

  const report = {
    schema_version: 1,
    generated_at: new Date().toISOString(),
    boa_revision: gitRevision(),
    host: {
      platform: `${process.platform}-${process.arch}`,
      cpu: os.cpus()[0]?.model ?? "unknown",
      node: process.version,
      v8: process.versions.v8,
    },
    protocol: {
      runs: options.runs,
      warmup: options.warmup,
      process_samples: options.samples,
      paired_and_order_alternated: true,
      max_cv_pct: options.maxCvPct,
      targets_enforced: options.enforceTargets,
    },
    engines: definitions.map(({ name, command, prefix, mode }) => ({ name, command, prefix, mode })),
    benchmarks: [],
    headline: {},
    performance_targets: {},
    valid: true,
    failures: [],
  };

  console.log(
    `Boa/V8 paired comparison: ${benchmarks.length} workloads, ${options.samples} fresh processes/engine, ${options.runs} runs + ${options.warmup} warmups`,
  );
  for (const benchmark of benchmarks) {
    process.stdout.write(`measuring ${benchmark.name} ... `);
    const samples = Object.fromEntries(definitions.map((engine) => [engine.name, []]));
    const sinks = Object.fromEntries(definitions.map((engine) => [engine.name, []]));
    for (let sample = 0; sample < options.samples; sample++) {
      for (const engine of sampleOrder(definitions, sample)) {
        const result = runEngine(engine, benchmark, options);
        samples[engine.name].push(result.ns_per_run);
        sinks[engine.name].push(String(result.acc));
      }
      const expected = definitions.map((engine) => sinks[engine.name][sample]);
      if (!expected.every((sink) => sink === expected[0])) {
        report.failures.push(`${benchmark.name} sample ${sample + 1}: sink mismatch (${expected.join(", ")})`);
      }
    }

    const engineReports = Object.fromEntries(
      definitions.map((engine) => {
        const summary = distribution(samples[engine.name]);
        if (summary.cv_pct > options.maxCvPct) {
          report.failures.push(
            `${benchmark.name}/${engine.name}: CV ${summary.cv_pct.toFixed(2)}% > ${options.maxCvPct}%`,
          );
        }
        return [engine.name, { samples_ns_per_run: samples[engine.name], sinks: sinks[engine.name], distribution: summary }];
      }),
    );
    const result = {
      name: benchmark.name,
      category: benchmark.category,
      headline: benchmark.headline,
      reason: benchmark.reason ?? null,
      engines: engineReports,
      ratios: {
        boa_to_v8_jitless: ratio(engineReports.boa.distribution, engineReports["v8-jitless"].distribution),
        boa_to_v8: ratio(engineReports.boa.distribution, engineReports.v8.distribution),
        boa_jit_to_v8: ratio(engineReports["boa-jit"]?.distribution, engineReports.v8.distribution),
      },
    };
    report.benchmarks.push(result);
    console.log("done");
  }

  const headline = report.benchmarks.filter((benchmark) => benchmark.headline);
  report.headline = {
    workload_count: headline.length,
    boa_to_v8_jitless_geomean: geometricMean(headline.map((benchmark) => benchmark.ratios.boa_to_v8_jitless)),
    boa_to_v8_geomean: geometricMean(headline.map((benchmark) => benchmark.ratios.boa_to_v8)),
    boa_jit_to_v8_geomean: options.boaJit
      ? geometricMean(headline.map((benchmark) => benchmark.ratios.boa_jit_to_v8))
      : null,
    boa_to_v8_jitless_worst: maxOrNull(
      headline.map((benchmark) => benchmark.ratios.boa_to_v8_jitless),
    ),
    boa_jit_to_v8_worst: options.boaJit
      ? maxOrNull(headline.map((benchmark) => benchmark.ratios.boa_jit_to_v8))
      : null,
  };
  const completeHeadlineSuite = headline.length === suite.headlineWorkloadCount;
  const targetFailures = performanceTargetFailures(
    report.benchmarks,
    suite.targets,
    completeHeadlineSuite,
  );
  report.performance_targets = {
    definitions: suite.targets,
    enforced: options.enforceTargets,
    complete_headline_suite: completeHeadlineSuite,
    failures: targetFailures,
  };
  const sinkFailures = report.failures.filter((failure) => failure.includes("sink mismatch"));
  const noiseFailures = report.failures.filter((failure) => failure.includes("CV "));
  report.valid =
    sinkFailures.length === 0 &&
    (!options.failNoisy || noiseFailures.length === 0) &&
    (!options.enforceTargets || targetFailures.length === 0);

  console.log("\n* headline workload; times are p50 ns/main() from independent processes");
  const header = ["workload".padEnd(31), "boa".padStart(12), "v8-jitless".padStart(12), "boa/jitless".padStart(12)];
  if (options.boaJit) header.push("boa-jit".padStart(12), "v8".padStart(12), "boa-jit/v8".padStart(12));
  console.log(header.join(" "));
  for (const benchmark of report.benchmarks) printBenchmark(benchmark, options.boaJit);
  console.log(`\nheadline Boa/V8-jitless geomean: ${formatRatio(report.headline.boa_to_v8_jitless_geomean)}`);
  if (options.boaJit) console.log(`headline Boa-JIT/V8 geomean: ${formatRatio(report.headline.boa_jit_to_v8_geomean)}`);

  if (report.failures.length) {
    console.error("\nmeasurement warnings/failures:");
    for (const failure of report.failures) console.error(`  - ${failure}`);
  }
  if (targetFailures.length) {
    console.error("\nperformance target misses:");
    for (const failure of targetFailures) console.error(`  - ${failure}`);
  }
  if (options.json) {
    mkdirSync(dirname(resolve(options.json)), { recursive: true });
    writeFileSync(options.json, `${JSON.stringify(report, null, 2)}\n`);
    console.log(`wrote ${options.json}`);
  }
  if (!report.valid) process.exitCode = 1;
}

main();
