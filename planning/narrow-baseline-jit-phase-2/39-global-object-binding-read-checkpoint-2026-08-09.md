# Global-object binding-read checkpoint — 2026-08-09

Status: accepted. Commit `a5ec76e8` implements the fail-closed
`GetNameGlobal` contract selected in the preceding design review.

# Result

The native function tier now reads a warm global-object binding through the
current realm's inline cache. Every execution validates locator stability,
the compile-time `GlobalObject` scope, the current global object's shape and
slot, the absence of a getter, and the requested numeric representation.

The helper copies the current `JsValue` into a traced VM register and returns
only a boolean tag to generated code. It retains no realm, object, shape, slot,
name, or value. IC misses, accessors, dynamic-environment changes, deletion,
and representation changes take a pre-effect `BindingRead` guard exit and
replay the original bytecode.

The implementation also extracts the shared post-copy binding guard/load path
used by global-declarative and global-object reads, and factors the current-
slot lookup shared with ordinary named-property helpers.

# Correctness evidence

Permanent coverage proves:

- a global function binding keeps a loop caller native across ordinary calls;
- same-slot replacement with a different ordinary function is observed
  without target-identity deoptimization;
- boxed function state survives forced collection;
- numeric same-representation mutation is observed on the next entry;
- a numeric representation change deoptimizes and replays correctly;
- an accessor invokes its getter the exact number of interpreter-visible
  times, and deletion produces the authoritative `ReferenceError`;
- a prototype data slot is cached and later reads its updated same-shape value;
- guard fallback matches interpreter results and remaining instruction budget
  exactly.

After the final changes:

- `cargo test -q -p boa_engine --features jit --lib`: 1,251 passed, 5 ignored;
- `cargo clippy -p boa_engine --features jit --lib --tests -- -D warnings`:
  passed;
- `cargo fmt --all --check`: passed;
- `git diff --check`: passed.

# Performance gate

Seven paired diagnostics-off release-process samples of `fn-call-flat`, four
timed calls after ten warmups, produced these nanoseconds-per-run medians:

| execution | median |
| --- | ---: |
| interpreter | 36,577,416 |
| JIT with global-object read | 30,130,979 |
| JIT before this slice | 44,407,791 |

The accepted implementation is 17.63% faster than its paired interpreter and
32.15% faster than the preceding JIT median. Every warm JIT sample installed
one 1,892-byte native caller, entered it 13 times, and recorded zero deopts and
zero scheduler call exits. The cold sample returned the exact `500000` result;
the four-run XOR sink was the expected zero in every timed sample.

The first cold caller entry records one expected deopt while the interpreter
populates the global property IC. Later entries use the guarded current slot.
The small `tiny` callee remains below the production straight-line admission
floor, so this gain comes from native caller continuity while its callee still
uses the interpreter.

# Decision and next boundary

The slice is retained. The flat-call result confirms that native caller
continuity is valuable even before direct compiled-callee entry. The next
call-path decision is now isolated: admit selected small hot leaf callees and
resolve their cached entries through VM/backend ownership, or continue
expanding high-value caller coverage. No executable pointer should be embedded
until backend teardown and cache-state validation are reviewed together.
