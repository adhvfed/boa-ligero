# Receiver frontier review — 2026-08-03

Status: review complete; standalone `This` lowering is a no-go for the current
whole-CodeBlock tier.

This review answers the pre-Slice-2B question without changing the JIT
allowlist. `This` is a real static frontier, but removing that frontier alone
cannot produce an admitted native entry for the measured method control.

## Interpreter contract

`This` has two semantically distinct paths:

1. Ordinary strict and sloppy calls normalize the receiver while creating the
   function environment, store the normalized `JsValue` in the frame prologue,
   and set `THIS_VALUE_CACHED`. Strict calls retain the original value; sloppy
   calls replace `null`/`undefined` with the realm global `this` and box primitive
   receivers.
2. Frames without a cached receiver resolve `GetThisBinding` through the
   environment chain. That is required for lexical `this`, global fallback,
   and derived constructors. Resolution can throw while a derived constructor's
   receiver remains uninitialized; a successful lookup is then cached in the
   frame.

Consequently, a native implementation must not unconditionally copy the frame
prologue slot. The smallest exact implementation would call the same VM-owned
operation as the interpreter and write a boxed destination register. That
keeps the cloned `JsValue` rooted in the VM stack, supports every receiver
representation, preserves environment lookup and exceptions, and needs no raw
object pointer or representation specialization. A budgeted entry would charge
the `This` bytecode before that helper exactly once; an exception is a completion
exit, not a replaying deopt.

If receiver lowering is reconsidered, make the interpreter operation a shared
crate-visible helper rather than duplicating its cache and environment logic in
the JIT.

## Whole-CodeBlock and admission result

The measured `Counter.prototype.inc` body is:

```text
GetArgument, Move,
This, This, GetPropertyByName, Add,
SetPropertyByName,
This, GetPropertyByName,
PushFromRegister, PopIntoRegister, SetAccumulator,
CheckReturn, Return, CheckReturn, Return
```

The first `This` is at decimal PC 18. Supporting it would only advance the
frontier to `SetPropertyByName` at PC `0x36`; that store is outside the native
allowlist. Supporting both operations would make a 16-instruction straight-line
helper eligible for compilation analysis, but production admission deliberately
rejects non-loop bodies below 45 instructions. Adding a method-specific admission
exception would undo the measured protection against helper-heavy boundary
overhead.

A post-Slice-2A sanity pair confirms the intended current behavior: five timed
runs after 70 warmups produced the same accumulator with 23.09 ms/run in the
interpreter and 24.08 ms/run with the tier enabled. The JIT run emitted zero
compilations and native entries and recorded only the caller/method admission
denials. This single pair is not a performance gate, but it confirms that the
admission path is doing what the design says.

## Decision

Do not implement standalone `This` lowering and do not widen admission for the
measured method helper. It would change code and ABI surface without creating a
production native entry or satisfying the complete-workload stop/go criterion.

The next design checkpoint is guarded binding reads:

- `GetName` is the first blocker in every measured microbenchmark caller;
- the floating-point arithmetic caller becomes a complete supported native loop
  if its stable `N` binding can be read safely;
- integer and array callers expose later bitwise/storage frontiers, so they are
  follow-up evidence rather than part of the first binding patch;
- method and flat-call callers also expose name reads first, but their helper
  transition cost remains a negative control.

Before implementation, identify a VM-owned binding identity and invalidation
signal covering reassignment, deletion, direct `eval`, realm changes, and mutable
lexical state. If Boa has no such lifetime/version contract, check in that design
first; do not cache a raw environment pointer or specialize from a JIT-only
counter.

## Receiver tests retained for a future slice

If later evidence selects receiver lowering as part of a useful admitted region,
the differential must cover:

- strict object, primitive, `null`, and `undefined` receivers;
- sloppy global substitution and primitive boxing;
- bound ordinary calls and nested calls;
- arrow lexical capture across nested environments;
- derived constructors before and after `super()`;
- forced GC while the copied receiver is live;
- exact instruction-budget exhaustion at each `This` occurrence;
- identical result, exception, frame PC, and diagnostic exit classification in
  interpreter, explicit-JIT, and context-tiered modes.
