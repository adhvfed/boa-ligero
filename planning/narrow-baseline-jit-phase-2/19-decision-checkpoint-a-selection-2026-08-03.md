# Decision checkpoint A selection — 2026-08-03

## Result

Select **Slice 4A, conservative loop-header OSR**, as the one next execution
ABI. This is deliberately narrow: the fixed engine and broader browser rows
contain no loop accepted by the first OSR shape, so the checkpoint does not
claim that OSR dominates arbitrary modern JavaScript. It selects OSR because
it is the only branch with all three of:

1. a dynamically hot, source-free, zero-drop production observation;
2. a loop whose bytecodes pass the conservative Phase 1 numeric/control-flow
   screen; and
3. an end-to-end whole-body native counterfactual with a matching result sink.

The screen is not a nonzero-PC compilation proof. Slice 4A0 subsequently found
two required ABI pieces: the loop's forward exit needs an explicit post-effect
continuation/materialization map, and region-only mode selection cannot infer
the fixture's live `F64` accumulator because its defining `StoreFloat` is before
the header. Those findings refine the implementation contract without changing
the ranking: no deferred alternative has a measured consumable native path.

Compiled calls, direct guarded storage, and region stitching remain planned.
No production admission rule, lowering allowlist, cache ownership rule, or VM
ABI changes here. JIT remains build- and runtime-opt-in.

## Protocol and measurement-tool correction

Headline timing used diagnostics-off release binaries. Micro controls used
seven fresh-process, order-alternating interpreter/JIT pairs, 70 warmups, and
five timed calls; the table reports time per call. One-shot controls used one
call and no warmup. Engine rows used five fresh-process pairs and one timed
call. W0 used seven fresh-process pairs through `ligero bench`.

The first engine diagnostic pass invalidated itself: the default 256-site
limit dropped 9,604,413 storage observations in Crypto, 42 in DeltaBlue, and
59,449,176 in Earley-Boyer. The first W0 pass likewise dropped 2,050 call and
2,087 storage observations. These incomplete snapshots were excluded.

Boa `343cf037` adds the runner's explicit
`--jit-diagnostic-record-limit`; Ligero `78d55bda` adds the corresponding
explicit bench option. Both request one bound for every detailed record kind,
while Boa retains the engine-owned hard ceiling of 4,096. The decision profile
reran diagnostics at 4,096 and requires every drop counter to be zero.

Build identity:

- Boa `343cf037e0a1b0b827ab4d4993e636aac6287676`;
- runner SHA-256
  `84c2459665a23c8abfead5d1a81bcd2abdb9e9152a10762346f8b053c9d7f14b`;
- Ligero `78d55bda9afbe6d2257ee262c1d7b2e0af1215d6`;
- Ligero SHA-256
  `574aac253c2b93991bf84b74d7baea2b147c03d4fe3de81253387f55be066f3b`.

## Diagnostics-off timing matrix

| Workload | Interpreter | JIT | JIT delta | Native evidence |
| --- | ---: | ---: | ---: | --- |
| integer arithmetic | 49.990 ms | 52.168 ms | +4.358% | denied `BitOr`; zero artifacts |
| floating-point arithmetic | 14.573 ms | 3.099 ms | −78.732% | one native body; 4.702× speedup |
| numeric array sum | 33.969 ms | 34.394 ms | +1.249% | denied `BitOr`; zero artifacts |
| monomorphic property | 22.374 ms | 22.887 ms | +2.293% | denied caller/helper; zero artifacts |
| four-shape property | 6.863 ms | 6.873 ms | +0.150% | denied caller/helper; zero artifacts |
| flat function call | 38.438 ms | 38.121 ms | −0.825% | denied caller/callee; zero artifacts |
| monomorphic method call | 23.230 ms | 23.087 ms | −0.616% | denied call boundary/receiver |
| eligible one-shot loop | 28.980 ms | 29.148 ms | +0.578% | hot after PC zero; zero warm artifacts |
| ineligible one-shot loop | 38.061 ms | 38.215 ms | +0.405% | denied `BitOr`; zero artifacts |
| Crypto | 11.651 s | 11.142 s | −4.372% | zero artifacts |
| DeltaBlue | 2.009 s | 2.011 s | +0.096% | zero artifacts |
| Earley-Boyer | 13.095 s | 13.265 s | +1.295% | zero artifacts |
| W0 browser gate | 43.137 ms | 29.971 ms | −30.521% | one native body, 999 entries, zero deopts |

Every row retained the same accumulator or W0 paint structure. All
diagnostics-off negative controls remain inside 5%. W0 retains 387 display
items, 258 paint segments, and 8,159,754 accounted bytes in both modes.

## Zero-drop dynamic attribution

The micro diagnostic process used one timed call after three warmups, except
the one-shot rows, which used one call and no warmup. Counts describe the
diagnostic process rather than the headline process.

| Workload | Ordinary calls | Loop backedges | First-OSR eligible | Storage reads | Cached native targets |
| --- | ---: | ---: | ---: | ---: | ---: |
| integer arithmetic | 0 | 4,000,000 | 0 | 0 | 0 |
| floating-point arithmetic | 0 | 500,000 interpreted | 500,000 | 0 | 0 |
| numeric array sum | 0 | 4,000,400 | 0 | 4,000,000 dense | 0 |
| monomorphic property | 800,000 | 800,000 | 0 | 2,400,000 named | 0 |
| four-shape property | 200,000 | 200,000 | 0 | 600,000 named + 200,000 dense | 0 |
| flat function call | 2,000,000 | 2,000,000 | 0 | 0 | 0 |
| monomorphic method call | 800,000 | 800,000 | 0 | 2,400,000 named | 0 |
| eligible one-shot loop | 0 | 2,000,000 | 2,000,000 | 0 | 0 |
| ineligible one-shot loop | 0 | 2,000,000 | 0 | 0 | 0 |

The 4,096-site engine pass retained every observation:

| Workload | Ordinary calls | Loop backedges | First-OSR eligible | Storage reads | Native storage helpers |
| --- | ---: | ---: | ---: | ---: | ---: |
| Crypto | 3,105,916 | 33,873,301 | 0 | 118,644,109 | 0 |
| DeltaBlue | 8,341,630 | 1,355,906 | 0 | 25,743,694 | 0 |
| Earley-Boyer | 20,292,477 | 8,383,439 | 0 | 74,290,300 | 0 |

All three engine rows had zero cached native/shim targets and zero native
storage-helper executions. Their call/storage counts are real frequency, but
not evidence that either proposed native ABI can currently consume the sites.

W0 retained 3,374 calls (1,390 ordinary, 998 already native-cached), 2,988
backedges (1,000 at one statically eligible site), and 3,784 storage reads. Its
numeric kernel already enters at PC zero 999 times, so that loop does not create
a lost OSR opportunity. A separate user-authorized application load retained
1,629 calls (454 ordinary), 1,067 backedges, and 3,801 storage reads, with zero
cached native targets, zero first-shape OSR candidates, zero artifacts, and
zero drops. Its 1.200-second networked load is compatibility evidence, not a
timing claim.

## Candidate ranking

### 1. Loop-header OSR — selected narrowly

The one-shot numeric body executes 2,000,000 backedges after its only PC-zero
opportunity and passes the conservative static screen. Production JIT stays at
interpreter parity because it correctly creates no mid-frame artifact. The
runner's intentional threshold-1 PC-zero control executes the complete
function natively with a 7.318 ms median including 0.429 ms median compilation,
versus 28.980 ms interpreted. This 3.96× counterfactual is not an OSR result; it bounds
the opportunity and proves the screened loop bytecodes are useful inside a
whole-body native artifact. It does not prove that the current compiler can
materialize or exit the region from a nonzero-PC entry.

### 2. Compiled ordinary calls — deferred

Fixed call sites are monomorphic and engine suites execute millions of ordinary
calls. However, broad target observations have zero cached native/shim
opportunity, callers are denied before a continuation exists, and call controls
remain at interpreter parity. W0's 998 cached-target calls originate in a
caller that is not a native continuation candidate. The call ABI would need
target admission plus caller continuation before broad rows can use it.

### 3. Direct guarded storage — deferred

Storage dominates engine frequency, but every counted site is interpreted and
native helper aggregates are zero. Replacing a native helper with a direct load
cannot accelerate an interpreted site. Direct storage stays behind a future
admitted-region/helper-cost profile and the GC/layout lifetime review.

### 4. Region stitching — deferred

Region stitching may eventually expose mixed loops, but it changes
control-flow/materialization semantics without a measured complete region in
this matrix. It is larger than the selected numeric OSR contract and has no
direct counterfactual here.

## Slice 4A0 design-review gate

Before implementation, check in one exact ABI design covering:

- a typed `(code_id, loop_header_pc, budgeted, diagnostic)` region/cache key;
- compilation only at the stable scheduler boundary after the interpreter has
  executed and charged the backedge;
- an explicit live-register/materialization map at header entry and every exit;
- numeric live-in guards before effects, with interpreter replay only for an
  audited pre-effect guard failure;
- exact finite-budget charging across interpreted prefix, OSR entry, native
  iterations, and deoptimization;
- generation/backend ownership that cannot outlive code, realm, or context;
- rejection of calls, properties, allocation, handlers, eval/with, suspension,
  host re-entry, unknown stack state, and object live-ins;
- synchronous compile-time and cache-byte bounds plus duplicate/failure
  suppression;
- exception, runtime-limit, forced-GC, representation-change, zero-trip,
  nested-frame, and cold-workload tests.

Slice 4A1 may implement only that reviewed shape. After its behavior and
diagnostic slices, schedule a separately revertible behavior-neutral refactor
of region-key, materialization-map, or exit-mapping plumbing before selecting a
second ABI. Decision checkpoint B then reruns this matrix.

## Raw headline samples

Values are elapsed nanoseconds per process except W0, which is milliseconds.
Micro values contain five timed calls; the table divides their median by five.

```text
int-arith interp: 270388125 248933292 254010334 249683208 249348417 249950375 261207084
int-arith jit:    263073083 260687208 262056000 260842334 258182542 261942042 259062542
float-arith interp: 71946958 71033333 76571083 73797625 71229291 75321250 72865625
float-arith jit:    15295750 15346292 15510875 15429042 15508750 15633292 15497291
array-sum interp: 169847000 185454500 198418250 170602708 169193458 169522708 169121917
array-sum jit:    171968625 177071917 178655917 176518625 168257000 166247334 163379084
property-mono interp: 111010375 112444291 111870459 111745333 113784625 114861250 110917083
property-mono jit:    112863916 117101167 114435750 114853042 114880083 114310166 114154958
property-poly4 interp: 33908458 34090917 34763583 34187625 34361125 34586958 34314625
property-poly4 jit:    34300916 34366083 34093417 36397917 34555167 34519917 34157167
fn-call interp: 193997458 191926541 192191625 197403292 195390334 186896292 191299542
fn-call jit:    189592541 193769416 188807625 192260625 190085875 192503083 190605458
method-call interp: 115959167 116881250 114521875 117479250 116189750 116150167 116028709
method-call jit:    124252500 117942834 115435000 114262417 114874000 116396291 115254500
one-shot interp: 29076917 29026458 28791292 28874625 28853167 28980416 29106458
one-shot jit:    29183625 28954083 29139666 29148000 29161334 28861875 29230667
ineligible interp: 38409667 38410375 38061417 36259667 37651166 37936250 38611208
ineligible jit:    38397042 38215417 37993208 38984833 38016500 38044417 38266167
crypto interp: 11584471625 11641248708 11650885625 11731739208 11671760208
crypto jit:    11132738042 11141533000 11167462125 11157244833 11109945125
deltablue interp: 1999725084 2022049333 2011705625 2005028000 2009453333
deltablue jit:    2001809166 2010425083 2014656500 2011386583 2024348375
earley interp: 13059564459 13218440875 13094974166 13052001042 13887306917
earley jit:    13289977875 13264617250 13217111250 13259594500 13291990417
W0 interp: 43.323458 42.421375 43.137292 43.081541 43.134459 43.734500 43.949375
W0 jit:    30.613292 29.971333 30.213500 29.805792 30.407000 29.825708 29.957375
```

## Verification

- Boa runner parser tests and checks pass with and without `jit`;
- affected-runner warning-denying Clippy passes with `--no-deps`; the wider
  engine retains exactly the 16 recorded strict-Clippy findings;
- Ligero CLI parsing requires diagnostics before accepting a custom limit;
- focused script-host diagnostic and CLI tests pass;
- Ligero checks pass with and without `jit`;
- affected Ligero all-target warning-denying Clippy passes;
- every retained decision profile reports schema 7 and zero drops.
