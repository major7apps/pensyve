#!/usr/bin/env bash

set -euo pipefail

MODE="${1:-}"
if [[ "${MODE}" == promote || "${MODE}" == finalize ]]; then
    shift
else
    MODE=promote
fi
readonly MODE
readonly VERIFIED_IMAGE="${1:-}"
readonly DOCKER_BIN="${DOCKER_BIN:-docker}"
readonly AWS_BIN="${AWS_BIN:-aws}"
readonly CURL_BIN="${CURL_BIN:-curl}"
readonly SLEEP_BIN="${SLEEP_BIN:-sleep}"
readonly EXPECTED_CLUSTER="pensyve-prod"
readonly EXPECTED_SERVICE="pensyve-prod-gateway"
readonly EXPECTED_CONTAINER="gateway"
readonly EXPECTED_MEDIA_TYPE="application/vnd.docker.distribution.manifest.v2+json"
readonly EXPECTED_GATEWAY_URL="https://mcp.pensyve.com"
readonly EXPECTED_GTE_REVISION="a829fd0e060bb84554da0dfd354d0de0f7712b7f"
readonly EXPECTED_BGE_REVISION="2cfc18c9415c912f9d8155881c133215df768a70"
readonly bge_calibration_query="Which codename is explicitly marked as the selected result by the production reranker proof?"
readonly bge_calibration_unreranked_order='["target","decoy","neutral"]'
readonly bge_calibration_reranked_order='["target","neutral","decoy"]'

die() {
    echo "gateway promotion error: $*" >&2
    exit 1
}

[[ -n "${VERIFIED_IMAGE}" && $# -eq 1 ]] \
  || die "usage: promote-gateway-image.sh [promote|finalize] verified-image.json"
[[ -f "${VERIFIED_IMAGE}" ]] || die "fixed verified-image.json is absent"
command -v jq >/dev/null 2>&1 || die "jq is required"
if [[ "${MODE}" == promote ]]; then
    command -v "${DOCKER_BIN}" >/dev/null 2>&1 || die "docker command is absent"
fi
command -v "${AWS_BIN}" >/dev/null 2>&1 || die "AWS command is absent"
command -v "${CURL_BIN}" >/dev/null 2>&1 || die "curl command is absent"
command -v "${SLEEP_BIN}" >/dev/null 2>&1 || die "sleep command is absent"

python3 - "${VERIFIED_IMAGE}" "${EXPECTED_CLUSTER}" "${EXPECTED_SERVICE}" "${EXPECTED_CONTAINER}" <<'PY'
import hashlib
import json
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
cluster, service, container = sys.argv[2:]
data = json.loads(path.read_text())

def fail(message):
    print(f"gateway promotion error: {message}", file=sys.stderr)
    raise SystemExit(1)

def exact_int(value):
    return type(value) is int

def field(name):
    value = data
    for part in name.split("."):
        if not isinstance(value, dict) or part not in value:
            fail(f"fixed verified-image.json field is absent: {name}")
        value = value[part]
    return value

expected_top = {"schema_version", "cleanup_required", "image", "scanner", "scan", "deployment"}
if set(data) != expected_top or not exact_int(data.get("schema_version")) or data.get("schema_version") != 1:
    fail("verified-image.json does not have the fixed promotion shape")
expected_image = {
    "archive_path", "archive_sha256", "config_path", "config_id", "platform",
    "source_label", "raw_manifest_path", "raw_manifest_sha256",
    "raw_manifest_media_type", "pushed_digest", "compressed_layer_bytes",
    "uncompressed_image_bytes",
}
if not isinstance(data.get("image"), dict) or set(data["image"]) != expected_image:
    fail("verified-image.json image shape is not fixed")
for name in ("compressed_layer_bytes", "uncompressed_image_bytes"):
    if not exact_int(data["image"].get(name)) or data["image"][name] <= 0:
        fail(f"image {name.replace('_', ' ')} must be a positive integer")
expected_scanner = {
    "image_digest", "version", "argv", "db_updated_at", "db_downloaded_at",
    "db_sha256", "db_path", "db_oci_digest",
}
if not isinstance(data.get("scanner"), dict) or set(data["scanner"]) != expected_scanner:
    fail("verified-image.json scanner shape is not fixed")
expected_scan = {
    "report_path", "report_sha256", "archive_sha256", "config_id", "scanned_at",
    "source_artifact_created_at", "policy_path", "policy_version", "policy_sha256", "policy_result",
}
if not isinstance(data.get("scan"), dict) or set(data["scan"]) != expected_scan:
    fail("verified-image.json scan shape is not fixed")
expected_deployment = {
    "region", "ecr_registry", "ecr_repository", "cluster", "service",
    "gateway_container", "baseline_task_definition_arn", "baseline_image",
    "baseline_environment_sha256", "baseline_service_snapshot",
    "baseline_service_snapshot_sha256", "probe_entity", "promotion_run_id", "promotion_run_attempt",
    "cpu", "memory", "desired_count",
    "running_count", "pending_count",
}
if not isinstance(data.get("deployment"), dict) or set(data["deployment"]) != expected_deployment:
    fail("verified-image.json deployment shape is not fixed")

sha = field("image.source_label")
if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
    fail("reviewed image source label must be real lowercase 40-hex")
if field("cleanup_required") is not False:
    fail("cleanup_required=false is mandatory")
if field("image.platform") != "linux/arm64":
    fail("reviewed image source/platform mismatch")
if not re.fullmatch(r"sha256:[0-9a-f]{64}", str(field("image.config_id"))):
    fail("reviewed image config ID is invalid")
if not re.fullmatch(r"sha256:[0-9a-f]{64}", str(field("image.pushed_digest"))):
    fail("reviewed manifest digest is invalid")
if field("image.raw_manifest_media_type") != "application/vnd.docker.distribution.manifest.v2+json":
    fail("reviewed manifest media type mismatch")
if field("scan.policy_result") != "pass":
    fail("reviewed deterministic scan policy did not pass")

if field("deployment.cluster") != cluster or field("deployment.service") != service:
    fail("Task 8 cluster/service mismatch")
if field("deployment.gateway_container") != container:
    fail("Task 8 gateway container mismatch")
if field("deployment.region") != "us-east-2" or field("deployment.ecr_repository") != "pensyve-gateway":
    fail("Task 8 region/repository mismatch")
registry = str(field("deployment.ecr_registry"))
if not re.fullmatch(r"[0-9]{12}\.dkr\.ecr\.us-east-2\.amazonaws\.com", registry):
    fail("Task 8 ECR registry is invalid")
if field("deployment.cpu") != "512" or field("deployment.memory") != "4096":
    fail("Task 8 CPU/memory shape mismatch")
deployment_counts = (
    field("deployment.desired_count"), field("deployment.running_count"),
    field("deployment.pending_count"),
)
if any(not exact_int(value) for value in deployment_counts) or deployment_counts != (2, 2, 0):
    fail("Task 8 counts must remain exactly 2/2/0")
baseline_arn = str(field("deployment.baseline_task_definition_arn"))
baseline_image = str(field("deployment.baseline_image"))
account = registry.split(".", 1)[0]
if not re.fullmatch(rf"arn:aws:ecs:us-east-2:{account}:task-definition/pensyve-prod-gateway:[1-9][0-9]*", baseline_arn):
    fail("Task 8 baseline task definition ARN is invalid")
if not re.fullmatch(re.escape(registry) + r"/pensyve-gateway@sha256:[0-9a-f]{64}", baseline_image):
    fail("Task 8 baseline image is not the exact immutable digest URI")
if baseline_arn.endswith(":157"):
    fail("rejected task definition :157 must never be selected or used for rollback")
if not re.fullmatch(r"[0-9a-f]{64}", str(field("deployment.baseline_environment_sha256"))):
    fail("Task 8 environment identity is invalid")
snapshot = field("deployment.baseline_service_snapshot")
expected_snapshot_keys = {
    "service_name", "status", "cluster_arn", "task_definition", "counts",
    "network_configuration", "load_balancers", "deployment_configuration",
    "health_grace_period_seconds", "primary_deployment",
}
if not isinstance(snapshot, dict) or set(snapshot) != expected_snapshot_keys:
    fail("Task 8 canonical service snapshot shape is invalid")
if snapshot.get("service_name") != service or snapshot.get("status") != "ACTIVE":
    fail("Task 8 canonical service snapshot identity is invalid")
if not str(snapshot.get("cluster_arn", "")).endswith("/" + cluster):
    fail("Task 8 canonical service snapshot cluster is invalid")
counts = snapshot.get("counts")
if (snapshot.get("task_definition") != baseline_arn or not isinstance(counts, dict) or
        set(counts) != {"desired", "running", "pending"} or
        any(not exact_int(counts[name]) for name in counts) or
        counts != {"desired": 2, "running": 2, "pending": 0}):
    fail("Task 8 canonical service snapshot task/count is invalid")
primary = snapshot.get("primary_deployment")
if (not isinstance(primary, dict) or
        any(not exact_int(primary.get(name)) for name in ("desired", "running", "pending")) or primary != {
    "status": "PRIMARY", "task_definition": baseline_arn, "rollout_state": "COMPLETED",
    "desired": 2, "running": 2, "pending": 0,
}):
    fail("Task 8 canonical primary deployment is invalid")
if not isinstance(snapshot.get("network_configuration"), dict) or not isinstance(snapshot.get("load_balancers"), list):
    fail("Task 8 canonical service network/load-balancer shape is invalid")
if (not isinstance(snapshot.get("deployment_configuration"), dict) or
        not exact_int(snapshot.get("health_grace_period_seconds"))):
    fail("Task 8 canonical deployment configuration is invalid")
network = snapshot["network_configuration"]
if set(network) != {"awsvpcConfiguration"} or not isinstance(network["awsvpcConfiguration"], dict):
    fail("Task 8 canonical network configuration shape is invalid")
awsvpc = network["awsvpcConfiguration"]
if set(awsvpc) != {"subnets", "securityGroups", "assignPublicIp"}:
    fail("Task 8 canonical awsvpc configuration shape is invalid")
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
    fail("Task 8 canonical load-balancer binding shape is invalid")
load_balancer = load_balancers[0]
if (load_balancer["containerName"] != container or not exact_int(load_balancer["containerPort"]) or
        load_balancer["containerPort"] != 3100 or
        not re.fullmatch(rf"arn:aws:elasticloadbalancing:us-east-2:{account}:targetgroup/[0-9A-Za-z._/-]+", str(load_balancer["targetGroupArn"]))):
    fail("Task 8 canonical load-balancer binding is invalid")
configuration = snapshot["deployment_configuration"]
if set(configuration) != {"deploymentCircuitBreaker", "maximumPercent", "minimumHealthyPercent"}:
    fail("Task 8 canonical deployment configuration shape is invalid")
breaker = configuration["deploymentCircuitBreaker"]
if (not isinstance(breaker, dict) or set(breaker) != {"enable", "rollback"} or
        breaker.get("enable") is not True or breaker.get("rollback") is not True):
    fail("Task 8 canonical deployment circuit breaker is invalid")
if (not exact_int(configuration["maximumPercent"]) or configuration["maximumPercent"] < 100 or
        not exact_int(configuration["minimumHealthyPercent"]) or
        not 0 <= configuration["minimumHealthyPercent"] <= 100 or
        snapshot["health_grace_period_seconds"] < 0):
    fail("Task 8 canonical deployment percentages/grace are invalid")
canonical = (json.dumps(snapshot, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode()
if hashlib.sha256(canonical).hexdigest() != field("deployment.baseline_service_snapshot_sha256"):
    fail("Task 8 canonical service snapshot digest mismatch")
for name in ("promotion_run_id", "promotion_run_attempt"):
    value = field(f"deployment.{name}")
    if not exact_int(value) or value <= 0:
        fail(f"Task 8 {name} is invalid")
probe_entity = field("deployment.probe_entity")
probe_prefix = f"task9-runtime-{field('deployment.promotion_run_id')}-{field('deployment.promotion_run_attempt')}-"
if not re.fullmatch(re.escape(probe_prefix) + r"[0-9a-f]{16}", str(probe_entity)):
    fail("Task 9 probe entity is not sealed to this promotion run and attempt")
if not Path(field("image.archive_path")).is_file() or not Path(field("image.raw_manifest_path")).is_file():
    fail("reviewed archive/raw manifest is absent")
PY

TEMP_ROOT="$(mktemp -d /tmp/pensyve-gateway-promote.XXXXXX)" \
    || die "could not create the promotion temporary root"
readonly TEMP_ROOT

cleanup_temp() {
    rm -rf -- "${TEMP_ROOT}"
}

aws_call() {
    local aws_service="$1" aws_operation="$2"
    shift 2
    "${AWS_BIN}" "${aws_service}" "${aws_operation}" "$@" \
        --cli-connect-timeout 5 --cli-read-timeout 30
}

curl_call() {
    "${CURL_BIN}" --connect-timeout 5 --max-time 30 "$@"
}

trap cleanup_temp EXIT

source_sha="$(jq -r '.image.source_label' "${VERIFIED_IMAGE}")"
archive="$(jq -r '.image.archive_path' "${VERIFIED_IMAGE}")"
archive_sha="$(jq -r '.image.archive_sha256' "${VERIFIED_IMAGE}")"
config_id="$(jq -r '.image.config_id' "${VERIFIED_IMAGE}")"
manifest_file="$(jq -r '.image.raw_manifest_path' "${VERIFIED_IMAGE}")"
manifest_sha="$(jq -r '.image.raw_manifest_sha256' "${VERIFIED_IMAGE}")"
manifest_digest="$(jq -r '.image.pushed_digest' "${VERIFIED_IMAGE}")"
region="$(jq -r '.deployment.region' "${VERIFIED_IMAGE}")"
registry="$(jq -r '.deployment.ecr_registry' "${VERIFIED_IMAGE}")"
repository="$(jq -r '.deployment.ecr_repository' "${VERIFIED_IMAGE}")"
cluster="$(jq -r '.deployment.cluster' "${VERIFIED_IMAGE}")"
service="$(jq -r '.deployment.service' "${VERIFIED_IMAGE}")"
gateway_container="$(jq -r '.deployment.gateway_container' "${VERIFIED_IMAGE}")"
baseline_arn="$(jq -r '.deployment.baseline_task_definition_arn' "${VERIFIED_IMAGE}")"
baseline_image="$(jq -r '.deployment.baseline_image' "${VERIFIED_IMAGE}")"
environment_sha="$(jq -r '.deployment.baseline_environment_sha256' "${VERIFIED_IMAGE}")"
service_snapshot_sha="$(jq -r '.deployment.baseline_service_snapshot_sha256' "${VERIFIED_IMAGE}")"
promotion_run_id="$(jq -r '.deployment.promotion_run_id' "${VERIFIED_IMAGE}")"
promotion_run_attempt="$(jq -r '.deployment.promotion_run_attempt' "${VERIFIED_IMAGE}")"
probe_entity="$(jq -r '.deployment.probe_entity' "${VERIFIED_IMAGE}")"
target_tag="${registry}/${repository}:${source_sha}"
jq -S -c '.deployment.baseline_service_snapshot' "${VERIFIED_IMAGE}" > "${TEMP_ROOT}/expected-service-snapshot.json"

canonicalize_service_snapshot() {
    local service_response="$1" output="$2"
    jq -S -c '
      (.services // []) as $services |
      (if ($services | length) != 1 then error("service cardinality") else $services[0] end) as $service |
      [$service.deployments[] | select(.status == "PRIMARY")] as $primary |
      (if ($primary | length) != 1 then error("primary deployment cardinality") else $primary[0] end) as $deployment |
      {service_name:$service.serviceName,status:$service.status,cluster_arn:$service.clusterArn,
       task_definition:$service.taskDefinition,
       counts:{desired:$service.desiredCount,running:$service.runningCount,pending:$service.pendingCount},
       network_configuration:$service.networkConfiguration,
       load_balancers:($service.loadBalancers | sort_by(.targetGroupArn,.containerName,.containerPort)),
       deployment_configuration:$service.deploymentConfiguration,
       health_grace_period_seconds:$service.healthCheckGracePeriodSeconds,
       primary_deployment:{status:$deployment.status,task_definition:$deployment.taskDefinition,
         rollout_state:$deployment.rolloutState,desired:$deployment.desiredCount,
         running:$deployment.runningCount,pending:$deployment.pendingCount}}
    ' "${service_response}" > "${output}"
}

verify_task8_baseline() {
    local prefix="$1"
    local service_response="${TEMP_ROOT}/${prefix}-service.json"
    local task_response="${TEMP_ROOT}/${prefix}-task-response.json"
    local task="${TEMP_ROOT}/${prefix}-task.json"
    local environment_hash="${TEMP_ROOT}/${prefix}-environment.sha256"
    local current_snapshot="${TEMP_ROOT}/${prefix}-service-snapshot.json"
    VERIFY_ERROR=""

    aws_call ecs describe-services --region "${region}" --cluster "${cluster}" --services "${service}" \
        > "${service_response}" || {
        VERIFY_ERROR="Task 8 service describe failed"
        return 1
    }
    jq -e --arg cluster "${cluster}" --arg service "${service}" --arg baseline "${baseline_arn}" '
        .services | length == 1 and
        .[0].serviceName == $service and
        .[0].status == "ACTIVE" and
        (.[0].clusterArn | endswith("/" + $cluster)) and
        .[0].taskDefinition == $baseline and
        .[0].desiredCount == 2 and .[0].runningCount == 2 and .[0].pendingCount == 0
    ' "${service_response}" >/dev/null || {
        VERIFY_ERROR="Task 8 service/cluster/baseline/count drift"
        return 1
    }
    canonicalize_service_snapshot "${service_response}" "${current_snapshot}" || {
        VERIFY_ERROR="Task 8 canonical service snapshot could not be constructed"
        return 1
    }
    [[ "$(sha256sum "${current_snapshot}" | cut -d' ' -f1)" == "${service_snapshot_sha}" ]] &&
      cmp --silent "${current_snapshot}" "${TEMP_ROOT}/expected-service-snapshot.json" || {
        VERIFY_ERROR="Task 8 canonical service snapshot drift"
        return 1
    }

    aws_call ecs describe-task-definition --region "${region}" --task-definition "${baseline_arn}" \
        > "${task_response}" || {
        VERIFY_ERROR="Task 8 task definition describe failed"
        return 1
    }
    jq '.taskDefinition' "${task_response}" > "${task}" || {
        VERIFY_ERROR="Task 8 task definition response is invalid"
        return 1
    }
    jq -e --arg baseline "${baseline_arn}" --arg cpu "512" --arg memory "4096" \
        --arg container "${gateway_container}" --arg image "${baseline_image}" '
        .taskDefinitionArn == $baseline and .cpu == $cpu and .memory == $memory and
        ([.containerDefinitions[] | select(.name == $container)] | length == 1) and
        ([.containerDefinitions[] | select(.name == $container)][0].image == $image)
    ' "${task}" >/dev/null || {
        VERIFY_ERROR="Task 8 task definition shape/image drift"
        return 1
    }
    jq -S -c --arg container "${gateway_container}" \
        '[.containerDefinitions[] | select(.name == $container)][0].environment' "${task}" \
        | sha256sum | cut -d' ' -f1 > "${environment_hash}" || {
        VERIFY_ERROR="Task 8 environment could not be hashed"
        return 1
    }
    [[ "$(cat "${environment_hash}")" == "${environment_sha}" ]] || {
        VERIFY_ERROR="Task 8 environment drift"
        return 1
    }
    jq -e --arg container "${gateway_container}" '
        ([.containerDefinitions[] | select(.name == $container)][0].environment | map(.name) | index("MCP_ALLOWED_HOSTS")) != null
    ' "${task}" >/dev/null || {
        VERIFY_ERROR="Task 8 MCP_ALLOWED_HOSTS is absent; promotion will not repair it"
        return 1
    }
    jq -e --arg container "${gateway_container}" '
        ([.containerDefinitions[] | select(.name == $container)][0].secrets |
          map(select(.name == "PENSYVE_API_KEYS"))) as $keys |
        ($keys | length == 1) and ($keys[0].valueFrom | type == "string" and length > 0)
    ' "${task}" >/dev/null || {
        VERIFY_ERROR="Task 8 PENSYVE_API_KEYS secret binding drift"
        return 1
    }
}

verify_candidate_deployment() {
    local service_response="${TEMP_ROOT}/candidate-service.json"
    local list_response="${TEMP_ROOT}/candidate-running-tasks.json"
    local tasks_response="${TEMP_ROOT}/candidate-tasks.json"
    local expected_task_arns
    local candidate_snapshot="${TEMP_ROOT}/candidate-service-snapshot.json"
    local expected_candidate_snapshot="${TEMP_ROOT}/expected-candidate-service-snapshot.json"
    local -a task_arns=()
    VERIFY_ERROR=""

    aws_call ecs describe-services --region "${region}" --cluster "${cluster}" --services "${service}" \
        > "${service_response}" || {
        VERIFY_ERROR="candidate service describe failed"
        return 1
    }
    jq -e --arg cluster "${cluster}" --arg service "${service}" --arg candidate "${new_arn}" '
        .services | length == 1 and
        .[0].serviceName == $service and
        .[0].status == "ACTIVE" and
        (.[0].clusterArn | endswith("/" + $cluster)) and
        .[0].taskDefinition == $candidate and
        .[0].desiredCount == 2 and .[0].runningCount == 2 and .[0].pendingCount == 0 and
        ([.[0].deployments[] | select(.status == "PRIMARY")] | length == 1) and
        ([.[0].deployments[] | select(.status == "PRIMARY")][0] |
          .taskDefinition == $candidate and .rolloutState == "COMPLETED" and
          .desiredCount == 2 and .runningCount == 2 and .pendingCount == 0)
    ' "${service_response}" >/dev/null || {
        VERIFY_ERROR="candidate service/task/count/rollout drift"
        return 1
    }
    canonicalize_service_snapshot "${service_response}" "${candidate_snapshot}" || {
        VERIFY_ERROR="candidate canonical service snapshot could not be constructed"
        return 1
    }
    jq -S -c --arg candidate "${new_arn}" '
      .task_definition=$candidate | .primary_deployment.task_definition=$candidate
    ' "${TEMP_ROOT}/expected-service-snapshot.json" > "${expected_candidate_snapshot}"
    cmp --silent "${candidate_snapshot}" "${expected_candidate_snapshot}" || {
        VERIFY_ERROR="candidate canonical service snapshot drift"
        return 1
    }

    aws_call ecs list-tasks --region "${region}" --cluster "${cluster}" --service-name "${service}" \
        --desired-status RUNNING > "${list_response}" || {
        VERIFY_ERROR="candidate running task list failed"
        return 1
    }
    jq -e '.taskArns | length == 2 and (unique | length == 2)' "${list_response}" >/dev/null || {
        VERIFY_ERROR="candidate running task cardinality drift"
        return 1
    }
    mapfile -t task_arns < <(jq -r '.taskArns[]' "${list_response}")
    expected_task_arns="$(jq -c '.taskArns | sort' "${list_response}")"

    aws_call ecs describe-tasks --region "${region}" --cluster "${cluster}" --tasks "${task_arns[@]}" \
        > "${tasks_response}" || {
        VERIFY_ERROR="candidate task describe failed"
        return 1
    }
    jq -e --arg cluster "${cluster}" --arg candidate "${new_arn}" --arg container "${gateway_container}" \
        --arg digest "${ecr_digest}" --argjson expected_arns "${expected_task_arns}" '
        (.failures | length == 0) and (.tasks | length == 2) and
        ([.tasks[].taskArn] | sort == $expected_arns) and
        ([.tasks[].taskArn] | unique | length == 2) and
        ([.tasks[] | select(
          (.clusterArn | endswith("/" + $cluster)) and
          .taskDefinitionArn == $candidate and
          .lastStatus == "RUNNING" and .desiredStatus == "RUNNING" and
          ([.containers[] | select(.name == $container)] | length == 1) and
          ([.containers[] | select(.name == $container)][0].imageDigest == $digest)
        )] | length == 2)
    ' "${tasks_response}" >/dev/null || {
        VERIFY_ERROR="candidate task definition/image/status/cardinality drift"
        return 1
    }
}

verify_candidate_target_health() {
    local phase="$1"
    local service_response="${TEMP_ROOT}/candidate-service.json"
    local tasks_response="${TEMP_ROOT}/candidate-tasks.json"
    local target_group target_health="${TEMP_ROOT}/candidate-target-health-${phase}.json"
    target_group="$(jq -r --arg container "${gateway_container}" '
      [.services[0].loadBalancers[] | select(.containerName == $container and .containerPort == 3100) | .targetGroupArn] |
      if length == 1 then .[0] else empty end' "${service_response}")"
    [[ "${target_group}" == arn:aws:elasticloadbalancing:us-east-2:*:targetgroup/* ]] || {
        VERIFY_ERROR="candidate target-group binding is absent (${phase})"
        return 1
    }
    aws_call elbv2 describe-target-health --region "${region}" --target-group-arn "${target_group}" \
        > "${target_health}" || {
        VERIFY_ERROR="candidate target health describe failed (${phase})"
        return 1
    }
    jq -e --slurpfile tasks "${tasks_response}" '
      [$tasks[0].tasks[].attachments[].details[] | select(.name == "privateIPv4Address") | .value] as $ips |
      (($ips | length) == 2 and ($ips | unique | length) == 2) and
      ([.TargetHealthDescriptions[] | select(.TargetHealth.State == "healthy") | .Target.Id] | sort) == ($ips | sort)
    ' "${target_health}" >/dev/null || {
        VERIFY_ERROR="candidate target health/IP/cardinality drift (${phase})"
        return 1
    }
}

verify_candidate_functional_runtime() {
    local service_response="${TEMP_ROOT}/candidate-service.json"
    local tasks_response="${TEMP_ROOT}/candidate-tasks.json"
    local secret_ref secret_value api_key entity auth_config
    local log_group log_region stream_prefix log_configuration
    local -a task_rows=()
    VERIFY_ERROR=""
    jq -n --arg query "${bge_calibration_query}" \
      --argjson unreranked_order "${bge_calibration_unreranked_order}" \
      --argjson reranked_order "${bge_calibration_reranked_order}" \
      '{schema_version:1,source:"actual-gateway-pinned-GTE-BGE-offline-calibration",
        query:$query,unreranked_order:$unreranked_order,reranked_order:$reranked_order,
        requirement:"reranked full order must differ from and reject the dense-only order"}' \
      > "${TEMP_ROOT}/bge-calibration-contract.json"
    jq -e '.unreranked_order != .reranked_order and .reranked_order == ["target","neutral","decoy"]' \
      "${TEMP_ROOT}/bge-calibration-contract.json" >/dev/null || {
        VERIFY_ERROR="actual-gateway BGE calibration contract is invalid"
        return 1
    }

    verify_candidate_target_health pre-recall || return 1

    log_configuration="$(jq -c --arg container "${gateway_container}" '
      [.taskDefinition.containerDefinitions[] | select(.name == $container)] |
      if length == 1 then .[0].logConfiguration else empty end
    ' "${TEMP_ROOT}/task-after-response.json")" || {
        VERIFY_ERROR="candidate reviewed awslogs configuration is absent"
        return 1
    }
    jq -e --arg region "${region}" '
      .logDriver == "awslogs" and (.options | type == "object") and
      (.options["awslogs-group"] | type == "string" and startswith("/ecs/") and length > 5) and
      .options["awslogs-region"] == $region and
      (.options["awslogs-stream-prefix"] | type == "string" and test("^[A-Za-z0-9._/-]+$") and
       (contains("..") | not))
    ' <<<"${log_configuration}" >/dev/null || {
        VERIFY_ERROR="candidate reviewed awslogs group/region/stream-prefix drift"
        return 1
    }
    log_group="$(jq -r '.options["awslogs-group"]' <<<"${log_configuration}")"
    log_region="$(jq -r '.options["awslogs-region"]' <<<"${log_configuration}")"
    stream_prefix="$(jq -r '.options["awslogs-stream-prefix"]' <<<"${log_configuration}")"

    mapfile -t task_rows < <(jq -c --arg container "${gateway_container}" --arg prefix "${stream_prefix}" '
      .tasks[] |
      ([.containers[] | select(.name == $container)] | if length == 1 then .[0] else empty end) as $gateway |
      (.taskArn | split("/")[-1]) as $task_id |
      {task_arn:.taskArn,task_id:$task_id,container_name:$gateway.name,
       stream:($prefix + "/" + $gateway.name + "/" + $task_id),started_at:.startedAt}
    ' "${tasks_response}")
    [[ "${#task_rows[@]}" -eq 2 ]] && printf '%s\n' "${task_rows[@]}" | jq -s -e --arg container "${gateway_container}" '
      length == 2 and ([.[].task_arn] | unique | length == 2) and
      ([.[].task_id] | unique | length == 2) and ([.[].stream] | unique | length == 2) and
      all(.[]; . as $row | .container_name == $container and
        (.task_arn | type == "string" and endswith("/" + $row.task_id)) and
        (.task_id | test("^[0-9a-f]{32}$")) and (.started_at | type == "string" and length > 0))
    ' >/dev/null || {
        VERIFY_ERROR="candidate task/log-stream binding cardinality drift"
        return 1
    }

    fetch_stream_events() {
        local task_arn="$1" stream="$2" started_at="$3" phase="$4" output="$5"
        local started_ms end_ms token="" next_token page=0 page_file events_file
        local -A seen_tokens=()
        [[ -n "${task_arn}" && -n "${stream}" && -n "${started_at}" ]] || return 1
        started_ms="$(python3 - "${started_at}" <<'PY'
import sys
from datetime import datetime
print(int(datetime.fromisoformat(sys.argv[1].replace("Z", "+00:00")).timestamp() * 1000))
PY
)" || return 1
        end_ms="$(date -u +%s%3N)"
        events_file="${TEMP_ROOT}/${phase}-$(printf '%s' "${task_arn}" | sha256sum | cut -c1-16).events.jsonl"
        : > "${events_file}"
        while [[ "${page}" -lt 4 ]]; do
            page=$((page + 1))
            page_file="${TEMP_ROOT}/${phase}-$(printf '%s' "${stream}" | sha256sum | cut -c1-16)-${page}.json"
            local -a token_args=()
            [[ -z "${token}" ]] || token_args=(--next-token "${token}")
            aws_call logs get-log-events --region "${log_region}" --log-group-name "${log_group}" \
              --log-stream-name "${stream}" --start-time "${started_ms}" --end-time "${end_ms}" \
              --limit 1000 --start-from-head --no-paginate "${token_args[@]}" > "${page_file}" || return 1
            jq -e --argjson start "${started_ms}" --argjson end "${end_ms}" '
              (.events | type == "array") and all(.events[]; (.timestamp | type == "number") and .timestamp >= $start and .timestamp <= $end)
            ' "${page_file}" >/dev/null || return 1
            jq -c '.events[]' "${page_file}" >> "${events_file}"
            next_token="$(jq -r '.nextForwardToken // empty' "${page_file}")"
            [[ -n "${next_token}" && "${next_token}" != "${token}" ]] || break
            [[ "${page}" -lt 4 ]] || return 1
            [[ -z "${seen_tokens[${next_token}]:-}" ]] || return 1
            seen_tokens["${next_token}"]=1
            token="${next_token}"
        done
        jq -s --arg task "${task_arn}" --arg stream "${stream}" --argjson start "${started_ms}" \
          --argjson end "${end_ms}" '{task_arn:$task,log_stream:$stream,started_ms:$start,end_ms:$end,events:.}' \
          "${events_file}" > "${output}"
    }

    validate_one_init() {
        local events="$1"
        jq -e --arg gte "${EXPECTED_GTE_REVISION}" '
          [.events[] | (.message | fromjson?) | (.fields // .) |
            select(.message == "model runtime initialized")] as $records |
          ($records | length) == 1 and
          ($records[0] | .strict_local_models == false and
            .embedding_model == "Alibaba-NLP/gte-base-en-v1.5" and .embedding_revision == $gte and
            .reranker_state == "deferred" and .reranker_model == "BGERerankerBase" and
            .reranker_revision == "resolved-on-first-use" and .cache_root == "/opt/pensyve/models" and
            .embedding_pool_size == 1)
        ' "${events}" >/dev/null
    }

    local startup_attempt startup_ok row task_arn task_id stream started_at log_file
    for startup_attempt in $(seq 1 12); do
        startup_ok=1
        unset seen_tasks seen_streams
        declare -A seen_tasks=() seen_streams=()
        for row in "${task_rows[@]}"; do
            task_arn="$(jq -r '.task_arn' <<<"${row}")"
            task_id="$(jq -r '.task_id' <<<"${row}")"
            stream="$(jq -r '.stream' <<<"${row}")"
            started_at="$(jq -r '.started_at' <<<"${row}")"
            [[ -z "${seen_tasks[${task_arn}]:-}" && -z "${seen_streams[${stream}]:-}" ]] || startup_ok=0
            seen_tasks["${task_arn}"]=1
            seen_streams["${stream}"]=1
            log_file="${TEMP_ROOT}/startup-${startup_attempt}-$(printf '%s' "${task_arn}" | sha256sum | cut -c1-16).json"
            if ! fetch_stream_events "${task_arn}" "${stream}" "${started_at}" "startup-${startup_attempt}" "${log_file}" ||
               ! validate_one_init "${log_file}"; then
                startup_ok=0
            fi
        done
        [[ "${startup_ok}" -eq 1 ]] && break
        [[ "${startup_attempt}" -eq 12 ]] || "${SLEEP_BIN}" 5
    done
    if [[ "${startup_ok}" -ne 1 ]]; then
        VERIFY_ERROR="candidate startup did not prove exact baked GTE/deferred BGE on both tasks"
        return 1
    fi

    secret_ref="$(jq -r --arg container "${gateway_container}" \
      '[.containerDefinitions[] | select(.name == $container)][0].secrets[] | select(.name == "PENSYVE_API_KEYS") | .valueFrom' \
      "${TEMP_ROOT}/task-before.json")"
    if [[ "${secret_ref}" == arn:aws:secretsmanager:* ]]; then
        secret_value="$(aws_call secretsmanager get-secret-value --region "${region}" --secret-id "${secret_ref}" --query SecretString --output text)" || {
            VERIFY_ERROR="Task 8 API key secret lookup failed"
            return 1
        }
    else
        secret_value="$(aws_call ssm get-parameter --region "${region}" --name "${secret_ref}" --with-decryption --query Parameter.Value --output text)" || {
            VERIFY_ERROR="Task 8 API key parameter lookup failed"
            return 1
        }
    fi
    api_key="${secret_value%%,*}"
    api_key="${api_key#"${api_key%%[![:space:]]*}"}"
    api_key="${api_key%"${api_key##*[![:space:]]}"}"
    [[ "${api_key}" =~ ^psy_[A-Za-z0-9_-]{16,}$ ]] || {
        VERIFY_ERROR="Task 8 API key material is absent or invalid"
        return 1
    }
    printf '::add-mask::%s\n' "${api_key}"
    auth_config="${TEMP_ROOT}/curl-auth.conf"
    umask 077
    printf 'header = "Authorization: Bearer %s"\nheader = "Content-Type: application/json"\n' "${api_key}" > "${auth_config}"
    entity="${probe_entity}"
    jq -n --arg entity "${entity}" --arg fact "The unrelated decoy describes a sourdough recipe using rye flour and warm water." \
      '{entity:$entity,fact:$fact,confidence:1.0}' > "${TEMP_ROOT}/remember-one.json"
    jq -n --arg entity "${entity}" --arg fact "The production reranker proof explicitly marks codename ORCHID as the selected result." \
      '{entity:$entity,fact:$fact,confidence:1.0}' > "${TEMP_ROOT}/remember-two.json"
    jq -n --arg entity "${entity}" --arg fact "A neutral third record mentions astronomy and the rings of Saturn." \
      '{entity:$entity,fact:$fact,confidence:1.0}' > "${TEMP_ROOT}/remember-three.json"
    jq -n --arg entity "${entity}" --arg query "${bge_calibration_query}" \
      '{entity:$entity,query:$query,limit:3}' > "${TEMP_ROOT}/recall.json"
    curl_call --fail --silent --show-error "${EXPECTED_GATEWAY_URL}/v1/health" > "${TEMP_ROOT}/health.json" || {
        VERIFY_ERROR="candidate public health request failed"
        return 1
    }
    jq -e '.status == "ok" and (.version | type == "string" and length > 0)' "${TEMP_ROOT}/health.json" >/dev/null || {
        VERIFY_ERROR="candidate public health response drift"
        return 1
    }
    local probe_armed=0 probe_cleaned=0 probe_cleanup_error=""
    cleanup_probe_once() {
        [[ "${probe_armed}" -eq 1 ]] || return 0
        # The production workflow delegates cleanup to the independent,
        # credentialed always() custody job so cancellation cannot orphan it.
        [[ "${PROMOTION_CUSTODY:-local}" != deferred ]] || return 0
        [[ "${probe_cleaned}" -eq 0 ]] || {
            probe_cleanup_error="candidate controlled cleanup ran more than once"
            return 1
        }
        probe_cleaned=1
        if ! curl_call --config "${auth_config}" --fail --silent --show-error --request DELETE \
          "${EXPECTED_GATEWAY_URL}/v1/entities/${entity}" > "${TEMP_ROOT}/forget-response.json"; then
            probe_cleanup_error="candidate controlled cleanup request failed"
            return 1
        fi
        if ! jq -e '.forgotten_count == 3' "${TEMP_ROOT}/forget-response.json" >/dev/null; then
            probe_cleanup_error="candidate controlled cleanup count drift"
            return 1
        fi
    }
    fail_probe() {
        local primary_error="$1"
        if ! cleanup_probe_once; then
            VERIFY_ERROR="${primary_error}; ${probe_cleanup_error}"
        else
            VERIFY_ERROR="${primary_error}"
        fi
        return 1
    }
    probe_armed=1
    curl_call --config "${auth_config}" --fail --silent --show-error --request POST \
      --data-binary "@${TEMP_ROOT}/remember-one.json" "${EXPECTED_GATEWAY_URL}/v1/remember" > "${TEMP_ROOT}/remember-one-response.json" || {
        fail_probe "candidate first controlled remember failed"
        return 1
    }
    curl_call --config "${auth_config}" --fail --silent --show-error --request POST \
      --data-binary "@${TEMP_ROOT}/remember-two.json" "${EXPECTED_GATEWAY_URL}/v1/remember" > "${TEMP_ROOT}/remember-two-response.json" || {
        fail_probe "candidate second controlled remember failed"
        return 1
    }
    curl_call --config "${auth_config}" --fail --silent --show-error --request POST \
      --data-binary "@${TEMP_ROOT}/remember-three.json" "${EXPECTED_GATEWAY_URL}/v1/remember" > "${TEMP_ROOT}/remember-three-response.json" || {
        fail_probe "candidate third controlled remember failed"
        return 1
    }
    local first_id second_id third_id recall_status=0
    first_id="$(jq -r '.id // empty' "${TEMP_ROOT}/remember-one-response.json")"
    second_id="$(jq -r '.id // empty' "${TEMP_ROOT}/remember-two-response.json")"
    third_id="$(jq -r '.id // empty' "${TEMP_ROOT}/remember-three-response.json")"
    [[ "${first_id}" =~ ^[0-9a-fA-F-]{36}$ && "${second_id}" =~ ^[0-9a-fA-F-]{36}$ &&
       "${third_id}" =~ ^[0-9a-fA-F-]{36}$ && "${first_id}" != "${second_id}" &&
       "${first_id}" != "${third_id}" && "${second_id}" != "${third_id}" ]] || {
        fail_probe "candidate controlled remember IDs are absent, invalid, or duplicated"
        return 1
    }
    curl_call --config "${auth_config}" --fail --silent --show-error --request POST \
      --data-binary "@${TEMP_ROOT}/recall.json" "${EXPECTED_GATEWAY_URL}/v1/recall" > "${TEMP_ROOT}/recall-response.json" \
      || recall_status=$?
    if [[ "${recall_status}" -ne 0 ]]; then
        fail_probe "candidate controlled recall failed"
        return 1
    fi
    jq -e --arg target "${second_id}" --arg neutral "${third_id}" --arg decoy "${first_id}" '
      (.memories | type == "array" and length == 3) and
      [.memories[].id] == [$target,$neutral,$decoy] and ([.memories[].id] | unique | length == 3) and
      .memories[0].content == "The production reranker proof explicitly marks codename ORCHID as the selected result." and
      .memories[1].content == "A neutral third record mentions astronomy and the rings of Saturn." and
      .memories[2].content == "The unrelated decoy describes a sourdough recipe using rye flour and warm water."
    ' "${TEMP_ROOT}/recall-response.json" >/dev/null || {
        fail_probe "candidate controlled recall did not prove exact actual-gateway BGE calibration ordering"
        return 1
    }
    if ! cleanup_probe_once; then
        VERIFY_ERROR="${probe_cleanup_error}"
        return 1
    fi
    rm -f -- "${auth_config}" "${TEMP_ROOT}/remember-one.json" "${TEMP_ROOT}/remember-two.json" \
      "${TEMP_ROOT}/remember-three.json" "${TEMP_ROOT}/recall.json"

    "${SLEEP_BIN}" 30
    local -a post_log_files=()
    for row in "${task_rows[@]}"; do
        task_arn="$(jq -r '.task_arn' <<<"${row}")"
        stream="$(jq -r '.stream' <<<"${row}")"
        started_at="$(jq -r '.started_at' <<<"${row}")"
        log_file="${TEMP_ROOT}/post-recall-$(printf '%s' "${task_arn}" | sha256sum | cut -c1-16).json"
        fetch_stream_events "${task_arn}" "${stream}" "${started_at}" post-recall "${log_file}" || {
            VERIFY_ERROR="candidate bounded post-recall log fetch failed"
            return 1
        }
        validate_one_init "${log_file}" || {
            VERIFY_ERROR="candidate post-recall init record drift or duplicate"
            return 1
        }
        if jq -r '.events[].message' "${log_file}" | grep -E -i \
          'MiniLM|mock embedder|trying .*fallback|Reranker disabled|Reranker unavailable|resolution task panicked|recall proceeding unreranked|download' >/dev/null; then
            VERIFY_ERROR="candidate used forbidden model fallback/download path"
            return 1
        fi
        post_log_files+=("${log_file}")
    done
    jq -s -e --arg query "${bge_calibration_query}" '
      [.[].events[] | (.message | fromjson?) | (.fields // .) |
       select(.message == "recall completed" and .event == "recall_decision" and
         .query == $query and .candidates_found == 3 and .results_returned == 3)] | length == 1
    ' "${post_log_files[@]}" >/dev/null || {
        VERIFY_ERROR="candidate post-recall log evidence did not bind one exact calibrated recall"
        return 1
    }
    if ! verify_candidate_deployment || ! verify_candidate_target_health post-recall; then
        VERIFY_ERROR="candidate post-recall replacement/drift: ${VERIFY_ERROR}"
        return 1
    fi
}

finalize_custody() {
    local promotion_result="${PROMOTION_RESULT:-}"
    local service_response="${TEMP_ROOT}/finalizer-service.json"
    local baseline_task_response="${TEMP_ROOT}/finalizer-baseline-task.json"
    local live_arn live_task_response="${TEMP_ROOT}/finalizer-live-task.json"
    local state="unknown" cleanup_status=0 rollback_status=0 final_status=0
    local secret_ref secret_value api_key auth_config forgotten_count
    ecr_digest="${manifest_digest}"
    [[ "${promotion_result}" =~ ^(success|failure|cancelled|skipped)$ ]] \
      || die "finalizer requires exact PROMOTION_RESULT success/failure/cancelled/skipped"

    # Custody is sealed in verified-image.json. Re-describe both baseline and
    # live service state; never rely on producer step outputs that cancellation
    # can suppress.
    aws_call ecs describe-task-definition --region "${region}" --task-definition "${baseline_arn}" \
      > "${baseline_task_response}" || die "promotion-custody baseline task describe failed"
    aws_call ecs describe-services --region "${region}" --cluster "${cluster}" --services "${service}" \
      > "${service_response}" || die "promotion-custody live service describe failed"
    live_arn="$(jq -r '.services | if length == 1 then .[0].taskDefinition else empty end' "${service_response}")"
    [[ "${live_arn}" =~ ^arn:aws:ecs:us-east-2:[0-9]{12}:task-definition/pensyve-prod-gateway:[1-9][0-9]*$ &&
       "${live_arn}" != *:157 ]] || die "promotion-custody live task definition is invalid or rejected :157"

    if [[ "${live_arn}" == "${baseline_arn}" ]]; then
        if verify_task8_baseline finalizer-baseline; then
            state=baseline
        else
            die "promotion-custody baseline drift: ${VERIFY_ERROR}"
        fi
    else
        aws_call ecs describe-task-definition --region "${region}" --task-definition "${live_arn}" \
          > "${live_task_response}" || die "promotion-custody candidate task describe failed"
        jq -e --arg candidate "${live_arn}" --arg container "${gateway_container}" \
          --arg image "${registry}/${repository}@${manifest_digest}" \
          --arg cpu "512" --arg memory "4096" '
          .taskDefinition.taskDefinitionArn == $candidate and
          .taskDefinition.cpu == $cpu and .taskDefinition.memory == $memory and
          ([.taskDefinition.containerDefinitions[] | select(.name == $container)] | length == 1) and
          ([.taskDefinition.containerDefinitions[] | select(.name == $container)][0].image == $image)
        ' "${live_task_response}" >/dev/null || die "promotion-custody refuses unrelated live deployment"
        jq --arg container "${gateway_container}" --arg baseline "${baseline_image}" '
          .taskDefinition | del(.taskDefinitionArn,.revision,.status,.requiresAttributes,.compatibilities,.registeredAt,.registeredBy) |
          .containerDefinitions |= map(if .name == $container then .image=$baseline else . end)
        ' "${live_task_response}" | jq -S . > "${TEMP_ROOT}/finalizer-live-reverted.json"
        jq '.taskDefinition | del(.taskDefinitionArn,.revision,.status,.requiresAttributes,.compatibilities,.registeredAt,.registeredBy)' \
          "${baseline_task_response}" | jq -S . > "${TEMP_ROOT}/finalizer-baseline-canonical.json"
        cmp --silent "${TEMP_ROOT}/finalizer-live-reverted.json" "${TEMP_ROOT}/finalizer-baseline-canonical.json" \
          || die "promotion-custody candidate differs from Task 8 by more than gateway.image"
        new_arn="${live_arn}"
        cp -- "${live_task_response}" "${TEMP_ROOT}/task-after-response.json"
        state=candidate
        if [[ "${promotion_result}" == success ]]; then
            if ! verify_candidate_deployment || ! verify_candidate_target_health finalizer; then
                echo "promotion-custody success describe-back failed: ${VERIFY_ERROR}" >&2
                promotion_result=failure
                final_status=1
            fi
        fi
    fi

    # Cleanup is armed by the sealed entity before the producer's first
    # mutating remember. It runs exactly once here even after ambiguous first
    # remember, cancellation, signal escalation, or producer job timeout.
    secret_ref="$(jq -r --arg container "${gateway_container}" '
      [.taskDefinition.containerDefinitions[] | select(.name == $container)][0].secrets[] |
      select(.name == "PENSYVE_API_KEYS") | .valueFrom' "${baseline_task_response}")"
    if [[ "${secret_ref}" == arn:aws:secretsmanager:* ]]; then
        secret_value="$(aws_call secretsmanager get-secret-value --region "${region}" --secret-id "${secret_ref}" --query SecretString --output text)" \
          || cleanup_status=$?
    else
        secret_value="$(aws_call ssm get-parameter --region "${region}" --name "${secret_ref}" --with-decryption --query Parameter.Value --output text)" \
          || cleanup_status=$?
    fi
    if [[ "${cleanup_status}" -eq 0 ]]; then
        api_key="${secret_value%%,*}"
        api_key="${api_key#"${api_key%%[![:space:]]*}"}"
        api_key="${api_key%"${api_key##*[![:space:]]}"}"
        if [[ ! "${api_key}" =~ ^psy_[A-Za-z0-9_-]{16,}$ ]]; then
            cleanup_status=1
            echo "promotion-custody cleanup key is invalid" >&2
        else
            printf '::add-mask::%s\n' "${api_key}"
            auth_config="${TEMP_ROOT}/finalizer-curl-auth.conf"
            umask 077
            printf 'header = "Authorization: Bearer %s"\nheader = "Content-Type: application/json"\n' "${api_key}" > "${auth_config}"
            if ! curl_call --config "${auth_config}" --fail --silent --show-error --request DELETE \
              "${EXPECTED_GATEWAY_URL}/v1/entities/${probe_entity}" > "${TEMP_ROOT}/finalizer-forget.json"; then
                cleanup_status=1
                echo "promotion-custody synthetic entity cleanup request failed" >&2
            else
                forgotten_count="$(jq -r '.forgotten_count // empty' "${TEMP_ROOT}/finalizer-forget.json")"
                if [[ ! "${forgotten_count}" =~ ^[0-3]$ ||
                      ( "${promotion_result}" == success && "${forgotten_count}" != 3 ) ]]; then
                    cleanup_status=1
                    echo "promotion-custody exact forget count invalid: ${forgotten_count:-absent}" >&2
                else
                    echo "promotion-custody cleanup verified exact forgotten_count=${forgotten_count}" >&2
                fi
            fi
        fi
    fi

    if [[ "${state}" == candidate && "${promotion_result}" == success && "${cleanup_status}" -ne 0 ]]; then
        echo "promotion-custody successful producer failed exact Task 9 cleanup; rollback required" >&2
        promotion_result=failure
    fi

    # DELETE is itself inside the guarded Task 9 window.  Re-describe every
    # candidate binding after forgotten_count=3 so replacement or deployment
    # drift during cleanup cannot be reported as a successful promotion.
    if [[ "${state}" == candidate && "${promotion_result}" == success && "${cleanup_status}" -eq 0 ]]; then
        if ! verify_candidate_deployment || ! verify_candidate_target_health post-cleanup; then
            echo "cleanup-final-state-drift: ${VERIFY_ERROR}; exact Task 8 rollback required" >&2
            promotion_result=failure
            final_status=1
        fi
    fi

    if [[ "${state}" == candidate && "${promotion_result}" != success ]]; then
        aws_call ecs update-service --region "${region}" --cluster "${cluster}" --service "${service}" \
          --task-definition "${baseline_arn}" > "${TEMP_ROOT}/finalizer-rollback-update.json" || rollback_status=$?
        if [[ "${rollback_status}" -eq 0 ]]; then
            aws_call ecs wait services-stable --region "${region}" --cluster "${cluster}" --services "${service}" \
              > "${TEMP_ROOT}/finalizer-rollback-wait.log" 2>&1 || rollback_status=$?
        fi
        if [[ "${rollback_status}" -eq 0 ]]; then
            verify_task8_baseline finalizer-rollback || rollback_status=$?
        fi
        if [[ "${rollback_status}" -ne 0 ]]; then
            echo "promotion-custody rollback failed status=${rollback_status}; producer_result=${PROMOTION_RESULT}" >&2
        else
            echo "promotion-custody rollback verified exact Task 8 baseline" >&2
        fi
    elif [[ "${state}" == baseline && "${promotion_result}" == success ]]; then
        echo "promotion-custody detected circuit-breaker rollback after reported success" >&2
        final_status=1
    fi

    [[ "${cleanup_status}" -eq 0 ]] || final_status="${cleanup_status}"
    [[ "${rollback_status}" -eq 0 ]] || final_status="${rollback_status}"
    [[ "${final_status}" -eq 0 ]] || return "${final_status}"
    echo "promotion-custody finalized producer_result=${PROMOTION_RESULT} live_state=${state}"
}

if [[ "${MODE}" == finalize ]]; then
    finalize_custody
    exit 0
fi

[[ "$(sha256sum "${archive}" | cut -d' ' -f1)" == "${archive_sha}" ]] || die "archive checksum mismatch before promotion"
[[ "$(sha256sum "${manifest_file}" | cut -d' ' -f1)" == "${manifest_sha}" ]] || die "raw manifest hash mismatch before promotion"
[[ "sha256:${manifest_sha}" == "${manifest_digest}" ]] || die "reviewed raw manifest digest mismatch"

"${DOCKER_BIN}" load --input "${archive}" > "${TEMP_ROOT}/docker-load.log"
loaded_id="$("${DOCKER_BIN}" image inspect --format '{{.Id}}' "${config_id}")"
[[ "${loaded_id}" == "${config_id}" ]] || die "loaded archive config ID mismatch"

VERIFY_ERROR=""
verify_task8_baseline before || die "${VERIFY_ERROR}"
cp -- "${TEMP_ROOT}/before-task.json" "${TEMP_ROOT}/task-before.json"

set +e
aws_call ecr describe-images --region "${region}" --repository-name "${repository}" \
    --image-ids "imageTag=${source_sha}" > "${TEMP_ROOT}/existing-tag.json" 2> "${TEMP_ROOT}/existing-tag.err"
existing_status=$?
set -e
if [[ "${existing_status}" -eq 0 ]]; then
    existing_digest="$(jq -r '.imageDetails | if length == 1 then .[0].imageDigest else empty end' "${TEMP_ROOT}/existing-tag.json")"
    [[ "${existing_digest}" == "${manifest_digest}" ]] || die "existing exact-SHA ECR tag points at different bytes"
elif ! grep -F 'ImageNotFoundException' "${TEMP_ROOT}/existing-tag.err" >/dev/null; then
    die "could not authoritatively determine whether the exact-SHA ECR tag already exists"
fi

"${DOCKER_BIN}" tag "${config_id}" "${target_tag}"
"${DOCKER_BIN}" push "${target_tag}" > "${TEMP_ROOT}/docker-push.log"
grep -F "${manifest_digest}" "${TEMP_ROOT}/docker-push.log" >/dev/null || die "Docker push digest mismatch"
aws_call ecr describe-images --region "${region}" --repository-name "${repository}" \
    --image-ids "imageTag=${source_sha}" > "${TEMP_ROOT}/ecr-image.json"
ecr_digest="$(jq -r '.imageDetails | if length == 1 then .[0].imageDigest else empty end' "${TEMP_ROOT}/ecr-image.json")"
[[ "${ecr_digest}" == "${manifest_digest}" ]] || die "ECR digest differs from reviewed loopback digest"
aws_call ecr batch-get-image --region "${region}" --repository-name "${repository}" \
    --image-ids "imageDigest=${ecr_digest}" --accepted-media-types "${EXPECTED_MEDIA_TYPE}" \
    > "${TEMP_ROOT}/ecr-manifest-response.json"
python3 - "${TEMP_ROOT}/ecr-manifest-response.json" "${TEMP_ROOT}/ecr-manifest.raw" <<'PY'
import json
import sys
from pathlib import Path
data = json.loads(Path(sys.argv[1]).read_text())
images = data.get("images", [])
if len(images) != 1:
    raise SystemExit("ECR batch-get-image did not return exactly one image")
Path(sys.argv[2]).write_text(images[0]["imageManifest"])
PY
[[ "$(jq -r '.images[0].imageManifestMediaType' "${TEMP_ROOT}/ecr-manifest-response.json")" == "${EXPECTED_MEDIA_TYPE}" ]] \
    || die "ECR manifest media type mismatch"
cmp --silent "${manifest_file}" "${TEMP_ROOT}/ecr-manifest.raw" || die "ECR manifest bytes differ from reviewed loopback manifest"
[[ "$(sha256sum "${TEMP_ROOT}/ecr-manifest.raw" | cut -d' ' -f1)" == "${manifest_sha}" ]] \
    || die "ECR manifest hash mismatch"

digest_uri="${registry}/${repository}@${ecr_digest}"
jq 'del(.taskDefinitionArn,.revision,.status,.requiresAttributes,.compatibilities,.registeredAt,.registeredBy)' \
    "${TEMP_ROOT}/task-before.json" > "${TEMP_ROOT}/register-before.json"
jq --arg container "${gateway_container}" --arg image "${digest_uri}" '
    .containerDefinitions |= map(if .name == $container then .image = $image else . end)
' "${TEMP_ROOT}/register-before.json" > "${TEMP_ROOT}/register.json"
jq --arg container "${gateway_container}" --arg old "${baseline_image}" '
    .containerDefinitions |= map(if .name == $container then .image = $old else . end)
' "${TEMP_ROOT}/register.json" | jq -S . > "${TEMP_ROOT}/register-reverted.json"
jq -S . "${TEMP_ROOT}/register-before.json" > "${TEMP_ROOT}/register-before.canonical.json"
cmp --silent "${TEMP_ROOT}/register-reverted.json" "${TEMP_ROOT}/register-before.canonical.json" \
    || die "canonical task definition mutation changed more than gateway.image"

aws_call ecs register-task-definition --region "${region}" --cli-input-json "file://${TEMP_ROOT}/register.json" \
    > "${TEMP_ROOT}/register-response.json"
new_arn="$(jq -r '.taskDefinition.taskDefinitionArn' "${TEMP_ROOT}/register-response.json")"
[[ -n "${new_arn}" && "${new_arn}" != "null" && "${new_arn}" != *":157" ]] || die "registered task definition ARN is invalid or rejected :157"
aws_call ecs describe-task-definition --region "${region}" --task-definition "${new_arn}" \
    > "${TEMP_ROOT}/task-after-response.json"
jq '.taskDefinition | del(.taskDefinitionArn,.revision,.status,.requiresAttributes,.compatibilities,.registeredAt,.registeredBy)' \
    "${TEMP_ROOT}/task-after-response.json" | jq -S . > "${TEMP_ROOT}/task-after.canonical.json"
jq -S . "${TEMP_ROOT}/register.json" > "${TEMP_ROOT}/register.canonical.json"
cmp --silent "${TEMP_ROOT}/task-after.canonical.json" "${TEMP_ROOT}/register.canonical.json" \
    || die "described registered task definition differs from canonical image-only request"

VERIFY_ERROR=""
verify_task8_baseline preupdate || die "pre-update Task 8 drift: ${VERIFY_ERROR}"

updated=0
promotion_committed=0
rollback_started=0
finalize_promotion() {
    local original_status=$? rollback_status=0 rollback_stage=""
    trap - EXIT TERM INT
    set +e
    if [[ "${PROMOTION_CUSTODY:-local}" == deferred ]]; then
        if [[ "${updated}" -eq 1 && "${promotion_committed}" -eq 0 ]]; then
            echo "gateway promotion custody deferred: original status=${original_status}; independent finalizer owns rollback and cleanup" >&2
        fi
        cleanup_temp
        if [[ "${updated}" -eq 1 && "${promotion_committed}" -eq 0 && "${original_status}" -eq 0 ]]; then
            exit 1
        fi
        exit "${original_status}"
    fi
    if [[ "${updated}" -eq 1 && "${promotion_committed}" -eq 0 ]]; then
        if [[ "${rollback_started}" -eq 1 ]]; then
            echo "gateway promotion rollback error: original status=${original_status}; second rollback refused" >&2
            cleanup_temp
            exit 1
        fi
        rollback_started=1
        rollback_stage="update"
        aws_call ecs update-service --region "${region}" --cluster "${cluster}" --service "${service}" \
            --task-definition "${baseline_arn}" > "${TEMP_ROOT}/rollback-update.json"
        rollback_status=$?
        if [[ "${rollback_status}" -eq 0 ]]; then
            rollback_stage="wait"
            aws_call ecs wait services-stable --region "${region}" --cluster "${cluster}" --services "${service}" \
                > "${TEMP_ROOT}/rollback-wait.log" 2>&1
            rollback_status=$?
        fi
        if [[ "${rollback_status}" -ne 0 && "${rollback_stage}" == update ]]; then
            echo "gateway promotion rollback error: original candidate failure status=${original_status}; rollback update failed status=${rollback_status}" >&2
        elif [[ "${rollback_status}" -ne 0 ]]; then
            echo "gateway promotion rollback error: original candidate failure status=${original_status}; rollback wait failed status=${rollback_status}" >&2
        elif ! verify_task8_baseline rollback; then
            rollback_status=$?
            [[ "${rollback_status}" -ne 0 ]] || rollback_status=1
            echo "gateway promotion rollback error: original candidate failure status=${original_status}; rollback describe-back verification failed status=${rollback_status}: ${VERIFY_ERROR}" >&2
        else
            echo "gateway promotion rollback: original candidate failure status=${original_status}; rollback verified exact Task 8 baseline" >&2
        fi
    fi
    cleanup_temp
    if [[ "${rollback_status}" -ne 0 ]]; then
        exit "${rollback_status}"
    fi
    if [[ "${updated}" -eq 1 && "${promotion_committed}" -eq 0 && "${original_status}" -eq 0 ]]; then
        exit 1
    fi
    exit "${original_status}"
}
trap finalize_promotion EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
updated=1
aws_call ecs update-service --region "${region}" --cluster "${cluster}" --service "${service}" \
    --task-definition "${new_arn}" > "${TEMP_ROOT}/update-response.json"
aws_call ecs wait services-stable --region "${region}" --cluster "${cluster}" --services "${service}" \
    > "${TEMP_ROOT}/wait.log"
VERIFY_ERROR=""
if ! verify_candidate_deployment; then
    echo "gateway promotion error: candidate describe-back verification failed: ${VERIFY_ERROR}" >&2
    false
fi
if ! verify_candidate_functional_runtime; then
    echo "gateway promotion error: candidate functional runtime verification failed: ${VERIFY_ERROR}" >&2
    false
fi
promotion_committed=1
updated=0

echo "gateway promotion completed for ${source_sha} at ${ecr_digest}"
