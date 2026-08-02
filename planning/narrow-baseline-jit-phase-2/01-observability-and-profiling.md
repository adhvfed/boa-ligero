# Observability and workload profiling

This is the first Phase 2 slice. It changes no generated code. Its purpose is
to replace guesses about “missing opcodes” or “call overhead” with measurements
from the workloads Boa actually needs to run.

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

Add an opt-in diagnostic snapshot, preferably behind the existing `jit`
feature and disabled by default. Keep hot-path counters cheap and keep detailed
site data behind an explicit diagnostic mode so normal JIT execution does not
pay for hash-map updates at every instruction.

Suggested records:

```text
CompileRecord {
    code_id, entry_pc, region_id,
    outcome: Native | Shim | Rejected,
    first_blocking_opcode, first_blocking_pc,
    native_instruction_count, bytecode_instruction_count,
    compile_ns, code_bytes,
}

ExitRecord {
    code_id, region_id, pc,
    kind: Deopt | Call | Return | Throw | Budget | OSR,
    reason: Unsupported | Type | Shape | Element | Binding | CallTarget |
            Entry | Exception | RuntimeLimit | HostReentry,
    count, native_ns,
}
```

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

Interleave interpreter and JIT runs, repeat enough to see machine noise, and
keep the same source/context setup. A synthetic native-entry count is not a
workload win.

## First-slice acceptance

This slice is done when:

- a representative script reports its first blocking opcode/PC or a clear
  native region/exit profile;
- the stats are zero-cost in a JIT-disabled build and bounded-cost in normal
  JIT mode;
- a profile identifies the next lowering target and the expected consumer
  workload;
- no new native ABI or direct object-layout assumption was introduced.

The output should be checked in as a dated measurement note only after the
workload owner confirms that the invocation is representative.

