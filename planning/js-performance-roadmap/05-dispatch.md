# 05 — Dispatch (lever #6)

This lever is listed low **not** because the literature undervalues it — it's the
classic ~2× interpreter win — but because Boa has already captured most of it,
the remaining slice is uncertain, and Rust makes the rest expensive. **Measure
before building.**

## Current state (grounded)

The main loop (`core/engine/src/vm/mod.rs:1025`):

```rust
while let Some(byte) = self.vm.frame().code_block.bytecode.bytes.get(pc) {
    let opcode = Opcode::decode(*byte);
    match self.execute_one(
        |context, opcode| {
            let pc = context.vm.frame().pc as usize;
            OPCODE_HANDLERS[opcode as usize](context, pc)   // central indirect call
        },
        opcode,
    ) { ControlFlow::Continue(()) => {} ControlFlow::Break(v) => return v }
}
```

`OPCODE_HANDLERS` is a 256-entry function-pointer table
(`vm/opcode/mod.rs:412`); each handler is `#[inline(always)]`, decodes its
operands, advances `pc`, runs `Operation::operation`, and returns `ControlFlow`.

This is **token threading via a function-pointer table** — already far better
than a `match` ladder, and Shi et al.'s register-VM win is banked. But note the
shape: there is **one** indirect call site (line 1041), reached in a loop, and it
is wrapped in an `execute_one` closure layer.

## Why there may still be something here

Ertl & Gregg (JILP 2003): indirect branches can cost >50% of interpreter
runtime, and the mispredict rate depends on _how many distinct dispatch sites_
the predictor sees. A single central indirect call (Boa today) gives the branch
predictor one site to predict the _next_ opcode from — it mispredicts on most
opcode→opcode transitions. **Threaded code** replicates the dispatch at the end
of _each_ handler, so the predictor learns pairwise opcode correlations; Berndl
et al.'s context threading (CGO 2005) eliminated ~95% of mispredicts for 30–40%
runtime. So Boa's central-site design is closer to `switch`'s prediction
behavior than to true threading — there is a real, but bounded, opportunity.

## Plan — measure first, then the hard part

### 5a. Size the lever (do this before anything else)

`perf stat -e branches,branch-misses` (Linux) or Instruments (macOS) on a
release `bench-compare-runner` running a hot loop. If branch-misses are a small
fraction of cycles, **stop** — the central indirect call predicts well enough on
this workload mix and threading isn't worth the friction. If mispredicts are
significant, proceed.

### 5b. Reduce per-dispatch overhead (cheap, do regardless)

- The `execute_one` closure wrapper (line 1036) adds a layer around every
  dispatch. Audit what it does (budget/trace/limits) and ensure the
  non-instrumented build path is zero-cost or hoisted out of the loop.
- The `pc` is read from the frame twice per step (loop condition + inside the
  closure). With the cached `current_frame_ptr`, ensure this is a single load.

### 5c. Threaded / tail-call dispatch (high friction in Rust)

True threading wants each handler to _tail-call_ the next handler through the
table, removing the central site. Rust's guaranteed tail call (`become`) is
**unstable**; `#[inline(never)]` + a manual trampoline doesn't get the predictor
benefit. Realistic paths:

- Wait for / gate behind `become` (explicit tail calls) and structure handlers
  to `become OPCODE_HANDLERS[next](...)`.
- Or a `musttail`-style approach via a nightly feature, accepting a nightly
  dependency for the perf build.
  This is the genuinely hard, possibly-not-worth-it part. Only attempt if 5a says
  mispredicts are large.

### 5d. Superinstructions for hot opcode pairs (optional polish)

Proebsting (POPL 1995): fuse common adjacent opcode sequences (e.g.
`GetName;Call`, `LdaSmi;Add`) into one handler to amortize dispatch. Ertl/Gregg's
follow-ups warn the gain is often <2× and sometimes negative from I-cache
pressure — so keep the set tiny and profile-selected. The method-call fusion in
[02-call-path](02-call-path.md) (2d) is the highest-value instance and is
motivated independently.

## Expected ROI & validation

Uncertain by design. Could be a real chunk if 5a shows heavy mispredicts; could
be ~nothing because the fn-ptr table already predicts acceptably and Rust blocks
the clean implementation. **This is the lever most likely to be a wash — treat
5a as a go/no-go gate and don't sink implementation effort before it.**
