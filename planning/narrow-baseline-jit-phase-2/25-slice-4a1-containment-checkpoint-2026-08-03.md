# Slice 4A1.5b containment/lifetime checkpoint — 2026-08-03

Status: complete in Boa `44d45ca3`, `68d795fd`, `e37f2398`, and `e34f8530`.
Slice 4A1 and Gate O remain open until the fixed performance/browser gate
(4A1.5c) passes. JIT code generation remains build-time and runtime opt-in.

## Result

The production loop-OSR scheduler now fails closed when a generated loop entry
returns state that does not match its immutable cached metadata. Invalid state
marks the backend compromised, clears both pending-native slots, unwinds the
active frame through the normal engine-error path, and prevents the context
from reinstalling that backend. A later call continues in the interpreter.

Production-path tests cover:

- forced GC before the cold compile, between compiled entries, and after a
  native return, with one compilation and three successful entries;
- a recursive wrapper around the OSR loop, visible nested-frame preservation,
  and the uncatchable recursion limit without disabling a healthy backend;
- exact generated-entry rejection before effects for a stale backend ID,
  wrong header PC, wrong `CodeBlock`, dynamic representation, and wrong budget
  mode;
- all 64 retained region keys reached through `Script::evaluate`, allocation-
  free suppression of the 65th key across state/plan/artifact maps, and native
  cache-hit reuse of a retained key at capacity; and
- 14 malformed return classes injected into a real cached loop artifact:
  untagged and undecodable statuses, every forbidden tagged exit kind, invalid
  entry-rejection/continuation/deopt metadata, and absent or mismatched paired
  runtime-limit state.

The malformed table proves the documented `PanicError` remains an uncatchable
engine error rather than page-visible JavaScript behavior. Every row restores
the base frame, clears pending state, removes the compromised backend, and
then successfully evaluates the same function in the interpreter.

## Evidence

- `cargo test -p boa_engine --lib --features jit jit_`: 73 passed, 1 ignored.
- `cargo test -p boa_engine --lib --features jit`: 1,218 passed, 1 ignored.
- `cargo check -p boa_engine --lib --no-default-features`: passed.
- `cargo check -p boa_engine --features jit`: passed.
- Warning-denying all-target JIT Clippy reports exactly the 16 independently
  recorded pre-existing findings and no Slice-4A1.5b-local finding.

The capacity and malformed-status tests both use the context-owned production
scheduler through `Script::evaluate`; they do not invoke the loop compiler
through its direct harness.

## Security boundary and non-claim

This checkpoint closes malformed-state containment for the new nonzero-PC loop
OSR entry. It does not certify every native entry ABI. The older PC-zero whole-
function scheduler still relies on `expect`-based pending-completion invariants
and a legacy untagged status protocol. Hardening that separate boundary must be
reviewed and tested before any default-JIT or default-remote-script decision;
it is not silently folded into this verification-only OSR slice.

## Next gate

Slice 4A1.5c is next. Run the fixed fresh-process performance/browser matrix
without changing thresholds, eligibility, representations, cache policy, or
the execution ABI. Its measurement preflight must isolate the production-
threshold OSR call from the existing runner's earlier threshold-1 PC-zero
control and expose exact OSR counters. A failure rolls back or disables only
the 4A1.4 scheduler edge. A pass authorizes the separately revertible behavior-
neutral 4A1.R refactor, not default JIT enablement or a second execution ABI.
