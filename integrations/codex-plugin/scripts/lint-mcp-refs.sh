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
MENTION_DOC_FILES=(
  "$PLUGIN_ROOT/skills/mention-workflow/SKILL.md"
  "$PLUGIN_ROOT/commands/pensyve.md"
  "$PLUGIN_ROOT/docs/ARCHITECTURE.md"
)
OPENAI_METADATA="$PLUGIN_ROOT/skills/pensyve/agents/openai.yaml"
PLUGIN_MANIFEST="$PLUGIN_ROOT/.codex-plugin/plugin.json"

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
if grep -nE '\[(proactive|auto-capture)' "${MCP_REF_FILES[@]}" | grep -vE "$VALID_PROVENANCE_RE"; then
  echo "  FAIL: some provenance tags do not match [<origin>/<trigger>/<tier>] format"
  EXIT_CODE=1
else
  echo "  PASS"
fi
echo ""

# Check 5: procedural memory convention — [procedural] prefix is used in observe content
echo "Check 5: procedural convention uses [procedural] prefix in pensyve_observe content"
if ! grep -qE '\[procedural\]' "${MCP_REF_FILES[@]}"; then
  echo "  WARN: no [procedural] prefix usage found in configured plugin surfaces."
else
  echo "  PASS"
fi
echo ""

# Check 6: @-mention compatibility is explicit about today's Codex limitation.
echo "Check 6: @-mention workflow documents current Codex dispatch limitation"
MISSING_MENTION_DOC=0
for file in "${MENTION_DOC_FILES[@]}"; do
  if grep -qiE 'true @-mention dispatch is not currently exposed|Codex does not currently expose true @-mention dispatch' "$file"; then
    echo "  PASS: $file"
  else
    echo "  FAIL: $file"
    MISSING_MENTION_DOC=1
  fi
done
if [ "$MISSING_MENTION_DOC" != "0" ]; then
  echo "  FAIL: mention workflow must document that true @-mention dispatch is not currently exposed in every mention surface"
  EXIT_CODE=1
fi
echo ""

# Check 7: current Codex requires structured DependencyTool objects, not scalar tool names.
echo "Check 7: openai.yaml declares the Pensyve MCP as a structured dependency"
if python3 - "$OPENAI_METADATA" <<'PY'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
lines = path.read_text(encoding="utf-8").splitlines()
items = []
in_dependencies = False
in_tools = False
current = None

# This is intentionally a canonical-format validator, not a general YAML parser.
# The plugin owns this small file and requires dependencies.tools to use block
# mappings, two-space indentation, quoted scalars, and no inline comments. Keeping
# that surface exact avoids adding a YAML runtime dependency to the release job.

for number, raw_line in enumerate(lines, start=1):
    if not raw_line.strip() or raw_line.lstrip().startswith("#"):
        continue
    indent = len(raw_line) - len(raw_line.lstrip(" "))
    text = raw_line.strip()

    if indent == 0:
        in_dependencies = text == "dependencies:"
        in_tools = False
        current = None
        continue
    if not in_dependencies:
        continue
    if indent == 2:
        in_tools = text == "tools:"
        current = None
        continue
    if not in_tools:
        continue

    match = re.fullmatch(r"- ([a-z_]+):\s*(.+)", text) if indent == 4 else None
    if match:
        current = {match.group(1): match.group(2)}
        items.append(current)
        continue
    match = re.fullmatch(r"([a-z_]+):\s*(.+)", text) if indent == 6 else None
    if match and current is not None:
        current[match.group(1)] = match.group(2)
        continue
    raise SystemExit(
        f"non-canonical dependencies.tools syntax at {path}:{number}: {raw_line}"
    )

def scalar(value):
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    raise SystemExit(f"canonical dependencies.tools scalars must be quoted: {value!r}")

parsed = [{key: scalar(value) for key, value in item.items()} for item in items]
expected = [{
    "type": "mcp",
    "value": "pensyve",
    "description": "Pensyve persistent-memory MCP server",
    "transport": "streamable_http",
    "url": "https://mcp.pensyve.com/mcp",
}]
if parsed != expected:
    raise SystemExit(f"expected exactly one Pensyve MCP dependency, found: {parsed!r}")
PY
then
  echo "  PASS"
else
  echo "  FAIL: dependencies.tools must use the canonical one-object Pensyve MCP block"
  EXIT_CODE=1
fi
echo ""

# Check 8: Codex accepts up to three non-empty string prompts, each at most 128 characters.
echo "Check 8: plugin manifest default prompts satisfy Codex constraints"
if python3 - "$PLUGIN_MANIFEST" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
manifest = json.loads(path.read_text(encoding="utf-8"))
value = manifest.get("interface", {}).get("defaultPrompt")
if isinstance(value, str):
    prompts = [value]
elif isinstance(value, list):
    prompts = value
else:
    raise SystemExit("interface.defaultPrompt must be a string or an array of strings")
if len(prompts) > 3:
    raise SystemExit(f"interface.defaultPrompt supports at most 3 entries, found {len(prompts)}")
for index, prompt in enumerate(prompts):
    if not isinstance(prompt, str):
        raise SystemExit(f"defaultPrompt[{index}] must be a string")
    normalized = " ".join(prompt.split())
    if not normalized:
        raise SystemExit(f"defaultPrompt[{index}] must not be empty")
    if len(normalized) > 128:
        raise SystemExit(
            f"defaultPrompt[{index}] exceeds 128 normalized characters ({len(normalized)})"
        )
print(f"validated {len(prompts)} prompts")
PY
then
  echo "  PASS"
else
  echo "  FAIL: invalid interface.defaultPrompt"
  EXIT_CODE=1
fi
echo ""

if [ "$EXIT_CODE" = "0" ]; then
  echo "All MCP contract checks PASSED."
else
  echo "MCP contract checks FAILED. Fix the issues above before committing."
fi

exit "$EXIT_CODE"
