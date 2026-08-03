# Slice 3A call attribution checkpoint — 2026-08-03

## Result

The call-attribution sub-slice is complete in Boa `0bc757a2` and projected by
Ligero `1d58914a`. Diagnostic schema 5 can now distinguish an absent call
opportunity from a production caller that was correctly denied before artifact
creation. This closes the call-specific evidence gap from Decision checkpoint
A; loop and storage attribution remain open, so Slice 3A as a whole is not yet
complete and no execution ABI is selected.

Production admission is unchanged. Call-containing callers still install no
artifact, detailed records remain explicit and diagnostics-only, and normal JIT
mode retains no call-site state. `scheduler_call_exits` is a separate always-on
counter whose production value remains zero; it prevents a future compiled-call
path from silently paying the general scheduler transition.

## Bounded source-free contract

Each retained record is keyed by runtime-local `(caller_code_id, call_pc)` and
contains only numeric counts:

- total and ordinary/non-ordinary calls;
- first, same, and changed ordinary-target observations;
- calls whose target already had a cached native or shim entry for the current
  instruction-budget mode.

The last ordinary target identity is private bounded state and is never
serialized. The default record limit is 256 and the hard caller-configurable
ceiling is 4,096. After the cap fills, every unretained observation increments
`dropped_call_observations`; no unbounded dropped-site set is retained. The
embedding test usefully fills Ligero's 256-record cap through realm/bootstrap
activity and confirms the drop-count contract.

The ordinary-function predicate is the existing narrow call-lowering
predicate. Reading the current target does not invoke compilation or mutate the
production cache. Legacy target feedback is available only under the in-crate
admission override used by semantic tests.

## Dynamic call evidence

Separate release diagnostic processes used one timed invocation after three
warmups. Sources retain the hashes recorded in the fixed matrix:

| Workload | Warm calls | Ordinary | Same target | Changed | Cached native/shim | Drops |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `fn-call-flat` | 2,000,000 | 2,000,000 | 1,999,999 | 0 | 0 / 0 | 0 |
| `method-call-mono` | 800,000 | 800,000 | 799,999 | 0 | 0 / 0 | 0 |

Both workloads therefore contain a real, completely monomorphic dynamic call
site even though production correctly reports zero compilations, native
entries, and scheduler call exits. Neither target was native-cached, so this
evidence does **not** yet estimate a native-to-native call win. A compiled-call
proposal would need to account for target admission/compilation as well as the
continuation ABI.

## Diagnostics-off performance control

The first sequential interpreter/JIT sample was thermally noisy, so the
regression decision used a direct A/B between pre-slice `dbb889e3` and
`0bc757a2`. Both release runners used the same build tree. Their SHA-256 values
were `0a4135acce04dc4f19632c00cdc2eefe614532be64b348cb931fbe377a293897`
and `aba323fea8415498d281b20bf536898eb2b39e51d0a3f5e90f0122875fa7266f`.
Five fresh-process pairs alternated order, with ten timed runs after 70 warmups
and diagnostics disabled.

| Workload | Pre-slice median | Current median | Current/pre-slice |
| --- | ---: | ---: | ---: |
| `method-call-mono` | 23.623 ms | 23.995 ms | 1.016× |
| `property-mono` | 22.415 ms | 22.673 ms | 1.012× |

All sinks matched. Every current run reported zero compilations, native
entries, deoptimizations, and scheduler call exits. Both controls remain within
the 5% diagnostics-off parity gate. A separate five-pair flat-call
interpreter/JIT control differed by 0.25% at the median and retained the same
zero-artifact invariants.

## Verification

- Boa focused call, production-denial, native-call, hard-bound, and runner
  tests passed.
- Boa's full JIT library suite passed: 1,182 passed, one ignored.
- Boa's non-JIT library suite passed: 1,138 passed.
- JIT and no-default-features checks passed; warning-denying Clippy reported
  only the 17 pre-existing engine/JIT findings.
- Ligero's full JIT script suite passed: 662 tests.
- Ligero feature-on CLI and feature-off script checks passed.
- Warning-denying Clippy passed for both affected Ligero crates.

## Next decision

Do not start a compiled-call ABI from this result alone. Complete the remaining
bounded loop/storage attribution or close Gate H first, then re-run the fixed
matrix with timing and diagnostics in separate processes. The new call counts
must be compared with dynamic loop/storage evidence, and any call design must
explain how an ordinary monomorphic target becomes native-cached without
relaxing `denied_call_boundary` prematurely.
