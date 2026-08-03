# Implementation sequence

Keep Phase 2 in small, independently revertible commits. A later ABI slice
must not be used to hide an earlier measurement or correctness gap.

## Slice 0 — integration baseline (landed)

The 2026-08-02 Ligero gate established opt-in feature/runtime plumbing, exact
finite-budget execution, an observable browser sink, and interleaved cold
measurements. It is W0 evidence only; it must not select the next native ABI.

## Slice 1 — profile before lowering (complete)

Add fallback reasons, native coverage, exit/transition counters, and a fixed
stats snapshot. Run the microbench and the agreed browser-shaped workload.

The bounded engine snapshot, standalone JSON publisher, Ligero bridge, and Gate
P matrix completed on 2026-08-03. The dated profile records seven micro
controls, three bounded engine workloads, W0, and a user-authorized application
load. It identifies `This` and name/global reads as the main static frontiers,
but also shows that several tiny native helper bodies regress complete warm
workloads.

**Stop/go:** do not proceed until the top blockers are known and the stats
distinguish shim fallback, native deopt, and scheduler transition costs.

Suggested commit:

```text
test(jit): add native coverage and fallback diagnostics
```

## Slice 2A1 — remove interpreter-only scheduler tax (complete)

The first admission prototype is rejected. Although it emitted neither native
code nor a shim for losing bodies, those bodies remained roughly 1.8–2.0×
slower than JIT-disabled interpretation because the context-owned tier wraps
the full opcode dispatch loop. Record and move only the function-entry, call-
target, and backward-edge observations needed for tiering so an interpreter-
only frame can use the ordinary fast path between events.

**Stop/go:** with the JIT enabled and no admitted body, the 0/4/8-addition
controls must be within 5% of the disabled interpreter while retaining exact
sinks, budgets, and diagnostics. W0 and the focused deopt/exception/GC tests
must remain unchanged.

Suggested commit:

```text
perf(jit): bypass dormant tiering in interpreter frames
```

## Slice 2A2 — enforce measured admission before widening coverage (complete)

Use the Gate P positive and negative controls to select the smallest explicit
native-admission rule that suppresses known losing tiny/helper-dominated
entries without excluding W0's winning numeric kernel. Record the crossover
experiment, keep diagnostic and headline timing separate, and test duplicate
shim/failure suppression. This slice adds no entry kind or VM ABI.

The source-free crossover and rejected first prototype are recorded in
[the dated checkpoint](10-admission-crossover-2026-08-03.md). Re-run that
protocol after Slice 2A1; do not assume the provisional loop-or-45 rule remains
optimal once scheduler overhead changes.

**Stop/go:** property, flat-call, polymorphic-property, and method controls must
not regress beyond the recorded noise guardrail, while W0 must retain native
execution and its visible checksum. If no static admission rule separates the
cases, stop and attribute transition cost before tuning a threshold.

Suggested commit:

```text
perf(jit): suppress unprofitable function entries
```

## Slice 2B — review receiver frontier (complete; no-go)

After Slice 2A, review guarded `This` loading as the smallest measured blocker
batch: it is the leading engine frontier and blocks the method control at PC 18.
First record whether the current PC-zero whole-CodeBlock compiler can express
the useful result. Add exact receiver representation, GC, strict/sloppy call,
and finite-budget guards and differential tests before adding another opcode.

The [dated review](11-receiver-frontier-review-2026-08-03.md) found that it
cannot. `SetPropertyByName` is the next unsupported opcode in the measured
method, and the complete 16-instruction helper is below the production admission
threshold even if both operations were supported. Standalone receiver lowering
therefore stops at review: no allowlist or ABI change is scheduled.

Name/global reads remain the next measured candidate. They require a separate
VM-owned binding identity and invalidation design; if that contract is
unavailable, do not substitute a raw environment pointer. Bitwise/conversion
and loop-edge operations remain candidates, not a checklist.

**Stop/go:** no-go. The selected method control cannot execute an admitted
native body after standalone `This` lowering. Revisit receiver coverage only as
part of a separately reviewed region that passes production admission and
complete-workload timing.

Review commit:

```text
docs(jit): reject standalone receiver lowering
```

## Slice 2C0 — reject non-continuable call entries (complete)

The binding prototype exposed a production-admission hole: a backward branch
currently admits a caller even when its static profile contains calls, although
generated code has no continuation after the first call. Deny call-containing
function entries before compilation, install neither native nor shim artifact,
and publish `denied_call_boundary`. Preserve a deliberate test-only override
for the existing call guard/deopt semantic tests.

**Stop/go:** the method, flat-call, and property negative controls must compile
zero artifacts and remain within 5% of interpreter medians. The floating-point
control and W0 contain no call boundary and must retain their native entries.

Suggested commit:

```text
perf(jit): reject non-continuable call entries
```

Landed as part of Boa `345767c5`: production admission reports
`denied_call_boundary`, creates no artifact, and retains an explicit test-only
override for call-lowering semantics.

## Slice 2C1 — lower one global-declarative binding read (complete)

The [dated review](12-binding-read-and-call-boundary-review-2026-08-03.md)
approves exactly `GetName` with a compile-time `GlobalDeclarative` locator. On
every entry, validate locator stability, read the current binding through the
active frame's realm, and copy it into a VM register before specializing or
using it. Retain no raw environment pointer, binding value, or cross-realm
snapshot. Do not combine `GetNameGlobal`, global-object, stack, eval-affected,
write/delete, bitwise, array-storage, receiver, region, OSR, or call ABI work.

Differential coverage must prove same-representation reassignment, changed-
representation guard deopt, TDZ/`ReferenceError`, direct-eval rejection, realm
separation, forced GC, and exact finite-budget replay. Global-object replacement
and deletion are exclusions to be reviewed separately, not acceptance criteria
for this lowering. Current PC-zero whole-CodeBlock and VM-register
materialization are sufficient for the target loop.

**Stop/go:** the floating-point control must execute one complete admitted body,
retain at least a 2× warm win with a matching sink and zero steady-state deopts,
and pass all semantic tests. Slice 2C0's negative controls and W0 remain gates.

Suggested commit:

```text
perf(jit): lower stable global binding reads
```

Landed in Boa `345767c5`; Ligero `2c39eafe` projects schema-4 admission and
binding-exit reasons. The semantic and release gates are recorded in the
[Slice 2C closure](13-slice-2c-closure-2026-08-03.md).

## Slice 2C2 — behavior-neutral helper refactor (complete)

The admission correction and binding lowering are two behavior slices. Pay the
scheduled refactor before selecting another execution ABI: consolidate the
duplicated generated-helper declaration or VM-register materialization plumbing
exposed by 2C, without widening the allowlist or changing diagnostics.

**Stop/go:** the float positive control, call-heavy negative controls, focused
JIT suite, feature-disabled checks, formatting, and affected strict Clippy must
remain unchanged.

Suggested commit:

```text
refactor(jit): unify binding helper materialization
```

Landed as Boa `54a109f6`. Compiler emission now borrows one generated helper
table and no longer stores or copies an unused 536-byte table. Native coverage,
diagnostics, and the allowlist are unchanged.

## Decision checkpoint A0 — verify attribution before choosing an ABI

The post-2C review found a measurement gap rather than a defensible winner.
The one-shot numeric control proves a useful nonzero-PC opportunity, but also
exposes a separate hot-but-unentered regression. Production call-containing
callers now correctly install no artifact, which means the old native
scheduler bridge cannot measure their current opportunity. Storage records are
static instruction counts, not dynamic helper cost. Record this outcome; do
not treat lack of a native exit as lack of workload cost.

## Slice 3A — bounded boundary-attribution telemetry

Add diagnostics-only, source-free, bounded site records needed to compare the
remaining branches without changing admission, lowering, cache ownership, or
the execution ABI:

- call-site executions, ordinary/non-ordinary classification, first/same/
  changed ordinary target counts, and whether the target already had a cached
  native or shim variant for the current budget mode;
- loop-header/backedge executions, the first hotness crossing, whether the
  frame had already passed PC zero, and dry-run eligibility/rejection for the
  conservative first OSR shape;
- executed named/dense access sites and, for existing native helper paths,
  guard hit/miss plus helper-load counts. Never retain a value, object, shape,
  environment, function name, source, URL, property name, or raw pointer.

Bound each record kind and count dropped observations without allocating an
unbounded dropped-site set. Headline timing keeps diagnostics disabled. A
separate diagnostics run must have zero dropped observations before it is used
for ranking. Keep `denied_call_boundary` unchanged. A scheduler-call-exit
aggregate may characterize the existing in-crate test override, but production
selection must use production call-site observations.

**Stop/go:** exact sinks remain equal; zero-cap/default/max-cap and source-free
serialization tests pass; normal diagnostics-disabled negative controls stay
within 5%; diagnostics are explicitly excluded from headline timing. This
slice may identify a candidate, but cannot itself relax admission.

Suggested commits:

```text
test(jit): attribute hot execution boundaries
feat(jit): project boundary diagnostics to ligero
```

### Slice 3A1 — interpreted loop sites (complete)

Boa `0233f60f` and Ligero `48573227` add and project schema-6 loop records.
The eligible and bitwise-ineligible one-shot controls retain exact 2,000,000-
backedge evidence in explicit diagnostics, including observations after a
frame becomes dormant, while diagnostics-disabled medians remain inside Gate
H's 5% parity bound. Static candidacy is not OSR approval. See the
[dated checkpoint](17-slice-3a-loop-attribution-2026-08-03.md).

### Slice 3A2 — storage attribution (complete)

Land interpreted storage-site attribution first. Observe only coarse named,
dense, computed, and specialized-length categories plus the pre-operation
state of Boa's existing named/dense inline caches. The observer must not run a
key conversion or any user-visible operation and must retain no key, name,
value, object, shape, source, URL, or pointer.

Then add fixed native guard/load aggregates through a distinct diagnostic
artifact variant. A typed cache key must separate budget mode from diagnostic
instrumentation; diagnostics-disabled execution must select the production
artifact and helper ABI with no new counter update. Keep interpreted sites
bounded with dropped-observation accounting and native counts fixed-size.

**Stop/go:** exact named/dense hit and miss controls, computed/length not-
applicable controls, dormant denied frames, zero/default/max caps, source-free
serialization, guard-miss replay, finite budgets, GC, feature-disabled builds,
and diagnostics-disabled parity all pass. Project schema 7 through Ligero only
after the engine contract is stable.

The separately revertible behavior-neutral refactor is the typed cache-key
plumbing required to isolate the diagnostic artifact variant.

Boa `753ca3ea` lands bounded interpreted sites; `04c18a03` isolates typed cache
keys as the prerequisite refactor; `8b9b58c3` adds diagnostic-only native
helper variants; and `a398b455` closes the affected strict-lint delta. Ligero
`e36b438f` projects schema 7. Seven diagnostics-off Gate H pairs measure 0.201%
above and 0.101% below the interpreter medians. See the
[closure record](18-slice-3a-storage-attribution-closure-2026-08-03.md).

## Slice 3B — hot-but-unentered tier guardrail (complete)

Add a durable one-shot numeric fixture and a separate ineligible-loop control.
Measure unreachable thresholds, default hotness with zero native entry, and an
intentional threshold-1 PC-zero native control. Once a frame/site has supplied
the hotness information needed for the next safe decision, stop repeating
expensive map/scheduler bookkeeping while preserving future PC-zero entry,
runtime limits, exact counters or their documented bounded replacement, and
the interpreter as the semantic authority.

The recorded reproducer (`SHA-256
0f54effe6b51cb7d0b29b88f478474cd3e9576e8a44f48fa1a6e90b12afef223`)
measured 27.455 ms interpreter, 37.963 ms default JIT with zero artifacts, and
7.429 ms for the intentional PC-zero native control. The last number is OSR
feasibility evidence; it is not an OSR result.

**Stop/go:** Gate H passes before a 4A speedup is accepted. Do not implement
OSR merely to conceal a zero-entry tier regression.

**Passed:** Boa `d64fe095` scopes admission and hotness state to the backend
generation without a backend hash map. Boa `cc07a908` latches hot frames and
returns nonzero-PC frames that cannot branch to PC zero to dormant interpreter
dispatch. Seven-pair release medians are within 0.94% of the interpreter for
both durable fixtures; explicit diagnostics preserve exact counts. Ligero
`6594d5a2` projects the new counters. See the
[Gate H closure](16-slice-3b-gate-h-closure-2026-08-03.md).

Suggested commit:

```text
perf(jit): bound hot nonzero backedge bookkeeping
```

## Decision checkpoint A — choose the next boundary (complete)

With Slice 3A complete, re-run the fixed matrix and rank attributable lost
time:

1. interpreted one-shot loop work after the PC-zero opportunity;
2. scheduler round trips to already-compiled monomorphic callees;
3. property/element helper and guard cost;
4. compilation, cache, or admission overhead.

Check in the profile and select exactly one of Slices 4A–4D. Do not begin two
new VM ABIs in parallel. A branch that is not selected remains planned, not
implicitly approved or rejected.

The [fixed zero-drop matrix](19-decision-checkpoint-a-selection-2026-08-03.md)
selects Slice 4A's conservative numeric loop-header OSR as the only next ABI.
The one-shot body has 2,000,000 eligible backedges and a 3.96× PC-zero native
counterfactual. Calls lack cached native targets in broad rows; direct storage
cannot consume interpreted sites; region stitching has no complete measured
region. No engine or broader application loop passes the first OSR screen, so
this selection is intentionally narrow.

## Slice 4A — loop-header OSR

Add conservative loop-region keys, safe backedge compile requests, OSR entry
guards, and exact deoptimization. Prove budget/exception/GC behavior before
allowing property or call operations in an OSR region.

Before implementation, review the nonzero-PC cache key, materialization map,
backend ownership at the safe compile boundary, and exact finite-budget charge
interval.

### Slice 4A0 — ABI design review (next)

Check in the typed region key, stable post-backedge compile boundary, exact
live-value/materialization map, pre-effect guard/refund rules, finite-budget
charge interval, backend-generation ownership, cache/failure bounds, and all
first-shape exclusions before editing the compiler or scheduler. Calls,
properties, allocation, handlers, eval/with, suspension, host re-entry, object
live-ins, and unknown stack state remain rejected.

### Slice 4A1 — first numeric OSR shape

Implement only the reviewed shape. After its behavior and diagnostic slices,
schedule a separately revertible behavior-neutral refactor of region-key,
materialization-map, or exit-mapping plumbing before a second execution ABI is
considered.

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

## Slice 4D — region stitching, only if selected

Represent native regions and unsupported exits explicitly. Make supported
forward/backward edges stay in Cranelift while preserving exact PC and
materialization maps at exits. This is an alternative evidence-selected ABI
branch, not a prerequisite that may land before Decision checkpoint A.

**Stop/go:** malformed targets, handler boundaries, and unsupported control
flow must reject/deopt safely; native code must not invoke the opcode shim for
selected operations; the complete measured region must pass production
admission and workload timing.

Suggested commit:

```text
perf(jit): stitch native regions across supported control flow
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
