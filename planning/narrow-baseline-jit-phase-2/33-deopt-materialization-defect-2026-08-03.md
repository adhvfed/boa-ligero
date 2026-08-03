# Deopt materialization defect — 2026-08-03

## Result

An external review found a correctness defect in both native tiers. Both are
fixed in Boa `67ae1673` (loop OSR) and `222d443e` (function tier). The defect
was live in every shipped loop-OSR artifact since slice 4A1 and in the function
tier since the first guard exits. No opcode eligibility, threshold, cache key,
diagnostic schema, or default-enablement decision changes.

## The wrong assumption

Both tiers wrote the comment, and one of them wrote it down explicitly:

> `try_use_var` also validates that every value has a definition on this path;
> an invalid map rejects native compilation.

That is not what cranelift-frontend does. `FunctionBuilder::try_use_var`
returns `Err(UseVariableError::UsedBeforeDeclared)` only for a variable that was
never *declared*. A declared variable with no definition reaching the current
path resolves through `SSABuilder::finish_predecessors_lookup`, which takes the
branch commented "the variable is used but never defined before ... rather than
throwing an error we silently initialize the variable to 0"
(cranelift-frontend 0.130.2, `src/ssa.rs`). The comment in that branch asserts
the situation only arises in unreachable code. Our generated code made it arise
in reachable code, so we consumed a zero and believed it was a real value.

## Loop OSR: what it cost

`LoopRegionCompiler::compile` declares one Cranelift variable per *frame*
register, not per region-defined register. `emit_available_materialization`
then walked the whole variable map and stored every entry that `try_use_var`
accepted — which was all of them. Every register the region never touched was
written back as integer zero through `jit_store_i32`/`jit_store_f64`.

Three live exits used that helper: the integer-overflow deopt, instruction-
budget exhaustion, and the loop-iteration-limit break. The overflow deopt is
observable from JavaScript:

```js
function once(limit, tag) {
  Math.abs(limit);
  let total = 2147483646;
  for (let i = 0; i < limit; i++) { total = total + 1; }
  return tag;
}
once(2, 'keep')
```

returned `0`, not `'keep'`. The other two exits raise engine errors, so their
frame state is not reachable from script; they shared the same defect.

The planner already classified clean-exit values as `NativeValue` or
`PreservedVmValue` and refused an exit value it could not prove
(`LoopPlanRejection::UnprovenValue`). The mid-region exits simply never
consulted it.

## Loop OSR: the fix

`LoopRegionPlan` now carries a per-region-instruction write-back set,
`available[i] = live_in[i] ∩ (entry_registers ∪ defined)`, and the compiler
emits exactly that set. It is exact, not conservative, in both directions:

- a register live at instruction `i` but never defined by the region has no
  native value to write, and the VM frame already holds the right one;
- a register the region defines but that is dead at `i` cannot be observed by
  the interpreter resuming there;
- every register in the set provably has a definition reaching `i`. If it were
  defined on only some paths, the undefined path would make it live at the
  region entry, which would have placed it in `entry_registers` and given it a
  guarded prologue load.

All three exits resume the interpreter at the same bytecode they left, so one
set per instruction index covers all of them. The budget-refund ordering on the
overflow path is untouched.

## Function tier: what it cost

`emit_guard_deopt` iterated `self.dirty`, the set of registers natively defined
anywhere in the body in emission order. A guard exit reached on a path that
branched around a definition still saw that register in `self.dirty`, resolved
`try_use_var` to zero, and stored it.

The review expected this to be unreachable, on the grounds that `self.dirty`
only holds registers defined somewhere and `use_register` rejects boxed
registers. It is reachable. Register numbers are reused, and the kind analysis
tracks the *current definition*, so a register can be object-typed at one
definition and natively numeric at a later one. At the exit the register carries
the numeric kind, passes the boxed check, and is in `self.dirty`:

```js
function pick(subject, iterations, value) {
  let width = subject.b;
  for (let i = 0; i < iterations; i++) { subject = 1; }
  let doubled = value + value;
  return subject;
}
```

With `iterations = 0` and an overflowing `value`, this returned `0` instead of
the object.

## Function tier: the fix, and the approach that failed

A static must-reach analysis over the instruction CFG was implemented first,
rejecting compilation where may-reach and must-reach disagreed. It was
discarded: loop-carried temporaries are defined on some paths only, so the
plain integer accumulator loop — the tier's headline shape — stopped compiling.
Twenty-one JIT tests failed. Making it precise enough would have required a
full liveness analysis over the whole function-tier opcode set, including the
stack effects of `PushFromRegister` and `Call`.

Each register instead carries a definedness flag that the same control flow
keeps in step, and the guard exit passes it to
`jit_store_i32_if_defined`/`jit_store_f64_if_defined`. Nothing is refused and
nothing is guessed. The two flag constants are defined once in the entry block,
so Cranelift's "all predecessors yield the same value" path drops the block
parameter wherever definedness agrees across a join; the flag becomes a real
phi only where the paths genuinely disagree. Definitions the lowering could not
keep in a Cranelift variable write the VM frame directly and clear the flag, so
a stale native value is never flushed over them.

One residual is knowingly left in place and is not new: `self.dirty` is
accumulated in emission order, so a register whose only native definition is at
a later instruction reached through a back edge is not written back at an
earlier exit. The flag makes what *is* written correct; it does not widen the
set.

## Verification

- new failing-then-passing tests:
  `context_owned_jit_osr_preserves_untouched_registers_across_overflow_deopt`,
  `context_owned_jit_preserves_branch_skipped_registers_across_guard_deopt`,
  and `jit_loop_planner_excludes_untouched_registers_from_mid_region_exits`
  for the two exits that raise engine errors and so have no script-visible
  frame state;
- focused JIT filter: 98 passed, one perf test ignored;
- full JIT-feature engine library: 1,236 passed, one perf test ignored;
- feature-disabled engine library: 1,138 passed;
- the exhaustive cold/cache-hit instruction-budget sweep and the loop-limit
  matrix both pass unchanged, which is the direct differential cover for the
  two exits the new planner set also narrows;
- `cargo fmt -p boa_engine -- --check`: pass;
- JIT-feature Clippy: the same pre-existing findings as before the change, none
  in the changed code;
- `jit_loop_perf` release measurement: interpreter `22.503791 ms`, JIT
  `2.27325 ms`, ratio `0.101`, against a pre-change `23.089959 ms` /
  `2.547041 ms`, ratio `0.110`. The definedness flags cost nothing measurable
  on this shape.

## What this says about the process

The existing suite had a test for preserving an untouched register across the
*clean* loop exit (`jit_loop_planner_preserves_untouched_exit_values_in_vm_registers`)
and tests for each mid-region exit's status, PC and accounting. It had no test
that a non-numeric register survives a mid-region exit. The exit taxonomy was
covered; the frame contract at those exits was not. Both tiers also had a
comment asserting a third-party library guarantee that nobody had checked
against the library's source.
