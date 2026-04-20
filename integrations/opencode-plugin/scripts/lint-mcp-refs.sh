#!/usr/bin/env bash
# Static MCP contract linter for the Pensyve opencode adapter.
#
# Verifies that every pensyve_* call example in AGENTS.md
# conforms to the current MCP tool schema in pensyve-mcp-tools/src/params.rs.
# Catches the category of bug PR #58 surfaced in the Claude Code adapter
# (unsupported parameters, missing required fields).
#
# opencode uses a single consolidated AGENTS.md file for working-memory substrate
# instructions, so this script targets AGENTS.md directly.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RULES_FILE="$SCRIPT_DIR/../AGENTS.md"

EXIT_CODE=0

if [ ! -f "$RULES_FILE" ]; then
  echo "ERROR: AGENTS.md not found: $RULES_FILE"
  exit 1
fi

echo "Linting MCP references in $RULES_FILE..."
echo ""

# Check 1: no actual use of unsupported 'related_entities' parameter in call examples.
# Lines that say "no related_entities" (documentation/reminders) are excluded.
echo "Check 1: no unsupported 'related_entities' on pensyve_recall"
FOUND_RELATED=0
# Also catch if related_entities appears inside a pensyve_recall( block
while read -r line; do
  if [ -n "$line" ]; then
    echo "  FAIL: $line"
    FOUND_RELATED=1
  fi
done < <(awk '/pensyve_recall\(/{capture=1; buf=""}
       capture {buf = buf "\n" $0}
       capture && /\)/{
       if(buf ~ /related_entities/ && buf !~ /\*\*no\*\*/ && buf !~ /no `related_entities`/)
       print FILENAME ": related_entities found in pensyve_recall block: " buf;
       capture=0
       }' "$RULES_FILE")
if [ "$FOUND_RELATED" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi
echo ""

# Check 2: no actual use of unsupported 'continuation_of' parameter in call examples.
echo "Check 2: no unsupported 'continuation_of' on pensyve_episode_start"
FOUND_CONT=0
while read -r line; do
  if [ -n "$line" ]; then
    echo "  FAIL: $line"
    FOUND_CONT=1
  fi
done < <(awk '/pensyve_episode_start\(/{capture=1; buf=""}
       capture {buf = buf "\n" $0}
       capture && /\)/{
       if(buf ~ /continuation_of/ && buf !~ /\*\*no\*\*/ && buf !~ /no `continuation_of`/)
       print FILENAME ": continuation_of found in pensyve_episode_start block: " buf;
       capture=0
       }' "$RULES_FILE")
if [ "$FOUND_CONT" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi
echo ""

# Check 3: every pensyve_observe call example in a code block has source_entity and about_entity
echo "Check 3: every pensyve_observe example has source_entity and about_entity"
MISSING_FIELDS=0
while read -r line; do
  if [ -n "$line" ]; then
    echo "  FAIL: $line"
    MISSING_FIELDS=1
  fi
done < <(awk '/pensyve_observe\(/{capture=1; buf=""; depth=0}
       capture {buf = buf "\n" $0;
       for(i=1; i<=length($0); i++){
       c=substr($0,i,1);
       if(c=="(") depth++;
       if(c==")") depth--;
       };
       if(depth==0 && buf ~ /pensyve_observe\(/){
       if(buf !~ /source_entity/) print FILENAME ": missing source_entity near: " buf;
       if(buf !~ /about_entity/) print FILENAME ": missing about_entity near: " buf;
       capture=0;
       }}' "$RULES_FILE")
if [ "$MISSING_FIELDS" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi
echo ""

# Check 4: provenance tag format — every proactive/auto-capture tag uses [<origin>/<trigger>/<tier>]
echo "Check 4: provenance tag format"
VALID_PROVENANCE_RE='\[(proactive|auto-capture)/(in-flight|stop|pre-compact|curator|user)/(tier-1|tier-2|residual/tier-1|residual/tier-2|open-question)\]'
if rg -n '\[(proactive|auto-capture)' "$RULES_FILE" | rg -v "$VALID_PROVENANCE_RE"; then
  echo "  FAIL: some provenance tags do not match [<origin>/<trigger>/<tier>] format"
  EXIT_CODE=1
else
  echo "  PASS"
fi
echo ""

# Check 5: procedural memory convention — [procedural] prefix is used in observe content
echo "Check 5: procedural convention uses [procedural] prefix in pensyve_observe content"
if ! rg -q '\[procedural\]' "$RULES_FILE"; then
  echo "  WARN: no [procedural] prefix usage found. Expected in AGENTS.md."
else
  echo "  PASS"
fi
echo ""

if [ "$EXIT_CODE" = "0" ]; then
  echo "All MCP contract checks PASSED."
else
  echo "MCP contract checks FAILED. Fix the issues above before committing."
fi

exit "$EXIT_CODE"
