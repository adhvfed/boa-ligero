# Guarded storage and feedback

This is deliberately after coverage, OSR, and compiled calls. Phase 1's
helper-backed property and dense-element paths are safe starting points; Phase
2 should only remove helper cost after their guard hit rate and workload impact
are known.

## Feedback snapshot

Generated code must consume an immutable snapshot captured for the compiled
entry, not read mutable IC internals opportunistically. A snapshot may contain:

```text
realm/runtime identity
receiver shape identity + liveness token
property key / slot / attributes
prototype/accessor exclusion facts
element storage kind / bounds policy / hole policy
feedback version and ABI version
```

The snapshot is metadata owned by the JIT runtime. It is not a new GC root set
and must not retain an object or shape in an untraced raw pointer.

## Helper-to-IR decision gate

Measure the current helper path first:

- guard-only cost;
- checked load cost;
- boxing/materialization cost;
- deopt/miss cost;
- GC/shape-liveness behavior.

If the helper path is already below the workload budget, keep it. Direct loads
are justified only when they remove a measured hot cost without making guard
or invalidation behavior fragile.

Helper attribution must not perturb the path it is intended to measure. Count
native named/dense guard hits, misses, and successful loads only in a distinct
diagnostic artifact/cache variant. The ordinary production artifact and its
helper ABI receive no diagnostic counter update. Interpreted-site telemetry is
separately bounded and records only coarse operation kind plus existing cache
state before execution; it must not perform conversion or invoke user code.

## Dense element direct load

The first direct dense load must prove:

- receiver and element storage kind match the snapshot;
- key is an in-range non-negative integer;
- the element is present and does not require prototype/getter lookup;
- bounds and storage layout are valid for the target platform;
- shape/element liveness cannot be invalidated by a GC or allocation between
  guard and load;
- the value is materialized before any subsequent helper or safepoint.

Holes, sparse properties, out-of-bounds reads, prototype changes, accessors,
typed-array edge cases, and storage-kind changes take the interpreter/helper
path. A failed guard must occur before a visible mutation; reads should not
silently turn a hole into `undefined` unless the complete ECMAScript lookup
semantics are proven.

## Named property direct load

Start with an own data property whose slot and shape are stable. The direct
path must exclude:

- accessors and proxy behavior;
- prototype traversal or prototype mutation;
- megamorphic sites;
- dictionary/layout transitions not represented by the snapshot;
- a shape pointer whose weak liveness cannot be checked safely.

The direct path can return a boxed value to the VM stack first. Unbox to `I32`
or `F64` only after the existing representation guard succeeds. Do not hold a
raw property-map pointer across a helper, allocation, or GC safepoint.

## Binding and cache invalidation

Shape and binding assumptions should initially be guard-and-deopt, not eager
code invalidation. If a feedback version changes, the next guard miss returns
to the interpreter and the runtime may compile a new variant. Add dependency
invalidation only if profiles show repeated misses make guard-and-deopt
materially worse.

## Tests and gate

Add positive and negative tests for dense arrays, holes, sparse arrays,
prototype mutation, accessors, GC, shape transitions, dictionary objects, and
megamorphic sites. Compare helper and direct forms on matching and mismatching
workloads. The direct form must win on the matching browser-shaped property
path without a meaningful regression on misses.
