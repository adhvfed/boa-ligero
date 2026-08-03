# Slice 4A1.R loop-exit contract refactor — 2026-08-03

## Result

The separately revertible behavior-neutral refactor is complete in Boa
`6dc6aa07`. It changes no native status encoding, generated code, threshold,
opcode eligibility, representation, cache/admission policy, diagnostic schema,
or scheduler ownership. Gate O remains passed. Decision checkpoint B is next;
no second execution ABI or default-enablement change is authorized by this
refactor.

## Refactored seam

`JitBackend::invoke_loop_region` previously interleaved three concerns:

1. decoding the native status and the legacy break bit;
2. validating pending completion state, immutable cached PCs, exit kind/reason,
   and the materialized current frame PC; and
3. accounting/diagnosing the accepted exit and mapping it to the scheduler.

It also duplicated exit accounting and diagnostic emission between runtime-
limit breaks and interpreter-resume exits.

Boa `6dc6aa07` introduces a private `ValidatedLoopExit::{Break, Resume}`
contract. One pure classifier now accepts an exit only when status, pending VM
state, cached metadata, and current-frame materialization agree. A single
post-validation path records the exact exit and maps the typed result to the
existing `JitLoopScheduleAction`. Invalid metadata still marks the backend
compromised and is unwound by the unchanged VM-owned containment path.

The generated entry ABI and public/runtime-visible APIs are unchanged.

## Semantic and containment verification

- `cargo fmt --all -- --check`: pass;
- OSR scheduler/containment filter: 15 passed;
- loop planner/compiler/cache filter: 16 passed, one perf test ignored;
- complete focused JIT filter: 73 passed, one perf test ignored;
- full JIT-feature engine library: 1,219 passed, one perf test ignored;
- feature-disabled engine check: pass;
- JIT-feature engine check: pass;
- strict all-target JIT Clippy: exactly the same 16 recorded pre-existing
  findings, with no finding in the refactored file.

The passing filters include the exhaustive cold/cache-hit instruction-budget
sweep, loop-limit matrix, 14 malformed native status/pending-state classes,
backend-disable/recovery path, stale entry guards, forced GC, nested/recursive
frames, exact cache variants, and production 64+1 ownership/reuse.

## Earley-Boyer regression sentinel

- Boa: `6dc6aa074042f4c06e17d9a4f2bf8414ba3cc404`;
- release runner SHA-256:
  `664db36d29b97f8b1fb57168ed778dda462f3782371f61f4e0cdf7b4dea0ac8c`;
- fixture SHA-256:
  `0e614bfc92b01d3fb44cfa55e18ba8fd9f84c0a4b075b1f8ed73e88e1af31ce2`;
- five alternating fresh-process pairs, one timed run, no warmup, diagnostics
  disabled;
- interpreter median: `12.984191875 s`;
- JIT median: `13.540789417 s`;
- delta: `+4.286732%`;
- every sample: matching accumulator, zero compilations, zero native entries,
  zero scheduler-call exits, and zero deoptimizations.

The result remains inside the fixed 5% negative-workload ceiling and is
slightly better than the pre-refactor +4.412% median. It is still close enough
to remain a mandatory sentinel rather than evidence of general parity.

```text
interp: 13045085667 12954853625 12938730583 12984191875 12999964333
jit:    13499867125 13540789417 13533005000 13623402208 13697163125
```

## Clean W0 structural and diagnostic gate

Ligero `9fa88f25` was built from a detached clean worktree against Boa
`6dc6aa07` into a fresh target directory. The user's main-worktree edits were
not part of the accepted binary.

- release Ligero SHA-256:
  `f8fbf57e308b9de995e9d04cb857a92d08765500ae12a6111e4af4b0a0106c74`;
- W0 SHA-256:
  `86bd2f5d96c7afcf06c291851ebba785a1250dd038bdc09604ba728c24e9496b`;
- interpreter structural sample: `61.315833 ms`;
- JIT structural sample: `31.163334 ms`, one PC-zero compilation, 1,000
  aggregate native entries, zero deopts and scheduler-call exits;
- separate schema-8 capped diagnostic sample: `31.529208 ms`, 387 display
  items, 258 paint segments, 8,159,754 accounted bytes, and zero drops in all
  six record classes;
- exit records: 999 PC-zero normal returns and one nonzero-PC loop continuation;
- OSR aggregate: one compile/entry/continuation, zero entry rejection/deopt,
  and 3,240 accounted loop-code bytes.

The unchanged fixture writes checksum `499500000` to visible output and
`body.dataset.checksum`. The clean build and both accepted samples preserve the
Gate-O structure and entry taxonomy.

## Handoff

Decision checkpoint B must now re-run the fixed diagnostics-off headline
matrix and separate zero-drop profiles to rank the remaining call, storage,
region-stitching, and admission costs after OSR. It is a measurement/selection
checkpoint, not permission to implement whichever ABI was expected before the
profile. Gate D default enablement remains downstream of representative W2,
PC-zero containment, supported-platform, security, and release rollback gates.
