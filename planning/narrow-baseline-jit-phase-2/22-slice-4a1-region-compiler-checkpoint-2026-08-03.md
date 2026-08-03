# Slice 4A1 region compiler checkpoint — 2026-08-03

Status: Slice 4A1.3 complete in Boa `c2885afe`. The generated loop artifact is
executable only through a test-only direct harness; no production scheduler
path can look it up or invoke it. Slice 4A1.4 is next.

## Result

The region compiler is separate from the PC-zero whole-function compiler and
accepts only a fully proven `LoopRegionPlan`. Before declaring machine code it
re-runs the pure planner against the live `CodeBlock` and requires the entire
typed plan—including instruction PCs, entry map, exit map, representation, and
variant key—to match exactly.

The compiler then:

- emits one uniform `I32` or `F64` artifact for the exact typed loop key;
- checks backend generation, code identity, header PC, construct mode, budget
  mode, register count, and register range before reading a native live-in;
- guards each planned VM register's numeric representation before using an
  unchecked native load;
- lowers only the planner allowlist: constants, moves, numeric add/subtract/
  multiply/increment, canonical comparisons and jumps, and loop-limit charge;
- materializes the exact path-specific native values before returning a fixed
  metadata-validated `Continuation(LoopExit)` PC;
- materializes all currently available native state before budget, loop-limit,
  or pre-effect arithmetic deoptimization exits; and
- refunds exactly the current bytecode charge only when the interpreter must
  replay that pre-effect bytecode.

Successful artifacts enter the already bounded loop cache and mark the exact
region `Ready`. A cold key cannot compile. Planner drift or lowering failure is
terminal, source-free, and uncached. The 64-key state cap and post-attempt code
byte/time circuit breakers from Slice 4A1.2 remain the ownership boundary.

## Correctness finding

The direct I32 harness exposed an existing whole-function JIT defect: integer
multiplication represented JavaScript `0 * -1` as positive zero. JavaScript
requires negative zero. Both compilers now treat a zero product with differing
operand signs as an I32 representation miss and replay the multiplication in
the interpreter. This preserves the observable sign without widening native
representations.

## Evidence

- Six direct compiler tests pass: fractional F64 continuation, strict entry
  guards, finite-budget and overflow replay, negative-zero multiplication,
  complete-plan revalidation failure, and exact hot-key admission.
- The focused JIT suite passes 66 tests with one benchmark ignored.
- The full `boa_engine` JIT-feature library suite passes 1,204 tests with one
  benchmark ignored.
- `cargo check -p boa_engine` and `cargo check -p boa_engine --features jit`
  pass.
- Warning-denying all-target Boa Clippy reports exactly the 16 independently
  known pre-existing findings and no new finding from Slice 4A1.3.
- `git diff --check` passes.

## Remaining boundary: Slice 4A1.4

Production still cannot enter the artifact. Slice 4A1.4 may add only the exact
post-backedge scheduler edge reviewed in the ABI: observe the already charged
canonical latch, compile synchronously, invoke a complete cached artifact, and
close further attempts for that frame without disturbing dormant dispatch or
ordinary PC-zero entry.

The next slice must not widen opcode eligibility, add mixed-mode SSA, box the
frame, or infer continuation PCs dynamically. Entry rejection must remain
pre-effect, and every returned PC must remain owned by the immutable artifact.
After scheduler wiring, Slice 4A1.5 runs the full differential and workload
matrix; Slice 4A1.R then pays the scheduled behavior-neutral refactor before a
second execution ABI is considered.
