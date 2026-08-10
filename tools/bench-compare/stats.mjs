export function percentile(sorted, percent) {
  if (sorted.length === 0) return 0;
  const index = Math.round((percent / 100) * (sorted.length - 1));
  return sorted[Math.min(sorted.length - 1, Math.max(0, index))];
}
export function distribution(samples) {
  if (samples.length === 0) {
    return {
      p50: 0,
      p95: 0,
      p99: 0,
      max: 0,
      mean: 0,
      mad: 0,
      cv_pct: 0,
    };
  }
  if (samples.some((sample) => !Number.isFinite(sample) || sample < 0)) {
    throw new TypeError("benchmark samples must be finite, non-negative numbers");
  }

  const sorted = [...samples].sort((left, right) => left - right);
  const mean = sorted.reduce((total, sample) => total + sample, 0) / sorted.length;
  const p50 = percentile(sorted, 50);
  const deviations = sorted
    .map((sample) => Math.abs(sample - p50))
    .sort((left, right) => left - right);
  const variance =
    sorted.reduce((total, sample) => total + (sample - mean) ** 2, 0) / sorted.length;

  return {
    p50,
    p95: percentile(sorted, 95),
    p99: percentile(sorted, 99),
    max: sorted.at(-1),
    mean,
    mad: percentile(deviations, 50),
    cv_pct: mean === 0 ? 0 : (Math.sqrt(variance) / mean) * 100,
  };
}

export function geometricMean(values) {
  if (values.length === 0) return null;
  if (values.some((value) => !Number.isFinite(value) || value <= 0)) {
    throw new TypeError("geometric-mean inputs must be finite and positive");
  }
  return Math.exp(values.reduce((total, value) => total + Math.log(value), 0) / values.length);
}

export function performanceTargetFailures(benchmarks, targets, completeHeadlineSuite) {
  const failures = [];
  for (const [ratioName, target] of Object.entries(targets)) {
    const measured = benchmarks
      .filter((benchmark) => benchmark.headline)
      .map((benchmark) => ({ name: benchmark.name, ratio: benchmark.ratios[ratioName] }))
      .filter(({ ratio }) => ratio != null);

    for (const { name, ratio } of measured) {
      if (ratio > target.workload_max) {
        failures.push(
          `${ratioName}/${name}: ${ratio.toFixed(3)}x > ${target.workload_max.toFixed(3)}x workload target`,
        );
      }
    }

    if (completeHeadlineSuite && measured.length > 0) {
      const geomean = geometricMean(measured.map(({ ratio }) => ratio));
      if (geomean > target.geomean_max) {
        failures.push(
          `${ratioName}/headline-geomean: ${geomean.toFixed(3)}x > ${target.geomean_max.toFixed(3)}x suite target`,
        );
      }
    }
  }
  return failures;
}

export function parseResultLine(output) {
  const line = output
    .trim()
    .split(/\r?\n/)
    .findLast((candidate) => candidate.includes("ns_per_run="));
  if (!line) throw new Error("runner output did not contain ns_per_run");

  const fields = {};
  for (const match of line.matchAll(/([A-Za-z_][A-Za-z0-9_]*)=([^\s]+)/g)) {
    const [, name, raw] = match;
    fields[name] = /^-?\d+(?:\.\d+)?$/.test(raw) ? Number(raw) : raw;
  }
  if (!Number.isFinite(fields.ns_per_run) || fields.ns_per_run < 0) {
    throw new Error(`invalid ns_per_run in runner output: ${line}`);
  }
  if (!("acc" in fields)) throw new Error(`runner output did not contain acc: ${line}`);
  return fields;
}
