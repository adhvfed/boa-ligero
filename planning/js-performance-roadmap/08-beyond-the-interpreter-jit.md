# 08 — Beyond the interpreter (Phase 2 / 3)

Setting expectations honestly, because "blistering JS" usually means "as fast as
V8," and that has a price tag the interpreter chapters can't pay.

## The ceiling of Phase 1

Everything in chapters 02–07 is _interpreter-tier_ work. Its ceiling is roughly
`node --jitless` (Ignition). Phase 1 is worth doing — we're 3.43× away from that
ceiling and most programs spend their time in the interpreter — but it cannot,
by construction, match a JIT.

Concretely (from the literature):

- V8's **Sparkplug** baseline compiler buys only **~5–15%** over Ignition on real
  workloads. It compiles bytecode to straight-line machine code with no IR and no
  optimization — it just deletes dispatch and decode overhead.
- The **10×–100×** gaps versus a pure interpreter come from **optimizing JITs**
  (TurboFan/Maglev, IonMonkey, JSC DFG/FTL): they speculate on the type feedback
  the interpreter collected, emit specialized native code, inline callees, do
  escape analysis / scalar replacement, and deoptimize when speculation fails.

## Phase 2 — a baseline compiler (only after Phase 1 saturates)

A template/baseline JIT (Sparkplug-style) is the natural next step _once the
interpreter is saturated_, because:

- It reuses Phase 1's inline caches and feedback verbatim — same ICs, just
  emitted as native code instead of interpreted.
- It's a bounded, well-understood project (no optimizing IR, no register
  allocator) — each bytecode maps to a fixed machine-code template.
- Realistic gain: the ~5–15% Sparkplug figure. Worth it for a browser that needs
  consistent frame budgets, but **not** a substitute for finishing Phase 1.

Prerequisite: the type-feedback infrastructure from
[03-bytecode-specialization](03-bytecode-specialization.md) and the ICs from
[02](02-call-path.md)/[04](04-inline-caches.md) must exist first — a baseline JIT
with nothing to specialize on is just a slower interpreter.

## Phase 3 — an optimizing JIT (a different project)

This is where "blistering" (V8-class) actually lives, and it is a multi-person-
year commitment: an optimizing IR (sea-of-nodes or similar), speculative type
specialization driven by feedback, inlining, escape analysis, a register
allocator, and — the hard part — **deoptimization / on-stack replacement** to
fall back to the interpreter when speculation fails (Hölzle/Ungar; the
interpreter must be able to _resume_ mid-function from JIT state).

Alternatives that change the calculus:

- **Truffle/Graal** (Würthinger et al., PLDI 2017): write a self-specializing AST
  interpreter and get a JIT "for free" via partial evaluation — but it requires
  the Graal/Truffle substrate (JVM), not applicable to a standalone Rust engine.
- **Meta-tracing** (PyPy/RPython, Bolz et al. 2009): also a whole-substrate
  commitment.

Neither is a drop-in for Boa. They're noted so the option space is on the record.

## Recommendation

Do not start Phase 2 or 3 now. The expected value is overwhelmingly in Phase 1:
we have a measured 3.43× gap to a tier we can reach with interpreter-only
techniques that are low-risk relative to a JIT. Revisit Phase 2 when the fair
geomean is at/near 1.5× and the call + specialization + IC infrastructure is in
place to feed it. Treat Phase 3 as a separate strategic decision, not a roadmap
item.
