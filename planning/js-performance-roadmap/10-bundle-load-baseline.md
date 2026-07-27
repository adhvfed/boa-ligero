# 10 — Bundle load baseline: the July parse/compile burst on real bundles

**Status:** first recorded measurement. This is the number W4 of the
demo-launch campaign (`ligero-browser/planning/demo-launch/README.md`) exists
to produce: a before/after on real corpus bundles for the run of parse/compile
commits that landed 2026-07-24.

**Date measured:** 2026-07-27 / 2026-07-28 (spanned midnight local time).
**Machine:** Apple M4, 10 cores, macOS (Darwin 24.3.0), on battery/AC not
controlled for. Another agent was concurrently running release (LTO,
`codegen-units=1`) Rust builds of the `ligero` browser in a sibling repo on the
same machine for most of the measurement window — see "Load conditions"
below.

## What was compared

**A (pre-burst baseline):** `5762658c88cdbb486a5fc1bcc11e6db769a96748` —
`5762658c`'s own commit, "Fix normal completion through finally jump tables",
i.e. the parent of the first burst commit. Built in a throwaway
`git worktree` (removed after building; main working tree was never touched).
`examples/src/bin/bundle_bench.rs` doesn't exist at this commit — it was added
three commits after the burst ended (`cc0e85d5`, 2026-07-24) — so the harness
source file was copied into the worktree unmodified before building. The
harness's `boa_engine` API surface it uses (`Context::default`, `Source`,
`script::Script`) is unchanged across the burst, so this is a like-for-like
build of the same benchmark against the old engine.

**B (current):** `6f7dd2d13537bbf7000bbab49dca312a91a81a07` — repo `HEAD` at
measurement time.

**The burst** is 18 commits, `3d9d5ff7` ("Speed up ASCII identifier lexing")
through `56a97e22`, landing over about 13 hours on 2026-07-24: lexer/parser
fast paths (ASCII identifier lexing, newline-skipping, current-token
lookahead), reduced AST/string cloning during parsing and scope analysis, and
a UTF-8 in-memory source fast path. `56a97e22`→`HEAD` (`6f7dd2d1`) is a single
formatting-only commit, not part of the burst.

Both binaries are release builds (`cargo build --release -p boa_examples --bin
bundle_bench`), built and run on the same machine back-to-back.

## Method: A/B by binary swap

Per `planning/js-performance-roadmap/01-measurement-methodology.md` rule 2
("A/B by binary swap, not by faith"), adapted for this harness: two prebuilt
binaries (`bundle_bench_A_baseline_5762658c`,
`bundle_bench_B_head_6f7dd2d1`, both under `tools/bench-compare/`), run
interleaved A/B/A/B (or more — see nrk-no below) per bundle. Each invocation
is one process, a fresh `Context` per iteration internally, 15 iterations,
reporting the **median** parse and compile time for that process run. The
number below is the **median of the process-run medians** per side.

## Load conditions (read before trusting the numbers)

This machine had a second agent running heavy concurrent Rust release builds
(`ligero` browser, LTO, `codegen-units=1`) in `ligero-browser/` for most of
the session. Per the noise-discipline rule in this task's brief, CPU load was
checked before each measurement block (`top -l 1`, target <30% total):

- **react-dev, vg-no, skeidar-no:** measured back-to-back in one block at
  ~14–17% total CPU (idle 73–76%), no `rustc`/`cargo build` processes running
  at the time. Two process runs per side (A/B/A/B), tight agreement between
  the two same-side runs (see table — all within ~8% of each other).
- **github-repo:** dropped from the corpus before any timing was recorded —
  every candidate bundle fails to parse regardless of load (see
  `tools/bench-compare/bundle-corpus/README.md`).
- **nrk-no — noise incident, redone.** The first A/B/A/B attempt for this
  bundle coincided with load climbing to ~43% total as the other agent's
  build resumed; the two same-side (A vs A) runs disagreed by 2× (25.4ms vs
  54.1ms parse) — a textbook noise-poisoned result, and it was **not**
  recorded as a number. It was rerun as 6 interleaved A/B pairs once a
  process-existence check (`pgrep -x rustc`, not just an instantaneous `top`
  percentage — a single low `top -l 1` sample can land between a bursty
  build's compilation units and misread as quiet) confirmed no active
  compiles, at ~19–26% total CPU. That rerun is tight: all 6 A-side parse
  samples in 23.1–24.5ms, all 6 B-side in 18.7–19.8ms. The nrk-no row below
  is from this rerun, not the original attempt.

No block was fully idle-machine (0% contention) — this is an honest
first-pass number on a shared machine, not a clean-room result. The
consistency within each side's repeated runs (≤8% spread on the three
larger bundles, ≤4% on the redone nrk-no) is the basis for trusting the
cross-side deltas below, which are far larger than that spread.

## Results

`parse` / `compile` are medians of process-run medians, milliseconds.
`Δ%` = `(B − A) / A × 100`, negative = B (current) is faster.

| Bundle       | Bytes     | A parse (ms) | B parse (ms) | Δ parse | A compile (ms) | B compile (ms) | Δ compile |
| ------------ | --------: | ------------: | ------------: | ------: | ---------------: | ---------------: | --------: |
| react-dev    |   577,725 |        142.28 |        114.09 | **−19.8%** |             48.55 |             36.93 | **−23.9%** |
| vg-no        | 1,165,867 |        187.21 |        138.58 | **−26.0%** |             70.34 |             43.61 | **−38.0%** |
| skeidar-no   |   723,783 |        176.87 |        148.10 | **−16.3%** |             51.02 |             41.56 | **−18.5%** |
| nrk-no       |   131,359 |         23.91 |         19.48 | **−18.5%** |              6.97 |              5.50 | **−21.1%** |

(`github-repo` excluded — see "What was compared" and the corpus README; every
bundle GitHub serves on this page is an ES module and none of them parse under
this harness's `Script::parse`-only entry point, on A or B.)

## Interpretation

The July 24 burst is a real, substantial win on real bundles, not just on
microbenchmarks: **parse time down 16–26%, compile time down 18–38%** across
four production bundles ranging 131 KB to 1.17 MB, with the effect direction
and rough magnitude consistent across all four (no bundle is a wash or a
regression). The compile-time win is consistently larger than the parse-time
win, which is directionally consistent with the burst's own commit list —
several of the 18 commits target compilation/scope-analysis cloning
(`Compile declarations without temporary AST clones`,
`Avoid cloning declarations during scope analysis`, `Reuse interned strings
in bytecode constants`, `Avoid temporary strings during bytecode
compilation`) rather than pure lexing/parsing.

This is a genuinely honest first number for W4, with two caveats stated
plainly rather than smoothed over:

1. **`github-repo` could not be measured at all.** Boa's own real-bundle
   harness only supports classic scripts; github.com's bundle output is 100%
   ES modules. If corpus TTFP work ever needs a same-origin GitHub page,
   `bundle_bench` needs a `Module::parse` mode first — that's new scope, not
   covered here.
2. **The machine was not quiet the whole time**, and one bundle's first
   attempt was visibly noise-poisoned and had to be redone. The numbers above
   are the ones that passed a consistency check (tight agreement between
   repeated same-side runs), not the first numbers produced.

## Reproducing

```sh
# B — current HEAD
cargo build --release -p boa_examples --bin bundle_bench
cp target/release/bundle_bench tools/bench-compare/bundle_bench_B_head_<sha>

# A — pre-burst baseline, via throwaway worktree
git worktree add /tmp/boa-baseline 5762658c88cdbb486a5fc1bcc11e6db769a96748
cp examples/src/bin/bundle_bench.rs /tmp/boa-baseline/examples/src/bin/bundle_bench.rs
(cd /tmp/boa-baseline && cargo build --release -p boa_examples --bin bundle_bench)
cp /tmp/boa-baseline/target/release/bundle_bench tools/bench-compare/bundle_bench_A_baseline_<sha>
git worktree remove /tmp/boa-baseline --force

# Check load before each block: top -l 1 | grep "CPU usage" (want <30% total)
# and pgrep -x rustc / cargo build --release should show nothing.

./tools/bench-compare/bundle_bench_A_baseline_<sha> tools/bench-compare/bundle-corpus/<bundle>.js
./tools/bench-compare/bundle_bench_B_head_<sha> tools/bench-compare/bundle-corpus/<bundle>.js
# Interleave A/B/A/B (more reps for small/fast bundles), take medians.
```

## Next steps for W4

- Re-run this table before/after any future parse/compile-targeted change, on
  a genuinely idle machine if one becomes available, to sharpen the
  uncertainty bars.
- Decide whether `bundle_bench` needs a `Module::parse` mode — github-repo
  (and likely other module-based sites in the demo corpus) can't be measured
  without one.
- Per the campaign charter, further parse/compile work is in scope for W4
  only where it moves corpus TTFP — this baseline doesn't yet connect
  bundle-level parse/compile time to the live-page TTFP numbers in
  `ligero-browser/tools/bench/gap-table.md`; that link is unmade and would be
  the natural next step before prioritizing more parser/compiler work here.
