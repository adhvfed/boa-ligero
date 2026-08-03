# Loop-header OSR

Status: the general direction remains current. The exact first-shape ABI is
normative in the [Slice 4A0 review](20-loop-osr-abi-review-2026-08-03.md), which
adds latch/backedge and numeric-representation identity, a distinct post-effect
continuation exit, and bounded per-region hotness.

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

Schedule this ABI only after the profile shows material hot work remaining in
a one-shot frame after its PC-zero entry opportunity has passed. Repeatedly
called hot functions that already enter through the normal cache do not, by
themselves, justify OSR.

## Region identity

An OSR entry is not the same artifact as a function entry. The first-shape
artifact uses this exact backend-owned key:

```text
(CodeBlock runtime-local ID,
 loop-header PC,
 latch/backedge PC,
 uniform I32/F64 representation,
 finite-budget mode,
 diagnostic mode)
```

The context-owned backend and its generation guard provide runtime/realm and
machine-code lifetime isolation. Runtime-local CodeBlock IDs are monotonic for
that backend lifetime, current CodeBlock identity is rechecked at entry, and
no artifact is shared across backends or processes. A future entry kind that
depends on mutable feedback or bytecode versions must add those assumptions to
its key; 4A1 has neither and must not imply a broader sharing contract.

The entry metadata must record:

- the exact header PC and predecessor/backedge PC;
- live register and operand-stack locations;
- the expected current CodeBlock/header and validated frame/register shape;
- the primitive representation of each native value;
- guards required before entering the region;
- every deopt PC and materialization map;
- whether the region contains a runtime-limit poll or helper safepoint.

Do not retain a raw frame pointer or share an OSR entry across mutable bytecode
versions. The first pure-numeric artifact is backend-owned and reloads the
current recursive frame after checking the live CodeBlock/header identity.

## Conservative first eligibility

Start with loops that satisfy all of these conditions:

- ordinary, non-async, non-generator function;
- validated backward edge to a known instruction boundary;
- no `try/finally`, `eval`, `with`, iterator suspension, or environment shape
  mutation in the region;
- no calls, allocations, host callbacks, or property writes in the first OSR
  region;
- all loop-carried native values are local registers with a complete uniform
  `I32` or `F64` representation; untouched boxed values may remain only in
  proven preserved VM slots and are never loaded into the first native region;
- the loop condition and backedge are supported native branches;
- loop and instruction budgets can be charged at the same frequency as the
  interpreter.

Expand eligibility only after a negative test proves that an excluded shape
returns to the interpreter rather than being partially entered.

## Entry protocol

At a safe interpreter backedge boundary:

1. let the interpreter charge and complete the canonical latch;
2. return to the scheduler-owned post-`execute_one` boundary and inspect the
   exact bounded region state/plan/artifact key;
3. when the exact key becomes hot, compile synchronously while the scheduler
   owns both the backend and stable current frame and no helper is in flight;
4. install only a complete artifact, then invoke either it or an already cached
   exact variant from the same boundary;
5. repeat the backend/code/header/frame/budget/representation entry guards in
   generated code before loading any native live-in; and
6. enter native code or close only this frame's OSR decision and continue in
   the interpreter on a dynamic miss.

The native entry must never assume that the interpreter's previous iteration
left values in Cranelift registers. Every OSR value comes from the VM stack or
an explicit helper and is guarded before use.

The design review must show the concrete `Context`/backend ownership path. It
must not hold a mutable `JitBackend` borrow across a helper, host callback, GC,
or nested VM execution. Compilation and entry are separate states so a failed
or over-budget compile cannot accidentally enter a partial artifact.

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

Budget accounting must name the charged bytecode interval at entry, each
backedge, and every exit. A pre-effect guard may refund only the current
instruction when the interpreter will replay it; completed loop work and
backedges are never refunded. Differential tests must include exhaustion one
instruction before, at, and one instruction after each OSR boundary.

Do not OSR across host re-entry, nested `Context::run`, async suspension, or a
frame whose identity has changed. Those cases remain interpreter boundaries.

## Tests and gate

Add tests for:

- a one-shot loop that reaches OSR and returns the expected value;
- zero-iteration and one-iteration loops;
- type change in an induction variable, guard failure at the header, and a
  numeric → nonnumeric → numeric sequence proving one frame cannot poison a
  reusable numeric artifact;
- loop and instruction limits during native execution;
- exception propagation and forced GC around an OSR helper;
- recursion and nested ordinary frames;
- malformed/non-boundary targets rejected before entry.

The OSR gate is met only when a loop that cannot receive a second function
entry shows native execution in stats and remains semantically identical to a
pure interpreter run. Admission must also reject a loop when estimated
remaining work cannot amortize synchronous compilation; the diagnostic record
must distinguish that decision from unsupported compilation.
