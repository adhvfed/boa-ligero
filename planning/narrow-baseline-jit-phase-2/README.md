# Narrow baseline JIT — Phase 2

Status: implementation underway, scheduled 2026-08-03. Phase 1 is landed
behind the opt-in `jit` feature and its first publisher-neutral Ligero workload
gate has passed. Slice 1 now has bounded engine diagnostics and standalone JSON
export; the Ligero projection and representative profile matrix remain before
the first lowering decision. Each new execution ABI still requires its own
design review before implementation.

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
2. Lower only the smallest measured blocker batch needed to expose useful
   native regions, first deciding whether the current whole-CodeBlock compiler
   can express the result or explicit region metadata is required.
3. At a recorded decision checkpoint, rank loop OSR, compiled calls, and
   helper-backed storage by measured lost time and transition count.
4. Implement the highest-ranked boundary behind its own ABI review; re-profile
   before selecting the next boundary rather than assuming the original order.
5. Apply admission, cache bounds, failure suppression, and cold-start
   guardrails throughout the program, then tune thresholds after the entry
   kinds are stable.
6. Keep direct storage last unless helper attribution proves it dominates and
   a GC/layout-lifetime review approves the snapshot contract.

The first slice is deliberately measurement-only. OSR and compiled calls are
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

Phase 1 remains the semantic contract: [exit/deopt/GC](../narrow-baseline-jit/03-exit-deopt-gc.md),
[native lowering](../narrow-baseline-jit/04-native-lowering.md), and
[verification](../narrow-baseline-jit/05-verification-and-benchmarks.md) are
normative unless this phase explicitly tightens them.
