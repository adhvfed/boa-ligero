# 04 — Inline-cache expansion (lever #3)

Boa has good property ICs already; this lever closes the gaps around them.

## Current state (grounded)

- **Property get/set by name**: 4-way polymorphic inline cache
  (`core/engine/src/vm/inline_cache/mod.rs`, `PIC_CAPACITY = 4`). Each entry holds
  a shape address + a `WeakShape` liveness guard + a cached `Slot`. Hot hit path
  does an address-equality + liveness check with no `Gc` refcount traffic
  (`vm/opcode/get/property.rs:77`, `set/property.rs:73`). 5th distinct shape →
  megamorphic, caching stops. This is solid, modern IC design.
- **Element access** (`obj[i]`, `GetByValue`/`SetByValue`,
  `vm/opcode/get/property.rs:130`+): has _ad hoc_ fast paths for dense arrays and
  string length, but **no shape-guarded IC**.
- **Calls**: no IC (covered in [02-call-path](02-call-path.md)).

## Plan

### 4a. Element-access inline cache

Give `GetByValue`/`SetByValue` an IC analogous to the by-name PIC: guard on
(receiver shape, "is dense i32/f64-indexed array"), cache the storage kind, and
on hit go straight to the backing `Vec` with a bounds check. On miss (sparse,
prototype-chain hit, proxy, string), fall back. Targets `array-numeric-sum`
(4.41×) and array-heavy real workloads (raytrace, navier-stokes).

This overlaps with [03](03-bytecode-specialization.md)'s adaptive opcodes —
implement it as a specialized `GetByValueDenseSMIIndex` variant that deopts, so
the IC _is_ the specialization. Pick one framing and reuse the storage.

### 4b. Megamorphic handling

Today the 5th shape disables caching for the site. A global megamorphic shape
cache (keyed by shape→slot across sites) can still serve megamorphic sites
without per-site storage — V8 does this. Lower priority; only worth it if
profiling shows hot megamorphic property sites in real workloads (measure with a
"megamorphic transition" counter on the PIC).

### 4c. Prototype-chain caching

Method access (`obj.toString`) resolves up the prototype chain. Cache the
_holder_ (where the property was found) and the holder's shape, not just the
receiver's, so prototype-method loads hit. This is the substrate the
[call-path](02-call-path.md) fusion (2d) sits on for `obj.method()`.

## Expected ROI & validation

- Medium and well-bounded: the by-name PIC already proved the pattern works in
  Boa; this extends its surface.
- Metrics: `array-numeric-sum`, plus Octane raytrace/navier-stokes (array+float).
- Opportunity check: count element-access sites in Octane that are monomorphic
  dense-array — if most are (they will be), 4a pays.

## Risks

- IC invalidation correctness: shape changes, array→dictionary transitions,
  `__proto__` reassignment, frozen/sealed objects. The existing `WeakShape`
  liveness discipline (`inline_cache/mod.rs`) is the model to follow — don't
  invent a weaker guard.
- Keep entries small; ICs that bloat the per-CodeBlock side tables hurt the very
  cache locality they're meant to improve.
