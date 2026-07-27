# 07 — Strings (lever #5)

A narrow lever — it mostly fixes one pathological case — but that case is
quadratic, common in real code, and the fix is well-understood and low-risk.

## Current state (grounded)

`boa_string` (`core/string/src/lib.rs`):

- **Dual encoding**: a string is either Latin1 (`u8`) or UTF-16 (`u16`), chosen
  per string (`JsStringKind`, `lib.rs:104`). Good — ASCII-dominant text (most JS)
  stays one byte per char.
- **Eager concatenation**: `concat_array` (`lib.rs:625`) allocates a new buffer
  and `ptr::copy_nonoverlapping`s every input; mixed encodings upgrade Latin1→
  UTF-16 on the fly. So `s += x` in a loop is **O(n²)** in total length — N
  allocations, each copying the whole accumulated prefix.
- **Identifier interning** via `Sym` and ~300 static well-known strings
  (`core/interner/`, `core/string/src/common.rs`). No _runtime_ property-key
  interning table beyond the static set.

## What the literature says

- **Ropes / `ConsString`**: V8 represents `a + b` as a lazy cons node (a tree of
  fragments), flattening to a flat string only on first indexed access. Turns
  repeated concatenation into O(1)-amortized appends instead of O(n²). V8 uses a
  `kMinLength` (~13) below which it just copies — small concatenations aren't
  worth a node.

## Plan

### 7a. `ConsString` variant for `+`

Add a lazy concatenation node to `JsStringKind`: instead of copying, `+` builds a
`Cons(left, right)` holding `Gc` handles to the operands and the total length.
Flatten (materialize a flat Latin1/UTF-16 buffer) lazily on the first operation
that needs contiguous bytes — indexing, `charCodeAt`, regex, comparison, passing
to a native. Below a small length threshold, keep copying (a cons node isn't free).

There is already a `Slice` (borrowed-slice) variant in `JsStringKind`
(`lib.rs:112`) — the lazy-node machinery is a natural sibling.

### 7b. Keep Latin1 fast paths honest

When flattening or operating, ensure the all-Latin1 case never silently widens to
UTF-16 (which doubles memory and halves scan throughput). The upgrade in
`concat_array` (`lib.rs:683`) is correct but should only fire on genuinely
mixed/non-Latin1 input.

### 7c. (Optional) runtime property-key interning

If profiling shows dynamic property keys (`obj[computedKey]`) hot, a runtime
intern table makes key comparison pointer-equality. Lower priority — most
property access is by static name (already `Sym`-interned).

## Expected ROI & validation

- Narrow but real. The headline `string-concat` benchmark is **DCE-suspect**
  (see [01](01-measurement-methodology.md)) — `node --jitless` elides it — so
  **build a sound concat benchmark** (accumulate into a string that is observed)
  before claiming a number. The win is most visible on string-builder patterns
  in real workloads (templating, serialization, earley-boyer's output).
- Validate flatten-on-access correctness with comparison/indexing/regex tests.

## Risks

- Cons trees can degenerate (deeply unbalanced) and blow the stack on flatten;
  flatten iteratively, and consider a depth cap that forces eager flatten.
- Every string consumer must trigger flatten before assuming contiguous storage —
  a missed site reads garbage. Centralize "give me flat bytes" behind one method
  and route all consumers through it.
