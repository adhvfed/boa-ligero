# 01 — Measurement methodology

Performance work is only as trustworthy as its measurements. This chapter is the
discipline that every other chapter assumes.

## The harness

- `tools/bench-compare/compare.sh [script-filter]` runs each script across
  engines (Boa, `node`, `node --jitless`, etc.) via `runner.mjs`, which calls a
  global `main()` and reports `elapsed_ns`.
- Microbench scripts: `core/engine/benches/scripts/microbench/`.
- **Real workloads**: the Octane/V8 suite lives in
  `core/engine/benches/scripts/v8-benches/` (richards, deltablue, raytrace,
  crypto, navier-stokes, splay, regexp, earley-boyer). Each is self-contained
  (bundles its own `BenchmarkSuite`) and prints a `SCORE` (higher = faster).

## Rules

1. **Run at `RUNS=200 WARMUP=30`, not the default.** At RUNS=30–50, individual
   microbenchmarks swing ±15–40% between runs — enough to fake an improvement or
   hide a regression. We once measured the _same_ property-read change as both
   +11% and −5% at low N. At RUNS=200 variance tightens enough to trust a result.

   ```
   cargo build --release -p boa_benches --bin bench-compare-runner
   RUNS=200 WARMUP=30 bash tools/bench-compare/compare.sh [filter]
   ```

2. **A/B by binary swap, not by faith.** Build, run 3×; `git stash`; rebuild; run
   3×; compare _per-script geomeans_. Never trust a single run. A release build
   is ~4–5 min, so batch your reps.

3. **Beware dead-code elimination.** Five baseline benchmarks are flagged
   DCE-suspect in `BASELINE.md` (`object-create-literal`, `property-mega`,
   `property-poly2`, `recursion-fib`, `string-concat`) — `node --jitless` runs
   them implausibly fast because the optimizer elides unobserved work. They are
   excluded from the parity geomean. Any new benchmark needs a **real sink**
   (XOR the result into an accumulator that's returned/printed).

4. **Don't commit a wash.** If a change doesn't show a _clear_ win at high N,
   revert it — the added complexity isn't worth a tie, and a tie today is a
   maintenance cost forever.

## Measure the opportunity before paying for the A/B

The expensive part is the release build + timing loop. Before that, ask the
cheap question: _does this optimization even have work to do on real code?_

This is the Move-elision lesson. Instead of two ~5-minute release builds and a
timing harness, we added a one-line counter to the pass and ran the Octane suite
once: **19 elision sites across 1017 CodeBlocks.** That number alone killed the
idea — 19 static sites cannot move a runtime geomean, and the pass adds a
full-bytecode decode to _every_ CodeBlock at compile time. No timing run needed.

Generalize this. For any specialization/cache/rewrite lever:

- Instrument the **hit/opportunity count** first (env-gated `eprintln!` or an
  atomic counter), run it over Octane, and sum.
- If the count is negligible on real workloads, stop — the mechanism is absent,
  not just untuned.
- Only when the opportunity count is large do you build release binaries and
  measure wall-clock.

## Profiling, not just timing

Wall-clock tells you _whether_; a profiler tells you _why_ and sizes the lever
before you build it.

- **`cargo flamegraph`** / `perf record` on a release `bench-compare-runner` to
  find the hot handlers.
- **`perf stat -e branch-misses,branches`** is the specific instrument for the
  [dispatch](05-dispatch.md) question — it directly measures whether the central
  indirect call is mispredicting, which is the only thing that justifies the
  (high-friction) threaded-dispatch work.
- **Allocation counts** (a counter in `boa_gc`'s `alloc_gc`,
  `core/gc/src/lib.rs:131`) size the [GC](06-gc-and-allocation.md) lever.

## Definition of done for Phase 1

From `BASELINE.md`: geomean over the fair subset within **1.5×** of
`node --jitless`, with **no fair benchmark worse than 2.5×**. Re-capture
`BASELINE.md`'s headline table whenever a perf-significant change lands; keep the
parity target line fixed — it's the goalpost, not the line of best fit.
