# Decision checkpoint A review — 2026-08-03

## Decision

Do not approve an execution ABI yet. Phase 2 remains underway, but the next
scheduled work is bounded attribution plus a hot-but-unentered tiering
guardrail. Region stitching is moved out of the pre-decision sequence and is
now an alternative branch alongside OSR, compiled calls, and guarded storage.

This is not indecision caused by noisy timings. The review found that schema 4
cannot observe the current production opportunity for two candidates, while
the strongest OSR-shaped micro result contains a separate no-native-entry
regression. Selecting a large VM ABI now would violate Gate P's measure-first
contract.

## Fixed post-Slice-2C matrix

The release runner was built from Boa `4e55c254`; its SHA-256 was
`90a776a87f41851886a55c995c66b05759472e056c1adafbc1d6953080e45d3a`.
Micro rows use five fresh processes per mode, five timed runs after 70 warmups.
Engine rows use five fresh processes per mode and one timed run. Diagnostics
were collected in separate processes and excluded from these medians.

| Workload | Interpreter | JIT | Interpreter/JIT |
| --- | ---: | ---: | ---: |
| integer arithmetic | 52.094 ms | 52.880 ms | 0.985× |
| floating-point arithmetic | 14.670 ms | 3.099 ms | 4.734× |
| numeric array sum | 37.397 ms | 36.190 ms | 1.033× |
| monomorphic property | 22.210 ms | 22.443 ms | 0.990× |
| four-shape property | 6.791 ms | 6.791 ms | 1.000× |
| flat function call | 37.601 ms | 37.309 ms | 1.008× |
| monomorphic method call | 23.755 ms | 23.290 ms | 1.020× |
| Crypto | 11.567 s | 11.171 s | 1.035× |
| DeltaBlue | 2.017 s | 2.011 s | 1.003× |
| Earley-Boyer | 13.224 s | 13.136 s | 1.007× |

All 50 pairs produced matching sinks, with no errors or timeouts. Only the
floating-point row compiled: one native artifact, 74 warm entries, and zero
deopts. Every other row compiled zero artifacts and had zero native entries;
the micro rows were at interpreter parity by design, while the three engine
rows recorded 52, 58, and 69 admission denials respectively. Independent
spot-checks confirmed the recorded source hashes and the float/call diagnostic
outputs.

The schema-4 diagnostic aggregate contained one allowed backward-branch body,
178 native-ineligible denials, eight small-straight-line denials, and two
call-boundary denials. All record-drop counts were zero and every compiled
entry PC was zero. Static blockers were led by `This` and `GetNameGlobal`, but
that ranks coverage rather than dynamically lost time. The matrix therefore
confirms that admission is safe and the one existing native shape wins; it does
not rank the next ABI.

## One-shot loop evidence

The publisher-neutral source below was passed through `/dev/stdin`; its UTF-8
bytes including the final newline have SHA-256
`0f54effe6b51cb7d0b29b88f478474cd3e9576e8a44f48fa1a6e90b12afef223`:

```js
function main() {
  let total = 0.5;
  for (let i = 0; i < 2000000; i++) {
    total = total + i;
  }
  return total;
}
```

Three fresh release processes on Boa `4e55c254` measured:

| Mode | Raw nanoseconds | Median | Native evidence |
| --- | --- | ---: | --- |
| interpreter | 27,455,041; 27,381,916; 27,827,500 | 27.455 ms | n/a |
| production JIT, one invocation | 37,944,583; 37,962,708; 38,238,583 | 37.963 ms | 0 compiles, 0 entries, 0 deopts |
| intentional threshold-1 PC-zero control | 7,549,209; 7,409,750; 7,429,000 | 7.429 ms | 1 native entry, 0 deopts |

All paths produced the same runner sink. The PC-zero control shows that this
body is a plausible native win. It does not measure OSR. The production result
is a distinct defect: the current scheduler records code-global hotness at
every backward edge, but only invokes a whole-function entry at PC zero. Once
the one-shot frame is hot at a nonzero PC, further map-backed observations
cannot make it enter native code.

Gate H therefore precedes Gate O. An OSR implementation may not claim the
37.963-to-7.429 ms delta while silently masking the zero-entry regression.
Measure unreachable thresholds, default hot-but-unentered execution, and an
ineligible loop independently. Preserve later PC-zero eligibility while
bounding repeated bookkeeping.

The result justifies completing the narrow 4A feasibility design. It does not
establish broad browser priority. The observed application backedges available
to this review were in exception-handler-ineligible frames, and W0 was designed
to enter at PC zero.

## Compiled-call evidence gap

`fn-call-flat` previously demonstrated tens of millions of native helper
entries under the legacy test shape, but production Slice 2C now correctly
denies non-continuable callers before artifact creation. The current flat-call
and method rows are near interpreter parity at 1.008× and 1.020×, with zero
artifacts or transitions. That shows safe rejection; it cannot say how many
executed call sites were monomorphic, how often the target was already native-
cached, or how much of the work would avoid a scheduler round trip.
`scheduler_call_exits == 0` is the expected production invariant today, not
evidence that calls are absent.

Schedule a schema-5, diagnostics-only call-site stream keyed by runtime-local
`(caller_code_id, call_pc)`. Count calls, ordinary/non-ordinary classification,
first/same/changed ordinary target observations, and cached native/shim target
opportunities. Keep target identity private to bounded in-memory aggregation;
never serialize it. Once the record cap is full, count every dropped
observation without retaining a dropped-site set. Production admission remains
unchanged. The existing in-crate override may characterize the old scheduler
bridge, but cannot satisfy Gate K.

## Storage and coverage evidence gap

Admission records expose static property instruction counts and the native
compiler reports guard exits. They do not count interpreted property/dense
site executions or successful helper loads, so they cannot rank direct storage
against OSR or calls. Add bounded site execution and current native helper
hit/miss/load counts only in the diagnostic run. Do not add speculative shape
or storage pointers merely for profiling.

Region stitching had also been listed before Decision checkpoint A even though
it changes materialization and exit structure. It is now Slice 4D: a measured
alternative that requires the same explicit ABI review as the other branches.

## Refined schedule

1. Check in this no-ABI decision and retain the one-shot fixture/protocol.
2. Add bounded source-free loop/call/storage attribution with dropped-record
   accounting and a mechanical Ligero projection. Headline timings keep it off.
3. Close Gate H without changing JavaScript semantics or relaxing
   `denied_call_boundary`.
4. Re-run the fixed micro, engine, and browser matrix with separate timing and
   diagnostics processes.
5. Select exactly one of OSR, compiled calls, guarded storage, or region
   stitching. Check in its runtime ownership, cache key, GC, exception, and
   finite-budget contract before implementation.

The narrow OSR contract, if selected, uses a typed nonzero-PC region key and an
explicit live-state map. Compilation is requested only after the interpreter
finishes and charges the backedge at a stable scheduler boundary. It retains no
frame, realm, environment, object, or shape pointer; guards numeric live-ins
before effects; distinguishes entry-guard replay from charged pre-effect exits;
and rejects calls, properties, handlers, allocation, eval/with, suspension,
host re-entry, and unknown stack state in the first slice.
