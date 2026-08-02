# Implementation sequence

Each slice below should be independently reviewable, testable, and revertible.
Keep commits focused; do not combine compiler, VM transition, and benchmark
changes in one large landing.

## Slice 0 — measurement and contract

Add JIT-only counters and test utilities before changing generated code:

- compile requests/successes/rejections;
- code-cache hits/misses;
- function entries and loop backedges;
- native/helper/deopt counts;
- deopt reason and PC;
- compile time and native time.

Add the cold/warm JIT benchmark mode and record a fresh Boa interpreter
baseline. Keep the existing shim runner available as a correctness control.

Done when stats can explain whether a test compiled, ran natively, or
immediately deoptimized. This slice should not change normal interpreter
behavior.

## Slice 1 — cache and explicit runtime

Introduce `JitCodeCache`/`JitRuntime` around the existing backend:

- compile a code block at most once per backend/cache key;
- retain the generated code for the backend lifetime;
- separate compile time from execution time;
- preserve `Script::evaluate_jit` as the explicit entry point;
- reject unsupported function kinds before compilation.

Add cache identity and backend-lifetime tests. Do not add automatic tiering yet.

## Slice 2 — decoded compiler IR and exit word

Replace the current compile-time bytecode walk with an internal decoded
instruction/CFG representation. Add:

- instruction-boundary validation;
- branch-target validation;
- basic-block metadata;
- explicit native/interpreter exit records;
- VM-owned helpers for return and pending-completion transitions.

The compiler may still emit no-op/deopt code at this point. The important result
is that all exits are represented explicitly and can be tested without relying
on the shim's implicit `frame.pc` convention.

## Slice 3 — first native straight-line region

Lower constants, moves, primitive loads, and a minimal return path. Keep the
authoritative values in the VM stack and use primitive SSA only after a guard.

Add differential tests for:

- supported operations;
- unsupported operation at entry;
- guard failure before mutation;
- return/deopt after several native operations;
- top-level and nested frames.

Do not proceed if the materialization map is unclear. The compiler should be
able to explain every live value at every exit.

## Slice 4 — native numeric loops

Add native `i32`/`f64` arithmetic, comparisons, conditional branches, and
backward edges. Add loop polling and exact budget/iteration charging.

First target `int-arith`, `float-arith`, and a small local-variable loop. Compare
warm and amortized cold performance against the interpreter. If this slice
does not produce a clear win, stop and profile the generated code before adding
arrays, properties, or calls.

## Slice 5 — deopt and safepoint hardening

Before object lowering, exercise the runtime boundary aggressively:

- exceptions and handler cleanup;
- forced GC/finalization;
- instruction and loop limits;
- nested VM entry/host callbacks;
- recursive frames;
- context drop/backend drop behavior;
- all unsupported control-flow and function kinds.

This is a gate, not optional cleanup. A native property or call path will make
these failures harder to localize.

## Slice 6 — dense elements and named properties

Add feedback snapshots and guarded helper lowerings for:

- dense numeric element reads;
- monomorphic named data-property reads.

Use existing `ElementIC` and `InlineCache` semantics. Do not expose unchecked
shape addresses or private property-map layout. Land direct memory loads only
as a follow-up after the helper version wins and its layout contract is
documented.

Benchmark matching and mismatch cases separately. A specialization that only
wins when its guard always succeeds is still acceptable, but a miss must be
cheap and correct.

## Slice 7 — hotness and automatic installation

Once the explicit compiled entry is faster and safe, connect the runtime to
interpreter function entries and backward edges:

- increment JIT-only hotness counters;
- issue compile requests at safe VM boundaries;
- install entries through the cache;
- run compiled code on subsequent entries/loop headers;
- expose a diagnostic threshold override for tests.

Keep the feature opt-in initially. Do not make every `Context::run` pay for
tiering until cold and real-workload measurements justify it.

## Slice 8 — direct ordinary calls

Add call-site feedback and a VM-owned frame transition ABI. Start with a
compiled callee entry and no inlining. Validate:

- ordinary target hit;
- target replacement/mismatch;
- recursion;
- return and exception propagation;
- stack traces and runtime limits;
- fallback to proxies, bound/native functions, constructors, spread, and
  async/generator calls.

Only after this slice is stable should method lookup plus call be considered
for fusion.

## Slice 9 — workload integration and policy

Run the real browser-shaped/bundle-shaped corpus with cold and warm modes.
Decide whether the public API should remain explicit (`evaluate_jit`/a runtime
handle) or gain a context-level opt-in. Keep default interpreter behavior
unchanged until the workload gate passes.

Document supported platforms, backend lifetime, cache limits, and diagnostic
controls. Update the broader roadmap with measured results rather than
predicted speedups.

## Suggested commit boundaries

Use commit-sized changes such as:

```text
perf(jit): add runtime counters and warm/cold benchmark mode
perf(jit): cache compiled code blocks per backend
perf(jit): add decoded bytecode CFG and explicit exit protocol
perf(jit): lower primitive straight-line regions
perf(jit): lower guarded numeric loops
test(jit): cover deopt, budgets, exceptions, and GC boundaries
perf(jit): lower feedback-backed dense and named reads
perf(jit): add hotness requests and tier installation
perf(jit): enter compiled ordinary callees directly
```

Each performance commit should include its focused test command and an
interleaved interpreter/JIT measurement. Revert a slice that is a wash or
regression rather than stacking the next specialization on top of it.

