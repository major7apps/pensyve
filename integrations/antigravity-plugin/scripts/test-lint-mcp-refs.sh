#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_ROOT="$(mktemp -d /tmp/pensyve-antigravity-lint-test.XXXXXX)"
trap 'rm -rf "$TEST_ROOT"' EXIT

expect_lint_failure() {
  local plugin_path="$1"
  local failure_message="$2"

  if bash "$plugin_path/scripts/lint-mcp-refs.sh" >/dev/null 2>&1; then
    echo "FAIL: $failure_message"
    exit 1
  fi
}

cp -R "$PLUGIN_DIR" "$TEST_ROOT/legacy-text"

bash "$TEST_ROOT/legacy-text/scripts/lint-mcp-refs.sh" >/dev/null

printf '%s\n' \
  'Former Gemini'' CLI users should install the Antigravity plugin. Google'\''s enterprise and paid API-key Gemini'' CLI compatibility path is a Google-owned legacy option; Pensyve supports Antigravity as its current Google coding-agent integration. gemini'' mcp add legacy' \
  >> "$TEST_ROOT/legacy-text/README.md"

expect_lint_failure \
  "$TEST_ROOT/legacy-text" \
  "legacy text appended to the approved migration line was accepted"

cp -R "$PLUGIN_DIR" "$TEST_ROOT/mixed-provenance"
printf '%s\n' \
  '[proactive/in-flight/tier-1] [proactive/invalid/tier-3]' \
  >> "$TEST_ROOT/mixed-provenance/skills/recall/SKILL.md"

expect_lint_failure \
  "$TEST_ROOT/mixed-provenance" \
  "a line containing valid and invalid provenance tags was accepted"

cp -R "$PLUGIN_DIR" "$TEST_ROOT/stale-provenance"
printf '%s\n' 'source_entity: "gemini"' \
  >> "$TEST_ROOT/stale-provenance/rules/memory-reflex.md"

expect_lint_failure \
  "$TEST_ROOT/stale-provenance" \
  "stale Gemini provenance was accepted"

for artifact in GEMINI.md gemini-extension.json .gemini/settings.json commands/legacy.toml; do
  fixture_name="$(printf '%s' "$artifact" | tr '/.' '--')"
  cp -R "$PLUGIN_DIR" "$TEST_ROOT/$fixture_name"
  mkdir -p "$TEST_ROOT/$fixture_name/$(dirname "$artifact")"
  printf '%s\n' 'legacy fixture' > "$TEST_ROOT/$fixture_name/$artifact"

  expect_lint_failure \
    "$TEST_ROOT/$fixture_name" \
    "forbidden legacy artifact $artifact was accepted"
done

echo "Antigravity lint regression tests PASSED."
