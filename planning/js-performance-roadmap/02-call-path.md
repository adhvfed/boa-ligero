# 02 — The call path (lever #1)

**Why first:** `method-call-mono` is the worst fair benchmark at **6.55×**
`node --jitless`, and `fn-call-flat` is 4.01×. Unlike a single arithmetic
opcode, _calls are everywhere_ — every method, every closure, every callback —
so a win here moves the whole geomean. And the call path today has **no
specialization at all**.

## Current state (grounded)

A `Call` opcode does, every single time
(`core/engine/src/vm/opcode/call/mod.rs:186`):

```rust
let func = context.vm.stack.calling_convention_get_function(argument_count.into());
let Some(object) = func.as_object() else { return Err(Self::handle_not_callable()); };
object.__call__(argument_count.into()).resolve(context)?;
```

`__call__` is a dynamic dispatch through the object's internal-methods vtable,
landing (for a normal JS function) in `function_call`
(`core/engine/src/builtins/function/mod.rs:982`), which clones realm / code /
environments, builds a `CallFrame`, and calls `push_frame`
(`core/engine/src/vm/mod.rs:611`):

```rust
frame.fp = current_stack_length - frame.argument_count - CallFrame::FUNCTION_PROLOGUE;
frame.rp = self.stack.stack.len() as u32;
self.stack.stack.resize(self.stack.stack.len() + register_count, JsValue::undefined()); // grow + zero-fill
// ...
self.current_frame_ptr = NonNull::from(self.frames.last_mut().unwrap_unchecked());
```

So each monomorphic call pays for, in order:

1. a vtable dispatch through `__call__` and a `CallValue` enum round-trip
   (`.resolve()`);
2. a not-callable guard and type checks already known to be false at warm sites;
3. realm/code/environment **clones** (`Gc` refcount traffic) in `function_call`;
4. a `Vec::resize` that may reallocate, **zero-filling every register** with
   `undefined`;
5. environment/binding prologue setup.

Already present: a **cached frame pointer** (`current_frame_ptr`,
`core/engine/src/vm/mod.rs`) so per-opcode frame access is one load. That's the
right kind of optimization; the call path needs more of it.

This is exactly the BASELINE's listed levers "CallFrame restructure",
"`MaybeUninit` frame init", and "polymorphic-call dispatch".

## What the literature says

Call sites are the original home of inline caching: Deutsch & Schiffman (POPL 1984) cached the looked-up method at the call site; Hölzle, Chambers & Ungar
(ECOOP 1991) generalized to polymorphic inline caches recording multiple
receiver classes. Every production JS engine has call ICs. The interpreter-tier
version (no native codegen) is to cache the resolved callee + a guard in the
bytecode's inline-cache slot, à la Brunthaler's "Inline Caching Meets Quickening"
(ECOOP 2010) and CPython's `CALL` specializations (PEP 659).

## Plan

Four sub-levers, each independently shippable and measurable. Do them in order;
each has a cheap opportunity-count check (see
[01](01-measurement-methodology.md)) before the A/B.

### 2a. Call-site inline cache for `OrdinaryFunction`

Add an IC slot to the `Call` opcode (mirroring the property PIC in
`vm/inline_cache/`). Cache: callee object identity (or shape) → its `CodeBlock`

- `register_count` + arity. On a hit where the callee is an ordinary function and
  non-proxy/non-bound, **skip `__call__`, the not-callable guard, and the
  `CallValue` resolve**, and jump straight to frame setup. Fall back to the generic
  path on miss/megamorphic.

### 2b. Specialized frame push (`MaybeUninit`, no zero-fill)

The `resize(.., JsValue::undefined())` zero-fills `register_count` slots every
call. Registers are written before read in well-formed bytecode, so the fill is
dead work. Options: `MaybeUninit` register window (BASELINE's "`MaybeUninit`
frame init"), reserve-ahead so `resize` never reallocates, or a frame freelist so
hot recursion reuses the same backing store. Measure register-fill cost with a
counter first.

### 2c. Skip redundant clones in `function_call`

`function_call` clones realm/code/environments (`Gc::clone` = refcount
inc/dec) on every call. On the IC-hit path these are derivable from the cached
callee; mirror the "borrowed fast path" pattern already used on IC-hit property
reads (`as_object_borrowed`) to avoid the refcount traffic. This is the same win
that landed for property reads, applied to calls.

### 2d. Fuse `GetProperty` + `Call` for method calls

`obj.m(...)` compiles to a property-get (one IC lookup) followed by a `Call`
(another dispatch) with intermediate value movement. A fused
`GetPropertyAndCall` opcode does the method-load IC and the call in one handler,
eliminating a dispatch and a register shuffle. This is what `method-call-mono`
specifically exercises. Lower priority than 2a–2c (it's a compile-time pattern
match in the bytecompiler), but it's the targeted fix for the worst benchmark.

## Expected ROI & validation

- Primary metrics: `method-call-mono` (6.55× → target <2.5×) and `fn-call-flat`
  (4.01×). Secondary: any call-heavy real workload (richards, deltablue).
- Because calls are pervasive, expect a measurable geomean move even on
  benchmarks not "about" calls.
- Validate at `RUNS=200 WARMUP=30`, A/B by binary swap. Add `richards`/`deltablue`
  Octane SCOREs as real-workload confirmation.

## Risks

Correctness is the whole game here: bound functions, `Proxy`, getters/setters as
callees, arity mismatch, spread/`apply`, generators/async, and re-entrancy must
all fall off the fast path correctly. Guard on the exact callee class and bail to
the generic path for anything else. Every fast path needs a test that proves the
slow path still runs for the excluded cases (cross-reference the operator-snapshot
regression tests under `tests/`).
