#!/usr/bin/env bash
# v2.1 release gate runner.
#
# Runs the six binding gates from the v2.1 ship spec
# (`pensyve-docs/specs/2026-05-04-pensyve-v2.1-ship.md` §8) and emits a
# single `verdict_v2_1.json` recording per-gate PASS/FAIL/NOT_RUN. The
# v2.1.0 release tag must NOT cut unless every gate reports PASS.
#
# Usage:
#   PENSYVE_DOCS_PATH=/path/to/pensyve-docs scripts/v2_1_release_gate.sh
#
# Defaults:
#   PENSYVE_DOCS_PATH=$HOME/workspace/major7apps/pensyve-docs
#   OUTPUT=$PENSYVE_DOCS_PATH/research/benchmark-sprint/v3/g0-tier-ablation/out/verdict_v2_1.json
#
# Exit codes:
#   0 — all gates PASS
#   1 — one or more gates FAILED (release blocked)
#   2 — one or more gates NOT_RUN (artifact missing, e.g. offline.json
#       absent before the interactive iptables-REJECT recipe runs)

set -uo pipefail

# Resolve the pensyve repo root from this script's location so the runner
# works from any cwd.
readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DOCS="${PENSYVE_DOCS_PATH:-${HOME}/workspace/major7apps/pensyve-docs}"
readonly G0_OUT="$DOCS/research/benchmark-sprint/v3/g0-tier-ablation/out"
readonly HARNESS_OUT="$DOCS/research/benchmark-sprint/harness/benchmarks/longmemeval/bench_v2/out/g0"
readonly OUTPUT="${RELEASE_GATE_OUTPUT:-$G0_OUT/verdict_v2_1.json}"
readonly LOG_DIR="$(mktemp -d -t v2_1_release_gate.XXXXXX)"

if [[ ! -d "$DOCS" ]]; then
    echo "FATAL: pensyve-docs not found at $DOCS" >&2
    echo "Set PENSYVE_DOCS_PATH or place pensyve-docs at the default path." >&2
    exit 2
fi

declare -A GATE_VERDICT
declare -A GATE_DETAIL

run_gate() {
    local id="$1"
    local label="$2"
    shift 2
    local log="$LOG_DIR/$id.log"
    printf '== %s: %s\n' "$id" "$label"
    if "$@" >"$log" 2>&1; then
        GATE_VERDICT[$id]="PASS"
        GATE_DETAIL[$id]="$log"
        printf '   PASS\n'
    else
        local rc=$?
        GATE_VERDICT[$id]="FAIL"
        GATE_DETAIL[$id]="$log (rc=$rc)"
        printf '   FAIL (rc=%s)  log: %s\n' "$rc" "$log"
    fi
}

mark_not_run() {
    local id="$1"
    local label="$2"
    local reason="$3"
    GATE_VERDICT[$id]="NOT_RUN"
    GATE_DETAIL[$id]="$reason"
    printf '== %s: %s\n   NOT_RUN — %s\n' "$id" "$label" "$reason"
}

# ---------------------------------------------------------------------------
# G1 — offline-proxy fail-closed validation.
#
# Recipe at `$G0_OUT/offline_proxy.PENDING_SUDO` requires interactive sudo
# (iptables -A REJECT, run a 1-Q harness, iptables -D revert). Operator
# runs the recipe out-of-band; this gate INGESTS the resulting
# `offline.json` and validates its `verdict` field.
# ---------------------------------------------------------------------------
g1_offline_proxy() {
    local offline_json="$G0_OUT/offline.json"
    if [[ ! -f "$offline_json" ]]; then
        return 64  # sentinel: not run
    fi
    python3 - <<PY "$offline_json"
import json, sys
d = json.load(open(sys.argv[1]))
verdict = d.get("verdict")
if verdict != "PASS":
    print(f"offline.json verdict={verdict!r}; expected PASS", file=sys.stderr)
    sys.exit(1)
elapsed = d.get("elapsed_s")
if not (isinstance(elapsed, (int, float)) and elapsed < 60):
    print(f"offline.json elapsed_s={elapsed!r}; expected < 60", file=sys.stderr)
    sys.exit(1)
print(f"offline.json: verdict=PASS elapsed_s={elapsed} method={d.get('method','')}")
PY
}

if [[ -f "$G0_OUT/offline.json" ]]; then
    run_gate G1 "offline-proxy fail-closed validation" g1_offline_proxy
else
    mark_not_run G1 "offline-proxy fail-closed validation" \
        "offline.json absent at $G0_OUT/offline.json — run the recipe at $G0_OUT/offline_proxy.PENDING_SUDO"
fi

# ---------------------------------------------------------------------------
# G2 — cargo build clean at 2.1.0.
# ---------------------------------------------------------------------------
g2_cargo_build() {
    cd "$REPO_ROOT"
    cargo build --workspace --exclude pensyve-python --all-features
}
run_gate G2 "cargo build --workspace at 2.1.0" g2_cargo_build

# ---------------------------------------------------------------------------
# G3 — NetworkPolicy tests pass.
# ---------------------------------------------------------------------------
g3_network_policy_tests() {
    cd "$REPO_ROOT"
    cargo test -p pensyve-core --lib --all-features network_policy:: \
        && cargo test -p pensyve-core --test network_policy_fail_closed --all-features
}
run_gate G3 "NetworkPolicy unit + integration tests" g3_network_policy_tests

# ---------------------------------------------------------------------------
# G4 — peer-card path tested under default-on.
#
# Two-fold: (a) the Rust port unit tests pass (covers the SQL ordering /
# action-map / dedupe / cap behavior); (b) the harness adapter under v2.1
# default-on env semantics produces a non-empty card on the locked SS-Pref
# subset. (b) requires a live vLLM run and is operator-driven; this script
# only enforces (a) automatically and surfaces (b) status from a marker file.
# ---------------------------------------------------------------------------
g4_peer_card_unit() {
    cd "$REPO_ROOT"
    cargo test -p pensyve-core --lib --all-features peer_card::
}
run_gate G4 "peer-card Rust port unit tests" g4_peer_card_unit

g4_peer_card_smoke() {
    local marker="$G0_OUT/v2_1_g4_peer_card.json"
    if [[ ! -f "$marker" ]]; then
        return 64
    fi
    python3 - <<PY "$marker"
import json, sys
d = json.load(open(sys.argv[1]))
verdict = d.get("verdict")
if verdict != "PASS":
    print(f"v2_1_g4_peer_card.json verdict={verdict!r}", file=sys.stderr)
    sys.exit(1)
print(f"peer-card harness smoke: verdict=PASS detail={d.get('detail','')}")
PY
}
if [[ -f "$G0_OUT/v2_1_g4_peer_card.json" ]]; then
    run_gate G4_smoke "peer-card harness smoke (default-on)" g4_peer_card_smoke
else
    mark_not_run G4_smoke "peer-card harness smoke (default-on)" \
        "v2_1_g4_peer_card.json absent — run the harness on SS-Pref-30 with default env (PENSYVE_PEER_CARD unset)"
fi

# ---------------------------------------------------------------------------
# G5 — recall_p95_ms populated in re-aggregated G0 summaries.
# ---------------------------------------------------------------------------
g5_recall_ms_populated() {
    python3 - <<PY "$HARNESS_OUT"
import json, os, sys
arms = ["1T", "2T", "3T", "5T", "2T_seed7"]
root = sys.argv[1]
missing = []
for arm in arms:
    p = os.path.join(root, arm, "summary.json")
    if not os.path.exists(p):
        missing.append(f"{arm}: summary.json missing at {p}")
        continue
    d = json.load(open(p))
    lat = d.get("latency", {})
    for key in ("recall_p50_ms", "recall_p95_ms", "recall_p99_ms", "recall_n"):
        if lat.get(key) is None:
            missing.append(f"{arm}: latency.{key} is None")
if missing:
    print("\n".join(missing), file=sys.stderr)
    sys.exit(1)
print(f"All {len(arms)} G0 summary.json files have populated latency rollups")
PY
}
run_gate G5 "recall_p95_ms populated in 5/5 G0 summaries" g5_recall_ms_populated

# ---------------------------------------------------------------------------
# G6 — harness audit clean on a v2.1 build.
#
# `audit_arm.sh` lives in the pensyve-docs harness and validates a per-arm
# directory's run.log / judge.log / eval-results / sockets per the local-
# only contract. Operator runs this against a fresh v2.1 ingest cycle out-
# of-band; the script ingests the resulting per-arm audit.json files.
# ---------------------------------------------------------------------------
g6_audit_clean() {
    python3 - <<PY "$HARNESS_OUT"
import json, os, sys
arms = ["1T", "2T", "3T", "5T", "2T_seed7"]
root = sys.argv[1]
clean = 0
issues = []
for arm in arms:
    p = os.path.join(root, arm, "audit.json")
    if not os.path.exists(p):
        issues.append(f"{arm}: audit.json missing")
        continue
    d = json.load(open(p))
    # audit_arm.sh writes `status` not `verdict`; accept either spelling.
    status = d.get("status") or d.get("verdict")
    if status == "PASS":
        clean += 1
    else:
        issues.append(f"{arm}: audit status={status!r}")
if issues:
    print("\n".join(issues), file=sys.stderr)
    sys.exit(1)
print(f"All {clean}/{len(arms)} arms passed audit_arm.sh")
PY
}
if [[ -f "$HARNESS_OUT/1T/audit.json" ]]; then
    run_gate G6 "harness audit clean on v2.1 build" g6_audit_clean
else
    mark_not_run G6 "harness audit clean on v2.1 build" \
        "audit.json files absent — run audit_arm.sh per arm against a v2.1 ingest cycle"
fi

# ---------------------------------------------------------------------------
# Aggregate verdict
# ---------------------------------------------------------------------------
mkdir -p "$(dirname "$OUTPUT")"
python3 - <<PY "$OUTPUT" "$LOG_DIR" "${!GATE_VERDICT[@]}"
import json
import os
import sys
from datetime import datetime, timezone
out = sys.argv[1]
log_dir = sys.argv[2]
gate_ids = sys.argv[3:]
verdicts = {}
for g in gate_ids:
    verdicts[g] = {
        "verdict": os.environ.get(f"V_{g}", ""),
        "detail": os.environ.get(f"D_{g}", ""),
    }
PY

# Bash builds the JSON inline since associative-array passthrough to python
# is awkward. Render the verdict block ourselves.
{
    printf '{\n'
    printf '  "v2_1_release_ready": '
    overall_pass=true
    has_not_run=false
    for id in "${!GATE_VERDICT[@]}"; do
        case "${GATE_VERDICT[$id]}" in
            FAIL) overall_pass=false ;;
            NOT_RUN) has_not_run=true; overall_pass=false ;;
        esac
    done
    if $overall_pass; then printf 'true,\n'; else printf 'false,\n'; fi
    printf '  "generated_at": "%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '  "log_dir": "%s",\n' "$LOG_DIR"
    printf '  "spec": "pensyve-docs/specs/2026-05-04-pensyve-v2.1-ship.md",\n'
    printf '  "gates": {\n'
    first=true
    for id in $(printf '%s\n' "${!GATE_VERDICT[@]}" | sort); do
        if $first; then first=false; else printf ',\n'; fi
        printf '    "%s": {"verdict": "%s", "detail": %s}' \
            "$id" "${GATE_VERDICT[$id]}" \
            "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "${GATE_DETAIL[$id]}")"
    done
    printf '\n  }\n'
    printf '}\n'
} > "$OUTPUT"

printf '\n=== verdict_v2_1.json written to %s ===\n' "$OUTPUT"
python3 -m json.tool "$OUTPUT"

# Exit code
overall_fail=false
overall_not_run=false
for id in "${!GATE_VERDICT[@]}"; do
    case "${GATE_VERDICT[$id]}" in
        FAIL) overall_fail=true ;;
        NOT_RUN) overall_not_run=true ;;
    esac
done
if $overall_fail; then
    exit 1
elif $overall_not_run; then
    exit 2
fi
exit 0
