# Slice 4A1 workload gate — 2026-08-03

## Result

Gate O and Slice 4A1.5c pass on Boa `8c8af54c` after the fixed engine and W0
rollback matrix. The scheduler edge remains enabled. This accepts only the
reviewed first numeric loop-OSR shape; it does not authorize a second execution
ABI, representative-workload claims, default JIT, or default remote scripts.

The first engine attempt exposed an 18% Earley-Boyer regression with zero
native artifacts. The scheduler-owned interpreter path was decoding every
executed instruction a second time to discover whether control moved backward,
and it evaluated a diagnostics branch even when diagnostics were disabled.
Earley-Boyer contains many denied, zero-trip, or rarely taken loops, so the
observer overhead accumulated without producing native work.

Boa `8c8af54c` keeps the scheduler as the sole loop-OSR owner but specializes
production and diagnostic dispatch at compile time. Production dispatch has no
per-opcode diagnostics branch and performs post-instruction frame/PC checks
only after an opcode class that can express a same-frame branch. The existing
exact bytecode/backedge validator remains authoritative before scheduling.
Diagnostic dispatch retains complete call, storage, and loop observations.

## Identified inputs

- Boa: `8c8af54cff49379ae0928f5d19e7ca5ba03c5dcc`;
- release runner SHA-256:
  `da8b3602247a348df71db7353a66db909dd3e4ecba045743e87de5117c016dc3`;
- Crypto SHA-256:
  `25b8ef32bd391caf7932ac8c58d20a1be37947d5f71a8034afeaf52c8faf213a`;
- DeltaBlue SHA-256:
  `295f74ca232c09e6ffb01a55308b8df3851cc56e5a9998dafd5cada98ac03036`;
- Earley-Boyer SHA-256:
  `0e614bfc92b01d3fb44cfa55e18ba8fd9f84c0a4b075b1f8ed73e88e1af31ce2`;
- Ligero: `b0703deb06611c09da9dc741a728eab14b4b17a2` with the local Boa commit
  above through its existing path dependency;
- release Ligero SHA-256:
  `175df1a71e010a8348207a555f88dcacfcafd64a749a7a6013f178c28364043e`;
- W0 SHA-256:
  `86bd2f5d96c7afcf06c291851ebba785a1250dd038bdc09604ba728c24e9496b`.

Ligero was built from a detached clean worktree at the recorded commit into a
fresh target directory. The user's unrelated main-worktree changes were not
part of the build.

## Engine rollback rows

Each workload used five alternating fresh-process interpreter/JIT pairs, one
timed run, no warmup, and diagnostics disabled. The JIT runner uses its existing
cold control plus production timed context; medians below are the production
timed result. All samples retained the same sink. Every JIT sample recorded
zero compilations, native entries, scheduler-call exits, and deoptimizations.

| Workload | Interpreter median | JIT median | Delta | Gate |
| --- | ---: | ---: | ---: | --- |
| Crypto | 11.112 s | 11.348 s | +2.125% | pass |
| DeltaBlue | 2.018 s | 2.010 s | -0.403% | pass |
| Earley-Boyer | 12.991 s | 13.564 s | +4.412% | pass |

Earley-Boyer is intentionally retained as a near-boundary regression sentinel.
Its +4.412% median passes the fixed 5% ceiling, but leaves only 0.588 percentage
points of headroom and must not be rounded into a claim of parity.

### Raw engine samples

Values are elapsed nanoseconds per fresh process.

```text
Crypto interp: 11249351459 11078880584 11122876042 11076166500 11111539041
Crypto jit:    11301098292 11347702250 11368343542 11315948666 11373186292

DeltaBlue interp: 2016689292 2018022750 2014427458 2019579625 2018775583
DeltaBlue jit:    2013964583 2009881917 2019438167 2006150375 2009361833

Earley-Boyer interp: 12976899250 13188602625 12990624292 12970147625 13674806458
Earley-Boyer jit:    13546980958 13527853083 13582030084 13771503875 13563718583
```

## W0 browser rollback row

Ligero was rebuilt in release mode with `--features jit` against the identified
Boa commit. Seven alternating fresh-process pairs used one frame, one run, no
warmup, no scroll, scale 1, scripts enabled, and diagnostics disabled.

- interpreter median cold load: `44.290583 ms`;
- JIT median cold load: `31.177667 ms`;
- median improvement: `29.607%`;
- every run: 387 display items, 258 paint segments, and 8,159,754 accounted
  bytes;
- the unchanged fixture writes checksum `499500000` to visible output and
  `body.dataset.checksum`;
- every headline JIT run: one reported PC-zero whole-function compilation,
  1,000 aggregate native entries, zero deoptimizations, and zero scheduler-call
  exits.

One separate schema-8 diagnostic run at the 4,096 hard cap produced 387 display
items, 258 paint segments, 8,159,754 accounted bytes, and zero drops in all six
record classes. Its immutable exit records distinguish 999 PC-zero normal
returns from one nonzero-PC loop continuation. OSR diagnostics record one
compile, one entry, one continuation, zero rejection/deopt, and 3,240 accounted
loop-code bytes. This confirms that the aggregate 1,000-entry count contains
both the existing PC-zero path and one first-call OSR transition.

### Raw W0 samples

Values are `load_ms`; each row preserves the structural values above.

```text
interp: 66.270458 43.301000 44.872041 43.439917 44.990042 43.784459 44.290583
jit:    32.111458 31.133583 31.177667 31.014875 31.134625 32.732834 31.772667
compile_ms:
        0.503125 0.503417 0.495125 0.488000 0.529500 0.511708 0.517792
```

The W0 timing distribution contains real cold-process noise, including a
66.270 ms first interpreter sample. The precommitted median gate passes despite
that sample; it is retained rather than discarded.

## Verification and next slice

- the two focused dormant-frame scheduler regressions pass;
- feature-disabled and JIT-enabled engine checks pass;
- strict engine Clippy retains exactly the 16 recorded pre-existing findings
  and no slice-local finding;
- all earlier 4A1.5a semantic/accounting and 4A1.5b containment/lifetime gates
  remain binding.

The next and only scheduled implementation slice is **4A1.R**, a separately
revertible behavior-neutral refactor of loop-exit contract plumbing. It may
extract the status/pending-state/metadata validation and action mapping now
concentrated in `JitBackend::invoke_loop_region`, but must preserve the public
status ABI and all scheduler ownership. It must change no threshold, opcode
allowlist, representation, cache policy, diagnostic schema/counters, admission
decision, or generated-code behavior.

4A1.R is complete only when the focused JIT suite, exhaustive budget/loop-limit
differentials, malformed-status table, 64+1 ownership tests, feature-disabled
check, strict affected-target Clippy, the Earley-Boyer sentinel, and one W0
structural/diagnostic sample remain green. Decision checkpoint B follows in a
separate planning commit; it must re-profile before selecting calls, storage,
region stitching, or any other second ABI.
