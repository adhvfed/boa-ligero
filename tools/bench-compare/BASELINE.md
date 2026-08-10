# Historical Microbenchmark Baseline

> This 2026-05-19 snapshot predates the paired-process harness. It is retained
> as historical evidence, but its single process sample is not a binding
> performance result. Capture the next baseline with `compare.sh --binding`.

Captured on 2026-05-19 against Node v25.2.1, Bun (latest), and Boa at commit
`a5dd302c` (`perf(vm): drop the to_object clone on IC-hit property writes`).

## Configuration

- `RUNS=100 WARMUP=10` via `tools/bench-compare/compare.sh`
- macOS, Apple Silicon, plugged in, no background load minimised manually
- Single run per benchmark (geomean over benchmarks, not over runs)

## Headline numbers

| metric                                          | value                                                                 |
| ----------------------------------------------- | --------------------------------------------------------------------- |
| Boa vs Node `--jitless` — geomean (fair subset) | **3.43×** slower                                                      |
| Boa vs Node `--jitless` — worst (fair subset)   | 6.55× (method-call-mono)                                              |
| Boa vs Node `--jitless` — geomean (all 15)      | 6.68× slower (inflated by DCE-suspect benches)                        |
| Boa vs Node (full JIT) — geomean (fair subset)  | ~150× slower (expected — that's the JIT gap, not the interpreter gap) |

## Current parity targets

Interpreter: p50 geomean over the headline suite must be no slower than V8
`--jitless`, with no headline workload worse than 1.25x. Tiered/JIT mode must
be no slower than ordinary V8 on the same geomean, again with no headline
workload worse than 1.25x. Cold execution and browser-shaped workloads are
separate gates; warm microbench parity cannot substitute for either.

Rationale:

- Node `--jitless` runs only Ignition (the interpreter tier). It is the right
  comparison point — V8 with JIT is a different problem (Phase 2).
- The interpreter comparison isolates VM quality from the native tier.
- The ordinary V8 comparison is the product goal; V8 `--jitless` is a useful
  diagnostic reference, not the finish line.
- `tools/bench-compare/suite.json` owns headline membership explicitly. A
  benchmark cannot enter or leave the geomean as an accidental side effect.

## Per-benchmark results

| script                | boa/jitless | boa/node | DCE-suspect |
| --------------------- | ----------: | -------: | :---------: |
| array-numeric-sum     |       4.41× |   721.9× |             |
| closure-capture       |       4.31× |   149.9× |             |
| float-arith           |       1.93× |    12.1× |             |
| fn-call-flat          |       4.01× |   255.1× |             |
| global-counter        |       4.87× |   430.2× |             |
| int-arith             |       1.19× |    49.4× |             |
| method-call-mono      |       6.55× |   210.3× |             |
| object-create-literal |      23.58× |   142.2× |      ✓      |
| property-mega         |      12.56× |    53.1× |      ✓      |
| property-mono         |       6.31× |   809.2× |             |
| property-poly2        |      26.57× |  1591.4× |      ✓      |
| property-poly4        |       1.76× |    87.1× |             |
| property-set-mono     |       3.62× |   124.2× |             |
| recursion-fib         |      25.54× |   426.2× |      ✓      |
| string-concat         |      52.24× |    25.2× |      ✓      |

## DCE-suspect benchmarks

Five benchmarks show Node `--jitless` running implausibly fast (sometimes
faster than full-JIT Node), which strongly suggests dead-code elimination
inside `main()` despite the harness's XOR-on-return guard.

- `object-create-literal` — jitless 9.7M ns, node 1.6M ns. Likely the literal
  is hoisted / elided when its identity isn't observed.
- `property-mega` — jitless 2.7M ns vs node 0.64M ns. Same likely cause.
- `property-poly2` — jitless 20.7M ns, node 0.34M ns — node is 60× faster
  than its own jitless mode. The full JIT is clearly eliding the read.
- `recursion-fib` — jitless 21.3M ns for a meaningfully-sized fib is too fast.
  Possibly Node has a recursion intrinsic, possibly DCE.
- `string-concat` — jitless 137k ns vs node 283k ns (jitless _faster_ than
  full JIT). Almost certainly DCE.

**Action**: these benchmarks need a real sink (e.g., write to a global
counter, append to a long-lived array, return a value that is XOR'd into
the accumulator at the caller). Until fixed, they are excluded from the
Ignition-parity geomean.

## How to reproduce

```bash
cargo build --release -p boa_benches --bin bench-compare-runner --features jit
tools/bench-compare/compare.sh --binding --json /tmp/boa-v8.json
```

## How to update this baseline

When a performance change lands, rerun the binding harness and preserve the raw
JSON with the change's before/after evidence. Update committed summaries only
after the report passes sink validation and the 5% inter-process CV ceiling.
Targets do not move to fit the implementation.
