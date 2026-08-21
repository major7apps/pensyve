#!/usr/bin/env bash
# Static MCP contract and package linter for the Pensyve Antigravity plugin.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RULES_DIR="$PLUGIN_DIR/rules"
SKILLS_DIR="$PLUGIN_DIR/skills"
README_FILE="$PLUGIN_DIR/README.md"
EXIT_CODE=0

if [ ! -d "$RULES_DIR" ]; then
  echo "ERROR: Rules directory not found: $RULES_DIR"
  exit 1
fi

mapfile -d '' CONTENT_FILES < <(find "$RULES_DIR" -type f -name '*.md' -print0 | sort -z)
if [ -d "$SKILLS_DIR" ]; then
  while IFS= read -r -d '' skill_file; do
    CONTENT_FILES+=("$skill_file")
  done < <(find "$SKILLS_DIR" -type f -name 'SKILL.md' -print0 | sort -z)
fi
if [ -f "$README_FILE" ]; then
  CONTENT_FILES+=("$README_FILE")
fi

if [ "${#CONTENT_FILES[@]}" -eq 0 ]; then
  echo "ERROR: No rule or skill content found under $PLUGIN_DIR"
  exit 1
fi

echo "Linting MCP references in $PLUGIN_DIR..."

echo "Check 1: only documented Pensyve MCP tool names are referenced"
UNKNOWN_TOOLS=0
while IFS= read -r tool_name; do
  case "$tool_name" in
    pensyve_recall|pensyve_remember|pensyve_observe|pensyve_episode_start|pensyve_episode_end|pensyve_forget|pensyve_inspect)
      ;;
    *)
      echo "  FAIL: undocumented MCP tool name: $tool_name"
      UNKNOWN_TOOLS=1
      ;;
  esac
done < <(rg -o --no-filename 'pensyve_[a-z_]+' "${CONTENT_FILES[@]}" | sort -u)
if [ "$UNKNOWN_TOOLS" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi

echo "Check 2: unsupported MCP parameters are absent from call examples"
UNSUPPORTED_PARAMS=0
for content_file in "${CONTENT_FILES[@]}"; do
  while IFS= read -r line; do
    if [ -n "$line" ]; then
      echo "  FAIL: $line"
      UNSUPPORTED_PARAMS=1
    fi
  done < <(awk '/pensyve_recall\(/{capture=1; buf=""}
       capture {buf = buf "\n" $0}
       capture && /\)/{
       if(buf ~ /related_entities/ && buf !~ /\*\*no\*\*/ && buf !~ /no `related_entities`/)
       print FILENAME ": related_entities found in pensyve_recall block:" buf;
       capture=0
       }' "$content_file")
  while IFS= read -r line; do
    if [ -n "$line" ]; then
      echo "  FAIL: $line"
      UNSUPPORTED_PARAMS=1
    fi
  done < <(awk '/pensyve_episode_start\(/{capture=1; buf=""}
       capture {buf = buf "\n" $0}
       capture && /\)/{
       if(buf ~ /continuation_of/ && buf !~ /\*\*no\*\*/ && buf !~ /no `continuation_of`/)
       print FILENAME ": continuation_of found in pensyve_episode_start block:" buf;
       capture=0
       }' "$content_file")
done
if [ "$UNSUPPORTED_PARAMS" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi

echo "Check 3: every pensyve_observe example has source_entity and about_entity"
MISSING_FIELDS=0
for content_file in "${CONTENT_FILES[@]}"; do
  while IFS= read -r line; do
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
       if(buf !~ /source_entity/) print FILENAME ": missing source_entity near:" buf;
       if(buf !~ /about_entity/) print FILENAME ": missing about_entity near:" buf;
       capture=0;
       }}' "$content_file")
done
if [ "$MISSING_FIELDS" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi

echo "Check 4: provenance tags use the documented format"
VALID_PROVENANCE_RE='\[(proactive|auto-capture)/(in-flight|stop|pre-compact|curator|user)/(tier-1|tier-2|residual/tier-1|residual/tier-2|open-question)\]'
if rg -n '\[(proactive|auto-capture)' "${CONTENT_FILES[@]}" | rg -v "$VALID_PROVENANCE_RE"; then
  echo "  FAIL: some provenance tags do not match [<origin>/<trigger>/<tier>]"
  EXIT_CODE=1
else
  echo "  PASS"
fi

echo "Check 5: Antigravity provenance is present and legacy provenance is constrained"
if rg -q 'source_entity: "antigravity-cli"|participants: \["antigravity-cli"' "${CONTENT_FILES[@]}"; then
  echo "  PASS: antigravity-cli provenance found"
else
  echo "  FAIL: no antigravity-cli provenance found"
  EXIT_CODE=1
fi

LEGACY_CLIENT_RE='Gemini'' CLI|gemini''-cli|gemini'' mcp|gemini''-extension|gemini''cli\.com'
APPROVED_MIGRATION_NOTE='Former Gemini'' CLI users should install the Antigravity plugin'
for content_file in "${CONTENT_FILES[@]}"; do
  if [ "$content_file" = "$README_FILE" ]; then
    if rg -n -i "$LEGACY_CLIENT_RE" "$content_file" | rg -v -F "$APPROVED_MIGRATION_NOTE"; then
      echo "  FAIL: unapproved legacy reference in $content_file"
      EXIT_CODE=1
    fi
  elif rg -n -i "$LEGACY_CLIENT_RE" "$content_file"; then
    echo "  FAIL: legacy Google CLI reference in $content_file"
    EXIT_CODE=1
  fi
done

echo "Check 6: every rule is at most 12,000 characters"
RULE_SIZE_FAILURE=0
while IFS= read -r -d '' rule_file; do
  rule_chars="$(wc -m < "$rule_file")"
  if [ "$rule_chars" -gt 12000 ]; then
    echo "  FAIL: $rule_file has $rule_chars characters"
    RULE_SIZE_FAILURE=1
  fi
done < <(find "$RULES_DIR" -type f -name '*.md' -print0)
if [ "$RULE_SIZE_FAILURE" = "0" ]; then
  echo "  PASS"
else
  EXIT_CODE=1
fi

echo "Check 7: procedural captures use the [procedural] prefix"
if rg -q '\[procedural\]' "${CONTENT_FILES[@]}"; then
  echo "  PASS"
else
  echo "  WARN: no [procedural] capture example found"
fi

if [ "$EXIT_CODE" = "0" ]; then
  echo "All MCP contract and package checks PASSED."
else
  echo "MCP contract and package checks FAILED."
fi

exit "$EXIT_CODE"
