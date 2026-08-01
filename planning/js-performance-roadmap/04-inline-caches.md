# 04 — Inline-cache expansion (lever #3)

Boa has good property ICs already; this lever closes the gaps around them.

## Current state (grounded)

- **Property get/set by name**: 4-way polymorphic inline cache
  (`core/engine/src/vm/inline_cache/mod.rs`, `PIC_CAPACITY = 4`). Each entry holds
  a shape address + a `WeakShape` liveness guard + a cached `Slot`. Hot hit path
  does an address-equality + liveness check with no `Gc` refcount traffic
  (`vm/opcode/get/property.rs:77`, `set/property.rs:73`). 5th distinct shape →
  megamorphic, caching stops. This is solid, modern IC design.
- **Element access** (`obj[i]`, `GetByValue`/`SetByValue`): has a monomorphic,
  shape-guarded `ElementIC`. Reads cover dense i32/f64/value storage and ordinary
  sparse data descriptors; a missing property or accessor deoptimizes to the
  generic internal-method path. Writes deliberately seed only dense storage.
  The cached storage discriminant is named `IndexedKind` because readonly Web IDL
  collection indices correctly live in sparse descriptor storage.
- **Calls**: no IC (covered in [02-call-path](02-call-path.md)).
- **Native array scans**: `Array.prototype.indexOf` has a related guarded fast
  path for contiguous own indexed data. It excludes proxies and exotic reads,
  resumes generic lookup at the first hole/accessor, and charges work to the
  runtime loop limit in bounded chunks (`30cacb9f`).

## Plan

### 4a. Element-access inline cache — shipped

`GetByValue`/`SetByValue` now have an IC analogous to the by-name PIC: guard on
receiver shape, cache the indexed-storage kind, and on hit read the backing
storage directly. Ordinary readonly sparse data participates in the read path;
prototype-chain hits, proxies, strings, holes, and accessors fall back. Tests pin
shape changes, out-of-bounds reads, sparse arrays, ordinary non-array receivers,
and replacement of a cached sparse data property by an accessor.

The next bounded refactor is measurement, not more receiver classes: add hit,
miss, and storage-kind counters to the existing benchmark harness, then decide
from real workloads whether monomorphic last-write-wins remains sufficient or
whether element sites need the by-name PIC's small polymorphic table. Keep the
feedback reusable by [03](03-bytecode-specialization.md) and the JIT tier.

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
- Metrics: `array-numeric-sum`, plus Octane raytrace/navier-stokes (array+float),
  `readonly-indexed-scan`, and Ligero's six static-NodeList tampering WPTs.
- Opportunity check: count element-access sites in Octane that are monomorphic
  by observed storage kind; do not assume dense arrays dominate before the
  counters exist.
- Current Web IDL result (2026-08-01): the dedicated release microbenchmark runs
  5,000 `indexOf` scans of 100 readonly entries in 3.72 ms. Ligero's three
  `indexOf` NodeList WPTs pass in 2.51–2.55 s. The three manual-loop variants
  reach the unchanged 200-million-instruction budget in about 1.20 s; their
  remaining problem is bytecode volume/hot-loop tiering, not indexed-property
  dispatch. Do not raise the limit to turn this performance gap green.

## Risks

- IC invalidation correctness: shape changes, array→dictionary transitions,
  `__proto__` reassignment, frozen/sealed objects. The existing `WeakShape`
  liveness discipline (`inline_cache/mod.rs`) is the model to follow — don't
  invent a weaker guard.
- Keep entries small; ICs that bloat the per-CodeBlock side tables hurt the very
  cache locality they're meant to improve.
