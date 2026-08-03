# Default-JIT resource bounds design — 2026-08-03

## Decision

D1 will put every production runtime artifact owned by one `JitBackend` behind
one lifetime governor. The backend will retain no more than 192 function-entry
variants and 64 exact loop-region keys, begin no attempt after 8 MiB of machine-
code payload has been accounted, and retain no backend whose final bounded
attempt crosses that payload threshold. It stops new compilation after an
attempt makes observed cumulative code-generation time exceed 100 ms or one
attempt takes more than 10 ms. A function with more than 1,024 decoded bytecode
instructions is denied by a bounded decoder before Cranelift IR construction.
Existing ready entries remain reusable after a breaker trips, except when a
code-payload overrun retires the whole owning module because individual
artifacts cannot be reclaimed.

This is a no-eviction policy. Cranelift's `JITModule` owns finalized code until
the backend is dropped and cannot reclaim one artifact. Removing a cache entry
would therefore retain the executable allocation, lose useful reuse, and make
recompilation pressure possible. Capacity means “retain and reuse what is
already ready; compile no unseen key until this backend generation is
dropped.” `Context::disable_jit` and context teardown remain the retirement
boundaries for normally retained artifacts. A payload overrun adds the
immediate `RetireAndInterpret` scheduler boundary defined below.

The policy governs scripts admitted by the context-owned tier. It does not
change opcode eligibility, thresholds, remote-script policy, or the JIT
default. The default remains opt-in until D0–D4 pass.

## Measured scale and safety margin

The fixed Decision-B corpus was profiled structurally with schema 8 and the
release runner whose SHA-256 is
`664db36d29b97f8b1fb57168ed778dda462f3782371f61f4e0cdf7b4dea0ac8c`.
This run is not performance evidence because the host did not pass D0's
quiescence preflight. Its source-free shape and artifact sizes are still valid
inputs to a resource policy:

| Observation | Result |
| --- | ---: |
| Largest decoded body in the nine-micro/three-engine corpus | 432 instructions |
| Largest admitted PC-zero body in that run | 23 instructions |
| Emitted PC-zero artifact | 960 bytes |
| Its synchronous compilation | 0.500 ms |
| One-shot OSR region | 6 instructions, 748 bytes, 0.473 ms |
| W0 retained loop artifact | 3,240 bytes |
| Recorded W0 total compilation | about 3.7 ms |

The 1,024-instruction ceiling is more than twice the widest decoded body in
the fixed corpus, more than 42 times the admitted positive PC-zero body, and
eight times the existing 128-instruction loop-region ceiling. At the observed
PC-zero density of about 42 emitted bytes per decoded instruction, 192 maximum-
size function variants would account for about 7.9 MiB. The 8 MiB backend
breaker therefore matches the retained-key and body limits while leaving much
more room than the current positive workloads use.

The 10 ms slow-attempt breaker is over twice the recorded complete W0 compile
cost and roughly twenty times the isolated positive artifact costs. The 100 ms
non-refilling cumulative breaker permits repeated useful compilation while
placing an order-of-magnitude ceiling above one acceptable attempt. These are
security/cold-start circuit breakers, not performance targets; D3 retains the
stricter per-page time and RSS rollback gates.

## Exact bounds

All counts are per `JitBackend` generation and use saturating accounting.

| Resource | Exact bound | Ownership and composition |
| --- | ---: | --- |
| Unique runtime artifact keys | 256 | 192 function variants plus 64 reserved loop variants |
| Function-entry cache states | 192 | Every `(code ID, budgeted, diagnostic)` key consumes one state |
| Loop region states | 64 | Existing exact typed-key cap; plans and ready artifacts are subsets |
| Retained accounted code payload | 8 MiB | Native PC-zero, shim, and loop `code_buffer().len()` together; not allocator/RSS bytes |
| Retained loop code payload | 1 MiB | Existing sub-threshold, now composed with immediate retirement on overrun |
| Cumulative compile threshold | 100 ms | Non-refilling post-attempt threshold; observed time can overshoot once |
| Slow-attempt threshold | 10 ms | Post-attempt threshold for PC-zero and loop compilation; elapsed time can overshoot |
| PC-zero body | 1,024 decoded instructions | Checked during static admission before IR construction |
| Legacy call-target sites | 1,024 | Existing sites stay stable; unseen sites are dropped at capacity |
| Detailed diagnostic records | 4,096 per class | Existing hard cap; six classes, at most 24,576 records total |
| Diagnostic index entries | 4,096 per indexed class | Call, loop, and storage only; at most 12,288 total |

The 64 loop slots are reserved even if no loop has yet been observed. Filling
the 192 function states cannot consume OSR capacity. Conversely, filling loop
state does not reduce the 192-function allowance. A loop key can occupy one
entry in each of `loop_regions`, `loop_plans`, and `loop_cache`, so the exact
map bound is 64/64/64 rather than a misleading claim that there are only 64
hash-map entries. The three plan/artifact maps must always be subsets of the
state keys. Together with 192 function states, production artifact ownership
therefore has at most 256 unique keys and 384 hash-map entries.

The 8 MiB/1 MiB values bound payload retained by a usable backend, not physical
executable allocation. `code_buffer().len()` excludes page rounding,
relocations, trampolines, module/compiler metadata, allocator slack, and
transient compiler memory. The matched peak-RSS gate below separately bounds
the observed process cost. A physical executable-allocation quota would need
an allocator API or disposable per-attempt module and is not claimed here.

Diagnostics remain explicitly opt-in. `compile_records`, `admission_records`,
and `exit_records` are bounded vectors. Call, loop, and storage each have one
bounded vector and one index map whose cardinality cannot exceed its vector.
The only retained strings are fixed opcode names from the engine taxonomy;
source, URL, property name/value, object identity, and raw pointers remain
forbidden.

## Admission and breaker order

For a function entry:

1. Look up the exact key. A ready entry runs even when every breaker is closed;
   a terminal failed entry falls back without retry.
2. Deny bodies over 1,024 decoded instructions during static admission and
   cache that decision in the existing generation-scoped `CodeBlock` state.
   Decoding must stop at instruction 1,025 rather than build an unbounded
   temporary vector merely to discover that the body is too large.
3. For an unseen key, check the 192-state function capacity, global code-byte
   breaker, cumulative-time breaker, and slow-attempt breaker. Suppression
   creates no map entry.
4. Reserve the function state, compile once, and account the complete attempt.
   The existing `cache` becomes the single 192-entry
   `FunctionEntryState::{Ready, TerminalFailure}` map. A recoverable failure
   becomes a terminal state so the same exact variant cannot retry; there is no
   second failure map.
5. Cache and invoke a successful artifact only when the backend remains inside
   its retained-payload contract.

For loop OSR:

1. Look up an already retained exact key before breaker checks.
2. Preflight global and loop breakers plus the 64-state capacity before
   `plan_loop_region`. This closes the current path where infinitely many
   unseen loop keys can repeatedly pay bounded planner work after saturation.
3. Retain one state/plan for an admitted key, compile at most once at its
   threshold, and account the result in both global and OSR counters.
4. Continue using a ready region after a breaker closes; never create state,
   plan, or cache entries for a suppressed unseen key.

The payload and time values are necessarily post-attempt checks: Cranelift
reveals exact code size and elapsed time only after doing the bounded attempt.
The 1,024/128 instruction ceilings bound the input to that unavoidable final
attempt. If a completed artifact would take global payload above 8 MiB, or
loop payload above 1 MiB, it is never entered or exposed as ready.
`JitBackendHealth::RetiringResourceOverrun` makes all subsequent lookup/
invocation paths refuse it; `Vm::run_with_jit_backend` returns a distinct
`RetireAndInterpret` scheduler outcome; and outer `Vm::run` drops the local
backend before invoking `run_interpreter` from the current proven VM state.
Thus an infinite interpreter continuation cannot retain the overrun module.
The counter calls this a finalized-module overrun, not publication, because
the artifact was never published to the ready cache.

A compile that crosses 10 ms or makes cumulative observed compile time cross
100 ms may retain its otherwise valid payload-in-budget artifact, but closes
all later unseen compilation. These are non-refilling circuit-breaker
thresholds, not elapsed-time limits: synchronous Cranelift cannot be cancelled,
so either value may overshoot by the final statically bounded attempt. A true
wall-time ceiling requires an interruptible/out-of-process compiler and is not
claimed by this synchronous baseline.

## Counters and failure suppression

Add fixed source-free backend counters, mirrored into diagnostics without
adding detailed records:

- function capacity suppressions;
- global code-byte suppressions;
- cumulative-time suppressions;
- slow-attempt suppressions;
- oversized-function admission denials;
- terminal failed-entry hits;
- call-target observations dropped at capacity;
- backend retirements after a finalized-module payload overrun.

The counters describe engine policy only. They contain no key, code ID,
source, URL, property, object, pointer, or realm address. Counter increments
must not allocate. Detailed compilation diagnostics continue to describe only
attempts that actually occurred.

## Raw-emission escape hatches

`JitBackend::compile_codeblock` and `JitBackend::compile_ctx_thunk` currently
allow callers to emit unaccounted functions outside the runtime cache. Both
are used only by in-module tests. D1 makes them test-only/private and keeps
production `declare_function`/`define_function` paths behind the governed
function, shim, or loop producers. The low-level differential harness may
exercise emission in tests, but it is not a production embedding API and
cannot be used by page JavaScript.

The legacy `call_targets` feedback map is populated only by the test override
while production admission denies call-containing entries. It is nevertheless
bounded at 1,024 sites so future relaxation cannot silently expose an old
unbounded map.

## Acceptance matrix

The implementation does not pass D1 until all of these are reproducible:

1. Retain 192 exact function variants. A 193rd unseen variant compiles nothing,
   allocates no cache state, and falls back; the first ready pointer is reused.
2. In both fill orders, 192 function states and 64 loop states coexist. The
   65th loop key changes no state/plan/artifact map.
3. Distinct budgeted/diagnostic PC-zero variants and I32/F64 × budgeted ×
   diagnostic loop variants never alias and each consumes one exact key.
4. Inject accounting immediately below and at 8 MiB, 10 ms, and 100 ms. New
   work is suppressed while ready entries still run. A global or loop payload
   overrun retires and drops the backend before the artifact can run or the
   interpreter can continue. Time tests assert one-attempt overshoot semantics
   rather than a false elapsed-time ceiling.
5. A 1,025-instruction otherwise-supported function is denied before artifact
   creation with the distinct source-free reason and no compile-time charge.
6. A recoverable failed entry is attempted once; subsequent exact-key hits do
   not retry. Suppressed unseen keys create no negative-cache growth.
7. Once loop capacity or a breaker is closed, a new loop performs no planner or
   compiler work. Existing ready loop entries remain reusable.
8. At 1,024 legacy call targets, unseen sites do not grow the map or alter an
   existing site.
9. At requested diagnostic maximums, every vector and index remains inside the
   numerical bounds above and serialized output remains source-free. Because
   vector/hash capacity and snapshot clones are not byte-exact, the maximum-
   diagnostics fixture is also included in the RSS process gate; this is a
   cardinality bound plus empirical memory gate, not an 8 MiB code claim.
10. Production builds expose no raw unaccounted emitter. Backend drop frees all
    owned executable code; cache keys do not cross context/backend generations.
11. Focused JIT tests, the full feature/no-feature engine matrix, formatting,
    and strict affected-target Clippy pass without a new finding.
12. On macOS use `/usr/bin/time -l`'s maximum-resident-set field; on Linux D4
    repeats with `/usr/bin/time -v`. Run seven fresh-process, order-alternating
    interpreter/JIT pairs for the no-artifact control, the 192-function/64-loop
    saturation fixture, and that fixture with all diagnostic classes requested
    at 4,096. Record raw peak RSS, compile time, accounted payload, cache reuse,
    binary/fixture hashes, OS, and allocator. D1 fails if the no-artifact JIT
    median regresses by more than 5%, or if any artifact/maximum-diagnostic pair
    has a JIT-minus-interpreter peak-RSS delta above 64 MiB. This is an empirical
    process rollback bound, not an executable-allocation claim; exceeding it
    reopens the design rather than silently widening the gate.

## Review outcome and implementation slices

The independent whole-backend audit found the unbounded PC-zero cache, missing
body ceiling, legacy call-target map, raw emitter escape hatches, and planner-
before-capacity ordering. This record incorporates those findings and tightens
the proposed slow-attempt breaker so it applies to function compilation as
well as loop compilation.

D1 is one behavior slice with two separately revertible implementation commits;
the gate closes only when both are present:

1. add the static body ceiling, exact map capacities, pre-planner checks,
   terminal suppression, counters, and private raw-test seam;
2. unify byte/time accounting, finalized-module-overrun retirement, and context
   fallback, then run the RSS/cold-start gate.

One further behavior slice may follow the complete D1 slice. The already
scheduled behavior-neutral refactor is then due before any second ABI or
default flip.

Implementation status and the remaining acceptance gaps are recorded in the
[resource-governor checkpoint](32-default-jit-resource-governor-checkpoint-2026-08-03.md).
