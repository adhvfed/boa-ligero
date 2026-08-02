# Gate P profile — 2026-08-03

This checkpoint completes the first representative profile matrix for Phase 2.
It ranks evidence; it does not enable the JIT or pre-approve a new VM ABI.

## Protocol

- arm64 macOS 15.3, Boa commit `bee20eef`;
- release `bench-compare-runner` built with `--features jit` (SHA-256
  `466655b58839069d5890e00b824414c95e6a2f5618aa236cdf388c09b4fccb31`);
- seven micro controls, each with 70 warmups and five timed runs;
- three bounded V8-derived workloads, each with one cold and one timed run;
- interpreter, normal JIT, and diagnostic JIT run separately;
- exact input SHA-256 values retained with the raw output;
- every interpreter/JIT accumulator matched and no diagnostic records dropped.

The diagnostic run is not a performance result. The property diagnostic was
already measured to add 12.1% overhead under an intentionally hostile native-
entry count, so headline comparisons below use normal JIT only.

## Micro controls

| Workload | Interpreter ns/run | JIT ns/run | Ratio | Native warm entries | First blocker |
|---|---:|---:|---:|---:|---|
| Integer arithmetic | 119,270,008 | 113,069,025 | 1.055x | 0 | `GetName` at PC 10 |
| Floating arithmetic | 25,863,316 | 28,299,266 | 0.914x | 0 | `GetName` at PC 10 |
| Dense numeric sum | 51,820,975 | 50,475,700 | 1.027x | 0 | `GetName` at PC 10 |
| Flat ordinary call | 53,395,716 | 55,367,041 | 0.964x | 37,499,937 | caller: `GetName` at PC 10 |
| Monomorphic method | 23,384,550 | 31,321,850 | 0.747x | 0 | method: `This` at PC 18 |
| Monomorphic property | 26,489,233 | 50,692,383 | 0.523x | 14,999,937 | caller: `GetName` at PC 10 |
| Four-shape property | 6,736,591 | 9,751,808 | 0.691x | 3,749,937 | caller: `GetName` at PC 10 |

The warm compile records contain three native helper bodies and eight shims.
The native bodies cover only 39 static bytecode instructions; their dynamic
exits are normal returns. This is important negative evidence: millions of
native entries do not imply a workload win when the caller stays interpreted
and every helper crosses the tier boundary.

## Engine subset

| Workload | Interpreter | JIT | Ratio | Native/shim blocks | Warm native entries / deopts |
|---|---:|---:|---:|---:|---:|
| Crypto | 15.796 s | 11.180 s | 1.413x | 1 / 50 | 75 / 75 |
| DeltaBlue | 2.008 s | 2.011 s | 0.999x | 3 / 55 | 16,947 / 0 |
| Earley-Boyer | 13.147 s | 15.417 s | 0.853x | 1 / 68 | 295 / 295 |

Across the warm diagnostic records, five native blocks cover 14 static
instructions while 174 blocks remain shims. The leading first blockers are:

1. `This`: 64 records;
2. `GetNameGlobal`: 46 records;
3. `StoreNull`: 15 records;
4. `GetLengthProperty` and `PutLexicalValue`: six records each.

Dynamic exits tell a different, complementary story: 17,235 normal returns,
301 argument-type deopts, and 95 entry-guard deopts. These static blocker counts
cannot be treated as executed-opcode coverage or an estimated speedup.

## Browser evidence

The existing W0 numeric/DOM gate still compiles its 24-instruction kernel as a
budgeted native entry. A current diagnostic run recorded 999 normal returns,
zero deopts, the visible checksum `499500000`, and the established 387 display
items / 258 paint segments.

A broader, user-authorized chess application supplied the real module/Wasm/DOM
load. Five interleaved fresh-process triples at 1280×720 produced:

| Mode | Median load | Range | Paint structure |
|---|---:|---:|---|
| Interpreter | 498.48 ms | 443.60–780.95 ms | 116 items / 74 segments |
| Normal JIT | 516.46 ms | 490.66–558.32 ms | 116 items / 74 segments |
| Diagnostic JIT | 499.67 ms | 479.07–532.63 ms | 116 items / 74 segments |

Network variance makes this a compatibility/coverage result, not a speed
claim. A separate live query confirmed the initialized landing section at
1280×720 and its primary action text. Every JIT run observed the same 495
function entries and 1,042 loop backedges, but compiled only two startup shims
with exception-handler metadata: zero native compilations, entries, deopts, or
diagnostic drops.

## Decision

1. Add an admission checkpoint before widening coverage. The helper-heavy
   negative controls prove that current native entry overhead can outweigh a
   fully native tiny body. Select a rule from a measured crossover; do not guess
   an instruction-count threshold.
2. After admission, `This` is the smallest candidate lowering. It has the
   broadest engine frontier and independently blocks the method control. Review
   exact frame-value representation, GC, strict/sloppy receivers, and finite-
   budget replay before implementation.
3. Keep `GetName`/`GetNameGlobal` separate. They block all three numeric micro
   callers and rank second in the engine subset, but need a VM-owned binding
   identity/version contract and mutation/eval/deletion differentials.
4. Retain OSR as a measured branch. The application reached 1,042 backedges
   without a native app entry, but one page load does not yet attribute enough
   time to approve a nonzero-PC ABI.
5. Re-run the same positive and negative controls after each slice. A higher
   native-entry count is not an acceptance criterion.

## Input fingerprints

```text
int-arith.js          85db2d6052caf30d50dace816cf9e01a178edc4f1a5837a7e1937f1c3e9c40b0
float-arith.js        6dfa997a955820fe0e1b6e61f691d89ef69d3ab36832efaf2433c9a9c3063f9d
array-numeric-sum.js  b7f7954dbcffd4ffc93c9553801fe90f457e158a79aad55a7e3f99f57b8b6e0f
property-mono.js      63378817ef478d6551ee675cdcff32b2322f2c628f811265958a98a1a95e905f
property-poly4.js     881830c3552292fcfbcabacf51d96eeb41dee212dc67d531343695c31eb8beb8
fn-call-flat.js       d26e20d195319d0a30162feb2d71b92bac60d46a34a9eda120e944969f759fd0
method-call-mono.js   f53209bdc92776d87f034869651b183404a2497faac0ca06d9233ce6b8de10df
crypto.js             25b8ef32bd391caf7932ac8c58d20a1be37947d5f71a8034afeaf52c8faf213a
deltablue.js          295f74ca232c09e6ffb01a55308b8df3851cc56e5a9998dafd5cada98ac03036
earley-boyer.js        0e614bfc92b01d3fb44cfa55e18ba8fd9f84c0a4b075b1f8ed73e88e1af31ce2
```
