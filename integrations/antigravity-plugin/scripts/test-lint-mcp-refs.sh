#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_ROOT="$(mktemp -d /tmp/pensyve-antigravity-lint-test.XXXXXX)"
trap 'rm -rf "$TEST_ROOT"' EXIT

cp -R "$PLUGIN_DIR" "$TEST_ROOT/plugin"

bash "$TEST_ROOT/plugin/scripts/lint-mcp-refs.sh" >/dev/null

printf '%s\n' \
  'Former Gemini'' CLI users should install the Antigravity plugin. Google'\''s enterprise and paid API-key Gemini'' CLI compatibility path is a Google-owned legacy option; Pensyve supports Antigravity as its current Google coding-agent integration. gemini'' mcp add legacy' \
  >> "$TEST_ROOT/plugin/README.md"

if bash "$TEST_ROOT/plugin/scripts/lint-mcp-refs.sh" >/dev/null 2>&1; then
  echo "FAIL: legacy text appended to the approved migration line was accepted"
  exit 1
fi

echo "Antigravity lint regression tests PASSED."
