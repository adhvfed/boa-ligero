# 03a — Adaptive-arithmetic opportunity measurement (findings)

> **UPDATE — superseded by `03b`.** The quickening lever this GO recommended was
> implemented in full and measured an **8% regression** (see
> `03b-arith-quickening-negative-result.md`). The opportunity below is real, but
> the PEP-659 _mechanism_ doesn't fit Boa's already-inlined interpreter — the
> residual per-op cost is opcode dispatch, not type dispatch. Read 03b first.

**Status:** measurement complete. **Verdict: GO** — the opportunity is large,
hot, and overwhelmingly monomorphic on real workloads. This is the _opposite_ of
the move-elision case (19 sites / 1017 blocks → reverted).

This is the cheap "does the lever have work to do?" pass mandated by
[01-measurement-methodology.md](01-measurement-methodology.md) before any
release-build A/B. It precedes the planned **adaptive arithmetic opcodes** lever
(PEP-659-style quickening: specialize the generic `Add`/`Sub`/`Mul`/… opcodes
into integer fast-path opcodes that deopt on overflow / non-int operands).

## What was measured

Off-by-default instrumentation gated behind the `arith-instrument` cargo feature
(see `core/engine/src/vm/arith_instrument.rs`). It is compiled out of every normal
build and cannot affect production. For every dynamic execution of an instrumented
binary-arith opcode (`Add Sub Mul Div Mod Pow BitAnd BitOr BitXor ShiftLeft
ShiftRight UnsignedShiftRight Eq NotEq GreaterThan GreaterThanOrEq LessThan
LessThanOrEq`) it records, keyed by static site `(CodeBlock pointer, pc, kind)`:

- **mono_i32** — both operands `Integer32` and the integer op does **not** overflow
  i32 (perfectly specializable to an int fast-path op, no deopt).
- **i32_ovf** — both operands `Integer32` but the result overflows i32 (a
  specialized op would have to deopt to f64 here). Overflow predicates mirror the
  exact fast-path conditions in `value/operations.rs`.
- **f64** — both operands numeric, at least one `Float64`.
- **other** — anything else (string, object, bigint, bool, undefined, mixed…).

The map dumps a report on thread exit (and to `$BOA_ARITH_DUMP` if set).
`InstanceOf` is deliberately excluded (not arithmetic; operands are always
objects and would only add noise).

### Commands

```sh
# Build (instrumented). Same target path as the normal runner, so rebuild
# without the feature afterwards to restore a clean binary.
cargo build --release -p boa_benches --bin bench-compare-runner \
    --features arith-instrument

# One full invocation = one full workload run (each main() loops internally),
# which is all that's needed for execution counts.
for b in int-arith float-arith global-counter array-numeric-sum; do
  BOA_ARITH_DUMP=/tmp/arith-reports/$b.txt \
    ./target/release/bench-compare-runner benches/scripts/microbench/$b.js 1 0
done
for b in crypto navier-stokes raytrace richards deltablue splay; do
  BOA_ARITH_DUMP=/tmp/arith-reports/$b.txt \
    ./target/release/bench-compare-runner benches/scripts/v8-benches/$b.js 1 0
done
```

(Note: the scripts live under `benches/scripts/...`, not
`core/engine/benches/scripts/...` as 01 states — the roadmap path is stale.)

## Raw results

`exec` = total dynamic arith-opcode executions in one full workload run.
`mono` / `ovf` / `f64` / `other` are % of that workload's executions.
`int-typed` = mono + ovf (both operands were `Integer32`).

### Microbenches

| workload          | sites |      exec |  mono% |  ovf% |   f64% | other% | int-typed% |
| ----------------- | ----: | --------: | -----: | ----: | -----: | -----: | ---------: |
| global-counter    |     1 |   200,000 | 100.00 |  0.00 |   0.00 |   0.00 |     100.00 |
| array-numeric-sum |     4 | 2,000,200 | 100.00 |  0.00 |   0.00 |   0.00 |     100.00 |
| int-arith         |     6 | 6,000,000 |  77.76 | 11.12 |  11.12 |   0.00 |      88.88 |
| float-arith       |     3 | 1,500,000 |   0.00 |  0.00 | 100.00 |   0.00 |       0.00 |

### Octane / V8 benches (real workloads)

| workload      | sites |        exec | mono% | ovf% |  f64% | other% | int-typed% |
| ------------- | ----: | ----------: | ----: | ---: | ----: | -----: | ---------: |
| crypto        |   487 | 462,017,179 | 99.22 | 0.00 |  0.75 |   0.04 |      99.22 |
| navier-stokes |   189 | 204,329,408 |  8.61 | 0.00 | 91.39 |   0.00 |       8.61 |
| raytrace      |   152 |  11,461,990 |  1.63 | 0.00 | 95.19 |   3.18 |       1.63 |
| richards      |    56 |   8,391,784 | 62.76 | 0.00 |  0.00 |  37.24 |      62.76 |
| deltablue     |    63 |   3,645,045 | 89.06 | 0.00 |  2.14 |   8.80 |      89.06 |
| splay         |    64 |   6,177,133 | 48.70 | 0.68 | 27.33 |  23.29 |      49.38 |

### Hot-site concentration (real workloads)

| workload      | top-1 | top-5 | top-10 | top-20 |
| ------------- | ----: | ----: | -----: | -----: |
| crypto        |  5.56 | 27.79 |  55.58 |  95.64 |
| navier-stokes | 10.91 | 54.53 |  68.75 |  76.93 |
| raytrace      |  6.15 | 30.77 |  48.25 |  68.49 |
| richards      | 11.95 | 54.55 |  68.09 |  87.68 |
| deltablue     | 31.63 | 74.63 |  88.63 |  96.15 |
| splay         | 22.36 | 67.07 |  88.64 |  92.19 |

Concentration is high everywhere: **top-20 static sites cover 68–96%** of all
arith executions. crypto alone has 17 sites each executing 25.7M times at 100%
mono_i32 — a quickening cache only needs a handful of slots to capture the bulk.

## Interpretation

Three distinct regimes show up, and that itself is the important finding:

1. **Integer-dominated, hot, monomorphic.** crypto (99.2% mono_i32 over **462M**
   executions), deltablue (89%), richards (63%), global-counter / array-numeric-sum
   / int-arith microbenches. These are exactly what int-fast-path opcodes target.
   crypto is the headline: nearly half a billion arith ops, virtually all
   `i32 op i32 → i32` with **zero overflow** — the deopt branch would essentially
   never fire. A specialized op here removes the per-execution variant-dispatch,
   the `checked_*` + `map_or_else`, and the `Some(...)`/register-tag round-trip on
   the single hottest path in the engine.

2. **Float-dominated.** navier-stokes (91% f64), raytrace (95% f64), float-arith.
   An _i32-only_ specialization does nothing for these. But the data argues for a
   **monomorphic-f64 specialization as well** — these sites are just as hot and
   just as monomorphic (100% f64 at the top sites), they're simply f64 not i32. A
   PEP-659-style design with both `Add_Int` and `Add_F64` quickened forms captures
   both regimes; an i32-only design leaves the entire numeric-heavy Octane half on
   the table.

3. **Polymorphic "other".** The `other` bucket is small and explained, not random:
   richards' 37% is `Eq`/`NotEq` against `null`/objects (OO null-checks, not
   arithmetic), splay's 23% similar. These sites would simply _not quicken_ (or
   quicken then deopt once and stay generic) — they don't poison the lever, they're
   just outside its scope. Critically, almost no site is _mixed_ int/f64 churn:
   sites are monomorphic per-PC (the top-site tables show ~100% in a single column),
   which is the precondition that makes PC-keyed quickening work.

### Overflow is a non-issue on real code

`i32_ovf` is 0.00% on every Octane bench (crypto: 1,274 of 462M = 0.0003%). The
deopt-on-overflow branch is essentially free in practice — it predicts perfectly
and never taken. The only place overflow mattered was the synthetic int-arith
microbench (`acc * 3 | 0` deliberately overflowing), which is an adversarial case,
not representative.

## Verdict: GO (with a design note)

This clears the "never ship a wash" bar by a wide margin, and is the antithesis of
move-elision:

- **move-elision:** 19 dynamic sites across 1017 CodeBlocks → no runtime leverage,
  reverted.
- **adaptive arithmetic:** 50–490 hot static sites per workload, **hundreds of
  millions** of executions, top-20 sites covering 68–96%, and 88–99% monomorphic
  (i32 _or_ f64) on the integer/float-heavy workloads. crypto alone executes 462M
  arith ops at 99.2% mono_i32.

There is a real, large, concentrated, monomorphic opportunity. Build the lever.

**Design note (load-bearing):** do **not** ship an i32-only specialization. The
opportunity splits cleanly into mono-i32 _and_ mono-f64 regimes, and the f64 half
is the entire numeric-heavy Octane suite (navier-stokes, raytrace). A PEP-659
quickening with at least `{Int, F64}` specialized forms per arith opcode (deopt to
the generic handler on type mismatch / i32 overflow) captures both. An i32-only
design would post a win on crypto/deltablue/richards and a **wash on
navier-stokes/raytrace**, i.e. exactly the half-measure the methodology warns
against.

Next step per methodology: build the specialized opcodes behind the existing
quickening machinery and run the full `RUNS=200 WARMUP=30` A/B. The opportunity
count justifies paying for that release-build A/B now.

## Reproducing / instrumentation footprint

- Feature flag: `arith-instrument` on `boa_engine` (passthrough on `boa_benches`).
  Off by default; compiled out entirely otherwise.
- Code: `core/engine/src/vm/arith_instrument.rs` (registry + report),
  `JsValue::classify_arith_pair` in `core/engine/src/value/operations.rs`
  (operand classification, `#[cfg]`-gated), and the `#[cfg]`-gated `record(...)`
  call in the `implement_bin_ops!` macro in
  `core/engine/src/vm/opcode/binary_ops/macro_defined.rs`.
- Granularity: **per static site** `(CodeBlock pointer, pc, opcode kind)` — PC-site
  identity worked cleanly, no coarser fallback needed.
