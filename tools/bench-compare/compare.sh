#!/usr/bin/env bash
# Compatibility entry point. The Node orchestrator owns process pairing,
# distribution statistics, sink validation, and machine-readable output.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
exec node "$REPO_ROOT/tools/bench-compare/compare.mjs" "$@"
