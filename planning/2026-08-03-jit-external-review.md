# External review — narrow-baseline JIT phase 2

Date: 2026-08-03. Reviewed at `7d5c5c74`. Independent read-only review of the
loop-OSR, resource-governor and admission work committed 2026-07-24 → 2026-08-03.

This document is deliberately kept **outside** `planning/narrow-baseline-jit-phase-2/`
so it does not collide with that series' numbering while remediation is in
flight.

## Verdict

The architecture is unusually disciplined for a from-scratch JIT. The
OSR-compiled loop region is a tiny, provably-closed numeric subset — no
property, call, environment or allocating opcode can appear inside it — so the
classic JIT dragons (shape invalidation, `eval`/`with`, global mutation,
exceptions, GC across compiled frames) are excluded **by construction** rather
than by enumeration. Every entry is re-guarded against a full frame identity
tuple, every exit PC is revalidated against immutable per-artifact metadata, and
inconsistent metadata fails closed into an uncatchable engine error that
permanently retires the backend.

The gates process is **not** verification theatre. Gates get refused rather than
closed; measurements are labelled non-binding when the host failed quiescence
preflight; negative results are recorded; an independent audit's findings were
incorporated with attribution.

Two things are wrong, one of them badly:

1. **A critical deopt-state-fidelity bug** that returns wrong values to running
   JavaScript.
2. **The resource governor cannot reclaim memory**, and an explicit D1
   acceptance criterion asserting that it can is false.

## Strengths (worth preserving through any remediation)

1. **Region eligibility is a whitelist, not a blacklist** — `loop_use_def`
   (`core/engine/src/jit/native.rs:546-589`) accepts only
   `Store{Zero,One,Int8,Int16,Int32,Float,Double}`, `Move`, `Inc`,
   `Add/Sub/Mul`, five `JumpIfNot*` comparisons, `IncrementLoopIteration`,
   `Jump`. Everything else is `UnsupportedRegionOpcode`.
2. **Continuation is proven, not guessed** — `continuation_live_in`
   (`native.rs:602-655`) accepts only a ≤16-instruction return epilogue.
3. **Entry guard covers the full identity tuple** — `jit_loop_entry_guard`
   (`native.rs:3416-3444`) checks backend id, `code_block.debug_id`,
   `frame.pc == header_pc`, `!construct()`, budget-mode parity, register-count
   and range. `debug_id` is a monotonic thread-local counter
   (`core/engine/src/vm/code_block.rs:483-491`), so there is no ABA hazard from
   freed CodeBlocks.
4. **Exit statuses are validated, not trusted** — `validate_loop_exit`
   (`core/engine/src/jit/mod.rs:2088-2129`) requires kind/reason/pc agreement,
   `frame.pc` agreement, and membership in cached `instruction_pcs`; anything
   else marks the backend compromised (`mod.rs:1536-1539`) and raises an
   uncatchable `PanicError` (`vm/mod.rs:1345-1359`).
5. **Compile is revalidated against the live CodeBlock** — `compile_loop_region`
   re-runs the planner and compares the whole plan for equality
   (`native.rs:480-493`).
6. **NaN and −0 semantics handled deliberately** — float branches use
   unordered-inclusive conditions (`native.rs:3126-3180`); i32 `Mul` deopts when
   the exact product would be −0 (`native.rs:3090-3097`); i32 `Add`/`Sub`/`Inc`
   emit sign-based overflow guards that refund the budget charge before replay
   (`native.rs:3239-3271`).
7. **Instruction-budget ownership is exact and exhaustively swept** —
   `mod.rs:5242` and `mod.rs:5322` compare JIT vs interpreter at *every* budget
   value across *every* region PC. Genuine differential testing on a risky path.
8. **Malformed-status containment is exhaustively enumerated** — `mod.rs:6221`
   (14 classes), `mod.rs:6115`.
9. **Production admission is conservative** — `admission_allow_call_boundaries:
   false` (`mod.rs:1451`), `MIN_STRAIGHT_LINE_INSTRUCTIONS = 45`
   (`mod.rs:1419`), per-frame OSR is one-shot (`vm/mod.rs:1328-1332`).
10. **The gate script is real** — `tools/bench-compare/jit-resource-gate.sh`
    records commit, binary SHA-256, fixture hashes, OS, CPU, allocator;
    alternates interpreter/JIT order per pair; emits a machine-checkable
    `summary.txt`.

## Dragon 1 — CRITICAL: loop-OSR deopt zeroes every undefined VM register

`LoopRegionCompiler::compile` declares a Cranelift variable for **every**
register in the frame:

```rust
// core/engine/src/jit/native.rs:2695-2697
self.variables = (0..self.code.register_count)
    .map(|_| bcx.declare_var(self.mode.value_type()))
    .collect();
```

`emit_available_materialization` then walks all of them, assuming `try_use_var`
fails for a value with no definition on the current path:

```rust
// core/engine/src/jit/native.rs:3273-3285
for (register, variable) in self.variables.iter().enumerate() {
    let Ok(value) = bcx.try_use_var(*variable) else { continue };
    self.emit_store(bcx, ctx, helpers, register, value);
}
```

**That assumption is false.** `try_use_var` returns `Err` only for an
*undeclared* variable (cranelift-frontend-0.130.2 `src/frontend.rs:449-474`, and
its own unit test at `frontend.rs:1895-1921`). A declared-but-never-defined
variable is silently materialised as zero:

```
// cranelift-frontend-0.130.2/src/ssa.rs:533-546
// "The variable is used but never defined before. ... rather than throwing an
//  error we silently initialize the variable to 0. This will have no effect
//  since this situation happens in unreachable code."
```

The comment's premise does not hold here. `emit_available_materialization` is
reached from three **live** exits: integer-overflow deopt (`native.rs:3259`),
instruction-budget exhaustion (`native.rs:2992`), and loop-iteration-limit break
(`native.rs:3196`). `emit_store` writes through `jit_store_i32`/`jit_store_f64` →
`JsValue::new(0)` into `Vm::set_register` (`native.rs:3991-4007`).

Observable via the overflow-deopt path (the other two raise uncatchable
`EngineError`s, so they are latent today):

```js
function f(limit, tag) {
  let total = 0;
  for (let i = 0; i < limit; i++) { total = total + i; }
  return tag;
}
```

`tag` is in `exit_live` and correctly classified `PreservedVmValue` at the clean
exit (`native.rs:404-425`) — that path is fine and is what
`jit_loop_planner_preserves_untouched_exit_values_in_vm_registers`
(`mod.rs:5653`) tests. But `tag` is never *defined* natively, so at an overflow
deopt its register is stored as integer `0`; the interpreter resumes mid-loop,
finishes, and returns `0` instead of the string.

This contradicts the reviewed ABI, which specified a per-PC map:

> "Before returning they materialize **the map for the current PC**, refund
> exactly the current native instruction only in budgeted mode…"
> — `planning/narrow-baseline-jit-phase-2/20-loop-osr-abi-review-2026-08-03.md:266-268`

The implementation checkpoint records the weaker behaviour without noticing the
SSA subtlety (`22-slice-4a1-region-compiler-checkpoint-2026-08-03.md:26`).

**Fix direction**: restrict the deopt store set to registers with a definition
that provably reaches that PC — the planner already computes `defs`,
`successors` and has `definitely_defined_before` (`native.rs:657-691`) — or
track a per-block dirty set instead of relying on Cranelift to reject undefined
uses.

> **Resolved 2026-08-03 in `67ae1673`.** `LoopRegionPlan` gained
> `available: Vec<Vec<u32>>`, computed in `plan_loop_region` as
> `live_in[i] ∩ (entry_registers ∪ defined)` per region instruction;
> `emit_available_materialization` emits exactly that set. The set is *exact*,
> not conservative: a register live at `i` but defined on only some paths is
> necessarily live at region entry, hence already in `entry_registers` with a
> guarded prologue load. Budget-refund ordering untouched.
>
> **The suggested direction did not transfer to the function tier.** A static
> must/may-reach analysis there caused loop-carried temporaries (may- but not
> must-defined) to stop compiling — the plain integer accumulator loop regressed
> and 21 JIT tests failed. Precision would require full liveness over the whole
> function-tier opcode set including `Call` and stack effects. The shipped fix
> instead carries a per-register definedness flag alongside the same control
> flow, consumed by `jit_store_{i32,f64}_if_defined`. Zero rejection, zero
> guessing, no measurable cost (`jit_loop_perf` ratio 0.101 vs 0.110 before).

## Dragon 2 — HIGH: the resource governor cannot reclaim memory

D1 acceptance item 10 asserts *"Backend drop frees all owned executable code"*
(`planning/narrow-baseline-jit-phase-2/31-default-jit-resource-bounds-design-2026-08-03.md:220-221`),
and the retirement design (`31:141-148`) depends on it:
`RetiringResourceOverrun` → `RetireAndInterpret` → `drop(backend)`
(`core/engine/src/vm/mod.rs:1487-1491`).

cranelift-jit deliberately leaks on `Drop`:

```
// cranelift-jit-0.130.2/src/memory/system.rs:221-227
impl Drop for Memory {
    fn drop(&mut self) {
        // leak memory to guarantee validity of function pointers
        mem::replace(&mut self.allocations, Vec::new()).into_iter().for_each(mem::forget);
    }
}
```

Reclamation requires the `unsafe fn free_memory` API
(cranelift-jit-0.130.2 `src/backend.rs:192-194`), which is **never called
anywhere in this repo** (verified by grep across `core/engine/src` and
`core/jit/src`). So `Context::disable_jit` (`core/engine/src/context/mod.rs:219`),
context teardown, and the payload-overrun retirement path all leak the emitted
pages permanently.

The governor bounds *how much code one backend generation will produce*
(≤8 MiB accounted `code_buffer` bytes); it does **not** bound process memory
across backend generations. The deferred seven-pair RSS gate (`31:224-233`) is
unlikely to catch it — an 8 MiB leak sits well under the 64 MiB per-pair
threshold.

Secondary accounting problems in the same area:

- On payload overrun the bytes are not added to `retained_code_bytes`
  (`mod.rs:1627-1632`), so the counter under-reports memory the module holds.
- A retiring backend records every loop suppression as `CodeBytes` regardless of
  cause (`mod.rs:1720-1727`), so a compilation-failure retirement is reported as
  a payload event.

## Dragon 3 — CRITICAL (upgraded): same false assumption, JS-observable

> **Correction, 2026-08-03.** This section originally rated the function tier
> MEDIUM-HIGH on the reasoning reproduced below — that `self.dirty` only holds
> registers defined somewhere and `use_register` rejects `RegisterKind::Boxed`,
> so the bug would be hard to construct. **That hedge is wrong.** Remediation
> found a JS-observable repro with native compilation confirmed:
>
> ```js
> function pick(subject, iterations, value) {
>   let width = subject.b;                                // marks subject's def Boxed
>   for (let i = 0; i < iterations; i++) { subject = 1; } // Numeric def, skipped when iterations = 0
>   let doubled = value + value;                          // overflow deopt
>   return subject;
> }
> ```
>
> With `iterations = 0` this returns `0` instead of the object. **Register reuse
> means `register_kind` reports `Numeric` at the exit, so the Boxed check does
> not protect it.** Severity is the same as Dragon 1. Fixed in `222d443e`; see
> `narrow-baseline-jit-phase-2/33-deopt-materialization-defect-2026-08-03.md`.
>
> The original reasoning is kept below because the way it failed is instructive:
> it reasoned about the *declared kind* of a definition without accounting for
> registers being reused across kinds within a frame.

```rust
// core/engine/src/jit/native.rs:2587-2593
// Dirty values are materialized before returning to the interpreter.
// `try_use_var` also validates that every value has a definition on
// this path; an invalid map rejects native compilation.
for register in &self.dirty {
    let Some(value) = self.use_register(bcx, *register) else { return false };
```

The stated validation does not exist. Exposure is narrower — `self.dirty` only
holds registers defined somewhere, and `use_register` rejects
`RegisterKind::Boxed` (`native.rs:2448-2459`) — so the failure mode is a wrong
numeric value rather than type confusion, and well-formed bytecode makes
"defined only on the not-taken branch and live at the deopt" hard to construct.
But the invariant the comment claims is load-bearing and untrue.

## Dragon 4 — MEDIUM: no broad differential validation with the JIT enabled

- `.github/workflows/test262.yml` and `test262_release.yml` contain no `jit`
  feature. **test262 never runs against the JIT.**
- No fuzz target enables the JIT.
- The only cross-engine differential is `assert_jit_matches_interp`
  (`mod.rs:7715-7733`), invoked on **four** hand-written snippets
  (`mod.rs:7736-7748`), and it uses `evaluate_jit`, which takes the *shim* path
  rather than the tiered loop-OSR path.

## Dragon 5 — MEDIUM: `Vm::set_register` is unchecked

`core/engine/src/vm/mod.rs:511-523` — `debug_assert` then
`get_unchecked_mut`. The defence is layered and adequate *today* (`loop_use_def`
bounds every operand by `register_count` at `native.rs:550-554`; the entry guard
verifies count and last-register existence at `native.rs:3429-3435`), but any
future widening of the region opcode set that forgets the bound check turns a
logic bug into UB. Worth a hard bound-check in the store helpers, since the JIT
is the only caller synthesising indices at runtime.

## Dragon 6 — LOW-MEDIUM: Integer/Rational representation drift on F64 exits

`jit_store_f64` always produces `JsValue::rational`
(`native.rs:4000-4007` → `value/conversions/mod.rs:53-58`), so a counter the
interpreter would keep as `Integer32(10)` returns as `Float64(10.0)`.
Observationally neutral in today's Boa (`to_property_key` on `Float64` round-trips
through `JsString` and re-parses to `Index` — `value/mod.rs:1127`,
`property/mod.rs:695-699`), but a real perf cliff after an OSR exit and a latent
divergence for anything switching on `JsVariant` or IC element kinds
(`native.rs:3671-3676`).

## Dragon 7 — LOW: trace fidelity, and a public API that skips admission

- Native loop iterations never reach `trace_execute_instruction`
  (`vm/mod.rs:898-903`), so `--features trace` under-reports iterations once a
  loop is OSR'd.
- `Script::evaluate_jit` is `pub` (`core/engine/src/script.rs:190-202`) and
  reaches `run_cached_entry` directly, bypassing `admit_function_entry`,
  hotness, and the scheduler-level `RetireAndInterpret` boundary. Resource-governed
  via `cached_entry` (`mod.rs:2638-2681`), so a policy gap rather than a safety
  gap — but it is a public API that skips the gates D1–D5 exist to enforce.

## Dragon 8 — INFORMATIONAL: the −0 deopt may hand off to a less-correct interpreter

`native.rs:3090-3097` deopts when `0 * -1` would need to produce `-0`. The replay
path, for two `Integer32` operands, uses `checked_mul` and produces
`Integer32(0)` — i.e. `+0`. If so the JIT is *more* spec-correct than its own
fallback and the guard is a no-op for observable behaviour. Worth confirming; if
the interpreter is wrong, that is a pre-existing engine bug the JIT surfaced.

## Test coverage

**Genuinely exercises risky paths**: budget/loop-limit differential *sweeps*
rather than samples (`mod.rs:5242,5322,5385`); fail-closed containment
(`mod.rs:6221,6115,6162,6073`); entry-guard strictness with pre-effect assertions
(`mod.rs:5761`); resource governor in both fill orders, 64+1 through the
production scheduler, diagnostic saturation, terminal source-free module
failures (`mod.rs:3464,6402,3541,3761,3826,3883,3923,3729,3955`); cache-variant
non-aliasing (`mod.rs:3520,4830`).

**Gaps, by risk:**

1. **No test asserts that a register untouched by the region survives a
   deopt/budget/loop-limit exit.** This is the single gap that hides Dragon 1.
   The two assertions of that shape (`mod.rs:5843,5858`) are on the
   `EntryRejected` path, which returns *before* any store executes — exactly the
   path where nothing could go wrong.
2. **No test262 or fuzz differential with the JIT enabled.** All engine-level
   semantic confidence rests on ~96 hand-written tests plus four snippets.
3. **The "forced GC" tests do not force GC across a compiled frame** —
   `mod.rs:4707` and `mod.rs:7319` call `boa_gc::force_collect()` *between*
   evaluations, never while native code holds state. Defensible for the loop tier
   (the region cannot allocate) but it does not test what its name claims, and it
   will not cover the function tier once `jit_call_ordinary` re-entry is admitted.
4. **No test that `JitBackend` drop releases memory** — acceptance item 10 is
   asserted in prose only, and is false (Dragon 2).
5. **`trace` and JIT are never tested together.**
6. **All OSR integration tests use the `Math.abs(limit);` prefix trick**
   (`mod.rs:4638,4716,4874,4910,4940`) to force function-level admission denial
   so the loop path is reached. The interaction between an *admitted* PC-zero
   native entry and loop OSR in the same frame is untested end-to-end.

## On the gates process

Not theatre, on in-repo evidence. Gates are refused rather than closed
(`32-*.md:5-6`); measurements are labelled with what they do *not* prove
(`31-*.md:35-37`, `32-*.md:72`); negative results are recorded
(`planning/narrow-baseline-jit-phase-2/README.md:12-15,63-64`); an independent
audit is incorporated with attribution (`31-*.md:235-241`); the gate is
automated as a hash-recording, order-alternating, machine-checkable script.

Two real weaknesses:

- **No raw gate output is committed.** The specific numbers quoted throughout the
  README (4.588×, +4.412% Earley-Boyer, 29.607% W0 median) are unauditable from
  the repo, so the process depends entirely on operator honesty.
- **At least one acceptance criterion is stated as fact without a test and is
  demonstrably false** (item 10, Dragon 2), which shows the acceptance matrix is
  not itself independently verified.

## Recommended sequence

1. Fix Dragon 1 with a failing test first (in flight — see the handoff).
2. Fix or correct Dragon 3's comment and enforce the real invariant.
3. Either call `free_memory` on retirement or **retract acceptance item 10** and
   restate the governor's guarantee honestly as "bounds code produced per
   generation, does not reclaim".
4. Add the missing untouched-register survival test to the standard suite.
5. Before any move toward default-on: test262 with `jit`, and a fuzz target that
   enables it.
6. Do this **after** the 2026-08-30 demo. See
   `../../ligero-browser/planning/2026-08-03-production-readiness-review/01-campaign-drift.md`.
