#!/usr/bin/env bash
set -euo pipefail

default_paths=(
  pensyve-mcp/src
  pensyve-mcp-gateway/src
  pensyve-mcp-tools/src
  pensyve-cli/src
  pensyve-python/src
)
if (( $# )); then
  paths=("$@")
else
  paths=("${default_paths[@]}")
fi
pattern='get_all_memories_by_namespace(_including_superseded)?\(|VectorIndex::new\('
found=0

# Keep source line numbers stable while blanking items guarded by an exact
# `cfg(test)`. The guard still scans test-adjacent shipping modules and does
# not allow symbol renames or line exceptions.
shipping_source() {
  awk '
    function brace_delta(line, scrubbed, opens, closes) {
      scrubbed = line
      gsub(/\\"/, "", scrubbed)
      gsub(/"([^"\\]|\\.)*"/, "", scrubbed)
      sub(/\/\/.*/, "", scrubbed)
      opens = gsub(/\{/, "{", scrubbed)
      closes = gsub(/\}/, "}", scrubbed)
      return opens - closes
    }

    function is_cfg_test(line) {
      return line ~ /^[[:space:]]*#[[:space:]]*\[[[:space:]]*cfg[[:space:]]*\([[:space:]]*test[[:space:]]*\)[[:space:]]*\][[:space:]]*$/
    }

    {
      if (skipping) {
        print ""
        delta = brace_delta($0)
        depth += delta
        if (saw_brace && depth <= 0) {
          skipping = 0
          saw_brace = 0
          depth = 0
        } else if (!saw_brace && delta > 0) {
          saw_brace = 1
          if (depth <= 0) {
            skipping = 0
            saw_brace = 0
            depth = 0
          }
        } else if (!saw_brace && $0 ~ /;[[:space:]]*$/) {
          skipping = 0
        }
        next
      }

      if (pending_cfg_test) {
        print ""
        if ($0 ~ /^[[:space:]]*$/ || $0 ~ /^[[:space:]]*#/) {
          next
        }
        pending_cfg_test = 0
        skipping = 1
        delta = brace_delta($0)
        depth = delta
        saw_brace = delta > 0 || $0 ~ /\{/
        if ((!saw_brace && $0 ~ /;[[:space:]]*$/) || (saw_brace && depth <= 0)) {
          skipping = 0
          saw_brace = 0
          depth = 0
        }
        next
      }

      if (is_cfg_test($0)) {
        print ""
        pending_cfg_test = 1
        next
      }

      print
    }
  ' "$1"
}

while IFS= read -r file; do
  if hits="$(shipping_source "$file" | rg -n "$pattern")"; then
    while IFS= read -r hit; do
      printf '%s:%s\n' "$file" "$hit"
    done <<<"$hits"
    found=1
  fi
done < <(rg --files "${paths[@]}" -g '*.rs')

if (( found )); then
  echo 'shipping runtime still contains corpus hydration or a resident vector index' >&2
  exit 1
fi
