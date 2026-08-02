# Implementation sequence

Keep Phase 2 in small, independently revertible commits. A later ABI slice
must not be used to hide an earlier measurement or correctness gap.

## Slice 0 — profile before lowering

Add fallback reasons, native coverage, exit/transition counters, and a fixed
stats snapshot. Run the microbench and the agreed browser-shaped workload.

**Stop/go:** do not proceed until the top blockers are known and the stats
distinguish shim fallback, native deopt, and scheduler transition costs.

Suggested commit:

```text
test(jit): add native coverage and fallback diagnostics
```

## Slice 1 — unblock measured primitive regions

Lower the smallest measured batch of environment/constant, integer
bitwise/conversion, and loop-edge operations. Add exact guards and differential
tests before adding more opcodes.

**Stop/go:** the selected primitive benchmark must execute a real native loop;
if native coverage remains low, inspect the next blocker instead of starting
OSR or calls.

Suggested commits:

```text
perf(jit): lower guarded binding and constant reads
perf(jit): lower guarded integer conversion and bitwise operations
```

## Slice 2 — region stitching

Represent native regions and unsupported exits explicitly. Make supported
forward/backward edges stay in Cranelift while preserving exact PC and
materialization maps at exits.

**Stop/go:** malformed targets, handler boundaries, and unsupported control
flow must reject/deopt safely; native code must not invoke the opcode shim for
selected operations.

Suggested commit:

```text
perf(jit): stitch native regions across supported control flow
```

## Slice 3 — loop-header OSR

Add conservative loop-region keys, safe backedge compile requests, OSR entry
guards, and exact deoptimization. Prove budget/exception/GC behavior before
allowing property or call operations in an OSR region.

**Stop/go:** a one-shot hot loop must show OSR execution and pass the full OSR
test set; otherwise leave OSR disabled and diagnose the materialization gap.

Suggested commit:

```text
perf(jit): enter hot loop regions with guarded OSR
```

## Slice 4 — compiled ordinary calls

Implement the VM-owned compiled-call trampoline, target entry metadata, direct
return continuation, and all fallback paths. Start with one ordinary target
and no inlining.

**Stop/go:** matching calls must reduce scheduler round trips and show a warm
win; any stack-trace, recursion, exception, or GC discrepancy blocks further
call optimization.

Suggested commits:

```text
perf(jit): add compiled ordinary call transition
test(jit): cover compiled call frames and fallback paths
```

## Slice 5 — direct guarded storage loads

Measure helper cost, then implement direct dense-element or named-data loads
only for the highest-impact stable snapshot. Keep the helper path as the miss
and compare both forms on positive and negative workloads.

**Stop/go:** direct loads must win on matching shapes without increasing miss,
GC, or pointer-liveness risk.

Suggested commit:

```text
perf(jit): inline the measured guarded storage load
```

## Slice 6 — admission and workload policy

Add region/call entry cache keys, coverage-based admission, repeated-failure
suppression, cache bounds, and threshold tuning. Run repeated cold/warm
browser measurements with the sibling workload owner.

**Stop/go:** keep the JIT opt-in and revert any policy that wins only the hot
loop while worsening complete workload time.

Suggested commits:

```text
perf(jit): add region admission and bounded cache policy
docs(jit): record Phase 2 workload gate
```

## Cross-slice verification

After every compiler/VM boundary change:

```text
cargo fmt --all -- --check
cargo test -p boa_engine --lib --features jit jit_
cargo check -p boa_engine --lib --no-default-features
cargo test -p boa_engine --lib
```

After each completed slice, run the cold/warm benchmark subset and inspect
diagnostic coverage. After Slices 3–6, run the full Phase 1 feature matrix and
`cargo test --workspace` before moving to the next ABI boundary.

## Decisions that require explicit review

Surface these before implementation rather than burying them in code:

- whether the compiled-call trampoline receives a function pointer or a
  runtime-owned entry handle;
- how a backend/code-cache lifetime is kept valid during nested calls;
- whether OSR requests compile synchronously at a VM boundary or are deferred;
- the minimum native coverage/admission rule;
- the browser workload's cold-start guardrail and result sink;
- when direct storage access is justified over the helper path.
