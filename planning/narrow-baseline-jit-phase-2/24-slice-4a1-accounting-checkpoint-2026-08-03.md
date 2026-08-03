# Slice 4A1.5a semantic/accounting checkpoint — 2026-08-03

Status: complete in Boa `92acfa22`. Slice 4A1 and Gate O remain open until the
containment/lifetime gate (4A1.5b) and performance/browser gate (4A1.5c) pass.
JIT code generation remains build-time and runtime opt-in.

## Result

The production OSR path now has a fresh-context interpreter differential for
every instruction budget from zero through one instruction beyond successful
completion. The sweep runs in both cold-compile and cache-hit modes and proves:

- the same completion or engine error and exact remaining instruction budget;
- synchronous compilation and entry guards consume no JavaScript instruction;
- the cold and cache-hit modes cross the same entry, continuation, and replay
  boundaries;
- every instruction PC in the first loop region can be observed as the exact
  budget-exhaustion PC;
- normal loop exit records the immutable continuation PC;
- I32 Add, Sub, Mul, and Inc overflow records the exact replaying arithmetic
  opcode and deoptimizes once; and
- an unbudgeted artifact and its budgeted counterpart occupy distinct cache
  variants and are reused only in their matching mode.

The loop-limit differential separately compares limits 0 through 4 in cold and
cache-hit modes. It proves the same result/error class as the interpreter and
records the native limit failure at the interpreter-visible post-
`IncrementLoopIteration` PC under the region's nonzero header entry PC.

Loop-region diagnostics now aggregate validated native exits by their actual
nonzero entry PC. Whole-function records retain entry PC zero. The scheduler
records only validated continuation, replay, entry-rejection, and paired
runtime-limit exits; malformed statuses remain outside diagnostics and follow
the existing containment path.

## Evidence

- `cargo test -p boa_engine --lib --features jit jit_`: 68 passed, 1 ignored.
- `cargo test -p boa_engine --lib --features jit`: 1,213 passed, 1 ignored.
- `cargo check -p boa_engine --lib --no-default-features`: passed.
- `cargo check -p boa_engine --features jit`: passed.
- Warning-denying all-target JIT Clippy reports exactly the 16 independently
  recorded pre-existing findings and no Slice-4A1.5a-local finding.
- Both accounting differentials use `Script::evaluate` and a real `Context`;
  neither calls a compiled entry through the direct harness.

The direct compiler harness still owns the negative-zero multiplication case.
The current production scheduler cannot select I32 for that transition after
the interpreted prefix has already produced `-0`, so manufacturing it in the
production differential would test an unreachable representation state rather
than the scheduler contract. A future representation or entry-boundary change
must add a production differential before making that state reachable.

## Next gate

Slice 4A1.5b is next. It must close forced-GC lifetime checks, nested/recursive
frame checks, stale and malformed native-state containment, backend disablement
after invalid native metadata, and the exact 64-key production-scheduler
capacity/breaker matrix. No opcode, representation, threshold, or cache-policy
widening belongs in that slice.
