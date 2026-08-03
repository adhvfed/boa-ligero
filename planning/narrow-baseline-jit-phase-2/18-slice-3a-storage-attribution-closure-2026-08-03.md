# Slice 3A storage-attribution closure — 2026-08-03

## Result

Slice 3A is complete. Boa `753ca3ea` adds bounded schema-7 interpreted storage
records; `8b9b58c3` adds fixed native helper aggregates through diagnostic-only
artifacts. Ligero `e36b438f` projects the complete schema through its host
boundary. No execution ABI, lowering allowlist, or production admission rule
changed.

The separately revertible prerequisite refactor is Boa `04c18a03`: runtime
cache keys are typed rather than positional tuples. Diagnostic artifacts then
add a third key dimension beside code identity and finite-budget mode.
Production artifacts retain the original helper addresses and perform no
diagnostic counter update. Enabling diagnostics compiles or reuses the
diagnostic variant; disabling diagnostics reuses the production variant.

## Interpreted-site contract

Before an interpreted read executes, diagnostics may record only:

- runtime-local code ID and bytecode PC;
- coarse `named`, `dense`, `computed`, or `length` kind;
- whether Boa's existing named/dense inline-cache fast path would hit, miss,
  or not apply.

The observer performs no property-key conversion, lookup, getter/proxy call,
allocation required by JavaScript semantics, or mutation. It retains no key,
property name, value, object, shape, source, URL, or pointer. Named/dense sites
are independently capped; observations after the cap increment one dropped
counter without retaining a dropped-site set.

The exact engine controls report:

- named reads: four executions, one cold miss, three hits;
- dense reads: four executions, one cold miss, three hits;
- computed and specialized-length reads: four not-applicable observations;
- a denied dormant callee: 40 named reads, one miss, 39 hits;
- zero-cap dormant control: 40 dropped observations and no retained record.

The standalone property control reports three distinct named sites, each with
400,000 executions, one cold miss, and 399,999 hits. The dense-array control
reports 2,000,000 executions, one cold miss, and 1,999,999 hits. Both retain
zero dropped storage observations.

## Native-helper contract

Diagnostic native artifacts wrap only the existing named/dense guard and load
helpers. A fixed scratch record in the VM counts guard hits, guard misses, and
helper loads for one native entry; the backend takes, clears, and saturating-
merges it immediately after return. It is not page-sized storage and carries no
site or object identity.

Focused tests prove matching named/dense guards count hits, successful guards
have one corresponding load, shape/representation changes count misses and
deopt to correct interpreter results, per-entry scratch state is cleared, and
production helpers leave it untouched. A cache-variant regression warms a
production property artifact, enables diagnostics and observes exactly one new
compilation plus ten named guard/load hits, then disables diagnostics and
reuses the original production artifact without another compilation.

## Diagnostics-off parity

Seven fresh-process, release, one-invocation, no-warmup pairs preserve Gate H:

| Workload | Interpreter median | JIT median | JIT tax |
| --- | ---: | ---: | ---: |
| eligible one-shot loop | 28.942 ms | 29.001 ms | +0.201% |
| statically ineligible loop | 38.150 ms | 38.112 ms | −0.101% |

All sinks match. Warm measurements create zero artifacts/entries, observe the
bounded 256 backedges and one dormant handoff, and keep diagnostics disabled.
The separate diagnostic runs are attribution evidence and are excluded from
headline timing.

## Verification

- full Boa JIT library suite: 1,192 passed, one ignored;
- full feature-disabled Boa library suite: 1,138 passed;
- benchmark-runner option test passed;
- JIT and no-default-feature checks passed;
- strict Boa Clippy reports the same 16 pre-existing findings and none from
  this slice;
- Ligero's full JIT-enabled script suite: 662 passed plus its harness capture;
- Ligero CLI checks pass with and without `jit`;
- strict all-target JIT-enabled Clippy passes for `script` and `ligero`;
- formatting, diff, source-free serialization, hard-cap, guard-miss, and cache-
  variant tests pass.

## Decision checkpoint A is next

Loop, call, interpreted storage, and native-helper attribution are now
comparable. Re-run the fixed micro/engine/browser matrix with diagnostics off
for headline timing and a separate zero-drop diagnostic pass. Rank attributable
lost time and approve exactly one of loop OSR, compiled ordinary calls, direct
guarded storage, or region stitching. Frequency alone does not approve an ABI;
the selected branch still needs its own ownership, materialization, GC,
exception, budget, cache, and cold-workload review.
