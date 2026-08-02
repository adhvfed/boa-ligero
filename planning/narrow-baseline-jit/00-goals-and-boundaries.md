# Goals, boundaries, and gates

## Problem statement

Boa's interpreter improvements have removed several avoidable costs, but the
remaining gap is structural: hot JavaScript still pays bytecode fetch, operand
decode, handler dispatch, VM frame access, and generic value checks for every
operation.

The current Cranelift integration does not remove that cost yet. Its generated
function calls an opcode shim for each instruction and checks the resulting
program counter. It proves the VM/JIT boundary, but it is not a useful steady-
state tier for hot code.

The current microbench harness should be rerun before setting a new numerical
baseline. The checked-in baseline reports Boa at 3.43x Node `--jitless` on its
fair subset, while recent local measurements still show roughly 4–5x gaps on
ordinary calls, methods, and global updates. These are interpreter comparisons;
full-JIT Node is intentionally not the first target.

## Primary goal

Build a narrow baseline tier that is measurably faster than the interpreter on
hot ordinary loops and functions, while preserving the interpreter as the
complete semantic implementation.

The first tier is successful when it can do all of the following:

1. Count calls and loop backedges without adding measurable overhead when JIT is
   disabled.
2. Compile and cache an eligible `CodeBlock` only after it is hot enough to
   amortize compilation.
3. Execute selected operations as native Cranelift IR rather than calling the
   per-opcode shim.
4. Preserve `vm.stack`, `CallFrame`, environments, return values, pending
   exceptions, and runtime limits at every exit.
5. Return to the interpreter for unsupported operations, failed guards,
   ordinary/host calls not handled by the current tier, exceptions, yields, and
   runtime-limit events.
6. Demonstrate a sustained speedup on representative Boa-vs-Boa warm runs,
   then improve the Node `--jitless` comparison without hiding compilation cost.

## In scope for the first tier

- ordinary, non-generator, non-async JavaScript functions;
- one `CodeBlock` per compiled function;
- simple structured control flow and backward loops;
- primitive register values and local temporaries;
- numeric arithmetic and comparisons with exact guards;
- dense numeric indexed reads;
- monomorphic data-property reads backed by existing shape/slot feedback;
- direct ordinary-function calls after target feedback exists;
- explicit deoptimization and interpreter resumption;
- optional execution through the existing `jit` feature.

## Explicit non-goals

Do not include these in the first tier:

- a full optimizing compiler, inlining heuristic framework, or escape analysis;
- generators, async functions, async generators, or suspension inside native
  code;
- proxies, bound functions, native functions, `eval`, `with`, or arbitrary host
  callbacks on a native fast path;
- direct manipulation of Boa's private object/property layout from generated
  code before a GC-safe ABI exists;
- a new garbage collector or a second independent GC root set;
- replacing the interpreter's bytecode or making JIT availability mandatory;
- cross-realm or process-wide code sharing;
- a generic interpreter call-site IC. The JIT may use private feedback for
  generated-code guards, but the previous interpreter-level experiment is not
  a design to revive.

## Semantic boundaries

The interpreter remains the source of truth for all behavior. A native path may
run only while these conditions hold:

- the current frame is the frame for which the native entry was compiled;
- the current bytecode and register layout are unchanged;
- every type, shape, element-kind, and call-target assumption has a guard;
- all JavaScript-visible state has been materialized before a helper can
  allocate, call user code, throw, or trigger GC;
- the current bytecode position is known at every exit;
- a runtime instruction budget or loop limit is charged with the same semantics
  as the interpreter.

If any premise is uncertain, exit before the operation and let the interpreter
execute it. A slower fallback is acceptable; a partially applied JavaScript
operation is not.

## Performance gates

Use two controls for every measurement:

1. the same Boa build with JIT execution disabled; and
2. a warm JIT run with compilation time reported separately.

Do not call a change a win based on a single cold invocation. The initial
working gates are intentionally modest and can be tightened after the first
native loop exists:

- primitive loop lowering: at least 1.5x faster than the interpreter after
  warm-up on both integer and floating-point loop benches;
- dense-array/property lowering: at least 1.3x faster on the matching-shape
  bench, with no meaningful regression on mismatching shapes;
- direct-call lowering: at least 1.5x faster on a hot ordinary-call bench;
- compilation overhead: publish cold and warm numbers, and do not enable
  automatic tiering until the chosen threshold wins over the complete workload
  rather than only the hot loop body;
- correctness: zero mismatches in the differential and deoptimization suites.

The gate is comparative, not absolute. Machine noise, Cranelift version, and
the exact benchmark corpus can move the numbers; the control and the workload
must stay fixed.

## Risk tripwires

Pause and investigate instead of adding more lowering if any of these occur:

- a deoptimization requires reconstructing a value that was not materialized;
- generated code holds a raw GC/object/shape pointer across a helper call;
- a helper can panic across the C ABI;
- a loop can run without charging instruction or loop budgets;
- a shape or call-target guard relies on an unchecked raw address;
- a change improves one synthetic loop but regresses cold scripts or real
  workloads;
- the compiler needs an exception-specific or generator-specific frame model to
  support an otherwise optional instruction.

These are architecture problems, not tuning opportunities.

