# Binding-read and call-boundary review — 2026-08-03

Status: implemented and gated 2026-08-03. Boa `345767c5` lands the call-boundary
denial and global-declarative read; Ligero `2c39eafe` projects schema 4; Boa
`54a109f6` lands the required behavior-neutral refactor.

This review refines Slice 2C after inspecting Boa's binding locator,
environment, realm, VM-register, deoptimization, and admission paths. It also
records the release measurements that selected the design. Final post-refactor
results are in the [Slice 2C closure](13-slice-2c-closure-2026-08-03.md).

## Binding contract selected

The first lowering is deliberately narrower than “name reads”:

- the bytecode is `GetName`;
- the compile-time `BindingLocator` scope is exactly `GlobalDeclarative`;
- the locator index is validated against the current `CodeBlock`;
- each native entry checks `Context::binding_locator_stable()` and reads the
  current value through the active frame's realm and global declarative
  environment;
- the value is copied into a VM register before generated code consumes it;
- an `i32` or `f64` specialization checks the current representation before
  producing an SSA value; a boxed use remains rooted in the VM register.

Generated code retains no environment pointer, binding value, `Gc` pointer, or
cross-realm cache. A mutable binding is read again on every native entry, so a
same-representation reassignment is observed without invalidation. A missing,
uninitialized, unstable, or differently represented value takes a pre-effect
guard exit at the `GetName` PC; the existing exact-budget refund lets the
interpreter replay the operation once and produce the authoritative value or
`ReferenceError`.

This contract is safer and smaller than adding a new binding-version system.
It uses the VM's current locator stability check and environment lookup rather
than treating a JIT-only counter as semantic identity.

## Explicit exclusions

This slice does not lower:

- `GetNameGlobal`, global-object properties, replacement, or deletion;
- stack, function, module, or object-environment locators;
- dynamic lookup through `with` or a poisoned environment;
- direct-`eval`-affected closures;
- writes, initialization, deletion, or environment-shape changes;
- nonzero-PC regions, OSR entries, or compiled calls.

The direct-eval requirement is therefore rejection coverage, not a promise to
optimize eval. The bytecompiler's affected closure bindings use a non-global-
declarative locator and must remain unsupported. Global-object reads need their
own object/property semantics and invalidation review; they are not an
automatic follow-up to this declarative read.

## Call-boundary admission defect

The production rule currently admits a fully native shape when it contains a
validated backward branch, even if its static profile also contains calls. A
call-containing caller cannot continue in native code today: ordinary calls
return to the VM scheduler, and generated code has no native continuation at
the caller's next PC. In the method control, analysis admits the loop-shaped
caller, lowering reaches a boxed `StoreOne` before the first call, and the
backend installs a complete-semantics shim.

Five interleaved release samples after 70 warmups measured the method control
at a 23.40 ms interpreter median and a 25.82 ms JIT median, 10.3% slower, with
one shim compilation and zero native entries. Adding boxed `StoreOne` would
only move the exit to the call boundary and would not create native continuity.

Before the binding lowering lands, production admission must reject any
function-entry body whose static profile contains a call. Record a distinct
source-free `denied_call_boundary` admission reason, install no native or shim
artifact, and retain an explicit test-only override for call-lowering semantic
coverage. This is a temporary admission rule, not a permanent claim that calls
cannot be compiled. Slice 4B may relax it only after the compiled-call ABI can
resume the caller natively and passes Gate K.

## Prototype evidence and acceptance gate

Five interleaved fresh-process release samples, each with 70 warmups and five
timed runs, measured the floating-point control at:

| Mode | Median ns/run | Relative result |
| --- | ---: | ---: |
| Interpreter | 14,928,941 | baseline |
| Prototype JIT | 3,103,325 | 4.81× faster |

Every JIT sample produced one native compilation, 74 native entries, zero
deoptimizations, and the same accumulator. This is strong evidence that the
global-declarative read completes a useful admitted loop rather than merely
moving the first blocker.

Slice 2C closes only when all of the following hold:

1. production call-containing entries report `denied_call_boundary`, compile
   no artifact, and the method, flat-call, and property negative controls are
   within 5% of interpreter medians;
2. the floating-point control retains at least a 2× warm win, one native
   artifact, matching sink, and zero steady-state deopts;
3. mutable same-representation and changed-representation bindings, TDZ,
   direct-eval rejection, realm separation, forced GC, and exact finite-budget
   replay pass interpreter/explicit-JIT/context-tier differential tests;
4. W0 retains its native loop, checksum, paint structure, and cold guardrail;
5. diagnostics distinguish binding guard exits from admission denial without
   retaining source, names, values, URLs, or pointers.

## Required follow-up refactor

The call-boundary admission correction and the binding-read lowering are two
behavior slices. Before Decision checkpoint A, land one separately revertible,
behavior-neutral refactor covering the duplicated generated-helper declaration
or VM-register materialization plumbing exposed by the slice. The refactor must
re-run the same float positive control, call-heavy negative controls, focused
JIT suite, feature-disabled checks, formatting, and affected warning-denying
Clippy. Do not use the refactor to widen binding scopes or opcode coverage.

Boa `54a109f6` completes this checkpoint by borrowing the generated helper
table throughout emission and removing its unused compiler copy. It changes no
helper signature, opcode allowlist, diagnostic reason, or generated exit path.
