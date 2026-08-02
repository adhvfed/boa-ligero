# Loop-header OSR

## Why OSR is Phase 2 work

The current tier can become eligible after function entries or observed
backedges, but the native entry is primarily attempted at bytecode PC zero.
That is sufficient for repeatedly called functions and insufficient for a
one-shot browser initialization function whose hot work is a loop. Phase 2
should compile a conservative loop region and enter it from the running frame
after a hot backedge.

OSR is not a license to reconstruct arbitrary interpreter state. The first
version should accept only loop headers for which the compiler can prove a
complete materialization map and a stable frame/environment contract.

## Region identity

An OSR entry is not the same artifact as a function entry. Key it by:

```text
(realm identity,
 CodeBlock identity,
 loop-header PC,
 bytecode/ABI version,
 feedback/representation signature)
```

The entry metadata must record:

- the exact header PC and predecessor/backedge PC;
- live register and operand-stack locations;
- the expected `CallFrame` identity and environment depth;
- the primitive representation of each native value;
- guards required before entering the region;
- every deopt PC and materialization map;
- whether the region contains a runtime-limit poll or helper safepoint.

Do not share an OSR entry across realms or mutable bytecode versions.

## Conservative first eligibility

Start with loops that satisfy all of these conditions:

- ordinary, non-async, non-generator function;
- validated backward edge to a known instruction boundary;
- no `try/finally`, `eval`, `with`, iterator suspension, or environment shape
  mutation in the region;
- no calls, allocations, host callbacks, or property writes in the first OSR
  region;
- all loop-carried values are local registers with a complete `I32`, `F64`, or
  boxed-stack representation;
- the loop condition and backedge are supported native branches;
- loop and instruction budgets can be charged at the same frequency as the
  interpreter.

Expand eligibility only after a negative test proves that an excluded shape
returns to the interpreter rather than being partially entered.

## Entry protocol

At a safe interpreter backedge boundary:

1. record the backedge and inspect the region cache;
2. if no entry exists, request compilation without borrowing the backend from
   inside a partially executed helper;
3. after the current operation completes, compile and install the entry;
4. materialize the live frame state and set the header PC;
5. run the entry guard against the current frame and feedback signature;
6. enter native code or continue interpreting on a miss.

The native entry must never assume that the interpreter's previous iteration
left values in Cranelift registers. Every OSR value comes from the VM stack or
an explicit helper and is guarded before use.

## Exit protocol

Every OSR exit must:

- write the exact resume PC before a fallible/allocating helper;
- materialize all live primitive values into VM stack registers;
- preserve the loop condition's observable state and operand stack;
- return a reason that distinguishes type/shape/unsupported/budget/exception
  exits in diagnostics;
- let the existing interpreter execute the current or next instruction exactly
  once.

If the region has already performed a visible mutation, its helper must return
the post-operation PC and state; it must not report a pre-operation deopt.

## Runtime limits and re-entry

The OSR loop must use the same `consume_loop_iterations` and instruction-budget
semantics as Phase 1 native loops. Polling must occur before a potentially long
native span, not only when the loop exits. If a poll throws, stash the
`CompletionRecord` in traced VM state and return the existing break/budget
protocol.

Do not OSR across host re-entry, nested `Context::run`, async suspension, or a
frame whose identity has changed. Those cases remain interpreter boundaries.

## Tests and gate

Add tests for:

- a one-shot loop that reaches OSR and returns the expected value;
- zero-iteration and one-iteration loops;
- type change in an induction variable and guard failure at the header;
- loop and instruction limits during native execution;
- exception propagation and forced GC around an OSR helper;
- recursion and nested ordinary frames;
- malformed/non-boundary targets rejected before entry.

The OSR gate is met only when a loop that cannot receive a second function
entry shows native execution in stats and remains semantically identical to a
pure interpreter run.

