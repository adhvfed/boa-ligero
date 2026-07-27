# Microbench Baseline — Ignition-Parity Target

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

## Ignition-parity target

> **Done with Phase 1 when:** geomean over the **fair subset** is within
> **1.5× of Node `--jitless`**, with **no individual fair benchmark worse
> than 2.5×**.

Rationale:

- Node `--jitless` runs only Ignition (the interpreter tier). It is the right
  comparison point — V8 with JIT is a different problem (Phase 2).
- 1.5× geomean is achievable: we're at 3.43× now, and the remaining
  Ignition levers on the list (CallFrame restructure, `MaybeUninit` frame
  init, slow-path outlining for `JsValue::{add,sub,…}` and `get/set_by_name`,
  borrowed-fast-path mirroring for `…WithThis` variants) plausibly add up
  to a 2.3× speedup if each lands cleanly.
- 2.5× worst-case is a stretch on `method-call-mono` (currently 6.55×) —
  it implies fixing the polymorphic-call dispatch path specifically. May
  warrant its own targeted lever.

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
cargo build --release -p boa_benches --bin bench-compare-runner
RUNS=100 WARMUP=10 bash tools/bench-compare/compare.sh
```

## How to update this baseline

When a perf-significant change lands, rerun the harness and update the
"Headline numbers" table. Keep the "Ignition-parity target" line stable —
that's the moving goalpost we're aiming at, not the line of best fit
through whatever we shipped.
