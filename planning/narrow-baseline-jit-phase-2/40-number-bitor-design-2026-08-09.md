# Number `BitOr` lowering design — 2026-08-09

Status: accepted and implemented by `dca5f7fc`. Loop OSR remains explicitly
excluded from this slice. Results are recorded in the
[implementation checkpoint](41-number-bitor-checkpoint-2026-08-09.md).

# Measured problem

On commit `58153bd7`, the 29-instruction `int-arith` body is denied with
`BitOr` as its first blocker at PC 65 and creates no artifact. Seven paired
diagnostics-off release-process samples, three timed calls after six warmups,
produced these nanoseconds-per-run medians:

| execution | median |
| --- | ---: |
| interpreter | 50,321,569 |
| current JIT | 51,878,222 |

The current JIT is 3.09% slower because it reaches the admission threshold and
then remains interpreted. The workload uses `(arithmetic) | 0` after additions,
multiplications, and subtractions.

# Why an `i32`-only lowering is incorrect

JavaScript evaluates the arithmetic as `Number` before `BitOr` applies
`ToInt32`. Wrapping the preceding `i32` arithmetic is not generally equivalent.
In particular, a product beyond exact IEEE-754 integer precision may round
before conversion; native wrapping multiplication would preserve different
low bits.

An overflow-deoptimizing `i32` lowering would also fail the performance goal:
the measured accumulator leaves the signed 32-bit range during the hot loop,
so the whole-function entry would deopt once and never re-enter.

# Selected contract

Any whole-function body containing supported `BitOr` selects the existing
`f64` native representation. Arithmetic therefore retains JavaScript `Number`
semantics, including infinities, NaN, negative zero, fractional values, and
IEEE-754 rounding.

At `BitOr`, generated code passes the two proven-number `f64` values to a small
Rust helper. The helper applies Boa's existing, independently tested
`f64_to_int32` conversion to both operands, computes the `i32` OR, and returns
that result as an exactly representable `f64` for subsequent native use.

Non-number arguments or bindings fail their existing pre-effect
representation guards before reaching `BitOr`; the interpreter then owns
coercion, objects, symbols, mixed BigInt errors, and observable conversion
hooks. No JavaScript value or GC pointer crosses the helper ABI.

# Explicit exclusions

- Loop-header OSR continues to reject `BitOr`; its separate region compiler
  has no conversion helper or mixed representation contract in this slice.
- `BitAnd`, `BitXor`, shifts, unary bitwise not, and explicit conversion
  opcodes remain unsupported until separately measured.
- The helper is not an inline Cranelift `ToInt32` implementation. That may be a
  later refinement if helper-call cost remains material.
- No wrapping shortcut is applied to an arithmetic instruction merely because
  a later operation converts it.

# Correctness gates

Differential tests must cover:

- `i32` operands and the `| 0` arithmetic pattern;
- fractional values, signed zero, NaN, positive and negative infinity;
- values around signed and unsigned 32-bit boundaries;
- products beyond `Number.MAX_SAFE_INTEGER` whose rounded `Number` result
  differs from wrapping integer multiplication;
- non-number coercion, objects with observable conversion hooks, BigInt, and
  mixed BigInt/Number errors through interpreter fallback;
- forced GC, exact finite instruction-budget parity, and loop-limit parity;
- preserved one-shot loop-OSR rejection until that compiler is extended.

# Performance gate

Retain the slice only if seven fresh paired diagnostics-off release-process
samples of `int-arith` produce the exact `-2034985248` result, install and enter
one native whole-function artifact, record zero steady deopts, and improve the
median by at least 2× over the current JIT above without regressing cold
correctness or bounded compilation behavior.
