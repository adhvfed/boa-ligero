# Observability and workload profiling

This is the first Phase 2 slice. It changes no JavaScript semantics and adds no
storage, call, or OSR ABI. It does add compact reason tags to the existing
generated-code exit status so a guard cause is not lost at the interpreter
boundary. Its purpose is to replace guesses about “missing opcodes” or “call
overhead” with measurements from the workloads Boa actually needs to run.

## Questions the profile must answer

For every hot code block or region:

- Was native compilation requested, rejected, or compiled as shim fallback?
- If rejected, what was the first unsupported/malformed instruction and PC?
- Which native regions ran, for how long, and how many instructions/loops did
  they cover?
- Which exit kind occurred, at which PC, and for what reason?
- How often did type, shape, element, binding, and call-target guards hit or
  miss?
- How many native-to-interpreter and native-to-native call/return transitions
  occurred?
- How often was OSR requested, compiled, installed, entered, or abandoned?
- What were compile time, code size, cold time, warm time, and fallback time?

The current aggregate `JitStats` counters are a starting point, but they do not
identify the first unsupported operation or distinguish an entry deopt from a
loop, guard, call, budget, or exception exit.

## Stats shape

Add a versioned, opt-in diagnostic snapshot behind the existing `jit` feature
and disabled by default. Keep the current aggregate hot-path counters cheap and
keep detailed site data behind an explicit runtime diagnostic mode so normal
JIT execution does not pay for hash-map updates at every instruction.

Detailed diagnostics must be bounded and publisher-neutral. Identify code by a
runtime-local numeric ID, opcode, PC, region, and reason; do not retain source
text, URLs, property names, or object values. Aggregate repeated sites into a
bounded top-N or fixed-capacity table with explicit dropped-record counts so a
hostile page cannot turn profiling into unbounded memory growth. Give the
machine-readable snapshot a schema version and deterministic ordering.

Initial whole-CodeBlock records:

```text
CompileRecord {
    code_id, entry_pc,
    outcome: Native | Shim,
    blocker,
    first_blocking_opcode, first_blocking_pc,
    supported_prefix_instruction_count,
    native_instruction_count, bytecode_instruction_count,
    compile_ns, code_bytes, charged_budget_variant,
}

ExitRecord {
    code_id, entry_pc, pc,
    kind: Deopt | Call | Return | Completion | Budget,
    reason: EntryGuard | ArgumentType | StackType | DenseElement |
            NamedProperty | CallTarget | IntegerOverflow | Scheduler |
            Return | RuntimeLimit | Exception,
    count, native_entry_wall_ns,
}
```

Region identity, OSR outcomes, direct-storage assumptions, and dynamic native
instruction counts are intentionally absent until those execution mechanisms
exist. Static lowered-bytecode coverage must not be described as dynamically
executed instructions.

The exact Rust representation can remain compact. The important property is
that a benchmark output can answer “why did this stay interpreted?” without
requiring a debugger or a second instrumented build.

## Workload protocol

Run three classes of inputs:

1. **Micro controls:** the existing Boa-vs-Boa interpreter/JIT runner for
   integer/floating loops, dense arrays, monomorphic properties, and ordinary
   calls.
2. **Engine workloads:** the existing V8/Octane-style scripts, using a real
   result sink and bounded repetitions.
3. **Browser-shaped workloads:** a fixed script/bundle set and invocation
   protocol supplied by the `../ligero-browser` effort. Boa owns the JIT stats
   and runner; the browser agent owns the page/task selection and host API
   setup. Do not copy browser assumptions into the engine benchmark silently.

For each workload capture:

- interpreter cold and warm controls;
- JIT cold execution including compilation;
- JIT warm execution after installation;
- compile requests/results and native coverage;
- deopts/exits by reason and PC;
- final observable sink and errors.
- profiler memory/record drops and diagnostic-on timing overhead.

Interleave interpreter and JIT runs, repeat enough to see machine noise, and
keep the same source/context setup. A synthetic native-entry count is not a
workload win.

## First-slice acceptance

This slice is done when:

- a representative script reports its first blocking opcode/PC or a clear
  native region/exit profile;
- the stats are absent in a JIT-disabled build, aggregate-only in normal JIT
  mode, and bounded in detailed diagnostic mode;
- deterministic machine-readable output reports schema version, dropped
  records, and the exact first blocker/exit without source or URL data;
- the same checksummed workload is compared with diagnostics off and on so
  profiler overhead is known rather than folded into the JIT result;
- a profile identifies the next lowering target and the expected consumer
  workload;
- no new native ABI or direct object-layout assumption was introduced.

The output should be checked in as a dated measurement note only after the
workload owner confirms that the invocation is representative.

## Implementation checkpoint — 2026-08-03

Engine groundwork landed in `bebcd640` and standalone JSON export in
`17a80a53`:

- diagnostics are explicit, absent when disabled, and hard-capped at 4,096
  compile plus 4,096 distinct exit records regardless of caller input;
- compilation records preserve the exact first unsupported opcode/PC and
  actual Cranelift code bytes, while diagnostics-off compilation retains its
  old early-rejection behavior;
- exit records aggregate exact guard/transition reasons, resume PCs, counts,
  and native-entry wall time; runtime-limit and final-return breaks use a VM
  sideband without changing the completion payload;
- deterministic snapshots retain numeric runtime-local IDs only and serialize
  without source text, function names, URLs, property names, values, or raw
  pointers;
- the standalone runner writes cold and warm JSON only after timing when
  `--jit-diagnostics-out <path>` is explicitly selected.

Verification: 1,167 JIT tests passed with one ignored performance test, 1,138
non-JIT tests passed, the no-default-features engine build passed, and focused
runner/serialization tests passed. Warning-denying Clippy has no new findings;
22 existing engine/JIT warnings remain scheduled for a behavior-neutral
refactoring checkpoint.

A five-pair release control on `property-mono` preserved the checksum and
measured a 31.16 ms median with diagnostics off versus 34.92 ms on: 12.1%
profiling overhead on an intentionally hostile 16.6-million-native-entry warm
sample. Headline performance measurements must therefore keep diagnostics off
and use a separate diagnostic run, as required above.

Ligero commit `745848d8` now publishes the same bounded snapshot through an
embedding-owned, source-free projection. `ligero bench --jit-diagnostics`
requires both script execution and the explicit experimental-JIT opt-in;
snapshot collection occurs after the timed page load and JSON serialization
after rendering. The feature-disabled build and normal JIT mode retain no
detailed records.

Five interleaved fresh-process triples on the fixed browser gate measured
42.90 ms median interpreter load, 30.88 ms normal-JIT load, and 30.75 ms
diagnostic-JIT load. The diagnostic mode retained three compilation records
and one exit record with no drops: one 24-instruction numeric kernel was native,
two entries selected shims because of exception-handler metadata, and the
native kernel recorded 999 normal returns with zero deoptimizations. All modes
retained the same 387 display items and 258 paint segments. One 49.52 ms
diagnostic outlier and the standalone runner's hostile-loop overhead result
continue to require separate diagnostic and headline runs.

This closes the Ligero projection part of Slice 1, but not Gate P. The fixed
page is deliberately native-friendly W0 evidence and cannot rank the blockers
in a representative bundle. Run the remaining agreed micro/engine/bundle
matrix, check in its publisher-neutral evidence, and only then select Slice
2's smallest blocker batch or execution ABI.
