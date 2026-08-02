# Narrow baseline JIT — Phase 2

Status: proposed implementation plan. Phase 1 is landed behind the opt-in
`jit` feature; this phase is not an implementation commitment until its
workload profile and ABI sketches have been reviewed.

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

## Recommended order

1. Add opt-in fallback/coverage observability and profile the actual
   `ligero-browser` workload plus the existing Boa benchmarks.
2. Lower the small set of environment, conversion, bitwise, and control-flow
   operations that prevent real numeric loops from becoming native regions.
3. Add loop-header OSR for a conservative class of ordinary loops.
4. Replace native-to-interpreter call transitions with a guarded
   compiled-to-compiled call/return ABI, without inlining.
5. Only then move helper-backed element/property reads to direct guarded loads.
6. Tune tiering, code-cache admission, and cold-start policy using the
   workload measurements rather than synthetic thresholds.

The first slice is deliberately measurement-only. It should tell us which of
steps 2–4 is the largest blocker before any new native ABI is added.

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

