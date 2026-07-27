# 09 — Cranelift JIT tier (the path beyond the interpreter)

**Why this, why now.** This session measured the interpreter-tier levers and found
the ceiling honestly:

- The call-dispatch fast path (lever #1) gave **−7% on `fn-call-flat` but 0% on
  richards** — call _dispatch_ is negligible in real code.
- `method-call-mono` profiling: cost is frame teardown + property ops, **not** the
  register zero-fill (lever 2b would be a wash — `push_frame` was ~54 samples).
- Arithmetic (lever #2) is near-parity already (`int-arith` 1.19×; i32 `_fast` paths
  exist).
- **GC is not a bottleneck**: 0% of runtime on compute benches, 0.05% on richards,
  1.4% on deltablue (the most allocation-heavy). A generational/GC rewrite is a wash
  for throughput.

The conclusion is structural: **the 3.43× real-workload gap is the cost of
_interpreting_ itself** — opcode dispatch, operand decode, per-op `Context`
threading, frame machinery. You don't close that with more interpreter fast paths;
you close it by **not interpreting hot code**. That means a JIT.

The goal is V8-parity. Full TurboFan parity is a multi-person-year target; this plan
stages toward it and the realistic high-impact outcome is a baseline JIT plus
IC-fed type specialization — several× the interpreter, decisively past `--jitless`.

## Why Cranelift

Hand-writing a multi-arch machine-code backend is the multi-year part. **Cranelift**
(the Rust-native codegen backend behind Wasmtime) does register allocation,
instruction selection, and aarch64/x86-64 for us. We lower Boa bytecode → Cranelift
IR → native. It even has an optimizing tier, so the same backend serves the baseline
_and_ the specialized tier. This is the force-multiplier that makes a JIT feasible
for a small team.

## The architecture (grounded in Boa as it exists)

- **Crate**: new `core/jit` (`boa_jit`), optional, behind a `jit` feature on
  `boa_engine`. Zero cost when off.
- **Unit of compilation**: one `CodeBlock` (`core/engine/src/vm/code_block.rs`) = one
  JS function. Tier per-function.
- **Tiering hook**: a hot counter (the existing `CallFrame::loop_iteration_count` and
  a per-`CodeBlock` call count). Cross a threshold → compile → install. Cold code
  stays interpreted; this keeps compile cost off the critical path.
- **VM-state calling convention (the GC-soundness keystone)**: the JIT'd function has
  signature `extern "C" fn(*mut Context) -> u32` (a status/control word). **It
  operates on the _same_ `vm.stack` register window the interpreter uses** (via
  `fp`/`rp`). Because the GC already traces `vm.stack`, JIT frames need **no new root
  management** — the single biggest integration risk dissolves. Registers are
  read/written through the existing stack, not Cranelift SSA values (except inlined
  fast paths).
- **Baseline lowering = call-threading**: each bytecode op lowers to a direct native
  call to its existing Rust handler (`Operation::operation`) with operands threaded
  in. This _reuses every correct slow path_ and removes exactly what the interpreter
  pays per op: the central indirect dispatch (`OPCODE_HANDLERS[op]`), the `pc`
  decode/advance, and the `frame()` reload. This is the Sparkplug model.
- **Control flow**: build a CFG from the bytecode (leaders at jump targets); bytecode
  jumps → Cranelift `br`/`brif`; the per-op `ControlFlow` becomes block edges.
- **Exceptions / bailout**: handlers already return `JsResult` / set a
  `CompletionRecord`. The JIT checks the status after each fallible call and branches
  to a shared bail epilogue that returns control to the interpreter's exception
  machinery. No unwinding through native frames.

## Staging (each independently shippable + measurable)

- **Stage 0 — spike (de-risk).** Cranelift compiles + runs a trivial native fn inside
  Boa's build; then a fn that takes `*mut Context` and calls one real handler.
  Validates toolchain, platform, and the calling convention. _(task JIT-0)_
- **Stage 1 — baseline call-threading JIT + tiering.** Compile a `CodeBlock`, install
  it, run it on the shared stack. A/B vs interpreter on `fn-call-flat` + a loop bench.
  Target: clearly beat the interpreter; reach ~`--jitless` neighbourhood. _(JIT-1)_

  **Safe-by-construction lowering (decided after the shim layer landed):** do NOT
  classify opcodes as "JIT-safe" via a denylist — missing one control-flow op is a
  _miscompile_, the worst failure. Instead, emit a linear sequence of shim calls and,
  after each, check whether `frame.pc` advanced to the _compile-time-known linear
  next pc_. Three outcomes per op: (a) break → return the stashed `CompletionRecord`;
  (b) continue & pc == linear-next → fall through to the next op; (c) continue & pc
  changed (a jump was taken, or a `Call`/`New` pushed a frame) → **deopt**: return a
  "resume in interpreter" status and let the caller run `Context::run()` (which
  resumes from `frame.pc` and naturally runs callees/branches). This needs **no
  opcode classification and no CFG** for the baseline — any control flow safely falls
  back to the interpreter. Straight-line leaf code runs entirely in JIT; everything
  else deopts correctly. The shim should return `(break_flag, new_pc)` (e.g. packed in
  a `u64`) so the JIT can do the pc check without re-decoding. Generalize to in-JIT
  jump/call handling (real CFG, inlined calls) only _after_ this safe baseline lands
  and measures a win. Each JIT-compiled op still needs a test proving the deopt path
  runs for excluded cases (cross-ref the operator-snapshot regression tests).

- **Stage 2 — inline simple ops + IC-fed specialization.** Inline register moves and
  i32 arithmetic as native IR with type guards + deopt (OSR-out to the interpreter at
  the right `pc`). Then use Boa's inline caches (shape→slot, observed types) to emit
  specialized property access, arithmetic, and monomorphic call inlining. **This is
  where the multiples are.** _(JIT-2)_
- **Stage 3+ — the long tail toward V8.** Inlining heuristics, escape analysis,
  range/representation analysis, on-stack replacement for hot loops. Open-ended.

## Risks (ranked)

1. **Deopt/OSR correctness** (Stage 2+): a wrong bailout state is a miscompile, far
   worse than slow. Stage behind a deopt stress suite; start with whole-function bail
   only.
2. **Type-guard soundness**: every specialization needs a guard that bails on the
   exact cases the fast path doesn't cover (`-0`, NaN, SMI overflow, shape change).
3. **Compile-time overhead**: only tier genuinely hot functions; measure the
   crossover.
4. **GC during JIT code** (mitigated): reuse the traced `vm.stack`; never hold
   un-rooted `Gc` in raw native locals across a safepoint.
5. **Scope**: this is the multi-month item. Ship stage by stage; each stage must beat
   the previous on the bench harness or it doesn't land (same discipline as the
   interpreter levers — never ship a wash).

## Validation

Same harness and rigor as the interpreter levers (`tools/bench-compare`, RUNS=200,
A/B by binary swap, real-workload Octane confirmation). The JIT must show a _real_
win on real workloads — the bar the interpreter call-path lever failed (richards 0%)
is exactly the bar the JIT must clear.
