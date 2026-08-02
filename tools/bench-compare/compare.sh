#!/usr/bin/env bash
# Cross-engine microbench comparison. Reads ns/run from runners and tabulates.
#
# Usage:
#   compare.sh                  # all scripts, all available engines
#   compare.sh property-mono    # filter scripts by substring
#   RUNS=200 WARMUP=10 compare.sh
#   BOA_JIT=1 compare.sh int-arith # add Boa's opt-in warm JIT column
#
# Build Boa's runner first:
#   cargo build --release -p boa_benches --bin bench-compare-runner
# For the optional JIT column, include the feature:
#   cargo build --release -p boa_benches --bin bench-compare-runner --features jit

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPTS_DIR="$REPO_ROOT/benches/scripts/microbench"
RUNNER_JS="$REPO_ROOT/tools/bench-compare/runner.mjs"
BOA_RUNNER="$REPO_ROOT/target/release/bench-compare-runner"

RUNS="${RUNS:-50}"
WARMUP="${WARMUP:-5}"
FILTER="${1:-}"

# Build ordered list of engine names + corresponding command (parallel arrays).
ENGINE_NAMES=()
ENGINE_CMDS=()
ENGINE_MODES=()

if command -v node >/dev/null 2>&1; then
  ENGINE_NAMES+=("node"); ENGINE_CMDS+=("node $RUNNER_JS"); ENGINE_MODES+=("")
  ENGINE_NAMES+=("node-jit-less"); ENGINE_CMDS+=("node --jitless $RUNNER_JS"); ENGINE_MODES+=("")
fi
if command -v bun >/dev/null 2>&1; then
  ENGINE_NAMES+=("bun"); ENGINE_CMDS+=("bun $RUNNER_JS"); ENGINE_MODES+=("")
fi
if command -v deno >/dev/null 2>&1; then
  ENGINE_NAMES+=("deno"); ENGINE_CMDS+=("deno run --allow-read $RUNNER_JS"); ENGINE_MODES+=("")
fi
if [[ -x "$BOA_RUNNER" ]]; then
  ENGINE_NAMES+=("boa"); ENGINE_CMDS+=("$BOA_RUNNER"); ENGINE_MODES+=("")
  if [[ "${BOA_JIT:-0}" == "1" ]]; then
    ENGINE_NAMES+=("boa-jit"); ENGINE_CMDS+=("$BOA_RUNNER"); ENGINE_MODES+=("jit")
  fi
else
  echo "warn: boa runner missing; run \`cargo build --release -p boa_benches --bin bench-compare-runner\`" >&2
fi

[[ ${#ENGINE_NAMES[@]} -gt 0 ]] || { echo "no engines available" >&2; exit 1; }

# Header.
printf "%-30s" "script"
for n in "${ENGINE_NAMES[@]}"; do printf " %12s" "$n (ns)"; done
printf " %12s\n" "boa/node"

# Find boa and node indexes for ratio (or -1 if absent).
boa_idx=-1; node_idx=-1
for i in "${!ENGINE_NAMES[@]}"; do
  [[ "${ENGINE_NAMES[$i]}" == "boa" ]] && boa_idx=$i
  [[ "${ENGINE_NAMES[$i]}" == "node" ]] && node_idx=$i
done

shopt -s nullglob
for f in "$SCRIPTS_DIR"/*.js; do
  name=$(basename "$f" .js)
  [[ "$name" == _* ]] && continue
  if [[ -n "$FILTER" && "$name" != *"$FILTER"* ]]; then continue; fi
  printf "%-30s" "$name"
  results=()
  for i in "${!ENGINE_NAMES[@]}"; do
    if [[ -n "${ENGINE_MODES[$i]}" ]]; then
      out=$(${ENGINE_CMDS[$i]} "$f" "$RUNS" "$WARMUP" "${ENGINE_MODES[$i]}" 2>/dev/null || echo "ns_per_run=NaN")
    else
      out=$(${ENGINE_CMDS[$i]} "$f" "$RUNS" "$WARMUP" 2>/dev/null || echo "ns_per_run=NaN")
    fi
    ns=$(echo "$out" | sed -n 's/.*ns_per_run=\([0-9NaN]*\).*/\1/p')
    [[ -z "$ns" ]] && ns="NaN"
    results+=("$ns")
    printf " %12s" "$ns"
  done
  ratio="-"
  if [[ $boa_idx -ge 0 && $node_idx -ge 0 ]]; then
    boa_v=${results[$boa_idx]}
    node_v=${results[$node_idx]}
    if [[ "$boa_v" != "NaN" && "$node_v" != "NaN" && "$node_v" != "0" ]]; then
      ratio=$(awk -v b="$boa_v" -v n="$node_v" 'BEGIN { printf "%.1fx", b/n }')
    fi
  fi
  printf " %12s\n" "$ratio"
done
