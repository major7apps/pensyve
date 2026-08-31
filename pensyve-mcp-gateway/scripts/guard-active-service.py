#!/usr/bin/env python3
"""Read-only fail-closed guard for the production gateway's active ECS base."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from typing import Any

REGION = "us-east-2"
CLUSTER = "pensyve-prod"
SERVICE = "pensyve-prod-gateway"
TASK_ARN = re.compile(
    r"^arn:[^:]+:ecs:[^:]+:[0-9]{12}:task-definition/pensyve-prod-gateway:[0-9]+$"
)
IMMUTABLE_IMAGE = re.compile(r"^[^@\s]+@sha256:[0-9a-f]{64}$")
REQUIRED_ENVIRONMENT = {
    "PENSYVE_REQUIRE_LOCAL_MODELS": "1",
    "HF_HOME": "/opt/pensyve/models",
    "FASTEMBED_CACHE_DIR": "/opt/pensyve/models",
    "PENSYVE_EMBEDDING_POOL_SIZE": "1",
}
FORBIDDEN_ENVIRONMENT = {"PENSYVE_ALLOW_MOCK_EMBEDDER", "PENSYVE_RERANKER"}
AUTHORITATIVE_ENVIRONMENT = set(REQUIRED_ENVIRONMENT) | FORBIDDEN_ENVIRONMENT


def fail(message: str) -> None:
    raise ValueError(f"active base contract violated: {message}")


def object_value(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    return value


def list_value(value: Any, label: str) -> list[Any]:
    if not isinstance(value, list):
        fail(f"{label} must be a list")
    return value


def aws_json(aws_bin: str, *arguments: str) -> dict[str, Any]:
    command = [aws_bin, *arguments, "--output", "json"]
    try:
        result = subprocess.run(command, check=True, capture_output=True, text=True)
    except (OSError, subprocess.CalledProcessError) as error:
        stderr = getattr(error, "stderr", "") or ""
        fail(f"AWS read failed: {' '.join(arguments)}: {stderr.strip()}")
    try:
        return object_value(json.loads(result.stdout), "AWS response")
    except json.JSONDecodeError as error:
        fail(f"AWS read returned invalid JSON: {error}")


def validate_service(payload: dict[str, Any], expected_arn: str | None) -> str:
    if payload.get("failures") not in (None, []):
        fail("describe-services returned failures")
    services = list_value(payload.get("services"), "services")
    if len(services) != 1:
        fail("describe-services must return exactly one service")
    service = object_value(services[0], "service")
    if [service.get(key) for key in ("desiredCount", "runningCount", "pendingCount")] != [2, 2, 0]:
        fail("desired/running/pending must be exactly 2/2/0")
    if service.get("healthCheckGracePeriodSeconds") != 300:
        fail("health grace must be exactly 300")

    deployments = list_value(service.get("deployments"), "deployments")
    if len(deployments) != 1:
        fail("service must have exactly one completed PRIMARY deployment")
    primary = object_value(deployments[0], "primary deployment")
    if primary.get("status") != "PRIMARY" or primary.get("rolloutState") != "COMPLETED":
        fail("service must have exactly one completed PRIMARY deployment")

    active_arn = service.get("taskDefinition")
    if not isinstance(active_arn, str) or not TASK_ARN.fullmatch(active_arn):
        fail("active task definition ARN is invalid")
    if active_arn.endswith(":157"):
        fail("rejected task definition :157 must never be selected or used")
    if primary.get("taskDefinition") != active_arn:
        fail("PRIMARY deployment task definition does not equal the active ARN")
    if expected_arn is not None and active_arn != expected_arn:
        fail("active task definition drifted from expected ARN")
    return active_arn


def validate_task(payload: dict[str, Any], active_arn: str) -> None:
    task = object_value(payload.get("taskDefinition"), "taskDefinition")
    if task.get("taskDefinitionArn") != active_arn:
        fail("described task definition does not equal the active ARN")
    if active_arn.endswith(":157") or task.get("revision") == 157:
        fail("rejected task definition :157 must never be selected or used")
    if task.get("cpu") != "512" or task.get("memory") != "4096":
        fail("task cpu/memory must be exactly 512/4096")
    runtime = object_value(task.get("runtimePlatform"), "runtimePlatform")
    if runtime.get("cpuArchitecture") != "ARM64":
        fail("runtime CPU architecture must be ARM64")

    containers = list_value(task.get("containerDefinitions"), "containerDefinitions")
    gateways = [
        item for item in containers if isinstance(item, dict) and item.get("name") == "gateway"
    ]
    if len(gateways) != 1:
        fail("task must contain exactly one gateway container")
    gateway = gateways[0]
    image = gateway.get("image")
    if not isinstance(image, str) or not IMMUTABLE_IMAGE.fullmatch(image):
        fail("gateway image must be an immutable sha256 digest")

    environment_files = list_value(gateway.get("environmentFiles", []), "gateway environmentFiles")
    if environment_files:
        fail("gateway environmentFiles must be absent or empty")

    secrets = list_value(gateway.get("secrets", []), "gateway secrets")
    for raw_secret in secrets:
        secret = object_value(raw_secret, "gateway secret")
        name = secret.get("name")
        value_from = secret.get("valueFrom")
        if not isinstance(name, str) or not isinstance(value_from, str):
            fail("gateway secrets must have string names and valueFrom values")
        if name in AUTHORITATIVE_ENVIRONMENT:
            fail(f"authoritative gateway key must not be supplied via secrets: {name}")

    environment = list_value(gateway.get("environment"), "gateway environment")
    values: dict[str, str] = {}
    seen: set[str] = set()
    for raw_entry in environment:
        entry = object_value(raw_entry, "gateway environment entry")
        name = entry.get("name")
        value = entry.get("value")
        if not isinstance(name, str) or not isinstance(value, str):
            fail("gateway environment entries must have string names and values")
        if name in AUTHORITATIVE_ENVIRONMENT and name in seen:
            fail(f"duplicate authoritative environment key: {name}")
        seen.add(name)
        values[name] = value
    for name in sorted(FORBIDDEN_ENVIRONMENT):
        if name in values:
            fail(f"forbidden gateway environment key: {name}")
    for name, expected_value in REQUIRED_ENVIRONMENT.items():
        if values.get(name) != expected_value:
            fail(f"gateway environment mismatch for {name}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--expected-task-arn")
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
        active_arn = validate_service(service, arguments.expected_task_arn)
        task = aws_json(
            aws_bin,
            "ecs",
            "describe-task-definition",
            "--region",
            REGION,
            "--task-definition",
            active_arn,
        )
        validate_task(task, active_arn)
    except ValueError as error:
        print(error, file=sys.stderr)
        return 1
    print(active_arn)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
