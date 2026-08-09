# Default-JIT runtime control checkpoint — 2026-08-09

## Result

Boa now defaults to JIT at both Cargo capability and `Context` policy levels,
while retaining independently verified interpreter controls. The default flip
does not change tier thresholds, generated-code coverage, diagnostics policy,
or remote-script policy.

The public control surface is:

- `Context::default()` and `ContextBuilder::new().build()` enable JIT when the
  engine has its default `jit` feature;
- `ContextBuilder::jit(false)` constructs no backend;
- `Context::disable_jit()` releases an existing backend;
- `boa --no-jit` selects the interpreter;
- `boa_tester run --jit` and `--no-jit` select explicit Test262 arms; and
- `--no-default-features` remains the compiled-out capability boundary.

Test262's effective mode is immutable run configuration, printed in summaries
and serialized as `m` in result records. Old records deserialize as
`interpreter`, matching the historical runner. Comparisons display both modes.
`$262.agent` workers inherit the parent context's resolved mode, preventing a
forced-off main agent from creating a backend on another thread.

## Review findings fixed during verification

Two failures demonstrated why the control matrix is part of the architecture,
not just release ceremony.

First, changing the workspace `boa_runtime` dependency to explicit feature
selection initially stopped propagating `boa_engine/xsum` into the tester.
Both JIT and interpreter full runs consequently lost exactly the ten
`Math.sumPrecise` passes. A compiled-out third arm proved the loss was unrelated
to JIT code generation. The tester and benchmark manifests now preserve their
complete prior engine feature sets explicitly; both modes pass all ten focused
`Math.sumPrecise` files and the full baseline returned to 51,130 passes.

Second, the benchmark runner's JIT samples initially constructed an active
backend before fixture setup. The isolated cold-OSR test observed two native
entries instead of one. Every benchmark mode now constructs its setup context
through the explicit interpreter builder; JIT modes enable the backend only at
their existing measurement boundary. This keeps `interp`, warm JIT, first-call
JIT, and production-threshold cold OSR semantically distinct after the default
flip.

## Verification evidence

Pinned Test262 suite: `5c8206929d81b2d3d727ca6aac56c18358c8d790`.

The final release manifests are identical by test identity:

| Mode | Total | Passed | Ignored | Failed | Panics | Conformance |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| JIT | 53,125 | 51,130 | 1,005 | 990 | 0 | 96.24% |
| Interpreter | 53,125 | 51,130 | 1,005 | 990 | 0 | 96.24% |

The tester's manifest comparator reported zero fixed tests, broken tests, new
panics, or panic fixes. Each JSON record contains its effective `jit` or
`interpreter` mode.

Additional completed gates:

- default-feature engine library: 1,257 passed, 5 ignored, 0 failed;
- no-default-feature engine library: 1,126 passed, 0 failed;
- focused JIT engine suite: 111 passed, 5 ignored, 0 failed;
- benchmark comparison runner with JIT: 7 passed, 0 failed;
- CLI default/opt-out context construction: passed;
- Test262 default, forced-on, forced-off, conflict, and feature-disabled CLI
  behavior: passed;
- Test262 agent JIT-mode inheritance: passed;
- affected CLI/runtime/tester tests in default and feature-disabled builds:
  passed; and
- affected-target Clippy with `-D warnings`, formatting, and diff checks:
  passed in default and no-default-feature configurations.

## Remaining release-quality work

This checkpoint validates the mode flip on the current `aarch64-apple-darwin`
host and establishes exact interpreter/JIT Test262 parity. It does not close
the earlier plan's representative W2 browser corpus, Linux evidence, or the
complete executable-memory security review. Those remain required evidence for
claiming broad state-of-the-art release readiness. The interpreter controls in
this slice are the rollback and differential-debugging path while that marathon
continues.
