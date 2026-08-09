# Ordinary-call continuation checkpoint — 2026-08-09

Status: accepted. Commit `d76d0b9e` implements the VM-owned continuation
selected in the preceding design review.

# Result

Call-containing whole-function bodies may now remain native across an
ordinary, non-class call. Generated code publishes live primitive state, the
helper constructs the normal Boa frame with `function_call`, and the existing
interpreter machinery owns the callee and any nested frames until the exact
compiled caller and continuation PC are restored.

This deliberately does not invoke a compiled callee directly. It establishes
the caller-side safepoint, frame, exception, budget, and replay contract on
which that later optimization can rely.

# Implementation shape

- `Call` is a fallthrough instruction in the native CFG rather than an
  unconditional scheduler exit.
- A whole-function backwards liveness analysis identifies primitive registers
  that remain live after each instruction. Only dirty values in that set are
  materialized at the call safepoint; guard exits still materialize every
  path-defined dirty value.
- The callable and receiver are identified from the bytecode calling-
  convention push group. Ambiguous stack shapes fail native analysis instead
  of guessing register roles.
- Boxed named-property results are copied directly between traced VM registers.
  No GC pointer is returned through the generated-code ABI.
- A non-ordinary or class-constructor target is rejected before effects and
  replays the `Call` in the interpreter. Different eligible ordinary targets
  use the same generic continuation without identity deoptimization.
- If an exception unwinds the compiled caller and is caught by an ancestor,
  the helper returns the ancestor's real PC to the scheduler. This behavior
  was added after the full JIT library suite exposed that ownership case.
- The obsolete production last-target map and its test-only admission switch
  were removed. Bounded call-site diagnostics remain observational only.

# Correctness and containment evidence

The permanent tests cover:

- same and different ordinary targets, plus non-ordinary pre-effect replay;
- nested ordinary frames;
- an exception caught inside the interpreted callee and an exception escaping
  through the compiled caller to an ancestor;
- recursion limits;
- exact successful and exhausted finite instruction-budget parity;
- a boxed method value and receiver surviving forced collection;
- exact results with zero successful-call scheduler exits.

After the final changes:

- `cargo test -q -p boa_engine --features jit --lib`: 1,247 passed, 5 ignored;
- `cargo clippy -p boa_engine --features jit --lib --tests -- -D warnings`:
  passed;
- `cargo fmt --all --check`: passed;
- `git diff --check`: passed.

# Performance gate

Seven paired release-process samples of `method-call-mono` produced these
nanoseconds-per-run medians:

| execution | median |
| --- | ---: |
| interpreter | 21,712,133 |
| JIT with ordinary-call continuation | 19,051,866 |
| prior production JIT | 21,320,858 |

The accepted implementation is 12.25% faster than its paired interpreter and
10.64% faster than the prior production-JIT median. Every steady JIT sample
returned the exact `acc=2481600` sink, installed one native caller, entered it
14 times, and recorded zero deopts and zero scheduler call exits. The native
caller payload is 2,248 bytes after liveness-based safepoint materialization,
down from 2,316 bytes in the initial all-dirty prototype.

The cold property-cache miss still produces one expected pre-effect deopt.
That is excluded from the declared steady-state gate and retains interpreter
replay semantics.

# Decision and next boundary

The slice is retained. The next call optimization should replace only callee
execution with a guarded cached native entry while preserving this
continuation helper as its interpreter fallback. That work first needs
target-driven admission for small leaf callees and an ownership-safe way to
resolve entries without embedding backend-owned executable pointers in
generated artifacts.

`fn-call-flat` remains a useful negative control: its caller is independently
blocked by `GetNameGlobal`, so the binding read must receive a separate
semantic review rather than being folded into the call ABI.
