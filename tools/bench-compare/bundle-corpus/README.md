# Bundle corpus

Real-world JavaScript bundles pulled from the frozen site mirrors in
`ligero-browser/tools/demo-gate/mirrors/` (read-only source of truth; these
files are copies). Used by `examples/src/bin/bundle_bench.rs` to measure cold
parse + compile time on actual production bundles, not synthetic
microbenchmarks.

Selection rule: for each site, the largest JS asset that Boa's `Script::parse`
can actually parse as a classic script. `bundle_bench` calls `Script::parse`,
not `Module::parse`, so bundles containing top-level `export`/`import` or
`import.meta` are out of scope for this harness as it exists today.

**`github-repo` was dropped from the corpus entirely.** Every JS chunk github.com
serves for this page (rspack/webpack output) is an ES module — every file
above 1 KB either has a top-level `export` or an `import.meta` reference
somewhere in the minified body (confirmed by actually running each of the
site's ~110 JS assets, largest-first, through the release `bundle_bench`
binary; all failed with `Syntax` errors until the 1097-byte
`high-contrast-cookie` file, which is not representative of a bundle). Rather
than force in a non-representative 1 KB file to keep 5 sites, the corpus uses
4 bundles. This — Boa's real-bundle harness cannot measure a same-origin
GitHub page at all yet — is itself a finding, not a gap to paper over.

| File               | Origin site | Original URL                                                              | Bytes     | sha256 |
|---------------------|-------------|-----------------------------------------------------------------------------|-----------|--------|
| `react-dev.js`      | react.dev   | `https://react.dev/_next/static/chunks/51.f403ee094080242b.js`              | 577,725   | `87536650a4924497ebcc42af83e143583e913b49c8d335f920617dadfd7a9eed` |
| `vg-no.js`          | vg.no       | `https://cdn.stream.schibsted.media/player/extras/latest/player-plugin-skin-vgtv2-latest.js` | 1,165,867 | `e3581bacc00ca89f243cf16820a6065a26cdfd5f2e5d71ee84e3af3157642c8e` |
| `skeidar-no.js`     | skeidar.no  | `https://www.skeidar.no/_next/static/chunks/991-729d1281835bceea.js`        | 723,783   | `0604ce707f0a8edc5d30f17cea69c4d95545fd687939341a935882de673679a4` |
| `nrk-no.js`         | nrk.no      | `https://static.nrk.no/nrkno-header/3.4.0/custom-element.umd.js`            | 131,359   | `fa49c7910b8941e590f1ea084fa9c42c4ec241a6204fd2759c8348fa1f4993c8` |

Mirror manifests consulted (each site's `manifest.json` `assets[]` entry with
matching `file`):

- `ligero-browser/tools/demo-gate/mirrors/react-dev/manifest.json`
- `ligero-browser/tools/demo-gate/mirrors/vg-no/manifest.json`
- `ligero-browser/tools/demo-gate/mirrors/github-repo/manifest.json` (consulted, excluded — see above)
- `ligero-browser/tools/demo-gate/mirrors/skeidar-no/manifest.json`
- `ligero-browser/tools/demo-gate/mirrors/nrk-no/manifest.json`

All mirrors were frozen 2026-07-27 against pinned Chrome 145.0.7632.77
(`frozenWith.chrome` in each manifest).

See `planning/js-performance-roadmap/10-bundle-load-baseline.md` for the
parse/compile measurements taken against this corpus.
