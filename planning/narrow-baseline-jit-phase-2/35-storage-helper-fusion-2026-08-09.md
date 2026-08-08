# Storage helper fusion checkpoint — 2026-08-09

Status: accepted in Boa `7904fe9c`, `86a1ea7b`, `597055cb`, and `f2843a77`.
One follow-up F64 argument experiment was measured and removed.

## Problem

The native named-property and dense-element paths called one helper to validate
the current object/shape/value representation and a second helper to repeat the
same lookup and load the value. Stable hits therefore borrowed the object,
looked up the inline-cache entry, and cloned the `JsValue` twice per access.

## Selected contract

Integer helpers now return either the complete `i32` payload in the low 32 bits
of a `u64` or the existing bit-61 guard-failure tag. Signed payloads cannot
overlap the tag.

Floating-point helpers receive an aligned eight-byte stack slot owned by the
active generated frame. The helper writes the `f64` only after every guard
passes and returns a boolean status. Generated code loads the slot only on the
success branch. This preserves arbitrary NaNs without reserving a NaN payload
as a failure sentinel.

Both forms retain all object, shape, property-storage, and `JsValue` borrows
inside one Rust helper call. No raw GC pointer or borrowed object crosses into
generated code or survives a safepoint. Guard failure still deoptimizes at the
original bytecode PC before the property read is replayed by the interpreter.

## Measurements

Seven fresh release-process samples per before/after set were collected on an
Apple M4 (`Darwin arm64`). Each ignored benchmark warms compilation and caches,
then times a second evaluation containing one million matching loads. Exact
sinks, zero steady-state deopts, and retained native code bytes were checked in
every sample.

| Path | Before median | After median | Change | Code bytes |
| --- | ---: | ---: | ---: | ---: |
| dense `i32` | 12.429 ms | 7.582 ms | -39.0% | 1,836 -> 1,784 |
| named `i32` | 14.522 ms | 6.034 ms | -58.5% | 1,720 -> 1,636 |
| dense `f64` | 16.206 ms | 12.222 ms | -24.6% | 1,392 -> 1,360 |
| named `f64` | 10.998 ms | 6.340 ms | -42.4% | 1,268 -> 1,240 |

The named-integer before set was noisier than the other rows, so its percentage
is directional. Its large timing separation and 84-byte code-size reduction
still pass the acceptance bar.

## Correctness and containment evidence

- `i32::MIN` survives both dense and named fused returns without colliding with
  the failure tag;
- NaN survives both floating-point paths and remains a successful native load;
- dense holes and named shape changes leave the output slot unread, deopt, and
  produce the interpreter result;
- existing forced-GC property coverage remains on the same helper boundary;
- diagnostic guard-hit, guard-miss, and load counts retain `loads == hits`;
- the focused JIT suite and strict affected-target Clippy pass after every
  accepted slice.

## Rejected follow-up

The same stack-output pattern was prototyped for F64 argument loads. On a
200,000-call fixture, the median moved from 24.728 ms to 25.586 ms (+3.5%) even
though code size fell from 1,076 to 1,036 bytes. The stack-slot cost outweighed
the removed helper call, so the implementation, tests, and benchmark were all
removed rather than retained without a performance win.

## Consequence

The duplicated storage-helper traversal is no longer the next continuity
boundary. Additional material gains require broader admitted bytecode regions,
direct native call/return continuation, or reviewed direct storage access. A
new helper fusion should not be selected without a workload profile showing a
different repeated boundary.
