# 06 — GC & allocation (lever #4)

High potential, but high effort and high risk (write barriers touch every
mutation path), so it sits mid-roadmap. Two distinct sub-problems: _how much we
allocate_ (cheap wins) and _how we collect_ (expensive, structural).

## Current state (grounded)

`boa_gc` (`core/gc/src/lib.rs`):

- **Mark-sweep, stop-the-world, non-generational.** Four phases: mark → finalize
  → mark again (resurrection) → sweep (`lib.rs:228`+).
- **Threshold-triggered** at 1 MB default (`lib.rs:68`), growing dynamically when
  > 70% live after a collection.
- **No write barriers.** Allocation is `Box::new` recorded in a `strongs` vector
  (`alloc_gc`, `lib.rs:131`); tracking is purely via `Trace::trace` at mark time.
- Thread-local (`BOA_GC: RefCell<BoaGc>`).

Per-operation allocation pressure: object literals allocate a
`Gc<VTableObject<…>>` + property storage; **function calls allocate a
`Gc<Environment>`** (and capture frames for closures); strings allocate eagerly
(see [07-strings](07-strings.md)). Numbers do **not** allocate (NaN-boxed).

## What the literature says

- **Generational hypothesis**: most objects die young. A nursery + promotion lets
  the common case (short-lived temporaries) be collected by a cheap scavenge that
  never touches the old generation. V8 Orinoco is the reference design.
- **Write barrier** maintains an old→new remembered set so a young-gen collection
  doesn't have to trace the whole heap. This is the enabling mechanism for
  generational GC — and the main implementation cost (every reference store into
  a heap object must run the barrier).
- **Allocation-site pretenuring**: objects from sites known to produce
  long-lived objects are allocated directly in old space.

## Plan — cheap wins first

### 6a. Cut per-call allocation (cheap, do first)

Calls are the hot path ([02](02-call-path.md)); each allocating a
`Gc<Environment>` is allocation pressure on the most frequent operation.
Investigate environment reuse / stack allocation for environments that don't
escape (no closure captures them). This compounds with the call-path work and is
far cheaper than touching the collector.

### 6b. Allocation accounting (measurement, do before 6c)

Add a per-site / per-kind allocation counter to `alloc_gc` (`lib.rs:131`),
gated by env var. Run Octane. This tells you _where_ the bytes come from and
whether the GC is even on the critical path for our benchmarks before committing
to the (large) generational rewrite. Per
[01-measurement](01-measurement-methodology.md): size the lever first.

### 6c. Generational nursery + write barrier (structural, expensive)

If 6b shows allocation/collection is a real cost, add a young generation:
bump-pointer nursery, scavenge on nursery-full, promote survivors, write barrier
on stores into old-gen objects. This is the big structural win but it's a
multi-week change that touches every `Gc` store site, and a buggy write barrier
is a use-after-free. Stage it behind solid GC stress tests.

### 6d. Incremental / concurrent marking (latency, optional)

The current STW pause scales with live-set size. Incremental marking (process
the mark queue in slices between interpreter ticks) cuts pause time. This is a
_latency_ win, not throughput — relevant for the browser embedding (smooth
frames) more than for the throughput benchmarks. Prioritize per the embedding's
needs, not the microbenchmark geomean.

## Expected ROI & validation

- 6a is cheap and compounds with the call work — do it alongside lever #1.
- 6c is the high-ceiling item but only justified by 6b's numbers. Don't start the
  generational rewrite on faith.
- Metrics: allocation-heavy real workloads (splay is GC-stress by design;
  earley-boyer allocates heavily) and any object-creation benchmark (with a real
  sink — `object-create-literal` is DCE-suspect, so build a sound variant).

## Risks

- Write barriers are correctness-critical and pervasive; a missed barrier is a
  silent heap-corruption bug. Gate behind GC stress testing and consider a
  debug-mode verification pass.
- Don't let GC work be a detour from the call/specialization levers, which are
  higher-ROI and lower-risk. Allocation reduction (6a) gets most of the
  interpreter-tier benefit at a fraction of the risk.
