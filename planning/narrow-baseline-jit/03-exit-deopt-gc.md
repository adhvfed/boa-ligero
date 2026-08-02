# Exit, deoptimization, and GC contract

This document is the safety boundary for generated code. Performance work must
not proceed past a missing rule here.

## Exit word

The current shim protocol uses the high bit of a `u64` for a pending completion
and the remaining bits for a program counter. The native tier needs a slightly
more expressive version while retaining a compact ABI:

```text
bits  0..7   exit kind
bits  8..39  resume bytecode PC
bits 40..63  kind-specific flags or small payload
```

The exact bit layout can change during implementation, but the kinds should be
explicitly represented:

- `Continue` — an internal native edge, not normally returned to Rust;
- `Deopt` — resume the current frame in the interpreter at `resume_pc`;
- `Return` — the current frame's return value is materialized and the VM must
  perform the normal frame transition;
- `Call` — a call site needs the runtime to push/enter a frame or dispatch a
  non-native target;
- `Throw` — a `CompletionRecord` or pending exception is stored in VM state;
- `Budget` — instruction or loop budget was exhausted;
- `Yield`/`Suspend` — reserved for a later tier, and initially always exits.

Do not pass `CompletionRecord`, `JsValue`, `JsError`, or Rust references across
the C ABI. Store them in traced VM state and return a status word.

## Deoptimization invariant

Every deoptimization must resume at a bytecode boundary with a state that the
interpreter could have produced after executing the preceding instructions.

Before returning `Deopt`, the generated code must have established:

- the current `CallFrame` is still the live frame for the compiled entry;
- `frame.pc` is the exact next bytecode PC, or the current unsupported PC if
  the operation has not started;
- every live bytecode register contains a valid boxed `JsValue` in the VM
  stack;
- `vm.return_value`, `pending_exception`, and the operand stack have their
  interpreter-visible values;
- environments, `env_fp`, iterators, binding stacks, and frame flags are
  unchanged unless the native operation owns their transition;
- no stale native pointer will be used after control returns to Rust.

For a guard failure, perform the guard before any externally visible mutation.
If a helper has already mutated state, its contract must return the post-helper
PC and the corresponding exit kind; it must not report a pre-operation deopt.

## Calls and returns

Calls are VM transitions, not ordinary arithmetic branches. The first direct
call implementation should use a runtime transition helper or return a `Call`
exit after materializing the caller. The runtime then uses the existing
`CallFrame`/stack calling convention.

Returns need the same discipline. A native return path may:

- materialize the return value into `vm.return_value`;
- return `Return`;
- let a small VM-owned transition perform `handle_return` semantics, including
  `EXIT_EARLY`, stack truncation, frame popping, and the caller's result push.

Do not duplicate `handle_return` semantics in Cranelift IR. Put the transition
in a small Rust VM helper so ordinary and JIT execution share one definition.

## Exceptions

Native code must not rely on Rust unwinding. A fallible helper must either:

1. complete successfully and return a normal status; or
2. record the error/completion in VM state and return `Throw`/`Deopt`.

If an exception is thrown by a native operation, set `frame.pc` to the
operation's bytecode PC before entering the VM error machinery. This preserves
handler lookup and backtrace positions. The existing `Context::handle_error`,
`handle_exception_at`, and throw/return transitions should remain the semantic
authority.

Test both caught and uncaught exceptions, including `try/finally`, because
exception-handler environment cleanup is part of the frame contract.

## Runtime budgets and safepoints

The JIT must not create a way around execution limits:

- charge instruction budget according to the original bytecode instructions;
- poll at loop backedges and before long native regions;
- charge loop-iteration limits exactly as the interpreter does;
- exit before async suspension or a host callback unless that transition has an
  explicit helper contract.

Every helper that may allocate, call JavaScript, mutate an object shape, invoke
GC, throw, or re-enter the VM is a safepoint. Before entering it:

1. write back the current PC;
2. materialize primitive SSA values into VM stack slots;
3. ensure no raw GC/object/shape pointer is live only in a native register;
4. call the helper;
5. reload state after the helper returns, or honor its exit status.

The initial native representation intentionally keeps object values boxed in
the VM stack. It may be less aggressive than a full optimizing JIT, but it
keeps the existing GC root model intact.

## Shape and object safety

Existing property ICs pair raw shape addresses with weak-shape liveness checks.
Generated code must not read a cached raw address and treat equality as proof
of identity. A JIT guard must either:

- call a VM helper that performs the existing address-plus-liveness check; or
- use a JIT-owned guard object whose lifetime and liveness contract are explicit
  and tested.

The same rule applies to `ElementIC`, object storage, and function targets.
Pointer reuse after GC must produce a miss, never a false hit.

## ABI and panic safety

All generated-to-Rust entry points must have a documented `extern "C"`
signature, ownership rules, and no-unwind guarantee. Add debug assertions and
test-only validation around:

- null/invalid context pointers;
- current-frame identity;
- register bounds;
- exit-word decoding;
- pending-completion ownership;
- backend/code-cache lifetime.

If a Rust helper can currently panic for malformed state, validate the state
before entering it or route the case to the interpreter. Never rely on an
`extern "C"` call to catch a Rust panic.

