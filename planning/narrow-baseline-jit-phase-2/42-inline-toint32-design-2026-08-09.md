# Inline `ToInt32` design — 2026-08-09

Status: rejected after prototype measurement. No production code from this
design is retained. The expanded differential coverage remains in `9e1aa2a9`.

# Problem

The accepted Number `BitOr` lowering is correct and improves `int-arith` by
2.67×, but it invokes a Rust helper twice for each `BitOr`: three operations per
iteration mean six ABI transitions and six conversions. The resulting 19.449
ms median remains far above the approximately 2.65 ms native floating-point
control, whose loop has no helper transitions.

# Selected translation

Translate the portable bit-level algorithm in Boa's canonical
`f64_to_int32` directly to Cranelift IR. This is not a new numeric algorithm.
For each input:

1. bitcast the `f64` to `i64`;
2. extract the 11-bit exponent, 52-bit physical significand, and sign;
3. select the denormal exponent/significand or add the implicit hidden bit;
4. return zero when the effective shift discards the complete 53-bit
   significand or when all retained low 32 bits are necessarily zero;
5. otherwise shift the significand, mask to 32 bits, apply the sign, and reduce
   to `i32`;
6. OR the two results and convert the final `i32` exactly back to `f64`.

The constants and branch bounds must remain visibly identical to
`f64_to_int32`. NaN and infinities naturally take the large-exponent zero path;
positive and negative zero and subnormals take the small-exponent zero path.

# Safety and scope

- This changes only generated primitive IR. It reads no VM or GC state and
  adds no pointer, allocation, or runtime callback.
- Existing representation guards continue proving that inputs are Numbers.
- The Rust helper is removed from production code only after differential
  tests use the interpreter/canonical path as an oracle.
- Loop OSR remains excluded.
- The generated-code payload increase remains subject to the existing
  per-function and aggregate bounds.

# Gates

- Preserve the complete Number edge/coercion/BigInt/budget suite from
  `dca5f7fc`.
- Add a deterministic arithmetic/conversion differential across interpreter
  and JIT contexts that exercises varied exponents, signs, fractions, and
  repeated conversions.
- Pass the complete JIT library suite and warning-denying Clippy.
- Seven diagnostics-off release samples must keep exact sinks and zero deopts,
  improve the `int-arith` median by at least 15% over 19,448,875 ns, and keep
  the artifact within the existing resource limits.

# Prototype outcome

The bit-for-bit IR translation passed the complete edge suite and a new
1,799-iteration interpreter/JIT differential across seven seeds. It produced
the exact benchmark sink with zero deopts. It nevertheless failed the
performance gate:

- a diagnostics run measured 20,492,208 ns per `int-arith` call, slower than
  the accepted helper median of 19,448,875 ns;
- the native artifact grew from 1,104 to 1,816 bytes;
- warm compilation time rose from the helper gate's roughly 0.35–0.43 ms range
  to 0.69 ms in the diagnostic sample;
- cold compilation plus execution rose to 32.45 ms.

The duplicated branch-heavy conversion CFG costs more than the optimized Rust
helper transition on this host. The prototype was removed in full. A future
attempt should use an ISA conversion instruction, a single shared generated
conversion stub, or a range-proven fast path with the helper as its uncommon
fallback; repeating this inline CFG is not justified.
