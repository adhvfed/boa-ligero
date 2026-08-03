# Dragon 2 close-out: the JIT backend now reclaims its executable memory

Date: 2026-08-03
Scope: `planning/2026-08-03-jit-external-review.md` Dragon 2 only. Everything
else in the JIT stream is parked.

## Decision: outcome A — call `free_memory` on backend release

The review's Dragon 2 offered two exits: make reclamation sound and implement
it, or retract D1 acceptance item 10 as a false guarantee. The code supports
outcome A, and the argument is strong enough to write down rather than hedge.

The reclamation point is **`JitBackend`'s destructor**, not the retirement call
site. Retirement is only one of three ways a backend is released — the others
are `Context::disable_jit` and ordinary context teardown — and all three leaked
identically. A destructor covers all of them and makes reclamation an invariant
of the type rather than a discipline the callers have to remember.

## Diagnosis

`cranelift-jit` leaks by design:

```
// cranelift-jit-0.130.2/src/memory/system.rs:221-227
impl Drop for Memory {
    fn drop(&mut self) {
        // leak memory to guarantee validity of function pointers
        mem::replace(&mut self.allocations, Vec::new()).into_iter().for_each(mem::forget);
    }
}
```

Reclamation requires `unsafe fn free_memory(mut self)`
(`cranelift-jit-0.130.2/src/backend.rs:192-194`), which was called nowhere in
this repo. `Memory::free_memory` is just `self.allocations.clear()`, so the
`PtrLen` allocations unmap normally — the leak really is only the `mem::forget`
in `Drop`.

Consequence, exactly as the review stated: the governor's `MAX_RETAINED_CODE_BYTES`
ceiling bounded *how much code one backend generation emits*, not how much a
process retains across generations. Item 10 ("Backend drop frees all owned
executable code") was false as written, as was the `JitBackend` doc comment
claiming "dropping it frees the emitted code".

## The safety argument

`JITModule::free_memory` requires that no function from the module is executing
and that no pointer obtained from it is called afterwards. Four load-bearing
points establish both for every `JitBackend`. The full argument also lives as
the `// SAFETY:` comment at the call site in `core/engine/src/jit/mod.rs`.

**1. No module-owned address escapes the type.** `get_finalized_function`
results are stored in exactly two places, both private fields of `JitBackend`:
`cache` (`CachedEntry::entry`) and `loop_cache`
(`LoopCachedEntry::compiled.entry`). The destructor drops both. `JitBackend`'s
entire public surface is `new`, `stats`, `thresholds`, `set_thresholds`,
`enable_diagnostics`, `disable_diagnostics`, `diagnostic_snapshot`; none returns
a code address, and `JitStats` / `JitDiagnosticSnapshot` are counters and
`(pc, kind, reason)` exit records. Nothing durable holds a native address
either: `CodeBlock` carries admission/hotness state keyed by backend id;
`call_targets` maps `(debug_id, pc)` to a *bytecode* `debug_id`, not an address
(this was the review's "call-target feedback maps holding native pointers"
concern, and it does not apply); `vm.jit_pending` is a `CompletionRecord` and
`vm.jit_exit_pending` a `JitExit`.

**2. Compiled code contains no intra-module code pointers.** Every call the
lowering emits is an indirect call through an absolute constant naming a *Rust*
function — `JIT_OP_SHIMS[op_idx] as usize` and the runtime helpers in
`native.rs`. There is no `declare_func_in_func` and there are no data objects
anywhere in `jit/`. Each compiled body is therefore a leaf with respect to its
own module, so freeing the module cannot invalidate a reference held by other
generated code, and no other module can hold a reference into it. Freeing also
cannot affect the helper addresses, which live in the Rust binary.
`cranelift-jit` 0.130.2 performs no `__register_frame`-style process-global
registration (unwind tables are behind the unused `wasmtime-unwinder` feature),
so nothing outside the module retains a reference either.

**3. Nothing can be executing.** A compiled entry is invoked from exactly two
places, `invoke_cached_entry` and `invoke_loop_region`, both `&mut self`
methods. The exclusive borrow keeps the backend alive and undroppable for the
whole native call. This survives the awkward cases:

- *Re-entrancy.* Native code receives only `*mut Context`, and
  `Context::jit_backend` is `None` for the entire duration of a native call —
  `Context::run` moves the backend into a stack local before entering. So a
  shim that re-enters the interpreter, or an embedder host function that calls
  `disable_jit` or `enable_jit`, cannot observe or drop the executing backend.
  A nested `Context::run` finds `None` and runs the interpreter.
- *The public explicit path.* `Script::evaluate_jit` borrows a caller-owned
  backend `&mut`, and such a backend is unreachable from `Context` (the field
  is private), so it cannot alias the context-owned one.
- *Unwinding.* Whatever a panic through a Cranelift frame does otherwise — a
  pre-existing question, unchanged by this work — it pops the native frames
  before the frame that owns the backend, so the destructor still runs last.

**4. Deopts and OSR continuations do not resume into freed code.** Every exit
is an ordinary `return` of a `u64` status; the native frame is gone before the
Rust caller inspects it. The interpreter continues from `frame.pc`, and any
re-entry requires a fresh cache lookup on a live backend. There is no stored
resume address and no mid-flight exit that outlives the call.

### Scope limits, stated honestly

- Reclamation is **whole-module only**. A live backend never releases an
  individual artifact, because `free_memory` consumes the module. The payload
  ceilings therefore still bound one generation's emission; the destructor is
  what turns that into a bound on process memory.
- The accounting test is a **witness that the release path ran**, measured in
  the governor's own charged bytes. It is not a page-level measurement; the
  process cannot portably observe its own unmapped ranges, and RSS is far too
  coarse to attribute an 8 MiB ceiling to (the review makes the same point
  about the deferred seven-pair RSS gate).
- A latent hazard worth knowing about: the `#[cfg(test)]`
  `compiled_loop_entry_for_test` hands a `LoopCachedEntry` clone to tests,
  which then call `.compiled.entry` directly. Today every such test keeps
  `backend` in scope past the call (locals drop in reverse declaration order
  and `backend` is declared first), so all are sound. A future test that drops
  the backend early would now be a use-after-free rather than a leak. Left as
  is rather than widened, per the parked-stream rule.

## What changed

`core/engine/src/jit/mod.rs`:

- `JitBackend::module` is now `ManuallyDrop<JITModule>`. `free_memory` consumes
  the module by value, and `JITModule`'s own `Drop` is the leak being
  overridden, so the field cannot simply be dropped. `Deref`/`DerefMut` mean no
  call site in `jit/mod.rs` or `jit/native.rs` needed touching.
- Added `impl Drop for JitBackend`, carrying the safety argument above as a
  `// SAFETY:` comment, and calling
  `ManuallyDrop::take(&mut self.module).free_memory()` exactly once.
- Corrected the `JitBackend` doc comment, which previously asserted the
  behaviour this change actually implements.
- Added a `#[cfg(test)]` thread-local `RELEASED_CODE` witness `(bytes, modules)`
  incremented by the destructor. Thread-local rather than global so the
  parallel test harness cannot perturb a measurement; zero cost in production.
- Two new tests:
  - `jit_backend_drop_releases_accounted_executable_code` — a backend that
    compiled real code reports zero released while live, and exactly its
    accounted `retained_code_bytes` after drop.
  - `context_owned_jit_retirement_frees_executed_code_and_keeps_interpreting` —
    primes and *executes* a native entry for `add`, forces a payload overrun on
    the next compile, and confirms the backend retires, one module's accounted
    bytes are released, and the interpreter then keeps returning correct
    results for both `add` and `mul` across repeated calls. This is the
    retire-then-continue-interpreting case with genuinely freed pages
    underneath.

`planning/narrow-baseline-jit-phase-2/31-default-jit-resource-bounds-design-2026-08-03.md`:

- Amended the retirement design (the `RetireAndInterpret` paragraph) to record
  that dropping the backend was necessary but not sufficient before this work.
- Rewrote acceptance item 10 to name the enforcing mechanism, state the
  whole-module-only limit, cite the two tests, and flag that the item was
  asserted before it was true.

## Not fixed (still open from Dragon 2's secondary list)

Deliberately out of scope for this bounded item, and unchanged:

- On payload overrun the bytes are not added to `retained_code_bytes`
  (`jit/mod.rs`, `record_compilation_result`), so the counter under-reports
  what the module holds. Note this interacts with the new witness: the
  released figure is the *accounted* total, so an overrun backend reports
  slightly less released than it actually unmapped. Conservative in the right
  direction, but still an accounting inaccuracy.
- A retiring backend records every loop suppression as `CodeBytes` regardless
  of cause, so a compilation-failure retirement is reported as a payload event.

`core/jit/src/lib.rs` (the standalone spike crate) still leaks its own
`JITModule` and says so in its own doc comment. It is not the engine governor
and its documentation is not false, so it was left alone.

## Verification

All gates run in the foreground on this commit.

| Gate | Result |
| --- | --- |
| `cargo test -p boa_engine --features jit --lib` | **1,238 passed / 0 failed / 1 ignored** (baseline 1,236 + the 2 new tests) |
| ↳ `context_owned_jit_osr_matches_every_interpreter_instruction_budget` | passed |
| ↳ `context_owned_jit_osr_matches_interpreter_loop_limits` | passed |
| `cargo test -p boa_engine --lib` (no feature) | **1,138 passed / 0 failed** — matches baseline |
| `cargo clippy -p boa_engine --features jit --all-targets` | **20 warnings (lib)** — exactly the recorded baseline; lib-test 20 (18 duplicates), also unchanged. No warning location falls inside the changed line ranges. |
| `cargo fmt --check` | clean |

One clippy finding *was* introduced and fixed before commit: a missing-backticks
doc lint on the new test's comment.

### `jit_loop_perf` release ratio

The changed code cannot affect the execution path — the destructor runs at
teardown, `ManuallyDrop` is a transparent compile-time wrapper, and the witness
counter is `#[cfg(test)]` and only touched in the destructor. Measured anyway,
and the host was heavily loaded (load average 10–17), so single runs were
useless: unpaired ratios ranged 0.067–1.815.

Interleaved A/B against a binary built from the pristine tree, 7 alternating
pairs:

- before: 0.076, 0.112, 0.125, 0.091, 0.107, 0.080, 0.113 → median **0.107**, min 0.076
- after: 0.097, 0.112, 0.104, 0.074, 0.119, 0.265, 0.150 → median **0.112**, min 0.074

The distributions overlap completely and the single 0.265 is load noise. No
detectable change against the recorded 0.101 baseline, consistent with the
structural argument. A clean-machine re-measurement would be needed to
discriminate anything finer than ~20%, and nothing here warrants one.
