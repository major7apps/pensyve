#!/usr/bin/env bash
# Live MCP smoke test for the Pensyve Cursor adapter.
#
# Confirms the MCP tool call shapes the rules instruct the model to use are
# actually accepted by the MCP server. Does NOT test behavioral compliance —
# only validates that call parameter shapes match the server's schema.
#
# Requires either:
#   - PENSYVE_API_KEY set + Pensyve Cloud reachable (default)
#   - pensyve-mcp binary on PATH + PENSYVE_USE_LOCAL=1 set
#
# Uses the MCP Inspector CLI (npx @modelcontextprotocol/inspector) for tool
# invocation. If Inspector is unavailable, prints instructions and exits.

set -euo pipefail

USE_LOCAL="${PENSYVE_USE_LOCAL:-0}"
TEST_ENTITY="pensyve-cursor-smoketest"
TEST_EPISODE_ID=""

EXIT_CODE=0

echo "Pensyve Cursor adapter — MCP smoke test"
echo "========================================"
echo ""

# Check prerequisites
if ! command -v npx >/dev/null 2>&1; then
  echo "ERROR: 'npx' not found. Install Node.js to run the MCP Inspector CLI."
  exit 1
fi

if [ "$USE_LOCAL" = "1" ]; then
  if ! command -v pensyve-mcp >/dev/null 2>&1; then
    echo "ERROR: 'pensyve-mcp' binary not on PATH. Build with:"
    echo "  cargo build --release -p pensyve-mcp"
    echo "  cp target/release/pensyve-mcp /usr/local/bin/"
    exit 1
  fi
  echo "Mode: Local stdio (pensyve-mcp binary)"
else
  if [ -z "${PENSYVE_API_KEY:-}" ]; then
    echo "ERROR: PENSYVE_API_KEY not set. Either:"
    echo "  1. Export PENSYVE_API_KEY=psy_... for Cloud mode, or"
    echo "  2. Export PENSYVE_USE_LOCAL=1 for local stdio mode"
    exit 1
  fi
  echo "Mode: Cloud (https://mcp.pensyve.com/mcp)"
fi
echo ""

# Helper: invoke an MCP tool and parse the response
# Usage: invoke_tool <tool_name> <params_json>
invoke_tool() {
  local tool="$1"
  local params="$2"
  local response
  if [ "$USE_LOCAL" = "1" ]; then
    response=$(npx --yes @modelcontextprotocol/inspector --cli pensyve-mcp --stdio \
      --method tools/call --tool-name "$tool" --tool-arg "$params" 2>&1)
  else
    response=$(npx --yes @modelcontextprotocol/inspector --cli \
      --transport http --url "https://mcp.pensyve.com/mcp" \
      --header "Authorization: Bearer $PENSYVE_API_KEY" \
      --method tools/call --tool-name "$tool" --tool-arg "$params" 2>&1)
  fi
  echo "$response"
}

# Test 1: pensyve_remember — RememberParams schema
echo "Test 1: pensyve_remember(entity, fact, confidence?)"
R1=$(invoke_tool pensyve_remember \
  '{"entity":"'"$TEST_ENTITY"'","fact":"[smoketest] Cursor adapter lint canary","confidence":0.9}')
if echo "$R1" | grep -qi 'error'; then
  echo "  FAIL: $R1"
  EXIT_CODE=1
else
  echo "  PASS"
fi
echo ""

# Test 2: pensyve_episode_start — EpisodeStartParams schema (participants only, no continuation_of)
echo "Test 2: pensyve_episode_start(participants)"
R2=$(invoke_tool pensyve_episode_start \
  '{"participants":["cursor","'"$TEST_ENTITY"'"]}')
if echo "$R2" | grep -qi 'error'; then
  echo "  FAIL: $R2"
  EXIT_CODE=1
else
  TEST_EPISODE_ID=$(echo "$R2" | grep -oP '(?<="episode_id":")[^"]+' | head -1 || true)
  if [ -z "$TEST_EPISODE_ID" ]; then
    echo "  FAIL: could not extract episode_id from response"
    EXIT_CODE=1
  else
    echo "  PASS (episode_id=$TEST_EPISODE_ID)"
  fi
fi
echo ""

# Test 3: pensyve_observe — ObserveParams schema (requires source_entity and about_entity)
echo "Test 3: pensyve_observe(episode_id, content, source_entity, about_entity, content_type?)"
if [ -n "$TEST_EPISODE_ID" ]; then
  R3=$(invoke_tool pensyve_observe \
    '{"episode_id":"'"$TEST_EPISODE_ID"'","content":"[smoketest] Cursor adapter observe canary","source_entity":"cursor","about_entity":"'"$TEST_ENTITY"'","content_type":"text"}')
  if echo "$R3" | grep -qi 'error'; then
    echo "  FAIL: $R3"
    EXIT_CODE=1
  else
    echo "  PASS"
  fi
else
  echo "  SKIP (no episode_id from test 2)"
  EXIT_CODE=1
fi
echo ""

# Test 4: pensyve_recall — RecallParams schema (no related_entities)
echo "Test 4: pensyve_recall(query, entity?, types?, limit?, min_confidence?)"
R4=$(invoke_tool pensyve_recall \
  '{"query":"Cursor adapter lint canary","entity":"'"$TEST_ENTITY"'","types":["semantic","episodic"],"limit":5}')
if echo "$R4" | grep -qi 'error'; then
  echo "  FAIL: $R4"
  EXIT_CODE=1
else
  echo "  PASS"
fi
echo ""

# Test 5: pensyve_inspect — InspectParams schema (entity, memory_type?, limit?)
echo "Test 5: pensyve_inspect(entity, memory_type?, limit?)"
R5=$(invoke_tool pensyve_inspect \
  '{"entity":"'"$TEST_ENTITY"'","limit":5}')
if echo "$R5" | grep -qi 'error'; then
  echo "  FAIL: $R5"
  EXIT_CODE=1
else
  echo "  PASS"
fi
echo ""

# Test 6: pensyve_episode_end — EpisodeEndParams schema
echo "Test 6: pensyve_episode_end(episode_id, outcome?)"
if [ -n "$TEST_EPISODE_ID" ]; then
  R6=$(invoke_tool pensyve_episode_end \
    '{"episode_id":"'"$TEST_EPISODE_ID"'","outcome":"success"}')
  if echo "$R6" | grep -qi 'error'; then
    echo "  FAIL: $R6"
    EXIT_CODE=1
  else
    echo "  PASS"
  fi
else
  echo "  SKIP (no episode_id)"
fi
echo ""

# Cleanup: forget the test entity
echo "Cleanup: pensyve_forget(entity='$TEST_ENTITY')"
invoke_tool pensyve_forget '{"entity":"'"$TEST_ENTITY"'"}' >/dev/null 2>&1 || true
echo "  (cleanup complete)"
echo ""

if [ "$EXIT_CODE" = "0" ]; then
  echo "All MCP smoke tests PASSED."
else
  echo "MCP smoke tests FAILED. See errors above."
fi

exit "$EXIT_CODE"
