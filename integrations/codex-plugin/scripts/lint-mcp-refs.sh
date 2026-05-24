#!/usr/bin/env bash
# Static MCP contract linter for the Pensyve Codex CLI adapter.
#
# Verifies that every pensyve_* call example in the Codex plugin instructions
# conforms to the current MCP tool schema in pensyve-mcp-tools/src/params.rs.
# Catches the category of bug PR #58 surfaced in the Claude Code adapter
# (unsupported parameters, missing required fields).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$SCRIPT_DIR/.."
MCP_REF_FILES=(
  "$PLUGIN_ROOT/AGENTS.md"
  "$PLUGIN_ROOT/skills/pensyve/SKILL.md"
  "$PLUGIN_ROOT/skills/mention-workflow/SKILL.md"
  "$PLUGIN_ROOT/commands/pensyve.md"
  "$PLUGIN_ROOT/docs/ARCHITECTURE.md"
)

EXIT_CODE=0

echo "Checking required Codex plugin surfaces..."
for file in "${MCP_REF_FILES[@]}"; do
  if [ ! -f "$file" ]; then
    echo "  FAIL: required file missing: $file"
    EXIT_CODE=1
  else
    echo "  PASS: $file"
  fi
done
echo ""

if [ "$EXIT_CODE" != "0" ]; then
  echo "Codex plugin surface checks FAILED."
  exit "$EXIT_CODE"
fi

echo "Linting MCP references in:"
printf '  %s\n' "${MCP_REF_FILES[@]}"
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
       }' "${MCP_REF_FILES[@]}")
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
       }' "${MCP_REF_FILES[@]}")
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
       }}' "${MCP_REF_FILES[@]}")
if [ "$MISSING_FIELDS" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi
echo ""

# Check 4: provenance tag format — every proactive/auto-capture tag uses [<origin>/<trigger>/<tier>]
echo "Check 4: provenance tag format"
VALID_PROVENANCE_RE='\[(proactive|auto-capture)/(in-flight|stop|pre-compact|curator|user)/(tier-1|tier-2|residual/tier-1|residual/tier-2|open-question)\]'
if rg -n '\[(proactive|auto-capture)' "${MCP_REF_FILES[@]}" | rg -v "$VALID_PROVENANCE_RE"; then
  echo "  FAIL: some provenance tags do not match [<origin>/<trigger>/<tier>] format"
  EXIT_CODE=1
else
  echo "  PASS"
fi
echo ""

# Check 5: procedural memory convention — [procedural] prefix is used in observe content
echo "Check 5: procedural convention uses [procedural] prefix in pensyve_observe content"
if ! rg -q '\[procedural\]' "${MCP_REF_FILES[@]}"; then
  echo "  WARN: no [procedural] prefix usage found. Expected in AGENTS.md."
else
  echo "  PASS"
fi
echo ""

# Check 6: @-mention compatibility is explicit about today's Codex limitation.
echo "Check 6: @-mention workflow documents current Codex dispatch limitation"
if rg -q 'true @-mention dispatch is not currently exposed|Codex does not currently expose true @-mention dispatch' \
  "$PLUGIN_ROOT/skills/mention-workflow/SKILL.md" \
  "$PLUGIN_ROOT/commands/pensyve.md" \
  "$PLUGIN_ROOT/docs/ARCHITECTURE.md"; then
  echo "  PASS"
else
  echo "  FAIL: mention workflow must document that true @-mention dispatch is not currently exposed"
  EXIT_CODE=1
fi
echo ""

if [ "$EXIT_CODE" = "0" ]; then
  echo "All MCP contract checks PASSED."
else
  echo "MCP contract checks FAILED. Fix the issues above before committing."
fi

exit "$EXIT_CODE"
