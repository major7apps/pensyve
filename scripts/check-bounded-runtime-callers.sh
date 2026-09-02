#!/usr/bin/env bash
set -euo pipefail

paths=(
  pensyve-mcp/src
  pensyve-mcp-gateway/src
  pensyve-mcp-tools/src
  pensyve-cli/src
  pensyve-python/src
)
if (( $# )); then
  paths=("$@")
fi

scanner="$(mktemp "${TMPDIR:-/tmp}/pensyve-bounded-runtime-guard.XXXXXX")"
trap 'rm -f "$scanner"' EXIT
rustc --edition=2024 -D warnings scripts/check-bounded-runtime-callers.rs -o "$scanner"
"$scanner" "${paths[@]}"
