# Default-JIT admission plan — 2026-08-03

## Result

Gate D is now an executable release gate rather than permission to flip a
feature after one fast fixture. The JIT remains build- and runtime-opt-in until
one identified release candidate passes every group below. The eventual
default change must be a separate, independently revertible commit and retain
an explicit interpreter-only runtime path.

This plan does not select a second execution ABI and does not change remote
script policy. Default JIT means only that scripts which Ligero already permits
to execute use the validated tier unless the operator opts out. It does not
enable `--allow-remote-scripts`, weaken private/loopback network protections,
or change script admission.

## D0 — finish Decision checkpoint B reproducibly

Re-run the fixed nine-micro, three-engine, and W0 matrix from Gate O using the
identified release runner, immutable fixture hashes, fresh processes, and the
recorded alternating order. Headline timings keep diagnostics off; one
separate schema-10 process per workload requests the 4,096 hard cap and is valid
only when all six dropped-record/observation counters are zero.

The 2026-08-03 first Decision-B attempt is not selection evidence. Its exact
runner SHA-256 was
`664db36d29b97f8b1fb57168ed778dda462f3782371f61f4e0cdf7b4dea0ac8c`,
and all nine micro rows passed their fixed semantic/performance guardrails, but
the engine phase ran while `mediaanalysisd` and `apfsd` each occupied roughly
one complete CPU core and video/window services occupied additional cores.
Earley-Boyer consequently moved from the earlier +4.287% sentinel to a
+10.234% median with the same binary and zero artifacts. A separate preliminary
five-pair replacement after those processes exited returned to +4.372%. It
used Boa code `6dc6aa07`, runner SHA-256
`664db36d29b97f8b1fb57168ed778dda462f3782371f61f4e0cdf7b4dea0ac8c`,
Earley-Boyer SHA-256
`0e614bfc92b01d3fb44cfa55e18ba8fd9f84c0a4b075b1f8ed73e88e1af31ce2`,
and macOS 15.3 build 24D2059 on a 10-core Apple M4. Raw elapsed nanoseconds:

```text
interp: 12991607041 14069364959 13039521042 13081458250 13079049292
jit:    13627569125 13700199375 13600293958 13650914333 13680560917
```

That observation supports the environmental diagnosis, but it predates the
continuous preflight below and is excluded from the binding decision. It does
not retroactively validate the other engine rows. No ABI is selected, and the
complete engine/browser group must still pass the binding preflight.

The preflight samples `ps -axo pid=,%cpu=,command=` three times at ten-second
intervals before each workload group, continues the same ten-second sampling
throughout the group, and takes three more samples afterward. Persist the raw
rows, exclude only the measured runner and confined mirror-server PIDs, and
sum every other process's `%CPU` per sample (`100%` is one logical core). Do
not start or accept a binding group when unrelated CPU is at least 100% in any
pre/post sample or its mean is at least 100% over any three consecutive in-run
samples. If load begins during a run, retain the raw samples, mark that entire
workload invalid, and repeat it in a new identified run; never discard
individual slow samples. The checkpoint also records OS version, hardware/CPU
count, Boa and Ligero commits, Boa runner and Ligero binary SHA-256 values,
every fixture/manifest hash, and exact flags.

Decision-B stop/go remains:

- eligible cold OSR keeps an identical sink, one loop compile/entry/
  continuation, zero whole-function native entries/rejections/deopts, and at
  least a 2x speedup including synchronous compilation;
- every negative micro and engine row stays within +5% of the interpreter
  median, with Earley-Boyer retained as the hard sentinel;
- W0 retains checksum `499500000`, 387 display items, 258 paint segments,
  8,159,754 accounted bytes, and at least a 20% cold-load win;
- another ABI may be selected only when its dynamic opportunity and a native
  path able to consume that work are both measured.

## D1 — bound the complete default tier

The loop-OSR cache already has 64-key, 1 MiB accounted-code, and 10 ms
compile-attempt circuit breakers. The PC-zero whole-function `cache` remains
unbounded and therefore blocks default enablement.

Before implementation, check in a D1 design record with exact constants and
their workload-derived rationale for total retained entries, total accounted
code bytes, cumulative compile-time budget/window, diagnostic overhead, and
every cache/metadata/plan map. Then one behavior slice must add and test that
backend-wide policy across whole-function and loop artifacts together:

The design record is now
[checked in](31-default-jit-resource-bounds-design-2026-08-03.md). It selects
192 reserved function variants plus 64 loop keys, an 8 MiB aggregate code
breaker, 10 ms per-attempt and 100 ms cumulative compile breakers, a 1,024-
instruction PC-zero ceiling, a 1,024-site legacy feedback cap, and the existing
4,096-per-class diagnostic cap. D1's two implementation commits are landed in
Boa `42623234` and `2888c024`, with the initial schema-9 Ligero projection in
`72568c9a` and `dfdc0714`. Returned compiler/module-failure containment adds a
source-free retirement counter and therefore deliberately advances the
diagnostic contract to schema 10. The seven-pair cold/RSS evidence remains
open, so D1 is not closed. See the
[implementation checkpoint](32-default-jit-resource-governor-checkpoint-2026-08-03.md).

- a hard retained-entry and accounted-code bound;
- a cumulative compile-time breaker for unseen keys;
- deterministic duplicate/failure suppression;
- an explicit retirement or no-more-compilation policy at capacity;
- continued reuse of existing ready entries after every breaker trips;
- distinct budgeted/diagnostic/entry-PC variants without aliasing;
- source-free counters for capacity, code-byte, compile-time, and failed-entry
  suppression;
- peak-RSS and cold-start measurements on both no-artifact and artifact-heavy
  controls.

The gate fails if a page can grow executable memory, cache metadata, retained
plans, or compilation time without a tested engine-owned bound. Schedule the
next behavior-neutral refactor after this and one further behavior slice, in
line with the every-two-slices cadence.

## D2 — close PC-zero containment

The older whole-function native boundary must receive the same fail-closed
coverage now required of loop OSR. Inject every malformed status class and
verify the exact expected outcome for:

- untagged/unknown status, forbidden exit kind/reason, and invalid resume PC;
- absent or mismatched pending completion and stale cached metadata;
- wrong backend, code ID, frame depth, current PC, register range, or budget
  mode;
- exception/catch/finally, finite-budget exhaustion, recursion limit, forced
  GC before and after compile/entry/return, and host re-entry;
- executable-memory publication or entry failure.

Every compromised-native case must clear paired pending state, unwind through
the engine error path, disable/drop the owning backend, never reuse the
artifact, and allow a later script in the same context to execute through the
interpreter when recovery is defined. No page-controlled PC may be resumed.

## D3 — W2 representative browser breadth

W2 is the checked-in Ligero Tier-A frozen-mirror corpus in
`tools/demo-gate/sites.mjs`, not a live-site sample. Its identity comprises the
Ligero and Boa commits, release-binary SHA-256, `pin.json`, `sites.mjs`, every
selected manifest, the per-site loopback transport, and the two declared
desktop/mobile viewports. W0 remains a separate positive integration control.

### D3.0 — blocking harness work

The current `run.mjs` path cannot conduct this gate: `automate` has no JIT or
diagnostic option, the host's process-global boolean cannot force JIT off, and
the harness has no paired warm or peak-RSS report. Add all of the following
before collecting W2:

- use one JIT-built, SHA-verified release-candidate binary for both arms;
- replace the host boolean with a tri-state runtime policy whose precedence is
  `ForceOff > ForceOn > ReleaseDefault` and plumb automation controls for both
  forced arms plus explicit diagnostics;
- emit one machine-readable report per process containing the resolved runtime
  policy, `jit_enabled`, wall time, and an identified OS-specific peak-RSS
  source; `ForceOn` includes engine JIT counters and an optional diagnostic
  snapshot, while `ForceOff` includes harness-normalized zero counters and no
  engine snapshot because it must create no backend;
- compare the two arms directly for DOM/layout/paint structure, screenshot,
  console, requests, and stable semantic sinks;
- fail closed unless all 32 site-by-viewport cells, all raw samples, and every
  policy-appropriate JIT report field are present.

For every Tier-A cell run seven fresh-process, order-alternating interpreter/
JIT cold pairs. Warm measurement uses one discarded reload followed by five
scored reloads in the same process per arm, with arm order alternating across
the same seven pair rounds. Diagnostics remain off in headline timings and run
once separately at the 4,096 limit. The semantic oracle requires:

- identical stable application checksums where supplied;
- no new crash, hang, console exception class, failed-request class, or G2
  issue relative to the interpreter run on identical mirror bytes;
- equal DOM/layout/paint structural counts where deterministic;
- interpreter/JIT screenshot `structuralAaPct` mismatch of at most 0.1% and
  pixel mismatch of at most 0.1%;
- zero diagnostic drops and no source, URL, property value, object identity,
  raw pointer, or realm address in serialized reports.

No JIT-specific allowlist exists. Any A/B nondeterminism invalidates that cell;
necessary canonicalization must be corpus-wide, separately reviewed, and
applied identically to both arms.

For every one of the 32 cells, both cold and warm `JIT-on / JIT-off` median
ratios must be at most 1.05. The unweighted geometric mean of those 32 ratios
must be at most 1.02 separately for cold and warm, and each JIT-on peak RSS
must be at most 1.10 times its paired JIT-off peak. W0 separately keeps its 20%
cold win. Record compilation time, artifact/code bytes, cache/suppression
counts, native coverage, entries, transitions, deopts, and the final semantic
sink per cell. A noisy cell is invalid, not a pass.

The confined loopback corpus is local-tooling workload evidence. It is not
evidence that remote scripts are enabled or safe by default.

## D4 — supported platforms and security

The first default release requires native evidence on
`aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`. Each target runs
`cargo test -p boa_engine --lib --features jit jit_`, the full engine feature
matrix, malformed-status containment, the W0-equivalent integration path
available on that platform, and the fixed micro/engine rollback subset.

The explicitly supported interpreter-only release targets are
`wasm32-unknown-unknown` and `x86_64-pc-windows-msvc`. CI runs
`cargo check -p boa_engine --lib --target <target> --no-default-features` for
both and a runnable feature-disabled engine suite on Windows. Tests assert
that these paths construct no JIT backend and take no JIT scheduler branch.
Adding another release target requires adding its expected native or
interpreter-only behavior and exact CI command here first.

The security review records a threat/acceptance matrix for executable-memory
write-to-execute publication, code lifetime and backend ownership, malformed
native metadata, resource exhaustion, runtime limits, GC and host re-entry,
diagnostic privacy, and recovery after backend compromise. Fuzz/property
controls must exercise status decoding, cache-key validation, and interpreter
replay boundaries. Default diagnostics stay off.

## D5 — release flip and rollback

Only after D0–D4 pass against the same candidate:

1. land the build/default-runtime change alone;
2. expose the D3.0 tri-state policy as an explicit `--no-jit` operator path
   and embedding `ForceOff`; test `ForceOff > ForceOn > ReleaseDefault` in
   subprocesses for default-on, CLI-off, embedding-off, and feature-not-built,
   and prove both off modes construct no `JitBackend`;
3. run formatting, warning-denying affected-target Clippy, both engine feature
   matrices, `cargo test --workspace`, Ligero checks with and without the JIT
   feature, W0, W2, and the fixed rollback rows;
4. prove diagnostics remain disabled unless explicitly requested;
5. with default JIT but no remote-script opt-in, prove a remote document's
   scripts remain skipped, no JIT realm/backend/artifact is created, and
   private/loopback network restrictions are unchanged; test remote execution
   only under the existing explicit remote-script flag;
6. revert only the default flip if any binding row fails, leaving the opt-in
   tier and its diagnostic evidence available.

The default commit cannot combine opcode widening, threshold tuning, cache
policy, a new ABI, remote-script policy, or unrelated refactoring. Those
changes require their own cascade and invalidate the release-candidate
identity if made after admission begins.
