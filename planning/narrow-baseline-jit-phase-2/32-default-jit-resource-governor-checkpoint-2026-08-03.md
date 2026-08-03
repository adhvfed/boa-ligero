# Default-JIT resource governor checkpoint — 2026-08-03

## Result

The two D1 implementation commits are landed, but D1 is not closed. Boa
`42623234` bounds retained function-entry state, decoded PC-zero bodies, legacy
call-target feedback, and unseen loop planning. Boa `2888c024` adds unified
payload/time accounting, schema-9 source-free counters, and immediate backend
retirement when a finalized artifact would cross the retained-payload bound.
Ligero `72568c9a` projects the new instruction-limit blocker and `dfdc0714`
projects all schema-9 resource counters.

The implementation does not change opcode eligibility, hotness thresholds,
remote-script policy, the JIT build default, or the runtime default. D0 and the
D1 process-level memory/cold-start gate remain open, so the tier remains
explicitly opt-in.

## Landed ownership and fallback

One backend generation now owns:

- at most 192 exact function-entry states and 64 exact loop states;
- no PC-zero admission body beyond 1,024 decoded instructions;
- at most 1,024 legacy call-target observations;
- at most 8 MiB retained aggregate `code_buffer()` payload and 1 MiB retained
  loop payload;
- post-attempt 10 ms single-attempt and 100 ms cumulative compilation
  circuit breakers.

Ready artifacts are looked up before capacity/time/payload suppression, so
ordinary breaker closure does not discard useful compiled work. An artifact
whose completed payload would cross either retained-payload bound is neither
cached nor invoked. The backend enters `RetiringResourceOverrun`, returns the
distinct `RetireAndInterpret` scheduler outcome, and the outer VM drops the
owning `JITModule` before resuming the interpreter from the current VM state.

The resource counters are fixed-size and source-free. Schema 9 reports
function-capacity, oversized-body, terminal-failure-hit, call-target-capacity,
payload, cumulative-time, slow-attempt, and payload-retirement events together
with retained payload bytes and observed compilation nanoseconds. Ligero
projects those numeric counters without page source, URL, names, values,
identities, or pointers.

## Verification completed

The implementation checkpoint passes:

- focused JIT tests: 79 passed, one performance benchmark ignored;
- full JIT engine library: 1,225 passed, one benchmark ignored;
- interpreter/default engine library: 1,138 passed;
- `cargo check -p boa_engine --lib --no-default-features`;
- `cargo fmt --all -- --check` and `git diff --check`;
- warning-denying JIT Clippy with exactly the recorded 16 pre-existing
  findings and no local finding;
- Ligero `cargo check -p script --features jit`;
- Ligero's focused explicit bounded/source-free diagnostic projection test.

The focused Ligero test also exercised the intended slow-attempt path. On an
unoptimized build, its first valid OSR compile exceeded 10 ms; schema 9 then
reported slow-attempt suppression for unseen function work while the script
completed through the interpreter. The test now accepts either a PC-zero
native record or an OSR compilation and requires retained-byte/time counters.
This is debug-build safety evidence, not release performance evidence.

## Open D1 acceptance work

D1 remains open until all of the following are checked in:

1. Run the required seven fresh-process, order-alternating interpreter/JIT
   pairs for the no-artifact, 192-function/64-loop saturation, and maximum-
   diagnostics fixtures. Record raw peak RSS, cold time, payload, compile time,
   cache reuse, hashes, OS, and allocator. Enforce the +5% no-artifact median
   and 64 MiB per-pair RSS-delta rollback bounds.
2. Add the combined capacity/variant matrix in both fill orders. It must prove
   coexistence of 192 function and 64 loop keys and non-aliasing of budgeted,
   diagnostic, entry-PC, and numeric-representation variants.
3. Saturate every detailed diagnostic class at the 4,096 hard cap and assert
   each index map remains no larger than its vector, all serialized fields are
   source-free, and the process-level RSS gate still passes.
4. Resolve the recoverable compilation-failure contract. The current
   production compiler treats Cranelift declaration/definition/finalization
   failures as fatal panics, while `FunctionEntryState::TerminalFailure` is a
   test-only ownership state. Either make production compilation return a
   contained failure that occupies one already-admitted key without retry, or
   move that failure class explicitly into D2's fail-closed backend-retirement
   contract and remove the unsupported D1 claim.
5. Add a structural/source audit proving production code has no direct raw
   emitter outside the governed native, shim, and loop producers. The two old
   convenience emitters are now private test-only seams, but the invariant
   should fail automatically if another production escape hatch appears.

The default flip is not scheduled for implementation by this checkpoint. D1
must close first, then D2–D4 must pass against one identified release
candidate. D5 remains a separately revertible policy-only commit with an
explicit interpreter opt-out and a regression proving it does not enable
remote scripts.
