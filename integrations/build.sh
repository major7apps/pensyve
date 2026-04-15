#!/usr/bin/env bash
# Pensyve Integrations Build Script
# Vendors shared library files into each integration's _vendor/ or src/ directory.
# Run from: integrations/
#
# Usage: ./build.sh [--validate-only]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SHARED_DIR="${SCRIPT_DIR}/shared"

# Colors (disabled if not a terminal)
if [ -t 1 ]; then
  GREEN='\033[0;32m'
  RED='\033[0;31m'
  YELLOW='\033[0;33m'
  NC='\033[0m'
else
  GREEN=''
  RED=''
  YELLOW=''
  NC=''
fi

VALIDATE_ONLY=false
ERRORS=0

if [ "${1:-}" = "--validate-only" ]; then
  VALIDATE_ONLY=true
fi

# ---------- helpers ----------

log_ok()   { printf "${GREEN}  [OK]${NC} %s\n" "$1"; }
log_warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$1"; }
log_err()  { printf "${RED} [ERR]${NC} %s\n" "$1"; ERRORS=$((ERRORS + 1)); }
log_info() { printf "       %s\n" "$1"; }

# vendor_py <integration> — copies Python shared files into <integration>/src/_vendor/
vendor_py() {
  local integration="$1"
  local target_dir="${SCRIPT_DIR}/${integration}/src/_vendor"
  local py_files=("memory_capture_core.py" "pensyve_client.py")
  local header="# Vendored from integrations/shared/ — do not edit directly"

  if $VALIDATE_ONLY; then
    for f in "${py_files[@]}"; do
      validate_file "${SHARED_DIR}/${f}" "${target_dir}/${f}" "${header}"
    done
    return
  fi

  mkdir -p "${target_dir}"

  # Create __init__.py if missing
  if [ ! -f "${target_dir}/__init__.py" ]; then
    printf "# Vendored shared libraries\n" > "${target_dir}/__init__.py"
    log_info "Created ${integration}/src/_vendor/__init__.py"
  fi

  for f in "${py_files[@]}"; do
    { printf "%s\n" "${header}"; cat "${SHARED_DIR}/${f}"; } > "${target_dir}/${f}"
    log_ok "Copied ${f} -> ${integration}/src/_vendor/${f}"
  done
}

# vendor_ts <integration> <target_subdir> — copies TS shared files into <integration>/<target_subdir>/
vendor_ts() {
  local integration="$1"
  local target_subdir="$2"
  local target_dir="${SCRIPT_DIR}/${integration}/${target_subdir}"
  local ts_files=("memory-capture-core.ts" "pensyve-client.ts")
  local header="// Vendored from integrations/shared/ — do not edit directly"

  if $VALIDATE_ONLY; then
    for f in "${ts_files[@]}"; do
      validate_file "${SHARED_DIR}/${f}" "${target_dir}/${f}" "${header}"
    done
    return
  fi

  mkdir -p "${target_dir}"

  for f in "${ts_files[@]}"; do
    { printf "%s\n" "${header}"; cat "${SHARED_DIR}/${f}"; } > "${target_dir}/${f}"
    log_ok "Copied ${f} -> ${integration}/${target_subdir}/${f}"
  done
}

# validate_file <source> <vendored> <header> — checks vendored copy matches source
validate_file() {
  local source="$1"
  local vendored="$2"
  local header="$3"

  if [ ! -f "${vendored}" ]; then
    log_err "Missing vendored file: ${vendored}"
    return
  fi

  # Build expected content: header + newline + source
  local expected
  expected=$(printf "%s\n" "${header}"; cat "${source}")

  local actual
  actual=$(cat "${vendored}")

  if [ "${expected}" = "${actual}" ]; then
    log_ok "Up to date: ${vendored#"${SCRIPT_DIR}/"}"
  else
    log_err "Out of date: ${vendored#"${SCRIPT_DIR}/"}"
  fi
}

# check_no_shared_imports — ensures no integration imports directly from shared/
check_no_shared_imports() {
  local bad_imports=0

  printf '\n%s\n' "--- Self-containment check ---"

  # Python: from ..shared or from ...shared (relative imports referencing shared/)
  while IFS= read -r -d '' file; do
    case "${file}" in
      "${SHARED_DIR}"/*) continue ;;
    esac

    if grep -qE 'from\s+\.+shared' "${file}" 2>/dev/null; then
      log_err "Direct shared import in: ${file#"${SCRIPT_DIR}/"}"
      bad_imports=$((bad_imports + 1))
    fi
  done < <(find "${SCRIPT_DIR}" -name '*.py' \
    -not -path '*/node_modules/*' \
    -not -path '*/__pycache__/*' \
    -not -path '*/.ruff_cache/*' \
    -print0 2>/dev/null)

  # TypeScript: from '../shared/' or from '../../shared/' patterns
  while IFS= read -r -d '' file; do
    case "${file}" in
      "${SHARED_DIR}"/*) continue ;;
    esac

    if grep -qE "from\s+['\"]\.\.\/.*shared\/" "${file}" 2>/dev/null; then
      log_err "Direct shared import in: ${file#"${SCRIPT_DIR}/"}"
      bad_imports=$((bad_imports + 1))
    fi
  done < <(find "${SCRIPT_DIR}" \( -name '*.ts' -o -name '*.tsx' \) \
    -not -path '*/node_modules/*' \
    -print0 2>/dev/null)

  if [ "${bad_imports}" -eq 0 ]; then
    log_ok "No direct imports from shared/ found"
  fi
}

# ---------- main ----------

printf "Pensyve Integrations Build Script\n"
printf "==================================\n"

if $VALIDATE_ONLY; then
  printf "Mode: validate-only\n\n"
else
  printf "Mode: vendor (copy shared files)\n\n"
fi

# Verify shared directory exists
if [ ! -d "${SHARED_DIR}" ]; then
  log_err "Shared directory not found: ${SHARED_DIR}"
  exit 1
fi

# --- Python integrations ---
printf '%s\n' "--- Python integrations ---"
vendor_py "langchain"
vendor_py "crewai"
vendor_py "autogen"

# --- TypeScript integrations ---
printf '\n%s\n' "--- TypeScript integrations ---"
vendor_ts "vscode"      "src"
vendor_ts "langchain-ts" "src/_vendor"

# --- Self-containment check ---
check_no_shared_imports

# --- Summary ---
printf "\n==================================\n"
if [ "${ERRORS}" -gt 0 ]; then
  log_err "${ERRORS} error(s) found"
  exit 1
else
  log_ok "All good"
  exit 0
fi
