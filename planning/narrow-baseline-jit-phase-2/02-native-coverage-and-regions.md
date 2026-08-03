# Native coverage and region continuity

Phase 1's native compiler is intentionally conservative. Phase 2 should make
the selected hot loops native as a region instead of compiling a shim entry
that quickly falls back because of one surrounding operation.

## Design principle

Do not widen the allowlist from intuition. Use the Phase 2 profile to choose a
small batch of high-frequency blockers, and lower them through the existing
value/materialization model. Unsupported instructions still terminate a native
region at an exact PC; they must never be silently treated as a fallthrough.

The compiler should distinguish:

```text
CodeBlock rejected     => no safe region or malformed metadata
Shim fallback          => no native region was profitable/available
Native region          => selected instructions and edges run in Cranelift
Region exit             => native code returned safely to the VM
```

## Likely first operation batches

The profile should confirm these, but the current microbench shapes make them
the leading candidates:

### Receiver reads

- `This`, which is the most frequent first blocker in the measured engine
  subset and independently blocks the monomorphic-method control;
- an exact boxed read through the VM-owned interpreter operation, including its
  cached-frame and environment-resolution paths;
- a pre-effect representation guard and exact-PC deopt only if a later lowering
  specializes instead of supporting every boxed receiver shape.

This is a VM binding read, not permission to cache a raw object pointer in
generated code. The implementation review must cover strict/sloppy receiver
normalization, lexical `this`, uninitialized derived constructors, primitive
and object receivers, bound/ordinary calls, GC while the value is live, and
finite-budget replay. The measured method control has
`This` at PC 18 after a supported prefix, so first determine whether adding this
operation makes the existing PC-zero whole-CodeBlock entry useful. Do not add a
nonzero entry merely to bypass it.

The [2026-08-03 receiver frontier review](11-receiver-frontier-review-2026-08-03.md)
answers that question: standalone `This` lowering is a no-go. The method next
reaches unsupported `SetPropertyByName`, and even a combined receiver/store
allowlist would leave the 16-instruction straight-line helper below production
admission. Retain receiver semantics and tests for a later useful region; do not
add an opcode solely to move the diagnostic frontier.

### Environment and constant reads

- `GetName`, `GetNameGlobal`, and the corresponding locator/undefined forms
  needed to read immutable or stable global bindings;
- constant and accumulator/register moves that currently break value tracking;
- a guarded binding/version snapshot so a global reassignment, `delete`,
  `eval`, or dynamic environment change deopts before the read.

A binding guard must use VM-owned identity/version information. Do not cache a
raw environment pointer in generated code without a lifetime contract.

This is now the next scheduled design checkpoint. `GetName` blocks every
measured microbenchmark caller, while a safe stable read of the floating-point
control's `N` binding would complete an otherwise-supported native loop. Treat
the call-heavy callers as negative controls: binding coverage is accepted only
if complete-workload time remains neutral or improves.

### Integer representation operations

- bitwise integer operations and shifts when both inputs are proven `i32`;
- `ToInt32`/`|0` fast paths with exact fallback for arbitrary numbers, NaN,
  infinities, negative zero, and out-of-range values;
- increment/decrement and loop induction updates when the result stays in the
  proven representation;
- comparisons and branch conditions needed to connect the loop CFG.

The fast path may use native integer operations only when JavaScript semantics
are identical for the guarded representation. A generic bitwise operation must
not be lowered as a floating-point or wrapping shortcut without a guard.

### Region edges and exits

- connect supported straight-line blocks through validated conditional edges;
- keep backward edges native when all live values have a materialization map;
- emit one explicit exit for the first unsupported operation in a block;
- avoid invoking the opcode shim for instructions already selected for native
  lowering.

Exception-handler ranges, environment-changing operations, calls, and returns
remain explicit boundaries until their dedicated Phase 2 contracts exist.

## Binding feedback snapshot

At compile request time, capture only the facts the native region needs:

```text
binding identity / environment version
binding mutability facts
value representation, if specialized
realm/code-block identity
```

On every native read, guard the snapshot before producing a value. If the
binding can invoke user code or has dynamic lookup semantics, leave it as an
interpreter/helper exit. Materialize all live primitive SSA values before the
guarded helper or deopt.

## Region selection

Before changing the compiler, record an architecture checkpoint: Phase 1
currently caches and invokes a whole-CodeBlock entry beginning at PC zero. If
the selected blocker batch can make that entry useful, extend the existing
compiler first. If profitable work lies behind an unsupported prefix or needs
nonzero entry PCs, approve explicit region identity, entry maps, and exit maps
before lowering more opcodes. Do not smuggle a second compiler architecture in
as an incidental opcode patch.

Prefer a measured hot region over whole-function rejection:

1. decode and validate the complete CodeBlock;
2. identify hot entry/loop/call PCs from the profile;
3. build a region whose entry and exits are bytecode boundaries;
4. lower only the explicit native allowlist;
5. attach coverage and materialization metadata to the compiled entry;
6. return to the interpreter for the first unsupported operation.

The first implementation may still compile a whole CodeBlock when it is
simple, but its metadata should model regions so OSR and later variants do not
need a second compiler architecture.

Before adding binding specialization, identify the existing VM-owned binding
identity and invalidation signal. If Boa has no suitable versioned contract,
the slice must first design and test one with environment mutation, deletion,
direct eval, and realm teardown. A JIT-only counter or raw environment pointer
is not an acceptable substitute.

## Done criteria

- `int-arith`, `float-arith`, and `array-numeric-sum` either show native
  execution through their hot loop or have a measured, documented blocker;
- integer overflow, coercion, negative zero, NaN, and binding mutation all
  deopt before semantic mutation;
- a region exit resumes at the same PC and produces the same sink/error as the
  interpreter;
- native coverage and first-blocker data are visible in the diagnostic stats;
- JIT-off builds and Phase 1 tests are unchanged.

The 2026-08-03 Gate P profile adds a negative criterion: a coverage change must
also preserve or improve the helper-heavy property, flat-call, and method
controls. Native entry count alone is insufficient when boundary overhead can
make an already-native tiny helper slower than interpretation.
