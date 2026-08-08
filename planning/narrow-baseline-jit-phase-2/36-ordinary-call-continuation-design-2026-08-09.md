# Ordinary-call continuation design — 2026-08-09

Status: selected for implementation. This is the first compiled-call ABI
slice, not the direct compiled-callee entry slice.

# Problem

The native function compiler can lower `Call`, but the successful helper path
always returns a `Call` exit to the general JIT scheduler. Production admission
therefore rejects every call-containing body: generated code has no way to
resume at the instruction following the call.

A fresh schema-10 profile on the current tree observed two million calls in
`fn-call-flat`, all to the same ordinary target after the first observation,
and 800,000 equally stable ordinary calls in `method-call-mono`. Both workloads
created zero artifacts. The method caller is otherwise a supported
24-instruction native body and is denied only by the call boundary.

# Constraints

- Use `function_call` to construct the normal `CallFrame`; do not reproduce
  argument, receiver, realm, environment, recursion-limit, or stack layout in
  Cranelift IR.
- Use the interpreter's existing opcode and `handle_return` machinery while
  the first version executes the callee. Nested calls and caught exceptions
  must remain valid.
- Materialize caller primitive state before a helper that can allocate, invoke
  host code, trigger GC, or inspect VM frames.
- Keep boxed values in traced VM registers. No `JsObject`, `Gc`, environment,
  shape, or backend-owned code pointer may cross the generated-code boundary.
- A guard miss must leave the calling-convention stack untouched and replay
  the same `Call` bytecode in the interpreter.
- A successful transition must restore the exact caller frame and next PC
  before generated code resumes.
- No Rust unwinding or `JsError` crosses the C ABI.
- Budgeted artifacts must charge the caller instruction once and let ordinary
  interpreter dispatch charge every callee instruction.

# Options

## Embed and invoke a compiled-callee pointer immediately

This is the eventual steady-state shape, but it requires target-driven small
callee admission, a stable entry handle, nested artifact status validation,
and a revised backend teardown proof. Implementing all of those before a
native caller can resume would make failures difficult to localize.

## Return to the general scheduler after every call

This is the current implementation. It is semantically safe but cannot admit
production callers and prevents native continuity across the dominant call
sites.

## VM-owned continuation trampoline

The generated caller invokes one helper. The helper validates an ordinary,
non-class target, calls `function_call`, and uses a depth-bounded interpreter
loop until the caller frame is restored. It returns success only when the
current frame identity and continuation PC still match. Completion and runtime
limit failures use the existing pending-completion protocol. A non-ordinary or
class target returns the pre-effect guard-failure tag.

# Recommendation

Implement the VM-owned continuation trampoline first. It establishes the
caller continuation, frame, exception, budget, and GC contract without
changing executable-code ownership. Once verified, replace only the callee
execution portion with a guarded cached native entry; the caller-side ABI and
fallback remain unchanged.

At the same time, remove production's dependency on last-target feedback for
this generic ordinary path. Identity speculation is unnecessary until direct
compiled entry selection exists. Detailed diagnostics may continue reporting
monomorphism independently.

The premise that might be wrong is performance: interpreting the callee inside
the helper could cost as much as returning to the scheduler. The slice is
accepted only if keeping the otherwise-supported caller native produces a
clear warm improvement.

# Evidence

Correctness coverage must include:

- ordinary call success and a different ordinary target at the same site;
- non-ordinary and class-constructor guard replay;
- nested calls, recursion limits, exceptions caught inside the callee, and an
  exception escaping the native caller;
- forced GC and host/native re-entry during the callee;
- exact finite instruction-budget parity;
- stack traces containing the normal caller and callee frames;
- zero scheduler call exits on successful continued calls.

Performance acceptance requires seven fresh release-process samples of
`method-call-mono`, diagnostics disabled, exact matching sinks, native caller
evidence, zero steady-state deopts, and at least a 10% median improvement over
the current production JIT. `fn-call-flat` remains a negative control until
its independently reviewed `GetNameGlobal` prerequisite is implemented.

# Consequences

If accepted, call-containing whole-function bodies can remain native across
ordinary calls even before their callees have compiled entries. The next call
slice can add target-driven leaf compilation and direct cached entry invocation
inside the same helper contract. If the performance gate fails, retain the
depth-bounded VM primitive only if it improves interpreter code structure;
otherwise remove the entire behavior slice and revisit region stitching or
direct compiled entry as one reviewed unit.
