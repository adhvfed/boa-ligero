# Native lowering sequence

This is the order in which the selected operations should become native. Each
stage has a narrower semantic surface than the next one and should land only
after its own performance and differential gates pass.

## Stage A — native frame-local primitives

Start with code that does not inspect object layout or call user code:

- numeric constant stores;
- `Move` between values already represented in the region;
- primitive `i32`/`f64` loads with a guard;
- `Add`, `Sub`, and `Mul` for proven numeric representations;
- numeric comparisons;
- simple conditional branches and backward edges;
- return-value materialization and the VM-owned return transition.

This stage should make a loop such as the following useful without relying on
the opcode shim:

```js
function sum(n) {
  let total = 0;
  for (let i = 0; i < n; i++) total += i;
  return total;
}
```

### Numeric rules

For an `i32` specialization:

- guard both operands using the engine's exact integer classification;
- use checked operations where JavaScript would leave the `i32` domain;
- deopt before the operation when the result needs a different representation;
- preserve the generic path for `+` with strings/coercion and for any unknown
  operand.

For an `f64` specialization:

- guard both operands as numbers;
- preserve IEEE-754 NaN and signed-zero behavior;
- do not silently turn a number into an integer representation;
- keep division, remainder, exponentiation, and other edge-heavy operators on
  the helper/interpreter path until their exact semantics are tested.

The first arithmetic milestone does not need every operator. It needs a small
subset with a clean guard and a visible speedup.

## Stage B — dense numeric element loads

Use the existing `ElementIC` feedback as the input to JIT specialization:

```text
receiver shape + live guard
integer key + non-negative/bounds guard
dense storage kind (DenseI32/DenseF64/DenseElement)
hole/prototype semantics guard
=> load or deopt/fallback
```

The first implementation should call a purpose-built helper that performs the
whole checked load and writes the result to the target register. This avoids
duplicating private `PropertyMap`/dense-storage layout in Cranelift and gives a
safe measurement of the guard and helper boundary.

Only after that helper wins should the compiler emit direct storage loads. A
direct load needs a stable object-layout contract, bounds checks, shape liveness,
and a rule for what happens if a read can invoke a getter or prototype lookup.
Those cases must exit before the direct load.

Target benches:

- `array-numeric-sum`;
- `readonly-indexed-scan`;
- a matching-shape dense-array loop;
- a mutation/hole/prototype negative case.

## Stage C — monomorphic named-property loads

Use `CodeBlock::ic`/`InlineCache` feedback for a data-property load with:

- a live receiver shape;
- a cached slot;
- no accessor invocation;
- no prototype traversal required by the cached result;
- no megamorphic site;
- a receiver class/layout that the helper understands.

The first JIT path should use a helper that rechecks the existing IC contract.
It may return a boxed `JsValue` to the VM stack. The native region can then
continue with an `i32`/`f64` guard if the next operation is numeric.

Do not expose `InlineCache::shape_addr` to generated code without carrying the
same weak-shape liveness proof as the interpreter. The existing compact IC
experiments showed that removing apparently small safety/representation costs
can regress the actual hot path; measure the helper and direct forms separately.

Target benches:

- `property-mono`;
- `property-poly4` as a non-monomorphic control;
- accessor/prototype/shape-mutation tests;
- a method lookup followed by a call once Stage D exists.

## Stage D — direct ordinary-function calls

The bytecode `Call` operand contains an argument count, not a target identity.
Therefore direct calls need a JIT-only feedback record at each call site:

```text
callee identity
target CodeBlock identity
ordinary/non-async/non-generator classification
expected arity / calling convention facts
```

A native call sequence should:

1. load and guard the callee value;
2. guard that the target remains an ordinary JavaScript function;
3. materialize caller registers and operand stack values;
4. use the normal VM frame construction, or a reviewed fast frame-entry helper;
5. enter a compiled callee only when its entry ABI and metadata match;
6. otherwise return a `Call`/`Deopt` exit and let the interpreter dispatch it.

Do not inline the callee in this stage. A direct compiled-to-compiled call is
already a substantial ABI problem; inlining belongs after frame maps,
exceptions, recursion, and GC safepoints are stable.

The generic cases remain exits: proxies, bound functions, native functions,
construct calls, `eval`, spread calls, async/generator targets, and any callee
whose identity guard misses.

Target benches:

- `fn-call-flat`;
- `method-call-mono` after property feedback is available;
- a mismatching target and polymorphic-call control;
- recursive ordinary calls, where every frame transition must remain visible
  to stack traces and runtime limits.

## What not to fuse yet

Avoid early fusion of large bytecode sequences that include:

- property access followed by arbitrary call dispatch;
- allocation plus object initialization;
- coercion, string concatenation, or user-defined conversion;
- exception handlers or finally blocks;
- host callbacks or promise scheduling.

These may eventually be profitable, but their deoptimization state is harder
than their apparent instruction count suggests. First prove the individual
guards and transitions.

