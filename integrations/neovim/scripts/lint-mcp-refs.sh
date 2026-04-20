#!/usr/bin/env bash
# Static MCP contract linter for the Pensyve Neovim adapter rules.
#
# Verifies that every pensyve_* call example in instructions/*.md conforms to
# the current MCP tool schema in pensyve-mcp-tools/src/params.rs. Catches the
# category of bug PR #58 surfaced in the Claude Code adapter (unsupported
# parameters, missing required fields).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RULES_DIR="$SCRIPT_DIR/../instructions"

EXIT_CODE=0

if [ ! -d "$RULES_DIR" ]; then
  echo "ERROR: Rules directory not found: $RULES_DIR"
  exit 1
fi

echo "Linting MCP references in $RULES_DIR..."
echo ""

# Check 1: no actual use of unsupported 'related_entities' parameter in call examples.
# We look for pensyve_recall( blocks that contain related_entities as a parameter.
# Lines that say "no related_entities" (documentation/reminders) are excluded.
echo "Check 1: no unsupported 'related_entities' on pensyve_recall"
FOUND_RELATED=0
for rule_file in "$RULES_DIR"/*.md; do
  # Extract lines that contain pensyve_recall( and related_entities together,
  # excluding documentation lines (those containing "**no**" or "no `related_entities`")
  if rg -n 'related_entities' "$rule_file" | rg -v '(\*\*no\*\*|no `related_entities`)' | rg -q 'pensyve_recall\|related_entities'; then
    echo "  FAIL in $rule_file: 'related_entities' used in a pensyve_recall call"
    FOUND_RELATED=1
  fi
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
       }' "$rule_file")
done
if [ "$FOUND_RELATED" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi
echo ""

# Check 2: no actual use of unsupported 'continuation_of' parameter in call examples.
# Documentation lines saying "no continuation_of" are excluded.
echo "Check 2: no unsupported 'continuation_of' on pensyve_episode_start"
FOUND_CONT=0
for rule_file in "$RULES_DIR"/*.md; do
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
       }' "$rule_file")
done
if [ "$FOUND_CONT" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi
echo ""

# Check 3: every pensyve_observe call example in a code block has source_entity and about_entity
# Scan each .md file's pensyve_observe blocks; require both fields nearby.
echo "Check 3: every pensyve_observe example has source_entity and about_entity"
MISSING_FIELDS=0
for rule_file in "$RULES_DIR"/*.md; do
  # Find each pensyve_observe( block (up to the next closing paren within 20 lines)
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
       }}' "$rule_file")
done
if [ "$MISSING_FIELDS" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi
echo ""

# Check 4: provenance tag format — every proactive/auto-capture tag uses [<origin>/<trigger>/<tier>]
# Format supports both 3-segment [origin/trigger/tier] and 4-segment [origin/trigger/residual/tier]
# where the third segment may be a literal "residual" qualifier before the tier.
echo "Check 4: provenance tag format"
VALID_PROVENANCE_RE='\[(proactive|auto-capture)/(in-flight|stop|pre-compact|curator|user)/(tier-1|tier-2|residual/tier-1|residual/tier-2|open-question)\]'
if rg -n '\[(proactive|auto-capture)' "$RULES_DIR" | rg -v "$VALID_PROVENANCE_RE"; then
  echo "  FAIL: some provenance tags do not match [<origin>/<trigger>/<tier>] format"
  EXIT_CODE=1
else
  echo "  PASS"
fi
echo ""

# Check 5: procedural memory convention — [procedural] prefix is used in observe content
echo "Check 5: procedural convention uses [procedural] prefix in pensyve_observe content"
# Only a warning if no rule mentions [procedural] — may be legitimate if the rule is non-procedural
if ! rg -q '\[procedural\]' "$RULES_DIR"; then
  echo "  WARN: no [procedural] prefix usage found across rules. Expected in memory-reflex and flow rules that handle procedural captures."
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
