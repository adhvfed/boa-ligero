# Boa Microbenchmark Suite

Targeted microbenchmarks for measuring specific engine hot-paths in isolation.
Each `.js` file defines a `main()` function that the harness runs many times.

## Running with Boa (Criterion)

```
cargo bench --bench scripts -- microbench
```

Each script is run as a Criterion benchmark group named after its path.

## Running against V8

```
cargo build --release -p boa_benches --bin bench-compare-runner --features jit
tools/bench-compare/compare.sh property-mono
tools/bench-compare/compare.sh --binding --json /tmp/boa-v8.json
```

The comparison tool runs the same JS under Boa, V8, and V8 `--jitless` in
fresh, order-alternated process pairs. It validates the observable result sink
and reports p50/p95/p99/max, median absolute deviation, and coefficient of
variation. `--binding` uses nine processes per engine, 200 measured calls, 80
warmups, includes Boa's tiered/JIT mode, and fails noisy measurements.
It also enforces the suite's parity targets: headline geomean no slower than
1.00x and every selected headline workload no slower than 1.25x for both
Boa/V8-jitless and Boa-JIT/V8. A filtered run enforces the per-workload target;
only a complete headline run can assert the suite geomean. Use
`--enforce-targets` with a shorter exploratory protocol when an early failing
gate is useful.

`tools/bench-compare/suite.json` owns the targets and names the headline set.
New scripts must be classified explicitly, so benchmark coverage cannot change
silently.

## Design rules

- The work in `main()` should dominate over the loop overhead (target ≥1ms per
  call so timing noise is small).
- No `console.log` or other I/O inside `main()`.
- Return a value to defeat dead-code elimination in JIT engines.
- Setup (allocations, etc.) belongs OUTSIDE `main()` so it isn't part of the
  measurement.
- Files starting with `_` are ignored by the harness.
