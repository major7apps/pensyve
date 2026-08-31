#!/usr/bin/env bash

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly GUARD="${SCRIPT_DIR}/guard-active-service.py"
readonly REGION="us-east-2"
readonly CLUSTER="pensyve-prod"
readonly SERVICE="pensyve-prod-gateway"
readonly CONTAINER="gateway"
readonly AWS_BIN="${AWS_BIN:-aws}"
readonly SOURCE_ACCOUNT="196881464893"
readonly SOURCE_TAG="63011d55f8cbf52f6f9e5609621f6b8cf0c37535"
readonly SOURCE_DIGEST="sha256:6f5f36741bc4c5d39455b2f2fd41108561ea6ea28d438f815462b9febe3e329b"

die() {
    echo "gateway promotion error: $*" >&2
    exit 1
}

usage() {
    echo "usage: promote-gateway-image.sh {task8-create|task8-rollback|task9-promote|task9-rollback} --custody FILE" >&2
    exit 2
}

MODE="${1:-}"
[[ "${MODE}" =~ ^(task8-create|task8-rollback|task9-promote|task9-rollback)$ ]] || usage
shift
CUSTODY=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --custody) [[ -n "${2:-}" ]] || usage; CUSTODY="$2"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -f "${CUSTODY}" ]] || die "canonical custody JSON is required"

TEMP_ROOT="$(mktemp -d /tmp/pensyve-gateway-promote.XXXXXX)" \
    || die "could not create promotion temporary root"
TARGET_GROUP_ARN=""
RETURN_ARN=""
RETURN_MODE=""
MUTATION_ARMED=0
ROLLBACK_STARTED=0

validate_custody() {
    python3 - "${CUSTODY}" <<'PY'
import json
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
raw = path.read_bytes()
try:
    data = json.loads(raw)
except (json.JSONDecodeError, UnicodeDecodeError) as error:
    raise SystemExit(f"gateway promotion error: custody JSON is invalid: {error}")
canonical = (json.dumps(data, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode()
if raw != canonical:
    raise SystemExit("gateway promotion error: custody JSON is not canonical")
if set(data) != {"source", "image", "evidence", "publisher"}:
    raise SystemExit("gateway promotion error: custody top-level shape mismatch")
source = data.get("source")
image = data.get("image")
evidence = data.get("evidence")
publisher = data.get("publisher")
if not all(isinstance(value, dict) for value in (source, image, evidence, publisher)):
    raise SystemExit("gateway promotion error: custody values must be objects")
hex40 = re.compile(r"[0-9a-f]{40}")
hex64 = re.compile(r"[0-9a-f]{64}")
if (set(source) != {"schema_version", "repository", "sha", "tree"} or
        type(source.get("schema_version")) is not int or source.get("schema_version") != 1 or
        source.get("repository") != "major7apps/pensyve" or
        not hex40.fullmatch(str(source.get("sha", ""))) or
        not hex40.fullmatch(str(source.get("tree", "")))):
    raise SystemExit("gateway promotion error: custody source mismatch")
expected_image = {"account", "registry", "repository", "manifest_digest", "config_digest",
                  "platform", "raw_manifest_media_type", "raw_manifest_sha256"}
if set(image) != expected_image:
    raise SystemExit("gateway promotion error: custody image shape mismatch")
account = str(image.get("account", ""))
if (account != "196881464893" or
        image.get("registry") != f"{account}.dkr.ecr.us-east-2.amazonaws.com" or
        image.get("repository") != "pensyve-gateway" or image.get("platform") != "linux/arm64" or
        image.get("raw_manifest_media_type") !=
        "application/vnd.docker.distribution.manifest.v2+json" or
        not re.fullmatch(r"sha256:[0-9a-f]{64}", str(image.get("manifest_digest", ""))) or
        not re.fullmatch(r"sha256:[0-9a-f]{64}", str(image.get("config_digest", ""))) or
        not hex64.fullmatch(str(image.get("raw_manifest_sha256", "")))):
    raise SystemExit("gateway promotion error: custody image identity mismatch")
if (set(evidence) != {"archive_sha256", "evidence_tree_sha256", "scan_report_sha256",
                     "scan_policy_sha256", "gate_summary_sha256"} or
        not all(hex64.fullmatch(str(value)) for value in evidence.values())):
    raise SystemExit("gateway promotion error: custody evidence mismatch")
expected_arn = f"arn:aws:sts::{account}:federated-user/pensyve-gateway-{source['sha']}"
if (set(publisher) != {"arn", "inline_session_policy_sha256"} or
        publisher.get("arn") != expected_arn or
        not hex64.fullmatch(str(publisher.get("inline_session_policy_sha256", "")))):
    raise SystemExit("gateway promotion error: custody publisher mismatch")
PY
}

aws_call() {
    "${AWS_BIN}" "$@" --cli-connect-timeout 5 --cli-read-timeout 30
}

source_arn() {
    printf 'arn:aws:ecs:us-east-2:%s:task-definition/pensyve-prod-gateway:156\n' \
        "${SOURCE_ACCOUNT}"
}

verify_source_image() {
    aws_call ecr describe-images --region "${REGION}" --registry-id "${SOURCE_ACCOUNT}" \
        --repository-name pensyve-gateway --image-ids "imageTag=${SOURCE_TAG}" \
        --output json > "${TEMP_ROOT}/source-image.json"
    python3 - "${TEMP_ROOT}/source-image.json" "${SOURCE_TAG}" "${SOURCE_DIGEST}" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
tag, digest = sys.argv[2:]
details = payload.get("imageDetails")
if not isinstance(details, list) or len(details) != 1 or not isinstance(details[0], dict):
    raise SystemExit("gateway promotion error: source tag lookup must return exactly one image detail")
detail = details[0]
if detail.get("imageDigest") != digest:
    raise SystemExit("gateway promotion error: source image tag resolved to unexpected digest")
tags = detail.get("imageTags")
if not isinstance(tags, list) or tag not in tags or not all(isinstance(item, str) for item in tags):
    raise SystemExit("gateway promotion error: source image detail does not contain exact reviewed tag")
PY
}

normalize_task() {
    python3 - "$1" "$2" <<'PY'
import json
import sys
from pathlib import Path

source = json.loads(Path(sys.argv[1]).read_text())
task = source.get("taskDefinition", source)
for name in ("taskDefinitionArn", "revision", "status", "requiresAttributes", "compatibilities",
             "registeredAt", "registeredBy", "deregisteredAt"):
    task.pop(name, None)
Path(sys.argv[2]).write_text(json.dumps(task, sort_keys=True, separators=(",", ":"),
                                             allow_nan=False) + "\n")
PY
}

derive_task8() {
    local source_response="$1" output="$2"
    normalize_task "${source_response}" "${output}"
    python3 - "${output}" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
task = json.loads(path.read_text())
task["cpu"] = "512"
task["memory"] = "4096"
path.write_text(json.dumps(task, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
PY
}

derive_task9() {
    local baseline_response="$1" output="$2"
    normalize_task "${baseline_response}" "${output}"
    python3 - "${output}" "${CUSTODY}" "${CONTAINER}" <<'PY'
import json
import sys
from pathlib import Path

path, custody_path = map(Path, sys.argv[1:3])
container_name = sys.argv[3]
task = json.loads(path.read_text())
custody = json.loads(custody_path.read_text())
gateways = [item for item in task.get("containerDefinitions", [])
            if item.get("name") == container_name]
if len(gateways) != 1:
    raise SystemExit("gateway promotion error: baseline must contain one gateway container")
image = custody["image"]
gateways[0]["image"] = f"{image['registry']}/{image['repository']}@{image['manifest_digest']}"
path.write_text(json.dumps(task, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
PY
}

register_and_compare() {
    local expected="$1"
    verify_source_image || return 1
    aws_call ecs register-task-definition --region "${REGION}" \
        --cli-input-json "file://${expected}" --output json > "${TEMP_ROOT}/register.json"
    local new_arn
    new_arn="$(jq -r '.taskDefinition.taskDefinitionArn' "${TEMP_ROOT}/register.json")"
    [[ "${new_arn}" =~ ^arn:aws:ecs:us-east-2:[0-9]{12}:task-definition/pensyve-prod-gateway:[0-9]+$ ]] \
        || die "registered task definition ARN is invalid"
    [[ "${new_arn##*:}" -ne 157 ]] || die "rejected task definition revision"
    aws_call ecs describe-task-definition --region "${REGION}" \
        --task-definition "${new_arn}" --output json > "${TEMP_ROOT}/registered.json"
    normalize_task "${TEMP_ROOT}/registered.json" "${TEMP_ROOT}/registered-normalized.json"
    cmp -s "${expected}" "${TEMP_ROOT}/registered-normalized.json" \
        || die "registered task definition differs from exact derivation"
    printf '%s\n' "${new_arn}"
}

update_once() {
    local target="$1"
    aws_call ecs update-service --region "${REGION}" --cluster "${CLUSTER}" \
        --service "${SERVICE}" --task-definition "${target}" --output json >/dev/null
}

wait_stable() {
    aws_call ecs wait services-stable --region "${REGION}" --cluster "${CLUSTER}" \
        --services "${SERVICE}"
    [[ -n "${TARGET_GROUP_ARN}" ]] || die "production target group is not bound"
    aws_call ecs describe-services --region "${REGION}" --cluster "${CLUSTER}" \
        --services "${SERVICE}" --output json > "${TEMP_ROOT}/stable-service.json"
    aws_call elbv2 describe-target-health --region "${REGION}" \
        --target-group-arn "${TARGET_GROUP_ARN}" --output json > "${TEMP_ROOT}/targets.json"
    python3 - "${TEMP_ROOT}/stable-service.json" "${TEMP_ROOT}/targets.json" <<'PY'
import json
import sys
from pathlib import Path

service_payload = json.loads(Path(sys.argv[1]).read_text())
if not isinstance(service_payload, dict) or service_payload.get("failures") not in (None, []):
    raise SystemExit("gateway promotion error: stable ECS service read returned failures")
services = service_payload.get("services")
if not isinstance(services, list) or len(services) != 1 or not isinstance(services[0], dict):
    raise SystemExit("gateway promotion error: stable ECS service read must return one service")
service = services[0]
if service.get("serviceName") != "pensyve-prod-gateway" or service.get("status") != "ACTIVE":
    raise SystemExit("gateway promotion error: stable ECS service identity/status mismatch")
desired = service.get("desiredCount")
running = service.get("runningCount")
pending = service.get("pendingCount")
if (type(desired) is not int or not 2 <= desired <= 4 or
        type(running) is not int or running != desired or
        type(pending) is not int or pending != 0):
    raise SystemExit("gateway promotion error: stable ECS counts are outside the steady 2..4 envelope")

target_payload = json.loads(Path(sys.argv[2]).read_text())
descriptions = target_payload.get("TargetHealthDescriptions")
if not isinstance(descriptions, list) or len(descriptions) != desired:
    raise SystemExit("gateway promotion error: ALB target count must equal steady ECS desiredCount")
identities = []
for description in descriptions:
    target = description.get("Target", {}) if isinstance(description, dict) else {}
    health = description.get("TargetHealth", {}) if isinstance(description, dict) else {}
    identity = target.get("Id")
    if (not isinstance(identity, str) or not identity or target.get("Port") != 3000 or
            type(target.get("Port")) is not int or health.get("State") != "healthy"):
        raise SystemExit("gateway promotion error: ALB target is not healthy on gateway port 3000")
    identities.append(identity)
if len(set(identities)) != len(identities):
    raise SystemExit("gateway promotion error: ALB healthy target identities must be distinct")
PY
}

rollback_once() {
    [[ "${ROLLBACK_STARTED}" -eq 0 ]] || return 1
    ROLLBACK_STARTED=1
    verify_source_image || return 1
    update_once "${RETURN_ARN}" || return 1
    wait_stable || return 1
    "${GUARD}" "${RETURN_MODE}" --expected-current-arn "${RETURN_ARN}" \
        --expected-target-group-arn "${TARGET_GROUP_ARN}" >/dev/null || return 1
}

on_exit() {
    local status=$?
    trap - EXIT
    trap '' INT TERM
    if [[ "${MUTATION_ARMED}" -eq 1 ]]; then
        if rollback_once; then
            echo "gateway promotion error: automatic rollback verified; rolled back once to ${RETURN_ARN}" >&2
        else
            echo "gateway promotion error: automatic rollback failed verification; service state is not accepted" >&2
            status=1
        fi
    fi
    rm -rf -- "${TEMP_ROOT}"
    exit "${status}"
}

on_signal() {
    local signal="$1" status="$2"
    echo "gateway promotion error: received ${signal} after mutation" >&2
    exit "${status}"
}

bind_target_group() {
    local mode="$1"
    shift
    "${GUARD}" "${mode}" "$@" --target-group-output "${TEMP_ROOT}/target-group.txt"
    IFS= read -r TARGET_GROUP_ARN < "${TEMP_ROOT}/target-group.txt"
    [[ "${TARGET_GROUP_ARN}" =~ ^arn:aws:elasticloadbalancing:us-east-2:[0-9]{12}:targetgroup/pensyve-prod-gw-tg/[0-9a-f]{16}$ ]] \
        || die "guard returned an invalid production target group ARN"
}

trap on_exit EXIT
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM

validate_custody
SOURCE_ARN="$(source_arn)"

case "${MODE}" in
    task8-create)
        bind_target_group source-156 >/dev/null
        aws_call ecs describe-task-definition --region "${REGION}" \
            --task-definition "${SOURCE_ARN}" --output json > "${TEMP_ROOT}/source.json"
        derive_task8 "${TEMP_ROOT}/source.json" "${TEMP_ROOT}/task8.json"
        NEW_ARN="$(register_and_compare "${TEMP_ROOT}/task8.json")"
        "${GUARD}" source-156 --expected-current-arn "${SOURCE_ARN}" \
            --expected-target-group-arn "${TARGET_GROUP_ARN}" >/dev/null
        RETURN_ARN="${SOURCE_ARN}"
        RETURN_MODE="source-156"
        verify_source_image
        MUTATION_ARMED=1
        update_once "${NEW_ARN}"
        wait_stable
        "${GUARD}" task8-baseline --expected-current-arn "${NEW_ARN}" \
            --expected-target-group-arn "${TARGET_GROUP_ARN}" >/dev/null
        MUTATION_ARMED=0
        printf 'PENSYVE_TASK8_BASELINE_ARN=%s\n' "${NEW_ARN}"
        ;;
    task8-rollback)
        bind_target_group task8-baseline > "${TEMP_ROOT}/current-arn.txt"
        CURRENT_ARN="$(<"${TEMP_ROOT}/current-arn.txt")"
        verify_source_image
        update_once "${SOURCE_ARN}"
        wait_stable || die "Task 8 rollback did not stabilize"
        "${GUARD}" source-156 --expected-current-arn "${SOURCE_ARN}" \
            --expected-target-group-arn "${TARGET_GROUP_ARN}" >/dev/null
        printf 'rolled_back_to=%s\n' "${SOURCE_ARN}"
        ;;
    task9-promote)
        BASELINE_ARN="${PENSYVE_TASK8_BASELINE_ARN:-}"
        [[ -n "${BASELINE_ARN}" ]] || die "PENSYVE_TASK8_BASELINE_ARN is required"
        bind_target_group task8-baseline --expected-current-arn "${BASELINE_ARN}" >/dev/null
        aws_call ecs describe-task-definition --region "${REGION}" \
            --task-definition "${BASELINE_ARN}" --output json > "${TEMP_ROOT}/baseline.json"
        derive_task9 "${TEMP_ROOT}/baseline.json" "${TEMP_ROOT}/task9.json"
        NEW_ARN="$(register_and_compare "${TEMP_ROOT}/task9.json")"
        "${GUARD}" task8-baseline --expected-current-arn "${BASELINE_ARN}" \
            --expected-target-group-arn "${TARGET_GROUP_ARN}" >/dev/null
        RETURN_ARN="${BASELINE_ARN}"
        RETURN_MODE="task8-baseline"
        verify_source_image
        MUTATION_ARMED=1
        update_once "${NEW_ARN}"
        wait_stable
        "${GUARD}" task9-candidate \
            --expected-current-arn "${NEW_ARN}" --baseline-arn "${BASELINE_ARN}" \
            --custody "${CUSTODY}" --expected-target-group-arn "${TARGET_GROUP_ARN}" >/dev/null
        MUTATION_ARMED=0
        printf 'PENSYVE_TASK9_CANDIDATE_ARN=%s\n' "${NEW_ARN}"
        ;;
    task9-rollback)
        BASELINE_ARN="${PENSYVE_TASK8_BASELINE_ARN:-}"
        [[ -n "${BASELINE_ARN}" ]] || die "PENSYVE_TASK8_BASELINE_ARN is required"
        bind_target_group task9-candidate --baseline-arn "${BASELINE_ARN}" \
            --custody "${CUSTODY}" > "${TEMP_ROOT}/current-arn.txt"
        CURRENT_ARN="$(<"${TEMP_ROOT}/current-arn.txt")"
        verify_source_image
        update_once "${BASELINE_ARN}"
        wait_stable || die "Task 9 rollback did not stabilize"
        "${GUARD}" task8-baseline --expected-current-arn "${BASELINE_ARN}" \
            --expected-target-group-arn "${TARGET_GROUP_ARN}" >/dev/null
        printf 'rolled_back_to=%s\n' "${BASELINE_ARN}"
        ;;
esac
