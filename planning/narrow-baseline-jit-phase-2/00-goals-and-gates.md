# Goals, boundaries, and gates

## Problem statement

Phase 1 can execute useful native fragments, but a fragment win is not yet a
workload win. The runtime still falls back too early for common bytecode shapes,
leaves hot loops in the interpreter when they become hot after entry, and
routes compiled calls through VM scheduling. The next phase must reduce those
boundaries without weakening the Phase 1 GC, exception, budget, or deopt
contracts.

## Primary goal

Make the opt-in baseline tier useful on browser-shaped workloads by increasing
the amount of hot work that executes natively and decreasing the number of
native/interpreter transitions, while keeping the interpreter authoritative for
all unsupported or invalidated behavior.

Phase 2 is successful when it can demonstrate all of the following:

1. A workload profile identifies why code blocks fail native compilation or
   leave native execution, by opcode/region/PC and guard family.
2. The common operations that block the selected hot loops lower to native IR
   or to reviewed helpers with exact deopt behavior; they no longer cause the
   entire useful region to become shim-only.
3. A hot eligible loop can enter a compiled loop region from an already-running
   frame at an exact bytecode boundary, including materialized state and
   runtime-limit accounting.
4. A guarded ordinary function can call an already-compiled ordinary callee
   without returning through the interpreter scheduler for the normal hit
   path; misses and all other call kinds retain the existing fallback.
5. The resulting tier improves at least one browser-shaped workload after
   compilation cost is included, with no unacceptable cold-start regression.

## In scope

- opt-in instrumentation for native coverage, fallback reasons, helper costs,
  OSR attempts, and call/return transitions;
- ordinary, non-async, non-generator functions and conservative loop regions;
- environment/global reads needed by hot numeric/property loops, subject to
  binding/version guards;
- exact integer bitwise/conversion operations when operands are proven safe;
- native loop-header entry and deoptimization to the interpreter;
- compiled-to-compiled direct calls with normal visible VM frames;
- helper-to-IR and then direct guarded dense/property loads where measured;
- code-cache admission and tiering policy based on observed benefit;
- cold/warm measurements against Boa interpreter mode and a real browser-shaped
  workload supplied by the sibling `ligero-browser` effort.

## Explicit non-goals

Do not expand this phase into:

- a speculative optimizing compiler, broad inlining, escape analysis, or
  allocation sinking;
- generators, async functions, suspension, `eval`, `with`, proxies, bound or
  native functions on a native fast path;
- a new GC root set, raw object/shape pointers held across safepoints, or
  unchecked direct access to private property storage;
- process-wide or cross-realm code sharing;
- persistent on-disk machine-code caching;
- making JIT execution default or changing the feature-disabled interpreter;
- optimizing code before a workload profile establishes that it matters.

Inlining may be reconsidered after the compiled-call ABI, frame maps,
exceptions, recursion, and GC safepoints have independent coverage. It is not a
Phase 2 prerequisite.

## Semantic boundaries

Every Phase 1 rule continues to apply. In addition:

- an OSR entry must describe the exact bytecode PC and all live registers;
- a compiled call must preserve a normal `CallFrame` for stack traces,
  recursion limits, exception handlers, and GC tracing;
- a native-to-native return must use the same VM-owned return transition as the
  interpreter;
- a feedback snapshot must be immutable for the lifetime of a compiled entry;
- a guard must be checked before any operation that can mutate, allocate,
  invoke user code, or throw;
- if the runtime cannot prove the entry/return state, it must deopt before the
  operation rather than attempt partial recovery.

## Risk tripwires

Stop the current slice and investigate if:

- the workload profile cannot distinguish compile rejection from runtime deopt;
- the same operation has multiple ad-hoc helper ABIs or PC-update rules;
- an OSR or call transition needs to reconstruct a value not materialized in
  the VM stack;
- a compiled call hides a frame from stack traces or recursion accounting;
- direct storage loads require a raw pointer to survive a helper or safepoint;
- a native region improves a synthetic loop but increases browser cold time;
- the feature-disabled build gains counters, branches, or new runtime state.

## Phase 2 gates

The gates are comparative and workload-based:

- **Profile gate:** the top native blockers and transition costs are measured
  on at least one browser-shaped workload before new lowering is selected.
- **Coverage gate:** the selected numeric/array/property workload executes a
  substantial hot region natively rather than merely compiling a shim entry;
  the exact coverage target is recorded with the profile, not guessed here.
- **OSR gate:** a one-shot hot loop enters native code and returns correct
  results/errors under budgets, exceptions, GC, and guard failure.
- **Call gate:** matching ordinary calls avoid the interpreter scheduler on the
  native hit path; mismatches, recursion, exceptions, and host calls remain
  correct.
- **Workload gate:** at least one browser-shaped workload wins after compile
  cost, and no agreed cold-start guardrail is violated.
- **Regression gate:** JIT-off behavior and all Phase 1 differential tests stay
  green.

