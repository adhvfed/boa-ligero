# Default-JIT runtime control design — 2026-08-09

## Decision

Boa's Cranelift tier becomes a default Cargo feature and a newly built
`Context` owns a JIT backend by default whenever that feature is present.
Operators and embedders retain an explicit interpreter-only path:

- the CLI accepts `--no-jit`;
- `ContextBuilder::jit(false)` constructs no backend;
- `Context::disable_jit()` remains available for an existing context; and
- builds can omit the tier with `--no-default-features` and an explicit list
  of the portable features they require.

This is a policy change requested after the earlier D0--D5 admission plan. It
does not claim that the still-open cross-platform and representative-browser
evidence in that plan has been completed. Those gates remain release-quality
work and the interpreter mode is the rollback/control arm.

## Ownership and propagation

The resolved mode belongs to each `Context`, not to process-global mutable
state. That keeps parallel tests deterministic and lets an embedder run JIT and
interpreter contexts in one process. `ContextBuilder` is the single creation
boundary. The Test262 `$262.agent` host copies the parent context's resolved
mode into worker contexts so `--no-jit` cannot accidentally create a backend
on another thread.

The CLI and Test262 runner expose policy at their own boundaries:

- `boa` uses the release default and `boa --no-jit` forces the interpreter;
- `boa_tester run` uses the build default;
- `boa_tester run --jit` explicitly selects JIT and fails clearly when the
  tester was built without JIT support; and
- `boa_tester run --no-jit` explicitly selects the interpreter.

`--jit` and `--no-jit` conflict. Test262 records and prints the effective mode
so two reports can be compared without guessing which engine path produced
them.

## Compatibility boundaries

The workspace dependency declaration intentionally disables dependency
defaults, so every Boa binary/library opts into JIT propagation explicitly.
The CLI, runtime, and tester default features opt in. WASM and other existing
portable consumers keep their current interpreter-only feature selections.
The benchmark comparison runner must explicitly construct an interpreter
context for `interp` mode; changing `Context::default()` must not contaminate
the control arm.

## Acceptance gates

1. Default-feature `Context::default()` has JIT enabled; builder `jit(false)`
   has no backend and can be re-enabled.
2. A no-default-feature engine build and tests still compile and pass.
3. CLI parsing exposes `--no-jit` only for a JIT-capable build and constructs
   the selected mode.
4. Test262 CLI parsing covers default, forced-on, forced-off, and conflicting
   flags; a feature-disabled binary rejects forced-on.
5. Test262 main and worker contexts use the same resolved mode.
6. The benchmark interpreter arm explicitly disables JIT.
7. Focused and full Test262 suites run in both modes, followed by affected
   tests, Clippy, formatting, and workspace checks.

No opcode widening, threshold change, or cache-policy change belongs in this
slice. If JIT mode regresses semantics, panics, or violates a bound, the fix is
made in a separate behavior commit or the default flip is rolled back.
