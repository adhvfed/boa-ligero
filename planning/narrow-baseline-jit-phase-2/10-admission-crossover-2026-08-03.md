# Admission crossover and scheduler finding — 2026-08-03

This checkpoint tests Slice 2A's proposed static admission rule. The rule was
implemented as a local prototype, measured, and reverted because it failed the
complete-workload stop/go gate. The source-free diagnostic additions used to
reach that decision remain landed.

## Static shape evidence

Diagnostic schema 2 adds the decoded native candidate's instruction count,
static backward branches, calls, and property reads. It retains no source,
URL, function name, property name, value, or pointer.

The three Gate P engine workloads confirmed that their existing native entries
are tiny straight-line bodies rather than useful loops:

| Workload | Warm native bodies | Static shape | Runtime result |
|---|---:|---|---|
| Crypto | 1 | 2 instructions, no backward branch | entry-guard deopt |
| DeltaBlue | 3 | 4, 2, and 2 instructions; no backward branch | normal returns |
| Earley-Boyer | 1 | 4 instructions, no backward branch | argument-type deopt |

The W0 browser kernel remains the positive counterexample: it has 24 native
instructions and a validated backward branch, produces the visible checksum
`499500000`, and previously recorded 999 normal native returns with no deopt.

## Straight-line crossover

Seven generated helpers performed 0, 1, 2, 4, 8, 12, or 16 additions per
call. Each control used 70 warmups and five timed runs. The caller performed
100,000 calls per run; sinks matched in interpreter and JIT modes.

| Additions | Native instructions | Interpreter ns/run | JIT ns/run | Interpreter / JIT |
|---:|---:|---:|---:|---:|
| 0 | 9 | 6,729,841 | 10,061,641 | 0.669x |
| 1 | 12 | 7,925,525 | 10,730,133 | 0.739x |
| 2 | 15 | 8,722,550 | 10,645,733 | 0.819x |
| 4 | 21 | 10,012,791 | 10,602,066 | 0.944x |
| 8 | 33 | 14,315,633 | 14,175,091 | 1.010x |
| 12 | 45 | 17,991,466 | 11,550,108 | 1.558x |
| 16 | 57 | 20,274,608 | 11,269,566 | 1.799x |

This selected a deliberately conservative prototype: admit a fully native
body when it has a validated backward branch or at least 45 decoded native
instructions. Reject native-ineligible bodies without installing the complete-
semantics shim in the context-owned tier. Explicit low-level JIT calls retained
the shim fallback.

## Prototype result

The prototype passed the full focused JIT module (30 active tests at that
checkpoint); after a second suppression case was added, both suppression
regressions also passed. It inspected each rejected code block once, emitted
neither native code nor a shim, and retained a test-only work-floor override
for guard/ABI coverage. Despite that, it made the losing controls worse:

| Additions | Interpreter ns/run | Prototype JIT ns/run | Interpreter / JIT |
|---:|---:|---:|---:|
| 0 | 6,636,916 | 13,332,458 | 0.498x |
| 4 | 11,710,741 | 21,188,875 | 0.553x |
| 8 | 14,117,141 | 28,605,833 | 0.494x |
| 12 | 17,345,575 | 12,603,458 | 1.376x |
| 16 | 20,357,658 | 12,945,000 | 1.572x |

The rejected rows compiled zero artifacts and reported two source-free
suppression decisions. Their remaining cost is therefore not compilation or a
native entry transition.

Code inspection explains the result. With the tier enabled,
`run_with_jit_backend` owns the entire interpreter dispatch loop: each opcode
reacquires frame/code state and runs tiering-side entry, call-target, and
backedge observation around `execute_one`. A rejected helper still executes
every opcode through that wrapper. Tiny native helpers had masked part of this
tax by shortening their interpreted bodies; suppressing them exposed it.

## Decision and revised order

The static rule is falsified as a standalone fix and was not committed. Do not
tune the work floor or widen native coverage to hide this scheduler cost.

Slice 2A is split:

1. **2A1 — interpreter fast path with dormant tiering.** Move observation to
   actual function-entry, call, and backward-edge events, or otherwise let an
   interpreter-only frame use the ordinary dispatch path between those events.
   Preserve exact instruction budgets, call-target feedback, deopt handoff,
   exception behavior, and source-free counters.
2. **2A2 — admission revisited.** Re-run this exact crossover after 2A1. A
   losing rejected row must be within 5% of JIT-disabled interpretation, W0
   must retain its native loop/checksum, and 45/57-instruction controls must
   retain a clear win before any production admission policy lands.
3. **2B — guarded receiver loading.** Remains the next coverage candidate only
   after 2A closes. `This`, binding reads, OSR, compiled calls, and direct
   storage must not be used to compensate for scheduler overhead.

This is a useful negative result: native-entry admission is necessary for code
growth and deopt control, but it cannot be evaluated honestly until enabling
the tier is cheap for code that stays interpreted.
