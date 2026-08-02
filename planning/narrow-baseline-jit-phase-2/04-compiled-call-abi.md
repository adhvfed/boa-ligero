# Compiled ordinary calls

## Performance problem

Phase 1 has a guarded ordinary-call lowering, but the normal hit path still
uses a VM transition and returns a `Call` exit to the scheduler. That is safe
and useful as a bridge; it is not the steady-state path for tiny functions or
method-heavy browser code. Phase 2 should remove the interpreter scheduler
from a matching compiled-callee call without inlining the callee body.

## Recommended shape

Keep the existing public `extern "C" fn(*mut Context) -> u64` entry contract
for generated code, and add a VM-owned compiled-call trampoline with a
versioned internal contract:

```text
compiled caller
  -> guard callee identity / realm / ordinary-function metadata
  -> materialize caller state and continuation PC
  -> VM helper pushes the normal CallFrame
  -> helper invokes the cached callee entry directly
  -> VM-owned return transition restores caller and result
  -> caller native continuation
```

The exact helper signature is an implementation decision, but it must pass a
stable compiled-entry handle or function pointer plus metadata, not an
unvalidated `CodeBlock` or raw object pointer. The backend must outlive every
entry pointer it publishes.

The helper may initially perform the frame push/pop and call the entry itself;
what matters is that it does not return to the general interpreter scheduler
for the normal matching case. A slow miss can still return `Call`/`Deopt` and
execute the ordinary `Call` opcode.

## Entry and target guards

Require all of the following before the direct path:

- callee identity matches the call-site feedback;
- caller and callee belong to the same realm/runtime owner;
- target is an ordinary, non-class-constructor, non-async, non-generator
  function;
- target entry ABI/version and frame metadata match;
- argument/return calling convention is supported;
- no spread, proxy, bound/native call, `eval`, or host callback is involved.

When a target is not compiled, choose one explicit policy and measure it:

1. take the normal interpreter call and let the callee accumulate hotness; or
2. compile the target synchronously at the safe boundary, then enter it.

Prefer option 1 initially to avoid adding compile latency to an arbitrary
native call. The direct path should become available on a later call after the
target cache is populated.

## Frame and return invariants

The compiled call must preserve the same observable frame structure as the
interpreter:

- caller continuation PC is recorded before frame construction;
- argument and `this` slots use the existing calling convention;
- recursion and stack limits are charged by the shared VM transition;
- stack traces see both caller and callee frames;
- callee exceptions find the correct handler and restore environments;
- return values are placed in the same destination register/stack slot;
- `EXIT_EARLY`, frame flags, and stack truncation use the shared
  `handle_return` implementation.

Do not implement return semantics by copying a subset of `handle_return` into
Cranelift IR.

## GC and safepoint contract

Before entering the helper:

1. write the caller PC and materialize all live primitive values;
2. keep the callee as a traced VM value or re-load it from the VM stack;
3. do not hold a raw object, shape, environment, or `Gc` pointer across the
   call;
4. let the helper push the normal frame before invoking code that can allocate
   or trigger GC;
5. reload the caller state only after the helper reports a valid return.

An exception, runtime limit, host re-entry, or guard failure returns through
the shared pending-completion/exit protocol. No Rust panic or unwinding may
cross the generated entry.

## Non-goals for this ABI

- no body inlining;
- no polymorphic inline cache for the first version;
- no native method lookup plus call fusion;
- no direct native calls to native functions, proxies, bound functions,
  constructors, or async/generator targets;
- no hidden frames that make debugging or stack traces inaccurate.

## Tests and gate

Cover matching and mismatching target identities, target replacement, nested
calls, recursion, exceptions, stack traces, runtime limits, forced GC,
callee deoptimization, and host/native fallback. Add a counter for scheduler
round-trips so the performance test proves the matching path actually avoids
the interpreter loop.

The call gate requires a warm win on `fn-call-flat` and a method-shaped test,
with the interpreter result and visible frame behavior unchanged.

