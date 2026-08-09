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
Slice 4A0's ownership/materialization/budget design review is complete. Slice
4A1.1 now proves the canonical region and its live-state maps, and Slice
4A1.2 retains bounded per-region hotness/terminal state, source-free schema-8
diagnostics, capacity/circuit-breaker policy, and the entry/continuation exit
taxonomy. Neither slice can emit or invoke a loop artifact. Slice 4A1.3 is the
now complete in Boa `c2885afe`: a separate uniform-mode region compiler
strictly guards planned live-ins, preserves exact budget/replay state, and
materializes its validated continuation. Boa `55701ef4` now completes Slice
4A1.4's production wiring at the reviewed post-backedge ownership boundary. A
separate per-frame OSR decision, exact cached-metadata validation, and dormant-
dispatch handoff make the first numeric shape reachable without changing
opcode eligibility. Boa `92acfa22` closes 4A1.5a with exhaustive cold/cache-hit
instruction-budget and loop-limit differentials through the production path,
including every first-shape opcode PC and every arithmetic replay guard. Boa
`44d45ca3`, `68d795fd`, `e37f2398`, and `e34f8530`
now close 4A1.5b with fail-closed malformed-state containment, forced-GC/
nested-frame lifetime coverage, exact 64+1 production ownership, and cached-
entry reuse at capacity. Its isolated cold-
OSR admission subgate now passes in Boa `229ac2e4`: seven alternating pairs
measure a 4.588× median speedup including compilation with exact one-entry
evidence and no PC-zero artifact. The first wider-matrix attempt then exposed
a 35–50% tax in denied loop-free property/method helpers. Boa `073c12cd`
caches that no OSR edge exists and bypasses both per-opcode observation and the
per-call scheduler round trip only when diagnostics are off. The complete
nine-row micro matrix now passes: every negative row is within 3.635%, while
the eligible one-shot loop is 4.824× faster. The first engine matrix then
exposed an 18% Earley-Boyer tax in the generic denied-loop observer. Boa
`8c8af54c` specializes production dispatch and inspects post-opcode control
flow only for branch-capable instructions while preserving exact scheduler
ownership and diagnostic observation. Gate O now passes: Crypto is +2.125%,
DeltaBlue -0.403%, Earley-Boyer +4.412%, and seven W0 pairs improve median cold
load by 29.607% with exact sink/paint/memory structure. A separate schema-8 W0
sample has zero drops and distinguishes 999 PC-zero returns from one OSR
continuation. Boa `6dc6aa07` now completes the separately revertible 4A1.R
loop-exit-contract refactor with the full semantic/containment suite green,
Earley-Boyer at +4.287%, and a clean zero-drop W0 sample. Decision checkpoint
B's binding D0 rerun remains open and must use schema 10 after the host passes
its continuous quiescence preflight. D1's two resource-governor implementation
commits are now landed in Boa `42623234` and `2888c024`, with Ligero projection
in `72568c9a` and `dfdc0714`. D1 is not closed: combined capacity/variant
coverage, maximum-diagnostic saturation, recoverable-failure ownership, an
automated raw-emitter audit, and the seven-pair cold/RSS gate remain. JIT
therefore stays opt-in.

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
    re-profile before selecting the next boundary. **ABI review complete:**
    Boa `3deaf18e`. **Planner complete:** Boa `c435d60e` proves the canonical
    region and path-specific maps without executable behavior. **Bounded state
    and diagnostics complete:** Boa `7837f0a8` adds the 64-key table,
    allocation-free new-site suppression at capacity, 1 MiB/10 ms post-attempt
    circuit breakers, schema-8 aggregate counters, and inert exit taxonomy;
    Ligero `c05557ed` projects schema 8. **Compiler complete:** Boa
    `c2885afe` revalidates the immutable plan, emits guarded uniform-mode loop
    artifacts, and proves entry rejection, continuation materialization,
    finite-budget/replay behavior, and terminal uncached rejection through a
    direct harness. **Scheduler wiring complete:** Boa `55701ef4` observes the
    already charged latch, compiles or reuses the exact bounded artifact,
    enters it under strict live-frame guards, validates every returned status
    and PC against immutable metadata, and closes one independent OSR decision
    per frame. **Semantic/accounting complete:** Boa `92acfa22` exhaustively
    compares cold and cache-hit instruction budgets, loop limits, exact exit
    PCs, replay guards, and budget-mode cache separation through the real
    scheduler. **Containment/lifetime complete:** Boa `44d45ca3`, `68d795fd`,
    `e37f2398`, and `e34f8530` disable compromised loop backends, prove GC/
    nested-frame/stale-guard safety, exhaust malformed loop status classes,
    and close the production 64+1 ownership matrix. **Cold-OSR admission
    complete:** Boa `229ac2e4` isolates the production-threshold sample and
    passes the ≥2× gate at 4.588× with no PC-zero artifact. **Workload gate
    complete:** Boa `8c8af54c` removes denied-loop observer overhead without
    weakening the scheduler boundary; all fixed engine rows remain inside 5%,
    W0 improves by 29.607%, and separate schema-8 diagnostics have zero drops.
    **Refactor complete:** Boa `6dc6aa07` consolidates native loop-exit
    validation/accounting behind a private typed contract without changing the
    ABI or policy; the focused/full engine, Earley-Boyer, and clean W0 gates
    pass. **Next:** Decision checkpoint B re-profiling.
11. Apply cache bounds, failure suppression, and cold-start guardrails throughout
   the program, then tune thresholds after the entry kinds are stable.
   **Implementation landed, gate open:** Boa `42623234` and `2888c024` bound
   retained state, body decoding, feedback, payload, and compile time and add
   safe overrun retirement; the remaining D1 acceptance work is recorded in
   checkpoint 32.
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
- [Slice 4A1 planner and metadata checkpoint, 2026-08-03](21-slice-4a1-planner-metadata-checkpoint-2026-08-03.md)
  — the landed pure planner, bounded per-region state and schema-8 diagnostics,
  verified non-invocation boundary, and the narrowed Slice 4A1.3 compiler gate.
- [Slice 4A1 region compiler checkpoint, 2026-08-03](22-slice-4a1-region-compiler-checkpoint-2026-08-03.md)
  — the guarded uniform-mode compiler, direct entry/exit harness, exact replay
  and continuation evidence, negative-zero correction, and the remaining
  production non-invocation boundary before Slice 4A1.4.
- [Slice 4A1 scheduler checkpoint and gate refinement, 2026-08-03](23-slice-4a1-scheduler-checkpoint-2026-08-03.md)
  — the landed post-backedge invocation edge, production integration evidence,
  independently reviewed residual risks, and the falsifiable 4A1.5
  differential/cache/browser rollback gate.
- [Slice 4A1 semantic/accounting checkpoint, 2026-08-03](24-slice-4a1-accounting-checkpoint-2026-08-03.md)
  — exhaustive production cold/cache-hit budget and loop-limit differentials,
  replay ownership, exact nonzero diagnostic PCs, and cache-mode separation.
- [Slice 4A1 containment/lifetime checkpoint, 2026-08-03](25-slice-4a1-containment-checkpoint-2026-08-03.md)
  — fail-closed malformed loop state, forced-GC and nested-frame lifetime
  coverage, exact 64+1 scheduler ownership, and the remaining workload gate.
- [Slice 4A1 cold-OSR admission checkpoint, 2026-08-03](26-slice-4a1-osr-admission-checkpoint-2026-08-03.md)
  — isolated production-threshold measurement, raw seven-pair evidence, exact
  entry-kind counters, and the remaining micro/engine/W0 rollback matrix.
- [Slice 4A1 micro rollback checkpoint, 2026-08-03](27-slice-4a1-micro-rollback-checkpoint-2026-08-03.md)
  — denied loop-free scheduler correction, the checksummed nine-row pass, raw
  samples, and the remaining engine/W0/zero-drop gate.
- [Slice 4A1 workload gate, 2026-08-03](28-slice-4a1-workload-gate-2026-08-03.md)
  — the final engine/W0 rollback matrix, denied-loop observer correction,
  zero-drop schema-8 evidence, Gate O decision, and bounded 4A1.R handoff.
- [Slice 4A1 exit-contract refactor, 2026-08-03](29-slice-4a1-exit-contract-refactor-2026-08-03.md)
  — the separately revertible typed exit-validation cleanup, semantic and
  containment reruns, Earley sentinel, clean W0 evidence, and checkpoint-B
  handoff.
- [Default-JIT admission plan, 2026-08-03](30-default-jit-admission-plan-2026-08-03.md)
  — executable D0–D5 gates for reproducible Decision-B profiling, backend-wide
  cache bounds, PC-zero containment, the named W2 mirror corpus, supported
  platforms/security, and the separately revertible default flip.
- [Default-JIT resource bounds design, 2026-08-03](31-default-jit-resource-bounds-design-2026-08-03.md)
  — D1's exact backend-lifetime function/loop key, code-byte, compile-time,
  body-size, feedback, and diagnostic bounds; suppression/retirement semantics;
  raw-emitter closure; and the implementation acceptance matrix.
- [Default-JIT resource governor checkpoint, 2026-08-03](32-default-jit-resource-governor-checkpoint-2026-08-03.md)
  — landed D1 implementation commits, schema-10 projection, deterministic
  acceptance, release-runner saturation fixture, and automated process gate;
  only the binding seven-pair cold-start/RSS run awaits a quiet host before D1
  closure.
- [Dragon 2 close-out: executable memory reclaimed, 2026-08-03](34-dragon-2-executable-memory-reclaimed-2026-08-03.md)
  — `JitBackend`'s destructor now calls `JITModule::free_memory`, overriding
  cranelift-jit's leak-by-default `Drop`; the safety argument for it, the
  whole-module-only scope limit, and the correction to acceptance item 10,
  which had asserted the guarantee before it was true.
- [Storage helper fusion checkpoint, 2026-08-09](35-storage-helper-fusion-2026-08-09.md)
  — measured guard/load fusion for dense and named `i32`/`f64` paths, the
  GC-safe tagged/stack-output contracts, and the rejected F64 argument variant.
- [Ordinary-call continuation design, 2026-08-09](36-ordinary-call-continuation-design-2026-08-09.md)
  — the bounded VM-owned continuation trampoline selected to keep compiled
  callers native across ordinary calls before direct compiled-callee entry.
- [Ordinary-call continuation checkpoint, 2026-08-09](37-ordinary-call-continuation-checkpoint-2026-08-09.md)
  — the accepted continuation ABI, liveness-based safepoints, exception and
  budget closure, and the measured method-call improvement.
- [Global-object binding-read design, 2026-08-09](38-global-object-binding-read-design-2026-08-09.md)
  — a fail-closed current-realm IC read selected to unlock the flat-call caller
  without embedding global state in generated code.
- [Global-object binding-read checkpoint, 2026-08-09](39-global-object-binding-read-checkpoint-2026-08-09.md)
  — accepted global-object binding semantics, mutation/GC/budget evidence, and
  the measured flat-call improvement.
- [Number `BitOr` lowering design, 2026-08-09](40-number-bitor-design-2026-08-09.md)
  — an exact `f64`-to-`ToInt32` contract selected to unlock overflowing
  integer-style arithmetic without incorrect wrapping shortcuts.
- [Number `BitOr` checkpoint, 2026-08-09](41-number-bitor-checkpoint-2026-08-09.md)
  — accepted Number semantics, coercion and budget closure, and the measured
  integer-arithmetic improvement.
- [Inline `ToInt32` design, 2026-08-09](42-inline-toint32-design-2026-08-09.md)
  — a correct but slower bit-for-bit Cranelift prototype, rejected after its
  larger branch-heavy artifact lost to the compact Rust helper.
- [Number bitwise-family design, 2026-08-09](43-number-bitwise-family-design-2026-08-09.md)
  — a measured `BitAnd`/`BitXor` extension of the accepted exact Number
  conversion contract, partially accepted after the `BitAnd` prototype exposed
  an unresolved boxed-argument boundary.
- [Number `BitXor` checkpoint, 2026-08-09](44-number-bitxor-checkpoint-2026-08-09.md)
  — retained exact Number XOR semantics, correctness and performance gates,
  the fully removed `BitAnd` regression, and the next representation/call
  architecture boundaries.
- [Default-JIT runtime-control design, 2026-08-09](45-default-jit-runtime-control-design-2026-08-09.md)
  — default-feature and default-context enablement, explicit CLI/embedder/
  Test262 interpreter controls, worker propagation, compatibility boundaries,
  and the acceptance matrix for the policy change.
- [Default-JIT runtime-control checkpoint, 2026-08-09](46-default-jit-runtime-control-checkpoint-2026-08-09.md)
  — the landed mode architecture, exact interpreter/JIT Test262 parity,
  feature-propagation and cold-measurement defects caught during verification,
  and the remaining cross-platform/browser/security evidence.

Phase 1 remains the semantic contract: [exit/deopt/GC](../narrow-baseline-jit/03-exit-deopt-gc.md),
[native lowering](../narrow-baseline-jit/04-native-lowering.md), and
[verification](../narrow-baseline-jit/05-verification-and-benchmarks.md) are
normative unless this phase explicitly tightens them.
