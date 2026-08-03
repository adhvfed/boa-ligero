# Slice 3B Gate H closure — 2026-08-03

## Result

Gate H passes. Boa `d64fe095` first moved hotness from an unbounded backend
hash map into backend-generation-scoped `CodeBlock` state. Boa `cc07a908` then
made the current frame's tiering decision bounded: after a nonzero-PC frame
crosses hotness and a one-time bytecode scan proves it has no branch back to PC
zero, the frame returns to dormant interpreter dispatch. Ligero `6594d5a2`
projects the resulting counters into its benchmark JSON.

The change does not add OSR, widen native admission, or create a new native
entry kind. A later invocation at PC zero remains eligible for the existing
whole-function entry, while the current one-shot frame stops paying scheduler
and hotness work that cannot change its entry decision.

## Durable controls

The two checked-in fixtures each run one 2,000,000-backedge loop:

| Fixture | Purpose | SHA-256 |
| --- | --- | --- |
| `one-shot-loop.js` | natively eligible numeric body, hot only after entry | `0f54effe6b51cb7d0b29b88f478474cd3e9576e8a44f48fa1a6e90b12afef223` |
| `one-shot-ineligible-loop.js` | statically ineligible bitwise body | `50aadab187a740d41dfc22d07ec02abfa90f7c96641bfac09a53451bf3dd82bf` |

Headline timing used release builds, production thresholds, diagnostics off,
one timed invocation, no warmup, and seven fresh interpreter/JIT process pairs.
All sinks matched.

| Workload | Interpreter median | JIT median | JIT tax | Native artifacts / entries |
| --- | ---: | ---: | ---: | ---: |
| eligible one-shot loop | 27.609 ms | 27.728 ms | +0.429% | 0 / 0 |
| statically ineligible loop | 36.889 ms | 37.235 ms | +0.939% | 0 / 0 |

Before the bounded transition, the same five-pair protocol measured about
27.736 ms versus 39.154 ms (+41.2%) for the eligible fixture and 37.750 ms
versus 52.531 ms (+39.2%) for the ineligible fixture. Moving the map into the
`CodeBlock` alone did not materially change those results; removing repeated
per-opcode scheduler observation after the decision closed did.

## Counter contract

In normal JIT mode each timed fixture reports:

```text
loop_backedges=256
hotness_threshold_crossings=1
saturated_loop_backedges=0
dormant_loop_frames=1
compilations=0
native_entries=0
```

`loop_backedges` is therefore a bounded tiering-observation count in headline
mode, not an exact JavaScript execution count. An explicit diagnostics run
retains exact observation and reports 2,000,000 loop backedges, one threshold
crossing, 1,999,744 saturated backedges, and zero dormant frames for both
fixtures. Diagnostic overhead is excluded from headline timing.

The distinction is deliberate: production mode proves bounded overhead;
diagnostic mode supplies exact attribution when requested. Both counters are
source-free aggregates and retain no values, objects, frames, URLs, names, or
raw pointers.

## Safety and lifetime contract

- Hotness belongs to a `CodeBlock` and is tagged with the owning backend
  generation. Replacing or dropping the backend cannot inherit stale heat or
  leave a machine-code pointer behind.
- A frame latches the hot decision once. A one-time instruction scan checks
  every static same-frame jump, including jump-table targets, before dormant
  dispatch is allowed.
- If a branch can return the current frame to PC zero, the scheduler remains
  active so the existing whole-function entry can still run.
- A separate regression invokes the same eligible function again and observes
  native compilation and entry at PC zero, proving that the one-shot latch does
  not suppress future invocations.
- Calls, returns, exceptions, recursion, runtime limits, instruction budgets,
  and GC remain interpreter/VM transitions. Dormant dispatch changes only JIT
  bookkeeping, not JavaScript semantics.
- Diagnostics keep the exact post-threshold path and do not affect the
  diagnostics-off production result.

## Verification

- focused hot/nonzero, ineligible, backend-generation, and exact-diagnostic
  tests pass;
- the full Boa JIT library suite passes: 1,186 passed, one ignored;
- the feature-disabled Boa library suite passes: 1,126 passed;
- the benchmark-runner option test passes;
- formatting and diff checks pass;
- warning-denying Boa Clippy still reports 16 pre-existing findings, none in
  the touched tiering/scheduler files;
- Ligero's JIT-enabled script suite passes: 662 tests;
- Ligero CLI checks pass with and without `jit`, and warning-denying Clippy
  passes for the affected `script` and `ligero` crates.

## Schedule consequence

Gate H is closed independently of Gate O. The large one-shot native control
remains evidence that OSR could be profitable, but the interpreter/JIT delta
can no longer be inflated by dormant-tier overhead.

Do not select OSR from this result alone. Complete diagnostics-only loop-site
eligibility and storage/helper attribution, then re-run the fixed micro,
engine, and browser matrix alongside the already-landed call attribution.
Decision checkpoint A must rank those comparable dynamic costs and approve
exactly one execution ABI.
