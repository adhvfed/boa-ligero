#!/usr/bin/env bash
# D1 default-JIT resource gate for macOS. Produces raw per-process output,
# diagnostics, metadata, a TSV ledger, and a machine-checkable summary.

set -euo pipefail
export LC_ALL=C
export LANG=C

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUNNER="$REPO_ROOT/target/release/bench-compare-runner"
NO_ARTIFACT="$REPO_ROOT/benches/scripts/microbench/one-shot-ineligible-loop.js"
SATURATION="$REPO_ROOT/benches/scripts/microbench/jit-resource-saturation.js"
OUTPUT_DIR="${1:?usage: jit-resource-gate.sh OUTPUT_DIR}"
PAIRS="${PAIRS:-7}"
RSS_LIMIT_BYTES=$((64 * 1024 * 1024))

[[ "$(uname -s)" == "Darwin" ]] || {
    echo "jit-resource-gate.sh currently requires macOS /usr/bin/time -l" >&2
    exit 2
}
[[ "$PAIRS" =~ ^[1-9][0-9]*$ ]] || {
    echo "PAIRS must be a positive integer" >&2
    exit 2
}
[[ -x "$RUNNER" ]] || {
    echo "missing release runner: cargo build --release -p boa_benches --features jit --bin bench-compare-runner" >&2
    exit 2
}
[[ ! -e "$OUTPUT_DIR/results.tsv" ]] || {
    echo "output directory already contains a gate run: $OUTPUT_DIR" >&2
    exit 2
}

mkdir -p "$OUTPUT_DIR/raw" "$OUTPUT_DIR/diagnostics"

allocator="Rust standard-library default; no workspace global allocator"
if git -C "$REPO_ROOT" grep -q '#\[global_allocator\]'; then
    allocator="workspace #[global_allocator] override; inspect the recorded Boa commit"
fi

{
    echo "boa_commit=$(git -C "$REPO_ROOT" rev-parse HEAD)"
    echo "runner_sha256=$(shasum -a 256 "$RUNNER" | awk '{print $1}')"
    echo "no_artifact_sha256=$(shasum -a 256 "$NO_ARTIFACT" | awk '{print $1}')"
    echo "saturation_sha256=$(shasum -a 256 "$SATURATION" | awk '{print $1}')"
    echo "pairs=$PAIRS"
    echo "rss_limit_bytes=$RSS_LIMIT_BYTES"
    echo "allocator=$allocator"
    uname -a
    sw_vers
    sysctl -n machdep.cpu.brand_string
    sysctl -n hw.logicalcpu
} > "$OUTPUT_DIR/metadata.txt"

printf 'scenario\tpair\tposition\tmode\telapsed_ns\tpeak_rss_bytes\tacc\n' \
    > "$OUTPUT_DIR/results.tsv"
printf 'scenario\tpair\tjit_minus_interpreter_peak_rss_bytes\n' \
    > "$OUTPUT_DIR/rss-deltas.tsv"

field() {
    local name="$1"
    local line="$2"
    tr ' ' '\n' <<< "$line" | sed -n "s/^${name}=//p" | tail -n 1
}

run_one() {
    local scenario="$1"
    local pair="$2"
    local position="$3"
    local mode="$4"
    local fixture="$5"
    local stem="$OUTPUT_DIR/raw/${scenario}-pair-${pair}-${position}-${mode}"
    local -a command=("$RUNNER" "$fixture" 1 0 "$mode")

    if [[ "$scenario" == "maximum-diagnostics" && "$mode" == "osr-cold" ]]; then
        command+=(
            --jit-diagnostics-out "$OUTPUT_DIR/diagnostics/pair-${pair}.json"
            --jit-diagnostic-record-limit 4096
        )
    fi

    /usr/bin/time -l "${command[@]}" > "$stem.stdout" 2> "$stem.time"

    local line elapsed rss acc
    line="$(tail -n 1 "$stem.stdout")"
    elapsed="$(field elapsed_ns "$line")"
    acc="$(field acc "$line")"
    rss="$(awk '/maximum resident set size/ { print $1 }' "$stem.time" | tail -n 1)"
    [[ "$elapsed" =~ ^[0-9]+$ && "$rss" =~ ^[0-9]+$ && "$acc" =~ ^-?[0-9]+$ ]] || {
        echo "unparseable result for $stem" >&2
        exit 2
    }

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$scenario" "$pair" "$position" "$mode" "$elapsed" "$rss" "$acc" \
        >> "$OUTPUT_DIR/results.tsv"
    printf '%s %s %s\n' "$elapsed" "$rss" "$acc"
}

failed=0
for scenario in no-artifact saturation maximum-diagnostics; do
    fixture="$SATURATION"
    [[ "$scenario" == "no-artifact" ]] && fixture="$NO_ARTIFACT"

    for ((pair = 1; pair <= PAIRS; pair++)); do
        if ((pair % 2 == 1)); then
            first=interp
            second=osr-cold
        else
            first=osr-cold
            second=interp
        fi

        read -r _first_elapsed first_rss first_acc < <(
            run_one "$scenario" "$pair" first "$first" "$fixture"
        )
        read -r _second_elapsed second_rss second_acc < <(
            run_one "$scenario" "$pair" second "$second" "$fixture"
        )

        if [[ "$first_acc" != "$second_acc" ]]; then
            echo "$scenario pair $pair: semantic sink mismatch" >&2
            failed=1
        fi

        if [[ "$first" == "interp" ]]; then
            interp_rss="$first_rss"
            jit_rss="$second_rss"
        else
            interp_rss="$second_rss"
            jit_rss="$first_rss"
        fi
        rss_delta=$((jit_rss - interp_rss))
        printf '%s\t%s\t%s\n' "$scenario" "$pair" "$rss_delta" \
            >> "$OUTPUT_DIR/rss-deltas.tsv"
        if [[ "$scenario" != "no-artifact" ]] && ((rss_delta > RSS_LIMIT_BYTES)); then
            echo "$scenario pair $pair: RSS delta $rss_delta exceeds $RSS_LIMIT_BYTES" >&2
            failed=1
        fi
    done
done

median() {
    sort -n | awk '{ values[NR] = $1 } END {
        if (NR % 2 == 1) print values[(NR + 1) / 2];
        else print (values[NR / 2] + values[NR / 2 + 1]) / 2;
    }'
}

interp_median="$(awk -F '\t' '$1 == "no-artifact" && $4 == "interp" { print $5 }' \
    "$OUTPUT_DIR/results.tsv" | median)"
jit_median="$(awk -F '\t' '$1 == "no-artifact" && $4 == "osr-cold" { print $5 }' \
    "$OUTPUT_DIR/results.tsv" | median)"
ratio="$(awk -v jit="$jit_median" -v interp="$interp_median" 'BEGIN { printf "%.6f", jit / interp }')"

{
    echo "binding=$([[ "$PAIRS" == 7 ]] && echo true || echo false)"
    echo "no_artifact_interpreter_median_ns=$interp_median"
    echo "no_artifact_jit_median_ns=$jit_median"
    echo "no_artifact_jit_over_interpreter=$ratio"
    echo "rss_limit_bytes=$RSS_LIMIT_BYTES"
} > "$OUTPUT_DIR/summary.txt"

if ! awk -v ratio="$ratio" 'BEGIN { exit !(ratio <= 1.05) }'; then
    echo "no-artifact JIT median ratio $ratio exceeds 1.05" >&2
    failed=1
fi

exit "$failed"
