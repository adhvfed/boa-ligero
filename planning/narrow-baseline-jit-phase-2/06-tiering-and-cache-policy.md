# Tiering and cache policy

Phase 2 adds more entry kinds and more assumptions. Without admission and
lifetime policy, the runtime can spend more time compiling or retaining code
than it saves executing it.

This is cross-cutting work, not a final cleanup slice. Before adding nonzero-PC
entries, establish explicit entry kinds, bounded diagnostic cardinality,
duplicate/failure suppression, and a conservative cache count/byte guard. The
final slice tunes policy from workload data; it must not be the first time a
long-lived browser context receives a bound.

## Entry kinds and keys

Keep one runtime-owned cache, but distinguish entry kinds explicitly:

```text
EntryKind::FunctionEntry
EntryKind::LoopOsr { header_pc }
EntryKind::CompiledCall { call_site_pc, target_code_id }
```

A key must include realm/runtime identity, code-block identity, entry PC/kind,
bytecode/ABI version, and the feedback/representation signature that generated
the assumptions. `debug_id` is useful for diagnostics but is not by itself a
cross-context ownership contract.

Each entry records native coverage, code size, compile time, assumptions,
entry/exit counters, and a reason it was rejected or evicted. The backend owns
the machine-code lifetime; no pointer survives backend teardown.

## Admission policy

Compile only when there is evidence that the entry can amortize compilation:

- function entries or loop backedges cross configured thresholds;
- the code block contains a native region with enough estimated coverage;
- the region is not known to be shim-only or dominated by unsupported calls;
- compile budget and code-cache budget permit the request;
- a prior variant has not repeatedly deoptimized for the same reason.

Do not lower the threshold merely to make a microbench show native entries.
Use the cold/warm runner and the browser workload to find the crossover.

The 2026-08-03 Gate P matrix makes this a prerequisite for further coverage,
not final tuning. Three tiny helper bodies compiled natively and returned tens
of millions of times, yet the flat-call, monomorphic-property, polymorphic-
property, and method controls were slower with normal JIT. Before adding a
receiver or binding lowering that can expose more such entries, establish a
measured coverage/size admission floor or another equally explicit suppression
rule. Validate it against both the positive W0 browser kernel and these negative
controls; do not infer a threshold from one instruction count.

The subsequent seven-point crossover did find a static boundary: 33 native
instructions were only at break-even while 45 clearly won, and W0's smaller
24-instruction body contains a useful loop. However, a loop-or-45 prototype
that emitted no artifact for rejected code made the losing controls slower.
The context-owned tier still wrapped every interpreted opcode with scheduler
bookkeeping. That prototype was reverted and is recorded in
[the dated admission checkpoint](10-admission-crossover-2026-08-03.md).

That prerequisite is complete: `fcfc2659` moves tiering decisions to frame
boundaries and `f0eeef75` lands the measured loop-or-45 rule. Rejected controls
are within the 5% JIT-disabled parity guardrail and profitable straight-line
entries plus W0 retain their wins. `612c7dc6` subsequently consolidates the
dormant interpreter path without changing that result.

The binding prototype exposed a narrower admission hole. A body with a
backward branch is currently admitted even when its static profile contains
calls, but generated callers cannot resume natively after a call. Production
admission must therefore deny call-containing function entries, emit no shim
or native artifact, and report `denied_call_boundary`. Keep a test-only
override for call-lowering semantics. This temporary rule is relaxed only by
the reviewed compiled-call ABI and Gate K, not by adding lowering for
instructions before the call.

## Thresholds and hysteresis

Keep function-entry, loop-backedge, OSR, and call-target thresholds separate.
Expose diagnostic overrides for tests, but use production defaults that are
stable and documented. Avoid immediate recompile loops:

- suppress a variant after repeated identical guard failures;
- require new feedback before compiling a replacement variant;
- do not compile a shim fallback repeatedly for the same code block;
- keep a failed compilation from being retried on every entry.

The first version can use a small bounded cache and no eviction if measurements
show acceptable memory growth. Add an explicit size/count limit before enabling
the tier for long-lived browser sessions.

## Cold-start policy

Report and optimize these separately:

```text
cold page/script time = setup + interpretation + compilation + execution
warm repeated task    = installed native execution + fallback transitions
```

Possible policy controls, to be selected by measurement:

- delay JIT until a region's estimated savings exceed compile cost;
- prefer OSR for long one-shot loops and function entries for repeated calls;
- avoid compiling code blocks whose native coverage is below a minimum;
- keep JIT opt-in while workload variance is high.

Do not add asynchronous compilation until the synchronous ABI and lifetime
rules are stable; it introduces thread-safety, realm ownership, and code
publication complexity without proving a throughput win.

## Invalidations and variants

Prefer guard misses and lazy replacement to global invalidation. A variant may
be retired when:

- its bytecode/ABI version changes;
- its feedback signature is stale;
- repeated misses show the assumption is not stable;
- the cache budget requires eviction.

Retirement must keep existing native pointers valid until no current call can
return through them. The simplest first policy is backend-lifetime retention
with bounded entry count, followed by explicit safe-point eviction later.

## Tests and gate

Test cache identity across realms and contexts, duplicate compilation
suppression, rejected-code suppression, variant replacement, backend drop,
threshold overrides, cache limits, and cold-start accounting. The policy gate
is met when the selected threshold wins on the complete workload, not only on
the hottest inner loop.

The existing `(code debug ID, budget mode)` key is sufficient only for Phase 1
PC-zero entries. The first Phase 2 artifact with a different entry PC or
assumption signature must replace that implicit key with a typed runtime-owned
key and demonstrate that bounded and unbounded budget variants cannot alias.
