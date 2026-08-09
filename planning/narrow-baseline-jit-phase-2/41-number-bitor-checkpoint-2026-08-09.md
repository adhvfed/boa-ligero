# Number `BitOr` checkpoint — 2026-08-09

Status: accepted. Commit `dca5f7fc` implements the exact Number conversion
contract selected in the preceding design review.

# Result

Whole-function bodies containing `BitOr` now select the `f64` representation.
All preceding arithmetic therefore retains JavaScript `Number` rounding and
overflow behavior. At the bitwise operation, a leaf helper applies Boa's
canonical `f64_to_int32` implementation to each operand, computes the `i32`
OR, and returns the exactly representable result as `f64`.

Existing argument and binding guards prove that operands reaching the helper
are Numbers. Strings, objects, symbols, and BigInts deopt before effects and
are handled by the interpreter's coercion and exception machinery.

Loop-header OSR continues to reject `BitOr` explicitly. This preserves that
compiler's narrower entry representation and keeps its diagnostic blocker
truthful even though a later PC-zero call can use the whole-function artifact.

# Correctness evidence

Permanent tests cover:

- signed and unsigned 32-bit boundaries, fractions, signed zero, NaN,
  infinities, and values above the safe-integer boundary;
- a rounded large multiplication for which wrapping `i32` multiplication
  would return a different result;
- string and object coercion, including the exact observable `valueOf` count;
- BigInt OR and the mixed BigInt/Number `TypeError` through deoptimization;
- forced collection before object coercion;
- exact guard-fallback instruction-budget parity;
- unchanged denied-loop diagnostics and shim-failure containment using a
  still-unsupported `BitAnd` sentinel.

After the final changes:

- `cargo test -q -p boa_engine --features jit --lib`: 1,254 passed, 5 ignored;
- `cargo clippy -p boa_engine --features jit --lib --tests -- -D warnings`:
  passed;
- `cargo fmt --all --check`: passed;
- `git diff --check`: passed.

# Performance gate

Seven paired diagnostics-off release-process samples of `int-arith`, three
timed calls after six warmups, produced these nanoseconds-per-run medians:

| execution | median |
| --- | ---: |
| interpreter | 51,691,514 |
| JIT with Number `BitOr` | 19,448,875 |
| JIT before this slice | 51,878,222 |

The accepted implementation is 62.37% faster than its paired interpreter and
62.51% faster than the preceding JIT median, a 2.67× improvement. Every warm
sample returned the exact `-2034985248` sink, installed one 1,104-byte native
artifact, entered it eight times, and recorded zero deopts and scheduler call
exits.

Cold compilation plus execution ranged from 18.14 to 21.30 ms in the paired
gate and also returned the exact result, well below the approximately 49–53 ms
interpreter calls.

# Next refinement

The generated loop performs three Rust helper calls per iteration. A native
floating-point control with no conversion helpers runs much faster, so helper
transition cost remains material. The next bounded refinement may translate
the already-tested bit-level `f64_to_int32` algorithm into Cranelift integer
operations and branches. It must prove bit-for-bit equivalence on the same
edge corpus and retain the helper implementation as a differential oracle
during development.
