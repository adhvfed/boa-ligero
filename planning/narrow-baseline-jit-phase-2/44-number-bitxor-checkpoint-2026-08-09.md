# Number `BitXor` checkpoint — 2026-08-09

Status: accepted. Commit `e3e2ac15` implements `BitXor` under the exact Number
conversion contract established for `BitOr`. The sibling `BitAnd` prototype
was rejected and removed.

# Result

Whole-function bodies containing `BitXor` select the `f64` representation, so
preceding arithmetic preserves JavaScript `Number` behavior. A dedicated leaf
helper applies Boa's canonical `f64_to_int32` conversion to both proven-number
operands, computes the `i32` XOR, and returns the exact result as `f64`.

The lowering shares its compiler structure with `BitOr` but keeps a separate
leaf helper, avoiding an operation-tag branch in the hot helper. Existing
pre-effect representation guards replay strings, objects, symbols, BigInts,
and observable conversions in the interpreter. Loop-header OSR continues to
reject all binary bitwise operations under its narrower representation
contract.

# Correctness evidence

Permanent tests cover:

- signed 32-bit overflow and infinity conversion;
- object conversion and its exact observable `valueOf` count;
- BigInt XOR and mixed BigInt/Number `TypeError` through deoptimization;
- forced collection between native warmup and object conversion;
- native artifact and backward-branch evidence;
- exact interpreter parity after a representation-guard replay with a finite
  instruction budget;
- the retained unsupported-`BitAnd` OSR diagnostic and module-failure
  sentinels.

After the final implementation:

- `cargo test -q -p boa_engine --features jit --lib`: 1,256 passed, 5 ignored;
- focused `BitXor`, bitwise-budget, denied-loop, module-failure, and `BitOr`
  regression tests: passed;
- `cargo clippy -p boa_engine --features jit --lib --tests -- -D warnings`:
  passed;
- `cargo fmt --all --check` and `git diff --check`: passed.

# Performance gate

Seven paired diagnostics-off release-process samples of
`array-numeric-sum`, three timed calls after six warmups, produced these
nanoseconds-per-run medians from the final source:

| execution | median |
| --- | ---: |
| interpreter | 32,554,000 |
| JIT with Number `BitXor` | 13,892,555 |
| JIT before this slice | 36,266,208 |

The retained implementation is 57.32% faster than its paired interpreter and
61.69% faster than the preceding JIT median, a 2.34x paired speedup. Every warm
sample returned the exact XOR sink, installed one 2,400-byte artifact, entered
it eight times, and recorded zero deopts and zero scheduler call exits.

The final-source `property-poly4` negative control remains deliberately
uncompiled: its seven-pair medians were 6,627,000 ns interpreted and 6,575,722
ns with JIT enabled. Every warm sample recorded zero artifacts, entries,
deopts, and scheduler call exits. This confirms that removing `BitAnd` removed
the prototype's repeated-deoptimization regression.

# Architecture finding and next boundary

The rejected `BitAnd` prototype localized the next meaningful coverage issue.
The computed-property path can load a boxed object that is subsequently used
as an ordinary-call argument, while the present whole-function compiler's
register plan is primarily numeric. Correctly retaining such a caller requires
role-sensitive value representation, GC-rooted boxed live ranges, and exact
deoptimization materialization.

Direct native-to-native calls are a separate large boundary. During VM
execution the context temporarily owns no accessible JIT backend; exposing a
raw active-backend pointer would permit recursive mutable aliasing and weaken
teardown guarantees. The next call design must instead provide an immutable
active dispatch session or resumable caller entries with generation-validated
targets. Neither boundary is safe as an incremental opcode allowlist change.
