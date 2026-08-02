# Tiering and cache policy

Phase 2 adds more entry kinds and more assumptions. Without admission and
lifetime policy, the runtime can spend more time compiling or retaining code
than it saves executing it.

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
