#!/usr/bin/env bash

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly ARTIFACT_SCRIPT="${SCRIPT_DIR}/gateway-image-artifact.sh"
readonly PROMOTE_SCRIPT="${SCRIPT_DIR}/promote-gateway-image.sh"
readonly GUARD_SCRIPT="${SCRIPT_DIR}/guard-active-service.py"
readonly DEPLOY_WORKFLOW="${REPO_ROOT}/.github/workflows/deploy-gateway.yml"
readonly CI_WORKFLOW="${REPO_ROOT}/.github/workflows/ci.yml"
readonly CASE="${1:-all}"
export PENSYVE_PROMOTION_STABILIZATION_ATTEMPTS=1
export PENSYVE_PROMOTION_STABILIZATION_INTERVAL_SECONDS=0

case "${CASE}" in
    structural | guard | publisher | promoter | task8-rollback | rollback | workflow | mutations | all) ;;
    *) echo "usage: $0 [structural|guard|publisher|promoter|task8-rollback|rollback|workflow|mutations|all]" >&2; exit 2 ;;
esac

readonly TEST_ROOT="$(mktemp -d /tmp/pensyve-local-custody.XXXXXX)"
trap 'rm -rf -- "${TEST_ROOT}"' EXIT

fail() {
    echo "gateway custody contract failure: $*" >&2
    exit 1
}

expect_failure() {
    local expected="$1"
    shift
    local output="${TEST_ROOT}/expected-failure.$RANDOM.log"
    if "$@" >"${output}" 2>&1; then
        fail "mutation unexpectedly passed: $*"
    fi
    grep -F -- "${expected}" "${output}" >/dev/null || {
        cat "${output}" >&2
        fail "failure did not name expected boundary '${expected}': $*"
    }
}

validate_contract() {
    python3 - "$1" "$2" "$3" "$4" <<'PY'
import json
import re
import sys
from pathlib import Path

import yaml

deploy_path, ci_path, artifact_path, promote_path = map(Path, sys.argv[1:])
deploy = yaml.load(deploy_path.read_text(), Loader=yaml.BaseLoader)
ci = yaml.load(ci_path.read_text(), Loader=yaml.BaseLoader)
artifact = artifact_path.read_text()
promote = promote_path.read_text()
errors = []

def mapping(value):
    return value if isinstance(value, dict) else {}

def steps(job):
    return [mapping(step) for step in mapping(job).get("steps", [])]

def step_text(job):
    return "\n".join(str(value) for step in steps(job) for value in step.values())

def require(condition, message):
    if not condition:
        errors.append(message)

build_archive = re.search(r"\nbuild_archive\(\) \{(?P<body>.*?)\n\}\n", artifact, re.S)
build_result = re.search(
    r'jq -n (?P<command>.*?) > "\$\{evidence_dir\}/build-result\.json"',
    build_archive.group("body") if build_archive else "", re.S,
)
require(build_result is not None and '--arg image_ref "${image_ref}"' in build_result.group("command"),
        "build-result jq must bind image_ref")

on = mapping(deploy.get("on"))
require(set(on) == {"workflow_dispatch"}, "deploy workflow must be manual-only")
inputs = mapping(mapping(on.get("workflow_dispatch")).get("inputs"))
require(set(inputs) == {"operation", "custody_json"},
        "deploy workflow inputs must be operation plus custody JSON")
operation = mapping(inputs.get("operation"))
require(operation.get("required") == "true", "operation input must be required")
require(set(operation.get("options", [])) == {
    "task8-create", "task8-rollback", "task9-promote", "task9-rollback"
}, "operation input has forbidden or missing mode")
require(mapping(inputs.get("custody_json")).get("required") == "true",
        "custody JSON input must be required")
require(mapping(deploy.get("permissions")) == {"contents": "read"},
        "workflow permissions must be contents read only")
concurrency = mapping(deploy.get("concurrency"))
require(concurrency.get("group") == "pensyve-production-gateway",
        "production concurrency group must be single and fixed")
require(concurrency.get("cancel-in-progress") == "false",
        "production concurrency must not cancel in progress")

jobs = mapping(deploy.get("jobs"))
require(set(jobs) == {"preflight", "production"},
        "deploy workflow must contain only preflight and production jobs")
preflight = mapping(jobs.get("preflight"))
production = mapping(jobs.get("production"))
require(preflight.get("runs-on") == "ubuntu-latest", "preflight must run on ubuntu-latest")
require(mapping(preflight.get("permissions")) == {"contents": "read"},
        "preflight permissions must be contents read only")
require("environment" not in preflight, "preflight must not have a production environment")
require(set(mapping(preflight.get("outputs"))) == {
    "custody_sha256", "source_sha", "source_tree", "manifest_digest", "account"
}, "preflight outputs must include the custody account")
preflight_text = step_text(preflight)
require("id-token" not in str(preflight), "preflight must not have OIDC")
for token in ("aws ", "\ndocker ", "actions/checkout", "actions/upload-artifact",
              "actions/download-artifact"):
    require(token not in preflight_text.lower(),
            f"preflight contains forbidden authority: {token.strip()}")
for token in (
    "refs/heads/main", "github.sha", "source.sha", 'source.get("tree"', "commits/main",
    "major7apps/pensyve", "sort_keys=True", "separators=(\",\", \":\")",
    "allow_nan=False", "custody_sha256", "GITHUB_OUTPUT", "GITHUB_STEP_SUMMARY",
    "account=",
):
    require(token in preflight_text,
            f"preflight is missing required current-main/canonical binding: {token}")

require(production.get("needs") == "preflight",
        "production must depend only on successful preflight")
require(production.get("environment") == "production",
        "production must use the production environment")
require(mapping(production.get("permissions")) == {"contents": "read", "id-token": "write"},
        "production permissions must be contents read plus OIDC")
production_steps = steps(production)
production_text = step_text(production)
credential_indexes = [index for index, step in enumerate(production_steps)
                      if str(step.get("uses", "")).startswith(
                          "aws-actions/configure-aws-credentials@")]
require(len(credential_indexes) == 1,
        "production must configure AWS credentials exactly once")
credential_index = credential_indexes[0] if credential_indexes else 10**6
require(any(str(step.get("uses", "")) ==
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
            and mapping(step.get("with")).get("ref") == "${{ github.sha }}"
            and mapping(step.get("with")).get("persist-credentials") == "false"
            for step in production_steps[:credential_index]),
        "production must check out only exact github.sha without credentials before AWS")
for token in ("PENSYVE_REVIEWED_CUSTODY_SHA256",
              "needs.preflight.outputs.custody_sha256", "commits/main",
              "refs/heads/main", "PENSYVE_TASK8_BASELINE_ARN"):
    require(token in production_text, f"production pre-OIDC binding is missing: {token}")
credential_step = production_steps[credential_index] if credential_indexes else {}
require(credential_step.get("uses") ==
        "aws-actions/configure-aws-credentials@e6de054238d6b7531b4efff3b6587d9aade6a06c",
        "AWS credential action must use the existing reviewed pin")
policy = str(mapping(credential_step.get("with")).get("inline-session-policy", ""))
require(mapping(credential_step.get("with")).get("allowed-account-ids") ==
        "${{ needs.preflight.outputs.account }}",
        "OIDC must allow only the custody account")
for allowed in ("ecr:DescribeImages", "ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer",
                "ecs:DescribeServices", "ecs:DescribeTaskDefinition",
                "ecs:RegisterTaskDefinition", "ecs:UpdateService", "iam:PassRole",
                "elasticloadbalancing:DescribeTargetHealth"):
    require(allowed in policy, f"production inline policy is missing {allowed}")
require("needs.preflight.outputs.account" in policy and "secrets.AWS_ACCOUNT_ID" not in policy,
        "production policy must be scoped only by the custody account output")
require("targetgroup/pensyve-prod-gw-tg/*" in policy,
        "target-health authority must be limited to the production gateway target group")
try:
    policy_document = json.loads(policy)
except json.JSONDecodeError:
    policy_document = {}
pass_role = [statement for statement in mapping(policy_document).get("Statement", [])
             if isinstance(statement, dict) and statement.get("Action") == "iam:PassRole"]
expected_pass_roles = [
    "arn:aws:iam::${{ needs.preflight.outputs.account }}:role/pensyve-prod-task",
    "arn:aws:iam::${{ needs.preflight.outputs.account }}:role/pensyve-prod-task-execution",
]
require(len(pass_role) == 1 and pass_role[0].get("Resource") == expected_pass_roles,
        "production PassRole resources must be the exact live project roles")
require("pensyve-prod-gateway-task" not in policy and
        "pensyve-prod-gateway-execution" not in policy,
        "production policy retains invented gateway-specific role names")
for forbidden in ("ecr:PutImage", "ecr:BatchDeleteImage", "ecr:InitiateLayerUpload",
                  "secretsmanager:", "ssm:", '"Action":"*"'):
    require(forbidden.lower() not in policy.lower(),
            f"production inline policy contains forbidden authority: {forbidden}")
for token in ("\ndocker ", "upload-artifact", "download-artifact", "buildx", "cargo test",
              "test-gateway-release-image"):
    require(token not in production_text.lower(),
            f"production workflow contains forbidden heavy path: {token}")
for token in ("batch-get-image", "get-download-url-for-layer", "raw manifest",
              "config blob", "linux/arm64", "org.opencontainers.image.revision",
              "sts get-caller-identity"):
    require(token in production_text.lower(),
            f"production ECR verification is missing: {token}")
require(production_text.count("--registry-id") == 3,
        "every production ECR call must use the custody --registry-id")
require("promote-gateway-image.sh" in production_text and "OPERATION" in production_text,
        "production must invoke only the selected promoter mode")

ci_jobs = mapping(ci.get("jobs"))
for forbidden in ("test-rust-models", "paraphrase-recall-gate",
                  "test-rust-no-network-invariants"):
    require(forbidden not in ci_jobs,
            f"CI still contains forbidden remote-heavy job: {forbidden}")
for required in ("lint", "test-rust", "test-rust-postgres", "test-python",
                 "test-typescript", "test-go", "test-wasm", "test-gateway",
                 "test-gateway-artifact-source"):
    require(required in ci_jobs,
            f"CI lost required ordinary/source-contract job: {required}")
source_job_text = step_text(mapping(ci_jobs.get("test-gateway-artifact-source"))).lower()
for token in ("--ignored", "docker build", "docker load", "docker push", "huggingface",
              "fastembed_cache"):
    require(token not in source_job_text,
            f"source-contract job contains forbidden heavy path: {token}")

publish_match = re.search(r"\npublish_ecr\(\) \{(?P<body>.*?)\n\}\n", artifact, re.S)
require(publish_match is not None,
        "artifact script is missing sole publish-ecr implementation")
publish = publish_match.group("body") if publish_match else ""
for token in ("get-caller-identity", "load --input", " tag ", " push ",
              "describe-images", "batch-get-image", "get-download-url-for-layer",
              "DOCKER_CONFIG", "federated-user", "inline-session-policy", "os",
              "architecture", "org.opencontainers.image.revision"):
    require(token in publish, f"publish-ecr is missing required custody behavior: {token}")
for forbidden in ("ecs ", " iam ", "ssm ", "secretsmanager", "batch-delete-image",
                  "create-access-key", "get-federation-token"):
    require(forbidden not in publish.lower(),
            f"publisher overlaps forbidden authority: {forbidden.strip()}")
require(artifact.count("publish-ecr)") == 1,
        "publish-ecr must be the sole local publisher mode")
require(publish.count('"${docker_bin}" push') == 1,
        "publish-ecr must contain exactly one candidate push")
require("artifact-custodian" not in artifact and "fetch-verify" not in artifact,
        "artifact script retains remote custodian/handoff modes")

for forbidden in ("\ndocker ", "buildx", "ecr put-image", "ecr initiate-layer-upload",
                  "ecr upload-layer-part", "ecr complete-layer-upload",
                  "ecr batch-delete-image"):
    require(forbidden not in promote.lower(),
            f"promoter contains forbidden Docker/ECR-write path: {forbidden}")
for mode in ("task8-create", "task8-rollback", "task9-promote", "task9-rollback"):
    require(mode in promote, f"promoter is missing mode {mode}")
for forbidden in ("desired-count", "force-new-deployment", "application-autoscaling",
                  "register-scalable-target", "put-scaling-policy", "latest", ":157"):
    require(forbidden not in promote,
            f"promoter contains forbidden selector/override: {forbidden}")
for required in ("guard-active-service.py", "register-task-definition",
                 "describe-task-definition", "update-service", "services-stable",
                 "PENSYVE_TASK8_BASELINE_ARN", "196881464893", "imageTag=",
                 "63011d55f8cbf52f6f9e5609621f6b8cf0c37535",
                 "sha256:6f5f36741bc4c5d39455b2f2fd41108561ea6ea28d438f815462b9febe3e329b"):
    require(required in promote, f"promoter is missing required safe operation: {required}")
require(promote.count("register-task-definition") == 1,
        "promoter must have one registration call site")
require(promote.count("update-service") == 1,
        "promoter must have one service-update call site")
for function in ("derive_task8", "derive_task9"):
    match = re.search(rf"\n{function}\(\) \{{(?P<body>.*?)\n\}}\n", promote, re.S)
    require(match is not None, f"promoter is missing {function}")
    if match is not None:
        require("environment" not in match.group("body"),
                f"{function} must not repair or mutate environment")

if errors:
    print("\n".join(errors), file=sys.stderr)
    raise SystemExit(1)
PY
}

run_structural() {
    validate_contract "${DEPLOY_WORKFLOW}" "${CI_WORKFLOW}" \
        "${ARTIFACT_SCRIPT}" "${PROMOTE_SCRIPT}"
}

make_guard_fixture() {
    local root="$1"
    mkdir -p "${root}/bin"
    python3 - "${root}" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
account = "196881464893"
prefix = f"arn:aws:ecs:us-east-2:{account}:task-definition/pensyve-prod-gateway:"
image = (
    f"{account}.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway:"
    "63011d55f8cbf52f6f9e5609621f6b8cf0c37535"
)
target_group = (
    f"arn:aws:elasticloadbalancing:us-east-2:{account}:"
    "targetgroup/pensyve-prod-gw-tg/0123456789abcdef"
)
base = {
    "taskDefinitionArn": prefix + "156", "family": "pensyve-prod-gateway", "revision": 156,
    "cpu": "256", "memory": "512", "networkMode": "awsvpc",
    "taskRoleArn": f"arn:aws:iam::{account}:role/pensyve-prod-task",
    "executionRoleArn": f"arn:aws:iam::{account}:role/pensyve-prod-task-execution",
    "requiresCompatibilities": ["FARGATE"],
    "runtimePlatform": {"cpuArchitecture": "ARM64", "operatingSystemFamily": "LINUX"},
    "containerDefinitions": [{"name": "gateway", "image": image, "environment": [
        {"name": "ENVIRONMENT", "value": "production"},
        {"name": "HOST", "value": "0.0.0.0"},
        {"name": "MCP_ALLOWED_HOSTS", "value": "mcp.pensyve.com"},
        {"name": "PENSYVE_ALLOW_MOCK_EMBEDDER", "value": "1"},
        {"name": "PENSYVE_NAMESPACE", "value": "default"},
        {"name": "PENSYVE_PATH", "value": "/data/pensyve"},
        {"name": "PENSYVE_SNAPSHOT_DIR", "value": "/mnt/snapshots"},
        {"name": "PENSYVE_VALIDATION_URL", "value": "https://pensyve.com/api/auth/validate-key"},
        {"name": "PORT", "value": "3000"},
        {"name": "REDIS_URL", "value": "rediss://pensyve-prod.example.cache.amazonaws.com:6379"},
    ], "secrets": [], "environmentFiles": []}],
}
task8 = json.loads(json.dumps(base))
task8.update(taskDefinitionArn=prefix + "200", revision=200, cpu="512", memory="4096")
candidate = json.loads(json.dumps(task8))
candidate.update(taskDefinitionArn=prefix + "201", revision=201)
candidate["containerDefinitions"][0]["image"] = (
    f"{account}.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:" + "b" * 64
)
for name, task in (("156", base), ("200", task8), ("201", candidate)):
    (root / f"task-{name}.json").write_text(json.dumps({"taskDefinition": task}))
service = {"failures": [], "services": [{
    "serviceName": "pensyve-prod-gateway", "status": "ACTIVE",
    "taskDefinition": prefix + "156", "desiredCount": 2, "runningCount": 2,
    "pendingCount": 0, "loadBalancers": [{"targetGroupArn": target_group,
        "containerName": "gateway", "containerPort": 3000}],
    "deployments": [{"status": "PRIMARY",
        "rolloutState": "COMPLETED", "taskDefinition": prefix + "156",
        "desiredCount": 2, "runningCount": 2, "pendingCount": 0}],
}]}
(root / "service.json").write_text(json.dumps(service))
service4 = json.loads(json.dumps(service))
for owner in (service4["services"][0], service4["services"][0]["deployments"][0]):
    owner["desiredCount"] = 4
    owner["runningCount"] = 4
(root / "service-4.json").write_text(json.dumps(service4))
PY
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        'set -euo pipefail' \
        'printf '\''%q '\'' "$@" >> "${AWS_LOG}"; printf '\''\n'\'' >> "${AWS_LOG}"' \
        'reject(){ echo "non-exact AWS argv: $*" >&2; exit 90; }' \
        '[[ "$1" == ecs ]] || reject "$@"' \
        'case "$2" in' \
        '  describe-services) [[ $# -eq 14 && "$*" == "ecs describe-services --region us-east-2 --cluster pensyve-prod --services pensyve-prod-gateway --cli-connect-timeout 5 --cli-read-timeout 30 --output json" ]] || reject "$@"; cat "${SERVICE_FIXTURE}" ;;' \
        '  describe-task-definition) arn=""; arguments=("$@"); while (( $# )); do if [[ "$1" == --task-definition ]]; then arn="$2"; break; fi; shift; done; [[ ${#arguments[@]} -eq 12 && "${arguments[*]}" == "ecs describe-task-definition --region us-east-2 --task-definition $arn --cli-connect-timeout 5 --cli-read-timeout 30 --output json" ]] || reject "${arguments[@]}"; cat "${TASK_FIXTURE_ROOT}/task-${arn##*:}.json" ;;' \
        '  *) reject "$@" ;;' \
        'esac' > "${root}/bin/aws"
    chmod +x "${root}/bin/aws"
}

run_guard() {
    local root="${TEST_ROOT}/guard" mutation kind
    make_guard_fixture "${root}"
    : > "${root}/aws.log"
    AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
        SERVICE_FIXTURE="${root}/service.json" TASK_FIXTURE_ROOT="${root}" \
        "${GUARD_SCRIPT}" source-156 \
        --target-group-output "${root}/target-group.txt" >/dev/null
    grep -Fx \
        "arn:aws:elasticloadbalancing:us-east-2:196881464893:targetgroup/pensyve-prod-gw-tg/0123456789abcdef" \
        "${root}/target-group.txt" >/dev/null || fail "guard did not bind the production target group"
    [[ "$(wc -l < "${root}/aws.log")" -eq 2 ]] ||
        fail "source guard must perform exactly two reads"
    python3 - "${root}/aws.log" <<'PY'
import shlex
import sys
from pathlib import Path

calls = [shlex.split(line) for line in Path(sys.argv[1]).read_text().splitlines()]
expected = [
    ["ecs", "describe-services", "--region", "us-east-2", "--cluster", "pensyve-prod",
     "--services", "pensyve-prod-gateway", "--cli-connect-timeout", "5",
     "--cli-read-timeout", "30", "--output", "json"],
    ["ecs", "describe-task-definition", "--region", "us-east-2", "--task-definition",
     "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:156",
     "--cli-connect-timeout", "5", "--cli-read-timeout", "30", "--output", "json"],
]
if calls != expected:
    raise SystemExit(f"guard complete ordered AWS sequence mismatch: {calls!r}")
PY
    AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
        SERVICE_FIXTURE="${root}/service-4.json" TASK_FIXTURE_ROOT="${root}" \
        "${GUARD_SCRIPT}" source-156 >/dev/null
    sed 's/:156"/:157"/g; s/"revision": 156/"revision": 157/' \
        "${root}/service.json" > "${root}/service-157.json"
    expect_failure "157" env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
        SERVICE_FIXTURE="${root}/service-157.json" TASK_FIXTURE_ROOT="${root}" \
        "${GUARD_SCRIPT}" source-156
    python3 - "${root}" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
base = json.loads((root / "service-4.json").read_text())

def write(name, mutate):
    data = json.loads(json.dumps(base))
    mutate(data["services"][0], data["services"][0]["deployments"][0])
    (root / f"service-{name}.json").write_text(json.dumps(data))

def counts(service, primary, desired, running, pending):
    for owner in (service, primary):
        owner["desiredCount"] = desired
        owner["runningCount"] = running
        owner["pendingCount"] = pending

write("desired-1", lambda service, primary: counts(service, primary, 1, 1, 0))
write("desired-5", lambda service, primary: counts(service, primary, 5, 5, 0))
write("desired-bool", lambda service, primary: counts(service, primary, True, True, 0))
write("desired-float", lambda service, primary: counts(service, primary, 4.0, 4.0, 0))
write("desired-string", lambda service, primary: counts(service, primary, "4", "4", 0))
write("running-mismatch", lambda service, primary: counts(service, primary, 4, 3, 0))
write("pending", lambda service, primary: counts(service, primary, 4, 4, 1))
write("deployment-count-mismatch", lambda service, primary: primary.update(runningCount=3))
write("secondary", lambda service, primary: service["deployments"].append(
    {**primary, "status": "ACTIVE"}))
write("rollout-drift", lambda service, primary: primary.update(rolloutState="IN_PROGRESS"))
PY
    for kind in desired-1 desired-5 desired-bool desired-float desired-string; do
        expect_failure "desiredCount must be an exact integer in range 2..4" env \
            AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
            SERVICE_FIXTURE="${root}/service-${kind}.json" TASK_FIXTURE_ROOT="${root}" \
            "${GUARD_SCRIPT}" source-156
    done
    expect_failure "runningCount must equal desiredCount" env AWS_BIN="${root}/bin/aws" \
        AWS_LOG="${root}/aws.log" SERVICE_FIXTURE="${root}/service-running-mismatch.json" \
        TASK_FIXTURE_ROOT="${root}" "${GUARD_SCRIPT}" source-156
    expect_failure "pendingCount must be exactly 0" env AWS_BIN="${root}/bin/aws" \
        AWS_LOG="${root}/aws.log" SERVICE_FIXTURE="${root}/service-pending.json" \
        TASK_FIXTURE_ROOT="${root}" "${GUARD_SCRIPT}" source-156
    expect_failure "PRIMARY deployment counts must equal service counts" env \
        AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
        SERVICE_FIXTURE="${root}/service-deployment-count-mismatch.json" \
        TASK_FIXTURE_ROOT="${root}" "${GUARD_SCRIPT}" source-156
    expect_failure "single deployment" env AWS_BIN="${root}/bin/aws" \
        AWS_LOG="${root}/aws.log" SERVICE_FIXTURE="${root}/service-secondary.json" \
        TASK_FIXTURE_ROOT="${root}" "${GUARD_SCRIPT}" source-156
    expect_failure "completed PRIMARY deployment" env AWS_BIN="${root}/bin/aws" \
        AWS_LOG="${root}/aws.log" SERVICE_FIXTURE="${root}/service-rollout-drift.json" \
        TASK_FIXTURE_ROOT="${root}" "${GUARD_SCRIPT}" source-156
    python3 - "${root}/service.json" "${root}/service-no-target.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
data["services"][0].pop("loadBalancers")
json.dump(data, open(sys.argv[2], "w", encoding="utf-8"))
PY
    expect_failure "loadBalancers" env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
        SERVICE_FIXTURE="${root}/service-no-target.json" TASK_FIXTURE_ROOT="${root}" \
        "${GUARD_SCRIPT}" source-156
    python3 - "${root}" <<'PY'
import json, sys
from pathlib import Path
root=Path(sys.argv[1]); service=json.loads((root/"service.json").read_text())
arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200"
service["services"][0]["taskDefinition"]=arn
service["services"][0]["deployments"][0]["taskDefinition"]=arn
(root/"service-200.json").write_text(json.dumps(service))
task=json.loads((root/"task-200.json").read_text())
task["taskDefinition"]["containerDefinitions"][0]["environment"].append(
    {"name":"UNREVIEWED_REPAIR","value":"1"})
(root/"task-200.json").write_text(json.dumps(task))
PY
    expect_failure "CPU/memory-only derivation" env AWS_BIN="${root}/bin/aws" \
        AWS_LOG="${root}/aws.log" SERVICE_FIXTURE="${root}/service-200.json" \
        TASK_FIXTURE_ROOT="${root}" "${GUARD_SCRIPT}" task8-baseline \
        --expected-current-arn "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200"

    python3 - "${root}/task-156.json" "${root}/task-156-wrong-role.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
data["taskDefinition"]["taskRoleArn"] = (
    "arn:aws:iam::196881464893:role/pensyve-prod-gateway-task"
)
json.dump(data, open(sys.argv[2], "w", encoding="utf-8"))
PY
    mkdir -p "${root}/wrong-role-root"
    cp "${root}/task-156-wrong-role.json" "${root}/wrong-role-root/task-156.json"
    expect_failure "exact production task and execution roles" env AWS_BIN="${root}/bin/aws" \
        AWS_LOG="${root}/aws.log" SERVICE_FIXTURE="${root}/service.json" \
        TASK_FIXTURE_ROOT="${root}/wrong-role-root" "${GUARD_SCRIPT}" source-156

    python3 - "${root}/task-156.json" "${root}/task-156-wrong-image.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
data["taskDefinition"]["containerDefinitions"][0]["image"] = (
    "196881464893.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:" + "a" * 64
)
json.dump(data, open(sys.argv[2], "w", encoding="utf-8"))
PY
    mkdir -p "${root}/wrong-source-image-root"
    cp "${root}/task-156-wrong-image.json" \
      "${root}/wrong-source-image-root/task-156.json"
    cp "${root}/task-200.json" "${root}/wrong-source-image-root/task-200.json"
    expect_failure "source revision 156 must use the exact reviewed source image tag" env \
        AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
        SERVICE_FIXTURE="${root}/service-200.json" \
        TASK_FIXTURE_ROOT="${root}/wrong-source-image-root" \
        "${GUARD_SCRIPT}" task8-baseline \
        --expected-current-arn \
        "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200"

    for kind in cluster service; do
        mutation="${root}/guard-wrong-${kind}.py"
        cp "${GUARD_SCRIPT}" "${mutation}"
        if [[ "${kind}" == cluster ]]; then
            sed -i 's/^            CLUSTER,$/            "wrong-cluster",/' "${mutation}"
        else
            sed -i 's/^            SERVICE,$/            "wrong-service",/' "${mutation}"
        fi
        expect_failure "non-exact AWS argv" env AWS_BIN="${root}/bin/aws" \
            AWS_LOG="${root}/aws.log" SERVICE_FIXTURE="${root}/service.json" \
            TASK_FIXTURE_ROOT="${root}" "${mutation}" source-156
    done
}

make_publisher_fixture() {
    local root="$1"
    mkdir -p "${root}/bin" "${root}/evidence"
    printf 'archive-bytes\n' > "${root}/image.tar"
    printf 'tree-evidence\n' > "${root}/evidence/tree.json"
    printf 'scan-report\n' > "${root}/evidence/scan.json"
    printf 'scan-policy\n' > "${root}/evidence/policy.json"
    printf 'gate-summary\n' > "${root}/evidence/gates.json"
    python3 - "${root}" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
sha = "0123456789abcdef0123456789abcdef01234567"
tree = "89abcdef0123456789abcdef0123456789abcdef"
media = "application/vnd.docker.distribution.manifest.v2+json"
config = {"architecture":"arm64","os":"linux","config":{"User":"1001:1001",
          "StopSignal":"SIGINT","Labels":{"org.opencontainers.image.revision":sha}}}
config_bytes = json.dumps(config, sort_keys=True, separators=(",", ":")).encode()
(root / "config.json").write_bytes(config_bytes)
config_digest = "sha256:" + hashlib.sha256(config_bytes).hexdigest()
manifest = {"schemaVersion":2,"mediaType":media,
            "config":{"mediaType":"application/vnd.docker.container.image.v1+json",
                      "size":len(config_bytes),"digest":config_digest},"layers":[]}
manifest_bytes = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
(root / "manifest.json").write_bytes(manifest_bytes)
def digest(path): return hashlib.sha256(path.read_bytes()).hexdigest()
local = {
 "schema_version":1,
 "source":{"repository":"major7apps/pensyve","sha":sha,"tree":tree},
 "image":{"archive_path":str(root/"image.tar"),"archive_sha256":digest(root/"image.tar"),
          "local_ref":f"pensyve-gateway:{sha}","config_path":str(root/"config.json"),
          "config_digest":config_digest,"platform":"linux/arm64","source_label":sha,
          "raw_manifest_path":str(root/"manifest.json"),
          "raw_manifest_sha256":digest(root/"manifest.json"),"raw_manifest_media_type":media,
          "manifest_digest":"sha256:"+digest(root/"manifest.json")},
 "evidence":{},"publisher":{"inline_session_policy_sha256":"c"*64}}
for name, filename in (("tree","tree.json"),("scan_report","scan.json"),
                       ("scan_policy","policy.json"),("gate_summary","gates.json")):
    path = root / "evidence" / filename
    local["evidence"][name+"_path"] = str(path)
    local["evidence"][name+"_sha256"] = digest(path)
(root / "tuple.json").write_text(json.dumps(local, sort_keys=True, separators=(",", ":"))+"\n")
(root / "identity.env").write_text(
    f"SOURCE_SHA={sha}\nSOURCE_TREE={tree}\nCONFIG_DIGEST={config_digest}\n"
    f"MANIFEST_DIGEST={local['image']['manifest_digest']}\n"
    f"MANIFEST_SHA={local['image']['raw_manifest_sha256']}\n")
PY
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'printf '\''%q '\'' "$@" >> "$AWS_LOG"; printf '\''\n'\'' >> "$AWS_LOG"' \
      'if [[ "$1 $2" == "sts get-caller-identity" ]]; then printf '\''%s\n'\'' "$CALLER_ARN"' \
      'elif [[ "$1 $2" == "ecr get-login-password" ]]; then printf '\''password\n'\''' \
      'elif [[ "$1 $2" == "ecr describe-images" ]]; then jq -n --arg d "${ECR_DESCRIBE_DIGEST:-$MANIFEST_DIGEST}" '\''{imageDetails:[{imageDigest:$d}]}'\''' \
      'elif [[ "$1 $2" == "ecr batch-get-image" ]]; then raw=$(<"$ECR_RAW_MANIFEST"); jq -n --arg d "$MANIFEST_DIGEST" --arg m "$MANIFEST_MEDIA" --arg raw "$raw" '\''{images:[{imageId:{imageDigest:$d},imageManifestMediaType:$m,imageManifest:$raw}],failures:[]}'\''' \
      'elif [[ "$1 $2" == "ecr get-download-url-for-layer" ]]; then printf '\''fixture://config\n'\''' \
      'else echo "unexpected aws argv: $*" >&2; exit 90; fi' > "${root}/bin/aws"
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'printf '\''%q '\'' "$@" >> "$DOCKER_LOG"; printf '\''\n'\'' >> "$DOCKER_LOG"' \
      'printf '\''%s\n'\'' "$DOCKER_CONFIG" >> "$DOCKER_CONFIG_LOG"' \
      'if [[ "$1" == login ]]; then cat >/dev/null; fi' > "${root}/bin/docker"
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'output=""; while (( $# )); do if [[ "$1" == --output ]]; then output="$2"; shift 2; else shift; fi; done' \
      'cp "$ECR_CONFIG" "$output"' > "${root}/bin/curl"
    printf '%s\n' '#!/usr/bin/env bash' 'echo arm64' > "${root}/bin/uname"
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'if [[ "$*" == *"rev-parse HEAD^{tree}"* ]]; then echo "$SOURCE_TREE"' \
      'elif [[ "$*" == *"rev-parse HEAD"* ]]; then echo "$SOURCE_SHA"' \
      'elif [[ "$*" == *"status --porcelain"* ]]; then :' \
      'else exit 90; fi' > "${root}/bin/git"
    chmod +x "${root}/bin/"*
}

assert_publisher_argv() {
    python3 - "$1" <<'PY'
import json
import shlex
import sys
from pathlib import Path

root = Path(sys.argv[1])
identity = dict(line.split("=", 1) for line in (root / "identity.env").read_text().splitlines())
sha = identity["SOURCE_SHA"]
manifest = identity["MANIFEST_DIGEST"]
config = identity["CONFIG_DIGEST"]
account = "123456789012"
registry = f"{account}.dkr.ecr.us-east-2.amazonaws.com"
remote = f"{registry}/pensyve-gateway:{sha}"
docker = [shlex.split(line) for line in (root / "docker.log").read_text().splitlines()]
expected_docker = [
    ["load", "--input", str(root / "image.tar")],
    ["tag", f"pensyve-gateway:{sha}", remote],
    ["login", "--username", "AWS", "--password-stdin", registry],
    ["push", remote],
]
if docker != expected_docker:
    raise SystemExit(f"publisher exact Docker argv mismatch: {json.dumps(docker)}")
aws = [shlex.split(line) for line in (root / "aws.log").read_text().splitlines()]
expected_aws = [
    ["sts", "get-caller-identity", "--query", "Arn", "--output", "text",
     "--cli-connect-timeout", "5", "--cli-read-timeout", "30"],
    ["ecr", "get-login-password", "--region", "us-east-2",
     "--cli-connect-timeout", "5", "--cli-read-timeout", "30"],
    ["ecr", "describe-images", "--region", "us-east-2", "--registry-id", account,
     "--repository-name", "pensyve-gateway", "--image-ids", f"imageDigest={manifest}",
     "--output", "json", "--cli-connect-timeout", "5", "--cli-read-timeout", "30"],
    ["ecr", "batch-get-image", "--region", "us-east-2", "--registry-id", account,
     "--repository-name", "pensyve-gateway", "--image-ids", f"imageDigest={manifest}",
     "--accepted-media-types", "application/vnd.docker.distribution.manifest.v2+json",
     "--output", "json", "--cli-connect-timeout", "5", "--cli-read-timeout", "30"],
    ["ecr", "get-download-url-for-layer", "--region", "us-east-2", "--registry-id", account,
     "--repository-name", "pensyve-gateway", "--layer-digest", config,
     "--query", "downloadUrl", "--output", "text", "--cli-connect-timeout", "5",
     "--cli-read-timeout", "30"],
]
if aws != expected_aws:
    raise SystemExit(f"publisher exact AWS argv mismatch: {json.dumps(aws)}")
PY
}

run_publisher() {
    local root="${TEST_ROOT}/publisher"
    make_publisher_fixture "${root}"
    set -a
    source "${root}/identity.env"
    set +a
    : > "${root}/aws.log"
    : > "${root}/docker.log"
    : > "${root}/docker-config.log"
    local expiry
    expiry="$(date -u -d '+30 minutes' '+%Y-%m-%dT%H:%M:%SZ')"
    env AWS_BIN="${root}/bin/aws" DOCKER_BIN="${root}/bin/docker" \
      CURL_BIN="${root}/bin/curl" GIT_BIN="${root}/bin/git" UNAME_BIN="${root}/bin/uname" \
      AWS_LOG="${root}/aws.log" DOCKER_LOG="${root}/docker.log" \
      DOCKER_CONFIG_LOG="${root}/docker-config.log" \
      SOURCE_SHA="${SOURCE_SHA}" SOURCE_TREE="${SOURCE_TREE}" MANIFEST_DIGEST="${MANIFEST_DIGEST}" \
      MANIFEST_MEDIA="application/vnd.docker.distribution.manifest.v2+json" \
      ECR_RAW_MANIFEST="${root}/manifest.json" ECR_CONFIG="${root}/config.json" \
      CALLER_ARN="arn:aws:sts::123456789012:federated-user/pensyve-gateway-${SOURCE_SHA}" \
      AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_SESSION_TOKEN=test \
      AWS_SESSION_EXPIRATION="${expiry}" PENSYVE_ECR_REGISTRY="123456789012.dkr.ecr.us-east-2.amazonaws.com" \
      PENSYVE_INLINE_SESSION_POLICY_SHA256="$(printf 'c%.0s' {1..64})" \
      "${ARTIFACT_SCRIPT}" publish-ecr --tuple "${root}/tuple.json" --output "${root}/custody.json" >/dev/null
    python3 - "${root}/custody.json" <<'PY'
import json, sys
from pathlib import Path
path=Path(sys.argv[1]); raw=path.read_bytes(); data=json.loads(raw)
assert raw == (json.dumps(data,sort_keys=True,separators=(",",":"),allow_nan=False)+"\n").encode()
assert set(data)=={"source","image","evidence","publisher"}
assert data["source"]["schema_version"]==1 and data["image"]["platform"]=="linux/arm64"
PY
    assert_publisher_argv "${root}"
    [[ "$(grep -c '^ecr describe-images ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^ecr batch-get-image ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^load --input ' "${root}/docker.log")" -eq 1 ]]
    [[ "$(grep -c '^push ' "${root}/docker.log")" -eq 1 ]]
    while IFS= read -r docker_config; do
        [[ ! -e "${docker_config}" ]] || fail "publisher left credential-bearing DOCKER_CONFIG behind"
    done < "${root}/docker-config.log"
    expect_failure "second ECR push" env AWS_BIN="${root}/bin/aws" DOCKER_BIN="${root}/bin/docker" \
      CURL_BIN="${root}/bin/curl" GIT_BIN="${root}/bin/git" UNAME_BIN="${root}/bin/uname" \
      AWS_LOG="${root}/aws.log" DOCKER_LOG="${root}/docker.log" SOURCE_SHA="${SOURCE_SHA}" \
      DOCKER_CONFIG_LOG="${root}/docker-config.log" \
      SOURCE_TREE="${SOURCE_TREE}" MANIFEST_DIGEST="${MANIFEST_DIGEST}" \
      MANIFEST_MEDIA="application/vnd.docker.distribution.manifest.v2+json" \
      ECR_RAW_MANIFEST="${root}/manifest.json" ECR_CONFIG="${root}/config.json" \
      CALLER_ARN="arn:aws:sts::123456789012:federated-user/pensyve-gateway-${SOURCE_SHA}" \
      AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_SESSION_TOKEN=test AWS_SESSION_EXPIRATION="${expiry}" \
      PENSYVE_ECR_REGISTRY="123456789012.dkr.ecr.us-east-2.amazonaws.com" \
      PENSYVE_INLINE_SESSION_POLICY_SHA256="$(printf 'c%.0s' {1..64})" \
      "${ARTIFACT_SCRIPT}" publish-ecr --tuple "${root}/tuple.json" --output "${root}/second.json"

    root="${TEST_ROOT}/publisher-cleanup-failure"
    make_publisher_fixture "${root}"
    set -a
    source "${root}/identity.env"
    set +a
    : > "${root}/aws.log"
    : > "${root}/docker.log"
    : > "${root}/docker-config.log"
    expect_failure "ECR describe-images identity mismatch" env AWS_BIN="${root}/bin/aws" \
      DOCKER_BIN="${root}/bin/docker" CURL_BIN="${root}/bin/curl" GIT_BIN="${root}/bin/git" \
      UNAME_BIN="${root}/bin/uname" AWS_LOG="${root}/aws.log" DOCKER_LOG="${root}/docker.log" \
      DOCKER_CONFIG_LOG="${root}/docker-config.log" SOURCE_SHA="${SOURCE_SHA}" \
      SOURCE_TREE="${SOURCE_TREE}" MANIFEST_DIGEST="${MANIFEST_DIGEST}" \
      ECR_DESCRIBE_DIGEST="sha256:$(printf 'd%.0s' {1..64})" \
      MANIFEST_MEDIA="application/vnd.docker.distribution.manifest.v2+json" \
      ECR_RAW_MANIFEST="${root}/manifest.json" ECR_CONFIG="${root}/config.json" \
      CALLER_ARN="arn:aws:sts::123456789012:federated-user/pensyve-gateway-${SOURCE_SHA}" \
      AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_SESSION_TOKEN=test \
      AWS_SESSION_EXPIRATION="${expiry}" \
      PENSYVE_ECR_REGISTRY="123456789012.dkr.ecr.us-east-2.amazonaws.com" \
      PENSYVE_INLINE_SESSION_POLICY_SHA256="$(printf 'c%.0s' {1..64})" \
      "${ARTIFACT_SCRIPT}" publish-ecr --tuple "${root}/tuple.json" --output "${root}/custody.json"
    while IFS= read -r docker_config; do
        [[ ! -e "${docker_config}" ]] || fail "failed publisher left credential-bearing DOCKER_CONFIG behind"
    done < "${root}/docker-config.log"
}

make_promoter_fixture() {
    local root="$1"
    make_guard_fixture "${root}"
    make_publisher_fixture "${root}/publisher"
    python3 - "${root}/publisher/tuple.json" "${root}/custody.json" <<'PY'
import json, sys
from pathlib import Path
local=json.loads(Path(sys.argv[1]).read_text()); image=local["image"]
account="196881464893"; registry=f"{account}.dkr.ecr.us-east-2.amazonaws.com"
record={
 "source":{"schema_version":1,**local["source"]},
 "image":{"account":account,"registry":registry,"repository":"pensyve-gateway",
          "manifest_digest":image["manifest_digest"],"config_digest":image["config_digest"],
          "platform":"linux/arm64","raw_manifest_media_type":image["raw_manifest_media_type"],
          "raw_manifest_sha256":image["raw_manifest_sha256"]},
 "evidence":{"archive_sha256":image["archive_sha256"],
             "evidence_tree_sha256":local["evidence"]["tree_sha256"],
             "scan_report_sha256":local["evidence"]["scan_report_sha256"],
             "scan_policy_sha256":local["evidence"]["scan_policy_sha256"],
             "gate_summary_sha256":local["evidence"]["gate_summary_sha256"]},
 "publisher":{"arn":f"arn:aws:sts::{account}:federated-user/pensyve-gateway-{local['source']['sha']}",
              "inline_session_policy_sha256":local["publisher"]["inline_session_policy_sha256"]}}
Path(sys.argv[2]).write_text(json.dumps(record,sort_keys=True,separators=(",",":"))+"\n")
PY
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'printf '\''%q '\'' "$@" >> "$AWS_LOG"; printf '\''\n'\'' >> "$AWS_LOG"' \
      'reject(){ echo "non-exact AWS argv: $*" >&2; exit 90; }' \
      'arg(){ local key="$1"; shift; while (( $# )); do if [[ "$1" == "$key" ]]; then echo "$2"; return; fi; shift; done; }' \
      'operation="$1 $2"' \
      'case "$operation" in' \
      ' "ecr describe-images") [[ $# -eq 16 && "$*" == "ecr describe-images --region us-east-2 --registry-id 196881464893 --repository-name pensyve-gateway --image-ids imageTag=63011d55f8cbf52f6f9e5609621f6b8cf0c37535 --output json --cli-connect-timeout 5 --cli-read-timeout 30" ]] || reject "$@"; digest="${SOURCE_ECR_DIGEST:-sha256:6f5f36741bc4c5d39455b2f2fd41108561ea6ea28d438f815462b9febe3e329b}"; tag=63011d55f8cbf52f6f9e5609621f6b8cf0c37535; case "${SOURCE_ECR_CARDINALITY:-one}" in missing) jq -n '\''{imageDetails:[]}'\'' ;; multiple) jq -n --arg d "$digest" --arg tag "$tag" '\''{imageDetails:[{imageDigest:$d,imageTags:[$tag]},{imageDigest:$d,imageTags:[$tag]}]}'\'' ;; one) jq -n --arg d "$digest" --arg tag "$tag" '\''{imageDetails:[{imageDigest:$d,imageTags:[$tag]}]}'\'' ;; *) reject "$@" ;; esac ;;' \
      ' "ecs describe-services") [[ $# -eq 14 && ( "$*" == "ecs describe-services --region us-east-2 --cluster pensyve-prod --services pensyve-prod-gateway --cli-connect-timeout 5 --cli-read-timeout 30 --output json" || "$*" == "ecs describe-services --region us-east-2 --cluster pensyve-prod --services pensyve-prod-gateway --output json --cli-connect-timeout 5 --cli-read-timeout 30" ) ]] || reject "$@"; if [[ "${SECOND_SIGNAL_PHASE:-}" == readback && -e "${ROLLBACK_UPDATE_MARKER:-/nonexistent}" && ! -e "${SECOND_SIGNAL_MARKER:-/nonexistent}" ]]; then : > "$SECOND_SIGNAL_MARKER"; promoter_pid=$(ps -o ppid= -p "$PPID" | tr -d " "); kill -s "$SECOND_SIGNAL" "$promoter_pid"; sleep 1; fi; arn=$(<"$SERVICE_STATE"); count="${SERVICE_COUNT:-2}"; jq -n --arg arn "$arn" --arg tg "$FIXTURE_TARGET_GROUP_ARN" --argjson count "$count" '\''{failures:[],services:[{serviceName:"pensyve-prod-gateway",status:"ACTIVE",taskDefinition:$arn,desiredCount:$count,runningCount:$count,pendingCount:0,loadBalancers:[{targetGroupArn:$tg,containerName:"gateway",containerPort:3000}],deployments:[{status:"PRIMARY",rolloutState:"COMPLETED",taskDefinition:$arn,desiredCount:$count,runningCount:$count,pendingCount:0}]}]}'\'' ;;' \
      ' "ecs describe-task-definition") arn=$(arg --task-definition "$@"); [[ $# -eq 12 && ( "$*" == "ecs describe-task-definition --region us-east-2 --task-definition $arn --cli-connect-timeout 5 --cli-read-timeout 30 --output json" || "$*" == "ecs describe-task-definition --region us-east-2 --task-definition $arn --output json --cli-connect-timeout 5 --cli-read-timeout 30" ) ]] || reject "$@"; cat "$TASK_FIXTURE_ROOT/task-${arn##*:}.json" ;;' \
      ' "ecs register-task-definition") input=$(arg --cli-input-json "$@"); [[ $# -eq 12 && "$*" == "ecs register-task-definition --region us-east-2 --cli-input-json $input --output json --cli-connect-timeout 5 --cli-read-timeout 30" ]] || reject "$@"; input=${input#file://}; rev=${REGISTER_REVISION:?}; arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:$rev"; jq --arg arn "$arn" --argjson rev "$rev" '\''. + {taskDefinitionArn:$arn,revision:$rev} | {taskDefinition:.}'\'' "$input" > "$TASK_FIXTURE_ROOT/task-$rev.json"; cat "$TASK_FIXTURE_ROOT/task-$rev.json" ;;' \
      ' "ecs update-service") arn=$(arg --task-definition "$@"); [[ $# -eq 16 && "$*" == "ecs update-service --region us-east-2 --cluster pensyve-prod --service pensyve-prod-gateway --task-definition $arn --output json --cli-connect-timeout 5 --cli-read-timeout 30" ]] || reject "$@"; if [[ -e "${WAIT_MARKER:-/nonexistent}" && -n "${ROLLBACK_UPDATE_MARKER:-}" ]]; then : > "$ROLLBACK_UPDATE_MARKER"; fi; printf '\''%s\n'\'' "$arn" > "$SERVICE_STATE"; jq -n --arg arn "$arn" '\''{service:{taskDefinition:$arn}}'\'' ;;' \
      ' "ecs wait") [[ $# -eq 13 && "$*" == "ecs wait services-stable --region us-east-2 --cluster pensyve-prod --services pensyve-prod-gateway --cli-connect-timeout 5 --cli-read-timeout 30" ]] || reject "$@"; if [[ -n "${SIGNAL_FIRST_WAIT:-}" && ! -e "$WAIT_MARKER" ]]; then : > "$WAIT_MARKER"; kill -s "$SIGNAL_FIRST_WAIT" "$PPID"; sleep 1; exit 55; fi; if [[ "${FAIL_FIRST_WAIT:-0}" == 1 && ! -e "$WAIT_MARKER" ]]; then : > "$WAIT_MARKER"; exit 55; fi; if [[ "${SECOND_SIGNAL_PHASE:-}" == wait && -e "${ROLLBACK_UPDATE_MARKER:-/nonexistent}" && ! -e "${SECOND_SIGNAL_MARKER:-/nonexistent}" ]]; then : > "$SECOND_SIGNAL_MARKER"; kill -s "$SECOND_SIGNAL" "$PPID"; sleep 1; fi ;;' \
      ' "elbv2 describe-target-health") [[ $# -eq 12 && "$*" == "elbv2 describe-target-health --region us-east-2 --target-group-arn $FIXTURE_TARGET_GROUP_ARN --output json --cli-connect-timeout 5 --cli-read-timeout 30" ]] || reject "$@"; if [[ -n "${TARGET_HEALTH_FIRST_FIXTURE:-}" && ! -e "${TARGET_HEALTH_FIRST_MARKER:?}" ]]; then : > "$TARGET_HEALTH_FIRST_MARKER"; cat "$TARGET_HEALTH_FIRST_FIXTURE"; elif [[ -n "${TARGET_HEALTH_FIXTURE:-}" ]]; then cat "$TARGET_HEALTH_FIXTURE"; else jq -n --arg state "${TARGET_HEALTH_STATE:-healthy}" '\''{TargetHealthDescriptions:[{Target:{Id:"10.0.1.10",Port:3000},TargetHealth:{State:$state}},{Target:{Id:"10.0.2.10",Port:3000},TargetHealth:{State:$state}}]}'\''; fi ;;' \
      ' *) reject "$@" ;; esac' > "${root}/bin/aws"
    chmod +x "${root}/bin/aws"
}

make_target_health_fixtures() {
    python3 - "$1" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])

def target(identity, port=3000, state="healthy"):
    return {"Target": {"Id": identity, "Port": port}, "TargetHealth": {"State": state}}

healthy = [target(f"10.0.{index}.10") for index in range(1, 6)]
fixtures = {
    "targets-1": healthy[:1],
    "targets-2": healthy[:2],
    "targets-2-draining": [healthy[0], target("10.0.2.10", state="draining")],
    "targets-4": healthy[:4],
    "targets-4-initial": [*healthy[:3], target("10.0.4.10", state="initial")],
    "targets-4-draining": [*healthy[:2], target("10.0.3.10", state="draining"),
                           target("10.0.4.10", state="draining")],
    "targets-5": healthy,
    "targets-unhealthy": [*healthy[:3], target("10.0.4.10", state="unhealthy")],
    "targets-unused": [*healthy[:3], target("10.0.4.10", state="unused")],
    "targets-unavailable": [*healthy[:3], target("10.0.4.10", state="unavailable")],
    "targets-wrong-port": [*healthy[:3], target("10.0.4.10", port=3001)],
    "targets-duplicate": [*healthy[:3], target("10.0.3.10")],
    "targets-missing": [*healthy[:3], {"Target": {"Port": 3000},
                                       "TargetHealth": {"State": "healthy"}}],
    "targets-malformed": [*healthy[:3], target("10.0.4.10", port="3000")],
}
for name, descriptions in fixtures.items():
    (root / f"{name}.json").write_text(json.dumps({"TargetHealthDescriptions": descriptions}))
(root / "targets-top-missing.json").write_text("{}")
(root / "targets-top-null.json").write_text('{"TargetHealthDescriptions":null}')
(root / "targets-top-non-list.json").write_text('{"TargetHealthDescriptions":{}}')
short = {
    "malformed": [healthy[0], "malformed"],
    "wrong-port": [healthy[0], target("10.0.2.10", port=3001)],
    "bad-id": [healthy[0], target(42)],
    "bad-state": [healthy[0], target("10.0.2.10", state=True)],
    "duplicate": [healthy[0], target("10.0.1.10")],
    "unhealthy": [healthy[0], target("10.0.2.10", state="unhealthy")],
}
for name, descriptions in short.items():
    (root / f"targets-short-{name}.json").write_text(
        json.dumps({"TargetHealthDescriptions": descriptions}))
PY
}

assert_promoter_argv() {
    python3 - "$1" "$2" "$3" "$4" <<'PY'
import json
import re
import shlex
import sys
from pathlib import Path

root, expected_registers, targets, expected_health = Path(sys.argv[1]), int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
target_group = "arn:aws:elasticloadbalancing:us-east-2:196881464893:targetgroup/pensyve-prod-gw-tg/0123456789abcdef"
prefix = "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:"
calls = [shlex.split(line) for line in (root / "aws.log").read_text().splitlines()]
registers = [call for call in calls if call[:2] == ["ecs", "register-task-definition"]]
if len(registers) != expected_registers:
    raise SystemExit(f"promoter register exact argv count mismatch: {json.dumps(registers)}")
for call in registers:
    if (len(call) != 12 or call[:4] != ["ecs", "register-task-definition", "--region", "us-east-2"] or
            call[4] != "--cli-input-json" or not re.fullmatch(r"file:///tmp/pensyve-gateway-promote\.[A-Za-z0-9]+/task[89]\.json", call[5]) or
            call[6:] != ["--output", "json", "--cli-connect-timeout", "5", "--cli-read-timeout", "30"]):
        raise SystemExit(f"promoter register exact argv mismatch: {json.dumps(call)}")
updates = [call for call in calls if call[:2] == ["ecs", "update-service"]]
target_arns = [target for target in targets.split(",") if target]
expected_updates = [
    ["ecs", "update-service", "--region", "us-east-2", "--cluster", "pensyve-prod",
     "--service", "pensyve-prod-gateway", "--task-definition", target, "--output", "json",
     "--cli-connect-timeout", "5", "--cli-read-timeout", "30"]
    for target in target_arns
]
if updates != expected_updates:
    raise SystemExit(f"promoter update exact argv mismatch: {json.dumps(updates)}")

def service_read():
    return ["ecs", "describe-services", "--region", "us-east-2", "--cluster", "pensyve-prod",
            "--services", "pensyve-prod-gateway", "--cli-connect-timeout", "5",
            "--cli-read-timeout", "30", "--output", "json"]

def guard_task(revision):
    return ["ecs", "describe-task-definition", "--region", "us-east-2", "--task-definition",
            prefix + str(revision), "--cli-connect-timeout", "5", "--cli-read-timeout", "30",
            "--output", "json"]

def direct_task(revision):
    return ["ecs", "describe-task-definition", "--region", "us-east-2", "--task-definition",
            prefix + str(revision), "--output", "json", "--cli-connect-timeout", "5",
            "--cli-read-timeout", "30"]

expected_wait = ["ecs", "wait", "services-stable", "--region", "us-east-2", "--cluster",
                 "pensyve-prod", "--services", "pensyve-prod-gateway",
                 "--cli-connect-timeout", "5", "--cli-read-timeout", "30"]
expected_target_health = ["elbv2", "describe-target-health", "--region", "us-east-2",
                          "--target-group-arn", target_group, "--output", "json",
                          "--cli-connect-timeout", "5", "--cli-read-timeout", "30"]
stable_service_read = ["ecs", "describe-services", "--region", "us-east-2", "--cluster",
                       "pensyve-prod", "--services", "pensyve-prod-gateway", "--output", "json",
                       "--cli-connect-timeout", "5", "--cli-read-timeout", "30"]

def stable_health(attempts):
    return [expected_wait, *[call for _ in range(attempts)
                             for call in (stable_service_read, expected_target_health)]]
source_ecr = [
    "ecr", "describe-images", "--region", "us-east-2", "--registry-id", "196881464893",
    "--repository-name", "pensyve-gateway", "--image-ids",
    "imageTag=63011d55f8cbf52f6f9e5609621f6b8cf0c37535", "--output", "json",
    "--cli-connect-timeout", "5", "--cli-read-timeout", "30",
]

register = registers[0] if registers else None
if expected_registers == 0 and target_arns[0].endswith(":200"):
    expected = [service_read(), guard_task(201), guard_task(156), guard_task(200),
                source_ecr, expected_updates[0], *stable_health(expected_health),
                service_read(), guard_task(200), guard_task(156)]
elif expected_registers == 0 and target_arns[0].endswith(":156"):
    expected = [service_read(), guard_task(200), guard_task(156), source_ecr,
                expected_updates[0], *stable_health(expected_health),
                service_read(), guard_task(156)]
elif len(target_arns) == 1 and target_arns[0].endswith(":200"):
    expected = [service_read(), guard_task(156), direct_task(156), source_ecr, register,
                direct_task(200), service_read(), guard_task(156), source_ecr,
                expected_updates[0], *stable_health(expected_health), service_read(),
                guard_task(200), guard_task(156)]
elif len(target_arns) == 1 and target_arns[0].endswith(":201"):
    expected = [service_read(), guard_task(200), guard_task(156), direct_task(200), source_ecr,
                register, direct_task(201), service_read(), guard_task(200), guard_task(156),
                source_ecr, expected_updates[0], *stable_health(expected_health),
                service_read(), guard_task(201), guard_task(156), guard_task(200)]
elif len(target_arns) == 2 and expected_health == 1:
    expected = [service_read(), guard_task(156), direct_task(156), source_ecr, register,
                direct_task(200), service_read(), guard_task(156), source_ecr,
                expected_updates[0], expected_wait, source_ecr, expected_updates[1],
                *stable_health(1),
                service_read(), guard_task(156)]
elif len(target_arns) == 2 and expected_health >= 2 and expected_health % 2 == 0:
    attempts = expected_health // 2
    expected = [service_read(), guard_task(156), direct_task(156), source_ecr, register,
                direct_task(200), service_read(), guard_task(156), source_ecr,
                expected_updates[0], *stable_health(attempts), source_ecr,
                expected_updates[1], *stable_health(attempts)]
else:
    raise SystemExit("promoter exact-argv test scenario is undefined")

if calls != expected:
    raise SystemExit(
        "promoter complete ordered AWS sequence mismatch:\n"
        f"actual={json.dumps(calls)}\nexpected={json.dumps(expected)}"
    )
PY
}

run_task8_manual_rollback() {
    local root="${TEST_ROOT}/promoter-task8-rollback"
    local source_arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:156"
    local baseline_arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200"
    local target_group="arn:aws:elasticloadbalancing:us-east-2:196881464893:targetgroup/pensyve-prod-gw-tg/0123456789abcdef"
    make_promoter_fixture "${root}"
    printf '%s\n' "${baseline_arn}" > "${root}/state"
    : > "${root}/aws.log"
    env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
      TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=202 WAIT_MARKER="${root}/wait.marker" \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-rollback --custody "${root}/custody.json" >/dev/null
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 0 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(<"${root}/state")" == "${source_arn}" ]]
    assert_promoter_argv "${root}" 0 "${source_arn}" 1
    ! grep -E -- '--desired-count|environment' "${root}/aws.log" >/dev/null

    root="${TEST_ROOT}/promoter-task8-rollback-nonderived"
    make_promoter_fixture "${root}"
    python3 - "${root}/task-200.json" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
data = json.loads(path.read_text())
data["taskDefinition"]["memory"] = "2048"
path.write_text(json.dumps(data))
PY
    printf '%s\n' "${baseline_arn}" > "${root}/state"
    : > "${root}/aws.log"
    expect_failure "CPU/memory-only derivation" env AWS_BIN="${root}/bin/aws" \
      AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
      REGISTER_REVISION=202 WAIT_MARKER="${root}/wait.marker" \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-rollback --custody "${root}/custody.json"
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 0 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 0 ]]

    root="${TEST_ROOT}/promoter-task8-rollback-157"
    make_promoter_fixture "${root}"
    printf '%s\n' \
      "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:157" \
      > "${root}/state"
    : > "${root}/aws.log"
    expect_failure "157" env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
      SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=202 \
      WAIT_MARKER="${root}/wait.marker" FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-rollback --custody "${root}/custody.json"
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 0 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 0 ]]
}

run_promoter() {
    local root service_count target_count
    local source_arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:156"
    local baseline_arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200"
    local target_group="arn:aws:elasticloadbalancing:us-east-2:196881464893:targetgroup/pensyve-prod-gw-tg/0123456789abcdef"

    for service_count in 4 2; do
        if [[ "${service_count}" == 4 ]]; then target_count=2; else target_count=4; fi
        root="${TEST_ROOT}/promoter-cardinality-${service_count}-${target_count}"
        make_promoter_fixture "${root}"
        make_target_health_fixtures "${root}"
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        expect_failure "ALB target count must equal steady ECS desiredCount" env \
          PENSYVE_PROMOTION_STABILIZATION_ATTEMPTS=2 \
          AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
          TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
          SERVICE_COUNT="${service_count}" \
          TARGET_HEALTH_FIXTURE="${root}/targets-${target_count}.json" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
            fail "service ${service_count} with ${target_count} targets changed exact rollback count"
        [[ "$(grep -c '^elbv2 describe-target-health ' "${root}/aws.log")" -eq 4 ]] ||
            fail "persistent mismatch did not exhaust two attempts before each exact update"
        assert_promoter_argv "${root}" 1 "${baseline_arn},${source_arn}" 4
    done

    for service_count in 4 2; do
        if [[ "${service_count}" == 4 ]]; then
            target_count=4
            first_fixture="targets-4-initial.json"
        else
            target_count=2
            first_fixture="targets-4-draining.json"
        fi
        root="${TEST_ROOT}/promoter-converges-${service_count}-${target_count}"
        make_promoter_fixture "${root}"
        make_target_health_fixtures "${root}"
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        env PENSYVE_PROMOTION_STABILIZATION_ATTEMPTS=2 \
          AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
          TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
          SERVICE_COUNT="${service_count}" \
          TARGET_HEALTH_FIRST_FIXTURE="${root}/${first_fixture}" \
          TARGET_HEALTH_FIRST_MARKER="${root}/first-target.marker" \
          TARGET_HEALTH_FIXTURE="${root}/targets-${target_count}.json" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json" >/dev/null
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 1 ]] ||
            fail "converged service ${service_count} performed rollback or duplicate update"
        [[ "$(grep -c '^elbv2 describe-target-health ' "${root}/aws.log")" -eq 2 ]] ||
            fail "converged service ${service_count} did not use exactly one retry"
        assert_promoter_argv "${root}" 1 "${baseline_arn}" 2
    done

    root="${TEST_ROOT}/promoter-converges-2-draining"
    make_promoter_fixture "${root}"
    make_target_health_fixtures "${root}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    : > "${root}/aws.log"
    env PENSYVE_PROMOTION_STABILIZATION_ATTEMPTS=2 \
      AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
      TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
      SERVICE_COUNT=2 TARGET_HEALTH_FIRST_FIXTURE="${root}/targets-2-draining.json" \
      TARGET_HEALTH_FIRST_MARKER="${root}/first-target.marker" \
      TARGET_HEALTH_FIXTURE="${root}/targets-2.json" \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json" >/dev/null
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 1 ]] ||
        fail "matching-cardinality draining convergence performed rollback or duplicate update"
    [[ "$(grep -c '^elbv2 describe-target-health ' "${root}/aws.log")" -eq 2 ]] ||
        fail "matching-cardinality draining convergence did not use exactly one retry"
    assert_promoter_argv "${root}" 1 "${baseline_arn}" 2

    for kind in top-missing top-null top-non-list short-malformed short-wrong-port \
      short-bad-id short-bad-state short-duplicate short-unhealthy; do
        root="${TEST_ROOT}/promoter-terminal-dominance-${kind}"
        make_promoter_fixture "${root}"
        make_target_health_fixtures "${root}"
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        case "${kind}" in
            top-*) expected="TargetHealthDescriptions must be a list" ;;
            short-malformed) expected="ALB target description is malformed" ;;
            short-wrong-port) expected="ALB target port must be exact integer 3000" ;;
            short-bad-id) expected="ALB target ID must be a nonempty string" ;;
            short-bad-state) expected="ALB target state must be a string" ;;
            short-duplicate) expected="ALB healthy target identities must be distinct" ;;
            short-unhealthy) expected="ALB target state is terminal or unknown" ;;
        esac
        expect_failure "${expected}" env PENSYVE_PROMOTION_STABILIZATION_ATTEMPTS=2 \
          AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
          TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
          SERVICE_COUNT=4 TARGET_HEALTH_FIXTURE="${root}/targets-${kind}.json" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
            fail "terminal ${kind} fixture changed exact rollback count"
        [[ "$(grep -c '^elbv2 describe-target-health ' "${root}/aws.log")" -eq 2 ]] ||
            fail "terminal ${kind} fixture was retried before candidate/rollback failure"
        assert_promoter_argv "${root}" 1 "${baseline_arn},${source_arn}" 2
    done

    root="${TEST_ROOT}/promoter-success"
    make_promoter_fixture "${root}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    : > "${root}/aws.log"
    env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
      TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json" > "${root}/result.log"
    grep -Fx "PENSYVE_TASK8_BASELINE_ARN=${baseline_arn}" "${root}/result.log" >/dev/null
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^elbv2 describe-target-health ' "${root}/aws.log")" -eq 1 ]] ||
        fail "successful promotion must verify exactly two healthy ALB targets once"
    jq -e \
      '.taskDefinition.containerDefinitions[] | select(.name == "gateway") | .image == "196881464893.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway:63011d55f8cbf52f6f9e5609621f6b8cf0c37535"' \
      "${root}/task-200.json" >/dev/null ||
        fail "Task 8 registration did not preserve the exact live source image tag"
    assert_promoter_argv "${root}" 1 "${baseline_arn}" 1
    ! grep -E -- '--desired-count|--force-new-deployment|application-autoscaling|register-scalable-target|put-scaling-policy' \
      "${root}/aws.log" >/dev/null

    root="${TEST_ROOT}/promoter-success-4"
    make_promoter_fixture "${root}"
    make_target_health_fixtures "${root}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    : > "${root}/aws.log"
    env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
      TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
      SERVICE_COUNT=4 TARGET_HEALTH_FIXTURE="${root}/targets-4.json" \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json" > "${root}/result.log"
    grep -Fx "PENSYVE_TASK8_BASELINE_ARN=${baseline_arn}" "${root}/result.log" >/dev/null
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^elbv2 describe-target-health ' "${root}/aws.log")" -eq 1 ]] ||
        fail "four-task promotion must verify four healthy ALB targets once"
    assert_promoter_argv "${root}" 1 "${baseline_arn}" 1
    ! grep -E -- '--desired-count|--force-new-deployment|application-autoscaling|register-scalable-target|put-scaling-policy' \
      "${root}/aws.log" >/dev/null

    for kind in 1 5 unhealthy unused unavailable wrong-port duplicate missing malformed; do
        root="${TEST_ROOT}/promoter-target-${kind}"
        make_promoter_fixture "${root}"
        make_target_health_fixtures "${root}"
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        case "${kind}" in
            1 | 5) expected="ALB target count must equal steady ECS desiredCount" ;;
            duplicate) expected="ALB healthy target identities must be distinct" ;;
            unhealthy | unused | unavailable) expected="ALB target state is terminal or unknown" ;;
            wrong-port | malformed) expected="ALB target port must be exact integer 3000" ;;
            missing) expected="ALB target ID must be a nonempty string" ;;
        esac
        expect_failure "${expected}" env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
          SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 \
          WAIT_MARKER="${root}/wait.marker" SERVICE_COUNT=4 \
          TARGET_HEALTH_FIXTURE="${root}/targets-${kind}.json" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
            fail "invalid ${kind} target fixture changed exact candidate/rollback update count"
        assert_promoter_argv "${root}" 1 "${baseline_arn},${source_arn}" 2
    done

    for ecr_case in wrong-digest missing multiple; do
        root="${TEST_ROOT}/promoter-source-ecr-${ecr_case}"
        make_promoter_fixture "${root}"
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        if [[ "${ecr_case}" == wrong-digest ]]; then
            expect_failure "source image tag resolved to unexpected digest" env \
              SOURCE_ECR_DIGEST="sha256:$(printf 'd%.0s' {1..64})" \
              AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
              SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 \
              WAIT_MARKER="${root}/wait.marker" FIXTURE_TARGET_GROUP_ARN="${target_group}" \
              "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
        else
            expect_failure "source tag lookup must return exactly one image detail" env \
              SOURCE_ECR_CARDINALITY="${ecr_case}" AWS_BIN="${root}/bin/aws" \
              AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
              TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 \
              WAIT_MARKER="${root}/wait.marker" FIXTURE_TARGET_GROUP_ARN="${target_group}" \
              "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
        fi
        [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 0 ]]
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 0 ]]
    done

    root="${TEST_ROOT}/promoter-failure"
    make_promoter_fixture "${root}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    : > "${root}/aws.log"
    expect_failure "rolled back once" env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" \
      SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 \
      WAIT_MARKER="${root}/wait.marker" FAIL_FIRST_WAIT=1 FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]]
    [[ "$(grep -c '^elbv2 describe-target-health ' "${root}/aws.log")" -eq 1 ]] ||
        fail "automatic rollback must verify exactly two healthy ALB targets"
    [[ "$(<"${root}/state")" == "${source_arn}" ]]
    assert_promoter_argv "${root}" 1 "${baseline_arn},${source_arn}" 1

    root="${TEST_ROOT}/promoter-task9"
    make_promoter_fixture "${root}"
    printf '%s\n' "${baseline_arn}" > "${root}/state"
    : > "${root}/aws.log"
    env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
      TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=201 WAIT_MARKER="${root}/wait.marker" \
      PENSYVE_TASK8_BASELINE_ARN="${baseline_arn}" FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task9-promote --custody "${root}/custody.json" >/dev/null
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 1 ]]
    assert_promoter_argv "${root}" 1 \
      "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:201" 1
    : > "${root}/aws.log"
    env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
      TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=202 WAIT_MARKER="${root}/wait.marker" \
      PENSYVE_TASK8_BASELINE_ARN="${baseline_arn}" FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task9-rollback --custody "${root}/custody.json" >/dev/null
    [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 0 ]]
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 1 ]]
    [[ "$(<"${root}/state")" == "${baseline_arn}" ]]
    assert_promoter_argv "${root}" 0 "${baseline_arn}" 1
}

run_rollback_traps() {
    local source_arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:156"
    local target_group="arn:aws:elasticloadbalancing:us-east-2:196881464893:targetgroup/pensyve-prod-gw-tg/0123456789abcdef"
    local root signal phase second_signal

    root="${TEST_ROOT}/rollback-ordinary"
    make_promoter_fixture "${root}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    : > "${root}/aws.log"
    expect_failure "automatic rollback verified" env AWS_BIN="${root}/bin/aws" \
      AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
      REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" FAIL_FIRST_WAIT=1 \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
        fail "ordinary failure must perform one candidate update and exactly one rollback update"
    [[ "$(<"${root}/state")" == "${source_arn}" ]] || fail "ordinary rollback target drifted"
    assert_promoter_argv "${root}" 1 \
      "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200,${source_arn}" 1

    for signal in TERM INT; do
        root="${TEST_ROOT}/rollback-${signal,,}"
        make_promoter_fixture "${root}"
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        expect_failure "automatic rollback verified" env AWS_BIN="${root}/bin/aws" \
          AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
          REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" SIGNAL_FIRST_WAIT="${signal}" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
            fail "${signal} must perform one candidate update and exactly one rollback update"
        [[ "$(<"${root}/state")" == "${source_arn}" ]] || fail "${signal} rollback target drifted"
        assert_promoter_argv "${root}" 1 \
          "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200,${source_arn}" 1
    done

    for phase in wait readback; do
        if [[ "${phase}" == wait ]]; then second_signal=TERM; else second_signal=INT; fi
        root="${TEST_ROOT}/rollback-second-signal-${phase}"
        make_promoter_fixture "${root}"
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        expect_failure "automatic rollback verified" env AWS_BIN="${root}/bin/aws" \
          AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
          REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" SIGNAL_FIRST_WAIT=TERM \
          SECOND_SIGNAL_PHASE="${phase}" SECOND_SIGNAL="${second_signal}" \
          SECOND_SIGNAL_MARKER="${root}/second-signal.marker" \
          ROLLBACK_UPDATE_MARKER="${root}/rollback-update.marker" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
        [[ -e "${root}/second-signal.marker" ]] ||
            fail "delayed rollback ${phase} fixture did not send its second ${second_signal}"
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
            fail "second ${second_signal} during rollback ${phase} caused a double/missing update"
        [[ "$(<"${root}/state")" == "${source_arn}" ]] ||
            fail "second ${second_signal} interrupted exact return during rollback ${phase}"
        assert_promoter_argv "${root}" 1 \
          "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200,${source_arn}" 1
    done

    root="${TEST_ROOT}/rollback-unhealthy"
    make_promoter_fixture "${root}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    : > "${root}/aws.log"
    expect_failure "automatic rollback failed verification" env AWS_BIN="${root}/bin/aws" \
      AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
      REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" TARGET_HEALTH_STATE=unhealthy \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
        fail "failed rollback verification must not attempt a second rollback"
    assert_promoter_argv "${root}" 1 \
      "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200,${source_arn}" 2

    root="${TEST_ROOT}/rollback-one-target"
    make_promoter_fixture "${root}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    printf '%s\n' '{"TargetHealthDescriptions":[{"Target":{"Id":"10.0.1.10","Port":3000},"TargetHealth":{"State":"healthy"}}]}' \
      > "${root}/one-target.json"
    : > "${root}/aws.log"
    expect_failure "automatic rollback failed verification" env AWS_BIN="${root}/bin/aws" \
      AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
      REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
      TARGET_HEALTH_FIXTURE="${root}/one-target.json" FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${PROMOTE_SCRIPT}" task8-create --custody "${root}/custody.json"
    assert_promoter_argv "${root}" 1 \
      "arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200,${source_arn}" 2
}

extract_workflow_step() {
    python3 - "${DEPLOY_WORKFLOW}" "$1" "$2" <<'PY'
import sys
from pathlib import Path
import yaml

workflow = yaml.load(Path(sys.argv[1]).read_text(), Loader=yaml.BaseLoader)
name, output = sys.argv[2], Path(sys.argv[3])
matches = [step for job in workflow["jobs"].values() for step in job.get("steps", [])
           if step.get("name") == name]
if len(matches) != 1 or not isinstance(matches[0].get("run"), str):
    raise SystemExit(f"workflow step is not unique/executable: {name}")
output.write_text(matches[0]["run"])
PY
}

verify_inline_policy() {
    python3 - "$1" "$2" <<'PY'
import json
import sys
from pathlib import Path

import yaml

workflow = yaml.load(Path(sys.argv[1]).read_text(), Loader=yaml.BaseLoader)
account = sys.argv[2]
steps = [step for job in workflow["jobs"].values() for step in job.get("steps", [])]
credentials = [step for step in steps if str(step.get("uses", "")).startswith(
               "aws-actions/configure-aws-credentials@")]
if len(credentials) != 1:
    raise SystemExit("workflow must contain exactly one credential action")
raw = credentials[0].get("with", {}).get("inline-session-policy")
if not isinstance(raw, str):
    raise SystemExit("workflow inline session policy is missing")
policy = json.loads(raw.replace("${{ needs.preflight.outputs.account }}", account))
pass_role = [statement for statement in policy.get("Statement", [])
             if statement.get("Action") == "iam:PassRole"]
expected = [
    f"arn:aws:iam::{account}:role/pensyve-prod-task",
    f"arn:aws:iam::{account}:role/pensyve-prod-task-execution",
]
if len(pass_role) != 1 or pass_role[0].get("Resource") != expected:
    raise SystemExit(
        "workflow PassRole resources differ from exact live project roles: "
        f"{pass_role!r}"
    )
PY
}

expect_rejection() {
    local label="$1"
    shift
    if "$@" >"${TEST_ROOT}/rejection.$RANDOM.log" 2>&1; then
        fail "adversarial workflow response passed: ${label}"
    fi
}

run_workflow_contract() {
    local root="${TEST_ROOT}/workflow"
    local sha="0123456789abcdef0123456789abcdef01234567"
    local tree="89abcdef0123456789abcdef0123456789abcdef"
    local account="196881464893"
    mkdir -p "${root}/bin" "${root}/runner"
    verify_inline_policy "${DEPLOY_WORKFLOW}" "${account}"
    cp "${DEPLOY_WORKFLOW}" "${root}/wrong-role.yml"
    sed -i 's|role/pensyve-prod-task|role/pensyve-prod-wrong-task|g' \
      "${root}/wrong-role.yml"
    expect_failure "workflow PassRole resources differ from exact live project roles" \
      verify_inline_policy "${root}/wrong-role.yml" "${account}"
    make_promoter_fixture "${root}/fixture"
    cp "${root}/fixture/custody.json" "${root}/runner/custody.json"
    extract_workflow_step \
      "Canonicalize and bind custody to exact current main" "${root}/preflight.sh"
    sed -i "s/\${{ github.sha }}/${sha}/g" "${root}/preflight.sh"
    extract_workflow_step \
      "Recheck canonical custody, reviewed hash, and current main before OIDC" \
      "${root}/recheck.sh"
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'jq -n --arg sha "${GH_MAIN_SHA}" --arg tree "${GH_MAIN_TREE}" '\''{sha:$sha,commit:{tree:{sha:$tree}}}'\''' \
      > "${root}/bin/gh"
    chmod +x "${root}/bin/gh"
    : > "${root}/output"
    : > "${root}/summary"
    env PATH="${root}/bin:${PATH}" CUSTODY_JSON="$(<"${root}/fixture/custody.json")" \
      EXPECTED_SHA="${sha}" EXPECTED_REF=refs/heads/main GITHUB_OUTPUT="${root}/output" \
      GITHUB_STEP_SUMMARY="${root}/summary" GITHUB_REPOSITORY=major7apps/pensyve \
      RUNNER_TEMP="${root}/runner" GH_MAIN_SHA="${sha}" GH_MAIN_TREE="${tree}" \
      bash "${root}/preflight.sh"
    local custody_hash
    custody_hash="$(awk -F= '$1 == "custody_sha256" {print $2}' "${root}/output")"
    grep -Fx "account=${account}" "${root}/output" >/dev/null ||
        fail "preflight did not emit the custody account"
    expect_rejection "wrong current main" env PATH="${root}/bin:${PATH}" \
      CUSTODY_JSON="$(<"${root}/fixture/custody.json")" EXPECTED_SHA="${sha}" \
      EXPECTED_REF=refs/heads/main GITHUB_OUTPUT="${root}/wrong-main-output" \
      GITHUB_STEP_SUMMARY="${root}/summary" GITHUB_REPOSITORY=major7apps/pensyve \
      RUNNER_TEMP="${root}/runner" GH_MAIN_SHA="$(printf 'f%.0s' {1..40})" \
      GH_MAIN_TREE="${tree}" bash "${root}/preflight.sh"
    expect_rejection "wrong source SHA" env PATH="${root}/bin:${PATH}" \
      CUSTODY_JSON="$(<"${root}/fixture/custody.json")" \
      EXPECTED_SHA="$(printf 'e%.0s' {1..40})" EXPECTED_REF=refs/heads/main \
      GITHUB_OUTPUT="${root}/wrong-source-output" GITHUB_STEP_SUMMARY="${root}/summary" \
      GITHUB_REPOSITORY=major7apps/pensyve RUNNER_TEMP="${root}/runner" \
      GH_MAIN_SHA="${sha}" GH_MAIN_TREE="${tree}" bash "${root}/preflight.sh"
    env PATH="${root}/bin:${PATH}" CUSTODY_JSON="$(<"${root}/fixture/custody.json")" \
      EXPECTED_SHA="${sha}" EXPECTED_TREE="${tree}" EXPECTED_HASH="${custody_hash}" \
      EXPECTED_ACCOUNT="${account}" \
      PENSYVE_REVIEWED_CUSTODY_SHA256="${custody_hash}" OPERATION=task8-create \
      PENSYVE_TASK8_BASELINE_ARN= RUNNER_TEMP="${root}/runner" GITHUB_REF=refs/heads/main \
      GITHUB_SHA="${sha}" GITHUB_REPOSITORY=major7apps/pensyve GH_MAIN_SHA="${sha}" \
      GH_MAIN_TREE="${tree}" bash "${root}/recheck.sh"
    expect_rejection "wrong custody hash" env PATH="${root}/bin:${PATH}" \
      CUSTODY_JSON="$(<"${root}/fixture/custody.json")" EXPECTED_SHA="${sha}" \
      EXPECTED_TREE="${tree}" EXPECTED_HASH="$(printf '0%.0s' {1..64})" \
      EXPECTED_ACCOUNT="${account}" \
      PENSYVE_REVIEWED_CUSTODY_SHA256="${custody_hash}" OPERATION=task8-create \
      PENSYVE_TASK8_BASELINE_ARN= RUNNER_TEMP="${root}/runner" GITHUB_REF=refs/heads/main \
      GITHUB_SHA="${sha}" GITHUB_REPOSITORY=major7apps/pensyve GH_MAIN_SHA="${sha}" \
      GH_MAIN_TREE="${tree}" bash "${root}/recheck.sh"
    expect_rejection "custody hash lacks environment-scoped human review" env \
      PATH="${root}/bin:${PATH}" CUSTODY_JSON="$(<"${root}/fixture/custody.json")" \
      EXPECTED_SHA="${sha}" EXPECTED_TREE="${tree}" EXPECTED_HASH="${custody_hash}" \
      EXPECTED_ACCOUNT="${account}" \
      PENSYVE_REVIEWED_CUSTODY_SHA256="$(printf '1%.0s' {1..64})" OPERATION=task8-create \
      PENSYVE_TASK8_BASELINE_ARN= RUNNER_TEMP="${root}/runner" GITHUB_REF=refs/heads/main \
      GITHUB_SHA="${sha}" GITHUB_REPOSITORY=major7apps/pensyve GH_MAIN_SHA="${sha}" \
      GH_MAIN_TREE="${tree}" bash "${root}/recheck.sh"

    extract_workflow_step \
      "Verify immutable ECR raw manifest, config blob, platform, and source" "${root}/ecr.sh"
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'printf '\''%q '\'' "$@" >> "$WORKFLOW_AWS_LOG"; printf '\''\n'\'' >> "$WORKFLOW_AWS_LOG"' \
      'case "$1 $2" in' \
      ' "sts get-caller-identity") printf '\''%s\n'\'' "${ASSUMED_ACCOUNT}" ;;' \
      ' "ecr describe-images") jq -n --arg d "${ECR_DESCRIBE_DIGEST:-$MANIFEST_DIGEST}" '\''{imageDetails:[{imageDigest:$d}]}'\'' ;;' \
      ' "ecr batch-get-image") raw=$(<"$ECR_RAW_MANIFEST"); jq -n --arg d "$MANIFEST_DIGEST" --arg m "$MANIFEST_MEDIA" --arg raw "$raw" '\''{images:[{imageId:{imageDigest:$d},imageManifestMediaType:$m,imageManifest:$raw}],failures:[]}'\'' ;;' \
      ' "ecr get-download-url-for-layer") printf '\''fixture://config\n'\'' ;;' \
      ' *) echo "unexpected workflow AWS call: $*" >&2; exit 90 ;; esac' \
      > "${root}/bin/aws"
    printf '%s\n' \
      '#!/usr/bin/env bash' 'set -euo pipefail' \
      'output=""; while (( $# )); do if [[ "$1" == --output ]]; then output="$2"; shift 2; else shift; fi; done' \
      'cp "$ECR_CONFIG" "$output"' > "${root}/bin/curl"
    chmod +x "${root}/bin/aws" "${root}/bin/curl"

    run_ecr_step() {
        local case_root="$1"
        shift
        : > "${case_root}/aws.log"
        env PATH="${root}/bin:${PATH}" WORKFLOW_AWS_LOG="${case_root}/aws.log" \
          RUNNER_TEMP="${case_root}" SOURCE_SHA="${sha}" EXPECTED_ACCOUNT="${account}" \
          ASSUMED_ACCOUNT="${account}" ECR_RAW_MANIFEST="${case_root}/manifest.json" \
          ECR_CONFIG="${case_root}/ecr-config.json" \
          MANIFEST_DIGEST="$(jq -r '.image.manifest_digest' "${case_root}/custody.json")" \
          MANIFEST_MEDIA="application/vnd.docker.distribution.manifest.v2+json" \
          "$@" bash "${root}/ecr.sh"
    }
    prepare_ecr_case() {
        local case_root="$1" mode="$2"
        mkdir -p "${case_root}"
        cp "${root}/fixture/custody.json" "${case_root}/custody.json"
        cp "${root}/fixture/publisher/manifest.json" "${case_root}/manifest.json"
        cp "${root}/fixture/publisher/config.json" "${case_root}/ecr-config.json"
        python3 - "${case_root}" "${mode}" <<'PY'
import hashlib, json, sys
from pathlib import Path
root, mode = Path(sys.argv[1]), sys.argv[2]
custody = json.loads((root / "custody.json").read_text())
config = json.loads((root / "ecr-config.json").read_text())
manifest = json.loads((root / "manifest.json").read_text())
if mode == "platform": config["architecture"] = "amd64"
if mode == "source": config["config"]["Labels"]["org.opencontainers.image.revision"] = "f" * 40
if mode in {"platform", "source"}:
    config_bytes = json.dumps(config, sort_keys=True, separators=(",", ":")).encode()
    (root / "ecr-config.json").write_bytes(config_bytes)
    custody["image"]["config_digest"] = "sha256:" + hashlib.sha256(config_bytes).hexdigest()
    manifest["config"]["digest"] = custody["image"]["config_digest"]
if mode == "manifest-config": manifest["config"]["digest"] = "sha256:" + "d" * 64
if mode in {"platform", "source", "manifest-config"}:
    raw = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
    (root / "manifest.json").write_bytes(raw)
    custody["image"]["raw_manifest_sha256"] = hashlib.sha256(raw).hexdigest()
    custody["image"]["manifest_digest"] = "sha256:" + hashlib.sha256(raw).hexdigest()
(root / "custody.json").write_text(json.dumps(custody, sort_keys=True, separators=(",", ":")) + "\n")
PY
    }

    local ecr_root="${root}/ecr-good"
    prepare_ecr_case "${ecr_root}" good
    run_ecr_step "${ecr_root}"
    python3 - "${ecr_root}/aws.log" "${ecr_root}/custody.json" "${account}" <<'PY'
import json, shlex, sys
from pathlib import Path
calls = [shlex.split(line) for line in Path(sys.argv[1]).read_text().splitlines()]
image = json.loads(Path(sys.argv[2]).read_text())["image"]
account = sys.argv[3]
expected = [
 ["sts","get-caller-identity","--query","Account","--output","text"],
 ["ecr","describe-images","--region","us-east-2","--registry-id",account,"--repository-name","pensyve-gateway","--image-ids",f"imageDigest={image['manifest_digest']}","--output","json"],
 ["ecr","batch-get-image","--region","us-east-2","--registry-id",account,"--repository-name","pensyve-gateway","--image-ids",f"imageDigest={image['manifest_digest']}","--accepted-media-types",image["raw_manifest_media_type"],"--output","json"],
 ["ecr","get-download-url-for-layer","--region","us-east-2","--registry-id",account,"--repository-name","pensyve-gateway","--layer-digest",image["config_digest"],"--query","downloadUrl","--output","text"],
]
if calls != expected: raise SystemExit(f"workflow exact AWS argv mismatch: {json.dumps(calls)}")
PY
    expect_rejection "wrong assumed account" run_ecr_step "${ecr_root}" ASSUMED_ACCOUNT=999999999999
    expect_rejection "wrong ECR manifest digest" run_ecr_step "${ecr_root}" \
      ECR_DESCRIBE_DIGEST="sha256:$(printf 'd%.0s' {1..64})"
    for mode in manifest-config platform source; do
        ecr_root="${root}/ecr-${mode}"
        prepare_ecr_case "${ecr_root}" "${mode}"
        expect_rejection "wrong ECR ${mode}" run_ecr_step "${ecr_root}"
    done
    ecr_root="${root}/ecr-config-bytes"
    prepare_ecr_case "${ecr_root}" good
    printf '{"wrong":"config"}\n' > "${ecr_root}/ecr-config.json"
    expect_rejection "wrong ECR config bytes" run_ecr_step "${ecr_root}"
}

run_executable_argv_mutations() {
    local kind root script expiry source_sha source_tree manifest_digest target_group source_arn baseline_arn
    target_group="arn:aws:elasticloadbalancing:us-east-2:196881464893:targetgroup/pensyve-prod-gw-tg/0123456789abcdef"
    source_arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:156"
    baseline_arn="arn:aws:ecs:us-east-2:196881464893:task-definition/pensyve-prod-gateway:200"
    expiry="$(date -u -d '+30 minutes' '+%Y-%m-%dT%H:%M:%SZ')"

    for kind in source-tag source-account; do
        root="${TEST_ROOT}/argv-promoter-${kind}"
        make_promoter_fixture "${root}"
        script="${root}/promote-gateway-image.sh"
        cp "${PROMOTE_SCRIPT}" "${script}"
        cp "${GUARD_SCRIPT}" "${root}/guard-active-service.py"
        chmod +x "${script}" "${root}/guard-active-service.py"
        if [[ "${kind}" == source-tag ]]; then
            sed -i 's/63011d55f8cbf52f6f9e5609621f6b8cf0c37535/ffffffffffffffffffffffffffffffffffffffff/g' \
              "${script}"
        else
            sed -i 's/readonly SOURCE_ACCOUNT="196881464893"/readonly SOURCE_ACCOUNT="999999999999"/' \
              "${script}"
        fi
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        expect_failure "non-exact AWS argv" env AWS_BIN="${root}/bin/aws" \
          AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
          REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${script}" task8-create --custody "${root}/custody.json"
        [[ "$(grep -c '^ecs register-task-definition ' "${root}/aws.log")" -eq 0 ]]
        [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 0 ]]
    done

    for kind in load tag push; do
        root="${TEST_ROOT}/argv-publisher-${kind}"
        make_publisher_fixture "${root}"
        script="${root}/artifact.sh"
        cp "${ARTIFACT_SCRIPT}" "${script}"
        case "${kind}" in
            load) sed -i '/"${docker_bin}" load --input/a\    "${docker_bin}" load --input "${archive}" >/dev/null' "${script}" ;;
            tag) sed -i '/"${docker_bin}" tag/a\    "${docker_bin}" tag "${local_ref}" "${remote_ref}"' "${script}" ;;
            push) sed -i '/"${docker_bin}" push/a\    "${docker_bin}" push "${remote_ref}" >/dev/null' "${script}" ;;
        esac
        source_sha="$(awk -F= '$1 == "SOURCE_SHA" {print $2}' "${root}/identity.env")"
        source_tree="$(awk -F= '$1 == "SOURCE_TREE" {print $2}' "${root}/identity.env")"
        manifest_digest="$(awk -F= '$1 == "MANIFEST_DIGEST" {print $2}' "${root}/identity.env")"
        : > "${root}/aws.log"
        : > "${root}/docker.log"
        : > "${root}/docker-config.log"
        env AWS_BIN="${root}/bin/aws" DOCKER_BIN="${root}/bin/docker" \
          CURL_BIN="${root}/bin/curl" GIT_BIN="${root}/bin/git" UNAME_BIN="${root}/bin/uname" \
          AWS_LOG="${root}/aws.log" DOCKER_LOG="${root}/docker.log" \
          DOCKER_CONFIG_LOG="${root}/docker-config.log" SOURCE_SHA="${source_sha}" \
          SOURCE_TREE="${source_tree}" MANIFEST_DIGEST="${manifest_digest}" \
          MANIFEST_MEDIA="application/vnd.docker.distribution.manifest.v2+json" \
          ECR_RAW_MANIFEST="${root}/manifest.json" ECR_CONFIG="${root}/config.json" \
          CALLER_ARN="arn:aws:sts::123456789012:federated-user/pensyve-gateway-${source_sha}" \
          AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_SESSION_TOKEN=test \
          AWS_SESSION_EXPIRATION="${expiry}" \
          PENSYVE_ECR_REGISTRY="123456789012.dkr.ecr.us-east-2.amazonaws.com" \
          PENSYVE_INLINE_SESSION_POLICY_SHA256="$(printf 'c%.0s' {1..64})" \
          "${script}" publish-ecr --tuple "${root}/tuple.json" --output "${root}/custody.json" >/dev/null
        expect_failure "publisher exact Docker argv mismatch" assert_publisher_argv "${root}"
    done

    for kind in register update; do
        root="${TEST_ROOT}/argv-promoter-${kind}"
        make_promoter_fixture "${root}"
        script="${root}/promote-gateway-image.sh"
        cp "${PROMOTE_SCRIPT}" "${script}"
        cp "${GUARD_SCRIPT}" "${root}/guard-active-service.py"
        chmod +x "${script}" "${root}/guard-active-service.py"
        if [[ "${kind}" == register ]]; then
            sed -i '/--cli-input-json "file:\/\/${expected}" --output json > "${TEMP_ROOT}\/register.json"/a\    aws_call ecs register-task-definition --region "${REGION}" --cli-input-json "file://${expected}" --output json >/dev/null' "${script}"
        else
            sed -i '/--service "${SERVICE}" --task-definition "${target}" --output json >\/dev\/null/a\    aws_call ecs update-service --region "${REGION}" --cluster "${CLUSTER}" --service "${SERVICE}" --task-definition "${target}" --output json >/dev/null' "${script}"
        fi
        printf '%s\n' "${source_arn}" > "${root}/state"
        : > "${root}/aws.log"
        env AWS_BIN="${root}/bin/aws" AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" \
          TASK_FIXTURE_ROOT="${root}" REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" \
          FIXTURE_TARGET_GROUP_ARN="${target_group}" \
          "${script}" task8-create --custody "${root}/custody.json" >/dev/null
        if [[ "${kind}" == register ]]; then
            expect_failure "promoter register exact argv count mismatch" \
              assert_promoter_argv "${root}" 1 "${baseline_arn}" 1
        else
            expect_failure "promoter update exact argv mismatch" \
              assert_promoter_argv "${root}" 1 "${baseline_arn}" 1
        fi
    done

    root="${TEST_ROOT}/argv-promoter-return-guard"
    make_promoter_fixture "${root}"
    script="${root}/promote-gateway-image.sh"
    cp "${PROMOTE_SCRIPT}" "${script}"
    cp "${GUARD_SCRIPT}" "${root}/guard-active-service.py"
    chmod +x "${script}" "${root}/guard-active-service.py"
    sed -i '/"${GUARD}" "${RETURN_MODE}" --expected-current-arn "${RETURN_ARN}"/,+1d' \
      "${script}"
    printf '%s\n' "${source_arn}" > "${root}/state"
    : > "${root}/aws.log"
    expect_failure "automatic rollback verified" env AWS_BIN="${root}/bin/aws" \
      AWS_LOG="${root}/aws.log" SERVICE_STATE="${root}/state" TASK_FIXTURE_ROOT="${root}" \
      REGISTER_REVISION=200 WAIT_MARKER="${root}/wait.marker" FAIL_FIRST_WAIT=1 \
      FIXTURE_TARGET_GROUP_ARN="${target_group}" \
      "${script}" task8-create --custody "${root}/custody.json"
    [[ "$(grep -c '^ecs update-service ' "${root}/aws.log")" -eq 2 ]] ||
        fail "removed return guard mutation changed rollback update count"
    expect_failure "promoter complete ordered AWS sequence mismatch" \
      assert_promoter_argv "${root}" 1 "${baseline_arn},${source_arn}" 1
}

run_mutations() {
    local mutate="${TEST_ROOT}/mutate"
    mkdir -p "${mutate}"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    cp "${CI_WORKFLOW}" "${mutate}/ci.yml"
    cp "${ARTIFACT_SCRIPT}" "${mutate}/artifact.sh"
    cp "${PROMOTE_SCRIPT}" "${mutate}/promote.sh"
    sed -i 's/^  preflight:/  missing-preflight:/' "${mutate}/deploy.yml"
    expect_failure "only preflight and production" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    sed -i \
      's|role/pensyve-prod-task-execution|role/pensyve-prod-gateway-execution|g; s|role/pensyve-prod-task"|role/pensyve-prod-gateway-task"|g' \
      "${mutate}/deploy.yml"
    expect_failure "exact live project roles" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    printf '\n  test-rust-models:\n    runs-on: ubuntu-latest\n    steps: []\n' \
        >> "${mutate}/ci.yml"
    expect_failure "remote-heavy job" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${CI_WORKFLOW}" "${mutate}/ci.yml"
    sed -i '/get-caller-identity/a\    aws_call ecs update-service --cluster forbidden' \
        "${mutate}/artifact.sh"
    expect_failure "publisher overlaps forbidden authority" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${ARTIFACT_SCRIPT}" "${mutate}/artifact.sh"
    sed -i '/^set -euo pipefail/a docker push forbidden' "${mutate}/promote.sh"
    expect_failure "forbidden Docker/ECR-write" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${PROMOTE_SCRIPT}" "${mutate}/promote.sh"
    sed -i 's/commits\/main/commits\/wrong/' "${mutate}/deploy.yml"
    expect_failure "commits/main" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    sed -i 's/task9-rollback/latest/' "${mutate}/promote.sh"
    expect_failure "mode task9-rollback" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${PROMOTE_SCRIPT}" "${mutate}/promote.sh"
    sed -i 's/PENSYVE_REVIEWED_CUSTODY_SHA256/UNREVIEWED_CUSTODY/g' "${mutate}/deploy.yml"
    expect_failure "PENSYVE_REVIEWED_CUSTODY_SHA256" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    sed -i 's/raw manifest/raw bytes/g; s/raw-manifest/raw-bytes/g' "${mutate}/deploy.yml"
    expect_failure "raw manifest" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    sed -i 's/get-download-url-for-layer/get-layer/g' "${mutate}/deploy.yml"
    expect_failure "get-download-url-for-layer" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    sed -i 's/linux\/arm64/linux\/amd64/g' "${mutate}/deploy.yml"
    expect_failure "linux/arm64" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    sed -i 's/org.opencontainers.image.revision/org.opencontainers.image.version/g' \
        "${mutate}/deploy.yml"
    expect_failure "org.opencontainers.image.revision" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    cp "${DEPLOY_WORKFLOW}" "${mutate}/deploy.yml"
    sed -i '/"${docker_bin}" push/a\    "${docker_bin}" push "${remote_ref}"' \
        "${mutate}/artifact.sh"
    expect_failure "exactly one candidate push" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${ARTIFACT_SCRIPT}" "${mutate}/artifact.sh"
    sed -i '/register-task-definition/a\    aws_call ecs register-task-definition --region us-east-2' \
        "${mutate}/promote.sh"
    expect_failure "one registration call site" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${PROMOTE_SCRIPT}" "${mutate}/promote.sh"
    sed -i '/update-service/a\    aws_call ecs update-service --desired-count 3' \
        "${mutate}/promote.sh"
    expect_failure "desired-count" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${PROMOTE_SCRIPT}" "${mutate}/promote.sh"
    sed -i '/task\["cpu"\]/a\task["environment"] = []' "${mutate}/promote.sh"
    expect_failure "must not repair or mutate environment" validate_contract \
        "${mutate}/deploy.yml" "${mutate}/ci.yml" "${mutate}/artifact.sh" \
        "${mutate}/promote.sh"
    cp "${PROMOTE_SCRIPT}" "${mutate}/promote.sh"
    sed -i '/^set -euo pipefail/a # latest selector mutation' "${mutate}/promote.sh"
    expect_failure "latest" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    cp "${PROMOTE_SCRIPT}" "${mutate}/promote.sh"
    sed -i '/^set -euo pipefail/a # :157 rollback mutation' "${mutate}/promote.sh"
    expect_failure ":157" validate_contract "${mutate}/deploy.yml" \
        "${mutate}/ci.yml" "${mutate}/artifact.sh" "${mutate}/promote.sh"
    run_executable_argv_mutations
}

if [[ "${CASE}" == structural || "${CASE}" == all ]]; then run_structural; fi
if [[ "${CASE}" == guard || "${CASE}" == all ]]; then run_guard; fi
if [[ "${CASE}" == publisher || "${CASE}" == all ]]; then run_publisher; fi
if [[ "${CASE}" == promoter || "${CASE}" == all ]]; then run_promoter; fi
if [[ "${CASE}" == task8-rollback || "${CASE}" == all ]]; then run_task8_manual_rollback; fi
if [[ "${CASE}" == rollback || "${CASE}" == all ]]; then run_rollback_traps; fi
if [[ "${CASE}" == workflow || "${CASE}" == all ]]; then run_workflow_contract; fi
if [[ "${CASE}" == mutations || "${CASE}" == all ]]; then run_mutations; fi

echo "gateway local-custody contract tests passed (${CASE})"
