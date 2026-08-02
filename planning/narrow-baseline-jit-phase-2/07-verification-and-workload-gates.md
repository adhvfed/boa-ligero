# Verification and workload gates

Phase 2 changes more VM boundaries than Phase 1. Every performance claim must
be paired with an independent semantic and workload check.

## Differential test layers

For each new region, OSR entry, call transition, and storage guard, compare:

1. interpreter-only execution;
2. explicit JIT execution;
3. context-owned tiered execution;
4. cold and warm execution when the cache/threshold behavior matters.

Compare values, thrown error class/message where stable, observable object
state, stack traces, runtime-limit behavior, and final sinks. Keep the
interpreter as the reference implementation.

## Required Phase 2 cases

### Coverage and regions

- global/lexical binding reads and mutation/deletion invalidation;
- bitwise and `ToInt32` edge cases, including NaN, infinities, `-0`, and
  out-of-range numbers;
- region fallthrough, forward branches, backward branches, zero-iteration
  loops, and unsupported operations at each boundary;
- register reuse and materialization after several native operations.

### OSR

- one-shot loop entry from an interpreter backedge;
- loop header guard miss and representation change;
- budget/limit exhaustion during native iterations;
- exception handler entry after a native helper;
- forced GC, recursion, nested frames, and host re-entry fallback.

### Compiled calls

- matching target and return continuation;
- target replacement, polymorphism, and uncompiled target;
- recursion and stack traces with visible caller/callee frames;
- thrown/caught/uncaught exceptions and `finally` cleanup;
- runtime limits, GC, native/proxy/bound/constructor/async fallback.

### Storage and feedback

- dense hits, holes, sparse/out-of-bounds reads, storage changes;
- named own-data hits, accessors, prototype mutation, dictionary objects,
  megamorphic sites, and forced GC;
- shape/binding liveness and pointer-reuse misses.

## Instrumentation validation

Use tests that assert the diagnostic reason, not just the final result:

- unsupported opcode reports the expected first blocker;
- a guard miss reports its site and resume PC;
- OSR success reports an OSR entry and native loop execution;
- compiled calls report a direct transition rather than a scheduler round trip;
- budget and exception exits retain their specific kind;
- JIT-disabled builds contain none of the counters or runtime branches.

Diagnostic assertions may be feature-gated and should not make normal tests
depend on unstable raw counts.

## Benchmark matrix

Run a fixed matrix with interpreter and JIT controls:

| Workload | Primary question |
| --- | --- |
| `int-arith`, `float-arith` | Do common primitive loops stay native? |
| `array-numeric-sum` | Does environment load plus dense element access compose? |
| `property-mono`, `property-poly4` | Do matching and mismatch shapes separate cleanly? |
| `fn-call-flat`, `method-call-mono` | Does a compiled call avoid scheduler overhead? |
| one-shot loop fixture | Does OSR matter without a second function entry? |
| V8/Octane-style scripts | Does the result generalize beyond microbenchmarks? |
| fixed `ligero-browser` workload | Does cold/warm page work improve? |

For each row report cold time, warm time, compile time, native coverage,
helper/transition counts, deopts by reason, and the final sink. Do not put a
shim-only or DCE-suspect result in a headline geomean.

## Stage gates

### Gate P — profile

The workload profile identifies the top fallback/transition costs and names
the next slice. No new specialization lands based only on the synthetic loop.

### Gate C — coverage

The selected primitive/array workload executes a measured native region through
its hot loop, with a warm speedup over the interpreter and no semantic or cold
regression outside the target shape.

### Gate O — OSR

A one-shot hot loop enters native code from an interpreter backedge and passes
all budget, exception, GC, and guard-failure tests.

### Gate K — calls

Matching ordinary calls use the compiled-call path and show a warm win;
non-matching targets retain the interpreter path and all frame semantics.

### Gate W — workload

At least one browser-shaped workload improves after compilation cost is
included. The JIT remains opt-in until repeated measurements establish a
stable cold/warm result.

## Platform matrix

Keep the Phase 1 matrix green:

```text
cargo test -p boa_engine --lib
cargo test -p boa_engine --lib --features jit
cargo check -p boa_engine --lib --features jit
cargo check -p boa_engine --lib --no-default-features
cargo test --workspace
```

Run native JIT tests on supported x86-64 and AArch64 environments. Do not
assume that function-pointer width, code placement, calling convention, or
object layout is host-specific only because development runs use Apple
Silicon.
