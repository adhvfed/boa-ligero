# Slice 4A1 planner and metadata checkpoint — 2026-08-03

Status: Slices 4A1.1 and 4A1.2 complete. Slice 4A1.3 is next. No production
scheduler path can compile, cache, or invoke a loop artifact yet.

## Result

Boa `c435d60e` lands the pure canonical loop-region planner. Boa `7837f0a8`
lands bounded per-region state, source-free OSR aggregates, circuit breakers,
and the exit taxonomy without invocation. Ligero `c05557ed` projects diagnostic
schema 8 through the browser-facing host API.

This closes the two behavior-inert foundations required by the reviewed
[Slice 4A0 ABI](20-loop-osr-abi-review-2026-08-03.md). It does not satisfy Gate
O and is not evidence that a loop has entered native code.

## Slice 4A1.1 — proven region and maps

The typed cache key distinguishes function and loop entries. A loop key owns
the runtime-local code ID, header, canonical latch, uniform `I32`/`F64`
representation, finite-budget mode, and diagnostic mode.

The pure planner:

- accepts exactly one unconditional latch and one conditional forward exit;
- retains at most 128 region instructions and inspects at most 16 instructions
  in the return continuation;
- rejects unknown boundaries, unmodelled operations, extra external edges, and
  unproven control flow;
- computes fixed-point liveness and path-specific entry/exit maps;
- distinguishes VM-register entry sources, native exit values, and preserved
  VM exit values at the type level; and
- proves the selected fractional-accumulator fixture requires three `F64`
  live-ins while its untouched returned argument remains in the VM slot.

Planner rejection declares no function and changes no scheduler, budget, or VM
state.

## Slice 4A1.2 — bounded state and diagnostics

The backend now retains at most 64 exact typed loop-region states. Per-region
backedge hotness is separate from CodeBlock-global function-entry hotness and
the dormant-frame handoff. Existing exact keys can continue to their terminal
state when the table is full; every unseen key is then suppressed without a
negative-cache allocation.

The first compile-result policy is deliberately post-attempt:

- successful loop code contributes accounted bytes and trips the 1 MiB breaker
  at or above the threshold;
- failed lowering contributes compile time but no compilation or code bytes;
- a completed attempt over 10 ms trips the time breaker; and
- once either breaker trips, already-retained entries remain queryable while
  new work is suppressed.

Schema 8 adds one fixed, source-free OSR aggregate: cache requests/hits/misses,
hotness crossings, attempts, compilations, entries, entry rejections,
continuations, deoptimizations, compile time, code bytes, and fixed rejection
and suppression reason counters. It adds no per-page-sized OSR record stream.
The native status encoding reserves `EntryRejected` and `Continuation`, and
the public reason taxonomy reserves `LoopExit`; PC-zero scheduling treats those
inert loop-only statuses as an internal error until Slice 4A1.4.

## Evidence

- `cargo check -p boa_engine` passes with the `jit` feature exercised by the
  focused suite.
- The focused JIT suite passes 23 tests with one benchmark ignored.
- Capacity tests prove all 64 retained exact keys remain accessible and the
  65th unseen key is suppressed without insertion.
- Breaker tests prove failed compiles account time but not code bytes, and that
  byte/time suppression applies only after the completed attempt.
- Native exit-kind round trips cover the new reserved status values.
- Warning-denying Boa Clippy reports exactly the 16 independently known
  pre-existing findings and no new finding from these slices.
- Ligero's schema-8 projection test, feature checks for script/CLI/automation,
  and strict affected-crate Clippy pass.

## Next bounded slice: 4A1.3

Add a separate uniform-mode region compiler over an already proven
`LoopRegionPlan`. Its direct tests must prove strict live-in guards, numeric
lowering, path-specific materialization, metadata-validated continuation PCs,
pre-effect replay, and exact finite-budget charging. A compile or lowering
failure must retain a terminal source-free rejection and expose no invokable
cache entry.

Production scheduler invocation is a binding non-goal for 4A1.3. It belongs to
4A1.4 only after the compiler harness independently proves the entry and every
exit. Do not widen the opcode set, add mixed-mode SSA, box the whole frame, or
combine scheduler wiring with compiler debugging to make the selected fixture
run.

After 4A1.4 and the 4A1.5 differential/fixed-matrix gate, Slice 4A1.R pays the
scheduled behavior-neutral region-key/materialization/exit-mapping refactor in
a separately revertible commit before Decision checkpoint B or any second
execution ABI.
