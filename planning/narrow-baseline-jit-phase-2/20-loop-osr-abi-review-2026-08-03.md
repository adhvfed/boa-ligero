# Loop-header OSR ABI review — 2026-08-03

Status: reviewed for Slice 4A0. No compiler, scheduler, cache, or admission
behavior changes in this document. Slice 4A1 may implement only the contract
below and must stop if its analysis cannot prove every entry and exit map.

## Result

Approve one conservative numeric loop-header OSR ABI, with four corrections to
the earlier sketch:

1. A loop artifact is keyed by both its header and its observed latch/backedge.
   Header PC alone is ambiguous when a loop has more than one latch or a future
   region boundary changes.
2. The key includes the numeric representation selected from the current live
   values. The selected one-shot fixture has mixed integer-valued and fractional
   live-ins, while the existing whole-body compiler chooses one `I32` or `F64`
   representation for the artifact.
3. A taken forward edge out of the native loop is a post-effect continuation,
   not a pre-effect deoptimization. It receives a distinct exit kind and never
   refunds or replays the branch.
4. CodeBlock-global hotness is not sufficient proof that a particular loop is
   hot. OSR admission uses bounded per-region state; the existing CodeBlock
   counters remain the function-entry tier's policy.
5. OSR has its own per-frame attempted/closed flag. It must not reuse either
   the PC-zero `jit_entry_attempted` flag or the CodeBlock-hotness saturation
   flag that hands frames to dormant dispatch.

The first implementation remains build- and runtime-opt-in. It does not enable
JIT by default, add background compilation, add an object representation, or
relax whole-function admission.

## Current boundary and concrete selected shape

`Context::run_with_jit_backend` executes one interpreter opcode through
`execute_one`, so the interpreter charges that opcode before the scheduler
observes a same-frame backward edge. At that observation:

- the backedge operation has completed exactly once;
- the current frame still has the same depth and `CodeBlock`;
- `frame.pc` is already the loop header;
- every register is boxed and rooted in the VM stack;
- the backend is owned by the scheduler, outside `Context`, and no opcode
  helper borrow is live.

That is the only approved compile-and-entry boundary.

The durable one-shot fixture compiles to this relevant bytecode shape:

```text
001c IncrementLoopIteration
001d Inc                    r03 -> r03
0026 JumpIfNotLessThan      r03, r04 -> 004e
0033 Add                    r01, r03 -> r05
0040 Move                   r05 -> r01
0049 Jump                   -> 001c
004e ...                    post-loop continuation
```

OSR is requested only after the interpreter executes and charges `0049`, with
the current PC already equal to `001c`. The native region owns `001c..0049`.
The exit to `004e` is outside the region and resumes after the already charged
conditional branch.

## Typed identity and bounded state

Replace the function-only cache identity with a typed entry point:

```text
JitEntryPoint::Function
JitEntryPoint::Loop {
    header_pc,
    backedge_pc,
    representation: I32 | F64,
}

JitCacheKey {
    code_id,
    entry_point,
    budgeted,
    diagnostic,
}
```

`backedge_pc` is part of identity even when the first eligibility rule accepts
only one canonical latch. `representation` is part of identity rather than
mutable metadata on a compiled entry. `budgeted` keeps the existing finite-
budget and unlimited fast paths separate. `diagnostic` preserves isolation of
instrumented artifacts. The backend generation is implicit in backend-owned
maps and must never be serialized into a reusable machine-code identity.

Each loop key has one bounded state:

```text
Observed { backedges }
Ready { entry, code_bytes, maps }
Rejected { reason }
Suppressed { cache | bytes | time }
```

There is no loop shim. Rejection and suppression continue in the interpreter
and are cached so a page cannot force repeated synchronous compilation.
Compilation is synchronous and non-reentrant, so `Compiling` is a local
transaction condition rather than a retained state.

Initial hard policy:

- at most 128 decoded instructions in a region;
- at most 64 retained loop-region states per backend;
- a 1 MiB accounted emitted-loop-code circuit breaker per backend;
- one compile attempt per exact key;
- after one region compile takes more than 10 ms, suppress further new OSR
  compilations in that backend while retaining already compiled entries.

The wall-time limit is a post-compile circuit breaker, not a claim that native
code generation can be preempted. Likewise, Cranelift module code is not
individually reclaimable and its exact size is known only after compilation:
one artifact may cross the accounted byte threshold, after which all new loop
compiles are suppressed. This is not a strict physical-allocation cap. The
static instruction bound limits the one unavoidable synchronous attempt.

Capacity overflow is global and allocation-free: an existing exact key may be
looked up or updated; a new key is inserted only while the table has capacity;
once full, every previously unseen site is suppressed without inserting a
negative-cache entry. All limits are private policy constants and aggregate
diagnostics expose counts only, never source, values, names, URLs, realms, or
pointers.

## Static region eligibility

`loop_admission_profile` is currently a profiling screen, not an entry proof.
It accepts several whole-body operations that the first OSR region must reject,
and it does not prove external-edge or live-state maps. Slice 4A1 must use a
separate region planner with these requirements:

- current frame is ordinary, non-async, non-generator, and non-construct;
- `header_pc` and `backedge_pc` are decoded instruction boundaries and
  `header_pc < backedge_pc`;
- the latch instruction is an unconditional same-frame jump to the header;
- there is exactly one accepted latch for the first shape;
- every internal branch lands on an instruction in the region;
- exactly one conditional forward edge may leave the region, to a decoded
  post-loop continuation; no fallthrough leaves the region;
- `CodeBlock::handlers` is empty, and no other backward edge, irreducible edge,
  iterator state, binding-stack transition, environment mutation, or operand-
  stack mutation intersects the region;
- allowed bytecodes are numeric constant stores, `Move`, `Add`, `Sub`, `Mul`,
  `Inc`, the already supported numeric comparisons/branches,
  `IncrementLoopIteration`, and the canonical latch;
- calls, construction, return, push/pop, properties, name/environment access,
  allocation, conversion/coercion, bitwise operations, handlers, `eval`,
  `with`, suspension, host re-entry, and object use are rejected;
- region and register counts fit the hard limits;
- the planner proves all live-in and per-exit materialization maps before any
  Cranelift function is declared.

A failure in any proof is `Rejected`, never a partial native prefix.

## Live-state and representation contract

The VM stack remains the canonical root set. The region planner performs CFG
liveness to a fixed point over the loop and its external continuation. It
records:

```text
LoopRegionPlan {
    key,
    instructions,
    entry: [{ register, representation, source: VmRegister }],
    exits: [{
        from_pc,
        resume_pc,
        kind,
        materialize: [{ register, source: NativeValue | PreservedVmValue }],
    }],
}
```

The entry map contains every register read on a path before a native definition,
including loop-carried values. Each path-specific exit map contains every
register live at the interpreter resume PC whose current native definition may
differ from the boxed VM register. Every exit-live value must have a definition
on that exact path. A body-local definition bypassed by the false-condition
exit is either proven dead, explicitly preserved from its VM slot, or causes
rejection; a static "all dirty registers" set is not an exit map. A value not
proven numeric is rejected; dead boxed registers do not poison an otherwise
numeric region.

At the safe scheduler boundary, inspect all live-ins without retaining them.
Any `StoreFloat` or `StoreDouble` in the region statically requires `F64`;
otherwise:

- select `I32` only when every live-in is an exact Boa integer value;
- otherwise select `F64` only when every live-in is a JavaScript Number;
- reject the observation when any live-in is non-numeric or out of bounds.

Generated entry then repeats the same guards before loading values. Selection
outside generated code is policy; the generated guards are correctness. An
`F64` region may represent integer-valued Numbers as `f64`; it must preserve
NaN, infinities, and signed zero. An `I32` arithmetic overflow is a pre-effect
guard exit and lets the interpreter replay the current operation.

The selected representation is uniform for the artifact: every Cranelift
register variable and native materialized value uses that one `I32` or `F64`
mode. The selected fixture therefore chooses `F64` from its live `r01 = 0.5`
and seeds its integer-valued loop variables as exact `F64` Numbers. Mixed-mode
SSA is not part of Slice 4A1. Current helper getters that silently return zero
on mismatch are forbidden at OSR entry; new loads must be preceded by strict
guards and cannot manufacture a default value.

No `JsValue`, `Gc`, object, environment, raw stack address, frame pointer, or
Rust reference is retained by the artifact. Register indices and numeric SSA
values are the only materialization data used by generated code.

## Entry guard and ownership

Compilation and invocation happen only after the interpreter backedge returns
to `run_with_jit_backend`. The scheduler holds `&mut JitBackend` and
`&mut Context`, but `Context::jit_backend` is `None`; therefore nested
`Context::run` cannot borrow or use this backend. The cloned current `Gc<CodeBlock>`
roots bytecode metadata for the synchronous compile. Compilation invokes no
JavaScript, host hook, allocation helper, or GC helper.

Immediately before loading live-ins, the generated entry validates:

- `Context::active_jit_backend_id` equals the owning backend generation;
- current `CodeBlock::debug_id` equals the key's `code_id`;
- current PC equals `header_pc`;
- the frame is non-construct and its register range is in bounds;
- current finite-budget mode equals the key's `budgeted` bit;
- every live-in matches the key's numeric representation.

No raw `CallFrame` identity is cached. A recursive invocation with the same
live `CodeBlock`, header, and valid materialized state may safely use the same
artifact because the entry reloads the current frame. Code IDs are monotonic
within the backend's thread and the cache is backend-owned; invocation always
also checks the currently live CodeBlock.

An entry-guard miss happens before any native bytecode charge or effect. It
returns a distinct `EntryRejected` status at `header_pc`, with no instruction-
budget refund and no deoptimization statistic. The interpreter already owns
the complete header state. Scheduler-side policy rejection uses the same
semantic category without invoking generated code.

## Exit and replay taxonomy

The native status word gains two explicit OSR boundary kinds:

```text
EntryRejected { reason, header_pc }
Continuation { reason: LoopExit, resume_pc }
```

The scheduler handles `EntryRejected` by continuing at the already materialized
header without incrementing the deoptimization count. It handles
`Continuation` by setting `frame.pc = resume_pc` and continuing, also without a
deoptimization. All live-out registers are materialized first. The exiting
conditional branch has already executed and been charged; it is never refunded
or replayed.

Both PCs are validated against immutable artifact metadata: `EntryRejected`
can name only the key's header and `Continuation` can name only an exit in the
fixed path-specific map. An encoded page-controlled or unknown PC is never
accepted as a scheduler continuation.

Pre-effect type, representation, or arithmetic-overflow guards use the existing
`Deopt` kind. Before returning they materialize the map for the current PC,
refund exactly the current native instruction only in budgeted mode, set the
current PC, and let the interpreter execute that bytecode exactly once.

Runtime-limit exits use `Budget`/pending completion. They never refund or
replay. Calls and exceptions are statically absent. Unknown exit kinds or an
invalid resume boundary are an engine error in debug/test validation and must
not be interpreted as a page-controlled PC.

Materialization helpers are infallible, non-allocating, no-unwind operations
over validated register indices. The generated code writes all mapped values
before writing the resume PC and returning. No native SSA value is used after
the C ABI returns.

## Exact budget and loop-limit ownership

The charged interval is explicit:

1. The interpreter charges and executes the triggering latch once.
2. Synchronous compilation consumes no JavaScript instruction budget.
3. The OSR entry guard consumes no JavaScript instruction budget.
4. Native execution charges each original bytecode immediately before that
   bytecode's lowering, beginning at `header_pc`.
5. `IncrementLoopIteration` charges one bytecode instruction, writes its
   interpreter-visible next PC, and consumes one loop iteration exactly as the
   interpreter does.
6. A normal external edge retains the conditional branch's charge.
7. A pre-effect guard refunds only its current bytecode because the interpreter
   replays exactly that bytecode.
8. A budget or loop-limit failure retains every completed charge and iteration.

Differential tests run with exhaustion one instruction before, at, and one
instruction after OSR entry, the first native header instruction, the loop-
iteration poll, a pre-effect arithmetic guard, and the normal external exit.
Remaining budgets and error kinds must match interpreter-only execution.

Loop-limit errors are uncatchable engine errors and the first region rejects
handlers, so native values cannot become visible through a catch path.
Nevertheless, the failure PC and pending completion must match the interpreter
and all numeric live values needed by diagnostics are materialized before the
exit.

## GC, exceptions, and security

The first region holds only unboxed numeric SSA values. All object and boxed
values remain in their original traced VM stack slots, and no raw GC pointer is
loaded. Entry guards and materialization helpers neither allocate nor invoke
GC. Forced GC immediately before compilation, after compilation, before a
cached entry, and after return must preserve results and cache safety.

No Rust panic or unwind may cross generated `extern "C"` calls. Planner bounds,
decoded-boundary validation, register-range validation, and exit-word decoding
must happen before unchecked helper access. The new planning and code-generation
path adds no page-controlled `expect`, unchecked index, or panicking conversion;
all recoverable module/codegen failures become cached rejection and interpreter
fallback. A Cranelift internal panic remains an existing backend-wide risk and
must be contained by dropping/disabling the context-owned JIT before this tier
can be considered safe for default remote-script execution.

The cache retains machine code and plain numeric metadata, not page source or
GC-owned values. Disabling JIT drops the backend and all region entries.

## Diagnostics and acceptance

Aggregate stats add, at minimum:

- OSR compile attempts, compilations, entries, normal continuations, and
  entry rejections and pre-effect deoptimizations;
- rejection/suppression counts by bounded source-free reason;
- compile time and code bytes;
- cache hits/misses for typed loop keys.

Detailed records reuse runtime-local code ID and PCs only. They never include
source, function/property names, URLs, values, raw identities, or pointers.
Diagnostics-on artifacts remain a separate cache variant and every diagnostic
record class stays hard bounded.

Slice 4A1 is accepted only when all of the following pass:

1. The durable one-shot fixture records a native OSR compilation and entry,
   produces the interpreter sink, and materially beats interpreter execution
   including synchronous compilation.
2. Zero-, one-, and below-threshold loops stay interpreter-only; nested and
   multiple-loop controls do not enter the wrong region.
3. Non-number live-ins, `I32` overflow, signed zero, NaN, infinities, and a
   representation change across cached invocations match the interpreter.
4. Normal external exit, pre-effect replay, instruction-budget boundaries,
   loop-limit failure, recursion, nested frames, malformed targets, and forced
   GC pass focused differential tests.
5. Ineligible calls, properties, stack mutation, handlers, allocation,
   bitwise/conversion, and object live-ins compile no loop artifact.
6. The ineligible one-shot control and Gate P negative controls remain within
   the recorded 5% diagnostics-off parity guardrail.
7. W0 retains its sink, paint structure, native PC-zero entry, and cold-load
   guardrail; no workload may accidentally replace a profitable function entry
   with OSR.
8. Feature-disabled builds contain no OSR state or dispatch branch, affected
   warning-denying Clippy passes, and all Phase 1 JIT tests remain green.

If the first planner cannot prove the selected fixture's live maps without
special-casing source or register numbers, stop at the planner and revise the
analysis. Do not substitute whole-frame boxing, unchecked default numeric
loads, or a replaying normal exit merely to make the benchmark enter native
code.

## Sequencing and scheduled refactor

Land Slice 4A1 in independently revertible behavior slices:

1. pure typed region identity, canonical-latch CFG/liveness planner,
   path-specific entry/exit map proofs, and rejection tests;
2. bounded backend state, full-table suppression, aggregate diagnostics, and
   the new exit taxonomy, with no scheduler invocation yet;
3. a separate region compiler with strict uniform-mode live-in loads and
   generated continuation/materialization paths;
4. scheduler wiring at the exact post-`execute_one` latch boundary, using a
   separate per-frame OSR flag and preserving dormant dispatch on rejection;
5. budget/limit/GC/representation differential gates and fixed-matrix rerun.

After behavior and diagnostic slices, land a separately revertible behavior-
neutral refactor that consolidates typed entry keys, materialization emission,
or exit mapping. Decision checkpoint B then reruns the fixed matrix before any
second execution ABI is selected.
