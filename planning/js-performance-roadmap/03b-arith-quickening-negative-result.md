# 03b — Adaptive-arithmetic quickening: NEGATIVE RESULT (do not retry as-is)

Follow-up to `03a-arith-opportunity-findings.md` (which gave a **GO** based on the
_opportunity_). The lever was implemented in full and **measured an 8% regression**.
The opportunity data was correct; the **mechanism does not fit Boa's interpreter**.
Recorded so this isn't re-attempted blindly (cf. the reverted move-elision pass).

## What was built (branch `perf/arith-quickening`, NOT merged)

PEP-659-style in-place opcode quickening for `Add`/`Sub`/`Mul`, both `Int` and
`F64` forms, with deopt:

- `Bytecode.bytes`: `Box<[u8]>` → `Box<[Cell<u8>]>` for interior-mutable in-place
  opcode rewriting; 6 new opcodes (`AddInt/AddF64/SubInt/SubF64/MulInt/MulF64`) in
  formerly-Reserved slots.
- Parallel `quicken_state: Box<[Cell<u8>]>` per-site warm-up counters on `CodeBlock`.
- Specialized handlers with deopt-to-generic on type-mismatch / i32 overflow.
- 12 correctness tests (deopt on mismatch/overflow, `-0`, string-concat, independent
  sites). All 1108 engine tests pass; CI (fmt + clippy all/no-features, `-D warnings`)
  green. **Correctness was fully achieved — the problem is purely performance.**

## A/B measurement — REGRESSION (RUNS=100, microbenches, baseline = main @ 04b1298b)

| benchmark         | baseline | quickened |          speedup |
| ----------------- | -------: | --------: | ---------------: |
| int-arith         |  149.6ms |   175.9ms |       **0.850×** |
| float-arith       |   44.1ms |    47.3ms |           0.932× |
| global-counter    |  137.9ms |   139.0ms |           0.992× |
| array-numeric-sum |  114.2ms |   124.7ms |           0.915× |
| **geomean**       |          |           | **0.921× (−8%)** |

## Why it regressed (root cause)

1. **The fast path is already inlined.** Boa's `add_fast`/`sub_fast`/`mul_fast` are
   `#[inline]` and the compiler folds them into the generic handler. The specialized
   `AddInt` does the _same_ NaN-box tag checks — no dispatch work is saved. The
   type-dispatch ladder PEP-659 targets was already eliminated at compile time.
2. **Quickening swaps the opcode but not the dispatch.** Both `Add` and `AddInt` go
   through the same `OPCODE_HANDLERS[op as usize](...)` indirect call. That indirect
   call — not type dispatch — is the residual per-op cost, and quickening leaves it
   untouched.
3. **Warm-up overhead.** The counter logic (load `quicken_state[pc]`, compare, match
   operand variant, increment, conditional write) runs on every fast-path hit before
   the threshold and adds a cache-line dependency the baseline doesn't have. On tight
   inner loops this is net-negative.

## Where the real arithmetic-site win actually is

The hot sites from `03a` are real — the bottleneck is just elsewhere than type
dispatch:

- **Dispatch itself** (`05-dispatch.md`): direct/threaded dispatch to kill the
  indirect `OPCODE_HANDLERS` call. Uncertain whether Rust/LLVM will emit
  computed-goto-quality code; investigate before committing.
- **JIT (`08`/`09`)**: native codegen for hot loops removes both dispatch _and_ the
  per-op tag checks. This is the roadmap's "where the multiples are" — and the
  measured wash here is evidence that _interpreter-level_ arithmetic levers have
  hit diminishing returns. JIT Stage 2 is the next real lever.

## Reusable groundwork on the branch

The `Box<[Cell<u8>]>` bytecode + in-place opcode-rewrite API and the per-site
`quicken_state` array are sound and could be reused by any future site-specialization
or dispatch experiment — `perf/arith-quickening` is worth keeping as a reference even
though it must not merge.
