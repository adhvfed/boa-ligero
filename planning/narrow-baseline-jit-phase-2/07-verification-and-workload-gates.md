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

- strict/sloppy/lexical/derived-constructor receiver reads if `This` is ever
  selected as part of an admitted useful region;
- global-declarative binding reads, same- and changed-representation mutation,
  TDZ, realm separation, forced GC, exact budget replay, and explicit rejection
  of global-object, stack, dynamic-environment, and direct-eval-affected forms;
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

Moving a first-blocker diagnostic to the next unsupported opcode does not
satisfy Gate C. The selected CodeBlock must pass production admission and
execute the complete measured hot region; a helper that remains denied or exits
at the immediately following opcode is a no-go.

For Slice 2C, Gate C additionally requires the floating-point binding control
to retain at least a 2× warm win with matching sink and zero steady-state
deopts. Call-containing entries must compile no artifact and keep the method,
flat-call, and property negative controls within 5% of interpreter medians.
W0 must retain its native loop, checksum, paint structure, and cold guardrail.
**Passed 2026-08-03:** the post-refactor float control is 4.80× faster, all
four negative controls are within 5%, and W0 is 30.0% lower at the median with
one native artifact, 999 entries, zero deopts, 387 display items, and 258 paint
segments. See the [Slice 2C closure](13-slice-2c-closure-2026-08-03.md).

### Gate O — OSR

A one-shot hot loop enters native code from an interpreter backedge and passes
all budget, exception, GC, and guard-failure tests.

### Gate H — hot-but-unentered tiering

With zero compilations and zero native entries, enabling the tier must remain
within 5% of interpreter time for below-threshold and statically ineligible
loops. A separate default-threshold control must prove that reaching hotness at
a nonzero PC does not keep paying unbounded map/scheduler bookkeeping. Report
executed backedges, threshold crossings, entries, and artifacts so an OSR win
cannot hide dormant-tier overhead.

**Passed 2026-08-03:** seven fresh-process pairs measured +0.429% for the
eligible one-shot loop and +0.939% for the statically ineligible loop, both
with zero artifacts and entries. Production observation stops at 256
backedges and one dormant-frame handoff; explicit diagnostics still count all
2,000,000 backedges exactly. See the
[Gate H closure](16-slice-3b-gate-h-closure-2026-08-03.md).

### Gate K — calls

Matching ordinary calls use the compiled-call path and show a warm win;
non-matching targets retain the interpreter path and all frame semantics.

### Gate W — workload

At least one browser-shaped workload improves after compilation cost is
included. The JIT remains opt-in until repeated measurements establish a
stable cold/warm result.

The 2026-08-02 numeric/DOM fixture satisfies **W0, integration baseline**:
five interleaved fresh-process pairs improved median cold load by 26.8%, with
the same sink and paint structure. It does not satisfy **W1, selection
profile**, because it was deliberately shaped to compile natively and cannot
rank unsupported bytecode, OSR, call, or storage costs. Before default
enablement, **W2, representative breadth** requires stable wins or a documented
neutral result across the agreed bundle/site set, with diagnostics disabled in
headline timings.

**Decision checkpoint A now satisfies the selection form of Gate P for one
narrow branch.** Schema 7 plus corrected 4,096-record runner controls retain
zero-drop loop/call/storage evidence. The one-shot numeric body is the only
candidate with a complete supported region and a matching 3.96× PC-zero native
counterfactual, so conservative loop-header OSR proceeds to its ABI design
review. No engine or broader application loop passes the first OSR screen;
calls have no cached native targets in broad rows, and counted storage is
interpreted. This selects an ABI to test, not representative breadth or JIT
default enablement. See the
[dated selection](19-decision-checkpoint-a-selection-2026-08-03.md).

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

## Quality and refactoring gate

After every two behavior slices, reserve a refactoring checkpoint for duplicated
entry/exit metadata, helper ABIs, cache-key construction, and diagnostic reason
mapping discovered in the preceding work. Refactoring commits must be behavior
neutral and separately revertible. Formatting plus warning-denying Clippy on
affected JIT/VM targets is required for each slice; pre-existing wider warnings
must be recorded rather than silently normalized into a growing baseline.
