import test from "node:test";
import assert from "node:assert/strict";

import {
  distribution,
  geometricMean,
  parseResultLine,
  percentile,
  performanceTargetFailures,
} from "./stats.mjs";

test("distribution reports tails, dispersion, and stable nearest-rank percentiles", () => {
  const result = distribution([5, 1, 4, 2, 3]);
  assert.deepEqual(result, {
    p50: 3,
    p95: 5,
    p99: 5,
    max: 5,
    mean: 3,
    mad: 1,
    cv_pct: Math.sqrt(2) / 3 * 100,
  });
  assert.equal(percentile([1, 2, 3, 4, 5], 0), 1);
  assert.equal(percentile([1, 2, 3, 4, 5], 100), 5);
});
test("geometric mean rejects values that cannot form a meaningful ratio", () => {
  assert.equal(geometricMean([0.5, 2]), 1);
  assert.equal(geometricMean([]), null);
  assert.throws(() => geometricMean([1, 0]), /finite and positive/);
});

test("performance targets distinguish workload and complete-suite failures", () => {
  const benchmarks = [
    {
      name: "fast",
      headline: true,
      ratios: { boa_jit_to_v8: 0.8, boa_to_v8_jitless: 1.1 },
    },
    {
      name: "slow",
      headline: true,
      ratios: { boa_jit_to_v8: 1.3, boa_to_v8_jitless: 1.2 },
    },
    {
      name: "diagnostic",
      headline: false,
      ratios: { boa_jit_to_v8: 100, boa_to_v8_jitless: 100 },
    },
  ];
  const targets = {
    boa_jit_to_v8: { geomean_max: 1, workload_max: 1.25 },
    boa_to_v8_jitless: { geomean_max: 1.25, workload_max: 1.25 },
  };

  assert.deepEqual(performanceTargetFailures(benchmarks, targets, false), [
    "boa_jit_to_v8/slow: 1.300x > 1.250x workload target",
  ]);
  assert.deepEqual(performanceTargetFailures(benchmarks, targets, true), [
    "boa_jit_to_v8/slow: 1.300x > 1.250x workload target",
    "boa_jit_to_v8/headline-geomean: 1.020x > 1.000x suite target",
  ]);
});

test("result parsing accepts Boa diagnostics and uses the last result line", () => {
  const result = parseResultLine(
    "setup chatter\nelapsed_ns=1200 runs=4 ns_per_run=300 acc=-7 mode=jit deopts=0\n",
  );
  assert.equal(result.ns_per_run, 300);
  assert.equal(result.acc, -7);
  assert.equal(result.mode, "jit");
  assert.equal(result.deopts, 0);
});

test("result parsing rejects incomplete output", () => {
  assert.throws(() => parseResultLine("nothing useful"), /ns_per_run/);
  assert.throws(() => parseResultLine("ns_per_run=3"), /acc/);
});
