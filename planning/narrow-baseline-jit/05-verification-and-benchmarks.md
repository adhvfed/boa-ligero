# Verification and benchmarks

The JIT must be verified as a second implementation of the VM boundary, not
only as a Cranelift code-generation test.

## Test layers

### Compiler unit tests

Test the compiler without running JavaScript:

- instruction decoding and `pc -> instruction` maps;
- branch-target validation;
- CFG leaders and successors;
- handler-range boundaries;
- register bounds and frame metadata;
- allowlist rejection for unsupported instructions;
- exit-word encoding/decoding;
- materialization maps for every supported native value kind.

Prefer small hand-built `CodeBlock`s for malformed-target and malformed-operand
tests so the failure is localized to the compiler.

### Native/interpreter differential tests

For every supported operation and every exit kind, evaluate the same source in:

1. the normal interpreter;
2. the explicit JIT entry;
3. the tiered JIT path once it exists.

Compare the final value, thrown error class/message where stable, and visible
VM behavior. Run both cold and after enough iterations to exercise the cache.

Minimum semantic cases:

- integer overflow and negative zero;
- NaN, infinities, floating-point rounding, and coercion fallback;
- strings on `+`;
- undefined/null/boolean/object operands;
- branch taken and not taken;
- loop backedges and zero-iteration loops;
- return from the top-level frame and nested ordinary frames;
- recursion and stack limits;
- `try/catch/finally`, thrown property access, and uncaught errors;
- closures and captured environments;
- `eval`/`with`/dynamic lookup fallback;
- array holes, out-of-bounds indexes, sparse arrays, prototype changes, and
  getters;
- named-property shape changes, accessors, prototype lookup, and megamorphic
  sites;
- callee replacement, polymorphic calls, proxies, bound/native functions, and
  spread calls;
- forced GC/finalization around property and call guard checks;
- instruction and loop budgets;
- host reentry and nested `Context::run` where the JIT is expected to exit.

### Stress and differential execution

Add a small differential runner that executes generated or hand-mutated
programs in interpreter and JIT modes with the same context setup. The initial
corpus can be modest; its value is exercising deopt combinations rather than
proving language completeness.

When a mismatch occurs, report:

- source or bytecode dump;
- code-block identity and native entry version;
- current bytecode PC and exit kind;
- guard/feedback state;
- interpreter/JIT result or error;
- whether a GC or helper call occurred before the mismatch.

### Feature and platform matrix

At minimum, keep these checks green:

```text
cargo test -p boa_engine --lib
cargo test -p boa_engine --lib --features jit
cargo check -p boa_engine --lib --features jit
cargo check -p boa_engine --lib --no-default-features
cargo test --workspace
```

Run the JIT tests on the supported x86-64 and AArch64 environments. Do not
assume a pointer representation or calling convention that is only valid on
the development Mac.

## JIT observability

Add an opt-in stats snapshot, not unconditional logging. It should report:

- compile requests, successes, and rejected code blocks;
- compiled code size and compile time;
- function-entry and backedge counts;
- native instructions/regions executed;
- helper calls by kind;
- guard hits/misses by site and reason;
- deopts by reason and resume PC;
- exceptions, calls, returns, and budget exits through native code;
- time in compilation, native code, and interpreter fallback.

This is necessary to tell a real JIT win from a benchmark that merely moved
work into compilation or deoptimized immediately.

## Benchmark protocol

Use `tools/bench-compare` and keep the interpreter/JIT controls separate:

```bash
cargo build --release -p boa_benches --bin bench-compare-runner
RUNS=200 WARMUP=20 tools/bench-compare/compare.sh
```

For JIT-specific measurements, add a runner mode that reports:

- cold execution including compilation;
- warm execution after the compiled entry is installed;
- compilation time and generated code size;
- deopt count and reason;
- the same final sink/result as the interpreter runner.

Keep DCE-suspect scripts out of the headline geomean until they have a real
observable sink. Always include a Boa-vs-Boa comparison; Node `--jitless` is a
useful reference, not proof that Boa's JIT is correct or that a change paid for
itself.

## Stage gates

### Gate 0 — ABI and fallback

- current JIT tests pass;
- a compiled function can deopt at every unsupported boundary;
- no result differs from the interpreter;
- JIT-off build has no new runtime work.

### Gate 1 — primitive loops

- integer and floating-point loop benches run native backedges;
- warm native execution clears the 1.5x interpreter speedup gate;
- compile time and cold behavior are reported;
- budget, exception, and guard-failure tests pass.

### Gate 2 — arrays/properties

- matching dense-array and monomorphic-property sites show a win;
- mismatch, mutation, accessor, prototype, and GC cases deopt safely;
- shape/element guard misses are visible in stats.

### Gate 3 — calls

- direct ordinary calls show a warm win;
- all non-target call kinds return to the interpreter;
- recursion, stack traces, runtime limits, exceptions, and host reentry pass.

### Gate 4 — real workloads

- at least one browser-shaped or bundle-shaped workload improves after compile
  cost is included;
- no large cold-start regression is hidden by the microbench suite;
- the JIT remains opt-in until the workload-level result is stable.

