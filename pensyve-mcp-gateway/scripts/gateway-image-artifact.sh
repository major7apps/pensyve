#!/usr/bin/env bash

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly EXPECTED_REPOSITORY="major7apps/pensyve"
readonly EXPECTED_WORKFLOW="Build & Deploy Gateway"
readonly EXPECTED_WORKFLOW_PATH=".github/workflows/deploy-gateway.yml"
readonly EXPECTED_MANIFEST_MEDIA="application/vnd.docker.distribution.manifest.v2+json"
readonly TRIVY_VERSION="0.74.0"
readonly TRIVY_IMAGE_DIGEST="sha256:55ad20f8a239a3e95427e60b8aaea38788550c18a3f1772976bebf732e6ae166"
readonly LIBP11_FLOOR="0.25.3-4ubuntu2.2"
# Docker Official Image registry 2.8.3 multi-platform index, resolved read-only 2026-08-30.
readonly LOOPBACK_REGISTRY_IMAGE="registry:2@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373"
ACTIVE_REGISTRY=""

cleanup_active_registry() {
    if [[ -n "${ACTIVE_REGISTRY}" ]]; then
        docker rm -f "${ACTIVE_REGISTRY}" >/dev/null 2>&1 || true
    fi
}
trap cleanup_active_registry EXIT

die() {
    echo "gateway image artifact error: $*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "required command is absent: $1"
}

need_arg() {
    [[ -n "${2:-}" ]] || die "missing value for $1"
}

sha256_file() {
    sha256sum "$1" | cut -d' ' -f1
}

verify_scan_common() {
    local tuple="$1" require_artifact_created="$2"
    [[ -f "${tuple}" ]] || die "tuple is absent: ${tuple}"
    python3 - "${tuple}" "${TRIVY_VERSION}" "${TRIVY_IMAGE_DIGEST}" "${LIBP11_FLOOR}" \
        "${require_artifact_created}" <<'PY'
import hashlib
import json
import re
import subprocess
import sys
from datetime import datetime
from pathlib import Path

tuple_path = Path(sys.argv[1])
expected_version, expected_digest, libp11_floor, require_artifact_created = sys.argv[2:]
data = json.loads(tuple_path.read_text())

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def field(path):
    value = data
    for part in path.split("."):
        if not isinstance(value, dict) or part not in value:
            fail(f"missing tuple field: {path}")
        value = value[part]
    return value

def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()

def parsed_time(value, name):
    try:
        return datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except ValueError:
        fail(f"invalid {name} timestamp")

if field("scanner.version") != expected_version:
    fail("scanner version mismatch")
if field("scanner.image_digest") != expected_digest:
    fail("scanner digest mismatch")

archive = field("image.archive_path")
report_path = field("scan.report_path")
expected_argv = [
    "trivy", "image", "--input", archive, "--offline-scan", "--skip-db-update",
    "--skip-check-update", "--scanners", "vuln,secret,misconfig", "--severity",
    "UNKNOWN,LOW,MEDIUM,HIGH,CRITICAL", "--exit-code", "0", "--format", "json",
    "--output", report_path,
]
if field("scanner.argv") != expected_argv:
    fail("scanner argv mismatch")

scanned = parsed_time(field("scan.scanned_at"), "scan time")
updated = parsed_time(field("scanner.db_updated_at"), "Trivy DB UpdatedAt")
downloaded = parsed_time(field("scanner.db_downloaded_at"), "Trivy DB download")
for stamp in (updated, downloaded):
    age_at_scan = (scanned - stamp).total_seconds()
    if age_at_scan < -300 or age_at_scan > 86400:
        fail("Trivy DB is stale at scan time (age must be <=24h)")
if (downloaded - updated).total_seconds() < -300:
    fail("Trivy DB download predates its UpdatedAt identity")
if require_artifact_created == "1":
    artifact_created = parsed_time(field("scan.source_artifact_created_at"), "source artifact creation")
    if scanned > artifact_created:
        fail("scan occurred after source artifact creation")
elif "source_artifact_created_at" in data.get("scan", {}):
    fail("pre-upload scan must not contain source artifact creation")
db_path = Path(field("scanner.db_path"))
if not db_path.is_file() or digest(db_path) != field("scanner.db_sha256"):
    fail("Trivy DB hash mismatch")
db_oci_digest = field("scanner.db_oci_digest")
if db_oci_digest is not None and not re.fullmatch(r"sha256:[0-9a-f]{64}", str(db_oci_digest)):
    fail("Trivy DB OCI digest is invalid")

report_file = Path(report_path)
if not report_file.is_file() or digest(report_file) != field("scan.report_sha256"):
    fail("scan report hash mismatch")
if field("scan.archive_sha256") != field("image.archive_sha256") or field("scan.config_id") != field("image.config_id"):
    fail("scan subject does not match archive/config")
policy_path = Path(field("scan.policy_path"))
if field("scan.policy_version") != "1" or not policy_path.is_file() or digest(policy_path) != field("scan.policy_sha256"):
    fail("scan policy identity mismatch")
if field("scan.policy_result") != "pass":
    fail("scan policy result is not pass")

try:
    report = json.loads(report_file.read_text())
except Exception:
    fail("scan report JSON is invalid")
if report.get("Metadata", {}).get("ImageID") != field("image.config_id"):
    fail("scan subject ImageID mismatch")
results = report.get("Results")
if not isinstance(results, list) or not results:
    fail("scan report is suppressed or has no results")
if any(report.get(name) for name in ("Suppressions", "IgnoredFindings", "Exceptions")):
    fail("scan report contains suppression metadata")

packages = []
vulnerabilities = []
secrets = []
misconfigurations = []
for result in results:
    if not isinstance(result, dict):
        continue
    packages.extend(result.get("Packages") or [])
    vulnerabilities.extend(result.get("Vulnerabilities") or [])
    secrets.extend(result.get("Secrets") or [])
    misconfigurations.extend(result.get("Misconfigurations") or [])
libp11_versions = []
for package in packages:
    if package.get("Name") != "libp11-kit0":
        continue
    version = str(package.get("Version", ""))
    release = str(package.get("Release", ""))
    libp11_versions.append(f"{version}-{release}" if release else version)
if not libp11_versions:
    fail("libp11-kit0 package evidence is absent")
for version in libp11_versions:
    result = subprocess.run(["dpkg", "--compare-versions", version, "ge", libp11_floor])
    if result.returncode != 0:
        fail(f"libp11-kit0 {version} is below {libp11_floor}")

for finding in vulnerabilities:
    cve = str(finding.get("VulnerabilityID", ""))
    severity = str(finding.get("Severity", "UNKNOWN")).upper()
    if cve in {"CVE-2026-13757", "CVE-2026-18938"}:
        fail(f"named vulnerability is present: {cve}")
    if severity in {"HIGH", "CRITICAL"}:
        fail(f"{severity} vulnerability is present: {cve}")
if secrets:
    fail("secret finding is present")
for finding in misconfigurations:
    if str(finding.get("Severity", "UNKNOWN")).upper() in {"HIGH", "CRITICAL"}:
        fail("High/Critical misconfiguration is present")

print("Trivy scan evidence and deterministic policy verified")
PY
}

verify_scan_preupload() {
    verify_scan_common "$1" 0
}

verify_scan_postupload() {
    verify_scan_common "$1" 1
}

verify_local() {
    local tuple="$1"
    [[ -f "${tuple}" ]] || die "tuple is absent: ${tuple}"
    python3 - "${tuple}" "${EXPECTED_REPOSITORY}" "${EXPECTED_WORKFLOW}" \
        "${EXPECTED_WORKFLOW_PATH}" "${EXPECTED_MANIFEST_MEDIA}" <<'PY'
import hashlib
import json
import math
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

tuple_path = Path(sys.argv[1])
expected_repository, expected_workflow, expected_path, expected_media = sys.argv[2:]

try:
    data = json.loads(tuple_path.read_text())
except Exception as exc:
    print(f"invalid tuple JSON: {exc}", file=sys.stderr)
    raise SystemExit(1)

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

def finite_number(value):
    return type(value) in (int, float) and math.isfinite(value)

def field(path):
    value = data
    for part in path.split("."):
        if not isinstance(value, dict) or part not in value:
            fail(f"missing tuple field: {path}")
        value = value[part]
    return value

def parse_time(value, name):
    try:
        return datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except ValueError:
        fail(f"invalid {name} timestamp")

def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()

if not exact_int(field("schema_version")) or field("schema_version") != 1:
    fail("tuple schema version is invalid")

sha = field("source.head_sha")
if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
    fail("source must be a real 40-hex PR head, never a merge ref")
if field("source.repository") != expected_repository:
    fail("source repository mismatch")
if field("source.workflow") != expected_workflow:
    fail("source workflow mismatch")
if field("source.workflow_path") != expected_path:
    fail("source workflow path mismatch")
if field("source.mode") != "artifact-build" or field("source.event") != "workflow_dispatch":
    fail("source event/mode mismatch")
if not str(field("source.ref")).startswith("refs/heads/"):
    fail("source ref must be a real branch ref")
if not exact_int(field("source.run_id")) or field("source.run_id") <= 0:
    fail("source run id must be positive")
if not exact_int(field("source.run_attempt")) or field("source.run_attempt") <= 0:
    fail("source run attempt must be positive")

if not exact_int(field("pull_request.number")) or field("pull_request.number") <= 0:
    fail("pull request number is required")
if field("pull_request.repository") != expected_repository:
    fail("pull request repository mismatch")
if field("pull_request.state") != "open":
    fail("pull request must remain open")
if field("pull_request.draft") is not True:
    fail("pull request must remain draft")
if field("pull_request.base_ref") != "main":
    fail("pull request base must be main")
if field("pull_request.head_repository") != expected_repository:
    fail("pull request head repository mismatch")
if field("pull_request.head_ref") != field("source.ref").removeprefix("refs/heads/"):
    fail("pull request head ref mismatch")
if field("pull_request.head_sha") != sha:
    fail("pull request head SHA drift")

run_id = field("source.run_id")
attempt = field("source.run_attempt")
expected_name = f"gateway-image-{run_id}-{attempt}-{sha}"
if field("artifact.name") != expected_name:
    fail("artifact name does not bind run, attempt, and source")
if field("artifact.repository") != expected_repository:
    fail("artifact repository mismatch")
if (not exact_int(field("artifact.run_id")) or not exact_int(field("artifact.run_attempt")) or
        field("artifact.run_id") != run_id or field("artifact.run_attempt") != attempt):
    fail("artifact run association mismatch")
if field("artifact.status") != "completed" or field("artifact.conclusion") != "success":
    fail("artifact source run is not completed successfully")
if not exact_int(field("artifact.id")) or field("artifact.id") <= 0:
    fail("artifact id must be positive")
if not exact_int(field("artifact.size_in_bytes")) or field("artifact.size_in_bytes") <= 0:
    fail("artifact REST size is invalid")
created = parse_time(field("artifact.created_at"), "artifact created_at")
expires = parse_time(field("artifact.expires_at"), "artifact expires_at")
retention = (expires - created).total_seconds()
if retention < 29 * 86400 or retention > 31 * 86400:
    fail("artifact retention must be 30 days")
if expires <= datetime.now(timezone.utc):
    fail("artifact has expired")
if not exact_int(field("artifact.retention_days")) or field("artifact.retention_days") != 30:
    fail("artifact retention_days mismatch")
if not re.fullmatch(r"sha256:[0-9a-f]{64}", str(field("artifact.server_digest"))):
    fail("artifact server digest is invalid")
if field("storage.rest_size_in_bytes") != field("artifact.size_in_bytes"):
    fail("artifact REST size does not match storage reconciliation")
if field("storage.rest_created_at") != field("artifact.created_at"):
    fail("artifact REST created_at does not match storage reconciliation")
if field("storage.rest_expires_at") != field("artifact.expires_at"):
    fail("artifact REST expires_at does not match storage reconciliation")
if field("cleanup_required") is not False:
    fail("accepted tuple must record cleanup_required=false")

snapshot = parse_time(field("storage.snapshot_at"), "billing snapshot")
snapshot_age_at_creation = (created - snapshot).total_seconds()
if snapshot_age_at_creation < -300:
    fail("billing snapshot is after source artifact creation")
if snapshot_age_at_creation > 86400:
    fail("billing snapshot was stale at source artifact creation")
if not finite_number(field("storage.price_per_gb_month")) or field("storage.price_per_gb_month") <= 0:
    fail("missing or invalid storage price_per_gb_month")
for name in (
    "current_billable_bytes", "archive_bytes", "evidence_bytes", "container_overhead_bytes",
    "handoff_overhead_bytes", "runner_available_bytes", "organization_actions_artifact_bytes",
    "organization_packages_bytes", "projected_content_bytes", "actual_artifact_bytes",
    "actual_total_billable_bytes", "rest_size_in_bytes", "retained_source_artifact_bytes",
):
    if not exact_int(field(f"storage.{name}")) or field(f"storage.{name}") < 0:
        fail(f"storage {name.replace('_', ' ')} must be a non-negative integer")
for name in (
    "projected_gb_hours", "projected_dollars", "computed_projected_gb_hours",
    "computed_projected_dollars", "actual_gb_hours", "actual_dollars",
):
    if not finite_number(field(f"storage.{name}")) or field(f"storage.{name}") < 0:
        fail(f"storage {name.replace('_', ' ')} must be a finite non-negative number")
for name in ("approved_gb_hours_ceiling", "approved_dollar_ceiling"):
    if not finite_number(field(f"storage.{name}")) or field(f"storage.{name}") <= 0:
        fail(f"missing or invalid storage {name}")
if field("storage.billing_unit") != "GB-month" or field("storage.payment_status") != "active" or field("storage.spending_status") != "within-limit":
    fail("storage billing/payment/spending approval mismatch")
if field("storage.current_billable_bytes") != field("storage.organization_actions_artifact_bytes") + field("storage.organization_packages_bytes"):
    fail("storage organization usage does not reconcile")
if field("storage.snapshot_inclusion_mode") != "source-excluded":
    fail("accepted source tuple must use the source-excluded storage snapshot")
if field("storage.retained_source_artifact_id") is not None or field("storage.retained_source_artifact_bytes") != 0:
    fail("source-excluded storage must not count a retained source artifact")
projected_content = sum(field(f"storage.{name}") for name in (
    "archive_bytes", "evidence_bytes", "container_overhead_bytes", "handoff_overhead_bytes",
))
if field("storage.projected_content_bytes") != projected_content:
    fail("projected content bytes do not reconcile sealed archive and overhead")
computed_projected_gb_hours = (field("storage.current_billable_bytes") + projected_content) / 1_000_000_000 * 24 * 30
computed_projected_dollars = computed_projected_gb_hours / (24 * 30) * field("storage.price_per_gb_month")
if abs(field("storage.computed_projected_gb_hours") - computed_projected_gb_hours) > 1e-12:
    fail("computed projected GB-hours do not reconcile authority bytes")
if abs(field("storage.computed_projected_dollars") - computed_projected_dollars) > 1e-12:
    fail("computed projected dollars do not reconcile authority bytes")
if field("storage.projected_gb_hours") + 1e-12 < computed_projected_gb_hours:
    fail("declared projected GB-hours understate authority bytes")
if field("storage.projected_dollars") + 1e-12 < computed_projected_dollars:
    fail("declared projected dollars understate authority bytes")
if field("storage.projected_gb_hours") > field("storage.approved_gb_hours_ceiling"):
    fail("projected GB-hour ceiling exceeded")
if field("storage.projected_dollars") > field("storage.approved_dollar_ceiling"):
    fail("projected dollar ceiling exceeded")
if field("storage.actual_gb_hours") > field("storage.approved_gb_hours_ceiling"):
    fail("actual GB-hour ceiling exceeded")
if field("storage.actual_dollars") > field("storage.approved_dollar_ceiling"):
    fail("actual dollar ceiling exceeded")
actual_total = field("storage.current_billable_bytes") + field("artifact.size_in_bytes")
if field("storage.actual_artifact_bytes") != field("artifact.size_in_bytes"):
    fail("actual artifact bytes do not match REST size")
if field("storage.actual_total_billable_bytes") != actual_total:
    fail("actual total billable bytes must include current organization usage plus uploaded artifact")
actual_gb_hours = actual_total / 1_000_000_000 * (retention / 3600)
actual_dollars = actual_gb_hours / (24 * 30) * field("storage.price_per_gb_month")
if abs(field("storage.actual_gb_hours") - actual_gb_hours) > 1e-12:
    fail("actual GB-hours do not reconcile current organization usage plus uploaded artifact")
if abs(field("storage.actual_dollars") - actual_dollars) > 1e-12:
    fail("actual dollars do not reconcile current organization usage plus uploaded artifact")
if field("storage.status") != "accepted":
    fail("accepted storage reconciliation status mismatch")

archive = Path(field("image.archive_path"))
if not archive.is_file() or digest(archive) != field("image.archive_sha256"):
    fail("archive checksum mismatch")
config = Path(field("image.config_path"))
if not config.is_file():
    fail("image config is absent")
config_digest = digest(config)
if field("image.config_id") != f"sha256:{config_digest}":
    fail("image config digest mismatch")
try:
    config_data = json.loads(config.read_text())
except Exception:
    fail("image config JSON is invalid")
if config_data.get("architecture") != "arm64" or config_data.get("os") != "linux" or field("image.platform") != "linux/arm64":
    fail("image platform mismatch")
runtime = config_data.get("config", {})
if runtime.get("Labels", {}).get("org.opencontainers.image.revision") != sha or field("image.source_label") != sha:
    fail("image source label mismatch")
if runtime.get("StopSignal") != "SIGINT":
    fail("image default stop signal must be SIGINT")
if not str(runtime.get("User", "")).startswith("1001"):
    fail("image runtime user must be 1001")

manifest = Path(field("image.raw_manifest_path"))
if not manifest.is_file():
    fail("raw manifest is absent")
manifest_sha = digest(manifest)
if manifest_sha != field("image.raw_manifest_sha256"):
    fail("raw manifest hash mismatch")
if field("image.raw_manifest_media_type") != expected_media:
    fail("raw manifest media type mismatch")
if field("image.pushed_digest") != f"sha256:{manifest_sha}":
    fail("raw manifest pushed digest mismatch")
try:
    manifest_data = json.loads(manifest.read_text())
except Exception:
    fail("raw manifest JSON is invalid")
if manifest_data.get("mediaType") != expected_media:
    fail("raw manifest media type does not match bytes")
if manifest_data.get("config", {}).get("digest") != field("image.config_id"):
    fail("raw manifest config digest mismatch")
for name in ("compressed_layer_bytes", "uncompressed_image_bytes"):
    if not exact_int(field(f"image.{name}")) or field(f"image.{name}") <= 0:
        fail(f"image {name.replace('_', ' ')} must be a positive integer")

expected_gate_values = {
    "bundle": "pass", "gte": "pass", "bge": "pass", "default_stop": "pass",
    "missing_model": "pass", "five_cgroups": "pass", "no_egress": True,
    "read_only_root": True, "embedding_pool_size": 1, "source_contract": "pass",
}
for name, expected in expected_gate_values.items():
    actual = field(f"gates.{name}")
    if name == "embedding_pool_size" and not exact_int(actual):
        fail(f"release gate did not pass: {name}")
    if name in ("no_egress", "read_only_root") and actual is not True:
        fail(f"release gate did not pass: {name}")
    if actual != expected:
        fail(f"release gate did not pass: {name}")
sizing_summary = Path(field("gates.sizing_summary_path"))
if not sizing_summary.is_file() or digest(sizing_summary) != field("gates.sizing_summary_sha256"):
    fail("release sizing summary hash mismatch")
source_gate_evidence = Path(field("gates.source_gate_evidence_path"))
if not source_gate_evidence.is_file() or digest(source_gate_evidence) != field("gates.source_gate_evidence_sha256"):
    fail("exact-head source-gate evidence hash mismatch")

print("local artifact tuple verified")
PY
    verify_scan_postupload "${tuple}" >/dev/null
}

storage_precheck() {
    local input="$1" output="$2" replay_reference="${3:-}"
    python3 - "${input}" "${output}" "${replay_reference}" <<'PY'
import json
import math
import sys
from datetime import datetime, timezone
from pathlib import Path

source, destination = map(Path, sys.argv[1:3])
replay_reference_raw = sys.argv[3]
data = json.loads(source.read_text())

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

def finite_number(value):
    return type(value) in (int, float) and math.isfinite(value)

required = (
    "snapshot_at", "approved_gb_hours_ceiling", "approved_dollar_ceiling",
    "price_per_gb_month", "current_billable_bytes", "archive_bytes",
    "evidence_bytes", "container_overhead_bytes", "handoff_overhead_bytes",
    "runner_available_bytes", "organization_actions_artifact_bytes",
    "organization_packages_bytes", "billing_unit", "payment_status", "spending_status",
    "snapshot_inclusion_mode", "retained_source_artifact_id", "retained_source_artifact_bytes",
)
for name in required:
    if name not in data:
        fail(f"missing storage input: {name}")
try:
    snapshot = datetime.fromisoformat(str(data["snapshot_at"]).replace("Z", "+00:00"))
    if snapshot.tzinfo is None:
        raise ValueError
except (TypeError, ValueError):
    fail("billing snapshot timestamp is invalid")
mode = data["snapshot_inclusion_mode"]
if mode not in ("source-excluded", "source-included"):
    fail("storage snapshot inclusion mode is invalid")
if replay_reference_raw:
    if mode != "source-excluded":
        fail("replay reference is only valid for source-excluded storage")
    try:
        replay_reference = datetime.fromisoformat(replay_reference_raw.replace("Z", "+00:00"))
        if replay_reference.tzinfo is None:
            raise ValueError
    except (TypeError, ValueError):
        fail("replay reference timestamp is invalid")
    age = (replay_reference - snapshot).total_seconds()
    if age < -300:
        fail("billing snapshot is after replay reference")
    if age > 86400:
        fail("billing snapshot was stale at replay reference")
else:
    age = (datetime.now(timezone.utc) - snapshot).total_seconds()
    if age < -300 or age > 86400:
        fail("billing snapshot is stale")
if not finite_number(data["price_per_gb_month"]) or data["price_per_gb_month"] <= 0:
    fail("price per gb month must be a positive finite number")
for name in ("current_billable_bytes", "archive_bytes", "evidence_bytes", "container_overhead_bytes", "handoff_overhead_bytes", "runner_available_bytes"):
    if not exact_int(data[name]) or data[name] < 0:
        fail(f"{name.replace('_', ' ')} must be a non-negative integer")
if data["billing_unit"] != "GB-month" or data["payment_status"] != "active" or data["spending_status"] != "within-limit":
    fail("billing unit, payment status, or spending status is not approved")
for name in ("organization_actions_artifact_bytes", "organization_packages_bytes"):
    if not exact_int(data[name]) or data[name] < 0:
        fail(f"{name.replace('_', ' ')} must be a non-negative integer")
for name, label in (("projected_gb_hours", "GB-hours"), ("projected_dollars", "dollars")):
    if name in data and (not finite_number(data[name]) or data[name] < 0):
        fail(f"declared projected {label} must be a finite non-negative number")
for name in ("approved_gb_hours_ceiling", "approved_dollar_ceiling"):
    if not finite_number(data[name]) or data[name] <= 0:
        fail(f"{name.replace('_', ' ')} must be a positive finite number")
if data["current_billable_bytes"] != data["organization_actions_artifact_bytes"] + data["organization_packages_bytes"]:
    fail("current billable bytes do not reconcile organization Actions artifacts and Packages")
retained_id = data["retained_source_artifact_id"]
retained_bytes = data["retained_source_artifact_bytes"]
if mode == "source-excluded":
    if retained_id is not None or not exact_int(retained_bytes) or retained_bytes != 0:
        fail("source-excluded snapshot must add the source exactly once after upload")
elif not exact_int(retained_id) or retained_id <= 0 or not exact_int(retained_bytes) or retained_bytes <= 0:
    fail("source-included snapshot must identify retained source bytes")
elif data["organization_actions_artifact_bytes"] < retained_bytes or data["current_billable_bytes"] < retained_bytes:
    fail("source-included snapshot omits retained source bytes")
if mode == "source-included":
    source_snapshot_raw = data.get("source_snapshot_at")
    if not source_snapshot_raw:
        fail("source-included snapshot is missing the immutable source snapshot")
    try:
        source_snapshot = datetime.fromisoformat(str(source_snapshot_raw).replace("Z", "+00:00"))
    except ValueError:
        fail("source snapshot timestamp is invalid")
    if source_snapshot >= snapshot:
        fail("source-inclusive snapshot was not refreshed after the immutable source snapshot")
payload = sum(data[name] for name in ("archive_bytes", "evidence_bytes", "container_overhead_bytes", "handoff_overhead_bytes"))
if data["runner_available_bytes"] < payload:
    fail("runner disk is insufficient for sealed archive and overhead")
projected_bytes = data["current_billable_bytes"] + payload
projected_gb_hours = projected_bytes / 1_000_000_000 * 24 * 30
projected_dollars = projected_gb_hours / (24 * 30) * data["price_per_gb_month"]
declared_gb_hours = data.get("projected_gb_hours", projected_gb_hours)
declared_dollars = data.get("projected_dollars", projected_dollars)
if not finite_number(projected_gb_hours) or not finite_number(projected_dollars):
    fail("computed projected storage authority is not finite")
if not finite_number(declared_gb_hours) or declared_gb_hours < 0:
    fail("declared projected GB-hours must be a finite non-negative number")
if not finite_number(declared_dollars) or declared_dollars < 0:
    fail("declared projected dollars must be a finite non-negative number")
if declared_gb_hours + 1e-12 < projected_gb_hours:
    fail("declared projected GB-hours understate actual sealed bytes")
if declared_dollars + 1e-12 < projected_dollars:
    fail("declared projected dollars understate actual sealed bytes")
if declared_gb_hours > data["approved_gb_hours_ceiling"]:
    fail("GB-hour ceiling exceeded")
if declared_dollars > data["approved_dollar_ceiling"]:
    fail("dollar ceiling exceeded")
data["projected_content_bytes"] = payload
data["projected_gb_hours"] = declared_gb_hours
data["projected_dollars"] = declared_dollars
data["computed_projected_gb_hours"] = projected_gb_hours
data["computed_projected_dollars"] = projected_dollars
data["cleanup_required"] = False
destination.write_text(json.dumps(data, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
PY
}

storage_reconcile() {
    local input="$1" output="$2"
    python3 - "${input}" "${output}" <<'PY'
import json
import math
import sys
from datetime import datetime
from pathlib import Path

source, destination = map(Path, sys.argv[1:])
data = json.loads(source.read_text())

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

def finite_number(value):
    return type(value) in (int, float) and math.isfinite(value)

for name in (
    "rest_size_in_bytes", "created_at", "expires_at", "approved_gb_hours_ceiling",
    "approved_dollar_ceiling", "price_per_gb_month", "current_billable_bytes",
    "organization_actions_artifact_bytes", "organization_packages_bytes",
    "snapshot_inclusion_mode", "retained_source_artifact_id", "retained_source_artifact_bytes",
):
    if name not in data:
        fail(f"missing reconciliation input: {name}")
if not exact_int(data["rest_size_in_bytes"]) or data["rest_size_in_bytes"] <= 0:
    fail("REST artifact size is invalid")
for name in ("current_billable_bytes", "organization_actions_artifact_bytes", "organization_packages_bytes"):
    if not exact_int(data[name]) or data[name] < 0:
        fail(f"{name.replace('_', ' ')} must be a non-negative integer")
if data["current_billable_bytes"] != data["organization_actions_artifact_bytes"] + data["organization_packages_bytes"]:
    fail("current billable bytes do not reconcile organization Actions artifacts and Packages")
mode = data["snapshot_inclusion_mode"]
if mode == "source-excluded":
    if (data["retained_source_artifact_id"] is not None or
            not exact_int(data["retained_source_artifact_bytes"]) or data["retained_source_artifact_bytes"] != 0):
        fail("source-excluded reconciliation double-counts the source artifact")
elif mode == "source-included":
    if not exact_int(data["retained_source_artifact_id"]) or data["retained_source_artifact_id"] <= 0:
        fail("source-included reconciliation is missing the retained source identity")
    if not exact_int(data["retained_source_artifact_bytes"]) or data["retained_source_artifact_bytes"] <= 0:
        fail("source-included reconciliation is missing retained source bytes")
    if data["organization_actions_artifact_bytes"] < data["retained_source_artifact_bytes"]:
        fail("source-included reconciliation omits retained source bytes")
else:
    fail("storage snapshot inclusion mode is invalid")
if not finite_number(data["price_per_gb_month"]) or data["price_per_gb_month"] <= 0:
    fail("price per gb month must be a positive finite number")
for name in ("approved_gb_hours_ceiling", "approved_dollar_ceiling"):
    if not finite_number(data[name]) or data[name] <= 0:
        fail(f"{name.replace('_', ' ')} must be a positive finite number")
projection_fields = {
    "archive_bytes", "evidence_bytes", "container_overhead_bytes", "handoff_overhead_bytes",
    "runner_available_bytes", "projected_content_bytes", "projected_gb_hours", "projected_dollars",
    "computed_projected_gb_hours", "computed_projected_dollars",
}
present_projection_fields = projection_fields.intersection(data)
if present_projection_fields and present_projection_fields != projection_fields:
    fail("reconciliation projection authority is incomplete")
if present_projection_fields:
    for name in (
        "archive_bytes", "evidence_bytes", "container_overhead_bytes", "handoff_overhead_bytes",
        "runner_available_bytes", "projected_content_bytes",
    ):
        if not exact_int(data[name]) or data[name] < 0:
            fail(f"{name.replace('_', ' ')} must be a non-negative integer")
    for name in ("projected_gb_hours", "projected_dollars", "computed_projected_gb_hours", "computed_projected_dollars"):
        if not finite_number(data[name]) or data[name] < 0:
            fail(f"{name.replace('_', ' ')} must be a finite non-negative number")
    projected_content = sum(data[name] for name in (
        "archive_bytes", "evidence_bytes", "container_overhead_bytes", "handoff_overhead_bytes",
    ))
    if data["projected_content_bytes"] != projected_content:
        fail("projected content bytes do not reconcile sealed archive and overhead")
    computed_projected_gb_hours = (data["current_billable_bytes"] + projected_content) / 1_000_000_000 * 24 * 30
    computed_projected_dollars = computed_projected_gb_hours / (24 * 30) * data["price_per_gb_month"]
    if (not finite_number(computed_projected_gb_hours) or not finite_number(computed_projected_dollars) or
            abs(data["computed_projected_gb_hours"] - computed_projected_gb_hours) > 1e-12 or
            abs(data["computed_projected_dollars"] - computed_projected_dollars) > 1e-12):
        fail("computed projected storage authority does not reconcile")
    if (data["projected_gb_hours"] + 1e-12 < computed_projected_gb_hours or
            data["projected_dollars"] + 1e-12 < computed_projected_dollars):
        fail("declared projected storage authority understates sealed bytes")
created = datetime.fromisoformat(str(data["created_at"]).replace("Z", "+00:00"))
expires = datetime.fromisoformat(str(data["expires_at"]).replace("Z", "+00:00"))
retention_hours = (expires - created).total_seconds() / 3600
if retention_hours < 29 * 24 or retention_hours > 31 * 24:
    fail("REST artifact retention is not 30 days")
actual_total = data["current_billable_bytes"] + data["rest_size_in_bytes"]
actual_gb_hours = actual_total / 1_000_000_000 * retention_hours
actual_dollars = actual_gb_hours / (24 * 30) * data["price_per_gb_month"]
if not finite_number(actual_gb_hours) or not finite_number(actual_dollars):
    fail("computed actual storage authority is not finite")
cleanup = actual_gb_hours > data["approved_gb_hours_ceiling"] or actual_dollars > data["approved_dollar_ceiling"]
data["actual_gb_hours"] = actual_gb_hours
data["actual_dollars"] = actual_dollars
data["actual_artifact_bytes"] = data["rest_size_in_bytes"]
data["actual_total_billable_bytes"] = actual_total
data["rest_created_at"] = data["created_at"]
data["rest_expires_at"] = data["expires_at"]
data["cleanup_required"] = cleanup
data["status"] = "over-ceiling" if cleanup else "accepted"
destination.write_text(json.dumps(data, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
PY
}

disk_precheck() {
    local input="$1" output="$2"
    python3 - "${input}" "${output}" <<'PY'
import json
import sys
from collections import defaultdict
from pathlib import Path

source, destination = map(Path, sys.argv[1:])
data = json.loads(source.read_text())

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

entries = data.get("filesystems")
expected_names = {"workspace", "cargo", "model_scratch", "docker", "tmp"}
if not isinstance(entries, list) or {entry.get("name") for entry in entries if isinstance(entry, dict)} != expected_names:
    fail("disk precheck must cover workspace, Cargo, model scratch, Docker root, and /tmp")
required_bytes_by_device = defaultdict(int)
available_by_device = {}
for entry in entries:
    path = entry.get("path")
    device = entry.get("device")
    available = entry.get("available_bytes")
    required = entry.get("required_bytes")
    if not isinstance(path, str) or not path.startswith("/") or not Path(path).exists():
        fail("disk precheck path is not an existing absolute path")
    if not isinstance(device, str) or not device or not exact_int(available) or available < 0:
        fail("disk precheck filesystem identity/availability is invalid")
    if not exact_int(required) or required <= 0:
        fail("disk precheck conservative peak demand is invalid")
    if device not in available_by_device or available < available_by_device[device]:
        available_by_device[device] = available
    required_bytes_by_device[device] += required
for device, required in required_bytes_by_device.items():
    if available_by_device[device] < required:
        fail(f"runner disk is insufficient on filesystem {device}")
data["required_bytes_by_device"] = dict(sorted(required_bytes_by_device.items()))
data["available_bytes_by_device"] = dict(sorted(available_by_device.items()))
data["status"] = "pass"
destination.write_text(json.dumps(data, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
PY
}

build_archive() {
    local source_sha="$1" archive="$2" evidence_dir="$3" image_ref="$4"
    need git; need cargo; need docker; need curl; need jq; need sha256sum
    [[ "${source_sha}" =~ ^[0-9a-f]{40}$ ]] || die "source SHA must be a real lowercase 40-hex commit"
    [[ "$(uname -m)" == "aarch64" || "$(uname -m)" == "arm64" ]] || die "release build requires a native ARM64 runner"
    [[ "$(git -C "${REPO_ROOT}" rev-parse HEAD)" == "${source_sha}" ]] || die "source SHA does not equal clean checkout HEAD"
    [[ -z "$(git -C "${REPO_ROOT}" status --porcelain)" ]] || die "release build requires a clean checkout"
    [[ "${image_ref}" == *":${source_sha}" ]] || die "image tag must be the exact source SHA"
    mkdir -p -- "${evidence_dir}"
    [[ ! -e "${evidence_dir}/build.completed" ]] || die "second build is forbidden"
    [[ ! -e "${archive}" ]] || die "archive target already exists; second export is forbidden"

    cargo build --locked --release -p pensyve-mcp-gateway
    docker buildx build --load --platform linux/arm64 \
        --build-arg "SOURCE_SHA=${source_sha}" --tag "${image_ref}" \
        --file "${REPO_ROOT}/pensyve-mcp-gateway/Dockerfile" "${REPO_ROOT}"
    docker save --output "${archive}" "${image_ref}"

    docker image inspect "${image_ref}" > "${evidence_dir}/image-inspect.json"
    local image_id architecture label user stop_signal archive_sha config_path config_member config_sha uncompressed_size
    image_id="$(docker image inspect --format '{{.Id}}' "${image_ref}")"
    architecture="$(docker image inspect --format '{{.Architecture}}' "${image_ref}")"
    label="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "${image_ref}")"
    user="$(docker image inspect --format '{{.Config.User}}' "${image_ref}")"
    stop_signal="$(docker image inspect --format '{{.Config.StopSignal}}' "${image_ref}")"
    uncompressed_size="$(docker image inspect --format '{{.Size}}' "${image_ref}")"
    [[ "${architecture}" == "arm64" && "${label}" == "${source_sha}" && "${user}" == 1001* && "${stop_signal}" == "SIGINT" ]] \
        || die "built image identity, platform, user, or default stop signal mismatch"
    archive_sha="$(sha256_file "${archive}")"

    local registry_name registry_id registry_port loopback_ref headers raw_manifest pushed_digest media_type
    registry_name="pensyve-artifact-$$_${RANDOM}"
    registry_id=""
    registry_id="$(docker run -d --name "${registry_name}" -p 127.0.0.1::5000 "${LOOPBACK_REGISTRY_IMAGE}")"
    ACTIVE_REGISTRY="${registry_id}"
    registry_port="$(docker port "${registry_id}" 5000/tcp | awk -F: 'NR==1 {print $NF}')"
    [[ "${registry_port}" =~ ^[0-9]+$ ]] || die "failed to resolve loopback registry port"
    loopback_ref="127.0.0.1:${registry_port}/pensyve-gateway:${source_sha}"
    docker tag "${image_ref}" "${loopback_ref}"
    docker push "${loopback_ref}" > "${evidence_dir}/loopback-push.log"
    headers="${evidence_dir}/raw-manifest.headers"
    raw_manifest="${evidence_dir}/raw-manifest.json"
    curl --fail --silent --show-error --dump-header "${headers}" \
        --header "Accept: ${EXPECTED_MANIFEST_MEDIA}" \
        "http://127.0.0.1:${registry_port}/v2/pensyve-gateway/manifests/${source_sha}" \
        --output "${raw_manifest}"
    pushed_digest="$(awk 'BEGIN{IGNORECASE=1} /^Docker-Content-Digest:/ {gsub("\\r", "", $2); print $2}' "${headers}")"
    media_type="$(awk 'BEGIN{IGNORECASE=1} /^Content-Type:/ {gsub("\\r", "", $2); print $2}' "${headers}")"
    [[ "${media_type}" == "${EXPECTED_MANIFEST_MEDIA}" ]] || die "loopback raw manifest media type mismatch"
    [[ "${pushed_digest}" == "sha256:$(sha256_file "${raw_manifest}")" ]] || die "loopback raw manifest digest mismatch"
    [[ "$(jq -r '.config.digest' "${raw_manifest}")" == "${image_id}" ]] || die "loopback manifest config digest mismatch"

    config_path="${evidence_dir}/${image_id#sha256:}.json"
    config_member="blobs/sha256/${image_id#sha256:}"
    tar -tf "${archive}" | grep -Fx -- "${config_member}" >/dev/null \
        || die "OCI-layout archive config blob is absent"
    tar -xOf "${archive}" "${config_member}" > "${config_path}"
    config_sha="$(sha256_file "${config_path}")"
    [[ "sha256:${config_sha}" == "${image_id}" ]] || die "exported config does not match image ID"

    jq -n --arg source_sha "${source_sha}" --arg archive "${archive}" \
        --arg archive_sha "${archive_sha}" --arg config "${config_path}" \
        --arg config_id "${image_id}" --arg raw_manifest "${raw_manifest}" \
        --arg manifest_sha "$(sha256_file "${raw_manifest}")" --arg media "${media_type}" \
        --arg pushed_digest "${pushed_digest}" --argjson uncompressed_size "${uncompressed_size}" \
        --argjson compressed_size "$(jq '[.layers[].size] | add' "${raw_manifest}")" \
        '{archive_path:$archive,archive_sha256:$archive_sha,
          config_path:$config,config_id:$config_id,platform:"linux/arm64",source_label:$source_sha,
          raw_manifest_path:$raw_manifest,raw_manifest_sha256:$manifest_sha,
          raw_manifest_media_type:$media,pushed_digest:$pushed_digest,
          compressed_layer_bytes:$compressed_size,uncompressed_image_bytes:$uncompressed_size}' \
        > "${evidence_dir}/build-result.json"
    sha256sum "${archive}" "${config_path}" "${raw_manifest}" "${evidence_dir}/build-result.json" \
        > "${evidence_dir}/sealed-files.sha256"
    printf '%s\n' "${source_sha}" > "${evidence_dir}/build.completed"
    docker rm -f "${registry_id}" >/dev/null
    ACTIVE_REGISTRY=""
}

materialize_model_links() {
    local root
    root="$(realpath "$1")"
    [[ -d "${root}" ]] || die "materialize-model-links root is absent: ${root}"
    python3 - "${root}" <<'PY'
import hashlib
import os
import re
import shutil
import stat
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve(strict=True)
links = []
records = []
parents = {}
blob_descriptors = []
directory_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
def walk_error(error):
    raise error

for current, directories, files in os.walk(root, topdown=True, followlinks=False, onerror=walk_error):
    for name in directories + files:
        path = Path(current, name)
        if stat.S_ISLNK(path.lstat().st_mode):
            links.append(path)

try:
    # Validate the complete link set before changing permissions or contents.
    for link in sorted(links):
        relative = link.relative_to(root)
        parts = relative.parts
        model_indexes = [
            index for index, part in enumerate(parts[:-1])
            if re.fullmatch(r"models--[A-Za-z0-9._-]+(?:--[A-Za-z0-9._-]+)+", part)
        ]
        if not model_indexes:
            raise ValueError(f"unsafe symlink outside a model snapshot: {relative}")
        model_index = model_indexes[-1]
        snapshot_index = model_index + 1
        if (len(parts) <= snapshot_index + 2 or
                parts[snapshot_index] != "snapshots" or
                not re.fullmatch(r"[0-9a-f]{40,64}", parts[snapshot_index + 1])):
            raise ValueError(f"unsafe model snapshot symlink shape: {relative}")
        for index in range(1, len(parts)):
            ancestor = root.joinpath(*parts[:index])
            if not stat.S_ISDIR(ancestor.lstat().st_mode):
                raise ValueError(
                    f"model repository ancestor is not a real directory: {ancestor.relative_to(root)}"
                )
            if not os.access(ancestor, os.R_OK | os.X_OK):
                raise ValueError(
                    f"model repository ancestor is not runner-readable: {ancestor.relative_to(root)}"
                )
        parent = link.parent
        if parent not in parents:
            parent_stat = parent.lstat()
            if (not stat.S_ISDIR(parent_stat.st_mode) or
                    parent_stat.st_uid != os.geteuid()):
                raise ValueError(f"materialization parent is not runner-owned: {parent.relative_to(root)}")
            parent_descriptor = os.open(parent, directory_flags)
            held_parent_stat = os.fstat(parent_descriptor)
            if ((held_parent_stat.st_dev, held_parent_stat.st_ino) !=
                    (parent_stat.st_dev, parent_stat.st_ino) or
                    not stat.S_ISDIR(held_parent_stat.st_mode) or
                    held_parent_stat.st_uid != os.geteuid()):
                os.close(parent_descriptor)
                raise ValueError(f"materialization parent changed during preflight: {parent.relative_to(root)}")
            parents[parent] = {
                "descriptor": parent_descriptor,
                "identity": (held_parent_stat.st_dev, held_parent_stat.st_ino),
                "mode": stat.S_IMODE(held_parent_stat.st_mode),
            }
        parent_metadata = parents[parent]
        parent_descriptor = parent_metadata["descriptor"]

        link_stat = os.stat(link.name, dir_fd=parent_descriptor, follow_symlinks=False)
        if not stat.S_ISLNK(link_stat.st_mode):
            raise ValueError(f"model snapshot symlink changed during preflight: {relative}")
        raw_target = os.readlink(link.name, dir_fd=parent_descriptor)
        if os.path.isabs(raw_target):
            raise ValueError(f"absolute symlink escapes artifact root: {relative}")
        try:
            target = link.resolve(strict=True)
        except (OSError, RuntimeError) as error:
            raise ValueError(f"symlink cycle or missing target: {relative}: {error}")
        try:
            target_relative = target.relative_to(root)
        except ValueError:
            raise ValueError(f"symlink target escapes artifact root: {relative}")
        target_parts = target_relative.parts
        expected_prefix = parts[:model_index + 1] + ("blobs",)
        if (target_parts[:-1] != expected_prefix or
                not re.fullmatch(r"[0-9a-f]{64}", target_parts[-1])):
            raise ValueError(f"model snapshot symlink does not bind an exact internal blob: {relative}")
        blobs = root.joinpath(*expected_prefix)
        if not stat.S_ISDIR(blobs.lstat().st_mode):
            raise ValueError(f"model blob directory is not a real directory: {blobs.relative_to(root)}")
        descriptor = os.open(
            target,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0),
        )
        blob_descriptors.append(descriptor)
        target_stat = os.fstat(descriptor)
        if not stat.S_ISREG(target_stat.st_mode):
            raise ValueError(f"model snapshot symlink target is not a regular file: {relative}")
        temporary_name = link.name + ".pensyve-materialize"
        rollback_name = link.name + ".pensyve-rollback"
        try:
            os.stat(temporary_name, dir_fd=parent_descriptor, follow_symlinks=False)
        except FileNotFoundError:
            pass
        else:
            raise ValueError(
                f"materialization temporary path already exists: "
                f"{link.with_name(temporary_name).relative_to(root)}"
            )
        try:
            os.stat(rollback_name, dir_fd=parent_descriptor, follow_symlinks=False)
        except FileNotFoundError:
            pass
        else:
            raise ValueError(
                f"materialization rollback path already exists: "
                f"{link.with_name(rollback_name).relative_to(root)}"
            )
        records.append({
            "link": link,
            "link_name": link.name,
            "relative": relative,
            "raw_target": raw_target,
            "link_identity": (link_stat.st_dev, link_stat.st_ino),
            "parent": parent,
            "parent_metadata": parent_metadata,
            "descriptor": descriptor,
            "target_identity": (
                target_stat.st_dev, target_stat.st_ino, target_stat.st_mode,
                target_stat.st_size, target_stat.st_mtime_ns,
            ),
            "target_mode": stat.S_IMODE(target_stat.st_mode) & 0o777,
            "temporary_name": temporary_name,
            "temporary_identity": None,
            "rollback_name": rollback_name,
        })
except Exception as error:
    for descriptor in blob_descriptors:
        os.close(descriptor)
    for metadata in parents.values():
        os.close(metadata["descriptor"])
    raise SystemExit(str(error))

materialized = []
try:
    # Add write permission only to globally validated, runner-owned parents.
    for parent, metadata in sorted(parents.items(), key=lambda item: str(item[0])):
        current_descriptor = os.open(parent, directory_flags)
        try:
            current = os.fstat(current_descriptor)
        finally:
            os.close(current_descriptor)
        if ((current.st_dev, current.st_ino) != metadata["identity"] or
                current.st_uid != os.geteuid() or not stat.S_ISDIR(current.st_mode)):
            raise ValueError(f"materialization parent changed after preflight: {parent.relative_to(root)}")
        os.fchmod(metadata["descriptor"], metadata["mode"] | stat.S_IWUSR)

    for record in records:
        parent = record["parent"]
        parent_metadata = record["parent_metadata"]
        parent_descriptor = parent_metadata["descriptor"]
        current_parent_descriptor = os.open(parent, directory_flags)
        try:
            current_parent = os.fstat(current_parent_descriptor)
        finally:
            os.close(current_parent_descriptor)
        if (current_parent.st_dev, current_parent.st_ino) != parent_metadata["identity"]:
            raise ValueError(f"materialization parent changed during materialization: {parent.relative_to(root)}")
        current = os.stat(record["link_name"], dir_fd=parent_descriptor, follow_symlinks=False)
        if (not stat.S_ISLNK(current.st_mode) or
                (current.st_dev, current.st_ino) != record["link_identity"] or
                os.readlink(record["link_name"], dir_fd=parent_descriptor) != record["raw_target"]):
            raise ValueError(f"model snapshot symlink changed during materialization: {record['relative']}")
        target_stat = os.fstat(record["descriptor"])
        target_identity = (
            target_stat.st_dev, target_stat.st_ino, target_stat.st_mode,
            target_stat.st_size, target_stat.st_mtime_ns,
        )
        if target_identity != record["target_identity"]:
            raise ValueError(f"model blob changed during materialization: {record['relative']}")
        os.lseek(record["descriptor"], 0, os.SEEK_SET)
        temporary_descriptor = os.open(
            record["temporary_name"],
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o600,
            dir_fd=parent_descriptor,
        )
        try:
            temporary_stat = os.fstat(temporary_descriptor)
            record["temporary_identity"] = (temporary_stat.st_dev, temporary_stat.st_ino)
            destination = os.fdopen(temporary_descriptor, "wb")
            temporary_descriptor = None
            with destination:
                with os.fdopen(os.dup(record["descriptor"]), "rb") as source:
                    shutil.copyfileobj(source, destination, 1024 * 1024)
                os.fchmod(destination.fileno(), record["target_mode"])
        finally:
            if temporary_descriptor is not None:
                os.close(temporary_descriptor)
        target_stat = os.fstat(record["descriptor"])
        target_identity = (
            target_stat.st_dev, target_stat.st_ino, target_stat.st_mode,
            target_stat.st_size, target_stat.st_mtime_ns,
        )
        current_parent_descriptor = os.open(parent, directory_flags)
        try:
            current_parent = os.fstat(current_parent_descriptor)
        finally:
            os.close(current_parent_descriptor)
        if (current_parent.st_dev, current_parent.st_ino) != parent_metadata["identity"]:
            raise ValueError(f"materialization parent changed during materialization: {parent.relative_to(root)}")
        current = os.stat(record["link_name"], dir_fd=parent_descriptor, follow_symlinks=False)
        if (target_identity != record["target_identity"] or
                not stat.S_ISLNK(current.st_mode) or
                (current.st_dev, current.st_ino) != record["link_identity"] or
                os.readlink(record["link_name"], dir_fd=parent_descriptor) != record["raw_target"]):
            raise ValueError(f"model snapshot symlink changed during materialization: {record['relative']}")
        temporary_stat = os.stat(
            record["temporary_name"], dir_fd=parent_descriptor, follow_symlinks=False
        )
        if ((temporary_stat.st_dev, temporary_stat.st_ino) != record["temporary_identity"] or
                not stat.S_ISREG(temporary_stat.st_mode)):
            raise ValueError(f"materialization temporary changed: {record['relative']}")
        os.replace(
            record["temporary_name"], record["link_name"],
            src_dir_fd=parent_descriptor, dst_dir_fd=parent_descriptor,
        )
        replaced = os.stat(record["link_name"], dir_fd=parent_descriptor, follow_symlinks=False)
        if ((replaced.st_dev, replaced.st_ino) != record["temporary_identity"] or
                not stat.S_ISREG(replaced.st_mode)):
            raise ValueError(f"installed materialization changed: {record['relative']}")
        record["materialized_identity"] = (replaced.st_dev, replaced.st_ino)
        materialized.append(record)

    for parent, metadata in sorted(parents.items(), key=lambda item: str(item[0])):
        current_descriptor = os.open(parent, directory_flags)
        try:
            current = os.fstat(current_descriptor)
        finally:
            os.close(current_descriptor)
        if (current.st_dev, current.st_ino) != metadata["identity"]:
            raise ValueError(f"materialization parent changed before commit: {parent.relative_to(root)}")

    # Integrity is guaranteed through this commit check; exclusive seal/replay owns post-return changes.
    for record in materialized:
        parent_descriptor = record["parent_metadata"]["descriptor"]
        installed_descriptor = os.open(
            record["link_name"],
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0),
            dir_fd=parent_descriptor,
        )
        try:
            installed_before = os.fstat(installed_descriptor)
            if ((installed_before.st_dev, installed_before.st_ino) !=
                    record["materialized_identity"] or
                    not stat.S_ISREG(installed_before.st_mode)):
                raise ValueError(f"materialized entry changed before commit: {record['relative']}")
            installed_hash = hashlib.sha256()
            while True:
                chunk = os.read(installed_descriptor, 1024 * 1024)
                if not chunk:
                    break
                installed_hash.update(chunk)
            installed_after = os.fstat(installed_descriptor)
            if ((installed_after.st_dev, installed_after.st_ino, installed_after.st_mode,
                    installed_after.st_size, installed_after.st_mtime_ns) !=
                    (installed_before.st_dev, installed_before.st_ino, installed_before.st_mode,
                    installed_before.st_size, installed_before.st_mtime_ns)):
                raise ValueError(f"materialized entry changed during commit: {record['relative']}")
        finally:
            os.close(installed_descriptor)

        os.lseek(record["descriptor"], 0, os.SEEK_SET)
        blob_hash = hashlib.sha256()
        while True:
            chunk = os.read(record["descriptor"], 1024 * 1024)
            if not chunk:
                break
            blob_hash.update(chunk)
        os.lseek(record["descriptor"], 0, os.SEEK_SET)
        if installed_hash.digest() != blob_hash.digest():
            raise ValueError(f"materialized entry bytes changed before commit: {record['relative']}")

    # Restore original modes; seal-tree performs the final 0755 normalization.
    for parent, metadata in sorted(parents.items(), key=lambda item: str(item[0]), reverse=True):
        os.fchmod(metadata["descriptor"], metadata["mode"])
except Exception as error:
    rollback_errors = []
    for parent, metadata in sorted(parents.items(), key=lambda item: str(item[0])):
        try:
            current = os.fstat(metadata["descriptor"])
            if ((current.st_dev, current.st_ino) != metadata["identity"] or
                    current.st_uid != os.geteuid() or not stat.S_ISDIR(current.st_mode)):
                raise OSError("held parent identity changed")
            os.fchmod(metadata["descriptor"], metadata["mode"] | stat.S_IWUSR)
        except OSError as rollback_error:
            rollback_errors.append(f"could not reopen {parent.relative_to(root)}: {rollback_error}")
    for record in records:
        try:
            try:
                temporary = os.stat(
                    record["temporary_name"],
                    dir_fd=record["parent_metadata"]["descriptor"],
                    follow_symlinks=False,
                )
            except FileNotFoundError:
                continue
            if ((temporary.st_dev, temporary.st_ino) != record["temporary_identity"] or
                    not stat.S_ISREG(temporary.st_mode)):
                raise OSError("materialization temporary identity changed")
            os.unlink(record["temporary_name"], dir_fd=record["parent_metadata"]["descriptor"])
        except OSError as rollback_error:
            rollback_errors.append(
                f"could not remove {record['relative']}.pensyve-materialize: {rollback_error}"
            )
    for record in reversed(materialized):
        rollback_identity = None
        try:
            parent_descriptor = record["parent_metadata"]["descriptor"]
            current = os.stat(record["link_name"], dir_fd=parent_descriptor, follow_symlinks=False)
            if ((current.st_dev, current.st_ino) != record["materialized_identity"] or
                    not stat.S_ISREG(current.st_mode)):
                raise OSError("materialized path identity changed")
            os.symlink(record["raw_target"], record["rollback_name"], dir_fd=parent_descriptor)
            rollback = os.stat(record["rollback_name"], dir_fd=parent_descriptor, follow_symlinks=False)
            rollback_identity = (rollback.st_dev, rollback.st_ino)
            os.replace(
                record["rollback_name"], record["link_name"],
                src_dir_fd=parent_descriptor, dst_dir_fd=parent_descriptor,
            )
        except OSError as rollback_error:
            rollback_errors.append(f"could not roll back {record['relative']}: {rollback_error}")
        finally:
            if rollback_identity is not None:
                try:
                    rollback = os.stat(
                        record["rollback_name"], dir_fd=parent_descriptor, follow_symlinks=False
                    )
                except FileNotFoundError:
                    pass
                else:
                    if (rollback.st_dev, rollback.st_ino) == rollback_identity:
                        os.unlink(record["rollback_name"], dir_fd=parent_descriptor)
                    else:
                        rollback_errors.append(
                            f"could not remove {record['relative']}.pensyve-rollback: identity changed"
                        )
    for parent, metadata in sorted(parents.items(), key=lambda item: str(item[0]), reverse=True):
        try:
            os.fchmod(metadata["descriptor"], metadata["mode"])
        except OSError as rollback_error:
            rollback_errors.append(f"could not restore {parent.relative_to(root)}: {rollback_error}")
    for descriptor in blob_descriptors:
        os.close(descriptor)
    for metadata in parents.values():
        os.close(metadata["descriptor"])
    detail = f"model materialization failed: {error}"
    if rollback_errors:
        detail += "; rollback errors: " + "; ".join(rollback_errors)
    raise SystemExit(detail)

for descriptor in blob_descriptors:
    os.close(descriptor)
for metadata in parents.values():
    os.close(metadata["descriptor"])
PY
}

seal_tree() {
    local root manifest transcript tree manifest_rel transcript_rel
    root="$(realpath "$1")"
    manifest="$(realpath -m "$2")"
    transcript="$(realpath -m "$3")"
    tree="${root}/sealed-tree.json"
    [[ -d "${root}" ]] || die "seal root is absent: ${root}"
    [[ "${manifest}" == "${root}/"* && "${transcript}" == "${root}/"* ]] \
        || die "seal manifest and transcript must remain inside the artifact root"
    [[ ! -e "${manifest}" && ! -e "${transcript}" && ! -e "${tree}" ]] || die "seal outputs already exist"
    manifest_rel="${manifest#${root}/}"
    transcript_rel="${transcript#${root}/}"
    python3 - "${root}" "${manifest_rel}" "${transcript_rel}" <<'PY'
import hashlib
import json
import os
import stat
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve(strict=True)
manifest_rel, transcript_rel = sys.argv[2:]
tree_rel = "sealed-tree.json"
excluded = {manifest_rel, transcript_rel, tree_rel}
entries = []

def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()

def walk_error(error):
    raise error

for current, directories, files in os.walk(root, topdown=True, followlinks=False, onerror=walk_error):
    for name in sorted(directories + files):
        path = Path(current, name)
        relative = path.relative_to(root).as_posix()
        if relative in excluded:
            continue
        if any(ord(character) < 32 or character == "\\" for character in relative):
            raise SystemExit(f"unsafe artifact path: {relative!r}")
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            raw_target = os.readlink(path)
            raise SystemExit(f"unmaterialized symlink rejected: {relative} -> {raw_target}")
        if stat.S_ISDIR(mode):
            if not os.access(path, os.R_OK | os.X_OK):
                raise SystemExit(f"artifact tree is not runner-readable for seal traversal: {relative}")
            os.chmod(path, 0o755)
            entries.append({"path": relative, "type": "directory", "mode": "0755"})
        elif stat.S_ISREG(mode):
            if not os.access(path, os.R_OK):
                raise SystemExit(f"artifact tree is not runner-readable: {relative}")
            os.chmod(path, 0o644)
            entries.append({"path": relative, "type": "file", "mode": "0644",
                            "bytes": path.stat().st_size, "sha256": digest(path)})
        else:
            raise SystemExit(f"special entry rejected: {relative} mode={stat.S_IFMT(mode):#o}")

if not any(entry["type"] == "file" for entry in entries):
    raise SystemExit("artifact tree has no files to seal")
document = {
    "schema_version": 1,
    "entries": sorted(entries, key=lambda entry: entry["path"]),
    "seal_outputs": {"manifest": manifest_rel, "transcript": transcript_rel, "tree": tree_rel},
}
tree_path = root / tree_rel
tree_path.write_text(json.dumps(document, sort_keys=True, separators=(",", ":")) + "\n")
os.chmod(tree_path, 0o644)

files = [entry for entry in document["entries"] if entry["type"] == "file"]
files.append({"path": tree_rel, "sha256": digest(tree_path)})
manifest = root / manifest_rel
manifest.parent.mkdir(parents=True, exist_ok=True)
manifest.write_text("".join(f'{entry["sha256"]}  {entry["path"]}\n' for entry in sorted(files, key=lambda item: item["path"])))
os.chmod(manifest, 0o644)
PY
    if ! (cd "${root}" && sha256sum --check "${manifest_rel}") > "${transcript}" 2>&1; then
        die "full artifact seal replay failed"
    fi
    chmod 0644 "${transcript}"
    du -sb "${root}" >/dev/null 2>&1 || die "artifact tree is not runner-readable for du traversal"
    grep -F ': OK' "${transcript}" >/dev/null || die "full artifact seal replay produced no audited checks"
}

verify_tree() {
    local root tree transcript
    root="$(realpath "$1")"
    tree="$(realpath "$2")"
    transcript="$(realpath -m "$3")"
    [[ -d "${root}" && -f "${tree}" ]] || die "sealed tree evidence is absent"
    [[ ! -e "${transcript}" ]] || die "tree replay transcript already exists"
    python3 - "${root}" "${tree}" "${transcript}" <<'PY'
import hashlib
import json
import os
import stat
import subprocess
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve(strict=True)
tree = Path(sys.argv[2]).resolve(strict=True)
replay = Path(sys.argv[3])
data = json.loads(tree.read_text())
if type(data.get("schema_version")) is not int or data.get("schema_version") != 1 or set(data) != {"schema_version", "entries", "seal_outputs"}:
    raise SystemExit("sealed tree shape mismatch")
outputs = data["seal_outputs"]
if set(outputs) != {"manifest", "transcript", "tree"} or outputs.get("tree") != tree.relative_to(root).as_posix():
    raise SystemExit("sealed tree output binding mismatch")

expected = {entry["path"]: entry for entry in data["entries"]}
expected_paths = set(expected) | set(outputs.values())
actual = {}
def walk_error(error):
    raise error

for current, directories, files in os.walk(root, topdown=True, followlinks=False, onerror=walk_error):
    for name in directories + files:
        path = Path(current, name)
        relative = path.relative_to(root).as_posix()
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            raise SystemExit(f"symlink retarget rejected: {relative} -> {os.readlink(path)}")
        if stat.S_ISDIR(mode):
            kind = "directory"
        elif stat.S_ISREG(mode):
            kind = "file"
        else:
            raise SystemExit(f"special entry rejected: {relative}")
        actual[relative] = (kind, f"{stat.S_IMODE(mode):04o}", path)
if set(actual) != expected_paths:
    missing = sorted(expected_paths - set(actual))
    extra = sorted(set(actual) - expected_paths)
    raise SystemExit(f"directory topology drift: missing={missing} extra={extra}")

def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()

for relative, entry in expected.items():
    kind, mode, path = actual[relative]
    if kind != entry.get("type") or mode != entry.get("mode"):
        raise SystemExit(f"entry type/mode drift: {relative}")
    if kind == "file" and (type(entry.get("bytes")) is not int or
            path.stat().st_size != entry.get("bytes") or digest(path) != entry.get("sha256")):
        raise SystemExit(f"entry byte/hash drift: {relative}")
for relative in outputs.values():
    kind, mode, _ = actual[relative]
    if kind != "file" or mode != "0644":
        raise SystemExit(f"seal output type/mode drift: {relative}")

manifest_rel = outputs["manifest"]
manifest = root / manifest_rel
files = [entry for entry in data["entries"] if entry["type"] == "file"]
files.append({"path": outputs["tree"], "sha256": digest(root / outputs["tree"])})
expected_manifest = "".join(f'{entry["sha256"]}  {entry["path"]}\n' for entry in sorted(files, key=lambda item: item["path"]))
if manifest.read_text() != expected_manifest:
    raise SystemExit("sealed file manifest drift")
result = subprocess.run(["sha256sum", "--check", manifest_rel], cwd=root, text=True,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=300)
replay.parent.mkdir(parents=True, exist_ok=True)
replay.write_text(result.stdout)
os.chmod(replay, 0o644)
if result.returncode != 0 or ": OK" not in result.stdout:
    raise SystemExit("exact-tree hash replay failed")
PY
}

verify_handoff() {
    local input="$1"
    [[ -f "${input}" ]] || die "verified-image input is absent: ${input}"
    verify_scan_postupload "${input}" >/dev/null
    python3 - "${input}" <<'PY'
import hashlib
import json
import re
import sys
from pathlib import Path

data = json.loads(Path(sys.argv[1]).read_text())

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

expected_top = {"schema_version", "cleanup_required", "image", "scanner", "scan", "deployment"}
if (set(data) != expected_top or not exact_int(data.get("schema_version")) or
        data.get("schema_version") != 1 or data.get("cleanup_required") is not False):
    fail("fixed verified-image shape is invalid")
image = data.get("image")
if not isinstance(image, dict):
    fail("fixed verified-image image authority is invalid")
for name in ("compressed_layer_bytes", "uncompressed_image_bytes"):
    if not exact_int(image.get(name)) or image[name] <= 0:
        fail(f"image {name.replace('_', ' ')} must be a positive integer")
deployment = data.get("deployment")
expected_deployment = {
    "region", "ecr_registry", "ecr_repository", "cluster", "service", "gateway_container",
    "baseline_task_definition_arn", "baseline_image", "baseline_environment_sha256",
    "baseline_service_snapshot", "baseline_service_snapshot_sha256",
    "probe_entity",
    "promotion_run_id", "promotion_run_attempt",
    "cpu", "memory", "desired_count", "running_count", "pending_count",
}
if not isinstance(deployment, dict) or set(deployment) != expected_deployment:
    fail("Task 8 deployment shape is invalid")
fixed = {
    "region": "us-east-2",
    "ecr_repository": "pensyve-gateway",
    "cluster": "pensyve-prod",
    "service": "pensyve-prod-gateway",
    "gateway_container": "gateway",
    "cpu": "512",
    "memory": "4096",
    "desired_count": 2,
    "running_count": 2,
    "pending_count": 0,
}
for name, expected in fixed.items():
    actual = deployment.get(name)
    if name in ("desired_count", "running_count", "pending_count") and not exact_int(actual):
        fail(f"Task 8 {name} mismatch")
    if actual != expected:
        fail(f"Task 8 {name} mismatch")
registry = deployment.get("ecr_registry")
match = re.fullmatch(r"([0-9]{12})\.dkr\.ecr\.us-east-2\.amazonaws\.com", str(registry))
if not match:
    fail("Task 8 ECR registry mismatch")
account = match.group(1)
baseline_arn = str(deployment.get("baseline_task_definition_arn", ""))
arn_match = re.fullmatch(
    "arn:" + "a" + rf"ws:ecs:us-east-2:{account}:task-definition/pensyve-prod-gateway:([1-9][0-9]*)",
    baseline_arn,
)
if not arn_match:
    fail("Task 8 baseline task-definition ARN mismatch")
if arn_match.group(1) == "157" or ":157" in json.dumps(data, sort_keys=True, allow_nan=False):
    fail("Task 8 binding refuses rejected :157")
expected_image_prefix = f"{registry}/pensyve-gateway@sha256:"
baseline_image = str(deployment.get("baseline_image", ""))
if not baseline_image.startswith(expected_image_prefix) or not re.fullmatch(
    re.escape(expected_image_prefix) + r"[0-9a-f]{64}", baseline_image
):
    fail("Task 8 baseline image shape mismatch")
if not re.fullmatch(r"[0-9a-f]{64}", str(deployment.get("baseline_environment_sha256", ""))):
    fail("Task 8 baseline environment digest mismatch")
snapshot = deployment.get("baseline_service_snapshot")
expected_snapshot_keys = {
    "service_name", "status", "cluster_arn", "task_definition", "counts",
    "network_configuration", "load_balancers", "deployment_configuration",
    "health_grace_period_seconds", "primary_deployment",
}
if not isinstance(snapshot, dict) or set(snapshot) != expected_snapshot_keys:
    fail("Task 8 canonical service snapshot shape mismatch")
if snapshot.get("service_name") != "pensyve-prod-gateway" or snapshot.get("status") != "ACTIVE":
    fail("Task 8 canonical service snapshot identity mismatch")
if not str(snapshot.get("cluster_arn", "")).endswith("/pensyve-prod"):
    fail("Task 8 canonical service snapshot cluster mismatch")
counts = snapshot.get("counts")
if (snapshot.get("task_definition") != baseline_arn or not isinstance(counts, dict) or
        set(counts) != {"desired", "running", "pending"} or
        any(not exact_int(counts[name]) for name in counts) or
        counts != {"desired": 2, "running": 2, "pending": 0}):
    fail("Task 8 canonical service snapshot task/count mismatch")
primary = snapshot.get("primary_deployment")
if (not isinstance(primary, dict) or
        any(not exact_int(primary.get(name)) for name in ("desired", "running", "pending")) or primary != {
    "status": "PRIMARY", "task_definition": baseline_arn, "rollout_state": "COMPLETED",
    "desired": 2, "running": 2, "pending": 0,
}):
    fail("Task 8 canonical primary deployment mismatch")
if not isinstance(snapshot.get("network_configuration"), dict) or not isinstance(snapshot.get("load_balancers"), list):
    fail("Task 8 canonical network/load-balancer snapshot mismatch")
if (not isinstance(snapshot.get("deployment_configuration"), dict) or
        not exact_int(snapshot.get("health_grace_period_seconds"))):
    fail("Task 8 canonical deployment configuration snapshot mismatch")
network = snapshot["network_configuration"]
if set(network) != {"awsvpcConfiguration"} or not isinstance(network["awsvpcConfiguration"], dict):
    fail("Task 8 canonical network configuration shape mismatch")
awsvpc = network["awsvpcConfiguration"]
if set(awsvpc) != {"subnets", "securityGroups", "assignPublicIp"}:
    fail("Task 8 canonical awsvpc configuration shape mismatch")
if (not isinstance(awsvpc["subnets"], list) or not awsvpc["subnets"] or
        len(set(awsvpc["subnets"])) != len(awsvpc["subnets"]) or
        not all(re.fullmatch(r"subnet-[0-9A-Za-z]+", str(value)) for value in awsvpc["subnets"])):
    fail("Task 8 canonical subnet bindings are invalid")
if (not isinstance(awsvpc["securityGroups"], list) or not awsvpc["securityGroups"] or
        len(set(awsvpc["securityGroups"])) != len(awsvpc["securityGroups"]) or
        not all(re.fullmatch(r"sg-[0-9A-Za-z]+", str(value)) for value in awsvpc["securityGroups"]) or
        awsvpc["assignPublicIp"] != "DISABLED"):
    fail("Task 8 canonical security/public-IP bindings are invalid")
load_balancers = snapshot["load_balancers"]
if len(load_balancers) != 1 or set(load_balancers[0]) != {"targetGroupArn", "containerName", "containerPort"}:
    fail("Task 8 canonical load-balancer binding shape mismatch")
load_balancer = load_balancers[0]
if (load_balancer["containerName"] != "gateway" or not exact_int(load_balancer["containerPort"]) or
        load_balancer["containerPort"] != 3100 or
        not re.fullmatch(rf"arn:aws:elasticloadbalancing:us-east-2:{account}:targetgroup/[0-9A-Za-z._/-]+", str(load_balancer["targetGroupArn"]))):
    fail("Task 8 canonical load-balancer binding is invalid")
configuration = snapshot["deployment_configuration"]
if set(configuration) != {"deploymentCircuitBreaker", "maximumPercent", "minimumHealthyPercent"}:
    fail("Task 8 canonical deployment configuration shape mismatch")
breaker = configuration["deploymentCircuitBreaker"]
if (not isinstance(breaker, dict) or set(breaker) != {"enable", "rollback"} or
        breaker.get("enable") is not True or breaker.get("rollback") is not True):
    fail("Task 8 canonical deployment circuit breaker is invalid")
if (not exact_int(configuration["maximumPercent"]) or configuration["maximumPercent"] < 100 or
        not exact_int(configuration["minimumHealthyPercent"]) or
        not 0 <= configuration["minimumHealthyPercent"] <= 100 or
        snapshot["health_grace_period_seconds"] < 0):
    fail("Task 8 canonical deployment percentages/grace are invalid")
snapshot_bytes = (json.dumps(snapshot, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode()
if hashlib.sha256(snapshot_bytes).hexdigest() != deployment.get("baseline_service_snapshot_sha256"):
    fail("Task 8 canonical service snapshot digest mismatch")
for name in ("promotion_run_id", "promotion_run_attempt"):
    if not exact_int(deployment.get(name)) or deployment[name] <= 0:
        fail(f"Task 8 {name} is invalid")
probe_entity = deployment.get("probe_entity")
expected_probe_prefix = f"task9-runtime-{deployment['promotion_run_id']}-{deployment['promotion_run_attempt']}-"
if not re.fullmatch(re.escape(expected_probe_prefix) + r"[0-9a-f]{16}", str(probe_entity)):
    fail("Task 9 probe entity is not sealed to promotion run, attempt, and random nonce")
source_label = data.get("image", {}).get("source_label")
if not re.fullmatch(r"[0-9a-f]{40}", str(source_label)):
    fail("Task 8 candidate image source label mismatch")
print("fixed verified-image and Task 8 deployment binding verified")
PY
}

seal_tuple() {
    local input="$1" output="$2"
    verify_local "${input}" >/dev/null
    [[ ! -e "${output}" ]] || die "sealed tuple output already exists"
    jq -S -c . "${input}" > "${output}"
    sha256sum "${output}" > "${output}.sha256"
}

fetch_verify() {
    local tuple="$1" request="$2" output="$3" output_tmp
    verify_local "${tuple}" >/dev/null
    output_tmp="$(mktemp "${output}.tmp.XXXXXX")"
    if ! python3 - "${tuple}" "${request}" "${output_tmp}" <<'PY'
import json
import sys
from pathlib import Path

tuple_path, request_path, output_path = map(Path, sys.argv[1:])
source = json.loads(tuple_path.read_text())
request = json.loads(request_path.read_text())

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

bindings = {
    "repository": source["source"]["repository"],
    "workflow": source["source"]["workflow"],
    "workflow_path": source["source"]["workflow_path"],
    "ref": source["source"]["ref"],
    "event": source["source"]["event"],
    "run_id": source["source"]["run_id"],
    "run_attempt": source["source"]["run_attempt"],
    "head_sha": source["source"]["head_sha"],
    "pull_request_number": source["pull_request"]["number"],
    "artifact_id": source["artifact"]["id"],
    "artifact_name": source["artifact"]["name"],
}
for name, actual in bindings.items():
    requested = request.get(name)
    if name in ("run_id", "run_attempt", "pull_request_number", "artifact_id") and not exact_int(requested):
        fail(f"cross-run {name} mismatch")
    if requested != actual:
        fail(f"cross-run {name} mismatch")

reviewed = source.get("reviewed_pull_request")
if not isinstance(reviewed, dict):
    fail("Task 5-reviewed pull request is absent")
expected_reviewed = {
    "number": source["pull_request"]["number"],
    "repository": source["source"]["repository"],
    "state": "open",
    "draft": False,
    "base_ref": "main",
    "head_repository": source["source"]["repository"],
    "head_ref": source["source"]["ref"].removeprefix("refs/heads/"),
    "head_sha": source["source"]["head_sha"],
}
if set(reviewed) != set(expected_reviewed):
    fail("Task 5-reviewed pull request shape mismatch")
for name, expected in expected_reviewed.items():
    if name == "number" and (not exact_int(reviewed.get(name)) or not exact_int(request.get(f"reviewed_pull_request_{name}"))):
        fail(f"Task 5-reviewed pull request {name} mismatch")
    if name == "draft" and (reviewed.get(name) is not False or request.get(f"reviewed_pull_request_{name}") is not False):
        fail("Task 5-reviewed pull request draft mismatch")
    if reviewed.get(name) != expected:
        fail(f"Task 5-reviewed pull request {name} mismatch")
    if request.get(f"reviewed_pull_request_{name}") != reviewed.get(name):
        fail(f"Task 5-reviewed pull request {name} drift")
if request.get("promotion_event") != "workflow_dispatch":
    fail("promotion event mismatch")
if request.get("promotion_head_sha") != source["source"]["head_sha"]:
    fail("promotion head SHA drift")
deployment = request.get("deployment")
if not isinstance(deployment, dict):
    fail("Task 8 deployment binding is absent")
verified = {
    "schema_version": 1,
    "cleanup_required": False,
    "image": source["image"],
    "scanner": source["scanner"],
    "scan": source["scan"],
    "deployment": deployment,
}
output_path.write_text(json.dumps(verified, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
PY
    then
        rm -f -- "${output_tmp}"
        return 1
    fi
    if ! verify_handoff "${output_tmp}" >/dev/null; then
        rm -f -- "${output_tmp}"
        return 1
    fi
    mv -- "${output_tmp}" "${output}"
}

verify_custodian_runs() {
    local input="$1" request="$2" output="$3" output_tmp
    [[ -f "${input}" ]] || die "paginated workflow runs are absent: ${input}"
    [[ -f "${request}" ]] || die "custody request is absent: ${request}"
    output_tmp="$(mktemp "${output}.tmp.XXXXXX")"
    if ! python3 - "${input}" "${request}" "${output_tmp}" <<'PY'
import json
import re
import sys
from datetime import datetime, timedelta
from pathlib import Path

pages_path, request_path, output_path = map(Path, sys.argv[1:])

def fail(message):
    print(f"gateway image artifact error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

def timestamp(value, name):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", value):
        fail(f"invalid custody {name}")
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        fail(f"invalid custody {name}")

try:
    pages = json.loads(pages_path.read_text())
    request = json.loads(request_path.read_text())
except Exception:
    fail("malformed paginated workflow runs")

request_keys = {
    "schema_version", "repository", "parent_run_id", "parent_run_attempt", "parent_ref",
    "reviewed_sha", "pull_request_number", "handoff_id", "handoff_name", "handoff_digest",
    "handoff_size", "handoff_created_at", "handoff_expires_at", "source_artifact_expires_at",
    "parent_created_at", "dispatch_started_at", "authorization_start_at",
    "authorization_end_at", "custody_lease_id",
}
if (not isinstance(request, dict) or set(request) != request_keys or
        not exact_int(request.get("schema_version")) or request.get("schema_version") != 2):
    fail("custody request authorization window shape mismatch")
if request.get("repository") != "major7apps/pensyve":
    fail("custody request repository mismatch")
if not re.fullmatch(r"[0-9a-f]{40}", str(request.get("reviewed_sha", ""))):
    fail("custody request reviewed SHA mismatch")
if not re.fullmatch(r"[0-9a-f]{64}", str(request.get("custody_lease_id", ""))):
    fail("custody request lease mismatch")
if not re.fullmatch(r"sha256:[0-9a-f]{64}", str(request.get("handoff_digest", ""))):
    fail("custody request handoff digest mismatch")
if not re.fullmatch(r"[A-Za-z0-9._/-]+", str(request.get("parent_ref", ""))):
    fail("custody request ref mismatch")
for name in ("parent_run_id", "parent_run_attempt", "pull_request_number", "handoff_id", "handoff_size"):
    if not exact_int(request.get(name)) or request[name] <= 0:
        fail(f"custody request {name} mismatch")
expected_name = f"gateway-handoff-{request['parent_run_id']}-{request['parent_run_attempt']}-{request['reviewed_sha']}"
if request.get("handoff_name") != expected_name:
    fail("custody request handoff name mismatch")

parent_created = timestamp(request.get("parent_created_at"), "parent creation timestamp")
dispatch_started = timestamp(request.get("dispatch_started_at"), "dispatch start timestamp")
handoff_created = timestamp(request.get("handoff_created_at"), "handoff creation timestamp")
handoff_expires = timestamp(request.get("handoff_expires_at"), "handoff expiry timestamp")
source_expires = timestamp(request.get("source_artifact_expires_at"), "source expiry timestamp")
authorization_start = timestamp(request.get("authorization_start_at"), "authorization start timestamp")
authorization_end = timestamp(request.get("authorization_end_at"), "authorization end timestamp")
if dispatch_started < parent_created:
    fail("custody dispatch start predates the parent run")
if handoff_created > dispatch_started or dispatch_started > handoff_expires or dispatch_started > source_expires:
    fail("custody dispatch is outside immutable artifact lifetime")
if authorization_start != parent_created - timedelta(minutes=5):
    fail("custody authorization start must be the sealed parent creation minus skew")
if authorization_end != min(handoff_expires, source_expires) or authorization_start >= authorization_end:
    fail("custody authorization end must be the earliest sealed artifact expiry")

if not isinstance(pages, list) or not pages:
    fail("malformed paginated workflow runs")
runs = []
total_count = None
for page in pages:
    if (not isinstance(page, dict) or not exact_int(page.get("total_count")) or
            page["total_count"] < 0 or page["total_count"] > 1000 or
            not isinstance(page.get("workflow_runs"), list)):
        fail("malformed paginated workflow runs")
    if total_count is None:
        total_count = page["total_count"]
    elif page["total_count"] != total_count:
        fail("malformed paginated workflow runs")
    runs.extend(page["workflow_runs"])
if len(runs) != total_count:
    fail("malformed paginated workflow runs")

title = f"gateway-custodian-{request['custody_lease_id']}"
matches = []
for run in runs:
    if not isinstance(run, dict):
        fail("malformed paginated workflow runs")
    created = timestamp(run.get("created_at"), "workflow run creation timestamp")
    if created < authorization_start or created > authorization_end:
        fail("workflow run lies outside the sealed authorization window")
    repository = run.get("repository")
    if not isinstance(repository, dict):
        fail("malformed paginated workflow runs")
    if (run.get("display_title") == title and repository.get("full_name") == request["repository"] and
            run.get("event") == "workflow_dispatch" and
            run.get("path") == ".github/workflows/deploy-gateway.yml" and
            run.get("head_sha") == request["reviewed_sha"] and
            run.get("head_branch") == request["parent_ref"] and exact_int(run.get("run_attempt")) and
            run.get("run_attempt") == 1 and exact_int(run.get("id")) and run["id"] > 0):
        matches.append(run)
if len(matches) != 1:
    fail(f"exact custodian run identity is ambiguous: matches={len(matches)}")
output_path.write_text(json.dumps(matches[0], sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
PY
    then
        rm -f -- "${output_tmp}"
        return 1
    fi
    mv -- "${output_tmp}" "${output}"
}

usage() {
    cat >&2 <<'EOF'
usage:
  gateway-image-artifact.sh verify-local --tuple FILE
  gateway-image-artifact.sh storage-precheck --input FILE --output FILE [--replay-reference ISO8601]
  gateway-image-artifact.sh storage-reconcile --input FILE --output FILE
  gateway-image-artifact.sh disk-precheck --input FILE --output FILE
  gateway-image-artifact.sh build --source-sha SHA --archive FILE --evidence-dir DIR --image-ref REF
  gateway-image-artifact.sh seal --input FILE --output FILE
  gateway-image-artifact.sh materialize-model-links --root DIR
  gateway-image-artifact.sh seal-tree --root DIR --manifest FILE --transcript FILE
  gateway-image-artifact.sh verify-tree --root DIR --input sealed-tree.json --transcript REPLAY_FILE
  gateway-image-artifact.sh verify-scan-preupload --tuple FILE
  gateway-image-artifact.sh verify-scan-postupload --tuple FILE
  gateway-image-artifact.sh verify-handoff --input FILE
  gateway-image-artifact.sh fetch-verify --tuple FILE --request FILE --output FILE
  gateway-image-artifact.sh verify-custodian-runs --input FILE --request FILE --output FILE
EOF
    exit 2
}

mode="${1:-}"
[[ -n "${mode}" ]] || usage
shift
tuple="" input="" output="" source_sha="" archive="" evidence_dir="" image_ref="" request=""
root="" manifest="" transcript=""
replay_reference=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --tuple) need_arg "$1" "${2:-}"; tuple="$2"; shift 2 ;;
        --input) need_arg "$1" "${2:-}"; input="$2"; shift 2 ;;
        --output) need_arg "$1" "${2:-}"; output="$2"; shift 2 ;;
        --source-sha) need_arg "$1" "${2:-}"; source_sha="$2"; shift 2 ;;
        --archive) need_arg "$1" "${2:-}"; archive="$2"; shift 2 ;;
        --evidence-dir) need_arg "$1" "${2:-}"; evidence_dir="$2"; shift 2 ;;
        --image-ref) need_arg "$1" "${2:-}"; image_ref="$2"; shift 2 ;;
        --request) need_arg "$1" "${2:-}"; request="$2"; shift 2 ;;
        --root) need_arg "$1" "${2:-}"; root="$2"; shift 2 ;;
        --manifest) need_arg "$1" "${2:-}"; manifest="$2"; shift 2 ;;
        --transcript) need_arg "$1" "${2:-}"; transcript="$2"; shift 2 ;;
        --replay-reference) need_arg "$1" "${2:-}"; replay_reference="$2"; shift 2 ;;
        *) usage ;;
    esac
done

case "${mode}" in
    verify-local)
        [[ -n "${tuple}" ]] || usage
        verify_local "${tuple}"
        ;;
    storage-precheck)
        [[ -n "${input}" && -n "${output}" ]] || usage
        storage_precheck "${input}" "${output}" "${replay_reference}"
        ;;
    storage-reconcile)
        [[ -n "${input}" && -n "${output}" ]] || usage
        storage_reconcile "${input}" "${output}"
        ;;
    disk-precheck)
        [[ -n "${input}" && -n "${output}" ]] || usage
        disk_precheck "${input}" "${output}"
        ;;
    build)
        [[ -n "${source_sha}" && -n "${archive}" && -n "${evidence_dir}" && -n "${image_ref}" ]] || usage
        build_archive "${source_sha}" "${archive}" "${evidence_dir}" "${image_ref}"
        ;;
    seal)
        [[ -n "${input}" && -n "${output}" ]] || usage
        seal_tuple "${input}" "${output}"
        ;;
    seal-tree)
        [[ -n "${root}" && -n "${manifest}" && -n "${transcript}" ]] || usage
        seal_tree "${root}" "${manifest}" "${transcript}"
        ;;
    materialize-model-links)
        [[ -n "${root}" ]] || usage
        materialize_model_links "${root}"
        ;;
    verify-tree)
        [[ -n "${root}" && -n "${input}" && -n "${transcript}" ]] || usage
        verify_tree "${root}" "${input}" "${transcript}"
        ;;
    verify-scan-preupload)
        [[ -n "${tuple}" ]] || usage
        verify_scan_preupload "${tuple}"
        ;;
    verify-scan-postupload)
        [[ -n "${tuple}" ]] || usage
        verify_scan_postupload "${tuple}"
        ;;
    verify-handoff)
        [[ -n "${input}" ]] || usage
        verify_handoff "${input}"
        ;;
    fetch-verify)
        [[ -n "${tuple}" && -n "${request}" && -n "${output}" ]] || usage
        fetch_verify "${tuple}" "${request}" "${output}"
        ;;
    verify-custodian-runs)
        [[ -n "${input}" && -n "${request}" && -n "${output}" ]] || usage
        verify_custodian_runs "${input}" "${request}" "${output}"
        ;;
    *) usage ;;
esac
