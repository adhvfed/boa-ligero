# Slice 4A1 micro rollback checkpoint — 2026-08-03

## Result

The checksummed Decision-A micro matrix passes after one scheduler correction
in Boa `073c12cd`. This is a partial 4A1.5c checkpoint, not Gate O closure:
Crypto, DeltaBlue, Earley-Boyer, W0, and separate zero-drop diagnostics remain
binding before 4A1.R or Decision checkpoint B.

The first post-OSR matrix attempt exposed a 35–50% JIT-enabled regression in
property and method controls even though they compiled and entered no native
artifact. The failure was useful: whole-function admission had permanently
denied small helpers containing property operations or `this`, but every new
helper frame still used the per-opcode loop-observing interpreter wrapper. The
wrapper could never find an OSR edge in those straight-line bytecode bodies.

Boa `073c12cd` caches a distinct `DeniedNoLoop` admission state after one
static same-frame backward-edge scan. Such a frame closes loop observation and
uses dormant interpreter dispatch. A call may drain it directly only when
per-opcode diagnostics are disabled; diagnostics retain the complete observed
site stream. A native-ineligible function that does contain a backward edge
remains `Denied` and still reaches loop OSR. Getter re-entry, exception unwind,
diagnostic retention, and both sides of that classification have focused tests.

An intermediate version removed the per-opcode helper tax but left a scheduler
round trip per proven loop-free call. Seven pairs put monomorphic property at
+5.973%, just outside the fixed 5% rollback bound. The production-only direct
drain removes that residual tax without bypassing diagnostic observation.

## Build and protocol

- Boa: `073c12cd2fcb2a97fb34d6ced8ccf594dfc62290`;
- release runner SHA-256:
  `df9d411076221b71bbd614bccbf95443a1d468442212a64f83758a42ae419a59`;
- fixtures retain the Decision-A SHA-256 values recorded below;
- seven fresh-process, order-alternating interpreter/JIT pairs;
- ordinary micro controls: 70 warmups and five timed calls per process;
- one-shot controls: no warmup and one timed call per process;
- diagnostics disabled for every headline sample;
- medians below are per call and every pair retained the same accumulator.

The legacy `jit` runner mode remains intentional for Decision-A rollback
comparability. The independently passed isolated cold-OSR result in checkpoint
26 remains the authoritative compile-inclusive admission measurement.

## Headline matrix

| Workload | Fixture SHA-256 | Interpreter | JIT | Delta | Artifact evidence |
| --- | --- | ---: | ---: | ---: | --- |
| integer arithmetic | `85db2d6052caf30d50dace816cf9e01a178edc4f1a5837a7e1937f1c3e9c40b0` | 51.208 ms | 52.361 ms | +2.252% | zero artifacts |
| floating-point arithmetic | `6dfa997a955820fe0e1b6e61f691d89ef69d3ab36832efaf2433c9a9c3063f9d` | 14.656 ms | 3.104 ms | −78.820% | one native body |
| numeric array sum | `b7f7954dbcffd4ffc93c9553801fe90f457e158a79aad55a7e3f99f57b8b6e0f` | 32.915 ms | 33.167 ms | +0.767% | zero artifacts |
| monomorphic property | `63378817ef478d6551ee675cdcff32b2322f2c628f811265958a98a1a95e905f` | 21.945 ms | 22.048 ms | +0.470% | zero artifacts |
| four-shape property | `881830c3552292fcfbcabacf51d96eeb41dee212dc67d531343695c31eb8beb8` | 6.638 ms | 6.712 ms | +1.107% | zero artifacts |
| flat function call | `d26e20d195319d0a30162feb2d71b92bac60d46a34a9eda120e944969f759fd0` | 38.633 ms | 38.590 ms | −0.110% | zero artifacts |
| monomorphic method call | `f53209bdc92776d87f034869651b183404a2497faac0ca06d9233ce6b8de10df` | 23.074 ms | 22.662 ms | −1.787% | zero artifacts |
| eligible one-shot loop | `0f54effe6b51cb7d0b29b88f478474cd3e9576e8a44f48fa1a6e90b12afef223` | 29.180 ms | 6.049 ms | −79.269% | one loop entry |
| ineligible one-shot loop | `50aadab187a740d41dfc22d07ec02abfa90f7c96641bfac09a53451bf3dd82bf` | 40.958 ms | 42.446 ms | +3.635% | zero artifacts |

Every negative/noncandidate median is inside the fixed 5% rollback bound. The
eligible one-shot result is a 4.824× win in the legacy comparable mode; it does
not replace the isolated `osr-cold` gate's exact OSR counters.

## Raw headline samples

Values are elapsed nanoseconds per process. Ordinary micro rows contain five
timed calls; one-shot rows contain one.

```text
int-arith interp: 256038958 259419209 249777584 261043625 248319625 246246500 264084583
int-arith jit:    260906584 261806208 260666166 262338458 271225167 262566750 258868375
float-arith interp: 73348500 73278375 70986875 70247208 77506333 71194750 74209458
float-arith jit:    15719917 15468625 15553459 15595375 15520667 14047792 15488208
array-sum interp: 166064875 167355917 164620959 161623875 164573916 161587167 161539708
array-sum jit:    166849208 163273416 165837000 181264334 167392834 161428083 163530083
property-mono interp: 108998250 115573208 109724875 127270000 109871459 108850833 108865667
property-mono jit:    109337416 110692917 109829333 110240875 111593916 111189959 108244000
property-poly4 interp: 33832292 32939041 33632417 33151667 33407292 33191250 33140375
property-poly4 jit:    34104083 33203916 33558750 33991750 34150833 33541916 33517333
fn-call interp: 190129625 191866292 190411792 193163708 193683625 250279792 204206000
fn-call jit:    192826916 192204542 186418250 192951542 213904584 214544458 216489833
method-call interp: 114278708 117557041 147099375 115370000 114606291 113669333 118951041
method-call jit:    115702750 112047000 120844709 112515709 111756250 116895458 113308417
one-shot interp: 28208666 29179666 29624625 28013083 28832916 30261375 30150666
one-shot jit:    6015375 5992000 6050291 6078875 6033958 6049375 6085208
ineligible interp: 40858250 40747125 43106583 40957791 45031292 42594041 38263583
ineligible jit:    41688541 44563125 44530208 42446459 41803958 39320792 44083208
```

## Verification and next gate

- six focused denied-frame/OSR/diagnostic tests pass;
- feature-disabled and JIT-enabled engine checks pass;
- strict engine Clippy reports exactly the 16 already recorded findings and no
  slice-local finding;
- the release runner was rebuilt against the recorded commit after the code
  commit and retained the hash above.

Next, run the five-pair Crypto, DeltaBlue, and Earley-Boyer controls, then the
seven-pair clean Ligero W0 gate, then separate schema-8 diagnostics at the
recorded 4,096 cap. Any negative row above +5%, any W0 structural mismatch, a
W0 win below 20%, or any dropped diagnostic observation still rolls back only
the scheduler edge. Only a complete pass permits the separately revertible
4A1.R behavior-neutral refactor.
