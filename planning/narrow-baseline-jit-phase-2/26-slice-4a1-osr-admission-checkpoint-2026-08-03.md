# Slice 4A1.5c cold-OSR admission checkpoint — 2026-08-03

Status: the isolated one-shot admission subgate passes in Boa `229ac2e4`.
Slice 4A1, Gate O, and 4A1.5c remain open until the complete Decision-A
micro/engine/W0 rollback matrix passes. JIT remains build-time and runtime
opt-in.

## Measurement correction

The existing `jit` runner mode cannot measure a fresh-process cold OSR entry:
before its production-threshold context it deliberately compiles and executes
a threshold-1 PC-zero control in the same process. It also exposes only the
aggregate `native_entries`, which includes loop entries, and did not print the
OSR counters needed to distinguish the two entry kinds.

Boa `229ac2e4` adds a separate `osr-cold` mode. It requires exactly one timed
call and zero warmups, performs parse/bytecode/top-level setup before timing,
enables unchanged production thresholds, and performs no earlier native work
or threshold override. Its post-timing output includes whole-function and OSR
compilation/entry counts, compilation time, continuation/deopt counts, and a
derived `function_native_entries` count. Optional diagnostics use a distinct
schema-8 single-sample envelope.

A runner-level production test executes a 300-backedge loop and proves one OSR
compilation/entry/continuation, no whole-function compilation/entry, no deopt,
and the expected sink. A second run proves bounded diagnostics with zero drops.
The same binary target has three feature-disabled parser/validation tests.

## Build and fixture identity

- Boa: `229ac2e4624ad24c350e9f23509aefcf087f31d1`.
- release runner SHA-256:
  `6270ebe26958427536a098f08781f7750a4a7f122d9125458e2212a80482851d`.
- `one-shot-loop.js` SHA-256:
  `0f54effe6b51cb7d0b29b88f478474cd3e9576e8a44f48fa1a6e90b12afef223`.

The runner was built with `--release --features jit`. Each table row launches a
fresh process. Odd pairs run interpreter first; even pairs run OSR first.
Diagnostics are disabled in all headline samples.

## Raw samples

| Pair | Order | Interpreter ns | Cold OSR ns | OSR compile ns |
| ---: | --- | ---: | ---: | ---: |
| 1 | interpreter first | 26,149,333 | 5,974,167 | 417,833 |
| 2 | OSR first | 29,126,167 | 6,116,041 | 366,792 |
| 3 | interpreter first | 28,219,000 | 6,179,458 | 418,750 |
| 4 | OSR first | 28,247,958 | 6,108,500 | 396,333 |
| 5 | interpreter first | 28,061,458 | 6,248,041 | 440,875 |
| 6 | OSR first | 27,987,000 | 6,120,417 | 394,375 |
| 7 | interpreter first | 28,077,792 | 6,160,708 | 432,042 |

Every row reports the same observable accumulator, `-1455759936`. Every OSR
row reports one function entry, zero whole-function compilations, zero
whole-function native entries, one OSR compilation, one OSR entry, one normal
continuation, zero entry rejections, and zero deopts. All seven paired speedups
are between 4.377× and 4.762×.

Median interpreter time is 28,077,792 ns. Median cold-OSR time is 6,120,417 ns,
including a median 417,833 ns synchronous loop compilation. This is a 4.588×
speedup, or 78.202% lower elapsed time, and passes the required 2× floor.

A separate diagnostics-enabled `osr-cold` process reports schema 8, the same
one compile/entry/continuation, no rejection/deopt/suppression, and zero drops
in every detailed record class. It is not included in headline timing.

## Verification

- JIT runner tests: 5 passed.
- feature-disabled runner tests: 3 passed.
- affected runner warning-denying Clippy passes with and without `jit`.
- the release CLI rejects any `osr-cold` invocation other than one run and zero
  warmups with status 2.

## Remaining rollback gate

Repeat the checksummed Decision-A micro and engine controls using the unchanged
legacy mode, then run W0 through a clean, identified Ligero build. Every
negative/noncandidate row must remain within 5% of its paired interpreter
median. W0 must retain its exact sink, 387 display items, 258 paint segments,
8,159,754 accounted bytes, PC-zero entry, and at least a 20% cold-load win.
Record raw samples, release hashes, and separate zero-drop diagnostics. A
failure still disables or reverts only the 4A1.4 scheduler edge; a pass permits
4A1.R, not default JIT or another execution ABI.
