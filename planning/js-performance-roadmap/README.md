# JavaScript performance roadmap

A plan to make Boa's JavaScript execution _blistering_, grounded in (a) Boa's
actual architecture as it exists today, (b) the published literature on
interpreter and dynamic-language performance, and (c) our own measured
benchmark gaps.

## The thesis (read this first)

Be honest about ceilings, because it determines where effort pays off:

- A **pure bytecode interpreter** — which Boa is — tops out near V8's _Ignition_
  tier, i.e. roughly `node --jitless`. That is the realistic Phase-1 finish line.
- A **baseline (non-optimizing) JIT** like V8's Sparkplug buys only **~5–15%**
  over the interpreter on real workloads — it mostly removes dispatch overhead.
- The **10×+ multiples** people mean by "blistering JS" come from an
  **optimizing JIT** (TurboFan/Maglev, IonMonkey, JSC FTL): type-specialized
  native code, inlining, escape analysis. That is a multi-person-year effort.

So the roadmap is staged:

| Phase                            | Goal                                                                        | Where the wins are                                                 |
| -------------------------------- | --------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| **1 — Saturate the interpreter** | Reach Ignition parity (≤1.5× `node --jitless` geomean, no fair bench >2.5×) | Call path, type specialization, ICs, allocation, strings, dispatch |
| **2 — Baseline compiler**        | +5–15% over a saturated interpreter                                         | Template/baseline JIT once Phase 1 is done                         |
| **3 — Optimizing JIT**           | Compete with full V8                                                        | Only if the project commits to it; out of scope here               |

**Almost all near-term ROI is in Phase 1.** We are currently **3.43× slower than
`node --jitless`** on the fair microbenchmark subset (see
`tools/bench-compare/BASELINE.md`, captured at commit `a5dd302c`, 2026-05-19),
with the worst fair benchmark `method-call-mono` at **6.55×**. Phase 1 is about
closing that 3.43× → 1.5×.

## What Boa already has (so we don't re-invent it)

These are _done_ — the literature's "big interpreter wins" that Boa has banked:

- **Register-based bytecode VM** (not stack-based). Shi et al. (TACO 2008) put
  this at ~25–32% over a stack VM. Main loop: `core/engine/src/vm/mod.rs:1025`.
- **Function-pointer dispatch table**, not a `match` ladder:
  `OPCODE_HANDLERS[256]` at `core/engine/src/vm/opcode/mod.rs:412`, dispatched
  at `core/engine/src/vm/mod.rs:1041`. (Still a _central_ indirect call — see
  [05-dispatch](05-dispatch.md).)
- **NaN-boxed `JsValue`**, 8 bytes, with inline 32-bit small integers (SMIs):
  `core/engine/src/value/inner/nan_boxed.rs:49`. Numbers are **not** boxed. So
  "add NaN-boxing / SMIs" is _not_ on this roadmap — it's already here.
- **Hidden classes / shapes** with shared transition chains:
  `core/engine/src/object/shape/mod.rs`.
- **Polymorphic inline caches (PIC, 4-way)** for property get/set _by name_:
  `core/engine/src/vm/inline_cache/mod.rs`, hot path in
  `core/engine/src/vm/opcode/get/property.rs:77`. Recent work already dropped
  `Gc` refcount traffic on IC hits.

## What's missing — the Phase-1 levers, ranked

Ranked by expected ROI against our _actual_ benchmark gaps and by how pervasive
the construct is in real code. Each links to a deep-dive chapter.

| #   | Lever                                                             | Primarily targets                                        | Expected         | Effort      | Risk |
| --- | ----------------------------------------------------------------- | -------------------------------------------------------- | ---------------- | ----------- | ---- |
| 1   | [Call path: inline-cache + specialized call](02-call-path.md)     | `method-call-mono` 6.55×, `fn-call-flat` 4.01×           | **High**         | Med         | Med  |
| 2   | [Type specialization / quickening](03-bytecode-specialization.md) | `global-counter` 4.87×, `array-numeric-sum` 4.41×, arith | **High (broad)** | Med–High    | Med  |
| 3   | [Inline-cache expansion](04-inline-caches.md)                     | element access, `array-numeric-sum`, megamorphic sites   | Med              | Med         | Low  |
| 4   | [GC & allocation](06-gc-and-allocation.md)                        | allocation-heavy code, call-frame & env churn            | Med              | High        | High |
| 5   | [Strings](07-strings.md)                                          | `string-concat` (O(n²) today)                            | Med (narrow)     | Med         | Low  |
| 6   | [Dispatch](05-dispatch.md)                                        | every opcode (branch mispredicts)                        | Med, uncertain   | High (Rust) | Med  |
| —   | [Beyond the interpreter](08-beyond-the-interpreter-jit.md)        | Phase 2/3 framing                                        | —                | —           | —    |

Cross-cutting, read before doing any of the above:
**[01 — Measurement methodology](01-measurement-methodology.md)** — how to benchmark
honestly, and the cautionary tale that produced this roadmap.

## The lesson that produced this document

We enabled and tested a dormant bytecode **Move-elision** peephole pass. Against
the _entire Octane suite_ (1017 compiled CodeBlocks) it found **19** elision
sites; three workloads found **zero**. The cause is structural and the
literature predicts it: a register VM already avoids the redundant moves such a
pass targets, and Boa's per-call-site receiver-passthrough fast paths cover the
rest. We reverted it (commits reverting `0b2aacfa`/`88ad8666`).

The takeaway, encoded throughout this roadmap: **chase structural levers with a
mechanistic reason to pay off, measure the opportunity cheaply before paying for
a full A/B, and never ship a micro-opt that's a wash.**
