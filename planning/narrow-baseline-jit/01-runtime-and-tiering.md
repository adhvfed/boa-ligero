# Runtime, cache, and tiering

## Current runtime shape

The current path is:

1. `Script::evaluate_jit` prepares the normal VM frame.
2. The caller supplies a `JitBackend`.
3. The backend compiles the `CodeBlock` for that invocation.
4. The generated function calls opcode shims and either returns a pending
   completion or resumes `Context::run`.

This is useful for proving the ABI, but it has three problems for a real tier:

- compilation is repeated instead of cached;
- cold code is compiled without a hotness decision;
- code running inside the interpreter has no tiering request for a hot callee or
  loop.

## Recommended ownership model

Use three layers with deliberately different lifetimes:

### `JitBackend`

Owns the Cranelift `JITModule`, helper symbols, finalized machine code, and the
monotonic symbol allocator. Generated function pointers are valid only while
this backend is alive.

The existing public `JitBackend` can remain the migration API. Do not make
compiled function pointers global or attach them to a `CodeBlock` without also
tying their lifetime to the backend.

### `JitCodeCache`

Owns compiled entries and metadata:

```text
JitCodeKey = (realm identity, code-block identity, bytecode version)

CompiledEntry:
    native entry pointer
    entry ABI/version
    eligible operation summary
    frame/spill metadata
    feedback assumptions
    compilation statistics
```

The cache is scoped to one context/realm/backend. A shape, object identity, or
function target observed in one realm must never be reused as a guard in
another realm.

The key must not be only `CodeBlock::debug_id`: it is useful for diagnostics but
is not a sufficient cross-context ownership contract. Use stable allocation
identity or a runtime-assigned identity together with the owning realm. Keep a
strong `Gc<CodeBlock>` in the cache entry if that is the simplest way to make
the identity and metadata lifetime explicit; add eviction only after the tier
works and cache growth is measured.

### `JitRuntime`

Owns the cache, hotness counters, feedback snapshots, thresholds, and stats for
one execution owner. It is the component that decides whether to request a
compile and which cached entry to run.

The first implementation should keep the backend/runtime outside `Context`
while the ABI is being stabilized. This preserves the current explicit
`evaluate_jit` entry point and avoids storing a raw `&mut JitBackend` in `Vm`.
Once the runtime loop is proven, add an internal `Context::run_with_jit` or
equivalent scoped runner that services tiering requests. Do not solve borrow
aliasing by putting an unscoped raw backend pointer in the VM.

## Hotness signals

Track separate signals:

- function-entry count for ordinary `CodeBlock`s;
- backward-edge count per loop header;
- time spent in compiled code versus compilation;
- deoptimization count by reason;
- guard hit/miss counts for each feedback family.

Do not reuse `CallFrame::loop_iteration_count` as a hotness counter. That field
is part of JavaScript runtime-limit semantics. Add JIT-only counters in a
side table or a carefully traced/interior-mutable code-block metadata field so
the feature-disabled build remains unchanged.

Initial thresholds should be configuration values, not magic constants buried
in the compiler. A reasonable starting experiment is a lower function-entry
threshold and a higher loop-backedge threshold, for example 1,000 entries and
10,000 backedges. Measure the compile crossover before changing them.

Compilation should be requested at a safe VM boundary, not from inside a
Cranelift helper while the VM is in a partially materialized state. A request
can be recorded by the VM and serviced by the outer JIT runner after the
current interpreter operation or native region exits.

## Eligibility

Before compiling, scan the `CodeBlock` and classify it:

- eligible ordinary function;
- unsupported function kind;
- contains an instruction that must always remain interpreted;
- contains control flow that the current compiler cannot represent;
- contains dynamic environment/host behavior requiring a conservative exit.

Eligibility is an optimization decision, not a correctness decision. An
eligible function may still deopt at any instruction. The compiler must use an
allowlist for native lowering and reject malformed bytecode or unknown
instruction shapes before emitting code.

The first release should reject async/generator code blocks and avoid entering
native code for functions whose hot path is dominated by `eval`, `with`,
proxies, or other unsupported dynamic behavior. Ordinary code containing a
rare unsupported operation may still compile if the generated path exits
before that operation.

## Installation and invalidation

Install a compiled entry only after:

1. bytecode decoding and CFG validation succeed;
2. all entry/exit metadata is available;
3. the native function is finalized by Cranelift;
4. the cache entry is atomically visible to the single-threaded runtime.

Do not patch bytecode in the first tier. The interpreter and JIT can consult
the cache at function entry or at a loop header, and a miss simply continues
interpreting.

Most assumptions should be guarded rather than invalidated:

- type assumptions guard the current `JsValue` representation;
- named-property assumptions guard a live shape and slot;
- element assumptions guard shape, key class, bounds/storage kind, and holes;
- call assumptions guard callee identity, ordinary-function kind, and the
  target code block.

When a guard misses, return to the interpreter. Do not try to patch machine
code or eagerly invalidate every dependent entry until measurements show that
guard misses are common enough to justify it.

## Reentrancy and lifetime

Generated code receives an exclusively borrowed `*mut Context` only for the
duration of the call from the runtime. It must not escape, be retained by a JS
object, or be used after the call returns.

The first tier should return to the interpreter before arbitrary host reentry,
async suspension, generators, or nested execution that could make the current
native assumptions difficult to represent. The existing context/VM call stack
must remain the stack trace and GC authority.

