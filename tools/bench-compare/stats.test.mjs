import test from "node:test";
import assert from "node:assert/strict";

import { distribution, geometricMean, parseResultLine, percentile } from "./stats.mjs";

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
