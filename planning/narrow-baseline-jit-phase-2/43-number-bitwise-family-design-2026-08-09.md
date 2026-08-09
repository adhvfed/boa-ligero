# Number bitwise-family design — 2026-08-09

Status: selected as a narrow extension of the accepted Number `BitOr`
contract. This slice covers binary `BitAnd` and `BitXor` only.

# Measured frontier

Current diagnostics show two otherwise useful callers stopped by the remaining
binary bitwise opcodes:

- `array-numeric-sum`: `BitXor` at PC 178 after a 22-instruction supported
  prefix in a 34-instruction loop body;
- `property-poly4`: `BitAnd` at PC 94 after a 13-instruction supported prefix
  in its 28-instruction loop caller. Its 14-instruction property callee remains
  independently below the straight-line admission floor.

Seven paired diagnostics-off release samples produced these median
nanoseconds-per-run baselines:

| workload | interpreter | current JIT |
| --- | ---: | ---: |
| `array-numeric-sum` | 35,085,812 | 36,266,208 |
| `property-poly4` | 7,287,583 | 7,023,180 |

The first samples were host-noisy, but the medians and retained per-process
evidence agree that neither workload currently installs an artifact.

# Contract

Apply the already accepted `dca5f7fc` Number semantics unchanged:

- a body containing `BitAnd` or `BitXor` selects `f64` mode;
- arithmetic remains IEEE-754 `Number` arithmetic until the bitwise operation;
- a leaf helper applies Boa's canonical `f64_to_int32` to both proven-number
  operands and performs the selected `i32` operation;
- the exact result returns as `f64`;
- existing pre-effect representation guards send non-number coercion, BigInt,
  symbols, and observable conversion hooks to the interpreter.

Use separate leaf helpers for AND and XOR so the hot helper has no operation
tag branch. Share the lowering structure in Rust to avoid three drifting
code-generation paths.

# Exclusions and gates

Shifts, unary bitwise not, and loop OSR remain unsupported. The one-shot OSR
diagnostic must keep reporting the actual first excluded opcode.

The permanent suite must extend the Number edge, non-number coercion, BigInt,
GC, and finite-budget evidence across the family. The slice is retained only
if seven diagnostics-off release pairs:

- return exact sinks with native artifact/entry evidence and zero steady
  deopts;
- improve `array-numeric-sum` by at least 15% over its current-JIT median;
- keep `property-poly4` within 5% of its current-JIT median, and retain it only
  as a positive result if its loop caller actually compiles;
- keep code payload and compilation within the existing governor.
