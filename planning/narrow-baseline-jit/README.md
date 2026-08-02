# Narrow baseline JIT

Status: baseline tier implemented; workload integration remains.
This directory is the implementation plan and verification contract for
turning Boa's experimental Cranelift integration into a small, safe,
measurable hot-code tier.

## Implementation checkpoint — 2026-08-02

The first native baseline slices are now landed behind the opt-in `jit`
feature. Hot ordinary functions can lower primitive `i32`/`f64` arithmetic and
comparisons, backward loops, dense integer/floating indexed reads,
monomorphic data-property reads, and guarded direct calls to ordinary
JavaScript functions. Native values are materialized at helper, call, and
deoptimization boundaries; unsupported operations use the existing shim or
interpreter fallback.

The final release warm-loop probe (`jit_loop_perf`) measured 6.309 ms with the
native baseline versus 49.607 ms in the interpreter on the same machine
(0.127 ratio, including a separate 3.748 ms compile phase during warm-up).
The 1,999 native entries had zero deoptimizations in this matching-shape
probe. This is a synthetic signal, not a workload gate. The follow-up
hardening checkpoint has 25
filtered JIT tests (24 active, 1 ignored), covering type/overflow fallback,
call-target replacement, runtime limits, exceptions, recursion, array holes,
and forced GC around property guards. The remaining work is guard
observability and browser-shaped cold/warm measurements.

The goal is not to compile all JavaScript immediately. The goal is to compile
the small set of operations that dominate hot, ordinary functions and loops,
while making every unsupported or invalidated case return to the existing VM
without observable differences.

## The outcome we want

Boa should be able to identify a hot ordinary `CodeBlock`, compile a supported
region to native code, execute it against the existing VM stack, and fall back
to the interpreter at an exact bytecode boundary when a guard, exception,
runtime limit, call, or unsupported operation requires it.

The first useful native paths are:

- register moves and primitive constant loads;
- guarded `i32` and `f64` arithmetic and comparisons;
- simple backward loops;
- dense numeric element loads;
- monomorphic named-property loads;
- direct calls to ordinary JavaScript functions whose target is known and
  compiled.

The implementation must remain optional behind the existing `jit` feature and
must not add work to normal interpreter execution when the feature is disabled.

## Current starting point

The repository already contains three useful pieces:

1. `core/jit` proves that Cranelift can emit native code in the workspace.
2. `core/engine/src/jit/mod.rs` can compile a `CodeBlock` and call into a real
   `Context`.
3. The current engine JIT uses an `extern "C"` shim for every opcode. That is a
   correctness bridge and a deoptimization prototype, but it still pays most
   of the per-opcode interpreter cost and is not cached or tiered.

`Script::evaluate_jit` is currently an explicit experimental entry point. It
compiles the requested code block for the supplied backend instead of
automatically tiering hot functions. The plan below evolves that path in small
steps rather than making the normal `Context::run` depend on Cranelift at once.

## Design decisions

These decisions are binding for the first implementation sequence:

- Compile one `CodeBlock` at a time. Do not start with whole-program or
  cross-function compilation.
- Keep a JIT runtime scoped to a context/realm/backend. Do not share generated
  code or shape assumptions globally between realms.
- Treat `vm.stack` and the live `CallFrame` as the authoritative JavaScript
  state. A second GC-visible register file is out of scope for the first tier.
- Keep only primitive values (`i32` and `f64`) in Cranelift SSA values across
  native instructions. Do not keep raw `Gc`, `JsObject`, `JsValue`, shape, or
  property-map pointers across a helper call or safepoint.
- Lower only an explicit allowlist of instructions. Unsupported instructions
  must exit to the interpreter; they must never be silently approximated.
- Use guards and deoptimization for type and shape assumptions. Do not mutate
  the interpreter's bytecode into an assumption that cannot be undone.
- Generated code must never unwind through an `extern "C"` boundary. Helpers
  communicate exceptions and exits through an explicit status protocol.
- Keep the current shim runner as a test/fallback tool until the native
  lowering path has differential coverage and an independent performance win.

## Document map

- [Goals, boundaries, and gates](00-goals-and-boundaries.md) — scope,
  non-goals, success criteria, and risk tripwires.
- [Runtime, cache, and tiering](01-runtime-and-tiering.md) — ownership,
  hotness counters, code cache, eligibility, and installation.
- [Bytecode and compiler pipeline](02-bytecode-and-compiler.md) — decoding,
  CFG construction, region selection, value representation, and lowering
  interfaces.
- [Exit, deoptimization, and GC contract](03-exit-deopt-gc.md) — the runtime
  ABI and the invariants required to resume in the interpreter safely.
- [Native lowering sequence](04-native-lowering.md) — the order for primitive
  operations, arrays, properties, and direct calls.
- [Verification and benchmarks](05-verification-and-benchmarks.md) — tests,
  differential checks, observability, and performance gates.
- [Implementation sequence](06-implementation-sequence.md) — independently
  committable slices and stop/go criteria.

The broader rationale remains in the [Cranelift roadmap](../js-performance-roadmap/09-cranelift-jit.md).
This directory is the more concrete implementation plan for the narrow first
tier.
