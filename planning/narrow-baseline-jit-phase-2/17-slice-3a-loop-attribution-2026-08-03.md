# Slice 3A loop-attribution checkpoint — 2026-08-03

## Result

Boa `0233f60f` and Ligero `48573227` complete the loop portion of Slice 3A.
Diagnostic schema 6 now reports bounded, source-free interpreted loop sites
keyed by runtime-local code ID, header PC, and backedge PC. The records retain
dynamic backedge count, first hotness crossing, observations after the current
frame's entry decision closed, and a conservative static OSR-candidacy result.

This is attribution only. It does not add a nonzero-PC cache key, live-register
materialization, OSR entry, native region, or new production admission path.
Static candidacy therefore means only that the observed header-to-backedge
range passes the narrow opcode and metadata screen.

## Evidence

The durable eligible and statically ineligible one-shot fixtures each report
exactly 2,000,000 interpreted backedges with one hotness crossing in an
explicit diagnostic run. The eligible site is a static candidate; the
ineligible site reports its first bitwise blocker and PC. Repeated denied
callees remain observable after dormant-frame dispatch, and a zero record cap
reports every omitted observation through the dropped counter rather than an
unbounded dropped-site set.

Seven fresh-process, diagnostics-disabled release pairs preserve Gate H:

| Workload | Interpreter median | JIT median | JIT tax |
| --- | ---: | ---: | ---: |
| eligible one-shot loop | 27.546 ms | 27.825 ms | +1.011% |
| statically ineligible loop | 36.795 ms | 37.142 ms | +0.942% |

Relative to the prior Gate H JIT medians, those results are +0.350% and
-0.251%. They remain inside the recorded 5% parity guardrail and show that the
diagnostics-off no-op path did not reintroduce per-opcode tiering work.

## Storage-attribution refinement

The remaining Slice 3A work is split into two independently reviewable parts:

1. **Interpreted storage sites.** Before an interpreted read executes, record
   only a coarse named, dense, computed, or specialized-length category and
   the existing named/dense inline-cache hit, miss, or not-applicable state.
   Inspection must be pure: it may not convert a key, invoke a getter/proxy,
   retain a value, object, shape, property name, source, URL, or raw pointer.
2. **Native helper aggregates.** Existing generated named/dense guards and
   loads may be counted only by a separately cached diagnostic artifact
   variant. The production artifact and diagnostics-disabled helper ABI must
   contain no new counter update. A typed cache key must distinguish budget
   mode and diagnostic instrumentation before this variant is installed.

The interpreted record classes retain independent default and hard caps with
explicit dropped-observation counts. Native helper counts are fixed aggregate
counters rather than page-controlled site storage. Diagnostics-disabled A/B
timing, source-free serialization, finite budgets, GC, guard miss replay, and
feature-disabled builds remain stop/go gates.

## Schedule consequence

Do not select OSR merely because the eligible loop is hot, and do not select
direct storage merely because storage sites are frequent. Finish both storage
attribution parts, project schema 7 through Ligero, and rerun the fixed
micro/engine/browser matrix with separate headline and diagnostic runs.
Decision checkpoint A must then rank attributable lost time and approve exactly
one of loop OSR, compiled ordinary calls, direct guarded storage, or region
stitching.

After the two storage behavior slices, pay the scheduled separately revertible
behavior-neutral refactor. Typed cache-key construction is the preferred
candidate if it is not already isolated as the prerequisite refactor for the
diagnostic artifact variant.

## Verification

- full Boa JIT library suite: 1,188 passed, one ignored;
- feature-disabled Boa library suite: 1,126 passed;
- benchmark-runner option test passed;
- strict Clippy reported only the 16 pre-existing findings, none in touched
  code;
- Ligero's JIT-enabled script suite: 662 passed;
- Ligero CLI checks passed with and without `jit`, and strict affected-crate
  Clippy passed;
- formatting, source-free serialization, hard-cap, and diff checks passed.
