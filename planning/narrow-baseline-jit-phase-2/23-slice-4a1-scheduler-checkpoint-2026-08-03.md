# Slice 4A1 scheduler checkpoint and gate refinement — 2026-08-03

Status: Slice 4A1.4 is complete in Boa `55701ef4`. The first guarded numeric
loop region is reachable from the production scheduler, but Gate O and Slice
4A1 remain open until the verification-only 4A1.5 matrix passes. JIT code
generation remains build-time and runtime opt-in.

## Result

The scheduler now observes a canonical latch only after the interpreter has
charged and completed it. At that stable boundary it selects the exact
budget/diagnostic/representation key, reuses a bounded immutable plan, and
synchronously compiles or invokes only a complete cached artifact.

The integration adds:

- a per-frame OSR-closed flag independent of PC-zero admission and CodeBlock
  hotness;
- one scheduler decision per frame after the exact region threshold;
- I32 selection only for exact integer live-ins and F64 selection only for
  JavaScript Numbers, with frame-local fallback on a dynamic type miss;
- immutable cached header, continuation, and instruction-PC metadata used to
  validate every native return before interpreter resumption;
- distinct normal continuation, pre-effect overflow replay, runtime-limit
  break, entry rejection, and invalid-status paths;
- global native entry/deopt counters composed with the schema-8 OSR aggregate;
  and
- dormant interpreter handoff after OSR completes or closes, without changing
  later PC-zero eligibility.

The scheduler clears paired pending exit metadata when it consumes a valid
runtime-limit completion. An invalid native status clears pending native state,
closes the frame's OSR decision, and becomes an engine error rather than a
page-selected continuation.

## Evidence

- The focused `jit_` engine suite passes 66 tests with one ignored performance
  test.
- The full `boa_engine` JIT-feature library passes 1,211 tests with one ignored.
- `cargo check -p boa_engine --lib --no-default-features` and
  `cargo check -p boa_engine --features jit` pass.
- Warning-denying all-target JIT Clippy reports exactly the 16 independently
  recorded pre-existing findings and no Slice-4A1.4-local finding.
- Production-path tests prove first-call compilation/entry, exact cache reuse
  in a later frame, below-threshold interpretation, distinct I32/F64 variants,
  nonnumeric frame fallback without numeric-artifact poisoning, I32 overflow
  replay, loop-limit propagation, matching generous-budget consumption, and
  rejection of a return PC outside immutable artifact metadata.
- An independent read-only scheduler/ABI review found no violation of the
  post-latch boundary, entry guards, one-frame decision, cache bound, or status
  validation. Its stale-pending-state cleanup was incorporated. Its proposal
  to make a nonnumeric observation permanently reject the F64 key was not:
  cross-invocation representation changes are required to remain recoverable,
  and a numeric → string → numeric regression now proves the cached numeric
  artifact survives.

## What this does not prove

One generous instruction budget proves equal successful consumption, not the
ABI's exact exhaustion boundaries. Direct cache-capacity tests do not prove the
production scheduler keeps region state, retained plans, and artifacts bounded
together. The current invalid-PC test calls the backend directly; it does not
yet inject every malformed status through `Context` or prove containment of
all pending-state mismatches. The production wiring has not yet been measured
against the fixed one-shot, micro, engine, and Ligero browser matrix.

Dynamic nonnumeric frames deliberately do not terminally poison a numeric key.
4A1.5 must also verify that repeated dynamic misses and full-table suppression
cannot cause unbounded planner retention or repeated page-amplified analysis.
This is a performance/containment gate, not permission to change type semantics
or add a new representation.

## Slice 4A1.5 — verification-only acceptance

### 4A1.5a — semantic and instruction accounting

For both cold compilation and cache-hit entry, compare JIT-off and JIT-on one
instruction before, at, and one after:

- OSR entry and the first native header opcode;
- `IncrementLoopIteration`;
- every pre-effect arithmetic replay guard; and
- the normal external conditional exit.

Assert the final sink, error/completion class, remaining budget, interpreter
resume PC, and OSR counters. Compilation and entry guards consume no JavaScript
budget; only a current opcode that the interpreter replays may be refunded.
Run unbudgeted then budgeted calls in the same backend and prove the variants
do not alias.

### 4A1.5b — containment, GC, and bounded ownership

Exercise forced GC before and after compilation, immediately before a cached
entry, and after return; recursion and nested frames; dynamic type changes;
stale backend/code/frame/budget guards; every invalid status class and resume
PC; and absent/mismatched pending completions. Invalid native metadata must
never control a resume PC, must clear paired pending state, and must surface the
documented engine error. Before default remote-script use is considered, an
invalid native status must also disable the owning backend for later entries.

Drive 64 exact keys through the production scheduler, then a 65th unseen key.
Prove the unseen key retains no state, plan, or artifact; an already ready key
remains reusable at capacity; I32/F64, diagnostics on/off, and budgeted/
unbudgeted variants cannot alias; and code/time breakers trip only after the
completed attempt while later unseen work is suppressed. Repeated dynamic
entry misses must not create unbounded state or repeated unbounded analysis.

### 4A1.5c — performance and browser rollback gate

Run seven fresh-process, order-alternating diagnostics-off pairs for the
2,000,000-backedge one-shot fixture, one cold call per process. Require the same
sink, exactly one expected OSR compile/entry, zero unexpected deopts, and at
least a 2× median speedup including synchronous compilation. The threshold is
conservative relative to the recorded 3.96× whole-function counterfactual but
rejects a scheduler path that loses most of the selected opportunity.

Repeat the checksummed Decision-checkpoint-A matrix: seven micro controls, the
eligible and ineligible one-shot controls, Crypto, DeltaBlue, Earley-Boyer, and
W0. Headline timings have diagnostics off; a separate diagnostic run must drop
zero records. Every negative/noncandidate row retains its sink and stays within
5% of the paired interpreter median. W0 retains its sink, 387 display items,
258 paint segments, 8,159,754 accounted bytes, PC-zero native entry, and at
least a 20% paired cold-load win against its recorded 30.521% baseline.

If any acceptance group fails, revert or disable only `55701ef4`'s scheduler
edge while retaining the unreachable planner/compiler and record the failed
evidence. Do not proceed to 4A1.R, Decision checkpoint B, or another execution
ABI. Passing 4A1.5 closes the first-shape Gate O only; it does not satisfy W2 or
authorize default JIT or remote-script execution.

## Scheduled refactor

After 4A1.5 passes, Slice 4A1.R consolidates exactly one repeated seam among
typed entry keys, materialization emission, or exit/status mapping in a
separately revertible behavior-neutral commit. It cannot change thresholds,
eligibility, cache policy, diagnostics, or scheduler behavior. Re-run the
focused JIT suite, full feature/no-feature engine gates, and strict lint before
Decision checkpoint B selects or defers any second ABI.
