# Narrow baseline JIT — Phase 2

Status: implementation underway, scheduled 2026-08-03. Phase 1 is landed
behind the opt-in `jit` feature and its first publisher-neutral Ligero workload
gate has passed. Slice 1 now has bounded engine diagnostics, standalone JSON
export, an opt-in Ligero projection, and a completed representative
micro/engine/browser profile. Slice 2A then measured and removed a dormant-tier
scheduler tax before landing a conservative loop-or-45-instruction admission
rule. The rejected controls are now within the recorded 5% interpreter-parity
gate, the profitable straight-line controls retain clear wins, and the W0
browser kernel remains native. The scheduled behavior-neutral scheduler
refactor and bounded per-code admission diagnostics are complete. The guarded
receiver review then rejected standalone `This` lowering: the measured method
immediately reaches an unsupported named store and remains below the production
admission threshold. Slice 2C is now complete. Boa `345767c5` rejects
non-continuable callers before artifact creation and lowers only
`GlobalDeclarative` `GetName` through a current-frame/current-realm read;
Ligero `2c39eafe` projects diagnostic schema 4. Boa `54a109f6` then pays the
scheduled separately revertible helper-table refactor. Post-refactor release
gates retain a 4.80× floating-point win, keep all negative controls inside 5%,
and preserve W0's checksum and paint structure at a 30.0% lower median cold
load. Each new execution ABI still requires its own design review before
implementation. Decision checkpoint A has now found that the current schema
cannot rank the remaining boundaries honestly: production call-containing
frames are denied before a scheduler exit can be observed, property counts are
static rather than dynamic, and one-shot loops expose a separate hot-but-
unentered tiering regression. The call-attribution sub-slice has now landed in
schema 5: the fixed flat and method controls are dynamically monomorphic, but
their targets are not native-cached. Slice 3B now closes the dormant-tier
guardrail: eligible and statically ineligible one-shot loops are within 0.94%
of interpreter medians with zero artifacts, while explicit diagnostics retain
exact 2,000,000-backedge evidence. Loop attribution has now landed in schema 6
with exact diagnostics-only evidence and a conservative static OSR-candidacy
screen. Storage attribution is now complete in schema 7: bounded interpreted
sites and fixed native-helper aggregates are deliberately separate, and only
separately cached diagnostic artifacts update native counters. The fixed
matrix rerun is now complete. Decision checkpoint A selects only conservative
numeric loop-header OSR: it is the sole branch with a zero-drop dynamic
opportunity, bytecodes inside the screened numeric/control-flow subset, and a
matching approximately 4× whole-body native counterfactual. Slice 4A0 confirms
that the region still needs an explicit external continuation and live-in
representation plan; the earlier screen was not a nonzero-PC compilation
proof. Broader engine and application rows contain no first-shape OSR
candidates, so this is a narrow ABI selection rather than a breadth claim.
Slice 4A0's ownership/materialization/budget design review is complete.

Phase 1 proved the important safety boundary: Cranelift can execute selected
Boa bytecode against the real VM stack, guard primitive/object assumptions, and
return to the interpreter without changing observable behavior. It also showed
the remaining performance problem clearly:

- the synthetic warm loop is several times faster when it stays native;
- representative arithmetic/array scripts can still compile to the shim
  because their surrounding bytecode contains unsupported environment,
  conversion, or bitwise operations;
- native direct calls still perform a VM transition and return a `Call` exit,
  so call-heavy code pays repeated scheduler/frame-entry overhead;
- hotness is primarily acted on at function entry, so a one-shot function can
  execute a long loop without on-stack replacement into native code;
- property and dense-element paths are guarded helper calls, not yet direct
  storage loads.

The Phase 2 objective is therefore **native continuity**: keep a hot region,
loop, and guarded ordinary call in native execution for as long as its
assumptions hold, while retaining exact interpreter fallback at every boundary.
This is a baseline-tier phase, not an optimizing compiler project.

## Evidence already available

The first browser-shaped gate is a useful baseline, not a feature-selection
profile. Five interleaved fresh-process pairs on 2026-08-02 measured a 44.04 ms
median interpreter load and 32.24 ms median JIT load, including 1.95 ms median
compilation. The numeric/DOM fixture produced the same visible checksum and
paint structure with 999 native entries and zero deoptimizations per JIT run.
That proves the opt-in integration and cold accounting path; it does not tell
us whether real bundles are primarily blocked by unsupported bytecode, late
loop hotness, scheduler call transitions, or helper-backed storage.

The finite instruction-budget contract tightened after the first browser
attempt: budgeted and unbudgeted entries are distinct cache variants, each
budgeted native bytecode is charged exactly once, and only an audited
pre-effect guard exit may refund the instruction the interpreter will replay.
Every Phase 2 entry and exit ABI inherits that rule.

## Evidence-driven order

1. Add bounded, opt-in fallback/coverage observability and profile micro,
   engine, and actual `ligero-browser` workloads.
2. Apply a measured admission guardrail against the positive and negative Gate
   P controls before exposing more native function entries. **Complete:** Boa
   `fcfc2659` removes the dormant scheduler tax and `f0eeef75` lands the
   measured admission rule with backend-generation and re-entrancy guards.
3. Pay the scheduled refactoring checkpoint: consolidate the duplicated
   frame-change interpreter loops without changing dispatch semantics, then
   add bounded source-free per-code admission records so later profiles can
   distinguish suppression from compilation. **Complete:** Boa `612c7dc6`
   unifies dormant dispatch with the negative controls still inside the parity
   gate; Boa `a7036d71` and Ligero `05690d09` add and project schema-3 bounded
   admission decisions.
4. Review the smallest measured blocker batch against complete CodeBlocks and
   production admission before changing the allowlist. **Complete for
   receivers:** standalone `This` lowering is a no-go because the method's next
   frontier is `SetPropertyByName` and the 16-instruction helper remains denied.
5. Close the call-boundary admission hole: while generated callers have no
   native continuation after a call, call-containing function entries must
   install no artifact and report a distinct denial reason. **Complete:** Boa
   `345767c5` reports `denied_call_boundary` and installs no artifact.
6. Lower only `GlobalDeclarative` `GetName` through a current-frame/current-
   realm read after a locator-stability guard. Keep global-object, stack,
   eval-affected, write, and deletion forms unsupported; gate the float win
   against call-heavy controls, W0, TDZ, mutation, GC, realm, and budgets.
   **Complete:** Boa `345767c5`, with Ligero schema projection `2c39eafe`.
7. Pay the scheduled behavior-neutral helper/materialization refactor after
   the two Slice 2C behavior changes. **Complete:** Boa `54a109f6` borrows the
   generated helper table and removes its unused compiler copy.
8. At a recorded decision checkpoint, verify that loop, call, and storage cost
   is dynamically attributable. **Reviewed:** schema 4 cannot yet make that
   comparison, so no execution ABI is selected.
9. Add bounded source-free site telemetry and close the measured hot-but-
   unentered tiering regression without relaxing production admission.
   **Call sub-slice complete:** Boa `0bc757a2` and Ligero `1d58914a` publish
   bounded schema-5 call attribution. **Gate H complete:** Boa `d64fe095` and
   `cc07a908` replace unbounded/map-backed hotness with generation-scoped state
   and a dormant-frame transition; Ligero `6594d5a2` projects the counters.
   **Loop attribution complete:** Boa `0233f60f` and Ligero `48573227` add and
   project bounded schema-6 loop-site evidence while preserving Gate H.
   **Storage attribution complete:** Boa `753ca3ea`, `04c18a03`, `8b9b58c3`,
   and `a398b455`, plus Ligero `e36b438f`, close schema 7 without instrumenting
   production artifacts. **Decision checkpoint A complete:** Boa `343cf037`
   and Ligero `78d55bda` enable zero-drop 4,096-record profiling; the fixed
   matrix selects only conservative loop-header OSR.
10. Check in Slice 4A0's exact OSR ownership, cache-key, materialization, GC,
   exception, and finite-budget ABI review; then implement that one shape and
   re-profile before selecting the next boundary.
11. Apply cache bounds, failure suppression, and cold-start guardrails throughout
   the program, then tune thresholds after the entry kinds are stable.
12. Keep direct storage last unless helper attribution proves it dominates and
   a GC/layout-lifetime review approves the snapshot contract.

The first slice was deliberately measurement-only. OSR and compiled calls are
alternative evidence-selected branches, not a pre-approved sequence. Both
remain Phase 2 targets, but either may be deferred with a checked-in profile
showing that another boundary dominates.

## Document map

- [Goals, boundaries, and gates](00-goals-and-gates.md) — outcome, scope,
  non-goals, and tripwires.
- [Observability and workload profiling](01-observability-and-profiling.md) —
  the counters and workload protocol needed to choose the next lowering.
- [Native coverage and region continuity](02-native-coverage-and-regions.md) —
  the operation frontier that turns shim-heavy loops into native regions.
- [Loop OSR](03-loop-osr.md) — compiling a hot loop in an already-running
  frame, with a conservative first eligibility contract.
- [Compiled calls](04-compiled-call-abi.md) — the VM/frame ABI for native to
  native ordinary-function calls and returns.
- [Guarded storage and feedback](05-guarded-storage-and-feedback.md) — direct
  dense-element/property loads after the helper path is measured.
- [Tiering and cache policy](06-tiering-and-cache-policy.md) — admission,
  thresholds, variants, lifetime, and cold-start behavior.
- [Verification and workload gates](07-verification-and-workload-gates.md) —
  differential, stress, platform, and real-workload acceptance criteria.
- [Implementation sequence](08-implementation-sequence.md) — commit-sized
  slices, dependencies, stop/go decisions, and suggested commands.
- [Gate P profile, 2026-08-03](09-gate-p-profile-2026-08-03.md) — the measured
  micro/engine/browser frontier, negative controls, and next-slice decision.
- [Admission crossover and scheduler finding, 2026-08-03](10-admission-crossover-2026-08-03.md)
  — static native shape, the rejected admission prototype, and revised Slice
  2A order.
- [Receiver frontier review, 2026-08-03](11-receiver-frontier-review-2026-08-03.md)
  — exact `this` semantics, whole-CodeBlock/admission analysis, and the no-go
  decision for standalone receiver lowering.
- [Binding-read and call-boundary review, 2026-08-03](12-binding-read-and-call-boundary-review-2026-08-03.md)
  — the approved global-declarative read contract, the call-containing entry
  admission correction, measured gate, exclusions, and refactor checkpoint.
- [Slice 2C closure, 2026-08-03](13-slice-2c-closure-2026-08-03.md) — landed
  commits, correctness matrix, post-refactor release controls, W0, and the
  remaining Decision checkpoint A.
- [Decision checkpoint A review, 2026-08-03](14-decision-checkpoint-a-review-2026-08-03.md)
  — the attributable one-shot-loop result, the separate dormant-tier
  regression, the call/storage observability gaps, and the refined no-ABI
  schedule.
- [Slice 3A call attribution checkpoint, 2026-08-03](15-slice-3a-call-attribution-2026-08-03.md)
  — schema-5 bounded call-site evidence, diagnostics-off A/B controls, and why
  monomorphic execution without a cached native target does not yet select the
  compiled-call ABI.
- [Slice 3B Gate H closure, 2026-08-03](16-slice-3b-gate-h-closure-2026-08-03.md)
  — generation-scoped hotness, the dormant-frame transition, exact-versus-
  bounded counter semantics, paired release measurements, and the remaining
  attribution work before an ABI decision.
- [Slice 3A loop-attribution checkpoint, 2026-08-03](17-slice-3a-loop-attribution-2026-08-03.md)
  — schema-6 interpreted loop evidence, diagnostics-off parity, and the
  refined two-part storage-attribution contract before Decision checkpoint A.
- [Slice 3A storage-attribution closure, 2026-08-03](18-slice-3a-storage-attribution-closure-2026-08-03.md)
  — schema-7 interpreted/native storage evidence, diagnostic cache isolation,
  complete gates, and the fixed-matrix handoff to Decision checkpoint A.
- [Decision checkpoint A selection, 2026-08-03](19-decision-checkpoint-a-selection-2026-08-03.md)
  — the zero-drop fixed matrix, corrected runner bounds, narrow loop-header OSR
  selection, deferred alternatives, and Slice 4A0 design-review contract.
- [Loop-header OSR ABI review, 2026-08-03](20-loop-osr-abi-review-2026-08-03.md)
  — the exact typed region identity, safe post-backedge ownership boundary,
  live-in and exit materialization rules, continuation/replay taxonomy, bounds,
  exclusions, and Slice 4A1 gates.

Phase 1 remains the semantic contract: [exit/deopt/GC](../narrow-baseline-jit/03-exit-deopt-gc.md),
[native lowering](../narrow-baseline-jit/04-native-lowering.md), and
[verification](../narrow-baseline-jit/05-verification-and-benchmarks.md) are
normative unless this phase explicitly tightens them.
