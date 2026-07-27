#!/usr/bin/env node
// Cross-engine microbench runner.
//
// Usage:   node runner.mjs <script.js> <runs> <warmup>
//
// Loads <script.js> (which must define a global `main` function),
// invokes main() <warmup> times, then times <runs> invocations.
// Prints a single line in the format `elapsed_ns=<N>` so the parent
// process can parse it without ambiguity.
//
// Works under node, bun, deno (with --allow-read), and qjs (with -e via the
// shell wrapper). Keep this file portable — no Node-specific APIs.

import { readFileSync } from "node:fs";

const [, , scriptPath, runsArg, warmupArg] = process.argv;
if (!scriptPath) {
  console.error("usage: runner.mjs <script.js> [runs] [warmup]");
  process.exit(2);
}
const runs = parseInt(runsArg ?? "100", 10);
const warmup = parseInt(warmupArg ?? "10", 10);

const code = readFileSync(scriptPath, "utf8");

// Evaluate the script in the global scope so `main` is reachable.
// `globalThis.eval` is sloppy mode; this matches the Boa harness.
(0, eval)(code);

if (typeof main !== "function") {
  console.error(
    `script ${scriptPath} did not define a global \`main\` function`,
  );
  process.exit(3);
}

// Warm up (relevant for JIT engines; harmless for interpreters).
for (let i = 0; i < warmup; i++) main();

const hr = process.hrtime.bigint;
let acc = 0;
const start = hr();
for (let i = 0; i < runs; i++) {
  // XOR to prevent dead-code elimination of main()'s result.
  acc ^= +main() | 0;
}
const elapsedNs = Number(hr() - start);

console.log(
  `elapsed_ns=${elapsedNs} runs=${runs} ns_per_run=${(elapsedNs / runs).toFixed(0)} acc=${acc}`,
);
