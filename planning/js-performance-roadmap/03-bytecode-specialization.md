# 03 — Type specialization / quickening (lever #2)

**Why:** This is the single most on-target technique for a no-JIT engine in the
literature, and it's broad — it touches arithmetic, comparisons, name access,
and element access, i.e. the inner loop of nearly every program. It directly
addresses `global-counter` (4.87×), `array-numeric-sum` (4.41×), and the
arithmetic benches.

## Current state (grounded)

Boa already has _value-level_ fast paths but **no per-site type feedback**.
`add_fast`/`sub_fast`/`mul_fast` and the comparison `*_fast` helpers
(`core/engine/src/value/operations.rs:693` onward) check "are both operands
i32?" and fall through to f64, then to the fully generic coercion. The binary-op
opcodes are macro-generated over these (`vm/opcode/binary_ops/macro_defined.rs`).

The cost: **every execution re-runs the type dispatch**. A loop adding two
integers a million times performs the "is it an i32? is it an f64? is it a
string? …" decision a million times, even though the answer never changes.

## What the literature says

- **Quickening** — Brunthaler, "Efficient Interpretation using Quickening" (DLS
  2010): after first execution, rewrite a generic bytecode _in place_ to a
  type-specialized variant. Pure interpretation, no native codegen.
- **Inline caching meets quickening** — Brunthaler (ECOOP 2010): cache type info
  in the rewritten bytecode; reported up to ~1.71× on CPython.
- **CPython 3.11 specializing adaptive interpreter** — Shannon, PEP 659: hot
  bytecodes carry a counter and specialize into a _family_ (`BINARY_OP` →
  int/float/str variants; `LOAD_ATTR` → cached-shape variant), de-specializing on
  type change. ~half of CPython's ~25% 3.10→3.11 gain is attributed to this.

## The key structural insight (why this is safe, unlike Move-elision)

Type-specialization quickening **swaps one opcode for another same-width opcode
in place** — `Add` → `AddSMI`. The bytecode length never changes, so there is
**no byte removal and no jump-address remapping** — exactly the fragile,
all-or-nothing machinery that made Move-elision complex and that we just
removed. Quickening is strictly local: write a different opcode byte (and
optionally fill an inline cache slot) at one PC. That makes it both safer to
implement and cheaper to reason about.

## Plan

### 3a. Adaptive arithmetic opcodes

For `Add`/`Sub`/`Mul`/`Div`/`Mod` and the relational/equality ops:

1. Start each site as a generic `Add`.
2. The generic handler, after computing, records the observed operand types
   (or just increments a per-site counter on the i32/i32 path).
3. After a threshold of consistent observations, overwrite the opcode byte with
   a specialized variant (`AddSMI`, `AddF64`) that assumes the type, does the
   minimal op, and on a type-guard miss **de-specializes** back to the generic
   opcode (so it self-heals on polymorphic sites).

Specialized handlers skip the type-dispatch ladder entirely — for `AddSMI`,
two `as_integer32()` reads, a checked add, overflow → deopt to generic.

### 3b. Where to store the feedback

Two options, pick after a spike:

- **In-bytecode counter**: a small counter in an inline-data slot adjacent to the
  opcode (CPython's approach). No separate table; cache-local.
- **Side feedback vector** keyed by PC (V8 Ignition's approach), kept on the
  `CodeBlock`. Cleaner separation, one indirection.

Boa already maintains per-CodeBlock inline-cache storage for properties
(`vm/inline_cache/`); reuse that infrastructure's lifetime/GC discipline rather
than inventing a new one.

### 3c. Extend to name access and element access

`global-counter` (4.87×) is dominated by global name reads/writes;
`array-numeric-sum` (4.41×) by element reads. Specialize:

- name access to a cached-slot variant (some of this exists —
  `SetNameGlobal` already has an inline-cached write path, commit `5342515c`);
- element access to a "dense-array, in-bounds, i32 index" variant (see
  [04-inline-caches](04-inline-caches.md), which is the same idea applied to
  `GetByValue`/`SetByValue`).

## Expected ROI & validation

- Broad, because the specialized opcodes run in every hot loop. PEP 659's
  experience (~half of 25%) is the right order-of-magnitude prior for an
  interpreter-only engine.
- Metrics: `global-counter`, `array-numeric-sum`, `int-arith` (already 1.19× —
  guard against regressing it), `float-arith` (1.93×). Plus Octane `crypto`
  (integer-heavy) and `navier-stokes` (float-heavy) as real workloads.
- **Opportunity check first**: instrument the generic arithmetic handlers to
  count "would have specialized to SMI/F64" per site across Octane. If most hot
  sites are monomorphic (they will be), proceed.

## Risks

- **Self-healing is mandatory.** A site that specializes to SMI then sees a
  string must deopt cleanly, or you get wrong results — far worse than slow ones.
  Test polymorphic sites explicitly.
- **Don't thrash.** A site that flip-flops types must settle to generic, not
  re-specialize every execution. Use a hysteresis counter.
- **JS arithmetic is not integer arithmetic.** `+` on objects calls
  `valueOf`/`toString`; `==` coerces. The guard must exclude every case the fast
  path doesn't handle, including `-0`, `NaN`, and SMI overflow to f64.
