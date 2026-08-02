# Bytecode and compiler pipeline

## Pipeline

The compiler should have explicit intermediate stages:

```text
CodeBlock
  -> decoded instructions with bytecode PCs
  -> validated control-flow graph
  -> eligible regions and exit sites
  -> feedback snapshot
  -> Cranelift IR
  -> finalized CompiledEntry + frame metadata
```

Keeping these stages separate matters. A decoder/CFG bug is a compiler bug;
an unsupported operation is an optimization miss. They must not be conflated.

## Decoding and validation

Use the existing `Bytecode::next_instruction` and `InstructionIterator` as the
source of instruction boundaries. Build a compact internal record containing:

```text
pc
next_pc
Opcode
Instruction operands
basic-block id
exception-handler membership
```

While decoding:

- reject a bytecode target that is not an instruction boundary;
- reject truncated operands and impossible register indices;
- record fallthrough and branch successors;
- record `CodeBlock::handlers` and their environment cleanup metadata;
- preserve the original bytecode PC on every internal instruction.

The compiler should never infer instruction size from an opcode number or from
the generated machine code. The bytecode decoder is the authority.

## CFG construction

Create leaders at:

- entry PC `0`;
- every valid branch target;
- the fallthrough after a conditional branch;
- exception-handler starts and protected-range boundaries;
- explicit call, return, throw, and yield boundaries.

The first native tier can use a conservative CFG:

- simple `Jump` and conditional jumps may become native edges;
- a backward edge is a candidate loop edge;
- `Call`, `New`, `Return`, `Throw`, `Yield`, and environment-changing
  instructions terminate the current native region unless their dedicated
  lowering has been implemented;
- unsupported or malformed edges exit to the interpreter.

Do not build a trace by assuming that the only jump opcodes are the ones known
today. The same-frame branch allowlist must be explicit and tested. A missing
control-flow case must cause a compile rejection or deoptimization, never a
fallthrough into the wrong block.

## Region strategy

The first useful compiler does not need to make every instruction in a
`CodeBlock` native. It should compile native regions and use exact exits:

```text
entry -> native region -> native branch/backedge
                       \-> deopt at unsupported/failing operation
```

There are two safe choices for unsupported operations:

1. stop compilation before the operation and return to the interpreter; or
2. emit a direct, typed helper for that operation if its VM transition contract
   has been reviewed.

Start with choice 1. Add helper lowerings only where they are independently
measurable and have a written ABI contract. The old per-opcode shim can remain
the fallback prototype, but it must not be hidden inside the native lowering
and called for every selected instruction.

## Compiler value model

The compiler needs a value state for each bytecode register:

```text
Unknown / boxed stack slot
I32 SSA value
F64 SSA value
```

`Unknown` means the value is read from or written to the VM's `JsValue` stack
using a reviewed runtime helper. `I32` and `F64` may live in Cranelift SSA
values only while their guards remain valid and before a safepoint.

Do not bake in the NaN-boxed representation. Boa also supports the enum-based
`JsValue` representation, so the JIT value model must be independent of that
implementation detail.

Each native region needs a small materialization map:

```text
bytecode register -> boxed VM slot, or
bytecode register -> primitive SSA value to materialize
```

Before a deopt or helper that may allocate, materialize every live primitive
value into its VM slot as a real `JsValue`. If materialization would require a
semantic decision the native path cannot prove, deopt before changing state.

## First lowering allowlist

Implement in this order:

### Frame-local and constant operations

- `StoreZero`, `StoreOne`, `StoreInt8`, `StoreInt16`;
- `StoreUndefined`, `StoreNull`, and simple numeric constants;
- `Move` when the source value is already represented in the native region;
- `SetAccumulator`, `SetRegisterFromAccumulator`, and the minimal return
  epilogue once the exit protocol supports them.

These operations validate register addressing, materialization, and frame
metadata without committing to object layout.

### Numeric operations

- `Add`, `Sub`, `Mul` for guarded `i32` values where the exact JavaScript result
  remains an `i32`;
- the corresponding `f64` numeric path when both operands are numbers and the
  operation's IEEE-754 behavior is preserved;
- numeric comparisons used by loop branches;
- `Jump` and the simple conditional jump variants.

`+` must fall back for strings, coercion, and any unproven operand class.
Overflow, division edge cases, `-0`, NaN behavior, and integer-to-number
representation changes must be covered by guards and tests rather than
assumed away.

### Feedback-backed reads

- `GetPropertyByValue` for a non-negative integer key and a matching dense
  numeric element cache;
- `GetPropertyByName` for a matching live shape and data-property slot;
- `GetLengthProperty` only after its exact array/object semantics are covered.

Start with a reviewed runtime helper that performs the complete guard and load.
Only move the shape/slot or dense-storage load into direct Cranelift memory
operations after the object layout, weak-shape liveness, and GC rules are
documented in code.

### Calls

Add direct ordinary-function calls after the frame and exit protocol are
stable. A call site needs feedback for the target function/code block; the
`Call` opcode's argument count alone is not a target identity.

The first direct-call tier should not inline the callee body. It should:

1. guard the callee identity and ordinary-function constraints;
2. materialize the caller state;
3. enter a compiled callee when one is available, or return a call/deopt exit;
4. let the runtime push/pop the normal `CallFrame` and resume the interpreter
   for all other callees.

Proxies, bound functions, native functions, constructors, generators, async
functions, and `eval` remain interpreter exits.

## Helper ABI

Generated code should call small, purpose-built `extern "C"` helpers only when
it cannot remain in native IR. Helpers should receive a context pointer plus
plain integer/pointer arguments and return a compact status/value word. They
must:

- be `#[inline(never)]` while being profiled so their cost is visible;
- never unwind or panic across the ABI;
- update `frame.pc` before any fallible or allocating work;
- leave all GC-visible values in the VM stack or other traced VM state;
- report exceptions, calls, and deoptimization through the shared exit
  protocol.

The helper table must be versioned with the generated entry ABI. A change to a
helper signature must invalidate or reject old compiled entries; the first
implementation can simply rebuild the backend and discard its cache.

