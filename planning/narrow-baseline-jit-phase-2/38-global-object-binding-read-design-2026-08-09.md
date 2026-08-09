# Global-object binding-read design — 2026-08-09

Status: selected for a bounded prototype. This is a binding/cache slice, not a
general environment or global-operation compiler.

# Measured problem

On commit `d76d0b9e`, `fn-call-flat` performs three million diagnostic-observed
calls in the warm sample. Every call is to the same ordinary function after
the first observation, but the 22-instruction loop caller creates no artifact:
its first blocker is `GetNameGlobal` at PC 52. The 11-instruction callee is
independently denied as a too-small straight-line function.

Seven paired diagnostics-off release-process samples, four timed calls after
ten warmups, produced these nanoseconds-per-run medians:

| execution | median |
| --- | ---: |
| interpreter | 42,524,927 |
| current JIT | 44,407,791 |

The current JIT is 4.43% slower because it observes thresholds and admission
without installing code. Lowering the global-object function read would make
the caller eligible for the ordinary-call continuation landed in `d76d0b9e`.

# Selected contract

Lower only `GetNameGlobal`, with all of these checks on every execution:

1. the current environment reports stable binding locators;
2. the active code block still contains the requested locator and its scope is
   exactly `GlobalObject`;
3. the current realm's global object matches the bytecode inline cache;
4. the cached slot exists and is not an accessor getter;
5. an `i32` or number use matches its selected representation.

On success, copy the current property value into the destination VM register.
Boxed functions and objects therefore remain in traced VM storage; generated
code receives only a boolean success tag. Numeric consumers load the checked
value from that register into SSA, matching the existing global-declarative
contract.

Any failed check is a pre-effect `BindingRead` guard exit at the original PC.
The budgeted artifact refunds that bytecode and the interpreter re-executes
`GetNameGlobal`, preserving lookup, cache fill, accessor invocation,
`ReferenceError`, and dynamic-environment semantics.

# Explicit exclusions

- No global pointer, shape, slot, property value, environment, or binding name
  is embedded in generated code.
- No accessor is invoked in the generated helper.
- `GetNameGlobalAndLocator`, writes, initialization, deletion, and reference
  creation remain unsupported.
- No `with` or eval-poisoned lookup is optimized.
- This does not lower a global read through a missing or cold IC entry; the
  interpreter owns cache population and the next hot entry may use it.
- The helper does not compile or directly invoke the callee.

# Correctness gates

Permanent differential coverage must include:

- a boxed global function read followed by an ordinary call;
- same-shape replacement with a different ordinary function;
- numeric same-representation mutation and representation mismatch replay;
- deletion/missing-property `ReferenceError`;
- accessor properties and prototype slots taking interpreter replay;
- shape invalidation, forced GC, realm separation, and unstable environment
  fallback;
- exact successful and guard-fallback instruction-budget parity;
- zero steady deopts once the global IC is warm and unchanged.

# Performance gate

Retain the slice only if seven fresh paired release-process samples of
`fn-call-flat`, diagnostics disabled, show:

- the exact `500000` result on every individual call;
- at least one native caller artifact and entry;
- zero steady-state deopts and scheduler call exits;
- at least a 10% median improvement over the current JIT median above;
- no regression against the paired interpreter median.

If the native caller plus interpreted-callee trampoline cannot clear that
gate, remove the lowering rather than widening it. Direct compiled-callee
entry would then need to land as the next combined performance unit.
