# Slice 2C closure — 2026-08-03

Status: complete. Decision checkpoint A remains open; JIT remains build- and
runtime-opt-in.

## Landed slices

- Boa `345767c5` denies call-containing entries before compilation, publishes
  `denied_call_boundary`, and lowers only compile-time `GlobalDeclarative`
  `GetName` through the current frame and realm. Guard failure reports
  `binding_read` and replays the same PC in the interpreter.
- Ligero `2c39eafe` projects diagnostic schema 4 without depending on Boa's
  enum representation.
- Boa `54a109f6` is the separately revertible behavior-neutral checkpoint. It
  borrows one generated helper table through emission and removes the unused
  536-byte compiler copy. Strict Clippy's pre-existing warning count falls from
  22 to 17; the refactor introduces no new warning.

The production call denial is deliberately temporary. It may be relaxed only
when a separately reviewed compiled-call ABI can resume the caller natively and
passes Gate K. The binding allowlist remains limited to global declarative
reads; global-object, stack, module, eval-affected, write, deletion, OSR, and
nonzero-PC forms remain unsupported.

## Correctness and feature matrix

The semantic matrix covers current-value mutation, representation-changing
deopt, TDZ `ReferenceError` replay, direct-eval rejection, realm separation,
forced GC with a boxed value, and exact finite-budget refund. Production
admission coverage proves that a hot call-containing loop creates no native or
shim artifact.

- focused JIT suite: 38 passed, 1 performance test ignored;
- full Boa engine with JIT: 1,180 passed, 1 ignored;
- full Boa engine without JIT: 1,138 passed;
- `--no-default-features` and `--no-default-features --features jit`: pass;
- Ligero script suite with JIT: 662 unit tests and the integration harness pass;
- formatting and diff checks: pass;
- warning-denying Boa JIT Clippy: no new warnings; still blocked by 17 recorded
  pre-existing warnings outside this slice (down from 22 before the refactor).

## Post-refactor release controls

Each row is the median of five fresh processes, with 70 warmups and five timed
runs per process. Diagnostics are disabled in headline timings.

| Control | Interpreter ns/run | Production JIT ns/run | Result | Artifacts |
| --- | ---: | ---: | ---: | ---: |
| floating-point binding loop | 15,033,758 | 3,129,991 | 4.80× faster | 1 native; 74 entries; 0 deopts |
| monomorphic method call | 24,920,175 | 25,502,275 | +2.34% | 0 |
| flat function call | 44,390,508 | 39,449,525 | −11.13% | 0 |
| monomorphic property | 22,157,491 | 22,961,225 | +3.63% | 0 |
| four-shape property | 6,833,300 | 6,932,583 | +1.45% | 0 |

The positive control retains a matching accumulator and zero steady-state
deopts. Every negative control is within the 5% regression guardrail; call-
containing controls are denied before artifact creation.

## W0 browser gate

Five interleaved fresh-process control/JIT pairs measure a 45.739 ms
interpreter median and a 32.005 ms JIT median, a 30.0% reduction including
compilation. Every JIT run retains one native artifact, 999 native entries,
zero deopts, 387 display items, 258 paint segments, and the established visible
checksum `499500000`.

A separate diagnostic run reports schema 4, one allowed 24-instruction loop,
two expected exception-handler denials, 999 normal returns, and zero dropped
records. Its 33.530 ms load is not included in the headline timing.

## Next decision

Slice 2C demonstrates that one VM-owned binding read can complete a useful
native loop, but it does not choose the next ABI. Decision checkpoint A must
re-run the fixed profile and rank loop OSR, compiled calls, and helper-backed
storage by attributable lost time and transition count. No new boundary should
be implemented until that evidence and its GC/exception/budget contract are
recorded.
