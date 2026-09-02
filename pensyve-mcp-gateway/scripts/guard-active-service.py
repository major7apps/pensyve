#!/usr/bin/env python3
"""Fail-closed, read-only guard for exact production gateway derivations."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

REGION = "us-east-2"
CLUSTER = "pensyve-prod"
SERVICE = "pensyve-prod-gateway"
FAMILY = "pensyve-prod-gateway"
CONTAINER = "gateway"
SOURCE_REVISION = 156
SOURCE_IMAGE = (
    "196881464893.dkr.ecr.us-east-2.amazonaws.com/"
    "pensyve-gateway:63011d55f8cbf52f6f9e5609621f6b8cf0c37535"
)
TASK_ARN = re.compile(
    r"^arn:aws:ecs:us-east-2:([0-9]{12}):task-definition/pensyve-prod-gateway:([0-9]+)$"
)
IMMUTABLE_IMAGE = re.compile(
    r"^([0-9]{12})\.dkr\.ecr\.us-east-2\.amazonaws\.com/"
    r"pensyve-gateway@sha256:[0-9a-f]{64}$"
)
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
TARGET_GROUP_ARN = re.compile(
    r"^arn:aws:elasticloadbalancing:us-east-2:([0-9]{12}):"
    r"targetgroup/pensyve-prod-gw-tg/[0-9a-f]{16}$"
)
PERMISSIVE_ENVIRONMENT = {"PENSYVE_ALLOW_MOCK_EMBEDDER": "1"}
STRICT_ONLY_ENVIRONMENT = {
    "PENSYVE_REQUIRE_LOCAL_MODELS",
    "HF_HOME",
    "FASTEMBED_CACHE_DIR",
    "PENSYVE_EMBEDDING_POOL_SIZE",
    "PENSYVE_RERANKER",
}
GENERATED_FIELDS = {
    "taskDefinitionArn",
    "revision",
    "status",
    "requiresAttributes",
    "compatibilities",
    "registeredAt",
    "registeredBy",
    "deregisteredAt",
}


def fail(message: str) -> None:
    raise ValueError(f"active service contract violated: {message}")


def as_object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    return value


def as_list(value: Any, label: str) -> list[Any]:
    if not isinstance(value, list):
        fail(f"{label} must be a list")
    return value


def aws_json(aws_bin: str, *arguments: str) -> dict[str, Any]:
    command = [
        aws_bin,
        *arguments,
        "--cli-connect-timeout",
        "5",
        "--cli-read-timeout",
        "30",
        "--output",
        "json",
    ]
    try:
        result = subprocess.run(
            command,
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
    except subprocess.TimeoutExpired:
        fail(f"AWS read timed out: {' '.join(arguments)}")
    except (OSError, subprocess.CalledProcessError) as error:
        stderr = getattr(error, "stderr", "") or ""
        fail(f"AWS read failed: {' '.join(arguments)}: {stderr.strip()}")
    try:
        return as_object(json.loads(result.stdout), "AWS response")
    except json.JSONDecodeError as error:
        fail(f"AWS read returned invalid JSON: {error}")


def arn_parts(arn: str, label: str) -> tuple[str, int]:
    match = TASK_ARN.fullmatch(arn)
    if not match:
        fail(f"{label} task definition ARN is invalid")
    revision = int(match.group(2))
    if revision == 157:
        fail("rejected revision 157 must never be selected or used for rollback")
    return match.group(1), revision


def validate_service(
    payload: dict[str, Any], expected_arn: str | None, expected_target_group_arn: str | None
) -> tuple[str, str]:
    if payload.get("failures") not in (None, []):
        fail("describe-services returned failures")
    services = as_list(payload.get("services"), "services")
    if len(services) != 1:
        fail("describe-services must return exactly one service")
    service = as_object(services[0], "service")
    if service.get("serviceName") != SERVICE or service.get("status") != "ACTIVE":
        fail("service identity/status mismatch")
    desired = service.get("desiredCount")
    running = service.get("runningCount")
    pending = service.get("pendingCount")
    if type(desired) is not int or not 2 <= desired <= 4:
        fail("desiredCount must be an exact integer in range 2..4")
    if type(running) is not int or running != desired:
        fail("runningCount must equal desiredCount")
    if type(pending) is not int or pending != 0:
        fail("pendingCount must be exactly 0")
    counts = [desired, running, pending]
    deployments = as_list(service.get("deployments"), "deployments")
    if len(deployments) != 1:
        fail("service must have a single deployment")
    primary = as_object(deployments[0], "PRIMARY deployment")
    primary_counts = [
        primary.get(name) for name in ("desiredCount", "runningCount", "pendingCount")
    ]
    if (
        primary.get("status") != "PRIMARY"
        or primary.get("rolloutState") != "COMPLETED"
        or any(type(value) is not int for value in primary_counts)
    ):
        fail("service must have one completed PRIMARY deployment")
    if primary_counts != counts:
        fail("PRIMARY deployment counts must equal service counts")
    active_arn = service.get("taskDefinition")
    if not isinstance(active_arn, str):
        fail("active task definition ARN is absent")
    arn_parts(active_arn, "active")
    if primary.get("taskDefinition") != active_arn:
        fail("PRIMARY deployment task definition does not equal the active ARN")
    if expected_arn is not None and active_arn != expected_arn:
        fail("active task definition drifted from expected-current ARN")
    account, _ = arn_parts(active_arn, "active")
    load_balancers = as_list(service.get("loadBalancers"), "loadBalancers")
    if len(load_balancers) != 1:
        fail("service must have exactly one load balancer binding")
    binding = as_object(load_balancers[0], "load balancer binding")
    target_group_arn = binding.get("targetGroupArn")
    target_match = TARGET_GROUP_ARN.fullmatch(str(target_group_arn))
    if (
        set(binding) != {"targetGroupArn", "containerName", "containerPort"}
        or not target_match
        or target_match.group(1) != account
        or binding.get("containerName") != CONTAINER
        or binding.get("containerPort") != 3000
        or type(binding.get("containerPort")) is not int
    ):
        fail("service load balancer binding is not the exact production gateway target group")
    if expected_target_group_arn is not None and target_group_arn != expected_target_group_arn:
        fail("service target group drifted from expected target group ARN")
    return active_arn, str(target_group_arn)


def gateway_container(task: dict[str, Any]) -> dict[str, Any]:
    containers = as_list(task.get("containerDefinitions"), "containerDefinitions")
    gateways = [
        value for value in containers if isinstance(value, dict) and value.get("name") == CONTAINER
    ]
    if len(gateways) != 1:
        fail("task must contain exactly one gateway container")
    return gateways[0]


def environment_values(task: dict[str, Any]) -> dict[str, str]:
    gateway = gateway_container(task)
    if as_list(gateway.get("environmentFiles", []), "gateway environmentFiles"):
        fail("gateway environmentFiles must be absent or empty")
    values: dict[str, str] = {}
    for raw in as_list(gateway.get("environment"), "gateway environment"):
        entry = as_object(raw, "gateway environment entry")
        name, value = entry.get("name"), entry.get("value")
        if not isinstance(name, str) or not isinstance(value, str) or name in values:
            fail("gateway environment entries must have unique string names/values")
        values[name] = value
    return values


def validate_permissive_environment(task: dict[str, Any]) -> None:
    values = environment_values(task)
    for name, expected in PERMISSIVE_ENVIRONMENT.items():
        if values.get(name) != expected:
            fail(f"permissive gateway environment mismatch for {name}")
    forbidden = STRICT_ONLY_ENVIRONMENT.intersection(values)
    if forbidden:
        fail(f"permissive gateway environment contains strict-only key: {sorted(forbidden)[0]}")
    gateway = gateway_container(task)
    for raw in as_list(gateway.get("secrets", []), "gateway secrets"):
        secret = as_object(raw, "gateway secret")
        if secret.get("name") in set(PERMISSIVE_ENVIRONMENT) | STRICT_ONLY_ENVIRONMENT:
            fail("model-mode gateway environment must not be supplied through secrets")


def validate_task(task: dict[str, Any], expected_arn: str) -> str:
    if task.get("taskDefinitionArn") != expected_arn or task.get("family") != FAMILY:
        fail("described task definition identity mismatch")
    account, revision = arn_parts(expected_arn, "described")
    if task.get("revision") != revision or type(task.get("revision")) is not int:
        fail("described task revision mismatch")
    runtime = as_object(task.get("runtimePlatform"), "runtimePlatform")
    if runtime != {"cpuArchitecture": "ARM64", "operatingSystemFamily": "LINUX"}:
        fail("runtime platform must be exact ARM64/Linux")
    gateway = gateway_container(task)
    image = gateway.get("image")
    image_match = IMMUTABLE_IMAGE.fullmatch(str(image))
    if image != SOURCE_IMAGE and (
        not image_match or image_match.group(1) != account or ":latest" in str(image)
    ):
        fail("gateway image must be the exact source tag or account's immutable digest URI")
    if revision == SOURCE_REVISION and image != SOURCE_IMAGE:
        fail("source revision 156 must use the exact reviewed source image tag")
    if (
        task.get("taskRoleArn") != f"arn:aws:iam::{account}:role/pensyve-prod-task"
        or task.get("executionRoleArn")
        != f"arn:aws:iam::{account}:role/pensyve-prod-task-execution"
    ):
        fail("task definition must use the exact production task and execution roles")
    environment_values(task)
    return str(image)


def describe_task(aws_bin: str, arn: str) -> dict[str, Any]:
    payload = aws_json(
        aws_bin,
        "ecs",
        "describe-task-definition",
        "--region",
        REGION,
        "--task-definition",
        arn,
    )
    task = as_object(payload.get("taskDefinition"), "taskDefinition")
    validate_task(task, arn)
    return task


def normalized(task: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in task.items() if key not in GENERATED_FIELDS}


def require_task8_derivation(task8: dict[str, Any], source: dict[str, Any]) -> None:
    candidate = normalized(task8)
    expected = normalized(source)
    expected["cpu"] = "512"
    expected["memory"] = "4096"
    if candidate != expected:
        fail("Task 8 baseline is not the exact CPU/memory-only derivation from revision 156")


def canonical_custody(path: Path) -> dict[str, Any]:
    raw = path.read_bytes()
    try:
        data = as_object(json.loads(raw), "custody")
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        fail(f"custody JSON is invalid: {error}")
    canonical = (
        json.dumps(data, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n"
    ).encode()
    if raw != canonical:
        fail("custody JSON is not canonical UTF-8 with one trailing newline")
    if set(data) != {"source", "image", "evidence", "publisher"}:
        fail("custody top-level shape mismatch")
    source = as_object(data.get("source"), "custody source")
    if (
        source.get("schema_version") != 1
        or type(source.get("schema_version")) is not int
        or source.get("repository") != "major7apps/pensyve"
        or not HEX40.fullmatch(str(source.get("sha", "")))
        or not HEX40.fullmatch(str(source.get("tree", "")))
    ):
        fail("custody source identity mismatch")
    image = as_object(data.get("image"), "custody image")
    required_image = {
        "account",
        "registry",
        "repository",
        "manifest_digest",
        "config_digest",
        "platform",
        "raw_manifest_media_type",
        "raw_manifest_sha256",
    }
    if set(image) != required_image:
        fail("custody image shape mismatch")
    if image.get("repository") != "pensyve-gateway" or image.get("platform") != "linux/arm64":
        fail("custody repository/platform mismatch")
    account = str(image.get("account", ""))
    if not re.fullmatch(r"[0-9]{12}", account) or image.get("registry") != (
        f"{account}.dkr.ecr.us-east-2.amazonaws.com"
    ):
        fail("custody account/registry mismatch")
    for name in ("manifest_digest", "config_digest"):
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", str(image.get(name, ""))):
            fail(f"custody {name} mismatch")
    if image.get("raw_manifest_media_type") != (
        "application/vnd.docker.distribution.manifest.v2+json"
    ) or not HEX64.fullmatch(str(image.get("raw_manifest_sha256", ""))):
        fail("custody raw manifest identity mismatch")
    evidence = as_object(data.get("evidence"), "custody evidence")
    if set(evidence) != {
        "archive_sha256",
        "evidence_tree_sha256",
        "scan_report_sha256",
        "scan_policy_sha256",
        "gate_summary_sha256",
    } or not all(HEX64.fullmatch(str(value)) for value in evidence.values()):
        fail("custody evidence hash shape mismatch")
    publisher = as_object(data.get("publisher"), "custody publisher")
    expected_arn = f"arn:aws:sts::{account}:federated-user/pensyve-gateway-{source['sha']}"
    if (
        set(publisher) != {"arn", "inline_session_policy_sha256"}
        or publisher.get("arn") != expected_arn
        or not HEX64.fullmatch(str(publisher.get("inline_session_policy_sha256", "")))
    ):
        fail("custody publisher identity mismatch")
    return data


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("source-156", "task8-baseline", "task9-candidate"))
    parser.add_argument("--expected-current-arn")
    parser.add_argument("--baseline-arn")
    parser.add_argument("--custody")
    parser.add_argument("--expected-target-group-arn")
    parser.add_argument("--target-group-output")
    arguments = parser.parse_args()
    aws_bin = os.environ.get("AWS_BIN", "aws")
    try:
        service = aws_json(
            aws_bin,
            "ecs",
            "describe-services",
            "--region",
            REGION,
            "--cluster",
            CLUSTER,
            "--services",
            SERVICE,
        )
        active_arn, target_group_arn = validate_service(
            service, arguments.expected_current_arn, arguments.expected_target_group_arn
        )
        active = describe_task(aws_bin, active_arn)
        _, revision = arn_parts(active_arn, "active")
        if arguments.mode == "source-156":
            if revision != SOURCE_REVISION:
                fail("active source task definition must be exact revision 156")
            validate_permissive_environment(active)
        else:
            source_arn = active_arn.rsplit(":", 1)[0] + f":{SOURCE_REVISION}"
            source = describe_task(aws_bin, source_arn)
            validate_permissive_environment(source)
            if arguments.mode == "task8-baseline":
                if revision in (SOURCE_REVISION, 157):
                    fail("Task 8 baseline must be a new derived revision")
                require_task8_derivation(active, source)
            else:
                if not arguments.baseline_arn or not arguments.custody:
                    fail("Task 9 guard requires baseline ARN and custody JSON")
                arn_parts(arguments.baseline_arn, "Task 8 baseline")
                baseline = describe_task(aws_bin, arguments.baseline_arn)
                require_task8_derivation(baseline, source)
                custody = canonical_custody(Path(arguments.custody))
                expected = normalized(baseline)
                containers = expected.get("containerDefinitions")
                if not isinstance(containers, list):
                    fail("Task 8 baseline containerDefinitions are invalid")
                gateways = [item for item in containers if item.get("name") == CONTAINER]
                if len(gateways) != 1:
                    fail("Task 8 baseline must contain exactly one gateway")
                image = custody["image"]
                gateways[0]["image"] = (
                    f"{image['registry']}/{image['repository']}@{image['manifest_digest']}"
                )
                if normalized(active) != expected:
                    fail("Task 9 candidate is not the exact image-only derivation from baseline")
        if arguments.target_group_output:
            Path(arguments.target_group_output).write_text(
                target_group_arn + "\n", encoding="utf-8"
            )
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    print(active_arn)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
