# Implementation sequence

Keep Phase 2 in small, independently revertible commits. A later ABI slice
must not be used to hide an earlier measurement or correctness gap.

## Slice 0 — integration baseline (landed)

The 2026-08-02 Ligero gate established opt-in feature/runtime plumbing, exact
finite-budget execution, an observable browser sink, and interleaved cold
measurements. It is W0 evidence only; it must not select the next native ABI.

## Slice 1 — profile before lowering (engine groundwork landed)

Add fallback reasons, native coverage, exit/transition counters, and a fixed
stats snapshot. Run the microbench and the agreed browser-shaped workload.

The bounded engine snapshot and standalone JSON publisher landed on 2026-08-03.
The browser bridge and profile matrix remain; Slice 2 is still blocked on that
evidence.

**Stop/go:** do not proceed until the top blockers are known and the stats
distinguish shim fallback, native deopt, and scheduler transition costs.

Suggested commit:

```text
test(jit): add native coverage and fallback diagnostics
```

## Slice 2 — unblock one measured primitive region

Lower the smallest measured blocker batch. Environment/constant,
bitwise/conversion, and loop-edge operations are candidates, not a checklist.
First record whether the current PC-zero whole-CodeBlock compiler can express
the useful result or whether explicit region identity is required. Add exact
guards and differential tests before adding more opcodes.

**Stop/go:** the selected primitive benchmark must execute a real native loop;
if native coverage remains low, inspect the next blocker instead of starting
OSR or calls.

Suggested commits:

```text
perf(jit): lower guarded binding and constant reads
perf(jit): lower guarded integer conversion and bitwise operations
```

## Slice 3 — region stitching, only if selected

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

## Decision checkpoint A — choose the next boundary

Re-run the fixed matrix after Slice 2 or 3 and rank attributable lost time:

1. interpreted one-shot loop work after the PC-zero opportunity;
2. scheduler round trips to already-compiled monomorphic callees;
3. property/element helper and guard cost;
4. compilation, cache, or admission overhead.

Check in the profile and select exactly one of Slices 4A–4C. Do not begin two
new VM ABIs in parallel. A branch that is not selected remains planned, not
implicitly approved or rejected.

## Slice 4A — loop-header OSR

Add conservative loop-region keys, safe backedge compile requests, OSR entry
guards, and exact deoptimization. Prove budget/exception/GC behavior before
allowing property or call operations in an OSR region.

Before implementation, review the nonzero-PC cache key, materialization map,
backend ownership at the safe compile boundary, and exact finite-budget charge
interval.

**Stop/go:** a one-shot hot loop must show OSR execution and pass the full OSR
test set; otherwise leave OSR disabled and diagnose the materialization gap.

Suggested commit:

```text
perf(jit): enter hot loop regions with guarded OSR
```

## Slice 4B — compiled ordinary calls

First check in the backend ownership/re-entrancy and active-entry lifetime
design. Then implement the VM-owned compiled-call trampoline, target entry
metadata, direct return continuation, and all fallback paths. Start with one
ordinary target and no inlining.

**Stop/go:** matching calls must reduce scheduler round trips and show a warm
win; any stack-trace, recursion, exception, or GC discrepancy blocks further
call optimization.

Suggested commits:

```text
perf(jit): add compiled ordinary call transition
test(jit): cover compiled call frames and fallback paths
```

## Slice 4C — direct guarded storage loads

Measure helper cost, then implement direct dense-element or named-data loads
only for the highest-impact stable snapshot. Keep the helper path as the miss
and compare both forms on positive and negative workloads.

**Stop/go:** direct loads must win on matching shapes without increasing miss,
GC, or pointer-liveness risk.

Suggested commit:

```text
perf(jit): inline the measured guarded storage load
```

## Decision checkpoint B — re-profile

Repeat the same interleaved matrix with diagnostics disabled for headline
timings and enabled in a separate diagnostic run. Proceed to another 4x branch
only if its remaining attributable cost exceeds the agreed threshold and the
first ABI has passed the complete correctness gate.

## Slice 5 — admission and workload policy

Finish coverage-based admission, cache-byte policy, variant retirement, and
threshold tuning. Typed entry keys, duplicate/failure suppression, and a
conservative cache bound are prerequisites introduced with the first Phase 2
entry kind, not deferred until here. Run repeated cold/warm browser
measurements with the sibling workload owner.

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
diagnostic coverage. After every two behavior slices, schedule a behavior-neutral
refactoring commit for duplicated helper ABIs, cache-key construction, exit
mapping, or profiling plumbing exposed by the work. After each 4x ABI branch
and Slice 5, run the full Phase 1 feature matrix and `cargo test --workspace`
before moving to the next ABI boundary.

## Decisions that require explicit review

Surface these before implementation rather than burying them in code:

- whether the compiled-call trampoline receives a function pointer or a
  runtime-owned entry handle;
- how a backend/code-cache lifetime is kept valid during nested calls;
- whether OSR requests compile synchronously at a VM boundary or are deferred;
- the minimum native coverage/admission rule;
- the browser workload's cold-start guardrail and result sink;
- when direct storage access is justified over the helper path.
