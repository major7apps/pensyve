#!/usr/bin/env bash

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly ARTIFACT_SCRIPT="${SCRIPT_DIR}/gateway-image-artifact.sh"
readonly RELEASE_SCRIPT="${SCRIPT_DIR}/test-gateway-release-image.sh"
readonly PROMOTE_SCRIPT="${SCRIPT_DIR}/promote-gateway-image.sh"
readonly FETCH_SCRIPT="${SCRIPT_DIR}/fetch-model-bundle.sh"
readonly MODEL_TEST="${SCRIPT_DIR}/test-model-bundle.sh"
readonly WORKFLOW="${REPO_ROOT}/.github/workflows/deploy-gateway.yml"
readonly CI_WORKFLOW="${REPO_ROOT}/.github/workflows/ci.yml"
readonly DOCKERFILE="${REPO_ROOT}/pensyve-mcp-gateway/Dockerfile"
readonly CASE="${1:-all}"
readonly SOURCE_SHA="0123456789abcdef0123456789abcdef01234567"
readonly SCANNER_DIGEST="sha256:55ad20f8a239a3e95427e60b8aaea38788550c18a3f1772976bebf732e6ae166"
readonly SCANNER_VERSION="0.74.0"

case "${CASE}" in
    structural | artifact | seal | storage | reviewed-pr | deployment | release-scan | promote | cleanup | handoff | round4-review | round5-review | round6-review | round9-review | round10-review | round11-review | round12-review | round13-review | round14-review | round15-review | all) ;;
    *) echo "usage: $0 [structural|artifact|seal|storage|reviewed-pr|deployment|release-scan|promote|cleanup|handoff|round4-review|round5-review|round6-review|round9-review|round10-review|round11-review|round12-review|round13-review|round14-review|round15-review|all]" >&2; exit 2 ;;
esac

readonly TEST_ROOT="$(mktemp -d /tmp/pensyve-artifact-flow.XXXXXX)"
trap 'rm -rf -- "${TEST_ROOT}"' EXIT

fail() {
    echo "artifact-flow test failure: $*" >&2
    return 1
}

expect_failure() {
    local expected="$1"
    shift
    local output="${TEST_ROOT}/expected-failure.$RANDOM.log"
    if "$@" >"${output}" 2>&1; then
        fail "mutation unexpectedly passed: $*"
    fi
    if ! grep -F -- "${expected}" "${output}" >/dev/null; then
        cat "${output}" >&2
        fail "mutation failure did not name '${expected}': $*"
    fi
}

capture_failure() {
    local output="$1"
    shift
    set +e
    "$@" >"${output}" 2>&1
    local status=$?
    set -e
    [[ "${status}" -ne 0 ]] || fail "expected command failure: $*"
}

require_scripts() {
    local path
    for path in "${ARTIFACT_SCRIPT}" "${RELEASE_SCRIPT}" "${PROMOTE_SCRIPT}" "${FETCH_SCRIPT}" "${MODEL_TEST}"; do
        [[ -x "${path}" ]] || fail "required executable script is absent: ${path}"
    done
}

validate_workflow() {
    local release_path="${2:-${RELEASE_SCRIPT}}"
    local artifact_path="${3:-${ARTIFACT_SCRIPT}}"
    local promote_path="${4:-${PROMOTE_SCRIPT}}"
    python3 - "$1" "$CI_WORKFLOW" "$DOCKERFILE" "$artifact_path" "$release_path" "$promote_path" <<'PY'
import re
import sys
from pathlib import Path

import yaml

workflow_path = Path(sys.argv[1])
ci_path = Path(sys.argv[2])
dockerfile_path = Path(sys.argv[3])
artifact_script_path = Path(sys.argv[4])
release_script_path = Path(sys.argv[5])
promote_script_path = Path(sys.argv[6])
workflow = yaml.load(workflow_path.read_text(), Loader=yaml.BaseLoader)
ci = yaml.load(ci_path.read_text(), Loader=yaml.BaseLoader)
errors = []

def mapping(value):
    return value if isinstance(value, dict) else {}

def job_text(job):
    return "\n".join(
        str(value)
        for step in mapping(job).get("steps", [])
        for value in mapping(step).values()
    )

def named_step(job, name):
    matches = [mapping(step) for step in mapping(job).get("steps", []) if mapping(step).get("name") == name]
    if len(matches) != 1:
        errors.append(f"missing or duplicate workflow step: {name}")
        return {}
    return matches[0]

authority_action_pins = {
    "actions/upload-artifact": ("ea165f8d65b6e75b540449e92b4886f43607fa02", 2),
    "aws-actions/configure-aws-credentials": ("e6de054238d6b7531b4efff3b6587d9aade6a06c", 3),
    "aws-actions/amazon-ecr-login": ("03f1aad4c6c7ffd436567f42f9384779290529bd", 2),
}
all_uses = [str(step.get("uses", "")) for job in mapping(workflow.get("jobs")).values()
            for step in mapping(job).get("steps", [])]
for action, (commit, expected_count) in authority_action_pins.items():
    matches = [value for value in all_uses if value.startswith(action + "@")]
    if len(matches) != expected_count or any(value != f"{action}@{commit}" for value in matches):
        if action == "actions/upload-artifact":
            errors.append("artifact-build must upload exactly once and the handoff exactly once using the verified action commit")
        else:
            errors.append(f"Task4 authority action must be pinned twice to verified commit: {action}")

root_permissions = mapping(workflow.get("permissions"))
if root_permissions:
    errors.append("workflow-level permissions must be empty")

on = mapping(workflow.get("on"))
dispatch = mapping(on.get("workflow_dispatch"))
inputs = mapping(dispatch.get("inputs"))
mode = mapping(inputs.get("mode"))
options = mode.get("options", [])
if mode.get("required") != "true" or set(options) != {"artifact-build", "artifact-promote", "artifact-custodian"}:
    errors.append("workflow_dispatch must expose one required mutually exclusive mode")
pull_request_input = mapping(inputs.get("pull_request_number"))
if pull_request_input.get("required") != "true":
    errors.append("workflow_dispatch must require pull_request_number")
if pull_request_input.get("description") != "Exact same-repository open non-draft pull request number":
    errors.append("workflow_dispatch must describe the full open non-draft pull request contract")
if "push" not in on:
    errors.append("push-main trigger is missing")

jobs = mapping(workflow.get("jobs"))
required_jobs = {
    "artifact-build",
    "artifact-promote-preflight",
    "artifact-promote-dispatch",
    "artifact-promote",
    "artifact-custodian-producer",
    "artifact-custodian-finalize",
    "artifact-cleanup",
    "push-main-test",
    "push-main-deploy",
}
missing = sorted(required_jobs - set(jobs))
if missing:
    errors.append(f"missing artifact modes/scripts jobs: {','.join(missing)}")

for job_name, job_value in jobs.items():
    steps = [mapping(step) for step in mapping(job_value).get("steps", [])]
    repo_script_indexes = [i for i, step in enumerate(steps)
                           if "pensyve-mcp-gateway/scripts/" in str(step.get("run", ""))]
    if not repo_script_indexes:
        continue
    checkout_indexes = [i for i, step in enumerate(steps)
                        if str(step.get("uses", "")).startswith("actions/checkout@")]
    if len(checkout_indexes) != 1:
        errors.append(f"{job_name} must have exactly one checkout before repository scripts")
        continue
    checkout_index = checkout_indexes[0]
    checkout = steps[checkout_index]
    checkout_with = mapping(checkout.get("with"))
    if (checkout.get("uses") != "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1" or
            checkout_with.get("ref") != "${{ github.sha }}" or
            checkout_with.get("persist-credentials") != "false"):
        errors.append(f"{job_name} repository scripts require pinned exact-SHA checkout without persisted credentials")
    if any(index <= checkout_index for index in repo_script_indexes):
        errors.append(f"{job_name} invokes a repository script before checkout")

read_permissions = {
    "contents": "read",
    "actions": "read",
    "pull-requests": "read",
}
for name in ("artifact-build", "artifact-promote-preflight"):
    job = mapping(jobs.get(name))
    if mapping(job.get("permissions")) != read_permissions:
        errors.append(f"{name} must have exact contents/actions/pull-requests read")
    text = job_text(job)
    if "pulls/${" not in text or "gh api" not in text:
        errors.append(f"{name} must use authenticated Get PR by exact number")
    if re.search(r"pulls(?:\?|\s|$)", text) or "--search" in text or "gh pr list" in text:
        errors.append(f"{name} must not list/search PRs")
    if any(token in text for token in ("configure-aws-credentials", "aws sts", "aws ecr", "aws ecs")):
        errors.append(f"{name} has AWS before verified preflight")
    if "environment" in job or mapping(job.get("permissions")).get("id-token"):
        errors.append(f"{name} has elevated production authority")

build = mapping(jobs.get("artifact-build"))
preflight = mapping(jobs.get("artifact-promote-preflight"))
for job_name, job, resolver_id in (
    ("artifact-build", build, "build-custody-resolver"),
    ("artifact-promote-preflight", preflight, "handoff-custody-resolver"),
):
    outputs_text = str(mapping(job.get("outputs")))
    if "||" in outputs_text or f"steps.{resolver_id}.outputs" not in outputs_text:
        errors.append(f"{job_name} outputs must come only from its terminal custody resolver")
    resolver_matches = [mapping(step) for step in job.get("steps", []) if mapping(step).get("id") == resolver_id]
    if len(resolver_matches) != 1 or "always()" not in str(resolver_matches[0].get("if", "")):
        errors.append(f"{job_name} must have one terminal always custody resolver")
    elif resolver_matches[0] is not job.get("steps", [])[-1]:
        errors.append(f"{job_name} custody resolver must be the final step")
    resolver_text = str(resolver_matches[0].get("run", "")) if resolver_matches else ""
    lookup_marker = "source-upload-name-lookup" if job_name == "artifact-build" else "handoff-upload-name-lookup"
    quiescence_marker = "source-upload-quiescence" if job_name == "artifact-build" else "handoff-upload-quiescence"
    if ("post-upload-invalid" not in resolver_text or "post-upload-unresolved" not in resolver_text or
            "cleanup=true" not in resolver_text or resolver_text.count("cleanup=false") != 1 or
            lookup_marker not in resolver_text or quiescence_marker not in resolver_text or
            'for attempt in 1 2 3' not in resolver_text or '[[ "$matches" == 1 ]]' not in resolver_text):
        errors.append(f"{job_name} resolver must default uploaded custody true and clear it only at terminal success")

if build.get("name") != "Build one reviewed non-draft ARM64 artifact":
    errors.append("artifact-build job must name the reviewed non-draft authority")
build_pr = str(named_step(jobs.get("artifact-build"), "Bind exact open non-draft pull request before checkout").get("run", ""))
for predicate in (
    '.state == "open"', '.draft == false', '.base.ref == "main"',
    '.base.repo.full_name == $repo', '.head.repo.full_name == $repo',
    '.head.ref == $branch', '.head.sha == $sha',
):
    if predicate not in build_pr:
        errors.append(f"artifact-build missing full PR predicate: {predicate}")

promotion_pr_step = named_step(jobs.get("artifact-promote-preflight"), "Bind promotion run and exact Task 5 pull request before checkout")
promotion_pr = str(promotion_pr_step.get("run", ""))
promotion_pr_env = mapping(promotion_pr_step.get("env"))
if promotion_pr_env.get("REVIEWED_PR_STATE") != "${{ vars.TASK5_REVIEWED_PR_STATE }}" or promotion_pr_env.get("REVIEWED_PR_DRAFT") != "${{ vars.TASK5_REVIEWED_PR_DRAFT }}":
    errors.append("promotion preflight must bind Task 5-reviewed PR state/draft")
for predicate in (
    '"$REVIEWED_PR_STATE" == open', '"$REVIEWED_PR_DRAFT" == false',
    '.state == $reviewed_state', '.draft == $reviewed_draft', '.base.ref == "main"',
    '.base.repo.full_name == $repo', '.head.repo.full_name == $repo',
    '.head.ref == $ref', '.head.sha == $sha',
):
    if predicate not in promotion_pr:
        errors.append(f"promotion preflight missing Task 5 PR predicate: {predicate}")

dispatcher = mapping(jobs.get("artifact-promote-dispatch"))
dispatcher_permissions = mapping(dispatcher.get("permissions"))
if dispatcher_permissions != {"actions": "write"}:
    errors.append("artifact-promote dispatcher must have exact actions-write-only authority")
needs = dispatcher.get("needs", [])
if isinstance(needs, str):
    needs = [needs]
if needs != ["artifact-promote-preflight"]:
    errors.append("artifact-promote dispatcher must depend only on successful preflight")
if "environment" in dispatcher or "id-token" in dispatcher_permissions:
    errors.append("artifact-promote dispatcher must not hold production environment or OIDC authority")
dispatcher_text = job_text(dispatcher)
for token in ("artifact-custodian", "custody_lease_id", "custody_request", "workflow_dispatch",
              "gateway-custodian-${lease}", "exact custodian run identity is ambiguous"):
    if token not in dispatcher_text:
        errors.append(f"artifact-promote dispatcher missing exact custodian dispatch/run binding: {token}")
if any(token in dispatcher_text for token in ("promote-gateway-image.sh", "configure-aws-credentials", "aws ", "docker ")):
    errors.append("artifact-promote dispatcher contains forbidden production/build authority")
dispatcher_step = named_step(dispatcher, "Dispatch one exact-ref custodian and bind its exact run")
dispatcher_env = mapping(dispatcher_step.get("env"))
dispatcher_run = str(dispatcher_step.get("run", ""))
if (dispatcher_env.get("INPUT_MODE") != "${{ inputs.mode }}" or
        dispatcher_env.get("PR_NUMBER") != "${{ inputs.pull_request_number }}"):
    errors.append("artifact-promote dispatcher must bind workflow inputs through explicit environment values")
if "${{ inputs." in dispatcher_run:
    errors.append("artifact-promote dispatcher shell must not interpolate GitHub inputs directly")
for predicate in ('"$INPUT_MODE" == artifact-promote', '"$PR_NUMBER" =~ ^[1-9][0-9]*$'):
    if predicate not in dispatcher_run:
        errors.append(f"artifact-promote dispatcher input validation is missing: {predicate}")

promote = mapping(jobs.get("artifact-promote"))
promote_permissions = mapping(promote.get("permissions"))
if promote_permissions != {"actions": "read"}:
    errors.append("artifact-promote observer must have exact actions-read-only authority")
promote_needs = promote.get("needs", [])
if not isinstance(promote_needs, list) or promote_needs != ["artifact-promote-dispatch"]:
    errors.append("artifact-promote observer must depend only on exact dispatcher outputs")
if "environment" in promote or "id-token" in promote_permissions or promote.get("timeout-minutes") != "70":
    errors.append("artifact-promote observer must be bounded and have no production authority")
promote_text = job_text(promote)
for token in ("CUSTODIAN_RUN_ID", "CUSTODY_LEASE_ID", "actions/runs/${CUSTODIAN_RUN_ID}",
              '.conclusion'):
    if token not in promote_text:
        errors.append(f"artifact-promote observer is not bound to the exact custodian terminal result: {token}")

workflow_concurrency = mapping(workflow.get("concurrency"))
concurrency_group = str(workflow_concurrency.get("group", ""))
if (workflow_concurrency.get("cancel-in-progress") != "false" or
        "pensyve-production-gateway" not in concurrency_group or
        "artifact-custodian" not in concurrency_group or "github.event_name == 'push'" not in concurrency_group or
        "pensyve-gateway-nonproduction" not in concurrency_group):
    errors.append("workflow must give custodian/push-main one non-canceling production lease and unique nonproduction groups")

producer = mapping(jobs.get("artifact-custodian-producer"))
producer_permissions = mapping(producer.get("permissions"))
if producer_permissions != {"contents": "read", "actions": "read", "id-token": "write"}:
    errors.append("artifact-custodian producer must have exact read plus existing production OIDC permissions")
if producer.get("environment") != "production" or producer.get("timeout-minutes") != "45" or "concurrency" in producer:
    errors.append("artifact-custodian producer must use the leased production environment with a fixed 45-minute timeout")
producer_text = job_text(producer)
if "promote-gateway-image.sh promote" not in producer_text or "custodian-ready" not in producer_text:
    errors.append("artifact-custodian producer must bind exact dispatched custody then call the reviewed promotion script")
if any(token in producer_text for token in ("docker build", ":latest")):
    errors.append("artifact-custodian producer must not rebuild or select latest")
producer_steps = [mapping(step) for step in producer.get("steps", [])]
credential_indexes = [i for i, step in enumerate(producer_steps) if str(step.get("uses", "")).startswith("aws-actions/configure-aws-credentials@")]
handoff_indexes = [i for i, step in enumerate(producer_steps) if "gateway-image-artifact.sh verify-handoff" in str(step.get("run", ""))]
if len(credential_indexes) != 1 or len(handoff_indexes) != 1 or handoff_indexes[0] >= credential_indexes[0]:
    errors.append("full verified-image, storage authority, and Task 8 validation must precede custodian producer credentials")

finalizer = mapping(jobs.get("artifact-custodian-finalize"))
finalizer_permissions = mapping(finalizer.get("permissions"))
if finalizer_permissions != {"contents": "read", "actions": "read", "id-token": "write"}:
    errors.append("artifact-custodian finalizer must have exact read plus existing production OIDC permissions")
finalizer_needs = finalizer.get("needs", [])
if not isinstance(finalizer_needs, list) or finalizer_needs != ["artifact-custodian-producer"]:
    errors.append("artifact-custodian finalizer must depend only on the custodian producer outcome")
if "always()" not in str(finalizer.get("if", "")) or "artifact-custodian" not in str(finalizer.get("if", "")):
    errors.append("artifact-custodian finalizer must run after every producer outcome")
if finalizer.get("environment") != "production" or "concurrency" in finalizer:
    errors.append("artifact-custodian finalizer must remain inside the workflow-level production lease")
if finalizer.get("timeout-minutes") != "20":
    errors.append("artifact-custodian finalizer must have a bounded 20-minute timeout")
finalizer_text = job_text(finalizer)
if ("needs.artifact-custodian-producer.result" not in finalizer_text or "PROMOTION_RESULT" not in finalizer_text or
        "promote-gateway-image.sh finalize" not in finalizer_text or "promotion-custody" not in finalizer_text):
    errors.append("artifact-custodian finalizer must derive sealed custody and exact producer result")
finalizer_steps = [mapping(step) for step in finalizer.get("steps", [])]
finalizer_credentials = [i for i, step in enumerate(finalizer_steps)
                         if str(step.get("uses", "")).startswith("aws-actions/configure-aws-credentials@")]
finalizer_verification = [i for i, step in enumerate(finalizer_steps)
                          if "gateway-image-artifact.sh verify-handoff" in str(step.get("run", ""))]
if (len(finalizer_credentials) != 1 or len(finalizer_verification) != 1 or
        finalizer_verification[0] >= finalizer_credentials[0]):
    errors.append("artifact-custodian finalizer must reverify sealed custody before credentials")
if any(token in finalizer_text for token in ("docker build", "amazon-ecr-login", ":latest")):
    errors.append("artifact-custodian finalizer has forbidden build/ECR/latest authority")

def validate_custodian_checkout_order(job_name, steps, inline_name, uniqueness_name):
    checkout_indexes = [i for i, step in enumerate(steps)
                        if str(step.get("uses", "")).startswith("actions/checkout@")]
    credential_indexes = [i for i, step in enumerate(steps)
                          if str(step.get("uses", "")).startswith("aws-actions/configure-aws-credentials@")]
    inline_indexes = [i for i, step in enumerate(steps) if step.get("name") == inline_name]
    uniqueness_indexes = [i for i, step in enumerate(steps) if step.get("name") == uniqueness_name]
    if not (len(checkout_indexes) == len(credential_indexes) == len(inline_indexes) == len(uniqueness_indexes) == 1):
        errors.append(f"{job_name} must have one inline authority, checkout, global uniqueness, and credential step")
        return
    checkout_index, credential_index = checkout_indexes[0], credential_indexes[0]
    inline_index, uniqueness_index = inline_indexes[0], uniqueness_indexes[0]
    checkout = steps[checkout_index]
    checkout_with = mapping(checkout.get("with"))
    if (checkout.get("uses") != "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1" or
            checkout_with.get("ref") != "${{ github.sha }}" or
            checkout_with.get("persist-credentials") != "false"):
        errors.append(f"{job_name} must checkout exact github.sha without persisted credentials")
    if not (inline_index < checkout_index < uniqueness_index < credential_index):
        errors.append(f"{job_name} authority order must be inline self/parent, checkout, global uniqueness, credentials")
    for index, step in enumerate(steps):
        if "pensyve-mcp-gateway/scripts/" in str(step.get("run", "")) and index < checkout_index:
            errors.append(f"{job_name} invokes a repository script before checkout")
    inline = str(steps[inline_index].get("run", ""))
    if ("actions/runs/${GITHUB_RUN_ID}" not in inline or "actions/runs/${parent_run}" not in inline or
            "parent-jobs.json" not in inline or "--paginate --slurp" in inline or
            "verify-custodian-runs" in inline or "pensyve-mcp-gateway/scripts/" in inline):
        errors.append(f"{job_name} inline step must prove only self/parent/dispatcher authority before checkout")
    uniqueness = str(steps[uniqueness_index].get("run", ""))
    if "--paginate --slurp" not in uniqueness or "verify-custodian-runs" not in uniqueness:
        errors.append(f"{job_name} must run fresh global uniqueness helper after checkout")

validate_custodian_checkout_order(
    "artifact-custodian producer", producer_steps,
    "Bind inline custodian self and parent authority before checkout",
    "Bind global custodian uniqueness after checkout",
)
validate_custodian_checkout_order(
    "artifact-custodian finalizer", finalizer_steps,
    "Bind inline finalizer self and parent authority before checkout",
    "Bind global finalizer uniqueness after checkout",
)

cleanup = mapping(jobs.get("artifact-cleanup"))
if mapping(cleanup.get("permissions")) != {"actions": "write"}:
    errors.append("artifact-cleanup must have actions-write-only authority")
cleanup_if = str(cleanup.get("if", ""))
if "cleanup_required" not in cleanup_if or "true" not in cleanup_if:
    errors.append("artifact-cleanup must be narrowly conditional on cleanup_required=true")
cleanup_text = job_text(cleanup)
for forbidden in (
    "actions/checkout", "gateway-image-artifact.sh", "configure-aws-credentials",
    "docker ", "aws ", "ecr", "ecs",
):
    if forbidden in cleanup_text:
        errors.append(f"artifact-cleanup has forbidden capability: {forbidden}")
delete_count = len(re.findall(r"(?:--method|-X)\s+DELETE", cleanup_text))
if delete_count != 1:
    errors.append("artifact-cleanup must issue exactly one explicit delete")
if "HTTP 404" not in cleanup_text and "--include" not in cleanup_text:
    errors.append("artifact-cleanup must verify REST 404 nonexistence")
if "$repository\" != \"$GITHUB_REPOSITORY" not in cleanup_text or "$run_id\" != \"$GITHUB_RUN_ID" not in cleanup_text:
    errors.append("artifact-cleanup must bind exact current repository/run")
if "over-ceiling|post-upload-invalid" not in cleanup_text:
    errors.append("artifact-cleanup must bind exact invalid post-upload status")
if "gateway-image-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}-${GITHUB_SHA}" not in cleanup_text or "gateway-handoff-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}-${GITHUB_SHA}" not in cleanup_text:
    errors.append("artifact-cleanup must distinguish current-run source and handoff targets")
if "actions/artifacts/${id}" not in cleanup_text or "exit 1" not in cleanup_text:
    errors.append("artifact-cleanup must delete only the bound ID then invalidate")
if cleanup_text.count('actions/artifacts/${id}"') != 3:
    errors.append("artifact-cleanup must target the same exact artifact ID for binding, delete, and 404")
if cleanup_text.count('gh api "repos/${repository}/actions/artifacts/${id}"') != 1:
    errors.append("artifact-cleanup must perform exactly one pre-delete REST binding lookup")
for predicate in ('.id == $id', '.name == $name', '.workflow_run.id == $run', '.expired == false'):
    if predicate not in cleanup_text:
        errors.append(f"artifact-cleanup pre-delete binding missing: {predicate}")
pre_get = cleanup_text.find('gh api "repos/${repository}/actions/artifacts/${id}"')
delete = cleanup_text.find('gh api --method DELETE "repos/${repository}/actions/artifacts/${id}"')
post_get = cleanup_text.find('gh api --include "repos/${repository}/actions/artifacts/${id}"')
if min(pre_get, delete, post_get) < 0 or not (pre_get < delete < post_get):
    errors.append("artifact-cleanup must GET-bind, delete once, then prove 404")
if not cleanup_text.rstrip().endswith("exit 1"):
    errors.append("artifact-cleanup must always invalidate after deletion")

for name, job in jobs.items():
    permissions = mapping(mapping(job).get("permissions"))
    if name not in ("artifact-cleanup", "artifact-promote-dispatch") and permissions.get("actions") == "write":
        errors.append(f"actions write leaked to {name}")
    if name not in ("artifact-build", "artifact-promote-preflight") and "pull-requests" in permissions:
        errors.append(f"pull-request permission leaked to {name}")

build_text = job_text(jobs.get("artifact-build"))
preflight_text = job_text(jobs.get("artifact-promote-preflight"))
if "gateway-image-artifact.sh" not in build_text or "test-gateway-release-image.sh" not in build_text:
    errors.append("artifact-build is missing reviewed artifact/release scripts")
if "gateway-image-artifact.sh seal-tree" not in build_text or "seal-reverify.log" not in build_text:
    errors.append("artifact-build must create and retain an auditable full seal replay")
upload_pin = "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02"
if build_text.count(upload_pin) != 1 or "'compression-level': '0'" not in build_text or "'retention-days': '30'" not in build_text:
    errors.append("artifact-build must upload exactly once at compression 0 / retention 30")
if all(token in build_text for token in ("gateway-image-artifact.sh build", "test-gateway-release-image.sh prove", upload_pin)):
    if not (build_text.index("gateway-image-artifact.sh build") < build_text.index("test-gateway-release-image.sh prove") < build_text.index(upload_pin)):
        errors.append("artifact-build must build once, prove exact bytes, then upload")
build_steps = [mapping(step) for step in build.get("steps", [])]
source_gate_indexes = [i for i, step in enumerate(build_steps) if step.get("id") == "source-gates"]
upload_indexes = [i for i, step in enumerate(build_steps) if str(step.get("uses", "")).startswith("actions/upload-artifact@")]
seal_indexes = [i for i, step in enumerate(build_steps) if step.get("name") == "Seal pre-upload source evidence"]
if len(source_gate_indexes) != 1 or len(upload_indexes) != 1 or len(seal_indexes) != 1 or not (source_gate_indexes[0] < seal_indexes[0] < upload_indexes[0]):
    errors.append("all exact-head source gates must run once before seal and paid upload")
elif not all(token in str(build_steps[source_gate_indexes[0]].get("run", "")) for token in (
    "test-gateway-artifact-flow.sh all", "cargo test --locked -p pensyve-mcp-gateway -p pensyve-mcp-tools",
    "test-model-bundle.sh", "bash -n", "actionlint_version=1.7.12",
    "325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6",
    "ac0323433c2853ec3fb978c611430c5b3dc5d43c58d1a1ec031b00ab572beb60",
    "source-gates.started", "git rev-parse HEAD", "git status --porcelain", "source-gates.json",
    "release-evidence.json", 'index("--offline-scan")', 'index("--skip-db-update")',
    'index("--skip-check-update")', "release_no_network",
    "cargo test --locked -p pensyve-core --test test_no_network_invariants",
    "HF_HOME=/tmp/pensyve-model-scratch/cache", "FASTEMBED_CACHE_DIR=/tmp/pensyve-model-scratch/cache",
    "HF_HUB_OFFLINE=1", "PENSYVE_NETWORK_POLICY=disabled", "no_network_invariants",
    "PyYAML==6.0.2", "--only-binary=:all:", "--require-hashes",
    "1f71ea527786de97d1a0cc0eacd1defc0985dcf6b3f17bb77dcfc8c34bec4dc5",
    "80bab7bfc629882493af4aa31a4cfa43a4c57c83813253626916b8c7ada83476",
)):
    errors.append("artifact-build exact-head source gate matrix is incomplete or unbound")
elif not (
    str(build_steps[source_gate_indexes[0]].get("run", "")).find("test-model-bundle.sh /tmp/pensyve-model-scratch/cache all") <
    str(build_steps[source_gate_indexes[0]].get("run", "")).find("cargo test --locked -p pensyve-core --test test_no_network_invariants")
):
    errors.append("dedicated no-network invariant must run against the extracted exact-image model cache")
if "source-gates.json" not in build_text or "source_gate" not in build_text:
    errors.append("artifact-build must bind source-gate pass evidence into the sealed tuple")
if "gateway-image-artifact.sh" not in preflight_text or "verified-image.json" not in preflight_text:
    errors.append("preflight is missing source artifact verification/fixed handoff")
if "actions/runs/${SOURCE_RUN_ID}" not in preflight_text or "actions/artifacts/${ARTIFACT_ID}" not in preflight_text:
    errors.append("preflight must fetch exact reviewed run and artifact IDs")
handoff_uploads = [
    mapping(step) for step in preflight.get("steps", [])
    if str(mapping(step).get("uses", "")).startswith("actions/upload-artifact@")
]
if len(handoff_uploads) != 1 or handoff_uploads[0].get("id") != "handoff-upload":
    errors.append("preflight must upload exactly one distinct current-run handoff")
elif not all(token in str(mapping(handoff_uploads[0].get("with")).get("name", "")) for token in (
    "gateway-handoff-", "${{ github.run_id }}", "${{ github.run_attempt }}", "${{ github.sha }}",
)):
    errors.append("preflight handoff name must bind current repo run attempt and reviewed SHA")
if "verified_json" in str(preflight.get("outputs", {})) or "complete_tuple" in str(preflight.get("outputs", {})):
    errors.append("preflight must not hand off trusted JSON through job-local outputs")
for name in ("Reconcile current-run handoff REST and storage ceilings",):
    named_step(preflight, name)
if "Re-fetch exact current-run handoff and immutable source before credentials" not in producer_text:
    errors.append("promotion must re-fetch exact current-run handoff and immutable source")
if "validate_minimal_handoff_tree" not in producer_text:
    errors.append("promotion must enforce the exact minimal handoff tree and file types")
if build_text.count('snapshot_inclusion_mode:"source-excluded"') != 1 or not all(token in build_text for token in (
    "retained_source_artifact_id:null", "retained_source_artifact_bytes:0",
    "/tmp/pensyve-reviewed-artifact/storage-input.json", "source-final-storage-precheck.json",
    "find /tmp/pensyve-reviewed-artifact -type f",
)):
    errors.append("source artifact storage must use one immutable source-excluded snapshot and add source only at REST reconciliation")
if preflight_text.count('snapshot_inclusion_mode:"source-included"') != 1 or not all(token in preflight_text for token in (
    "HANDOFF_BILLING_SNAPSHOT_AT", "HANDOFF_CURRENT_BILLABLE_BYTES",
    "HANDOFF_INCLUDED_SOURCE_ARTIFACT_ID", "HANDOFF_INCLUDED_SOURCE_ARTIFACT_BYTES",
    "source_snapshot=$(jq -r '.storage.snapshot_at'", '"$HANDOFF_INCLUDED_SOURCE_ARTIFACT_ID" == "$source_id"',
    '"$HANDOFF_INCLUDED_SOURCE_ARTIFACT_BYTES" == "$source_bytes"',
)):
    errors.append("promotion handoff storage must use one refreshed source-included snapshot with exact retained source bytes")
for token in ("workspace", "cargo", "model_scratch", "docker", "tmp", "docker info --format", "disk-precheck"):
    if token not in build_text:
        errors.append(f"artifact-build disk capacity gate is missing actual filesystem surface: {token}")

push_test = mapping(jobs.get("push-main-test"))
push_deploy = mapping(jobs.get("push-main-deploy"))
push_text = job_text(push_deploy)
if "github.event_name == 'push'" not in str(push_test.get("if", "")):
    errors.append("push-main test path is not event-isolated")
if "github.event_name == 'push'" not in str(push_deploy.get("if", "")):
    errors.append("push-main deploy path is not event-isolated")
if "concurrency" in push_deploy or "github.event_name == 'push'" not in concurrency_group:
    errors.append("push-main deploy must share only the workflow-level production gateway lease")
if ":latest" in push_text or "MCP_ALLOWED_HOSTS" in push_text:
    errors.append("push-main must not use latest or repair environment")

ci_on = mapping(ci.get("on"))
if "workflow_dispatch" in ci_on:
    errors.append("ci.yml must not add a manual artifact trigger")
ci_text = "\n".join(job_text(job) for job in mapping(ci.get("jobs")).values())
if "test-gateway-artifact-flow.sh" not in ci_text:
    errors.append("ci.yml must orchestrate artifact-flow source/static tests")
for token in ("PyYAML==6.0.2", "--only-binary=:all:", "--require-hashes",
              "1f71ea527786de97d1a0cc0eacd1defc0985dcf6b3f17bb77dcfc8c34bec4dc5",
              "80bab7bfc629882493af4aa31a4cfa43a4c57c83813253626916b8c7ada83476"):
    if token not in ci_text:
        errors.append(f"ci.yml clean workflow validator dependency pin is missing: {token}")
if "docker buildx build" in ci_text or "docker push" in ci_text:
    errors.append("ci.yml must remain source-only and not build/push the release artifact")

docker_text = dockerfile_path.read_text()
if not re.search(r"^STOPSIGNAL\s+SIGINT\s*$", docker_text, re.MULTILINE):
    errors.append("Dockerfile must declare STOPSIGNAL SIGINT")
if "libp11-kit0" not in docker_text or "0.25.3-4ubuntu2.2" not in docker_text:
    errors.append("Dockerfile must enforce fixed libp11-kit0 floor")

artifact_text = artifact_script_path.read_text()
release_text = release_script_path.read_text()
promotion_text = promote_script_path.read_text()
if artifact_text.count("docker buildx build") != 1 or artifact_text.count("docker save") != 1:
    errors.append("artifact script must own exactly one build and one export command")
if 'blobs/sha256/${image_id#sha256:}' not in artifact_text:
    errors.append("artifact script must extract the config from the OCI-layout archive blob")
if (re.search(r'(?m)^\s*(?:"?\$\{AWS_BIN\}"?|aws)(?:\s|$)', artifact_text) or
        re.search(r"configure-aws-credentials@", artifact_text, re.IGNORECASE)):
    errors.append("artifact script must not contain an AWS mode")
if "verify_scan_common()" not in artifact_text or "verify_scan_preupload()" not in artifact_text or "verify_scan_postupload()" not in artifact_text or "Trivy scan evidence and deterministic policy verified" not in artifact_text:
    errors.append("artifact script must own scanner/DB/report/policy validation")
verify_scan_text = artifact_text.split("verify_local()", 1)[0]
if "datetime.now" in verify_scan_text or "source_artifact_created_at" not in verify_scan_text:
    errors.append("scan freshness must be immutable at scan time and bound before source artifact creation")
if "snapshot_inclusion_mode" not in artifact_text or "retained_source_artifact_bytes" not in artifact_text:
    errors.append("artifact storage accounting must declare an explicit source inclusion model")
if "disk-precheck" not in artifact_text or "required_bytes_by_device" not in artifact_text:
    errors.append("artifact script must aggregate conservative peak disk demand by actual filesystem")
if "seal_tree()" not in artifact_text or "sha256sum --check" not in artifact_text:
    errors.append("artifact script must own full tree sealing and auditable replay")
if "docker stop \"${container}\"" not in release_text or "docker kill --signal" in release_text or "docker stop --" in release_text:
    errors.append("release script must use only unmodified default docker stop")
if 'blobs/sha256/' not in release_text or 'RepoTags' not in release_text:
    errors.append("release script must verify the OCI-layout config and exact archive tag")
if 'dst=/root/.cache/trivy,readonly' in release_text or 'Trivy DB changed during no-network scan' not in release_text:
    errors.append("release scan must permit local cache writes while proving the DB remains immutable")
if release_text.count('--user "$(id -u):$(id -g)"') < 2 or release_text.count('TRIVY_CACHE_DIR=/trivy-cache') < 2:
    errors.append("Trivy prepare/scan must create runner-owned traversable cache evidence")
if release_text.count('--cgroup-parent "${cgroup_parent}"') != 2:
    errors.append("standalone and five-run lifecycle must use persistent evidence parent cgroups")
if 'memory.events.post-stop.txt' not in release_text or 'post-stop OOM-event delta is nonzero' not in release_text:
    errors.append("release lifecycle must capture and assert genuine post-default-stop OOM deltas")
stop_index = release_text.find('docker stop "${container}"')
post_events_index = release_text.find('cat "${events_cgroup_dir}/memory.events" > "${run_dir}/memory.events.post-stop.txt"')
if stop_index < 0 or post_events_index < 0 or post_events_index <= stop_index:
    errors.append("post-default-stop OOM evidence must be captured after default docker stop")
gte_command = re.search(
    r'embedding::tests::disabled_gte_constructs_from_complete_real_seeded_cache[^\n]*\n[ \t]*-- --ignored --exact --nocapture --test-threads=1',
    release_text,
)
bge_command = re.search(
    r'--test test_no_network_invariants reranker_does_not_make_network_calls[^\n]*\n[ \t]*-- --exact --nocapture --test-threads=1',
    release_text,
)
if not gte_command:
    errors.append("release GTE proof must retain its explicit ignored-test selection")
if not bge_command or "--ignored" in bge_command.group(0):
    errors.append("release BGE proof must select the non-ignored exact test")
for token in (
    "real-gte-inference.log", "real-bge-inference.log", "verify_exact_test_result",
    "exact test selection is invalid", "1 passed; 0 failed; 0 ignored", "skipping",
):
    if token not in release_text:
        errors.append(f"release exact result gate is missing: {token}")
for label, log_name in (("GTE", "real-gte-inference.log"), ("BGE", "real-bge-inference.log")):
    if f'verify_exact_test_result "{label}" "${{evidence_dir}}/{log_name}"' not in release_text:
        errors.append(f"release {label} proof must use the generalized exact result gate")
if "verify_scan_common()" in release_text:
    errors.append("release script must delegate scanner policy ownership to the artifact script")
if "gh api" in promotion_text or "GITHUB_" in promotion_text or "pull_request" in promotion_text:
    errors.append("promotion script must not parse GitHub metadata")
if promotion_text.count('"${DOCKER_BIN}" push') != 1:
    errors.append("promotion script must contain exactly one push site")
if promotion_text.count("verify_task8_baseline preupdate") != 1:
    errors.append("promotion must reverify exact Task 8 immediately before its sole candidate update")
else:
    preupdate_index = promotion_text.find("verify_task8_baseline preupdate")
    arm_index = promotion_text.find("updated=1")
    candidate_update_index = promotion_text.find('aws_call ecs update-service', arm_index)
    if min(preupdate_index, arm_index, candidate_update_index) < 0 or not (preupdate_index < arm_index < candidate_update_index):
        errors.append("promotion Task 8 recheck must occur after registration and immediately before candidate update authority")
if "verify_candidate_functional_runtime" not in promotion_text:
    errors.append("promotion must prove exact candidate GTE/BGE functionality inside rollback")
for token in ("describe-target-health", "get-log-events", "PENSYVE_API_KEYS", "/v1/remember", "/v1/recall", "/v1/entities/"):
    if token not in promotion_text:
        errors.append(f"candidate functional proof is missing existing scoped mechanism: {token}")
for token in ('.status == "ok"', "seq 1 12", '"${SLEEP_BIN}" 30', "cleanup_probe_once", "fetch_stream_events"):
    if token not in promotion_text:
        errors.append(f"candidate functional proof is missing realistic health/log/cleanup boundary: {token}")

if errors:
    print("\n".join(errors), file=sys.stderr)
    raise SystemExit(1)
PY
}

copy_mutation() {
    local name="$1"
    local destination="${TEST_ROOT}/${name}.yml"
    cp -- "${WORKFLOW}" "${destination}"
    printf '%s\n' "${destination}"
}

run_structural() {
    validate_workflow "${WORKFLOW}"
    require_scripts

    local mutation
    local artifact_aws_mutation="${TEST_ROOT}/artifact-aws-mode.sh"
    cp -- "${ARTIFACT_SCRIPT}" "${artifact_aws_mutation}"
    printf '\naws sts get-caller-identity\n' >> "${artifact_aws_mutation}"
    expect_failure "must not contain an AWS mode" validate_workflow "${WORKFLOW}" \
      "${RELEASE_SCRIPT}" "${artifact_aws_mutation}"

    local action_mutation action_ref mutable_ref expected_count expected_pin_error
    while IFS=$'\t' read -r action_mutation action_ref mutable_ref expected_count expected_pin_error; do
        mutation="$(copy_mutation "workflow-${action_mutation}-mutable")"
        python3 - "${mutation}" "${action_ref}" "${mutable_ref}" "${expected_count}" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1]); source, replacement, count = sys.argv[2:]
s = p.read_text()
if s.count(source) != int(count):
    raise SystemExit("action pin hard target lookup failed")
p.write_text(s.replace(source, replacement, 1))
PY
        expect_failure "${expected_pin_error}" validate_workflow "${mutation}"
    done <<'EOF'
upload	actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02	actions/upload-artifact@v4	2	verified action commit
credentials	aws-actions/configure-aws-credentials@e6de054238d6b7531b4efff3b6587d9aade6a06c	aws-actions/configure-aws-credentials@v6	3	pinned twice to verified commit
ecr-login	aws-actions/amazon-ecr-login@03f1aad4c6c7ffd436567f42f9384779290529bd	aws-actions/amazon-ecr-login@v2	2	pinned twice to verified commit
EOF

    mutation="$(copy_mutation workflow-promote-timeout)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1]); s = p.read_text(); target = "    timeout-minutes: 45\n"
if s.count(target) != 1:
    raise SystemExit("promotion timeout hard target lookup failed")
p.write_text(s.replace(target, "", 1))
PY
    expect_failure "fixed 45-minute timeout" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-oidc)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); p.write_text(s.replace("permissions: {}", "permissions:\n  id-token: write", 1))
PY
    expect_failure "workflow-level permissions" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-latest)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
s = p.read_text()
target = 'image="$REGISTRY/pensyve-gateway:$GITHUB_SHA"'
if s.count(target) != 1:
    raise SystemExit("hard target lookup failed for push-main image tag")
p.write_text(s.replace(target, 'image="$REGISTRY/pensyve-gateway:latest"', 1))
PY
    expect_failure "latest" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-cleanup-unconditional)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
s = p.read_text()
targets = (
    "needs.artifact-build.outputs.cleanup_required == 'true'",
    "needs.artifact-promote-preflight.outputs.cleanup_required == 'true'",
)
if any(s.count(target) != 1 for target in targets):
    raise SystemExit("hard target lookup failed for cleanup predicate")
for target in targets:
    s = s.replace(target, "always()", 1)
p.write_text(s)
PY
    expect_failure "cleanup_required" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-cleanup-delete)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); p.write_text(s.replace("--method DELETE", "--method DELETE\n          gh api --method DELETE", 1))
PY
    expect_failure "exactly one explicit delete" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-pr-write)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target="pull-requests: read"
if s.count(target) != 2: raise SystemExit("hard target lookup failed for PR permissions")
p.write_text(s.replace(target, "pull-requests: write", 1))
PY
    expect_failure "artifact-build must have exact" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-pr-list)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='pulls/${PR_NUMBER}'
if s.count(target) != 2: raise SystemExit("hard target lookup failed for exact PR API")
p.write_text(s.replace(target, 'pulls?state=open', 1))
PY
    expect_failure "must not list/search PRs" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-build-pr-predicate)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='.base.repo.full_name == $repo'
if s.count(target) != 2: raise SystemExit("hard target lookup failed for build/preflight base repository predicates")
p.write_text(s.replace(target, 'true', 1))
PY
    expect_failure "artifact-build missing full PR predicate" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-task5-draft)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='.draft == $reviewed_draft'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for Task 5 draft predicate")
p.write_text(s.replace(target, '.draft == true', 1))
PY
    expect_failure "Task 5 PR predicate" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-task5-state)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='.state == $reviewed_state'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for Task 5 state predicate")
p.write_text(s.replace(target, '.state == "open"', 1))
PY
    expect_failure "Task 5 PR predicate" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-aws-before-preflight)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='      - name: Bind promotion run and exact Task 5 pull request before checkout\n'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for preflight first step")
insert='      - name: forbidden early authority\n        run: aws sts get-caller-identity\n'
p.write_text(s.replace(target, insert + target, 1))
PY
    expect_failure "AWS before verified preflight" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-promote-needs)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='    needs: [artifact-promote-preflight]\n'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for promotion needs")
p.write_text(s.replace(target, '    needs: []\n', 1))
PY
    expect_failure "dispatcher must depend only on successful preflight" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-credential-order)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='gateway-image-artifact.sh verify-handoff'
if s.count(target) != 2: raise SystemExit("hard target lookup failed for precredential handoff validation")
p.write_text(s.replace(target, 'true # removed verify-handoff', 1))
PY
    expect_failure "must precede custodian producer credentials" validate_workflow "${mutation}"

    local order_job order_inline
    while IFS=$'\t' read -r order_job order_inline; do
        mutation="$(copy_mutation "workflow-${order_job}-repo-before-checkout")"
        python3 - "${mutation}" "${order_job}" "${order_inline}" <<'PY'
from pathlib import Path
import sys
import yaml
p = Path(sys.argv[1])
job_name, inline_name = sys.argv[2:]
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"][job_name]["steps"]
matches = [step for step in steps if step.get("name") == inline_name]
if len(matches) != 1:
    raise SystemExit("inline custody step hard target lookup failed")
matches[0]["run"] += "\npensyve-mcp-gateway/scripts/gateway-image-artifact.sh verify-local --tuple forbidden.json\n"
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
        expect_failure "invokes a repository script before checkout" validate_workflow "${mutation}"
    done <<'EOF'
artifact-custodian-producer	Bind inline custodian self and parent authority before checkout
artifact-custodian-finalize	Bind inline finalizer self and parent authority before checkout
EOF

    mutation="$(copy_mutation workflow-producer-global-uniqueness-before-checkout)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p = Path(sys.argv[1])
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["artifact-custodian-producer"]["steps"]
step = next(item for item in steps if item.get("name") == "Bind global custodian uniqueness after checkout")
steps.remove(step)
steps.insert(0, step)
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "authority order must be inline self/parent, checkout, global uniqueness, credentials" \
      validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-finalizer-inline-after-checkout)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p = Path(sys.argv[1])
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["artifact-custodian-finalize"]["steps"]
step = next(item for item in steps if item.get("name") == "Bind inline finalizer self and parent authority before checkout")
steps.remove(step)
steps.insert(2, step)
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "authority order must be inline self/parent, checkout, global uniqueness, credentials" \
      validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-producer-global-uniqueness-after-credentials)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p = Path(sys.argv[1])
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["artifact-custodian-producer"]["steps"]
step = next(item for item in steps if item.get("name") == "Bind global custodian uniqueness after checkout")
steps.remove(step)
credential = next(index for index, item in enumerate(steps)
                  if str(item.get("uses", "")).startswith("aws-actions/configure-aws-credentials@"))
steps.insert(credential + 1, step)
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "authority order must be inline self/parent, checkout, global uniqueness, credentials" \
      validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-finalizer-checkout-ref)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p = Path(sys.argv[1])
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["artifact-custodian-finalize"]["steps"]
checkout = next(item for item in steps if str(item.get("uses", "")).startswith("actions/checkout@"))
checkout["with"]["ref"] = "main"
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "must checkout exact github.sha without persisted credentials" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-wide-repository-script-before-checkout)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p = Path(sys.argv[1])
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["push-main-test"]["steps"]
script = next(item for item in steps if "pensyve-mcp-gateway/scripts/" in str(item.get("run", "")))
steps.remove(script)
steps.insert(0, script)
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "invokes a repository script before checkout" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-cleanup-hidden-delete)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='gh api --include "repos/${repository}/actions/artifacts/${id}"'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for cleanup 404")
p.write_text(s.replace(target, 'gh api -X DELETE --include "repos/${repository}/actions/artifacts/${id}"', 1))
PY
    expect_failure "exactly one explicit delete" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-cleanup-wrong-target)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='gh api --method DELETE "repos/${repository}/actions/artifacts/${id}"'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for cleanup delete")
p.write_text(s.replace(target, 'gh api --method DELETE "repos/${repository}/actions/artifacts/${id}-wrong"', 1))
PY
    expect_failure "same exact artifact ID" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-cleanup-success)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='          echo "deleted invalid artifact id=$id name=$name; minimal incurred bytes only" >&2\n          exit 1\n'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for cleanup terminal failure")
p.write_text(s.replace(target, target.replace('exit 1', 'exit 0'), 1))
PY
    expect_failure "always invalidate" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-second-upload)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='      - name: Upload one immutable release artifact\n'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for upload")
insert='      - name: forbidden second upload\n        uses: actions/upload-artifact@v4\n'
p.write_text(s.replace(target, insert + target, 1))
PY
    expect_failure "upload exactly once" validate_workflow "${mutation}"

    local release_mutation="${TEST_ROOT}/release-signal-substitution.sh"
    cp -- "${RELEASE_SCRIPT}" "${release_mutation}"
    python3 - "${release_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='docker stop "${container}"'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for default stop")
p.write_text(s.replace(target, 'docker kill --signal SIGINT "${container}"', 1))
PY
    expect_failure "default docker stop" validate_workflow "${WORKFLOW}" "${release_mutation}"

    local release_events_mutation="${TEST_ROOT}/release-post-stop-events.sh"
    cp -- "${RELEASE_SCRIPT}" "${release_events_mutation}"
    python3 - "${release_events_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='memory.events.post-stop.txt'
if s.count(target) < 1: raise SystemExit("hard target lookup failed for post-stop event evidence")
p.write_text(s.replace(target, 'memory.events.before-stop.txt'))
PY
    expect_failure "post-default-stop OOM" validate_workflow "${WORKFLOW}" "${release_events_mutation}"

    local release_bge_mutation="${TEST_ROOT}/release-bge-zero-selected.sh"
    cp -- "${RELEASE_SCRIPT}" "${release_bge_mutation}"
    python3 - "${release_bge_mutation}" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
s = p.read_text()
target = "--test test_no_network_invariants reranker_does_not_make_network_calls \\\n            -- --exact --nocapture --test-threads=1"
replacement = target.replace("-- --exact", "-- --ignored --exact")
if s.count(target) != 1:
    raise SystemExit("BGE exact-selection hard target lookup failed")
p.write_text(s.replace(target, replacement, 1))
PY
    expect_failure "non-ignored exact test" validate_workflow "${WORKFLOW}" "${release_bge_mutation}"

    local release_events_order_mutation="${TEST_ROOT}/release-post-stop-events-order.sh"
    cp -- "${RELEASE_SCRIPT}" "${release_events_order_mutation}"
    python3 - "${release_events_order_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text()
stop='    docker stop "${container}" > "${run_dir}/default-stop-output.txt"\n'
capture='    cat "${events_cgroup_dir}/memory.events" > "${run_dir}/memory.events.post-stop.txt"\n'
if s.count(stop) != 1 or s.count(capture) != 1: raise SystemExit("hard target lookup failed for post-stop ordering")
p.write_text(s.replace(capture, '', 1).replace(stop, capture + stop, 1))
PY
    expect_failure "after default docker stop" validate_workflow "${WORKFLOW}" "${release_events_order_mutation}"

    local release_fanal_mutation="${TEST_ROOT}/release-fanal-owner.sh"
    cp -- "${RELEASE_SCRIPT}" "${release_fanal_mutation}"
    python3 - "${release_fanal_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='--user "$(id -u):$(id -g)"'
if s.count(target) < 2: raise SystemExit("hard target lookup failed for runner-owned Trivy cache")
p.write_text(s.replace(target, '--user 65534:65534', 1))
PY
    expect_failure "runner-owned traversable" validate_workflow "${WORKFLOW}" "${release_fanal_mutation}"

    local release_cgroup_mutation="${TEST_ROOT}/release-cgroup-parent.sh"
    cp -- "${RELEASE_SCRIPT}" "${release_cgroup_mutation}"
    python3 - "${release_cgroup_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='--cgroup-parent "${cgroup_parent}"'
if s.count(target) != 2: raise SystemExit("hard target lookup failed for persistent lifecycle cgroups")
p.write_text(s.replace(target, '', 1))
PY
    expect_failure "persistent evidence parent cgroups" validate_workflow "${WORKFLOW}" "${release_cgroup_mutation}"

    local artifact_mutation="${TEST_ROOT}/artifact-second-build.sh"
    cp -- "${ARTIFACT_SCRIPT}" "${artifact_mutation}"
    python3 - "${artifact_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='docker buildx build'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for single build")
p.write_text(s.replace(target, 'docker buildx build\n    docker buildx build', 1))
PY
    expect_failure "exactly one build" validate_workflow "${WORKFLOW}" "${RELEASE_SCRIPT}" "${artifact_mutation}"

    local artifact_scanner_mutation="${TEST_ROOT}/artifact-scanner-owner.sh"
    cp -- "${ARTIFACT_SCRIPT}" "${artifact_scanner_mutation}"
    python3 - "${artifact_scanner_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='verify_scan_common()'
if s.count(target) != 1: raise SystemExit("hard target lookup failed for scanner policy owner")
p.write_text(s.replace(target, 'verify_scan_removed()', 1))
PY
    expect_failure "must own scanner" validate_workflow "${WORKFLOW}" "${RELEASE_SCRIPT}" "${artifact_scanner_mutation}"

    local artifact_seal_mutation="${TEST_ROOT}/artifact-seal-replay.sh"
    cp -- "${ARTIFACT_SCRIPT}" "${artifact_seal_mutation}"
    python3 - "${artifact_seal_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='sha256sum --check'
if s.count(target) < 1: raise SystemExit("hard target lookup failed for seal replay")
p.write_text(s.replace(target, 'true', 1))
PY
    expect_failure "auditable replay" validate_workflow "${WORKFLOW}" "${RELEASE_SCRIPT}" "${artifact_seal_mutation}"

    mutation="$(copy_mutation workflow-source-gate-no-network)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='release-evidence.json'
if s.count(target) != 2: raise SystemExit("source no-network hard target lookup failed")
p.write_text(s.replace(target, 'release-proof-removed.json', 1))
PY
    expect_failure "exact-head source gate matrix" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-source-gate-dedicated-no-network)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='cargo test --locked -p pensyve-core --test test_no_network_invariants'
if s.count(target) != 1: raise SystemExit("dedicated no-network hard target lookup failed")
p.write_text(s.replace(target, 'cargo test --locked -p pensyve-core network_policy', 1))
PY
    expect_failure "exact-head source gate matrix" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-source-gate-late)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p=Path(sys.argv[1]); workflow=yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps=workflow["jobs"]["artifact-build"]["steps"]
gates=[step for step in steps if step.get("id") == "source-gates"]
uploads=[step for step in steps if str(step.get("uses", "")).startswith("actions/upload-artifact@")]
if len(gates) != 1 or len(uploads) != 1: raise SystemExit("source gate ordering hard target lookup failed")
gate=gates[0]; steps.remove(gate); steps.insert(steps.index(uploads[0]) + 1, gate)
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "before seal and paid upload" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-source-gate-double)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import copy
import sys
import yaml
p=Path(sys.argv[1]); workflow=yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps=workflow["jobs"]["artifact-build"]["steps"]
gates=[step for step in steps if step.get("id") == "source-gates"]
if len(gates) != 1: raise SystemExit("source gate second-run hard target lookup failed")
duplicate=copy.deepcopy(gates[0]); duplicate["name"]="Forbidden second exact-head source gate run"
steps.insert(steps.index(gates[0]) + 1, duplicate)
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "run once before seal" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-source-resolver-early-false)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='          cleanup=true\n          status=post-upload-invalid\n'
if s.count(target) != 2: raise SystemExit("terminal resolver default hard target lookup failed")
p.write_text(s.replace(target, target.replace('cleanup=true', 'cleanup=false'), 1))
PY
    expect_failure "default uploaded custody true" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-source-resolver-lost-response)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p=Path(sys.argv[1]); workflow=yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps=workflow["jobs"]["artifact-build"]["steps"]
matches=[step for step in steps if step.get("id") == "build-custody-resolver"]
if len(matches) != 1 or "source-upload-name-lookup" not in matches[0].get("run", ""):
    raise SystemExit("lost-response resolver hard target lookup failed")
matches[0]["run"] = matches[0]["run"].replace("source-upload-name-lookup", "source-upload-lookup-removed")
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "default uploaded custody true" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-resolver-output-or)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p=Path(sys.argv[1]); workflow=yaml.load(p.read_text(), Loader=yaml.BaseLoader)
outputs=workflow["jobs"]["artifact-build"]["outputs"]
outputs["cleanup_required"] += " || 'false'"
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "outputs must come only" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-handoff-resolver-not-terminal)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p=Path(sys.argv[1]); workflow=yaml.load(p.read_text(), Loader=yaml.BaseLoader)
workflow["jobs"]["artifact-promote-preflight"]["steps"].append({"name":"Forbidden post-resolver boundary", "run":"false"})
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "resolver must be the final step" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-storage-source-double-count)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='snapshot_inclusion_mode:"source-excluded"'
if s.count(target) != 1: raise SystemExit("source storage inclusion hard target lookup failed")
p.write_text(s.replace(target, 'snapshot_inclusion_mode:"source-included"', 1))
PY
    expect_failure "source-excluded snapshot" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-storage-handoff-omits-source)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='snapshot_inclusion_mode:"source-included"'
if s.count(target) != 1: raise SystemExit("handoff storage inclusion hard target lookup failed")
p.write_text(s.replace(target, 'snapshot_inclusion_mode:"source-excluded"', 1))
PY
    expect_failure "refreshed source-included snapshot" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-disk-docker-root)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target="docker info --format '{{.DockerRootDir}}'"
if s.count(target) != 1: raise SystemExit("Docker root disk hard target lookup failed")
p.write_text(s.replace(target, "printf '/tmp'"))
PY
    expect_failure "actual filesystem surface: docker info" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-minimal-tree-removed)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='validate_minimal_handoff_tree()'
if s.count(target) != 1: raise SystemExit("minimal tree hard target lookup failed")
p.write_text(s.replace('validate_minimal_handoff_tree', 'validate_handoff_tree_removed'))
PY
    expect_failure "exact minimal handoff tree" validate_workflow "${mutation}"

    mutation="$(copy_mutation workflow-production-concurrency)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p=Path(sys.argv[1]); workflow=yaml.load(p.read_text(), Loader=yaml.BaseLoader)
workflow["concurrency"]["group"] = "pensyve-gateway-unserialized"
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "one non-canceling production lease" validate_workflow "${mutation}"

    local promote_preupdate_mutation="${TEST_ROOT}/promote-preupdate-late.sh"
    cp -- "${PROMOTE_SCRIPT}" "${promote_preupdate_mutation}"
    python3 - "${promote_preupdate_mutation}" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); s=p.read_text(); target='verify_task8_baseline preupdate || die "pre-update Task 8 drift: ${VERIFY_ERROR}"\n'
anchor='updated=1\n'
if s.count(target) != 1 or s.count(anchor) != 1: raise SystemExit("pre-update Task 8 ordering hard target lookup failed")
p.write_text(s.replace(target, '', 1).replace(anchor, anchor + target, 1))
PY
    expect_failure "immediately before candidate update authority" validate_workflow "${WORKFLOW}" "${RELEASE_SCRIPT}" "${ARTIFACT_SCRIPT}" "${promote_preupdate_mutation}"

    echo "workflow structural contract passed"
}

make_local_fixture() {
    local root="$1"
    mkdir -p "${root}/evidence"
    printf 'archive fixture\n' > "${root}/gateway-image.tar"
    printf '{"architecture":"arm64","os":"linux","config":{"Labels":{"org.opencontainers.image.revision":"%s"},"StopSignal":"SIGINT","User":"1001:1001"}}\n' \
        "${SOURCE_SHA}" > "${root}/config.json"
    printf '{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"digest":"sha256:%s"}}\n' \
        "$(sha256sum "${root}/config.json" | cut -d' ' -f1)" > "${root}/raw-manifest.json"
    printf 'policy-v1\n' > "${root}/policy"
    printf 'db\n' > "${root}/trivy.db"
    printf 'five fresh cgroups passed\n' > "${root}/sizing-summary.txt"
    printf '{"schema_version":1,"source_sha":"%s","status":"pass"}\n' "${SOURCE_SHA}" > "${root}/source-gates.json"
    local archive_sha config_sha manifest_sha manifest_digest db_sha policy_sha gate_sha source_gate_sha now expires
    archive_sha="$(sha256sum "${root}/gateway-image.tar" | cut -d' ' -f1)"
    config_sha="$(sha256sum "${root}/config.json" | cut -d' ' -f1)"
    manifest_sha="$(sha256sum "${root}/raw-manifest.json" | cut -d' ' -f1)"
    manifest_digest="sha256:${manifest_sha}"
    db_sha="$(sha256sum "${root}/trivy.db" | cut -d' ' -f1)"
    policy_sha="$(sha256sum "${root}/policy" | cut -d' ' -f1)"
    gate_sha="$(sha256sum "${root}/sizing-summary.txt" | cut -d' ' -f1)"
    source_gate_sha="$(sha256sum "${root}/source-gates.json" | cut -d' ' -f1)"
    now="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    expires="$(date -u -d '+30 days' +'%Y-%m-%dT%H:%M:%SZ')"

    jq -n \
        --arg sha "${SOURCE_SHA}" \
        --arg root "${root}" \
        --arg archive_sha "${archive_sha}" \
        --arg config_sha "${config_sha}" \
        --arg manifest_sha "${manifest_sha}" \
        --arg manifest_digest "${manifest_digest}" \
        --arg scanner_digest "${SCANNER_DIGEST}" \
        --arg scanner_version "${SCANNER_VERSION}" \
        --arg db_sha "${db_sha}" \
        --arg policy_sha "${policy_sha}" \
        --arg gate_sha "${gate_sha}" \
        --arg source_gate_sha "${source_gate_sha}" \
        --arg now "${now}" \
        --arg expires "${expires}" \
        '{
          schema_version: 1,
          cleanup_required: false,
          source: {
            repository: "major7apps/pensyve", workflow: "Build & Deploy Gateway",
            workflow_path: ".github/workflows/deploy-gateway.yml",
            ref: "refs/heads/fix/strict-local-model-runtime-2026-08-28",
            mode: "artifact-build", run_id: 1234, run_attempt: 1,
            event: "workflow_dispatch", head_sha: $sha
          },
          pull_request: {
            number: 42, repository: "major7apps/pensyve", state: "open", draft: false,
            base_ref: "main", head_repository: "major7apps/pensyve",
            head_ref: "fix/strict-local-model-runtime-2026-08-28", head_sha: $sha
          },
          artifact: {
            id: 777, name: ("gateway-image-1234-1-" + $sha),
            server_digest: ("sha256:" + $archive_sha), size_in_bytes: 4096,
            created_at: $now, expires_at: $expires, retention_days: 30,
            repository: "major7apps/pensyve", run_id: 1234, run_attempt: 1,
            conclusion: "success", status: "completed"
          },
          storage: {
            snapshot_at: $now, approved_gb_hours_ceiling: 1000,
            approved_dollar_ceiling: 10, price_per_gb_month: 0.25,
            current_billable_bytes: 0, archive_bytes: 16, evidence_bytes: 1024,
            container_overhead_bytes: 512, handoff_overhead_bytes: 512,
            projected_content_bytes: 2064, projected_gb_hours: 1, projected_dollars: 0.01,
            computed_projected_gb_hours: 0.00148608,
            computed_projected_dollars: 0.000000516,
            actual_artifact_bytes: 4096, actual_total_billable_bytes: 4096,
            actual_gb_hours: 0.00294912, actual_dollars: 0.000001024,
            status: "accepted",
            runner_available_bytes: 100000000000,
            organization_actions_artifact_bytes: 0, organization_packages_bytes: 0,
            snapshot_inclusion_mode:"source-excluded",retained_source_artifact_id:null,
            retained_source_artifact_bytes:0,
            billing_unit: "GB-month", payment_status: "active", spending_status: "within-limit",
            rest_size_in_bytes: 4096, rest_created_at: $now, rest_expires_at: $expires
          },
          image: {
            archive_path: ($root + "/gateway-image.tar"), archive_sha256: $archive_sha,
            config_path: ($root + "/config.json"), config_id: ("sha256:" + $config_sha),
            platform: "linux/arm64", source_label: $sha,
            raw_manifest_path: ($root + "/raw-manifest.json"),
            raw_manifest_sha256: $manifest_sha,
            raw_manifest_media_type: "application/vnd.docker.distribution.manifest.v2+json",
            pushed_digest: $manifest_digest,
            compressed_layer_bytes: 1024, uncompressed_image_bytes: 2048
          },
          scanner: {
            image_digest: $scanner_digest, version: $scanner_version,
            argv: ["trivy","image","--input",($root + "/gateway-image.tar"),"--offline-scan","--skip-db-update","--skip-check-update","--scanners","vuln,secret,misconfig","--severity","UNKNOWN,LOW,MEDIUM,HIGH,CRITICAL","--exit-code","0","--format","json","--output",($root + "/scan-report.json")],
            db_updated_at: $now, db_downloaded_at: $now, db_sha256: $db_sha,
            db_path: ($root + "/trivy.db"), db_oci_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
          },
          scan: {
            report_path: ($root + "/scan-report.json"), report_sha256: "pending",
            archive_sha256: $archive_sha, config_id: ("sha256:" + $config_sha),
            scanned_at: $now, source_artifact_created_at: $now, policy_path: ($root + "/policy"),
            policy_version: "1", policy_sha256: $policy_sha, policy_result: "pass"
          },
          gates: {bundle:"pass",gte:"pass",bge:"pass",default_stop:"pass",
            missing_model:"pass",five_cgroups:"pass",no_egress:true,read_only_root:true,
            embedding_pool_size:1,source_contract:"pass",
            source_gate_evidence_path:($root + "/source-gates.json"),source_gate_evidence_sha256:$source_gate_sha,
            sizing_summary_path:($root + "/sizing-summary.txt"),sizing_summary_sha256:$gate_sha}
        }' > "${root}/tuple.base.json"

    jq -n --arg id "sha256:${config_sha}" '{SchemaVersion:2, ArtifactName:"gateway-image.tar", ArtifactType:"container_image", Metadata:{ImageID:$id,OS:{Family:"ubuntu",Name:"24.04"}}, Results:[{Target:"ubuntu 24.04",Class:"os-pkgs",Type:"ubuntu",Packages:[{Name:"libp11-kit0",Version:"0.25.3",Release:"4ubuntu2.2"}],Vulnerabilities:[],Secrets:[],Misconfigurations:[]}]}' \
        > "${root}/scan-report.json"
    local report_sha
    report_sha="$(sha256sum "${root}/scan-report.json" | cut -d' ' -f1)"
    jq --arg report_sha "${report_sha}" '.scan.report_sha256=$report_sha' \
        "${root}/tuple.base.json" > "${root}/tuple.json"
}

mutate_json() {
    local source="$1" destination="$2" filter="$3"
    jq "${filter}" "${source}" > "${destination}"
}

mutate_authority_json() {
    local source="$1" destination="$2"
    shift 2
    python3 - "${source}" "${destination}" "$@" <<'PY'
import json
import sys
from pathlib import Path

source, destination = map(Path, sys.argv[1:3])
changes = sys.argv[3:]
if len(changes) % 2:
    raise SystemExit("numeric mutation path/value pairs are incomplete")
data = json.loads(source.read_text())
values = {"true": True, "false": False, "infinity": float("inf")}
for path, value_name in zip(changes[::2], changes[1::2]):
    value = data
    parts = path.split(".")
    for part in parts[:-1]:
        value = value[int(part)] if isinstance(value, list) else value[part]
    leaf = parts[-1]
    if isinstance(value, list):
        value[int(leaf)] = values[value_name]
    else:
        value[leaf] = values[value_name]
destination.write_text(json.dumps(data, sort_keys=True, separators=(",", ":")) + "\n")
PY
}

make_reviewed_tuple_and_request() {
    local source_tuple="$1" reviewed_tuple="$2" request="$3"
    local snapshot="${request}.service-snapshot.json" snapshot_sha
    jq '. + {reviewed_pull_request:{
          number:.pull_request.number,repository:.pull_request.repository,state:"open",draft:false,
          base_ref:.pull_request.base_ref,head_repository:.pull_request.head_repository,
          head_ref:.pull_request.head_ref,head_sha:.pull_request.head_sha}}' \
        "${source_tuple}" > "${reviewed_tuple}"
    jq -n '{service_name:"pensyve-prod-gateway",status:"ACTIVE",
      cluster_arn:"arn:aws:ecs:us-east-2:123456789012:cluster/pensyve-prod",
      task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",
      counts:{desired:2,running:2,pending:0},
      network_configuration:{awsvpcConfiguration:{subnets:["subnet-aaa","subnet-bbb"],securityGroups:["sg-aaa"],assignPublicIp:"DISABLED"}},
      load_balancers:[{targetGroupArn:"arn:aws:elasticloadbalancing:us-east-2:123456789012:targetgroup/pensyve-gateway/abc",containerName:"gateway",containerPort:3100}],
      deployment_configuration:{deploymentCircuitBreaker:{enable:true,rollback:true},maximumPercent:200,minimumHealthyPercent:100},
      health_grace_period_seconds:300,
      primary_deployment:{status:"PRIMARY",task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",rollout_state:"COMPLETED",desired:2,running:2,pending:0}}' \
      > "${snapshot}"
    snapshot_sha="$(jq -S -c . "${snapshot}" | sha256sum | cut -d' ' -f1)"
    jq -n --slurpfile tuple "${reviewed_tuple}" --slurpfile snapshot "${snapshot}" --arg snapshot_sha "${snapshot_sha}" '
      {repository:$tuple[0].source.repository,workflow:$tuple[0].source.workflow,
       workflow_path:$tuple[0].source.workflow_path,ref:$tuple[0].source.ref,
       event:$tuple[0].source.event,run_id:$tuple[0].source.run_id,
       run_attempt:$tuple[0].source.run_attempt,head_sha:$tuple[0].source.head_sha,
       pull_request_number:$tuple[0].pull_request.number,artifact_id:$tuple[0].artifact.id,
       artifact_name:$tuple[0].artifact.name,promotion_event:"workflow_dispatch",
       promotion_head_sha:$tuple[0].source.head_sha,
       reviewed_pull_request_number:$tuple[0].reviewed_pull_request.number,
       reviewed_pull_request_repository:$tuple[0].reviewed_pull_request.repository,
       reviewed_pull_request_state:$tuple[0].reviewed_pull_request.state,
       reviewed_pull_request_draft:$tuple[0].reviewed_pull_request.draft,
       reviewed_pull_request_base_ref:$tuple[0].reviewed_pull_request.base_ref,
       reviewed_pull_request_head_repository:$tuple[0].reviewed_pull_request.head_repository,
       reviewed_pull_request_head_ref:$tuple[0].reviewed_pull_request.head_ref,
       reviewed_pull_request_head_sha:$tuple[0].reviewed_pull_request.head_sha,
       deployment:{region:"us-east-2",ecr_registry:"123456789012.dkr.ecr.us-east-2.amazonaws.com",
         ecr_repository:"pensyve-gateway",cluster:"pensyve-prod",service:"pensyve-prod-gateway",
         gateway_container:"gateway",baseline_task_definition_arn:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",
         baseline_image:"123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
         baseline_environment_sha256:"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
         baseline_service_snapshot:$snapshot[0],baseline_service_snapshot_sha256:$snapshot_sha,
         probe_entity:"task9-runtime-5678-2-0123456789abcdef",
         promotion_run_id:5678,promotion_run_attempt:2,
         cpu:"512",memory:"4096",desired_count:2,running_count:2,pending_count:0}}
    ' > "${request}"
}

run_seal() {
    require_scripts
    local root="${TEST_ROOT}/seal-tree"
    mkdir -p "${root}/evidence/trivy-cache/fanal" "${root}/evidence/release"
    printf 'runner-owned fanal\n' > "${root}/evidence/trivy-cache/fanal/fanal.db"
    printf 'release evidence\n' > "${root}/evidence/release/result.json"
    local manifest="${root}/sealed-files.sha256" transcript="${root}/seal-reverify.log"
    "${ARTIFACT_SCRIPT}" seal-tree --root "${root}" --manifest "${manifest}" --transcript "${transcript}"
    du -sb "${root}" >/dev/null
    while IFS= read -r -d '' path; do [[ -r "${path}" ]]; done < <(find "${root}" -type f -print0 | sort -z)
    "${ARTIFACT_SCRIPT}" verify-tree --root "${root}" --input "${root}/sealed-tree.json" \
        --transcript "${TEST_ROOT}/seal-roundtrip-tree.replay.log"
    grep -F ': OK' "${transcript}" >/dev/null || fail "seal replay transcript contains no verified file"
    [[ "$(tr -d '[:space:]' < "${transcript}")" != true ]] || fail "seal replay transcript is a placeholder"

    local archive="${TEST_ROOT}/roundtrip-tree.zip" roundtrip="${TEST_ROOT}/roundtrip-tree"
    (cd "${root}" && zip -q -r "${archive}" .)
    mkdir -p "${roundtrip}"
    unzip -q "${archive}" -d "${roundtrip}"
    "${ARTIFACT_SCRIPT}" verify-tree --root "${roundtrip}" --input "${roundtrip}/sealed-tree.json" \
        --transcript "${TEST_ROOT}/roundtrip-tree.replay.log"

    local mutation_root="${TEST_ROOT}/seal-mode-drift"
    cp -a -- "${root}" "${mutation_root}"
    chmod 0600 "${mutation_root}/evidence/release/result.json"
    expect_failure "entry type/mode drift" "${ARTIFACT_SCRIPT}" verify-tree --root "${mutation_root}" \
        --input "${mutation_root}/sealed-tree.json" --transcript "${TEST_ROOT}/mode-drift.replay.log"

    mutation_root="${TEST_ROOT}/seal-directory-drift"
    cp -a -- "${root}" "${mutation_root}"
    mkdir "${mutation_root}/unexpected-directory"
    expect_failure "directory topology drift" "${ARTIFACT_SCRIPT}" verify-tree --root "${mutation_root}" \
        --input "${mutation_root}/sealed-tree.json" --transcript "${TEST_ROOT}/directory-drift.replay.log"

    mutation_root="${TEST_ROOT}/seal-symlink-retarget"
    cp -a -- "${root}" "${mutation_root}"
    rm -- "${mutation_root}/evidence/release/result.json"
    ln -s ../trivy-cache/fanal/fanal.db "${mutation_root}/evidence/release/result.json"
    expect_failure "symlink retarget rejected" "${ARTIFACT_SCRIPT}" verify-tree --root "${mutation_root}" \
        --input "${mutation_root}/sealed-tree.json" --transcript "${TEST_ROOT}/symlink-retarget.replay.log"

    local special_root="${TEST_ROOT}/seal-special-entry"
    mkdir -p "${special_root}"
    printf 'ordinary\n' > "${special_root}/ordinary"
    mkfifo "${special_root}/unexpected.fifo"
    expect_failure "special entry rejected" "${ARTIFACT_SCRIPT}" seal-tree --root "${special_root}" \
        --manifest "${special_root}/sealed-files.sha256" --transcript "${special_root}/seal-reverify.log"

    local materialize_root="${TEST_ROOT}/materialize-model-links"
    local model_repo="models--Example--model" blob_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    mkdir -p "${materialize_root}/${model_repo}/blobs" \
        "${materialize_root}/${model_repo}/snapshots/1111111111111111111111111111111111111111/onnx"
    printf 'model-bytes\n' > "${materialize_root}/${model_repo}/blobs/${blob_sha}"
    ln -s ../../../blobs/${blob_sha} \
        "${materialize_root}/${model_repo}/snapshots/1111111111111111111111111111111111111111/onnx/model.onnx"
    "${ARTIFACT_SCRIPT}" materialize-model-links --root "${materialize_root}"
    [[ -f "${materialize_root}/${model_repo}/snapshots/1111111111111111111111111111111111111111/onnx/model.onnx" &&
       ! -L "${materialize_root}/${model_repo}/snapshots/1111111111111111111111111111111111111111/onnx/model.onnx" ]] \
        || fail "valid internal model link was not safely materialized"
    ln -s /etc/passwd \
        "${materialize_root}/${model_repo}/snapshots/1111111111111111111111111111111111111111/unsafe"
    expect_failure "absolute symlink escapes artifact root" "${ARTIFACT_SCRIPT}" materialize-model-links \
        --root "${materialize_root}"

    local blocked_root="${TEST_ROOT}/seal-blocked"
    mkdir -p "${blocked_root}/blocked"
    printf 'blocked\n' > "${blocked_root}/blocked/file"
    chmod 000 "${blocked_root}/blocked"
    expect_failure "runner-readable" "${ARTIFACT_SCRIPT}" seal-tree \
        --root "${blocked_root}" --manifest "${blocked_root}/sealed-files.sha256" \
        --transcript "${blocked_root}/seal-reverify.log"
    chmod 700 "${blocked_root}/blocked"
    echo "runner-readable full seal contract passed"
}

run_storage() {
    require_scripts
    local fixture="${TEST_ROOT}/storage"
    make_local_fixture "${fixture}"

    # Execute the source-replay precheck fragment extracted from the actual
    # promotion workflow. Its authenticated artifact REST created_at is the
    # immutable reference for the source-excluded billing snapshot.
    local replay_root="${fixture}/source-replay" replay_runner="${fixture}/source-replay-runner"
    local replay_step="${fixture}/source-replay-step.sh" replay_created replay_expires
    mkdir -p "${replay_root}" "${replay_runner}"
    printf 'archive bytes\n' > "${replay_root}/gateway-image.tar"
    printf 'source evidence\n' > "${replay_root}/source-evidence.json"
    replay_created="$(date -u -d '-3 days' +'%Y-%m-%dT%H:%M:%SZ')"
    replay_expires="$(date -u -d "${replay_created} +30 days" +'%Y-%m-%dT%H:%M:%SZ')"
    jq --arg created "${replay_created}" '
      .storage | .snapshot_at=($created | fromdateiso8601 - 60 | todateiso8601) |
      .archive_bytes=0 | .evidence_bytes=0
    ' "${fixture}/tuple.json" > "${replay_root}/storage-input.json"
    jq -n --arg created "${replay_created}" --arg expires "${replay_expires}" '
      {id:777,name:"gateway-image-1234-1-0123456789abcdef0123456789abcdef01234567",
       digest:("sha256:"+("a"*64)),size_in_bytes:4096,created_at:$created,
       expires_at:$expires,expired:false,workflow_run:{id:1234}}
    ' > "${replay_runner}/artifact.json"
    python3 - "${WORKFLOW}" "${replay_step}" "${replay_root}" <<'PY'
import sys
from pathlib import Path
import yaml

workflow_path, output_path, replay_root = map(Path, sys.argv[1:])
workflow = yaml.load(workflow_path.read_text(), Loader=yaml.BaseLoader)
matches = [step for step in workflow["jobs"]["artifact-promote-preflight"]["steps"]
           if step.get("name") == "Reconstruct and verify complete immutable tuple"]
if len(matches) != 1:
    raise SystemExit("source replay workflow step is absent or ambiguous")
lines = matches[0]["run"].splitlines()
start = next(index for index, line in enumerate(lines) if line.startswith("source_archive="))
end = next(index for index, line in enumerate(lines[start:], start)
           if '--replay-reference "$(jq -r' in line and 'artifact.json' in line)
fragment = "\n".join(lines[start:end + 1]) + "\n"
output_path.write_text(fragment.replace("/tmp/pensyve-reviewed-artifact", str(replay_root)))
PY
    RUNNER_TEMP="${replay_runner}" bash "${replay_step}"

    local replay_mutation="${replay_root}/storage-mutation.json"
    jq --arg created "${replay_created}" \
      '.snapshot_at=($created | fromdateiso8601 - 86401 | todateiso8601)' \
      "${replay_root}/storage-input.json" > "${replay_mutation}"
    mv -- "${replay_mutation}" "${replay_root}/storage-input.json"
    expect_failure "billing snapshot was stale at replay reference" \
      env RUNNER_TEMP="${replay_runner}" bash "${replay_step}"
    jq --arg created "${replay_created}" \
      '.snapshot_at=($created | fromdateiso8601 + 301 | todateiso8601)' \
      "${replay_root}/storage-input.json" > "${replay_mutation}"
    mv -- "${replay_mutation}" "${replay_root}/storage-input.json"
    expect_failure "billing snapshot is after replay reference" \
      env RUNNER_TEMP="${replay_runner}" bash "${replay_step}"
    jq '.created_at="not-a-timestamp"' "${replay_runner}/artifact.json" \
      > "${replay_runner}/artifact-mutation.json"
    mv -- "${replay_runner}/artifact-mutation.json" "${replay_runner}/artifact.json"
    expect_failure "replay reference timestamp is invalid" \
      env RUNNER_TEMP="${replay_runner}" bash "${replay_step}"

    jq '.storage | . + {
          current_billable_bytes:999000000,
          organization_actions_artifact_bytes:999000000,
          organization_packages_bytes:0,
          approved_gb_hours_ceiling:720,
          approved_dollar_ceiling:100,
          projected_gb_hours:720,
          projected_dollars:1}' "${fixture}/tuple.json" > "${fixture}/reconcile-precheck-input.json"
    "${ARTIFACT_SCRIPT}" storage-precheck --input "${fixture}/reconcile-precheck-input.json" \
      --output "${fixture}/reconcile-prechecked.json"
    jq '. + {
          rest_size_in_bytes:2000000,
          created_at:.rest_created_at,
          expires_at:.rest_expires_at}' "${fixture}/reconcile-prechecked.json" > "${fixture}/reconcile.json"
    "${ARTIFACT_SCRIPT}" storage-reconcile --input "${fixture}/reconcile.json" --output "${fixture}/result.json"
    [[ "$(jq -r '.actual_total_billable_bytes' "${fixture}/result.json")" == 1001000000 ]] \
        || fail "actual total did not include current organization usage plus uploaded bytes"
    [[ "$(jq -r '.cleanup_required' "${fixture}/result.json")" == true ]] \
        || fail "larger-than-projected upload crossing total ceiling did not require cleanup"

    local mutation="${fixture}/storage-mutation.json"
    jq '.storage | .retained_source_artifact_id=777 | .retained_source_artifact_bytes=4096' \
        "${fixture}/tuple.json" > "${mutation}"
    expect_failure "source-excluded snapshot" "${ARTIFACT_SCRIPT}" storage-precheck \
        --input "${mutation}" --output "${fixture}/bad-source-double-count.json"
    jq '.storage | .snapshot_inclusion_mode="source-included" |
          .retained_source_artifact_id=777 | .retained_source_artifact_bytes=4096 |
          .source_snapshot_at=(.snapshot_at | fromdateiso8601 - 1 | todateiso8601) | .current_billable_bytes=4096 |
          .organization_actions_artifact_bytes=4096 | .organization_packages_bytes=0 |
          .archive_bytes=0 | .evidence_bytes=1024 | .container_overhead_bytes=0' \
        "${fixture}/tuple.json" > "${fixture}/handoff-storage.json"
    "${ARTIFACT_SCRIPT}" storage-precheck --input "${fixture}/handoff-storage.json" \
        --output "${fixture}/handoff-storage-result.json"
    expect_failure "replay reference is only valid for source-excluded storage" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${fixture}/handoff-storage.json" \
      --output "${fixture}/bad-handoff-replay-reference.json" --replay-reference "${replay_created}"
    jq '.retained_source_artifact_bytes=8192' "${fixture}/handoff-storage.json" > "${mutation}"
    expect_failure "omits retained source bytes" "${ARTIFACT_SCRIPT}" storage-precheck \
        --input "${mutation}" --output "${fixture}/bad-source-omission.json"
    jq '.snapshot_at="2020-01-01T00:00:00Z"' "${fixture}/handoff-storage.json" > "${mutation}"
    expect_failure "billing snapshot is stale" "${ARTIFACT_SCRIPT}" storage-precheck \
        --input "${mutation}" --output "${fixture}/bad-stale-handoff-snapshot.json"
    jq '.source_snapshot_at=.snapshot_at' "${fixture}/handoff-storage.json" > "${mutation}"
    expect_failure "not refreshed after" "${ARTIFACT_SCRIPT}" storage-precheck \
        --input "${mutation}" --output "${fixture}/bad-mixed-snapshot.json"

    # Executable near-ceiling custody: the upload marker is reachable only
    # after the exact sealed-byte precheck. A one-byte-over projection must
    # fail before any paid upload invocation can be logged.
    jq '.approved_gb_hours_ceiling=0.000000001 | .approved_dollar_ceiling=0.000000001' \
      "${fixture}/handoff-storage.json" > "${fixture}/near-ceiling.json"
    : > "${fixture}/upload-invocations.log"
    set +e
    if "${ARTIFACT_SCRIPT}" storage-precheck --input "${fixture}/near-ceiling.json" \
      --output "${fixture}/near-ceiling-result.json"; then
        printf 'upload-invoked\n' >> "${fixture}/upload-invocations.log"
        near_ceiling_status=0
    else
        near_ceiling_status=$?
    fi
    set -e
    [[ "${near_ceiling_status}" -ne 0 && ! -s "${fixture}/upload-invocations.log" ]] \
      || fail "near-ceiling precheck allowed paid upload invocation"

    local device available
    device="$(df --output=source /tmp | awk 'NR==2 {print $1}')"
    available="$(df --output=avail -B1 /tmp | awk 'NR==2 {print $1}')"
    jq -n --arg device "${device}" --argjson available "${available}" '
      {filesystems:["workspace","cargo","model_scratch","docker","tmp"] |
       map({name:.,path:"/tmp",device:$device,available_bytes:$available,required_bytes:1})}' \
      > "${fixture}/disk.json"
    "${ARTIFACT_SCRIPT}" disk-precheck --input "${fixture}/disk.json" --output "${fixture}/disk-result.json"
    [[ "$(jq -r '[.required_bytes_by_device[]][0]' "${fixture}/disk-result.json")" == 5 ]] \
      || fail "disk gate did not aggregate simultaneous demand on one filesystem"
    jq '.filesystems[3].device="distinct-docker" | .filesystems[3].available_bytes=1 |
        .filesystems[3].required_bytes=2' "${fixture}/disk.json" > "${mutation}"
    expect_failure "insufficient on filesystem distinct-docker" "${ARTIFACT_SCRIPT}" disk-precheck \
      --input "${mutation}" --output "${fixture}/bad-distinct-disk.json"
    echo "current-plus-actual storage reconciliation contract passed"
}

run_reviewed_pr() {
    require_scripts
    local fixture="${TEST_ROOT}/reviewed-pr"
    make_local_fixture "${fixture}"
    make_reviewed_tuple_and_request "${fixture}/tuple.json" "${fixture}/reviewed.json" "${fixture}/request.json"
    "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed.json" --request "${fixture}/request.json" --output "${fixture}/verified.json"
    local mutation="${fixture}/mutation.json" field
    for field in number repository state draft base_ref head_repository head_ref head_sha; do
        case "${field}" in
            number) mutate_json "${fixture}/request.json" "${mutation}" '.reviewed_pull_request_number=999' ;;
            draft) mutate_json "${fixture}/request.json" "${mutation}" '.reviewed_pull_request_draft=true' ;;
            *) jq --arg field "reviewed_pull_request_${field}" '.[$field]="mutation"' "${fixture}/request.json" > "${mutation}" ;;
        esac
        expect_failure "Task 5-reviewed pull request ${field}" "${ARTIFACT_SCRIPT}" fetch-verify \
            --tuple "${fixture}/reviewed.json" --request "${mutation}" --output "${fixture}/bad.json"
    done
    mutate_json "${fixture}/reviewed.json" "${mutation}" '.reviewed_pull_request.draft=true'
    expect_failure "Task 5-reviewed pull request draft" "${ARTIFACT_SCRIPT}" fetch-verify \
        --tuple "${mutation}" --request "${fixture}/request.json" --output "${fixture}/bad.json"
    echo "Task 5-reviewed PR state/drift contract passed"
}

run_deployment() {
    require_scripts
    local fixture="${TEST_ROOT}/deployment"
    make_local_fixture "${fixture}"
    make_reviewed_tuple_and_request "${fixture}/tuple.json" "${fixture}/reviewed.json" "${fixture}/request.json"
    "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed.json" --request "${fixture}/request.json" --output "${fixture}/verified.json"
    "${ARTIFACT_SCRIPT}" verify-handoff --input "${fixture}/verified.json"
    local mutation="${fixture}/mutation.json" filter expected
    for filter in \
        '.deployment.region="us-east-1"' \
        '.deployment.ecr_registry="example.invalid"' \
        '.deployment.ecr_repository="other"' \
        '.deployment.cluster="other"' \
        '.deployment.service="other"' \
        '.deployment.gateway_container="other"' \
        '.deployment.baseline_task_definition_arn="arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:157"' \
        '.deployment.baseline_image="example.invalid/pensyve-gateway@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"' \
        '.deployment.baseline_environment_sha256="invalid"' \
        '.deployment.baseline_service_snapshot.network_configuration.awsvpcConfiguration.subnets[0]="subnet-drift"' \
        '.deployment.baseline_service_snapshot.load_balancers[0].targetGroupArn="arn:aws:elasticloadbalancing:us-east-2:123456789012:targetgroup/drift/xyz"' \
        '.deployment.baseline_service_snapshot.deployment_configuration.deploymentCircuitBreaker.rollback=false' \
        '.deployment.baseline_service_snapshot.deployment_configuration.maximumPercent=150' \
        '.deployment.baseline_service_snapshot.health_grace_period_seconds=0' \
        '.deployment.baseline_service_snapshot.primary_deployment.rollout_state="FAILED"' \
        '.deployment.baseline_service_snapshot_sha256="invalid"' \
        '.deployment.probe_entity="task9-runtime-wrong"' \
        '.deployment.promotion_run_id=0' \
        '.deployment.promotion_run_attempt=0' \
        '.deployment.cpu="1024"' \
        '.deployment.memory="8192"' \
        '.deployment.desired_count=3' \
        '.deployment.running_count=1' \
        '.deployment.pending_count=1' \
        '.deployment.extra="forbidden"'; do
        mutate_json "${fixture}/request.json" "${mutation}" "${filter}"
        expected="Task 8"
        [[ "${filter}" != *'.deployment.probe_entity='* ]] || expected="Task 9"
        expect_failure "${expected}" "${ARTIFACT_SCRIPT}" fetch-verify \
            --tuple "${fixture}/reviewed.json" --request "${mutation}" --output "${fixture}/bad.json"
    done
    mutate_json "${fixture}/verified.json" "${mutation}" '.deployment.cluster="other"'
    expect_failure "Task 8" "${ARTIFACT_SCRIPT}" verify-handoff --input "${mutation}"

    local shape_name shape_filter snapshot_sha
    while IFS='|' read -r shape_name shape_filter; do
        jq "${shape_filter}" "${fixture}/verified.json" > "${mutation}"
        snapshot_sha="$(jq -S -c '.deployment.baseline_service_snapshot' "${mutation}" | sha256sum | cut -d' ' -f1)"
        jq --arg sha "${snapshot_sha}" '.deployment.baseline_service_snapshot_sha256=$sha' \
          "${mutation}" > "${fixture}/mutation-rehashed.json"
        mv -- "${fixture}/mutation-rehashed.json" "${mutation}"
        expect_failure "Task 8 canonical" "${ARTIFACT_SCRIPT}" verify-handoff --input "${mutation}"
    done <<'EOF'
subnets|.deployment.baseline_service_snapshot.network_configuration.awsvpcConfiguration.subnets=[]
load-balancer|.deployment.baseline_service_snapshot.load_balancers=[]
circuit-breaker|.deployment.baseline_service_snapshot.deployment_configuration.deploymentCircuitBreaker.rollback=false
deployment-configuration|del(.deployment.baseline_service_snapshot.deployment_configuration.maximumPercent)
health-grace|.deployment.baseline_service_snapshot.health_grace_period_seconds=-1
EOF
    echo "precredential full Task 8 deployment contract passed"
}

run_artifact() {
    require_scripts
    local fixture="${TEST_ROOT}/artifact"
    make_local_fixture "${fixture}"
    "${ARTIFACT_SCRIPT}" verify-local --tuple "${fixture}/tuple.json"

    # Immutable source billing authority is judged at source artifact creation,
    # not at a later Task 5/9 replay within the artifact's 30-day lifetime.
    local aged_source="${fixture}/aged-valid-source.json"
    jq '
      (now - 259200 | floor) as $created_epoch |
      ($created_epoch | todateiso8601) as $created |
      ($created_epoch + 2592000 | todateiso8601) as $expires |
      ($created_epoch - 60 | todateiso8601) as $snapshot |
      ($created_epoch - 300 | todateiso8601) as $scanned |
      ($created_epoch - 600 | todateiso8601) as $db |
      .artifact.created_at=$created |
      .artifact.expires_at=$expires |
      .storage.rest_created_at=$created |
      .storage.rest_expires_at=$expires |
      .storage.snapshot_at=$snapshot |
      .scanner.db_updated_at=$db |
      .scanner.db_downloaded_at=$db |
      .scan.scanned_at=$scanned |
      .scan.source_artifact_created_at=$created
    ' "${fixture}/tuple.json" > "${aged_source}"
    "${ARTIFACT_SCRIPT}" verify-local --tuple "${aged_source}"

    local snapshot_mutation="${fixture}/snapshot-mutation.json"
    jq '.storage.snapshot_at=(.artifact.created_at | fromdateiso8601 + 300 | todateiso8601)' \
      "${aged_source}" > "${snapshot_mutation}"
    "${ARTIFACT_SCRIPT}" verify-local --tuple "${snapshot_mutation}"
    jq '.storage.snapshot_at=(.artifact.created_at | fromdateiso8601 - 86401 | todateiso8601)' \
      "${aged_source}" > "${snapshot_mutation}"
    expect_failure "billing snapshot was stale at source artifact creation" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${snapshot_mutation}"
    jq '.storage.snapshot_at=(.artifact.created_at | fromdateiso8601 + 301 | todateiso8601)' \
      "${aged_source}" > "${snapshot_mutation}"
    expect_failure "billing snapshot is after source artifact creation" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${snapshot_mutation}"

    "${ARTIFACT_SCRIPT}" seal --input "${fixture}/tuple.json" --output "${fixture}/sealed-tuple.json"
    sha256sum --check "${fixture}/sealed-tuple.json.sha256" >/dev/null
    "${ARTIFACT_SCRIPT}" verify-local --tuple "${fixture}/sealed-tuple.json" >/dev/null
    expect_failure "already exists" "${ARTIFACT_SCRIPT}" seal --input "${fixture}/tuple.json" --output "${fixture}/sealed-tuple.json"

    local mutation="${fixture}/mutation.json"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.head_sha="refs/pull/42/merge"'
    expect_failure "real 40-hex PR head" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.repository="fork/pensyve"'
    expect_failure "source repository" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.workflow="Other"'
    expect_failure "source workflow" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.workflow_path=".github/workflows/ci.yml"'
    expect_failure "source workflow path" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.event="push"'
    expect_failure "event/mode" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.ref="refs/pull/42/merge"'
    expect_failure "branch ref" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.run_id=0'
    expect_failure "run id" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.source.run_attempt=0'
    expect_failure "run attempt" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.number=null'
    expect_failure "pull request number" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.draft=true'
    expect_failure "draft" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.repository="fork/pensyve"'
    expect_failure "pull request repository" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.state="closed"'
    expect_failure "remain open" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.base_ref="dev"'
    expect_failure "base must be main" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.head_repository="fork/pensyve"'
    expect_failure "head repository" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.head_ref="other"'
    expect_failure "head ref" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.pull_request.head_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "head SHA drift" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.artifact.run_id=999'
    expect_failure "run association" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.artifact.name="gateway-image-unrelated"'
    expect_failure "artifact name" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.artifact.conclusion="failure"'
    expect_failure "completed successfully" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.artifact.expires_at=.artifact.created_at'
    expect_failure "retention" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.artifact.created_at="2020-01-01T00:00:00Z" | .artifact.expires_at="2020-01-31T00:00:00Z" | .storage.rest_created_at=.artifact.created_at | .storage.rest_expires_at=.artifact.expires_at'
    expect_failure "expired" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.artifact.size_in_bytes=8192'
    expect_failure "REST size" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.storage.rest_created_at="2020-01-01T00:00:00Z"'
    expect_failure "REST created_at" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.cleanup_required=true'
    expect_failure "cleanup_required=false" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.image.archive_sha256="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "archive checksum" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.image.raw_manifest_media_type="application/vnd.oci.image.manifest.v1+json"'
    expect_failure "raw manifest media type" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.gates.default_stop="signal-override"'
    expect_failure "default_stop" "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}"

    local storage="${fixture}/storage.json"
    jq '.storage' "${fixture}/tuple.json" > "${storage}"
    "${ARTIFACT_SCRIPT}" storage-precheck --input "${storage}" --output "${fixture}/storage-result.json"
    mutate_json "${storage}" "${mutation}" '.runner_available_bytes=1'
    expect_failure "runner disk" "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json"
    mutate_json "${storage}" "${mutation}" '.snapshot_at="2020-01-01T00:00:00Z"'
    expect_failure "billing snapshot" "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json"
    mutate_json "${storage}" "${mutation}" '.approved_dollar_ceiling=0'
    expect_failure "dollar ceiling" "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json"
    mutate_json "${storage}" "${mutation}" '.payment_status="past-due"'
    expect_failure "payment status" "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json"

    jq '. + {rest_size_in_bytes:4096,created_at:.rest_created_at,expires_at:.rest_expires_at}' "${storage}" > "${fixture}/reconcile.json"
    "${ARTIFACT_SCRIPT}" storage-reconcile --input "${fixture}/reconcile.json" --output "${fixture}/reconcile-result.json"
    [[ "$(jq -r '.cleanup_required' "${fixture}/reconcile-result.json")" == "false" ]] || fail "within-ceiling reconciliation requested cleanup"
    jq '.approved_dollar_ceiling=0.0000000001 | .rest_size_in_bytes=1000000000' "${fixture}/reconcile.json" > "${mutation}"
    "${ARTIFACT_SCRIPT}" storage-reconcile --input "${mutation}" --output "${fixture}/over-ceiling.json"
    [[ "$(jq -r '.cleanup_required' "${fixture}/over-ceiling.json")" == "true" ]] || fail "over-ceiling reconciliation did not require cleanup"
    [[ "$(jq -r '.status' "${fixture}/over-ceiling.json")" == "over-ceiling" ]] || fail "over-ceiling reconciliation did not invalidate artifact"

    make_reviewed_tuple_and_request "${fixture}/tuple.json" "${fixture}/reviewed.json" "${fixture}/request.json"
    "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed.json" --request "${fixture}/request.json" --output "${fixture}/verified-image.json"
    [[ "$(jq -r 'keys | sort | join(",")' "${fixture}/verified-image.json")" == "cleanup_required,deployment,image,scan,scanner,schema_version" ]] \
        || fail "fetch-verify did not emit the fixed promotion shape"
    local request_field
    for request_field in repository workflow workflow_path ref event run_id run_attempt head_sha pull_request_number artifact_id artifact_name; do
        jq --arg field "${request_field}" '.[$field] = (if (.[$field]|type)=="number" then 999999 else "mutation" end)' "${fixture}/request.json" > "${mutation}"
        expect_failure "cross-run ${request_field} mismatch" "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed.json" --request "${mutation}" --output "${fixture}/bad-verified.json"
    done
    mutate_json "${fixture}/request.json" "${mutation}" '.promotion_event="push"'
    expect_failure "promotion event" "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed.json" --request "${mutation}" --output "${fixture}/bad-verified.json"
    mutate_json "${fixture}/request.json" "${mutation}" '.promotion_head_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "promotion head SHA" "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed.json" --request "${mutation}" --output "${fixture}/bad-verified.json"

    python3 - "${WORKFLOW}" "${fixture}/build-resolver.sh" <<'PY'
from pathlib import Path
import sys
import yaml

workflow = yaml.load(Path(sys.argv[1]).read_text(), Loader=yaml.BaseLoader)
matches = [step for step in workflow["jobs"]["artifact-build"]["steps"]
           if step.get("name") == "Resolve source artifact cleanup custody at terminal boundary"]
if len(matches) != 1 or not matches[0].get("run"):
    raise SystemExit("source terminal custody resolver hard target missing")
Path(sys.argv[2]).write_text("#!/usr/bin/env bash\n" + matches[0]["run"])
PY
    chmod +x "${fixture}/build-resolver.sh"
    local build_digest="sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    local build_state="${fixture}/build-custody-state.json"
    local build_output="${fixture}/build-resolver.outputs"
    mkdir -p "${fixture}/bin"
    export STUB_LOG="${fixture}/source-resolver-gh.log"
    export STUB_SOURCE_ARTIFACT_NAME="gateway-image-1234-2-${SOURCE_SHA}"
    export STUB_SOURCE_ARTIFACT_DIGEST="${build_digest}"
    write_stub "${fixture}/bin/gh" '
endpoint=""
for argument in "$@"; do [[ "$argument" == repos/* ]] && endpoint="$argument"; done
if [[ "$endpoint" == *"/actions/runs/1234/artifacts" ]]; then
  case "${STUB_SOURCE_LOOKUP_MODE:-one}" in
    zero) jq -n "[{total_count:0,artifacts:[]}]" ;;
    zero-one)
      lookups=$(grep -Fxc "$(printf "ARG\trepos/major7apps/pensyve/actions/runs/1234/artifacts")" "$STUB_LOG")
      if [[ "$lookups" -eq 1 ]]; then
        jq -n "[{total_count:0,artifacts:[]}]"
      else
        jq -n --arg name "$STUB_SOURCE_ARTIFACT_NAME" --arg digest "$STUB_SOURCE_ARTIFACT_DIGEST" \
          "[{total_count:1,artifacts:[{id:666,name:\$name,digest:\$digest,size_in_bytes:4096,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}}]}]"
      fi ;;
    one) jq -n --arg name "$STUB_SOURCE_ARTIFACT_NAME" --arg digest "$STUB_SOURCE_ARTIFACT_DIGEST" \
      "[{total_count:1,artifacts:[{id:666,name:\$name,digest:\$digest,size_in_bytes:4096,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}}]}]" ;;
    duplicate) jq -n --arg name "$STUB_SOURCE_ARTIFACT_NAME" --arg digest "$STUB_SOURCE_ARTIFACT_DIGEST" \
      "[{total_count:2,artifacts:[{id:666,name:\$name,digest:\$digest,size_in_bytes:4096,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}},{id:667,name:\$name,digest:\$digest,size_in_bytes:4096,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}}]}]" ;;
  esac
  exit 0
fi
echo "unexpected gh source-resolver argv: $*" >&2
exit 95'
    write_stub "${fixture}/bin/sleep" 'exit 0'
    run_build_resolver() {
        local artifact_id="${1-666}" digest="${2-$build_digest}"
        : > "${build_output}"
        PATH="${fixture}/bin:${PATH}" RUNNER_TEMP="${fixture}" GITHUB_OUTPUT="${build_output}" \
          UPLOAD_OUTCOME="${UPLOAD_OUTCOME_OVERRIDE:-success}" \
          GITHUB_REPOSITORY=major7apps/pensyve GITHUB_RUN_ID=1234 GITHUB_RUN_ATTEMPT=2 \
          GITHUB_SHA="${SOURCE_SHA}" ARTIFACT_ID="${artifact_id}" ARTIFACT_DIGEST="${digest}" \
          bash "${fixture}/build-resolver.sh"
    }
    rm -f -- "${build_state}"
    : > "${STUB_LOG}"
    STUB_SOURCE_LOOKUP_MODE=zero UPLOAD_OUTCOME_OVERRIDE=failure run_build_resolver "" ""
    grep -Fx 'cleanup_required=false' "${build_output}" >/dev/null || fail "source no-upload resolver requested cleanup"
    grep -Fx 'cleanup_status=no-upload' "${build_output}" >/dev/null || fail "source no-upload resolver lost status"
    [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 3 ]] \
      || fail "source failure no-artifact inventory did not reach bounded quiescence"
    local source_outcome
    for source_outcome in skipped; do
      rm -f -- "${build_state}"; : > "${STUB_LOG}"
      STUB_SOURCE_LOOKUP_MODE=zero UPLOAD_OUTCOME_OVERRIDE="${source_outcome}" run_build_resolver "" ""
      grep -Fx 'cleanup_required=false' "${build_output}" >/dev/null || fail "source ${source_outcome} zero inventory requested cleanup"
      grep -Fx 'cleanup_status=no-upload' "${build_output}" >/dev/null || fail "source ${source_outcome} zero inventory lost status"
      [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 3 ]] \
        || fail "source ${source_outcome} zero inventory did not reach quiescence"
    done
    rm -f -- "${build_state}"; : > "${STUB_LOG}"
    STUB_SOURCE_LOOKUP_MODE=zero UPLOAD_OUTCOME_OVERRIDE=success run_build_resolver "" ""
    grep -Fx 'cleanup_required=true' "${build_output}" >/dev/null || fail "source success zero inventory suppressed cleanup custody"
    grep -Fx 'cleanup_status=post-upload-unresolved' "${build_output}" >/dev/null || fail "source success zero inventory lost unresolved status"
    grep -Fx "artifact_name=gateway-image-1234-2-${SOURCE_SHA}" "${build_output}" >/dev/null \
      || fail "source success zero inventory lost exact cleanup-by-name custody"
    [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 3 ]] \
      || fail "source success zero inventory did not reach bounded resolver"
    for source_outcome in success failure skipped; do
      rm -f -- "${build_state}"; : > "${STUB_LOG}"
      STUB_SOURCE_LOOKUP_MODE=one UPLOAD_OUTCOME_OVERRIDE="${source_outcome}" run_build_resolver "" ""
      grep -Fx 'artifact_id=666' "${build_output}" >/dev/null || fail "source ${source_outcome} one inventory lost recovered ID"
      grep -Fx 'cleanup_required=true' "${build_output}" >/dev/null || fail "source ${source_outcome} one inventory suppressed invalid cleanup"
    done
    rm -f -- "${build_state}"; : > "${STUB_LOG}"
    STUB_SOURCE_LOOKUP_MODE=zero-one UPLOAD_OUTCOME_OVERRIDE=success run_build_resolver "" ""
    grep -Fx 'artifact_id=666' "${build_output}" >/dev/null || fail "source eventual zero-to-one upload was not recovered"
    [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 2 ]] \
      || fail "source eventual zero-to-one recovery cardinality mismatch"
    for source_outcome in success failure skipped; do
      rm -f -- "${build_state}"; : > "${STUB_LOG}"
      STUB_SOURCE_LOOKUP_MODE=duplicate UPLOAD_OUTCOME_OVERRIDE="${source_outcome}" \
        capture_failure "${fixture}/source-${source_outcome}-duplicate.log" run_build_resolver \
          "" ""
      cp -- "${build_output}" "${fixture}/source-${source_outcome}-duplicate.outputs"
      grep -Fx 'cleanup_required=true' "${build_output}" >/dev/null \
        || fail "source ${source_outcome} duplicate inventory lost cleanup custody"
      grep -Fx "artifact_name=gateway-image-1234-2-${SOURCE_SHA}" \
        "${build_output}" >/dev/null \
        || fail "source ${source_outcome} duplicate inventory lost exact name custody"
    done
    : > "${STUB_LOG}"
    run_build_resolver "" ""
    grep -Fx 'cleanup_required=true' "${build_output}" >/dev/null || fail "successful source upload with missing ID emitted false cleanup"
    grep -Fx 'cleanup_status=post-upload-invalid' "${build_output}" >/dev/null || fail "successful source upload with missing ID lost fail-safe status"
    [[ "$(grep -Fxc $'ARG\trepos/major7apps/pensyve/actions/runs/1234/artifacts' "${STUB_LOG}")" -eq 1 ]] \
      || fail "lost-response source resolver did not perform one exact current-run lookup"
    STUB_SOURCE_LOOKUP_MODE=duplicate capture_failure "${fixture}/source-resolver-duplicate-upload.log" \
      run_build_resolver "" ""
    grep -F "ambiguous source upload custody: matches=2" "${fixture}/source-resolver-duplicate-upload.log" >/dev/null \
      || fail "duplicate source discovery did not fail closed"
    jq -n --arg digest "${build_digest}" --arg sha "${SOURCE_SHA}" '
      {schema_version:1,artifact_id:666,artifact_name:("gateway-image-1234-2-"+$sha),
       artifact_digest:$digest,repository:"major7apps/pensyve",run_id:1234,run_attempt:2,
       reviewed_sha:$sha,status:"accepted",tuple_sha256:("a"*64),seal_replay_sha256:("b"*64)}' \
      > "${build_state}"
    cp -- "${build_state}" "${fixture}/accepted-build-custody-state.json"
    run_build_resolver
    grep -Fx 'cleanup_required=false' "${build_output}" >/dev/null || fail "accepted source resolver requested cleanup"
    grep -Fx 'cleanup_status=accepted' "${build_output}" >/dev/null || fail "accepted source resolver lost status"
    local build_filter build_name
    while IFS='|' read -r build_name build_filter; do
        jq "${build_filter}" "${fixture}/accepted-build-custody-state.json" > "${build_state}"
        run_build_resolver
        grep -Fx 'cleanup_required=true' "${build_output}" >/dev/null \
          || fail "source resolver accepted mutated ${build_name} custody state"
        grep -Fx 'cleanup_status=post-upload-invalid' "${build_output}" >/dev/null \
          || fail "source resolver lost fail-safe status for mutated ${build_name} custody state"
    done <<'EOF'
id|.artifact_id=999
name|.artifact_name="gateway-image-unrelated"
digest|.artifact_digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
repository|.repository="other/pensyve"
run|.run_id=9999
attempt|.run_attempt=3
sha|.reviewed_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
seal|.seal_replay_sha256="invalid"
tuple|.tuple_sha256="invalid"
status|.status="unknown"
EOF
    jq '.status="over-ceiling" | .tuple_sha256=null' "${fixture}/accepted-build-custody-state.json" > "${build_state}"
    run_build_resolver
    grep -Fx 'cleanup_required=true' "${build_output}" >/dev/null || fail "over-ceiling source resolver lost cleanup custody"
    grep -Fx 'cleanup_status=over-ceiling' "${build_output}" >/dev/null || fail "over-ceiling source resolver lost status"
    echo "artifact custody contract passed"
}

run_release_scan() {
    require_scripts
    local fixture="${TEST_ROOT}/release"
    make_local_fixture "${fixture}"
    jq 'del(.scan.source_artifact_created_at)' "${fixture}/tuple.json" > "${fixture}/pre-upload-tuple.json"
    "${ARTIFACT_SCRIPT}" verify-scan-preupload --tuple "${fixture}/pre-upload-tuple.json"
    expect_failure "must not contain source artifact creation" \
      "${ARTIFACT_SCRIPT}" verify-scan-preupload --tuple "${fixture}/tuple.json"
    "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${fixture}/tuple.json"

    jq '.scanner.db_updated_at="2025-01-01T00:00:00Z" |
        .scanner.db_downloaded_at="2025-01-01T00:00:01Z" |
        .scan.scanned_at="2025-01-01T12:00:00Z" |
        .scan.source_artifact_created_at="2025-01-01T12:05:00Z"' \
      "${fixture}/tuple.json" > "${fixture}/later-replay.json"
    "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${fixture}/later-replay.json"

    local mutation="${fixture}/mutation.json"
    mutate_json "${fixture}/pre-upload-tuple.json" "${mutation}" 'del(.scan.scanned_at)'
    expect_failure "missing tuple field: scan.scanned_at" "${ARTIFACT_SCRIPT}" verify-scan-preupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" 'del(.scan.source_artifact_created_at)'
    expect_failure "missing tuple field: scan.source_artifact_created_at" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scan.scanned_at=(.scan.source_artifact_created_at | fromdateiso8601 + 1 | todateiso8601)'
    expect_failure "after source artifact creation" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scanner.version="0.0.0"'
    expect_failure "scanner version" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scanner.image_digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "scanner digest" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scanner.argv[4]="--skip-db-update=wrong"'
    expect_failure "scanner argv" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scanner.db_updated_at="2020-01-01T00:00:00Z"'
    expect_failure "Trivy DB is stale" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scanner.db_updated_at="2030-01-01T00:00:00Z" | .scanner.db_downloaded_at="2030-01-01T00:00:00Z"'
    expect_failure "stale at scan time" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scan.source_artifact_created_at="2020-01-01T00:00:00Z"'
    expect_failure "after source artifact creation" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scan.source_artifact_created_at=(.scan.scanned_at | fromdateiso8601 - 1 | todateiso8601)'
    expect_failure "after source artifact creation" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scanner.db_sha256="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "Trivy DB hash" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scanner.db_oci_digest="floating"'
    expect_failure "DB OCI digest" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scan.report_sha256="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "scan report hash" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scan.config_id="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "scan subject" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    mutate_json "${fixture}/tuple.json" "${mutation}" '.scan.policy_result="suppressed"'
    expect_failure "policy result" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"

    jq '.Suppressions=[{"VulnerabilityID":"CVE-MUTATION"}]' "${fixture}/scan-report.json" > "${fixture}/scan-mutated.json"
    local report_sha
    report_sha="$(sha256sum "${fixture}/scan-mutated.json" | cut -d' ' -f1)"
    jq --arg path "${fixture}/scan-mutated.json" --arg sha "${report_sha}" '.scan.report_path=$path | .scan.report_sha256=$sha | .scanner.argv[-1]=$path' "${fixture}/tuple.json" > "${mutation}"
    expect_failure "suppression" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"

    jq '.Results[0].Packages[0].Version="0.25.3" | .Results[0].Packages[0].Release="4ubuntu2.1"' "${fixture}/scan-report.json" > "${fixture}/scan-mutated.json"
    report_sha="$(sha256sum "${fixture}/scan-mutated.json" | cut -d' ' -f1)"
    jq --arg path "${fixture}/scan-mutated.json" --arg sha "${report_sha}" '.scan.report_path=$path | .scan.report_sha256=$sha | .scanner.argv[-1]=$path' "${fixture}/tuple.json" > "${mutation}"
    expect_failure "libp11-kit0" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"

    local cve severity
    for cve in CVE-2026-13757 CVE-2026-18938; do
        jq --arg cve "${cve}" '.Results[0].Vulnerabilities=[{VulnerabilityID:$cve,Severity:"MEDIUM"}]' "${fixture}/scan-report.json" > "${fixture}/scan-mutated.json"
        report_sha="$(sha256sum "${fixture}/scan-mutated.json" | cut -d' ' -f1)"
        jq --arg path "${fixture}/scan-mutated.json" --arg sha "${report_sha}" '.scan.report_path=$path | .scan.report_sha256=$sha | .scanner.argv[-1]=$path' "${fixture}/tuple.json" > "${mutation}"
        expect_failure "${cve}" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    done
    for severity in HIGH CRITICAL; do
        jq --arg severity "${severity}" '.Results[0].Vulnerabilities=[{VulnerabilityID:"CVE-MUTATION",Severity:$severity}]' "${fixture}/scan-report.json" > "${fixture}/scan-mutated.json"
        report_sha="$(sha256sum "${fixture}/scan-mutated.json" | cut -d' ' -f1)"
        jq --arg path "${fixture}/scan-mutated.json" --arg sha "${report_sha}" '.scan.report_path=$path | .scan.report_sha256=$sha | .scanner.argv[-1]=$path' "${fixture}/tuple.json" > "${mutation}"
        expect_failure "${severity}" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    done
    jq '.Results[0].Secrets=[{RuleID:"mutation",Severity:"HIGH"}]' "${fixture}/scan-report.json" > "${fixture}/scan-mutated.json"
    report_sha="$(sha256sum "${fixture}/scan-mutated.json" | cut -d' ' -f1)"
    jq --arg path "${fixture}/scan-mutated.json" --arg sha "${report_sha}" '.scan.report_path=$path | .scan.report_sha256=$sha | .scanner.argv[-1]=$path' "${fixture}/tuple.json" > "${mutation}"
    expect_failure "secret" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    jq '.Results[0].Misconfigurations=[{ID:"MUTATION",Severity:"CRITICAL"}]' "${fixture}/scan-report.json" > "${fixture}/scan-mutated.json"
    report_sha="$(sha256sum "${fixture}/scan-mutated.json" | cut -d' ' -f1)"
    jq --arg path "${fixture}/scan-mutated.json" --arg sha "${report_sha}" '.scan.report_path=$path | .scan.report_sha256=$sha | .scanner.argv[-1]=$path' "${fixture}/tuple.json" > "${mutation}"
    expect_failure "misconfiguration" "${ARTIFACT_SCRIPT}" verify-scan-postupload --tuple "${mutation}"
    echo "release scan policy contract passed"
}

write_stub() {
    local path="$1" body="$2"
    {
        printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
        printf '%s\n' 'printf "BEGIN\t%s\n" "$(basename -- "$0")" >> "$STUB_LOG"'
        printf '%s\n' 'for arg in "$@"; do printf "ARG\t%s\n" "$arg" >> "$STUB_LOG"; done'
        printf '%s\n' 'printf "END\n" >> "$STUB_LOG"'
        printf '%s\n' "${body}"
    } > "${path}"
    chmod +x "${path}"
}

call_count() {
    local log="$1" command="$2" word="$3"
    awk -v command="${command}" -v word="${word}" '
        $1=="BEGIN" {active=($2==command); found=0}
        active && $1=="ARG" && $2==word {found=1}
        $1=="END" && active && found {count++}
        END {print count+0}
    ' "${log}"
}

validate_cleanup_log() {
    local log="$1" artifact_id="$2"
    local endpoint="repos/major7apps/pensyve/actions/artifacts/${artifact_id}"
    local expected="${log}.expected"
    printf 'BEGIN\tgh\nARG\tapi\nARG\t%s\nEND\nBEGIN\tgh\nARG\tapi\nARG\t--method\nARG\tDELETE\nARG\t%s\nEND\nBEGIN\tgh\nARG\tapi\nARG\t--include\nARG\t%s\nEND\n' \
        "${endpoint}" "${endpoint}" "${endpoint}" > "${expected}"
    cmp --silent "${expected}" "${log}" || { diff -u "${expected}" "${log}" >&2 || true; fail "cleanup exact argv/cardinality mismatch"; }
}

run_cleanup_program() {
    local program="$1" output="$2"
    shift 2
    : > "${output}.summary"
    capture_failure "${output}" env GITHUB_STEP_SUMMARY="${output}.summary" \
      PRICE_PER_GB_MONTH=0.008 BILLING_UNIT=GB-month "$@" bash "${program}"
}

run_cleanup() {
    local fixture="${TEST_ROOT}/cleanup"
    mkdir -p "${fixture}/bin"
    local program="${fixture}/cleanup.sh"
    python3 - "${WORKFLOW}" "${program}" <<'PY'
from pathlib import Path
import sys
import yaml

workflow = yaml.load(Path(sys.argv[1]).read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["artifact-cleanup"]["steps"]
matches = [step for step in steps if step.get("name") == "Delete exact failed current-run artifact once and prove 404"]
if len(matches) != 1 or not matches[0].get("run"):
    raise SystemExit("cleanup executable hard target lookup failed")
Path(sys.argv[2]).write_text("#!/usr/bin/env bash\n" + matches[0]["run"])
PY
    chmod +x "${program}"
    export STUB_LOG="${fixture}/gh.log"
    write_stub "${fixture}/bin/gh" '
endpoint=""
for argument in "$@"; do [[ "$argument" == repos/* ]] && endpoint="$argument"; done
if [[ "$endpoint" == "repos/major7apps/pensyve/actions/runs/1234/artifacts" ]]; then
  lookups=$(grep -Fxc "$(printf "ARG\trepos/major7apps/pensyve/actions/runs/1234/artifacts")" "$STUB_LOG")
  artifact_id="${STUB_NAME_ID:-777}"
  artifact_name="${STUB_NAME_NAME:-gateway-image-1234-1-${GITHUB_SHA}}"
  artifact="$(jq -cn --argjson id "$artifact_id" --arg name "$artifact_name" \
    '"'"'{id:$id,name:$name,size_in_bytes:4096,created_at:"2026-08-29T00:00:00Z",expires_at:"2026-09-28T00:00:00Z",expired:false,workflow_run:{id:1234}}'"'"')"
  case "${STUB_NAME_LOOKUP_MODE:-one}" in
    zero) jq -cn '"'"'[{total_count:0,artifacts:[]}]'"'"' ;;
    zero-one)
      if [[ "$lookups" -eq 1 ]]; then jq -cn '"'"'[{total_count:0,artifacts:[]}]'"'"'
      else jq -cn --argjson artifact "$artifact" '"'"'[{total_count:1,artifacts:[$artifact]}]'"'"'; fi ;;
    one) jq -cn --argjson artifact "$artifact" '"'"'[{total_count:1,artifacts:[$artifact]}]'"'"' ;;
    duplicate)
      jq -cn --argjson artifact "$artifact" '"'"'[{total_count:2,artifacts:[$artifact,($artifact + {id:779})]}]'"'"' ;;
  esac
  exit 0
fi
if [[ " $* " == *" --method DELETE "* ]]; then
  exit 0
fi
if [[ " $* " == *" --include "* ]]; then
  if [[ "${STUB_CLEANUP_LOOKUP_MODE:-404}" == "404" ]]; then
    echo "HTTP 404: Not Found" >&2
    exit 1
  fi
  echo "HTTP 200: still exists" >&2
  exit 0
fi
if [[ "$1" == api && "$#" -eq 2 ]]; then
  artifact_id="${2##*/}"
  case "$artifact_id" in
    777) artifact_name="gateway-image-1234-1-${GITHUB_SHA}" ;;
    888) artifact_name="gateway-handoff-1234-1-${GITHUB_SHA}" ;;
    999) artifact_name="unrelated-current-run-artifact" ;;
    *) echo "unknown artifact" >&2; exit 1 ;;
  esac
  rest_id="$artifact_id"; rest_run=1234; expired=false
  case "${STUB_PREGET_MODE:-exact}" in
    wrong-id) rest_id=999 ;;
    wrong-name) artifact_name="unrelated-current-run-artifact" ;;
    wrong-run) rest_run=9999 ;;
    expired) expired=true ;;
  esac
  printf "{\"id\":%s,\"name\":\"%s\",\"size_in_bytes\":4096,\"created_at\":\"2026-08-29T00:00:00Z\",\"expires_at\":\"2026-09-28T00:00:00Z\",\"expired\":%s,\"workflow_run\":{\"id\":%s}}\n" \
    "$rest_id" "$artifact_name" "$expired" "$rest_run"
  exit 0
fi
echo "unexpected gh cleanup argv: $*" >&2
exit 95'
    write_stub "${fixture}/bin/sleep" 'exit 0'

    local common=(
        PATH="${fixture}/bin:${PATH}"
        RUNNER_TEMP="${fixture}"
        GITHUB_REPOSITORY="major7apps/pensyve"
        GITHUB_RUN_ID="1234"
        GITHUB_RUN_ATTEMPT="1"
        GITHUB_SHA="${SOURCE_SHA}"
        BUILD_ID="777"
        BUILD_NAME="gateway-image-1234-1-${SOURCE_SHA}"
        BUILD_REPOSITORY="major7apps/pensyve"
        BUILD_RUN_ID="1234"
        BUILD_STATUS="over-ceiling"
        PREFLIGHT_ID="888"
        PREFLIGHT_NAME="gateway-handoff-1234-1-${SOURCE_SHA}"
        PREFLIGHT_REPOSITORY="major7apps/pensyve"
        PREFLIGHT_RUN_ID="1234"
        PREFLIGHT_STATUS="over-ceiling"
    )
    local output="${fixture}/within-ceiling.log"
    : > "${STUB_LOG}"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=false PREFLIGHT_REQUIRED=false
    [[ ! -s "${STUB_LOG}" ]] || fail "within-ceiling cleanup performed a delete or lookup"
    grep -F "no exact over-ceiling cleanup target" "${output}" >/dev/null \
        || fail "within-ceiling cleanup did not fail closed without mutation"

    : > "${STUB_LOG}"
    output="${fixture}/source-over-ceiling.log"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false
    validate_cleanup_log "${STUB_LOG}" 777
    grep -F "deleted invalid artifact id=777" "${output}" >/dev/null || fail "source cleanup did not name exact artifact"
    for label in 'ID:' 'Name:' 'Size bytes:' 'Created:' 'Expires:' 'Incurred byte-hours:' 'Incurred GB-hours:' 'Incurred dollars:'; do
        grep -F -- "${label}" "${output}.summary" >/dev/null \
          || fail "pre-delete durable incurred-usage summary is missing ${label}"
    done

    : > "${STUB_LOG}"
    output="${fixture}/handoff-over-ceiling.log"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=false PREFLIGHT_REQUIRED=true
    validate_cleanup_log "${STUB_LOG}" 888
    grep -F "deleted invalid artifact id=888" "${output}" >/dev/null || fail "handoff cleanup did not name exact artifact"

    local preget_mode
    for preget_mode in wrong-id wrong-name wrong-run expired; do
        : > "${STUB_LOG}"
        output="${fixture}/preget-${preget_mode}.log"
        run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false \
          STUB_PREGET_MODE="${preget_mode}"
        [[ "$(call_count "${STUB_LOG}" gh DELETE)" -eq 0 ]] || fail "pre-delete ${preget_mode} mutation deleted an artifact"
        [[ "$(call_count "${STUB_LOG}" gh api)" -eq 1 ]] || fail "pre-delete ${preget_mode} mutation lookup cardinality mismatch"
        grep -F "cleanup pre-delete REST binding mismatch" "${output}" >/dev/null \
          || fail "pre-delete ${preget_mode} mutation did not fail loudly"
    done
    : > "${STUB_LOG}"
    output="${fixture}/valid-wrong-artifact-id.log"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false BUILD_ID=999
    [[ "$(call_count "${STUB_LOG}" gh DELETE)" -eq 0 ]] || fail "valid unrelated artifact ID was deleted"
    [[ "$(call_count "${STUB_LOG}" gh api)" -eq 1 ]] || fail "valid unrelated artifact ID was not pre-bound exactly once"

    : > "${STUB_LOG}"
    output="${fixture}/ambiguous-target.log"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=true
    [[ ! -s "${STUB_LOG}" ]] || fail "ambiguous cleanup target performed a mutation"
    grep -F "exactly one over-ceiling cleanup target is required" "${output}" >/dev/null \
        || fail "ambiguous cleanup target did not fail loudly"

    local field value
    for field in BUILD_NAME BUILD_REPOSITORY BUILD_RUN_ID BUILD_STATUS; do
        case "${field}" in
            BUILD_NAME) value="gateway-image-unrelated" ;;
            BUILD_REPOSITORY) value="other/repository" ;;
            BUILD_RUN_ID) value="not-a-run" ;;
            BUILD_STATUS) value="accepted" ;;
        esac
        : > "${STUB_LOG}"
        output="${fixture}/wrong-${field}.log"
        run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false "${field}=${value}"
        [[ ! -s "${STUB_LOG}" ]] || fail "wrong cleanup ${field} performed a mutation"
        grep -F "cleanup target binding mismatch" "${output}" >/dev/null \
            || fail "wrong cleanup ${field} did not fail loudly"
    done

    for field in PREFLIGHT_NAME PREFLIGHT_REPOSITORY PREFLIGHT_RUN_ID PREFLIGHT_STATUS; do
        case "${field}" in
            PREFLIGHT_NAME) value="gateway-handoff-unrelated" ;;
            PREFLIGHT_REPOSITORY) value="other/repository" ;;
            PREFLIGHT_RUN_ID) value="9999" ;;
            PREFLIGHT_STATUS) value="accepted" ;;
        esac
        : > "${STUB_LOG}"
        output="${fixture}/wrong-${field}.log"
        run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=false PREFLIGHT_REQUIRED=true "${field}=${value}"
        [[ ! -s "${STUB_LOG}" ]] || fail "wrong cleanup ${field} performed a mutation"
        grep -F "cleanup target binding mismatch" "${output}" >/dev/null \
            || fail "wrong cleanup ${field} did not fail loudly"
    done

    local custody_kind custody_id custody_name required_args
    for custody_kind in source handoff; do
        if [[ "${custody_kind}" == source ]]; then
            custody_id=777
            custody_name="gateway-image-1234-1-${SOURCE_SHA}"
            required_args=(BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false BUILD_ID="" BUILD_STATUS=post-upload-unresolved)
        else
            custody_id=888
            custody_name="gateway-handoff-1234-1-${SOURCE_SHA}"
            required_args=(BUILD_REQUIRED=false PREFLIGHT_REQUIRED=true PREFLIGHT_ID="" PREFLIGHT_STATUS=post-upload-unresolved)
        fi

        : > "${STUB_LOG}"
        output="${fixture}/${custody_kind}-name-zero-one.log"
        run_cleanup_program "${program}" "${output}" "${common[@]}" "${required_args[@]}" \
          STUB_NAME_LOOKUP_MODE=zero-one STUB_NAME_ID="${custody_id}" STUB_NAME_NAME="${custody_name}"
        [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 2 ]] \
          || fail "${custody_kind} cleanup-by-name did not retry eventual inventory exactly once"
        [[ "$(call_count "${STUB_LOG}" gh DELETE)" -eq 1 ]] \
          || fail "${custody_kind} cleanup-by-name did not delete exactly once"
        [[ "$(call_count "${STUB_LOG}" gh --include)" -eq 1 ]] \
          || fail "${custody_kind} cleanup-by-name did not prove exactly one 404"
        grep -F "deleted invalid artifact id=${custody_id}" "${output}" >/dev/null \
          || fail "${custody_kind} cleanup-by-name did not bind the recovered artifact"

        : > "${STUB_LOG}"
        output="${fixture}/${custody_kind}-name-zero.log"
        run_cleanup_program "${program}" "${output}" "${common[@]}" "${required_args[@]}" \
          STUB_NAME_LOOKUP_MODE=zero STUB_NAME_ID="${custody_id}" STUB_NAME_NAME="${custody_name}"
        [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 3 ]] \
          || fail "${custody_kind} cleanup-by-name did not reach bounded no-artifact quiescence"
        [[ "$(call_count "${STUB_LOG}" gh DELETE)" -eq 0 ]] \
          || fail "${custody_kind} zero-inventory cleanup deleted an artifact"
        grep -F "cleanup-by-name inventory remained empty after bounded quiescence" "${output}" >/dev/null \
          || fail "${custody_kind} zero-inventory cleanup did not fail loudly"

        : > "${STUB_LOG}"
        output="${fixture}/${custody_kind}-name-duplicate.log"
        run_cleanup_program "${program}" "${output}" "${common[@]}" "${required_args[@]}" \
          STUB_NAME_LOOKUP_MODE=duplicate STUB_NAME_ID="${custody_id}" STUB_NAME_NAME="${custody_name}"
        [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 1 ]] \
          || fail "${custody_kind} ambiguous cleanup-by-name did not stop immediately"
        [[ "$(call_count "${STUB_LOG}" gh DELETE)" -eq 0 ]] \
          || fail "${custody_kind} ambiguous cleanup-by-name deleted an artifact"
        grep -F "cleanup-by-name inventory is ambiguous: matches=2" "${output}" >/dev/null \
          || fail "${custody_kind} ambiguous cleanup-by-name did not fail loudly"
    done

    : > "${STUB_LOG}"
    output="${fixture}/wrong-current-attempt.log"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false GITHUB_RUN_ATTEMPT=2
    [[ ! -s "${STUB_LOG}" ]] || fail "wrong current attempt performed a cleanup mutation"
    grep -F "cleanup target binding mismatch" "${output}" >/dev/null || fail "wrong current attempt did not fail loudly"

    : > "${STUB_LOG}"
    output="${fixture}/wrong-current-sha.log"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false \
        GITHUB_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    [[ ! -s "${STUB_LOG}" ]] || fail "wrong current SHA performed a cleanup mutation"
    grep -F "cleanup target binding mismatch" "${output}" >/dev/null || fail "wrong current SHA did not fail loudly"

    : > "${STUB_LOG}"
    output="${fixture}/no-404.log"
    run_cleanup_program "${program}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false STUB_CLEANUP_LOOKUP_MODE=200
    [[ "$(call_count "${STUB_LOG}" gh DELETE)" -eq 1 ]] || fail "no-404 case did not delete exactly once"
    [[ "$(call_count "${STUB_LOG}" gh api)" -eq 3 ]] || fail "no-404 case did not perform one pre-GET, delete, and post-GET"
    [[ "$(call_count "${STUB_LOG}" gh --include)" -eq 1 ]] || fail "no-404 case did not perform exactly one post-delete lookup"
    grep -F "cleanup lookup did not return HTTP 404" "${output}" >/dev/null \
        || fail "no-404 cleanup did not fail loudly"

    local double_delete="${fixture}/cleanup-double-delete.sh"
    cp -- "${program}" "${double_delete}"
    python3 - "${double_delete}" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
s = p.read_text()
target = 'gh api --method DELETE "repos/${repository}/actions/artifacts/${id}"'
if s.count(target) != 1:
    raise SystemExit("cleanup double-delete hard target lookup failed")
p.write_text(s.replace(target, target + "\n" + target, 1))
PY
    : > "${STUB_LOG}"
    output="${fixture}/double-delete.log"
    run_cleanup_program "${double_delete}" "${output}" "${common[@]}" BUILD_REQUIRED=true PREFLIGHT_REQUIRED=false
    expect_failure "cleanup exact argv/cardinality" validate_cleanup_log "${STUB_LOG}" 777
    echo "cleanup executable cardinality contract passed"
}

run_handoff() {
    python3 - "${WORKFLOW}" <<'PY'
import sys
from pathlib import Path
import yaml

workflow = yaml.load(Path(sys.argv[1]).read_text(), Loader=yaml.BaseLoader)
jobs = workflow["jobs"]
preflight = jobs["artifact-promote-preflight"]
promote = jobs["artifact-custodian-producer"]
cleanup = jobs["artifact-cleanup"]

def named(job, name):
    matches = [step for step in job.get("steps", []) if step.get("name") == name]
    if len(matches) != 1:
        raise SystemExit(f"real handoff lifecycle missing exact step: {name}")
    return matches[0]

uploads = [step for step in preflight.get("steps", []) if str(step.get("uses", "")).startswith("actions/upload-artifact@")]
if len(uploads) != 1:
    raise SystemExit("promotion preflight must upload exactly one distinct current-run handoff")
upload = uploads[0]
if upload.get("id") != "handoff-upload":
    raise SystemExit("promotion handoff upload must expose fixed custody ID")
name = upload.get("with", {}).get("name", "")
for token in ("gateway-handoff-", "${{ github.run_id }}", "${{ github.run_attempt }}", "${{ github.sha }}"):
    if token not in name:
        raise SystemExit(f"promotion handoff name lacks current-run binding: {token}")
if "gateway-image-" in name:
    raise SystemExit("promotion handoff must be distinct from reviewed source artifact")
named(preflight, "Reconcile current-run handoff REST and storage ceilings")
resolver = named(preflight, "Resolve handoff artifact cleanup custody at terminal boundary")
if resolver.get("id") != "handoff-custody-resolver" or "always()" not in str(resolver.get("if", "")):
    raise SystemExit("handoff must have one terminal always custody resolver")
step_names = [step.get("name") for step in preflight.get("steps", [])]
if not (step_names.index("Upload one immutable current-run promotion handoff") <
        step_names.index("Reconcile current-run handoff REST and storage ceilings") <
        step_names.index("Resolve handoff artifact cleanup custody at terminal boundary") == len(step_names) - 1):
    raise SystemExit("handoff custody/reconciliation must run only after the upload")
outputs = str(preflight.get("outputs", {}))
if "steps.handoff-custody-resolver.outputs.cleanup_required" not in outputs or "||" in outputs:
    raise SystemExit("post-upload failure must retain only terminal resolver custody outputs")
refetch = named(promote, "Re-fetch exact current-run handoff and immutable source before credentials")
if "steps.verify.outputs.verified_json" in str(preflight.get("outputs", {})):
    raise SystemExit("promotion must not pass trusted verified-image JSON through job-local outputs")
if "actions/artifacts/${HANDOFF_ID}" not in str(refetch.get("run", "")):
    raise SystemExit("mutation job must fetch exact current-run handoff by immutable REST ID")
cleanup_text = "\n".join(str(value) for step in cleanup.get("steps", []) for value in step.values())
if '"$run_id" != "$GITHUB_RUN_ID"' not in cleanup_text:
    raise SystemExit("cleanup must refuse artifacts outside the current workflow run")
if "gateway-handoff-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}-${GITHUB_SHA}" not in cleanup_text:
    raise SystemExit("cleanup must bind promotion handoff names independently from source artifacts")
if "always()" not in str(cleanup.get("if", "")):
    raise SystemExit("cleanup must remain eligible after a post-upload job failure")
PY

    local mutation
    mutation="$(copy_mutation handoff-upload-missing)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import sys
import yaml
p = Path(sys.argv[1])
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["artifact-promote-preflight"]["steps"]
matches = [step for step in steps if step.get("name") == "Upload one immutable current-run promotion handoff"]
if len(matches) != 1:
    raise SystemExit("handoff missing-upload hard target lookup failed")
matches[0]["uses"] = "actions/download-artifact@v4"
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "preflight must upload exactly one" validate_workflow "${mutation}"

    mutation="$(copy_mutation handoff-upload-double)"
    python3 - "${mutation}" <<'PY'
from pathlib import Path
import copy
import sys
import yaml
p = Path(sys.argv[1])
workflow = yaml.load(p.read_text(), Loader=yaml.BaseLoader)
steps = workflow["jobs"]["artifact-promote-preflight"]["steps"]
matches = [step for step in steps if step.get("name") == "Upload one immutable current-run promotion handoff"]
if len(matches) != 1:
    raise SystemExit("handoff double-upload hard target lookup failed")
duplicate = copy.deepcopy(matches[0])
duplicate["name"] = "Forbidden duplicate promotion handoff upload"
steps.insert(steps.index(matches[0]), duplicate)
p.write_text(yaml.safe_dump(workflow, sort_keys=False))
PY
    expect_failure "preflight must upload exactly one" validate_workflow "${mutation}"

    local fixture="${TEST_ROOT}/handoff"
    local reconcile_root="${fixture}/reconcile-handoff"
    local download_handoff="${fixture}/download-handoff"
    local download_source="${fixture}/download-source"
    mkdir -p "${fixture}/bin" "${reconcile_root}" "${download_handoff}" "${download_source}"
    python3 - "${WORKFLOW}" "${fixture}" "${reconcile_root}" "${download_handoff}" "${download_source}" <<'PY'
from pathlib import Path
import sys
import yaml

workflow_path, fixture, reconcile_root, download_handoff, download_source = map(Path, sys.argv[1:])
workflow = yaml.load(workflow_path.read_text(), Loader=yaml.BaseLoader)

def extract(job, name, target, replacements=()):
    matches = [step for step in workflow["jobs"][job]["steps"] if step.get("name") == name]
    if len(matches) != 1 or not matches[0].get("run"):
        raise SystemExit(f"executable handoff hard target missing: {name}")
    body = matches[0]["run"]
    for old, new in replacements:
        body = body.replace(old, str(new))
    target.write_text("#!/usr/bin/env bash\n" + body)

extract(
    "artifact-promote-preflight",
    "Reconcile current-run handoff REST and storage ceilings",
    fixture / "reconcile.sh",
    (("/tmp/pensyve-promotion-handoff", reconcile_root),),
)
extract(
    "artifact-promote-preflight",
    "Resolve handoff artifact cleanup custody at terminal boundary",
    fixture / "resolver.sh",
    (("/tmp/pensyve-promotion-handoff", reconcile_root),),
)
extract(
    "artifact-custodian-producer",
    "Re-fetch exact current-run handoff and immutable source before credentials",
    fixture / "refetch.sh",
    (
        ("/tmp/pensyve-promotion-handoff", download_handoff),
        ("/tmp/pensyve-reviewed-artifact", download_source),
    ),
)
extract(
    "artifact-custodian-finalize",
    "Re-fetch exact sealed promotion-custody before credentials",
    fixture / "finalizer-refetch.sh",
    (
        ("/tmp/pensyve-promotion-custody", download_handoff),
        ("/tmp/pensyve-reviewed-artifact", download_source),
        ("pensyve-mcp-gateway/scripts/gateway-image-artifact.sh", fixture / "finalizer-artifact-wrapper.sh"),
    ),
)
extract("artifact-cleanup", "Delete exact failed current-run artifact once and prove 404", fixture / "cleanup.sh")
PY
    chmod +x "${fixture}"/*.sh

    export STUB_LOG="${fixture}/gh.log"
    export STUB_HANDOFF_ID="888"
    export STUB_HANDOFF_NAME="gateway-handoff-1234-2-${SOURCE_SHA}"
    export STUB_HANDOFF_DIGEST="sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    export STUB_SOURCE_ID="777"
    export STUB_SOURCE_NAME="gateway-image-5678-1-${SOURCE_SHA}"
    export STUB_SOURCE_DIGEST="sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    export STUB_HANDOFF_ZIP="${fixture}/payload-handoff.zip"
    export STUB_SOURCE_ZIP="${fixture}/payload-source.zip"
    export ROUND5_ARTIFACT_SCRIPT="${ARTIFACT_SCRIPT}"
    write_stub "${fixture}/finalizer-artifact-wrapper.sh" '
case "$1" in
  verify-tree|verify-local|verify-handoff) exit 0 ;;
  *) exec "$ROUND5_ARTIFACT_SCRIPT" "$@" ;;
esac'
    write_stub "${fixture}/bin/gh" '
endpoint=""
for argument in "$@"; do [[ "$argument" == repos/* ]] && endpoint="$argument"; done
if [[ "$endpoint" == *"/actions/runs/1234/artifacts" ]]; then
  case "${STUB_UPLOAD_LOOKUP_MODE:-one}" in
    zero) jq -n "[{total_count:0,artifacts:[]}]" ;;
    zero-one)
      lookups=$(grep -Fxc "$(printf "ARG\trepos/major7apps/pensyve/actions/runs/1234/artifacts")" "$STUB_LOG")
      if [[ "$lookups" -eq 1 ]]; then
        jq -n "[{total_count:0,artifacts:[]}]"
      else
        jq -n --argjson id "$STUB_HANDOFF_ID" --arg name "$STUB_HANDOFF_NAME" --arg digest "$STUB_HANDOFF_DIGEST" \
          "[{total_count:1,artifacts:[{id:\$id,name:\$name,digest:\$digest,size_in_bytes:1000,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}}]}]"
      fi ;;
    one) jq -n --argjson id "$STUB_HANDOFF_ID" --arg name "$STUB_HANDOFF_NAME" --arg digest "$STUB_HANDOFF_DIGEST" \
      "[{total_count:1,artifacts:[{id:\$id,name:\$name,digest:\$digest,size_in_bytes:1000,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}}]}]" ;;
    duplicate) jq -n --arg name "$STUB_HANDOFF_NAME" --arg digest "$STUB_HANDOFF_DIGEST" \
      "[{total_count:2,artifacts:[{id:888,name:\$name,digest:\$digest,size_in_bytes:1000,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}},{id:889,name:\$name,digest:\$digest,size_in_bytes:1000,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}}]}]" ;;
    wrong) jq -n "[{total_count:1,artifacts:[{id:999,name:\"unrelated\",digest:\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",size_in_bytes:1,created_at:\"2026-08-29T00:00:00Z\",expires_at:\"2026-09-28T00:00:00Z\",expired:false,workflow_run:{id:1234}}]}]" ;;
  esac
  exit 0
fi
if [[ " $* " == *" --method DELETE "* ]]; then exit 0; fi
if [[ " $* " == *" --include "* ]]; then
  if [[ "${STUB_CLEANUP_LOOKUP_MODE:-404}" == 404 ]]; then echo "HTTP 404: Not Found" >&2; exit 1; fi
  echo "HTTP 200: still exists" >&2; exit 0
fi
if [[ "$endpoint" == *"/actions/artifacts/${STUB_HANDOFF_ID}/zip" ]]; then cat "$STUB_HANDOFF_ZIP"; exit 0; fi
if [[ "$endpoint" == *"/actions/artifacts/${STUB_SOURCE_ID}/zip" ]]; then cat "$STUB_SOURCE_ZIP"; exit 0; fi
if [[ "$endpoint" == *"/actions/artifacts/${STUB_HANDOFF_ID}" ]]; then
  [[ "${STUB_HANDOFF_MODE:-exact}" != rest-fail ]] || { echo "handoff REST failure" >&2; exit 96; }
  id="$STUB_HANDOFF_ID"; name="$STUB_HANDOFF_NAME"; digest="$STUB_HANDOFF_DIGEST"; run=1234
  created="2026-08-29T00:00:00Z"; expires="2026-09-28T00:00:00Z"
  size="${STUB_HANDOFF_SIZE:-${STUB_HANDOFF_ARCHIVE_SIZE:-1000}}"
  case "${STUB_HANDOFF_MODE:-exact}" in
    wrong-id) id=889 ;; wrong-name) name="gateway-handoff-wrong" ;;
    wrong-digest) digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ;;
    wrong-run) run=9999 ;; wrong-size) size=$((size + 1)) ;;
    wrong-created) created="2026-08-29T00:00:01Z" ;; wrong-expires) expires="2026-09-27T00:00:00Z" ;;
  esac
  jq -n --argjson id "$id" --arg name "$name" --arg digest "$digest" --argjson size "$size" --argjson run "$run" \
    --arg created "$created" --arg expires "$expires" \
    "{id:\$id,name:\$name,digest:\$digest,size_in_bytes:\$size,created_at:\$created,expires_at:\$expires,expired:false,workflow_run:{id:\$run}}"
  exit 0
fi
if [[ "$endpoint" == *"/actions/artifacts/${STUB_SOURCE_ID}" ]]; then
  jq -n --argjson id "$STUB_SOURCE_ID" --arg name "$STUB_SOURCE_NAME" --arg digest "$STUB_SOURCE_DIGEST" \
    --argjson size "${STUB_SOURCE_SIZE:-2000}" \
    "{id:\$id,name:\$name,digest:\$digest,size_in_bytes:\$size,created_at:\"2026-08-28T00:00:00Z\",expires_at:\"2026-09-27T00:00:00Z\",expired:false,workflow_run:{id:5678}}"
  exit 0
fi
echo "unexpected gh handoff argv: $*" >&2
exit 95'
    write_stub "${fixture}/bin/sleep" 'exit 0'

    printf 'handoff payload\n' > "${reconcile_root}/verified-image.json"
    jq -n '{source_artifact:{expires_at:"2026-09-27T00:00:00Z"}}' \
      > "${reconcile_root}/handoff-metadata.json"
    "${ARTIFACT_SCRIPT}" seal-tree --root "${reconcile_root}" \
        --manifest "${reconcile_root}/sealed-files.sha256" --transcript "${reconcile_root}/seal-reverify.log"
    jq -n '{snapshot_at:"2026-08-29T00:00:00Z",source_snapshot_at:"2026-08-28T00:00:00Z",
      approved_gb_hours_ceiling:1,approved_dollar_ceiling:100,price_per_gb_month:0.008,
      current_billable_bytes:2000,organization_actions_artifact_bytes:2000,organization_packages_bytes:0,
      snapshot_inclusion_mode:"source-included",retained_source_artifact_id:777,
      retained_source_artifact_bytes:2000,billing_unit:"GB-month",payment_status:"active",spending_status:"within-limit",
      rest_size_in_bytes:1,created_at:"2026-08-29T00:00:00Z",expires_at:"2026-09-28T00:00:00Z"}' \
      > "${fixture}/handoff-storage-precheck.json"

    run_handoff_reconcile() {
        local mode="$1" size="$2"
        : > "${STUB_LOG}"
        rm -f -- "${fixture}/handoff-custody-state.json" "${fixture}/handoff-artifact.json" \
          "${fixture}/handoff-reconcile.json" "${fixture}/handoff-post-upload-seal-reverify.log"
        PATH="${fixture}/bin:${PATH}" RUNNER_TEMP="${fixture}" \
          GITHUB_REPOSITORY=major7apps/pensyve GITHUB_RUN_ID=1234 GITHUB_RUN_ATTEMPT=2 GITHUB_SHA="${SOURCE_SHA}" \
          ARTIFACT_ID="${STUB_HANDOFF_ID}" ARTIFACT_DIGEST="${STUB_HANDOFF_DIGEST}" \
          STUB_HANDOFF_MODE="${mode}" STUB_HANDOFF_SIZE="${size}" \
          bash "${fixture}/reconcile.sh"
    }

    run_handoff_resolver() {
        local output_file="$1" artifact_id="${2-$STUB_HANDOFF_ID}" artifact_digest="${3-$STUB_HANDOFF_DIGEST}"
        : > "${output_file}"
        PATH="${fixture}/bin:${PATH}" RUNNER_TEMP="${fixture}" GITHUB_OUTPUT="${output_file}" \
          UPLOAD_OUTCOME="${UPLOAD_OUTCOME_OVERRIDE:-success}" \
          GITHUB_REPOSITORY=major7apps/pensyve GITHUB_RUN_ID=1234 GITHUB_RUN_ATTEMPT=2 GITHUB_SHA="${SOURCE_SHA}" \
          ARTIFACT_ID="${artifact_id}" ARTIFACT_DIGEST="${artifact_digest}" bash "${fixture}/resolver.sh"
    }

    local no_upload_output="${fixture}/no-upload.outputs"
    : > "${STUB_LOG}"
    STUB_UPLOAD_LOOKUP_MODE=zero UPLOAD_OUTCOME_OVERRIDE=failure run_handoff_resolver "${no_upload_output}" "" ""
    grep -Fx 'cleanup_required=false' "${no_upload_output}" >/dev/null || fail "no-upload resolver requested cleanup"
    grep -Fx 'cleanup_status=no-upload' "${no_upload_output}" >/dev/null || fail "no-upload resolver lost status"
    [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 3 ]] \
      || fail "handoff failure no-artifact inventory did not reach bounded quiescence"
    local handoff_outcome handoff_output
    for handoff_outcome in skipped; do
      : > "${STUB_LOG}"; rm -f -- "${fixture}/handoff-custody-state.json"
      handoff_output="${fixture}/${handoff_outcome}-zero.outputs"
      STUB_UPLOAD_LOOKUP_MODE=zero UPLOAD_OUTCOME_OVERRIDE="${handoff_outcome}" \
        run_handoff_resolver "${handoff_output}" "" ""
      grep -Fx 'cleanup_required=false' "${handoff_output}" >/dev/null || fail "handoff ${handoff_outcome} zero inventory requested cleanup"
      grep -Fx 'cleanup_status=no-upload' "${handoff_output}" >/dev/null || fail "handoff ${handoff_outcome} zero inventory lost status"
      [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 3 ]] \
        || fail "handoff ${handoff_outcome} zero inventory did not reach quiescence"
    done
    : > "${STUB_LOG}"; rm -f -- "${fixture}/handoff-custody-state.json"
    handoff_output="${fixture}/success-zero.outputs"
    STUB_UPLOAD_LOOKUP_MODE=zero UPLOAD_OUTCOME_OVERRIDE=success run_handoff_resolver "${handoff_output}" "" ""
    grep -Fx 'cleanup_required=true' "${handoff_output}" >/dev/null || fail "handoff success zero inventory suppressed cleanup custody"
    grep -Fx 'cleanup_status=post-upload-unresolved' "${handoff_output}" >/dev/null || fail "handoff success zero inventory lost unresolved status"
    grep -Fx "artifact_name=${STUB_HANDOFF_NAME}" "${handoff_output}" >/dev/null \
      || fail "handoff success zero inventory lost cleanup-by-name custody"
    [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 3 ]] \
      || fail "handoff success zero inventory did not reach bounded resolver"
    for handoff_outcome in success failure skipped; do
      : > "${STUB_LOG}"; rm -f -- "${fixture}/handoff-custody-state.json"
      handoff_output="${fixture}/${handoff_outcome}-one.outputs"
      STUB_UPLOAD_LOOKUP_MODE=one UPLOAD_OUTCOME_OVERRIDE="${handoff_outcome}" \
        run_handoff_resolver "${handoff_output}" "" ""
      grep -Fx "artifact_id=${STUB_HANDOFF_ID}" "${handoff_output}" >/dev/null \
        || fail "handoff ${handoff_outcome} one inventory lost recovered ID"
      grep -Fx 'cleanup_required=true' "${handoff_output}" >/dev/null \
        || fail "handoff ${handoff_outcome} one inventory suppressed invalid cleanup"
    done
    : > "${STUB_LOG}"; rm -f -- "${fixture}/handoff-custody-state.json"
    handoff_output="${fixture}/success-zero-one.outputs"
    STUB_UPLOAD_LOOKUP_MODE=zero-one UPLOAD_OUTCOME_OVERRIDE=success run_handoff_resolver "${handoff_output}" "" ""
    grep -Fx "artifact_id=${STUB_HANDOFF_ID}" "${handoff_output}" >/dev/null \
      || fail "handoff eventual zero-to-one upload was not recovered"
    [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/1234/artifacts)" -eq 2 ]] \
      || fail "handoff eventual zero-to-one recovery cardinality mismatch"
    for handoff_outcome in success failure skipped; do
      : > "${STUB_LOG}"; rm -f -- "${fixture}/handoff-custody-state.json"
      handoff_output="${fixture}/${handoff_outcome}-duplicate.outputs"
      STUB_UPLOAD_LOOKUP_MODE=duplicate UPLOAD_OUTCOME_OVERRIDE="${handoff_outcome}" \
        capture_failure "${fixture}/${handoff_outcome}-duplicate.log" run_handoff_resolver \
          "${handoff_output}" "" ""
      grep -Fx 'cleanup_required=true' "${handoff_output}" >/dev/null \
        || fail "handoff ${handoff_outcome} duplicate inventory lost cleanup custody"
      grep -Fx "artifact_name=${STUB_HANDOFF_NAME}" "${handoff_output}" >/dev/null \
        || fail "handoff ${handoff_outcome} duplicate inventory lost exact name custody"
    done
    local invalid_success_output="${fixture}/invalid-success.outputs"
    : > "${STUB_LOG}"
    run_handoff_resolver "${invalid_success_output}" "" ""
    grep -Fx 'cleanup_required=true' "${invalid_success_output}" >/dev/null \
      || fail "successful handoff upload with missing ID emitted false cleanup"
    grep -Fx 'cleanup_status=post-upload-invalid' "${invalid_success_output}" >/dev/null \
      || fail "successful handoff upload with missing ID lost fail-safe status"
    [[ "$(grep -Fxc $'ARG\trepos/major7apps/pensyve/actions/runs/1234/artifacts' "${STUB_LOG}")" -eq 1 ]] \
      || fail "lost-response handoff resolver did not perform one exact current-run lookup"
    STUB_UPLOAD_LOOKUP_MODE=duplicate capture_failure "${fixture}/resolver-duplicate-upload.log" \
      run_handoff_resolver "${fixture}/resolver-duplicate-upload.outputs" "" ""
    grep -F "ambiguous handoff upload custody: matches=2" "${fixture}/resolver-duplicate-upload.log" >/dev/null \
      || fail "duplicate handoff discovery did not fail closed"

    local accepted_output="${fixture}/accepted.outputs"
    run_handoff_reconcile exact 1000
    run_handoff_resolver "${accepted_output}"
    grep -Fx 'cleanup_required=false' "${accepted_output}" >/dev/null || fail "accepted handoff did not reconcile within ceiling"
    grep -Fx 'cleanup_status=accepted' "${accepted_output}" >/dev/null || fail "accepted handoff status mismatch"
    [[ "$(call_count "${STUB_LOG}" gh "repos/major7apps/pensyve/actions/artifacts/888")" -eq 1 ]] \
      || fail "handoff reconciliation must query exact REST artifact once"
    cp -- "${fixture}/handoff-custody-state.json" "${fixture}/accepted-handoff-custody-state.json"

    local state_filter state_name state_output
    while IFS='|' read -r state_name state_filter; do
        cp -- "${fixture}/accepted-handoff-custody-state.json" "${fixture}/handoff-custody-state.json"
        jq "${state_filter}" "${fixture}/handoff-custody-state.json" > "${fixture}/handoff-state.tmp"
        mv -- "${fixture}/handoff-state.tmp" "${fixture}/handoff-custody-state.json"
        state_output="${fixture}/state-${state_name}.outputs"
        run_handoff_resolver "${state_output}"
        grep -Fx 'cleanup_required=true' "${state_output}" >/dev/null \
          || fail "handoff resolver accepted mutated ${state_name} custody state"
        grep -Fx 'cleanup_status=post-upload-invalid' "${state_output}" >/dev/null \
          || fail "handoff resolver lost fail-safe status for mutated ${state_name} custody state"
    done <<'EOF'
id|.artifact_id=999
name|.artifact_name="gateway-handoff-unrelated"
digest|.artifact_digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
size|.artifact_size=0
created|.artifact_created_at=""
expires|.artifact_expires_at=.artifact_created_at
repository|.repository="other/pensyve"
run|.run_id=9999
attempt|.run_attempt=3
sha|.reviewed_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
seal|.seal_replay_sha256="invalid"
status|.status="unknown"
EOF
    cp -- "${fixture}/accepted-handoff-custody-state.json" "${fixture}/handoff-custody-state.json"

    local over_output="${fixture}/over.outputs" over_log="${fixture}/over.log"
    capture_failure "${over_log}" run_handoff_reconcile exact 2000000000
    run_handoff_resolver "${over_output}"
    grep -Fx 'cleanup_required=true' "${over_output}" >/dev/null || fail "actual handoff crossing current-org ceiling did not require cleanup"
    grep -Fx 'cleanup_status=over-ceiling' "${over_output}" >/dev/null || fail "actual handoff crossing ceiling lost status"

    local mode failure_output failure_log
    for mode in rest-fail wrong-id wrong-name wrong-digest wrong-run; do
        failure_output="${fixture}/${mode}.outputs"
        failure_log="${fixture}/${mode}.log"
        capture_failure "${failure_log}" run_handoff_reconcile "${mode}" 1000
        run_handoff_resolver "${failure_output}"
        grep -Fx 'cleanup_required=true' "${failure_output}" >/dev/null || fail "handoff ${mode} lost post-upload cleanup custody"
        grep -Fx 'cleanup_status=post-upload-invalid' "${failure_output}" >/dev/null || fail "handoff ${mode} did not remain fail-safe"
    done

    cp -- "${fixture}/handoff-storage-precheck.json" "${fixture}/invalid-storage.json"
    jq '.organization_packages_bytes=41' "${fixture}/invalid-storage.json" > "${fixture}/invalid-storage.tmp"
    mv -- "${fixture}/invalid-storage.tmp" "${fixture}/handoff-storage-precheck.json"
    failure_output="${fixture}/reconcile-failure.outputs"
    capture_failure "${fixture}/reconcile-failure.log" run_handoff_reconcile exact 1000
    run_handoff_resolver "${failure_output}"
    grep -Fx 'cleanup_required=true' "${failure_output}" >/dev/null || fail "handoff reconcile failure lost cleanup custody"
    jq '.organization_packages_bytes=0' "${fixture}/handoff-storage-precheck.json" > "${fixture}/storage-restored.tmp"
    mv -- "${fixture}/storage-restored.tmp" "${fixture}/handoff-storage-precheck.json"

    printf 'tampered\n' >> "${reconcile_root}/verified-image.json"
    failure_output="${fixture}/seal-failure.outputs"
    capture_failure "${fixture}/seal-failure.log" run_handoff_reconcile exact 1000
    run_handoff_resolver "${failure_output}"
    grep -Fx 'cleanup_required=true' "${failure_output}" >/dev/null || fail "handoff seal failure lost cleanup custody"

    local cleanup_output="${fixture}/integrated-cleanup.log"
    : > "${STUB_LOG}"
    run_cleanup_program "${fixture}/cleanup.sh" "${cleanup_output}" \
      PATH="${fixture}/bin:${PATH}" GITHUB_REPOSITORY=major7apps/pensyve GITHUB_RUN_ID=1234 \
      GITHUB_RUN_ATTEMPT=2 GITHUB_SHA="${SOURCE_SHA}" RUNNER_TEMP="${fixture}" \
      BUILD_REQUIRED=false BUILD_ID=777 BUILD_NAME="gateway-image-1234-1-${SOURCE_SHA}" \
      BUILD_REPOSITORY=major7apps/pensyve BUILD_RUN_ID=1234 BUILD_STATUS=over-ceiling \
      PREFLIGHT_REQUIRED=true PREFLIGHT_ID="$(sed -n 's/^artifact_id=//p' "${over_output}" | tail -1)" \
      PREFLIGHT_NAME="$(sed -n 's/^artifact_name=//p' "${over_output}" | tail -1)" \
      PREFLIGHT_REPOSITORY="$(sed -n 's/^repository=//p' "${over_output}" | tail -1)" \
      PREFLIGHT_RUN_ID="$(sed -n 's/^run_id=//p' "${over_output}" | tail -1)" \
      PREFLIGHT_STATUS="$(sed -n 's/^cleanup_status=//p' "${over_output}" | tail -1)"
    validate_cleanup_log "${STUB_LOG}" 888
    if grep -F 'actions/artifacts/777' "${STUB_LOG}" >/dev/null; then fail "handoff cleanup touched reviewed source artifact"; fi

    rm -rf -- "${download_handoff}" "${download_source}"
    mkdir -p "${download_handoff}" "${download_source}"
    printf 'source payload\n' > "${download_source}/source-evidence.json"
    "${ARTIFACT_SCRIPT}" seal-tree --root "${download_source}" \
      --manifest "${download_source}/sealed-files.sha256" --transcript "${download_source}/seal-reverify.log"
    (cd "${download_source}" && zip -q "${STUB_SOURCE_ZIP}" ./*)
    export STUB_SOURCE_DIGEST="sha256:$(sha256sum "${STUB_SOURCE_ZIP}" | cut -d' ' -f1)"
    export STUB_SOURCE_SIZE="$(stat -c '%s' "${STUB_SOURCE_ZIP}")"

    local verified_sha complete_sha storage_approval_sha snapshot_now source_snapshot
    snapshot_now="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    source_snapshot="$(date -u -d '1 hour ago' +'%Y-%m-%dT%H:%M:%SZ')"
    printf '{"schema_version":1,"deployment":{"probe_entity":"task9-runtime-1234-2-0123456789abcdef"}}\n' \
      > "${download_handoff}/verified-image.json"
    jq -n --argjson id "$STUB_SOURCE_ID" --arg name "$STUB_SOURCE_NAME" --arg digest "$STUB_SOURCE_DIGEST" --argjson size "$STUB_SOURCE_SIZE" \
      --arg sha "$SOURCE_SHA" '{artifact:{id:$id,name:$name,server_digest:$digest,size_in_bytes:$size,
        created_at:"2026-08-28T00:00:00Z",expires_at:"2026-09-27T00:00:00Z",retention_days:30,
        repository:"major7apps/pensyve",run_id:5678,run_attempt:1,status:"completed",conclusion:"success"},
        source:{repository:"major7apps/pensyve",head_sha:$sha}}' > "${download_handoff}/complete-tuple.json"
    jq -n --arg snapshot "${snapshot_now}" --arg source_snapshot "${source_snapshot}" \
      --argjson retained_source_bytes "${STUB_SOURCE_SIZE}" \
      '{schema_version:1,snapshot_at:$snapshot,source_snapshot_at:$source_snapshot,
        approved_gb_hours_ceiling:1,approved_dollar_ceiling:100,price_per_gb_month:0.008,
        current_billable_bytes:2000,archive_bytes:0,evidence_bytes:0,container_overhead_bytes:0,
        handoff_overhead_bytes:512,runner_available_bytes:100000000,
        organization_actions_artifact_bytes:2000,organization_packages_bytes:0,
        snapshot_inclusion_mode:"source-included",retained_source_artifact_id:777,
        retained_source_artifact_bytes:$retained_source_bytes,billing_unit:"GB-month",payment_status:"active",
        spending_status:"within-limit"}' > "${download_handoff}/storage-approval.json"
    verified_sha="$(sha256sum "${download_handoff}/verified-image.json" | cut -d' ' -f1)"
    complete_sha="$(sha256sum "${download_handoff}/complete-tuple.json" | cut -d' ' -f1)"
    storage_approval_sha="$(sha256sum "${download_handoff}/storage-approval.json" | cut -d' ' -f1)"
    jq -n --argjson id "$STUB_SOURCE_ID" --arg name "$STUB_SOURCE_NAME" --arg digest "$STUB_SOURCE_DIGEST" --argjson size "$STUB_SOURCE_SIZE" \
      --arg sha "$SOURCE_SHA" --arg verified "$verified_sha" --arg complete "$complete_sha" \
      --arg storage "$storage_approval_sha" --arg probe_entity "task9-runtime-1234-2-0123456789abcdef" \
      '{schema_version:1,repository:"major7apps/pensyve",workflow:"Build & Deploy Gateway",
        workflow_path:".github/workflows/deploy-gateway.yml",run_id:1234,run_attempt:2,reviewed_sha:$sha,
        source_artifact:{id:$id,name:$name,server_digest:$digest,size_in_bytes:$size,
          created_at:"2026-08-28T00:00:00Z",expires_at:"2026-09-27T00:00:00Z",retention_days:30,
          repository:"major7apps/pensyve",run_id:5678,run_attempt:1,status:"completed",conclusion:"success"},
        reviewed_pull_request:{number:16},complete_tuple_sha256:$complete,verified_image_sha256:$verified,
        storage_approval_sha256:$storage,probe_entity:$probe_entity}' \
      > "${download_handoff}/handoff-metadata.json"
    "${ARTIFACT_SCRIPT}" seal-tree --root "${download_handoff}" \
      --manifest "${download_handoff}/sealed-files.sha256" --transcript "${download_handoff}/seal-reverify.log"
    chmod 0644 "${download_handoff}"/*
    (cd "${download_handoff}" && zip -q "${STUB_HANDOFF_ZIP}" ./*)
    export STUB_HANDOFF_DIGEST="sha256:$(sha256sum "${STUB_HANDOFF_ZIP}" | cut -d' ' -f1)"
    export STUB_HANDOFF_ARCHIVE_SIZE="$(stat -c '%s' "${STUB_HANDOFF_ZIP}")"
    rm -rf -- "${download_handoff}" "${download_source}"

    run_handoff_refetch() {
        local program="${1:-${fixture}/refetch.sh}" current_run="${2:-1234}"
        : > "${STUB_LOG}"
        rm -rf -- "${download_handoff}" "${download_source}"
        PATH="${fixture}/bin:${PATH}" RUNNER_TEMP="${fixture}" GITHUB_REPOSITORY=major7apps/pensyve \
          GITHUB_WORKFLOW="Build & Deploy Gateway" GITHUB_RUN_ID="${current_run}" GITHUB_RUN_ATTEMPT=1 GITHUB_SHA="${SOURCE_SHA}" \
          HANDOFF_ID="${HANDOFF_ID_OVERRIDE:-$STUB_HANDOFF_ID}" HANDOFF_NAME="${HANDOFF_NAME_OVERRIDE:-$STUB_HANDOFF_NAME}" \
          HANDOFF_DIGEST="${HANDOFF_DIGEST_OVERRIDE:-$STUB_HANDOFF_DIGEST}" \
          HANDOFF_SIZE="${HANDOFF_SIZE_OVERRIDE:-$STUB_HANDOFF_ARCHIVE_SIZE}" \
          HANDOFF_CREATED_AT="${HANDOFF_CREATED_OVERRIDE:-2026-08-29T00:00:00Z}" \
          HANDOFF_EXPIRES_AT="${HANDOFF_EXPIRES_OVERRIDE:-2026-09-28T00:00:00Z}" \
          HANDOFF_REPOSITORY=major7apps/pensyve HANDOFF_RUN_ID="${HANDOFF_RUN_OVERRIDE:-1234}" \
          HANDOFF_RUN_ATTEMPT="${HANDOFF_ATTEMPT_OVERRIDE:-2}" \
          HANDOFF_REVIEWED_SHA="${HANDOFF_SHA_OVERRIDE:-$SOURCE_SHA}" STUB_HANDOFF_MODE="${STUB_HANDOFF_MODE:-exact}" \
          CURRENT_APPROVED_GB_HOURS="${CURRENT_APPROVED_GB_HOURS_OVERRIDE:-1}" \
          CURRENT_APPROVED_DOLLARS=100 CURRENT_PRICE_PER_GB_MONTH=0.008 CURRENT_BILLING_UNIT=GB-month \
          BILLING_SNAPSHOT_AT="${BILLING_SNAPSHOT_AT_OVERRIDE:-$snapshot_now}" \
          CURRENT_BILLABLE_BYTES="${CURRENT_BILLABLE_BYTES_OVERRIDE:-2000}" \
          ORGANIZATION_ACTIONS_ARTIFACT_BYTES="${ORGANIZATION_ACTIONS_ARTIFACT_BYTES_OVERRIDE:-2000}" \
          ORGANIZATION_PACKAGES_BYTES="${ORGANIZATION_PACKAGES_BYTES_OVERRIDE:-0}" \
          INCLUDED_SOURCE_ARTIFACT_ID="${INCLUDED_SOURCE_ARTIFACT_ID_OVERRIDE:-777}" \
          INCLUDED_SOURCE_ARTIFACT_BYTES="${INCLUDED_SOURCE_ARTIFACT_BYTES_OVERRIDE:-$STUB_SOURCE_SIZE}" \
          HANDOFF_OVERHEAD_BYTES="${HANDOFF_OVERHEAD_BYTES_OVERRIDE:-512}" \
          PAYMENT_STATUS="${PAYMENT_STATUS_OVERRIDE:-active}" \
          SPENDING_STATUS="${SPENDING_STATUS_OVERRIDE:-within-limit}" \
          bash "${program}"
    }
    run_handoff_refetch
    [[ "$(grep -Fxc $'ARG\trepos/major7apps/pensyve/actions/artifacts/888' "${STUB_LOG}")" -eq 1 ]] \
      || fail "handoff refetch REST lookup cardinality mismatch"
    [[ "$(grep -Fxc $'ARG\trepos/major7apps/pensyve/actions/artifacts/888/zip' "${STUB_LOG}")" -eq 1 ]] \
      || fail "handoff archive fetch cardinality mismatch"
    [[ "$(grep -Fxc $'ARG\trepos/major7apps/pensyve/actions/artifacts/777' "${STUB_LOG}")" -eq 1 ]] \
      || fail "source refetch REST lookup cardinality mismatch"
    [[ "$(grep -Fxc $'ARG\trepos/major7apps/pensyve/actions/artifacts/777/zip' "${STUB_LOG}")" -eq 1 ]] \
      || fail "source archive fetch cardinality mismatch"
    CURRENT_APPROVED_GB_HOURS_OVERRIDE=2 \
      capture_failure "${fixture}/refetch-repository-storage-variable-drift.log" run_handoff_refetch
    grep -F "repository storage variable drift" "${fixture}/refetch-repository-storage-variable-drift.log" >/dev/null \
      || fail "current repository storage-variable drift was not explicit"

    local authority_name authority_assignment
    while IFS='|' read -r authority_name authority_assignment; do
      export "${authority_assignment}"
      capture_failure "${fixture}/promotion-authority-${authority_name}.log" run_handoff_refetch
      unset "${authority_assignment%%=*}"
      grep -F "repository storage variable drift" "${fixture}/promotion-authority-${authority_name}.log" >/dev/null \
        || fail "promotion-authority-${authority_name} mutation did not fail before credentials"
    done <<EOF
snapshot|BILLING_SNAPSHOT_AT_OVERRIDE=2026-08-29T00:00:01Z
current-bytes|CURRENT_BILLABLE_BYTES_OVERRIDE=2001
org-actions|ORGANIZATION_ACTIONS_ARTIFACT_BYTES_OVERRIDE=2001
org-packages|ORGANIZATION_PACKAGES_BYTES_OVERRIDE=1
source-id|INCLUDED_SOURCE_ARTIFACT_ID_OVERRIDE=778
source-bytes|INCLUDED_SOURCE_ARTIFACT_BYTES_OVERRIDE=2001
handoff-overhead|HANDOFF_OVERHEAD_BYTES_OVERRIDE=513
payment|PAYMENT_STATUS_OVERRIDE=inactive
spending|SPENDING_STATUS_OVERRIDE=blocked
EOF

    run_handoff_refetch "${fixture}/finalizer-refetch.sh" 9001
    while IFS='|' read -r authority_name authority_assignment; do
      export "${authority_assignment}"
      capture_failure "${fixture}/custodian-authority-${authority_name}.log" \
        run_handoff_refetch "${fixture}/finalizer-refetch.sh" 9001
      unset "${authority_assignment%%=*}"
      grep -F "repository storage variable drift" "${fixture}/custodian-authority-${authority_name}.log" >/dev/null \
        || fail "custodian-authority-${authority_name} mutation did not fail before credentials"
    done <<EOF
snapshot|BILLING_SNAPSHOT_AT_OVERRIDE=2026-08-29T00:00:01Z
current-bytes|CURRENT_BILLABLE_BYTES_OVERRIDE=2001
org-actions|ORGANIZATION_ACTIONS_ARTIFACT_BYTES_OVERRIDE=2001
org-packages|ORGANIZATION_PACKAGES_BYTES_OVERRIDE=1
source-id|INCLUDED_SOURCE_ARTIFACT_ID_OVERRIDE=778
source-bytes|INCLUDED_SOURCE_ARTIFACT_BYTES_OVERRIDE=2001
handoff-overhead|HANDOFF_OVERHEAD_BYTES_OVERRIDE=513
payment|PAYMENT_STATUS_OVERRIDE=inactive
spending|SPENDING_STATUS_OVERRIDE=blocked
EOF

    HANDOFF_RUN_OVERRIDE=9999 capture_failure "${fixture}/refetch-wrong-run.log" run_handoff_refetch
    HANDOFF_NAME_OVERRIDE=gateway-handoff-wrong capture_failure "${fixture}/refetch-wrong-name.log" run_handoff_refetch
    HANDOFF_ID_OVERRIDE=999 capture_failure "${fixture}/refetch-wrong-id.log" run_handoff_refetch
    HANDOFF_DIGEST_OVERRIDE=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
      capture_failure "${fixture}/refetch-wrong-digest.log" run_handoff_refetch
    HANDOFF_SIZE_OVERRIDE=999999 capture_failure "${fixture}/refetch-wrong-size.log" run_handoff_refetch
    HANDOFF_CREATED_OVERRIDE=2026-08-29T00:00:01Z \
      capture_failure "${fixture}/refetch-wrong-created.log" run_handoff_refetch
    HANDOFF_EXPIRES_OVERRIDE=2026-09-27T00:00:00Z \
      capture_failure "${fixture}/refetch-wrong-expires.log" run_handoff_refetch
    HANDOFF_ATTEMPT_OVERRIDE=3 capture_failure "${fixture}/refetch-wrong-attempt.log" run_handoff_refetch
    HANDOFF_SHA_OVERRIDE=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
      capture_failure "${fixture}/refetch-wrong-reviewed-sha.log" run_handoff_refetch
    cp -- "${STUB_HANDOFF_ZIP}" "${fixture}/corrupt-handoff.zip"
    printf 'corrupt\n' >> "${fixture}/corrupt-handoff.zip"
    local exact_handoff_zip="${STUB_HANDOFF_ZIP}"
    STUB_HANDOFF_ZIP="${fixture}/corrupt-handoff.zip" \
      capture_failure "${fixture}/refetch-corrupt-handoff-archive.log" run_handoff_refetch
    STUB_HANDOFF_ZIP="${exact_handoff_zip}"
    cp -- "${STUB_SOURCE_ZIP}" "${fixture}/corrupt-source.zip"
    printf 'corrupt\n' >> "${fixture}/corrupt-source.zip"
    local exact_source_zip="${STUB_SOURCE_ZIP}"
    STUB_SOURCE_ZIP="${fixture}/corrupt-source.zip" \
      capture_failure "${fixture}/refetch-corrupt-source-archive.log" run_handoff_refetch
    STUB_SOURCE_ZIP="${exact_source_zip}"

    local exact_handoff_digest="${STUB_HANDOFF_DIGEST}"
    local exact_handoff_size="${STUB_HANDOFF_ARCHIVE_SIZE}"
    local archive_mode archive_path archive_log
    for archive_mode in unexpected-dir hidden traversal symlink device unsafe-mode; do
        archive_path="${fixture}/handoff-${archive_mode}.zip"
        python3 - "${exact_handoff_zip}" "${archive_path}" "${archive_mode}" <<'PY'
import stat
import sys
import zipfile

source, destination, mutation = sys.argv[1:]
with zipfile.ZipFile(source) as original, zipfile.ZipFile(destination, "w") as changed:
    entries = [(info, original.read(info.filename)) for info in original.infolist()]
    for info, payload in entries:
        clone = zipfile.ZipInfo(info.filename, info.date_time)
        clone.create_system = 3
        clone.compress_type = info.compress_type
        clone.external_attr = info.external_attr
        if mutation in {"symlink", "device", "unsafe-mode"} and info.filename == "verified-image.json":
            kind = {
                "symlink": stat.S_IFLNK | 0o777,
                "device": stat.S_IFCHR | 0o600,
                "unsafe-mode": stat.S_IFREG | 0o755,
            }[mutation]
            clone.external_attr = kind << 16
            if mutation == "symlink":
                payload = b"complete-tuple.json"
        changed.writestr(clone, payload)
    if mutation in {"unexpected-dir", "hidden", "traversal"}:
        name, mode = {
            "unexpected-dir": ("unexpected/", stat.S_IFDIR | 0o755),
            "hidden": (".hidden", stat.S_IFREG | 0o644),
            "traversal": ("../escape", stat.S_IFREG | 0o644),
        }[mutation]
        extra = zipfile.ZipInfo(name)
        extra.create_system = 3
        extra.external_attr = mode << 16
        changed.writestr(extra, b"" if mutation == "unexpected-dir" else b"forbidden\n")
PY
        STUB_HANDOFF_ZIP="${archive_path}"
        STUB_HANDOFF_DIGEST="sha256:$(sha256sum "${archive_path}" | cut -d' ' -f1)"
        STUB_HANDOFF_ARCHIVE_SIZE="$(stat -c '%s' "${archive_path}")"
        archive_log="${fixture}/refetch-${archive_mode}.log"
        capture_failure "${archive_log}" run_handoff_refetch
        if grep -F 'actions/artifacts/777' "${STUB_LOG}" >/dev/null; then
            fail "invalid ${archive_mode} handoff was allowed to reach reviewed source custody"
        fi
        [[ ! -e "${fixture}/escape" ]] || fail "handoff traversal escaped the extraction root"
    done
    STUB_HANDOFF_ZIP="${exact_handoff_zip}"
    STUB_HANDOFF_DIGEST="${exact_handoff_digest}"
    STUB_HANDOFF_ARCHIVE_SIZE="${exact_handoff_size}"

    echo "promotion handoff executable lifecycle passed"
}

run_promote() {
    require_scripts
    local fixture="${TEST_ROOT}/promote"
    make_local_fixture "${fixture}"
    mkdir -p "${fixture}/bin"
    local environment_sha service_snapshot_sha
    printf '[{"name":"MCP_ALLOWED_HOSTS","value":"mcp.pensyve.com"}]\n' > "${fixture}/environment.json"
    environment_sha="$(jq -S -c . "${fixture}/environment.json" | sha256sum | cut -d' ' -f1)"
    jq -n '{service_name:"pensyve-prod-gateway",status:"ACTIVE",
      cluster_arn:"arn:aws:ecs:us-east-2:123456789012:cluster/pensyve-prod",
      task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",
      counts:{desired:2,running:2,pending:0},
      network_configuration:{awsvpcConfiguration:{subnets:["subnet-aaa","subnet-bbb"],securityGroups:["sg-aaa"],assignPublicIp:"DISABLED"}},
      load_balancers:[{targetGroupArn:"arn:aws:elasticloadbalancing:us-east-2:123456789012:targetgroup/pensyve-gateway/abc",containerName:"gateway",containerPort:3100}],
      deployment_configuration:{deploymentCircuitBreaker:{enable:true,rollback:true},maximumPercent:200,minimumHealthyPercent:100},
      health_grace_period_seconds:300,
      primary_deployment:{status:"PRIMARY",task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",rollout_state:"COMPLETED",desired:2,running:2,pending:0}}' \
      > "${fixture}/service-snapshot.json"
    service_snapshot_sha="$(jq -S -c . "${fixture}/service-snapshot.json" | sha256sum | cut -d' ' -f1)"
    jq --arg env_sha "${environment_sha}" --arg snapshot_sha "${service_snapshot_sha}" \
      --slurpfile snapshot "${fixture}/service-snapshot.json" \
      '{schema_version:1,cleanup_required:false,image:.image,scanner:.scanner,scan:.scan,deployment:{region:"us-east-2",ecr_registry:"123456789012.dkr.ecr.us-east-2.amazonaws.com",ecr_repository:"pensyve-gateway",cluster:"pensyve-prod",service:"pensyve-prod-gateway",gateway_container:"gateway",baseline_task_definition_arn:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",baseline_image:"123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",baseline_environment_sha256:$env_sha,baseline_service_snapshot:$snapshot[0],baseline_service_snapshot_sha256:$snapshot_sha,probe_entity:"task9-runtime-1234-2-0123456789abcdef",promotion_run_id:1234,promotion_run_attempt:2,cpu:"512",memory:"4096",desired_count:2,running_count:2,pending_count:0}}' \
        "${fixture}/tuple.json" > "${fixture}/verified-image.json"
    export STUB_LOG="${fixture}/stub.log"
    : > "${STUB_LOG}"
    export STUB_RAW_MANIFEST_PATH="${fixture}/raw-manifest.json"
    export STUB_ENVIRONMENT_PATH="${fixture}/environment.json"
    export STUB_SOURCE_SHA="${SOURCE_SHA}"
    export STUB_CONFIG_ID="$(jq -r '.image.config_id' "${fixture}/tuple.json")"
    export STUB_MANIFEST_DIGEST="$(jq -r '.image.pushed_digest' "${fixture}/tuple.json")"
    export STUB_BASELINE_ARN="$(jq -r '.deployment.baseline_task_definition_arn' "${fixture}/verified-image.json")"
    export GITHUB_RUN_ID=1234
    export GITHUB_RUN_ATTEMPT=2

    write_stub "${fixture}/bin/docker" '
case "${1:-}" in
  load) echo "Loaded image ID: ${STUB_CONFIG_ID}" ;;
  image) echo "${STUB_CONFIG_ID}" ;;
  tag) ;;
  push) echo "digest: ${STUB_MANIFEST_DIGEST} size: 1" ;;
  *) ;;
esac'
    write_stub "${fixture}/bin/aws" '
[[ " $* " == *" --cli-connect-timeout 5 "* && " $* " == *" --cli-read-timeout 30 "* ]] || {
  echo "AWS operation is missing fixed connect/read timeout" >&2
  exit 96
}
update_calls=$(grep -F -c $'"'"'ARG\tupdate-service'"'"' "$STUB_LOG" || true)
rollback_update_calls=$(awk -v baseline="$STUB_BASELINE_ARN" '"'"'
  $1=="BEGIN" {active=($2=="aws"); update=0; matched=0}
  active && $1=="ARG" && $2=="update-service" {update=1}
  active && $1=="ARG" && $2==baseline {matched=1}
  $1=="END" && active && update && matched {count++}
  END {print count+0}
'"'"' "$STUB_LOG")
wait_calls=$(grep -F -c $'"'"'ARG\twait'"'"' "$STUB_LOG" || true)
service_describes=$(grep -F -c $'"'"'ARG\tdescribe-services'"'"' "$STUB_LOG" || true)
task_describes=$(grep -F -c $'"'"'ARG\tdescribe-task-definition'"'"' "$STUB_LOG" || true)
delete_calls=$(grep -F -c $'"'"'ARG\tDELETE'"'"' "$STUB_LOG" || true)
candidate_arn="arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:201"
candidate_describe=0
    if [[ "${STUB_FAIL_CANDIDATE_WAIT:-0}" != "1" && "$update_calls" -eq 1 && "$service_describes" -ge 3 ]]; then
  candidate_describe=1
fi
if [[ "${STUB_FINALIZER_STATE:-}" == candidate && "$rollback_update_calls" -eq 0 ]]; then
  candidate_describe=1
fi
service_task="${STUB_BASELINE_ARN}"
service_desired=2
service_running=2
service_pending=0
    service_status="ACTIVE"
    rollout_state="COMPLETED"
    if [[ "${STUB_PREUPDATE_DRIFT:-0}" == "1" && "$service_describes" -eq 2 ]]; then
      service_task="$candidate_arn"
    fi
if [[ "$candidate_describe" -eq 1 ]]; then
  service_task="$candidate_arn"
  case "${STUB_CANDIDATE_MODE:-exact}" in
    auto-rollback | service-arn) service_task="${STUB_BASELINE_ARN}" ;;
    service-count) service_running=1; service_pending=1 ;;
    service-state) service_status="DRAINING" ;;
    rollout) rollout_state="FAILED" ;;
  esac
fi
if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == cleanup-final-state-drift && "$delete_calls" -ge 1 && "$rollback_update_calls" -eq 0 ]]; then
  service_running=1
  service_pending=1
fi
if [[ "$update_calls" -ge 2 ]]; then
  service_task="${STUB_BASELINE_ARN}"
  service_desired=2
  service_running=2
  service_pending=0
  service_status="ACTIVE"
  rollout_state="COMPLETED"
fi
if [[ "${STUB_FINALIZER_STATE:-}" == candidate && "$rollback_update_calls" -ge 1 ]]; then
  service_task="${STUB_BASELINE_ARN}"
  service_desired=2
  service_running=2
  service_pending=0
  service_status="ACTIVE"
  rollout_state="COMPLETED"
fi
    rollback_describe=4
    if [[ "${STUB_FAIL_CANDIDATE_WAIT:-0}" == "1" ]]; then rollback_describe=3; fi
if [[ "${STUB_ROLLBACK_SERVICE_DRIFT:-0}" == "1" && "$service_describes" -ge "$rollback_describe" ]]; then
  service_task="$candidate_arn"
  service_running=1
  service_pending=1
fi
    subnet_two=subnet-bbb
    target_group="arn:aws:elasticloadbalancing:us-east-2:123456789012:targetgroup/pensyve-gateway/abc"
    circuit_rollback=true
    maximum_percent=200
    minimum_healthy=100
    health_grace=300
    drift_now=0
    case "${STUB_SERVICE_DRIFT_PHASE:-none}:$service_describes" in
      initial:1|preupdate:2|rollback:3|rollback:4) drift_now=1 ;;
    esac
    if [[ "$drift_now" -eq 1 ]]; then
      case "${STUB_SERVICE_DRIFT_FIELD:-none}" in
        network) subnet_two=subnet-drift ;;
        load-balancer) target_group="arn:aws:elasticloadbalancing:us-east-2:123456789012:targetgroup/pensyve-gateway/drift" ;;
        circuit-breaker) circuit_rollback=false ;;
        deployment-config) maximum_percent=150 ;;
        health-grace) health_grace=0 ;;
      esac
    fi
    service_json="{\"services\":[{\"serviceName\":\"pensyve-prod-gateway\",\"status\":\"${service_status}\",\"clusterArn\":\"arn:aws:ecs:us-east-2:123456789012:cluster/pensyve-prod\",\"taskDefinition\":\"${service_task}\",\"desiredCount\":${service_desired},\"runningCount\":${service_running},\"pendingCount\":${service_pending},\"networkConfiguration\":{\"awsvpcConfiguration\":{\"subnets\":[\"subnet-aaa\",\"${subnet_two}\"],\"securityGroups\":[\"sg-aaa\"],\"assignPublicIp\":\"DISABLED\"}},\"loadBalancers\":[{\"targetGroupArn\":\"${target_group}\",\"containerName\":\"gateway\",\"containerPort\":3100}],\"deploymentConfiguration\":{\"deploymentCircuitBreaker\":{\"enable\":true,\"rollback\":${circuit_rollback}},\"maximumPercent\":${maximum_percent},\"minimumHealthyPercent\":${minimum_healthy}},\"healthCheckGracePeriodSeconds\":${health_grace},\"deployments\":[{\"status\":\"PRIMARY\",\"taskDefinition\":\"${service_task}\",\"rolloutState\":\"${rollout_state}\",\"desiredCount\":${service_desired},\"runningCount\":${service_running},\"pendingCount\":${service_pending}}]}]}"
task_arn="${STUB_BASELINE_ARN}"
task_image="123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
task_cpu="${STUB_TASK_CPU:-512}"
task_memory="4096"
if [[ " $* " == *":201"* ]]; then
  task_arn="arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:201"
  task_image="123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@${STUB_MANIFEST_DIGEST}"
  if [[ "${STUB_AFTER_DRIFT:-0}" == "1" ]]; then task_memory="8192"; fi
fi
if [[ "${STUB_ROLLBACK_TASK_DRIFT:-0}" == "1" && "$task_describes" -ge 4 ]]; then
  task_memory="8192"
fi
    log_container=gateway
    log_prefix=ecs
    log_region=us-east-2
    log_group=/ecs/pensyve-prod/gateway
    case "${STUB_LOG_CONFIG_MODE:-exact}" in
      wrong-container) : ;;
      wrong-prefix) log_prefix=../escape ;;
      wrong-region) log_region=us-east-1 ;;
      wrong-group) log_group=not-an-ecs-group ;;
    esac
    log_configuration=",\"logConfiguration\":{\"logDriver\":\"awslogs\",\"options\":{\"awslogs-group\":\"${log_group}\",\"awslogs-region\":\"${log_region}\",\"awslogs-stream-prefix\":\"${log_prefix}\"}}"
    [[ "${STUB_LOG_CONFIG_MODE:-exact}" != absent ]] || log_configuration=""
    task_json="{\"taskDefinition\":{\"taskDefinitionArn\":\"${task_arn}\",\"family\":\"pensyve-prod-gateway\",\"cpu\":\"${task_cpu}\",\"memory\":\"${task_memory}\",\"containerDefinitions\":[{\"name\":\"${log_container}\",\"image\":\"${task_image}\",\"environment\":$(cat "$STUB_ENVIRONMENT_PATH"),\"secrets\":[{\"name\":\"PENSYVE_API_KEYS\",\"valueFrom\":\"/pensyve/prod/api-key\"}]${log_configuration}}],\"volumes\":[],\"requiresCompatibilities\":[\"FARGATE\"],\"networkMode\":\"awsvpc\",\"runtimePlatform\":{\"cpuArchitecture\":\"ARM64\",\"operatingSystemFamily\":\"LINUX\"}}}"
running_task_1="arn:aws:ecs:us-east-2:123456789012:task/pensyve-prod/11111111111111111111111111111111"
running_task_2="arn:aws:ecs:us-east-2:123456789012:task/pensyve-prod/22222222222222222222222222222222"
list_tasks_json="{\"taskArns\":[\"${running_task_1}\",\"${running_task_2}\"]}"
if [[ "${STUB_CANDIDATE_MODE:-exact}" == "list-count" ]]; then
  list_tasks_json="{\"taskArns\":[\"${running_task_1}\"]}"
fi
runtime_task_definition="$candidate_arn"
runtime_digest="$STUB_MANIFEST_DIGEST"
runtime_status="RUNNING"
runtime_desired="RUNNING"
runtime_container="gateway"
[[ "${STUB_LOG_CONFIG_MODE:-exact}" != wrong-container ]] || runtime_container="sidecar"
case "${STUB_CANDIDATE_MODE:-exact}" in
  task-arn) runtime_task_definition="$STUB_BASELINE_ARN" ;;
  image) runtime_digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ;;
  task-status) runtime_status="STOPPED" ;;
esac
if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == duplicate-stream ]]; then
  running_task_2="arn:aws:ecs:us-east-2:123456789012:task/other-cluster/11111111111111111111111111111111"
  list_tasks_json="{\"taskArns\":[\"${running_task_1}\",\"${running_task_2}\"]}"
fi
if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == wrong-task-id ]]; then
  running_task_2="arn:aws:ecs:us-east-2:123456789012:task/pensyve-prod/not-a-task-id"
  list_tasks_json="{\"taskArns\":[\"${running_task_1}\",\"${running_task_2}\"]}"
fi
runtime_task_json="{\"failures\":[],\"tasks\":[{\"taskArn\":\"${running_task_1}\",\"clusterArn\":\"arn:aws:ecs:us-east-2:123456789012:cluster/pensyve-prod\",\"taskDefinitionArn\":\"${runtime_task_definition}\",\"lastStatus\":\"${runtime_status}\",\"desiredStatus\":\"${runtime_desired}\",\"startedAt\":\"2026-08-29T00:00:00Z\",\"attachments\":[{\"details\":[{\"name\":\"privateIPv4Address\",\"value\":\"10.0.1.11\"}]}],\"containers\":[{\"name\":\"${runtime_container}\",\"imageDigest\":\"${runtime_digest}\"}]},{\"taskArn\":\"${running_task_2}\",\"clusterArn\":\"arn:aws:ecs:us-east-2:123456789012:cluster/pensyve-prod\",\"taskDefinitionArn\":\"${runtime_task_definition}\",\"lastStatus\":\"${runtime_status}\",\"desiredStatus\":\"${runtime_desired}\",\"startedAt\":\"2026-08-29T00:00:01Z\",\"attachments\":[{\"details\":[{\"name\":\"privateIPv4Address\",\"value\":\"10.0.1.12\"}]}],\"containers\":[{\"name\":\"${runtime_container}\",\"imageDigest\":\"${runtime_digest}\"}]}]}"
if [[ "${STUB_CANDIDATE_MODE:-exact}" == "task-count" ]]; then
  runtime_task_json="{\"failures\":[],\"tasks\":[$(printf '%s' "$runtime_task_json" | jq -c '.tasks[0]')]}"
fi
case "$1 $2" in
  "ecs describe-services") printf "%s\n" "$service_json" ;;
  "ecs describe-task-definition") printf "%s\n" "$task_json" ;;
  "ecs list-tasks") printf "%s\n" "$list_tasks_json" ;;
      "ecs describe-tasks") printf "%s\n" "$runtime_task_json" ;;
      "elbv2 describe-target-health")
        target_state=healthy
        [[ "${STUB_FUNCTIONAL_MODE:-exact}" != "target-health" ]] || target_state=unhealthy
        printf "%s\n" "{\"TargetHealthDescriptions\":[{\"Target\":{\"Id\":\"10.0.1.11\"},\"TargetHealth\":{\"State\":\"${target_state}\"}},{\"Target\":{\"Id\":\"10.0.1.12\"},\"TargetHealth\":{\"State\":\"healthy\"}}]}"
        ;;
      "logs get-log-events")
        for required in --start-time --end-time --limit --no-paginate; do
          if [[ " $* " != *" $required "* ]]; then
            echo "bounded per-stream log query missing $required" >&2
            exit 95
          fi
        done
        [[ " $* " == *" --limit 1000 "* ]] || { echo "bounded log limit mismatch" >&2; exit 95; }
        embedding_revision=a829fd0e060bb84554da0dfd354d0de0f7712b7f
        [[ "${STUB_FUNCTIONAL_MODE:-exact}" != "startup" ]] || embedding_revision=wrong
        extra=""
        log_calls=$(grep -F -c $'"'"'ARG\tget-log-events'"'"' "$STUB_LOG" || true)
        if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "fallback" && "$log_calls" -ge 3 ]]; then extra=" Reranker unavailable; recall proceeding unreranked"; fi
        if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "fallback-delayed" && "$log_calls" -ge 3 ]]; then extra=" Reranker unavailable; recall proceeding unreranked"; fi
        model_event="{\\\"fields\\\":{\\\"message\\\":\\\"model runtime initialized${extra}\\\",\\\"strict_local_models\\\":false,\\\"embedding_model\\\":\\\"Alibaba-NLP/gte-base-en-v1.5\\\",\\\"embedding_revision\\\":\\\"${embedding_revision}\\\",\\\"reranker_state\\\":\\\"deferred\\\",\\\"reranker_model\\\":\\\"BGERerankerBase\\\",\\\"reranker_revision\\\":\\\"resolved-on-first-use\\\",\\\"cache_root\\\":\\\"/opt/pensyve/models\\\",\\\"embedding_pool_size\\\":1}}"
        stream=one
        [[ " $* " == *" ecs/gateway/22222222222222222222222222222222 "* ]] && stream=two
        timestamp=1787961605000
        [[ "${STUB_FUNCTIONAL_MODE:-exact}" != "stale-event" ]] || timestamp=100
        if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "pagination" && " $* " != *" --next-token page-two "* ]]; then
          printf "%s\n" "{\"events\":[],\"nextForwardToken\":\"page-two\"}"
        elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "pagination-cycle" ]]; then
          token=cycle-a
          [[ " $* " == *" --next-token cycle-a "* ]] && token=cycle-b
          [[ " $* " == *" --next-token cycle-b "* ]] && token=cycle-a
          printf "%s\n" "{\"events\":[{\"timestamp\":${timestamp},\"message\":\"${model_event}\"}],\"nextForwardToken\":\"${token}\"}"
        elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "startup-lag" && "$log_calls" -le 2 ]]; then
          printf "%s\n" "{\"events\":[],\"nextForwardToken\":\"\"}"
        elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "startup-asymmetric" && "$stream" == one ]]; then
          printf "%s\n" "{\"events\":[{\"timestamp\":${timestamp},\"message\":\"${model_event}\"},{\"timestamp\":${timestamp},\"message\":\"${model_event}\"}],\"nextForwardToken\":\"\"}"
        elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "startup-asymmetric" && "$stream" == two ]]; then
          printf "%s\n" "{\"events\":[],\"nextForwardToken\":\"\"}"
        elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "duplicate-delayed" && "$log_calls" -ge 3 && "$stream" == one ]]; then
          printf "%s\n" "{\"events\":[{\"timestamp\":${timestamp},\"message\":\"${model_event}\"},{\"timestamp\":${timestamp},\"message\":\"${model_event}\"}],\"nextForwardToken\":\"\"}"
        elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "startup-duplicate" && "$log_calls" -le 24 ]]; then
          printf "%s\n" "{\"events\":[{\"timestamp\":${timestamp},\"message\":\"unrelated plain-text startup line\"},{\"timestamp\":${timestamp},\"message\":\"${model_event}\"},{\"timestamp\":${timestamp},\"message\":\"${model_event}\"}],\"nextForwardToken\":\"\"}"
        else
          recall_event=""
          if [[ "$log_calls" -ge 3 && "$stream" == one ]]; then
            recall_event=",{\"timestamp\":${timestamp},\"message\":\"{\\\"fields\\\":{\\\"message\\\":\\\"recall completed\\\",\\\"event\\\":\\\"recall_decision\\\",\\\"query\\\":\\\"Which codename is explicitly marked as the selected result by the production reranker proof?\\\",\\\"candidates_found\\\":3,\\\"results_returned\\\":3}}\"}"
          fi
          printf "%s\n" "{\"events\":[{\"timestamp\":${timestamp},\"message\":\"unrelated plain-text startup line\"},{\"timestamp\":${timestamp},\"message\":\"${model_event}\"},{\"timestamp\":${timestamp},\"message\":\"{\\\"fields\\\":{\\\"message\\\":\\\"HTTP listener started\\\"}}\"}${recall_event}],\"nextForwardToken\":\"\"}"
        fi
        ;;
      "ssm get-parameter")
        if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "secret" ]]; then
          exit 82
        elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "invalid-secret" ]]; then
          printf "%s\n" "not-a-pensyve-key"
        else
          printf "%s\n" "  psy_task9-stub-api-key  ,psy_unused"
        fi
        ;;
  "ecr describe-images") printf "{\"imageDetails\":[{\"imageDigest\":\"%s\"}]}\n" "$STUB_MANIFEST_DIGEST" ;;
  "ecr batch-get-image") jq -n --rawfile manifest "${STUB_ECR_MANIFEST_PATH:-$STUB_RAW_MANIFEST_PATH}" --arg digest "$STUB_MANIFEST_DIGEST" "{images:[{imageId:{imageDigest:\$digest},imageManifest:\$manifest,imageManifestMediaType:\"application/vnd.docker.distribution.manifest.v2+json\"}]}" ;;
  "ecs register-task-definition") printf "{\"taskDefinition\":{\"taskDefinitionArn\":\"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:201\"}}\n" ;;
  "ecs update-service")
    if [[ "${STUB_FAIL_ROLLBACK_UPDATE:-0}" == "1" && "$update_calls" -eq 2 ]]; then
      echo "rollback update stub failure" >&2
      exit 93
    fi
    printf "{\"service\":{\"taskDefinition\":\"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:201\"}}\n"
    ;;
  "ecs wait")
    if [[ "${STUB_HANG_CANDIDATE_WAIT:-0}" == "1" && "$wait_calls" -eq 1 ]]; then
      if [[ "${STUB_IGNORE_WAIT_SIGNALS:-0}" == "1" ]]; then
        trap "" INT TERM
        while :; do /bin/sleep 1; done
      else
        sleep 8
      fi
    fi
    if [[ "${STUB_FAIL_CANDIDATE_WAIT:-0}" == "1" && "$wait_calls" -eq 1 ]]; then
      echo "selected wait failure" >&2
      exit 92
    fi
    if [[ "${STUB_FAIL_ROLLBACK_WAIT:-0}" == "1" && "$wait_calls" -eq 2 ]]; then
      echo "rollback wait stub failure" >&2
      exit 94
    fi
    if [[ "${STUB_FINALIZER_STATE:-}" == candidate && "${STUB_FAIL_FINALIZER_ROLLBACK_WAIT:-0}" == 1 && "$wait_calls" -eq 1 ]]; then
      echo "finalizer rollback wait stub failure" >&2
      exit 94
    fi
    ;;
  *) echo "unexpected aws command: $*" >&2; exit 91 ;;
esac'

    write_stub "${fixture}/bin/curl" '
[[ " $* " == *" --connect-timeout 5 "* && " $* " == *" --max-time 30 "* ]] || {
  echo "curl operation is missing fixed connect/total timeout" >&2
  exit 96
}
url="${!#}"
data_file=""
config_file=""
previous=""
for arg in "$@"; do
  if [[ "$previous" == "--data-binary" ]]; then data_file="${arg#@}"; fi
  if [[ "$previous" == "--config" ]]; then config_file="$arg"; fi
  previous="$arg"
done
if [[ -n "$config_file" ]]; then
  grep -Fx '"'"'header = "Authorization: Bearer psy_task9-stub-api-key"'"'"' "$config_file" >/dev/null || exit 86
  if grep -F '"'"'psy_unused'"'"' "$config_file" >/dev/null; then exit 86; fi
fi
case "$url" in
  */v1/health)
    if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "health" ]]; then
      printf "%s\n" "{\"status\":\"degraded\",\"version\":\"0.1.0\"}"
    else
      printf "%s\n" "{\"status\":\"ok\",\"version\":\"0.1.0\"}"
    fi
    ;;
  */v1/remember)
    [[ -n "$data_file" && -f "$data_file" ]] || exit 87
    jq -e '"'"'.entity | test("^task9-runtime-1234-2-[0-9a-f]{16}$")'"'"' "$data_file" >/dev/null || exit 87
    if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "remember-ambiguous" ]]; then
      printf "%s\n" "{\"id\":\"11111111-1111-1111-1111-111111111111\"}"
      exit 83
    fi
    [[ "${STUB_FUNCTIONAL_MODE:-exact}" != "remember" ]] || exit 83
    remember_calls=$(grep -F -c $'"'"'ARG\thttps://mcp.pensyve.com/v1/remember'"'"' "$STUB_LOG" || true)
    if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "after-first-remember" && "$remember_calls" -ge 2 ]]; then /bin/sleep 8; fi
    [[ "${STUB_FUNCTIONAL_MODE:-exact}" != "remember-two" || "$remember_calls" -lt 2 ]] || exit 83
    if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "remember-response" && "$remember_calls" -ge 2 ]]; then
      printf "%s\n" "{}"
    elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "remember-duplicate" || "$remember_calls" -eq 1 ]]; then
      printf "%s\n" "{\"id\":\"11111111-1111-1111-1111-111111111111\",\"content\":\"The unrelated decoy describes a sourdough recipe using rye flour and warm water.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"extraction_tier\":1}"
    elif [[ "$remember_calls" -eq 2 ]]; then
      printf "%s\n" "{\"id\":\"22222222-2222-2222-2222-222222222222\",\"content\":\"The production reranker proof explicitly marks codename ORCHID as the selected result.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"extraction_tier\":1}"
    else
      printf "%s\n" "{\"id\":\"33333333-3333-3333-3333-333333333333\",\"content\":\"A neutral third record mentions astronomy and the rings of Saturn.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"extraction_tier\":1}"
    fi
    ;;
  */v1/recall)
    [[ -n "$data_file" && -f "$data_file" ]] || exit 88
    jq -e '"'"'.query == "Which codename is explicitly marked as the selected result by the production reranker proof?" and .limit == 3 and (.entity | test("^task9-runtime-1234-2-[0-9a-f]{16}$"))'"'"' "$data_file" >/dev/null || exit 88
    [[ "${STUB_FUNCTIONAL_MODE:-exact}" != "recall" && "${STUB_FUNCTIONAL_MODE:-exact}" != "recall-and-delete" ]] || exit 84
    if [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "recall-count" ]]; then
      printf "%s\n" "{\"memories\":[{\"id\":\"22222222-2222-2222-2222-222222222222\",\"content\":\"The production reranker proof explicitly marks codename ORCHID as the selected result.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":0.91}],\"contradictions\":[]}"
    elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "recall-wrong-ids" ]]; then
      printf "%s\n" "{\"memories\":[{\"id\":\"44444444-4444-4444-4444-444444444444\",\"content\":\"wrong\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":0.91},{\"id\":\"55555555-5555-5555-5555-555555555555\",\"content\":\"wrong\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":0.08},{\"id\":\"66666666-6666-6666-6666-666666666666\",\"content\":\"wrong\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":0.01}],\"contradictions\":[]}"
    elif [[ "${STUB_FUNCTIONAL_MODE:-exact}" == "unreranked" ]]; then
      printf "%s\n" "{\"memories\":[{\"id\":\"22222222-2222-2222-2222-222222222222\",\"content\":\"The production reranker proof explicitly marks codename ORCHID as the selected result.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":0.90},{\"id\":\"11111111-1111-1111-1111-111111111111\",\"content\":\"The unrelated decoy describes a sourdough recipe using rye flour and warm water.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":0.53},{\"id\":\"33333333-3333-3333-3333-333333333333\",\"content\":\"A neutral third record mentions astronomy and the rings of Saturn.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":0.51}],\"contradictions\":[]}"
    else
      printf "%s\n" "{\"memories\":[{\"id\":\"22222222-2222-2222-2222-222222222222\",\"content\":\"The production reranker proof explicitly marks codename ORCHID as the selected result.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":10.24},{\"id\":\"33333333-3333-3333-3333-333333333333\",\"content\":\"A neutral third record mentions astronomy and the rings of Saturn.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":-10.19},{\"id\":\"11111111-1111-1111-1111-111111111111\",\"content\":\"The unrelated decoy describes a sourdough recipe using rye flour and warm water.\",\"memory_type\":\"semantic\",\"confidence\":1.0,\"stability\":1.0,\"score\":-10.20}],\"contradictions\":[]}"
    fi
    ;;
  */v1/entities/*)
    [[ "${STUB_FUNCTIONAL_MODE:-exact}" != "delete" && "${STUB_FUNCTIONAL_MODE:-exact}" != "recall-and-delete" ]] || exit 85
    case "${STUB_FUNCTIONAL_MODE:-exact}" in
      forget-count-1) printf "%s\n" "{\"forgotten_count\":1}" ;;
      forget-count-4) printf "%s\n" "{\"forgotten_count\":4}" ;;
      *) printf "%s\n" "{\"forgotten_count\":3}" ;;
    esac
    ;;
  *) echo "unexpected curl URL: $url" >&2; exit 81 ;;
esac'
    write_stub "${fixture}/bin/sleep" 'exit 0'
    export SLEEP_BIN="${fixture}/bin/sleep"

    STUB_FUNCTIONAL_MODE=exact \
      DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
        "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    [[ "$(call_count "${STUB_LOG}" docker load)" -eq 1 ]] || fail "promotion must load exactly once"
    [[ "$(call_count "${STUB_LOG}" docker tag)" -eq 1 ]] || fail "promotion must tag exactly once"
    [[ "$(call_count "${STUB_LOG}" docker push)" -eq 1 ]] || fail "promotion must push exactly once"
    [[ "$(call_count "${STUB_LOG}" aws batch-get-image)" -eq 1 ]] || fail "promotion must batch-get exact manifest once"
    [[ "$(call_count "${STUB_LOG}" aws register-task-definition)" -eq 1 ]] || fail "promotion must register exactly once"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] || fail "promotion must update exactly once"
    [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 1 ]] || fail "promotion must wait exactly once for candidate stability"
    [[ "$(call_count "${STUB_LOG}" aws describe-services)" -eq 4 ]] || fail "promotion must verify Task 8 twice and the exact candidate service before and after recall"
    [[ "$(call_count "${STUB_LOG}" aws list-tasks)" -eq 2 ]] || fail "promotion must list candidate service tasks before and after recall"
    [[ "$(call_count "${STUB_LOG}" aws describe-tasks)" -eq 2 ]] || fail "promotion must describe candidate tasks before and after recall"
    [[ "$(call_count "${STUB_LOG}" aws describe-target-health)" -eq 2 ]] || fail "functional proof must describe exact target health before and after recall"
    [[ "$(call_count "${STUB_LOG}" aws get-log-events)" -eq 4 ]] || fail "functional proof must fetch one bounded startup and one bounded post-recall window per task stream"
    [[ "$(call_count "${STUB_LOG}" aws get-parameter)" -eq 1 ]] || fail "functional proof must fetch the exact Task 8 API key once"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/health' "${STUB_LOG}")" -eq 1 ]] || fail "functional proof health cardinality mismatch"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/remember' "${STUB_LOG}")" -eq 3 ]] || fail "functional proof remember cardinality mismatch"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/recall' "${STUB_LOG}")" -eq 1 ]] || fail "functional proof recall cardinality mismatch"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] || fail "functional proof cleanup cardinality mismatch"
    if grep -F 'task9-stub-api-key' "${STUB_LOG}" >/dev/null; then fail "functional proof leaked API key into argv evidence"; fi
    if grep -E 'ORCHID|sourdough|Which codename' "${STUB_LOG}" >/dev/null; then
        fail "functional proof leaked synthetic facts or query into argv evidence"
    fi
    [[ "$(grep -Fxc $'ARG\t--service-name' "${STUB_LOG}")" -eq 2 ]] || fail "candidate task lists must each bind one service name"
    [[ "$(grep -Fxc $'ARG\t--desired-status' "${STUB_LOG}")" -eq 2 ]] || fail "candidate task lists must each bind one desired status"
    [[ "$(grep -Fxc $'ARG\tRUNNING' "${STUB_LOG}")" -eq 2 ]] || fail "candidate task lists must each select RUNNING"
    [[ "$(grep -Fxc $'ARG\tarn:aws:ecs:us-east-2:123456789012:task/pensyve-prod/11111111111111111111111111111111' "${STUB_LOG}")" -eq 2 ]] \
        || fail "candidate task describe lost first exact task ARN"
    [[ "$(grep -Fxc $'ARG\tarn:aws:ecs:us-east-2:123456789012:task/pensyve-prod/22222222222222222222222222222222' "${STUB_LOG}")" -eq 2 ]] \
        || fail "candidate task describe lost second exact task ARN"
    if grep -F $'ARG\t--desired-count' "${STUB_LOG}" >/dev/null; then fail "promotion overrode desired count"; fi
    if grep -F $'ARG\tlatest' "${STUB_LOG}" >/dev/null; then fail "promotion selected latest"; fi
    if grep -F ':157' "${STUB_LOG}" >/dev/null; then fail "promotion selected rejected :157"; fi

    : > "${STUB_LOG}"
    STUB_FUNCTIONAL_MODE=startup-lag DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    [[ "$(call_count "${STUB_LOG}" aws get-log-events)" -eq 6 ]] \
      || fail "functional proof did not tolerate one bounded CloudWatch ingestion-lag poll"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] \
      || fail "ingestion-lag success did not clean the synthetic entity exactly once"

    : > "${STUB_LOG}"
    STUB_FUNCTIONAL_MODE=pagination DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    [[ "$(call_count "${STUB_LOG}" aws get-log-events)" -eq 8 ]] \
      || fail "functional proof did not fetch exactly two bounded pages per task/window"
    [[ "$(grep -F -c $'ARG\t--next-token' "${STUB_LOG}")" -eq 4 ]] \
      || fail "functional proof did not bind the exact continuation token once per task/window"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] \
      || fail "paginated success did not clean the synthetic entity exactly once"

    : > "${STUB_LOG}"
    local exit_log="${fixture}/rollback-normal-exit.log"
    local exit_mutation="${fixture}/promote-normal-exit.sh"
    cp -- "${PROMOTE_SCRIPT}" "${exit_mutation}"
    python3 - "${exit_mutation}" <<'PY'
import sys
from pathlib import Path
p = Path(sys.argv[1])
s = p.read_text()
target = '    --task-definition "${new_arn}" > "${TEMP_ROOT}/update-response.json"\naws_call ecs wait services-stable'
if s.count(target) != 1:
    raise SystemExit("hard target lookup failed for uncommitted normal exit")
p.write_text(s.replace(target, '    --task-definition "${new_arn}" > "${TEMP_ROOT}/update-response.json"\nexit 0\naws_call ecs wait services-stable', 1))
PY
    chmod +x "${exit_mutation}"
    capture_failure "${exit_log}" env DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" \
      CURL_BIN="${fixture}/bin/curl" "${exit_mutation}" "${fixture}/verified-image.json"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
      || fail "normal uncommitted EXIT did not perform exactly one candidate update and one rollback"
    [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 1 ]] \
      || fail "normal uncommitted EXIT did not run exactly one rollback waiter"
    grep -F "rollback verified exact Task 8 baseline" "${exit_log}" >/dev/null \
      || fail "normal uncommitted EXIT did not preserve verified rollback evidence"

    local signal_mode signal_log signal_status
    for signal_mode in TERM INT; do
        : > "${STUB_LOG}"
        signal_log="${fixture}/rollback-${signal_mode}.log"
        set +e
        setsid timeout --preserve-status --kill-after=3s --signal="${signal_mode}" 2s \
          env STUB_HANG_CANDIDATE_WAIT=1 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" \
          CURL_BIN="${fixture}/bin/curl" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json" \
          > "${signal_log}" 2>&1
        signal_status=$?
        set -e
        [[ "${signal_status}" -ne 0 ]] || fail "${signal_mode} cancellation returned success"
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -ge 1 ]] || fail "signal fixture never observed candidate update"
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
          || fail "${signal_mode} cancellation did not perform exactly one candidate update and one rollback"
        [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 2 ]] \
          || fail "${signal_mode} cancellation did not run candidate and rollback waiters exactly once"
        grep -F "rollback verified exact Task 8 baseline" "${signal_log}" >/dev/null \
          || fail "${signal_mode} cancellation did not preserve verified rollback evidence"
    done

    : > "${STUB_LOG}"
    local toctou_log="${fixture}/preupdate-drift.log"
    capture_failure "${toctou_log}" env STUB_PREUPDATE_DRIFT=1 \
      DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    grep -F "pre-update Task 8 drift" "${toctou_log}" >/dev/null || fail "pre-update baseline drift was not explicit"
    [[ "$(call_count "${STUB_LOG}" aws describe-services)" -eq 2 ]] || fail "pre-update drift must recheck the baseline exactly at the mutation boundary"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 0 ]] || fail "pre-update Task 8 drift overwrote a concurrent deployment"
    [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 0 ]] || fail "pre-update Task 8 drift entered candidate/rollback waiters"

    local snapshot_phase snapshot_field snapshot_log
    for snapshot_phase in initial preupdate rollback; do
        for snapshot_field in network load-balancer circuit-breaker deployment-config health-grace; do
            : > "${STUB_LOG}"
            snapshot_log="${fixture}/service-${snapshot_phase}-${snapshot_field}.log"
            if [[ "${snapshot_phase}" == rollback ]]; then
                capture_failure "${snapshot_log}" env STUB_FAIL_CANDIDATE_WAIT=1 \
                  STUB_SERVICE_DRIFT_PHASE="${snapshot_phase}" STUB_SERVICE_DRIFT_FIELD="${snapshot_field}" \
                  DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
                  "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
                [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
                  || fail "rollback ${snapshot_field} drift did not attempt exactly one candidate and one rollback update"
                grep -F "rollback describe-back verification failed" "${snapshot_log}" >/dev/null \
                  || fail "rollback ${snapshot_field} drift was not explicit"
            else
                capture_failure "${snapshot_log}" env \
                  STUB_SERVICE_DRIFT_PHASE="${snapshot_phase}" STUB_SERVICE_DRIFT_FIELD="${snapshot_field}" \
                  DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
                  "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
                [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 0 ]] \
                  || fail "${snapshot_phase} ${snapshot_field} drift reached candidate update authority"
                grep -F "canonical service snapshot drift" "${snapshot_log}" >/dev/null \
                  || fail "${snapshot_phase} ${snapshot_field} drift was not explicit"
            fi
            if grep -F ':157' "${STUB_LOG}" >/dev/null; then fail "${snapshot_phase} ${snapshot_field} drift selected rejected :157"; fi
        done
    done

    local functional_mode functional_log
    local functional_modes="target-health startup startup-duplicate startup-asymmetric duplicate-delayed stale-event pagination-cycle secret invalid-secret health remember remember-ambiguous remember-two remember-response remember-duplicate recall recall-and-delete recall-count recall-wrong-ids unreranked delete forget-count-1 forget-count-4 fallback fallback-delayed duplicate-stream wrong-task-id"
    for functional_mode in ${functional_modes}; do
        : > "${STUB_LOG}"
        functional_log="${fixture}/functional-${functional_mode}.log"
        capture_failure "${functional_log}" env STUB_FUNCTIONAL_MODE="${functional_mode}" \
          DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
          "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
        grep -F "candidate functional runtime verification failed" "${functional_log}" >/dev/null \
          || fail "functional ${functional_mode} failure was not explicit"
        grep -F "rollback verified exact Task 8 baseline" "${functional_log}" >/dev/null \
          || fail "functional ${functional_mode} failure did not prove rollback"
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
          || fail "functional ${functional_mode} failure must update candidate once and rollback once"
        [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 2 ]] \
          || fail "functional ${functional_mode} failure must wait once for candidate and rollback"
        case "${functional_mode}" in
          remember | remember-ambiguous | remember-two | remember-response | remember-duplicate | recall | recall-and-delete | recall-count | recall-wrong-ids | unreranked | delete | forget-count-1 | forget-count-4 | fallback | fallback-delayed | duplicate-delayed)
            [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] \
              || fail "functional ${functional_mode} failure did not clean the created synthetic entity exactly once"
            ;;
          target-health | startup | startup-duplicate | startup-asymmetric | stale-event | secret | invalid-secret | health)
            [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}" || true)" -eq 0 ]] \
              || fail "functional ${functional_mode} failure cleaned an entity before any memory was created"
            ;;
        esac
        if [[ "${functional_mode}" == recall-and-delete ]]; then
            grep -F "candidate controlled recall failed; candidate controlled cleanup request failed" "${functional_log}" >/dev/null \
              || fail "combined recall/cleanup failure did not preserve both errors"
        fi
        if grep -F ':157' "${STUB_LOG}" >/dev/null; then fail "functional ${functional_mode} selected rejected :157"; fi
    done

    local log_config_mode log_config_log
    for log_config_mode in absent wrong-container wrong-prefix wrong-region wrong-group; do
        : > "${STUB_LOG}"
        log_config_log="${fixture}/functional-awslogs-${log_config_mode}.log"
        capture_failure "${log_config_log}" env STUB_LOG_CONFIG_MODE="${log_config_mode}" \
          DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
          "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
        grep -E 'candidate reviewed awslogs|candidate functional runtime verification failed|gateway promotion error:' "${log_config_log}" >/dev/null \
          || fail "awslogs ${log_config_mode} failure was not explicit"
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
          || fail "awslogs ${log_config_mode} must perform one candidate update and one rollback"
        [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 2 ]] \
          || fail "awslogs ${log_config_mode} waiter cardinality mismatch"
        grep -F "rollback verified exact Task 8 baseline" "${log_config_log}" >/dev/null \
          || fail "awslogs ${log_config_mode} did not prove rollback"
        if grep -F ':157' "${STUB_LOG}" >/dev/null; then fail "awslogs ${log_config_mode} selected rejected :157"; fi
    done

    # Independent production custody: these fixtures start from durable live
    # state and never consume producer-local files or step outputs.
    : > "${STUB_LOG}"
    local finalizer_log="${fixture}/finalizer-success.log"
    if ! env PROMOTION_RESULT=success STUB_FINALIZER_STATE=candidate \
      AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json" > "${finalizer_log}" 2>&1; then
        sed -n '1,160p' "${finalizer_log}" >&2
        fail "nominal-success durable finalizer failed"
    fi
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 0 ]] \
      || fail "successful exact candidate finalizer performed a rollback"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] \
      || fail "successful finalizer did not forget the sealed entity exactly once"
    grep -F 'cleanup verified exact forgotten_count=3' "${finalizer_log}" >/dev/null \
      || fail "successful finalizer did not prove exact cleanup count"

    : > "${STUB_LOG}"
    finalizer_log="${fixture}/finalizer-after-candidate-update.log"
    env PROMOTION_RESULT=failure STUB_FINALIZER_STATE=candidate \
      AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json" > "${finalizer_log}" 2>&1
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 &&
       "$(call_count "${STUB_LOG}" aws wait)" -eq 1 ]] \
      || fail "after-candidate-update finalizer did not perform exactly one rollback update/wait"
    grep -F 'rollback verified exact Task 8 baseline' "${finalizer_log}" >/dev/null \
      || fail "after-candidate-update finalizer lost describe-back proof"

    : > "${STUB_LOG}"
    finalizer_log="${fixture}/finalizer-after-first-remember.log"
    env PROMOTION_RESULT=cancelled STUB_FINALIZER_STATE=candidate STUB_FUNCTIONAL_MODE=forget-count-1 \
      AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json" > "${finalizer_log}" 2>&1
    grep -F 'cleanup verified exact forgotten_count=1' "${finalizer_log}" >/dev/null \
      || fail "after-first-remember finalizer did not bind the exact interrupted cleanup count"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] \
      || fail "after-first-remember finalizer rollback cardinality mismatch"

    : > "${STUB_LOG}"
    finalizer_log="${fixture}/finalizer-success-cleanup-failure.log"
    capture_failure "${finalizer_log}" env PROMOTION_RESULT=success STUB_FINALIZER_STATE=candidate \
      STUB_FUNCTIONAL_MODE=forget-count-1 AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json"
    grep -F 'successful producer failed exact Task 9 cleanup; rollback required' "${finalizer_log}" >/dev/null \
      || fail "nominal success cleanup failure did not trigger Task 8 rollback"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 &&
       "$(call_count "${STUB_LOG}" aws wait)" -eq 1 ]] \
      || fail "nominal success cleanup failure rollback cardinality mismatch"

    : > "${STUB_LOG}"
    finalizer_log="${fixture}/finalizer-success-state-drift.log"
    capture_failure "${finalizer_log}" env PROMOTION_RESULT=success STUB_FINALIZER_STATE=candidate \
      STUB_CANDIDATE_MODE=service-count AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json"
    grep -F 'success describe-back failed' "${finalizer_log}" >/dev/null \
      || fail "nominal success final service drift was not explicit"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] \
      || fail "nominal success final state drift did not roll back exactly once"

    : > "${STUB_LOG}"
    finalizer_log="${fixture}/finalizer-cleanup-final-state-drift.log"
    capture_failure "${finalizer_log}" env PROMOTION_RESULT=success STUB_FINALIZER_STATE=candidate \
      STUB_FUNCTIONAL_MODE=cleanup-final-state-drift AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json"
    grep -F 'cleanup-final-state-drift:' "${finalizer_log}" >/dev/null \
      || fail "DELETE-time candidate drift was not explicit"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 &&
       "$(call_count "${STUB_LOG}" aws wait)" -eq 1 ]] \
      || fail "DELETE-time drift did not perform exact-one Task 8 rollback and wait"
    [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] \
      || fail "DELETE-time drift cleanup cardinality is not exactly one"

    : > "${STUB_LOG}"
    finalizer_log="${fixture}/finalizer-during-rollback-wait.log"
    capture_failure "${finalizer_log}" env PROMOTION_RESULT=cancelled STUB_FINALIZER_STATE=candidate \
      STUB_FAIL_FINALIZER_ROLLBACK_WAIT=1 AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 &&
       "$(call_count "${STUB_LOG}" aws wait)" -eq 1 ]] \
      || fail "during-rollback-wait failure retried or skipped rollback authority"
    grep -F 'rollback failed' "${finalizer_log}" >/dev/null \
      || fail "during-rollback-wait failure was not preserved"

    # Producer job-timeout and INT->TERM->kill escalation leave exactly one
    # candidate update; the later finalizer owns the only rollback and cleanup.
    local boundary boundary_signal signal_status boundary_log
    for boundary in job-timeout INT TERM; do
        : > "${STUB_LOG}"
        boundary_log="${fixture}/producer-${boundary}.log"
        set +e
        boundary_signal="${boundary}"
        [[ "${boundary}" != job-timeout ]] || boundary_signal=TERM
        setsid timeout --preserve-status --kill-after=2s --signal="${boundary_signal}" 2s \
          env PROMOTION_CUSTODY=deferred STUB_HANG_CANDIDATE_WAIT=1 \
          DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
          "${PROMOTE_SCRIPT}" promote "${fixture}/verified-image.json" > "${boundary_log}" 2>&1
        signal_status=$?
        set -e
        [[ "${signal_status}" -ne 0 && "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] \
          || fail "${boundary} producer boundary did not leave one candidate update"
        env PROMOTION_RESULT=cancelled STUB_FINALIZER_STATE=candidate \
          AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
          "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json" >> "${boundary_log}" 2>&1
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
          || fail "${boundary} custody did not add exactly one finalizer rollback"
        [[ "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] \
          || fail "${boundary} custody cleanup cardinality mismatch"
    done

    : > "${STUB_LOG}"
    boundary_log="${fixture}/producer-INT-to-TERM-to-kill.log"
    local escalation_log="${fixture}/producer-INT-to-TERM-to-kill.signals" producer_pid producer_status
    local caller_sid caller_pgid producer_sid producer_pgid
    : > "${escalation_log}"
    caller_sid="$(ps -o sid= -p "$$" | tr -d ' ')"
    caller_pgid="$(ps -o pgid= -p "$$" | tr -d ' ')"
    setsid env PROMOTION_CUSTODY=deferred STUB_HANG_CANDIDATE_WAIT=1 STUB_IGNORE_WAIT_SIGNALS=1 \
      DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" promote "${fixture}/verified-image.json" > "${boundary_log}" 2>&1 &
    producer_pid=$!
    producer_sid=""
    producer_pgid=""

    cleanup_started_producer() {
        if [[ "${producer_sid}" =~ ^[0-9]+$ && "${producer_pgid}" =~ ^[0-9]+$ &&
              "${producer_sid}" != "${caller_sid}" && "${producer_pgid}" != "${caller_pgid}" &&
              "${producer_pgid}" -gt 1 ]]; then
            kill -TERM -- "-${producer_pgid}" >/dev/null 2>&1 || true
            /bin/sleep 0.05
            kill -KILL -- "-${producer_pgid}" >/dev/null 2>&1 || true
        else
            kill -KILL "${producer_pid}" >/dev/null 2>&1 || true
        fi
        wait "${producer_pid}" 2>/dev/null || true
    }

    for _ in $(seq 1 20); do
        producer_sid="$(ps -o sid= -p "${producer_pid}" 2>/dev/null | tr -d ' ' || true)"
        producer_pgid="$(ps -o pgid= -p "${producer_pid}" 2>/dev/null | tr -d ' ' || true)"
        [[ "${producer_sid}" =~ ^[0-9]+$ && "${producer_pgid}" =~ ^[0-9]+$ ]] && break
        /bin/sleep 0.01
    done
    [[ "${caller_sid}" =~ ^[0-9]+$ && "${caller_pgid}" =~ ^[0-9]+$ &&
       "${producer_sid}" =~ ^[0-9]+$ && "${producer_pgid}" =~ ^[0-9]+$ ]] \
      || { cleanup_started_producer; fail "INT-to-TERM-to-kill session identity was not numeric"; }
    [[ "${producer_pid}" != "$$" && "${producer_sid}" == "${producer_pid}" &&
       "${producer_pgid}" == "${producer_pid}" && "${producer_sid}" != "${caller_sid}" &&
       "${producer_pgid}" != "${caller_pgid}" ]] \
      || { cleanup_started_producer; fail "INT-to-TERM-to-kill producer did not own an isolated session/process group"; }

    signal_isolated_group() {
        local signal="$1" target_pgid="$2"
        [[ "${target_pgid}" =~ ^[0-9]+$ && "${target_pgid}" -gt 1 &&
           "${target_pgid}" != "${caller_pgid}" && "${target_pgid}" != "${caller_sid}" ]] \
          || fail "refusing unsafe INT-to-TERM-to-kill process-group target"
        kill -s "${signal}" -- "-${target_pgid}"
    }

    isolated_group_members() {
        local target_pgid="$1"
        ps -eo pid=,pgid= | awk -v group="${target_pgid}" '$2 == group {print $1}'
    }

    for _ in $(seq 1 40); do
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] && break
        /bin/sleep 0.05
    done
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] \
      || { cleanup_started_producer; fail "INT-to-TERM-to-kill never reached candidate update"; }
    for boundary_signal in INT TERM KILL; do
        if [[ -n "$(isolated_group_members "${producer_pgid}")" ]]; then
            printf '%s\tdelivered\n' "${boundary_signal}" >> "${escalation_log}"
            if ! signal_isolated_group "${boundary_signal}" "${producer_pgid}"; then
                cleanup_started_producer
                fail "INT-to-TERM-to-kill failed to signal the isolated process group"
            fi
            /bin/sleep 0.1
        else
            printf '%s\tgroup-already-empty\n' "${boundary_signal}" >> "${escalation_log}"
        fi
    done
    if [[ "$(grep -Fxc $'INT\tdelivered' "${escalation_log}")" -ne 1 ||
          "$(grep -Fxc $'TERM\tdelivered' "${escalation_log}")" -ne 1 ||
          "$(grep -Fxc $'KILL\tdelivered' "${escalation_log}")" -ne 1 ||
          "$(grep -c $'group-already-empty' "${escalation_log}" || true)" -ne 0 ]]; then
        cleanup_started_producer
        fail "INT-to-TERM-to-kill did not deliver exactly one INT, TERM, and KILL to the isolated process group"
    fi
    set +e
    wait "${producer_pid}"
    producer_status=$?
    set -e
    for _ in $(seq 1 40); do
        [[ -z "$(isolated_group_members "${producer_pgid}")" ]] && break
        /bin/sleep 0.05
    done
    if [[ -n "$(isolated_group_members "${producer_pgid}")" ]]; then
        cleanup_started_producer
        fail "INT-to-TERM-to-kill left a surviving isolated process-group member"
    fi
    [[ "${producer_status}" -ne 0 && "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] \
      || fail "INT-to-TERM-to-kill producer boundary did not preserve one candidate update"
    env PROMOTION_RESULT=cancelled STUB_FINALIZER_STATE=candidate \
      AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json" >> "${boundary_log}" 2>&1
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 &&
       "$(grep -F -c $'ARG\thttps://mcp.pensyve.com/v1/entities/' "${STUB_LOG}")" -eq 1 ]] \
      || fail "INT-to-TERM-to-kill finalizer update/delete cardinality mismatch"

    : > "${STUB_LOG}"
    boundary_log="${fixture}/producer-after-first-remember.log"
    set +e
    setsid timeout --preserve-status --kill-after=2s --signal=TERM 2s \
      env PROMOTION_CUSTODY=deferred STUB_FUNCTIONAL_MODE=after-first-remember \
      DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" promote "${fixture}/verified-image.json" > "${boundary_log}" 2>&1
    signal_status=$?
    set -e
    [[ "${signal_status}" -ne 0 && "$(call_count "${STUB_LOG}" aws update-service)" -eq 1 ]] \
      || fail "after-first-remember producer cancellation lost candidate state"
    env PROMOTION_RESULT=cancelled STUB_FINALIZER_STATE=candidate STUB_FUNCTIONAL_MODE=forget-count-1 \
      AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      "${PROMOTE_SCRIPT}" finalize "${fixture}/verified-image.json" >> "${boundary_log}" 2>&1
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
      || fail "after-first-remember custody rollback cardinality mismatch"
    grep -F 'cleanup verified exact forgotten_count=1' "${boundary_log}" >/dev/null \
      || fail "after-first-remember custody lost exact cleanup count"

    local mutation="${fixture}/mutation.json"
    mutate_json "${fixture}/verified-image.json" "${mutation}" '.github={run_id:1}'
    expect_failure "fixed promotion shape" env DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${mutation}"
    mutate_json "${fixture}/verified-image.json" "${mutation}" '.deployment.cluster="other"'
    expect_failure "Task 8 cluster" env DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${mutation}"
    mutate_json "${fixture}/verified-image.json" "${mutation}" '.deployment.desired_count=3'
    expect_failure "Task 8 counts" env DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${mutation}"
    mutate_json "${fixture}/verified-image.json" "${mutation}" '.deployment.baseline_environment_sha256="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "environment drift" env DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${mutation}"
    mutate_json "${fixture}/verified-image.json" "${mutation}" '.deployment.baseline_image="123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
    expect_failure "shape/image drift" env DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${mutation}"
    expect_failure "shape/image drift" env STUB_TASK_CPU=1024 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    printf '{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","mutation":true}\n' > "${fixture}/wrong-manifest.json"
    expect_failure "manifest bytes" env STUB_ECR_MANIFEST_PATH="${fixture}/wrong-manifest.json" DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    expect_failure "canonical image-only" env STUB_AFTER_DRIFT=1 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    mutate_json "${fixture}/verified-image.json" "${mutation}" '.deployment.baseline_task_definition_arn="arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:157"'
    expect_failure ":157" env DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${mutation}"

    local candidate_mode
    for candidate_mode in auto-rollback service-arn service-count service-state rollout list-count task-count task-arn image task-status; do
        : > "${STUB_LOG}"
        local candidate_failure_log="${fixture}/candidate-${candidate_mode}.log"
        capture_failure "${candidate_failure_log}" env STUB_CANDIDATE_MODE="${candidate_mode}" DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
        grep -F "candidate describe-back verification failed" "${candidate_failure_log}" >/dev/null \
            || fail "candidate ${candidate_mode} drift did not name describe-back failure"
        grep -F "rollback verified exact Task 8 baseline" "${candidate_failure_log}" >/dev/null \
            || fail "candidate ${candidate_mode} drift did not prove rollback"
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] \
            || fail "candidate ${candidate_mode} drift must update once and rollback exactly once"
        [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 2 ]] \
            || fail "candidate ${candidate_mode} drift must wait once for candidate and once for rollback"
        [[ "$(call_count "${STUB_LOG}" aws describe-services)" -eq 4 ]] \
            || fail "candidate ${candidate_mode} drift must describe initial, preupdate, candidate, and rollback service states"
        [[ "$(call_count "${STUB_LOG}" aws describe-task-definition)" -eq 4 ]] \
            || fail "candidate ${candidate_mode} drift must describe baseline twice, registered candidate, and rollback baseline"
        case "${candidate_mode}" in
            auto-rollback | service-arn | service-count | service-state | rollout)
                [[ "$(call_count "${STUB_LOG}" aws list-tasks)" -eq 0 ]] || fail "candidate ${candidate_mode} must fail before task listing"
                [[ "$(call_count "${STUB_LOG}" aws describe-tasks)" -eq 0 ]] || fail "candidate ${candidate_mode} must fail before task describe"
                ;;
            list-count)
                [[ "$(call_count "${STUB_LOG}" aws list-tasks)" -eq 1 ]] || fail "candidate list-count must list exactly once"
                [[ "$(call_count "${STUB_LOG}" aws describe-tasks)" -eq 0 ]] || fail "candidate list-count must fail before task describe"
                ;;
            *)
                [[ "$(call_count "${STUB_LOG}" aws list-tasks)" -eq 1 ]] || fail "candidate ${candidate_mode} must list exactly once"
                [[ "$(call_count "${STUB_LOG}" aws describe-tasks)" -eq 1 ]] || fail "candidate ${candidate_mode} must describe tasks exactly once"
                ;;
        esac
        if grep -F ':157' "${STUB_LOG}" >/dev/null; then fail "candidate ${candidate_mode} drift selected rejected :157"; fi
    done

    : > "${STUB_LOG}"
    local failure_log="${fixture}/selected-failure.log"
    capture_failure "${failure_log}" env STUB_FAIL_CANDIDATE_WAIT=1 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
    grep -F "selected wait failure" "${failure_log}" >/dev/null || fail "selected failure evidence was lost"
    grep -F "rollback verified exact Task 8 baseline" "${failure_log}" >/dev/null || fail "selected failure did not prove rollback"
    [[ "$(call_count "${STUB_LOG}" docker load)" -eq 1 ]] || fail "selected failure must load exactly once"
    [[ "$(call_count "${STUB_LOG}" docker push)" -eq 1 ]] || fail "selected failure must push exactly once"
    [[ "$(call_count "${STUB_LOG}" aws register-task-definition)" -eq 1 ]] || fail "selected failure must register exactly once"
    [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] || fail "selected failure must update once and rollback exactly once"
    [[ "$(call_count "${STUB_LOG}" aws wait)" -eq 2 ]] || fail "selected failure must wait once for candidate and once for rollback"
    [[ "$(call_count "${STUB_LOG}" aws describe-services)" -eq 3 ]] || fail "selected failure must describe initial, preupdate, and rollback services"
    [[ "$(call_count "${STUB_LOG}" aws describe-task-definition)" -eq 4 ]] || fail "selected failure must describe exact Task 8 twice, candidate, and rollback"
    [[ "$(grep -F -c $'ARG\t'"${STUB_BASELINE_ARN}" "${STUB_LOG}")" -eq 4 ]] \
        || fail "selected failure must use exact Task 8 baseline ARN for both checks, rollback update, and describe-back"
    if grep -F ':157' "${STUB_LOG}" >/dev/null; then fail "selected failure rollback selected rejected :157"; fi

    local scenario expected expected_waits expected_services expected_tasks
    for scenario in update wait service-drift task-drift; do
        : > "${STUB_LOG}"
        failure_log="${fixture}/rollback-${scenario}.log"
        case "${scenario}" in
            update)
                capture_failure "${failure_log}" env STUB_FAIL_CANDIDATE_WAIT=1 STUB_FAIL_ROLLBACK_UPDATE=1 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
                expected="rollback update failed"
                expected_waits=1; expected_services=2; expected_tasks=3
                ;;
            wait)
                capture_failure "${failure_log}" env STUB_FAIL_CANDIDATE_WAIT=1 STUB_FAIL_ROLLBACK_WAIT=1 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
                expected="rollback wait failed"
                expected_waits=2; expected_services=2; expected_tasks=3
                ;;
            service-drift)
                capture_failure "${failure_log}" env STUB_FAIL_CANDIDATE_WAIT=1 STUB_ROLLBACK_SERVICE_DRIFT=1 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
                expected="rollback describe-back verification failed"
                expected_waits=2; expected_services=3; expected_tasks=3
                ;;
            task-drift)
                capture_failure "${failure_log}" env STUB_FAIL_CANDIDATE_WAIT=1 STUB_ROLLBACK_TASK_DRIFT=1 DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" "${PROMOTE_SCRIPT}" "${fixture}/verified-image.json"
                expected="rollback describe-back verification failed"
                expected_waits=2; expected_services=3; expected_tasks=4
                ;;
        esac
        grep -F "selected wait failure" "${failure_log}" >/dev/null || fail "${scenario} lost original candidate failure"
        grep -F "${expected}" "${failure_log}" >/dev/null || fail "${scenario} rollback failure was not explicit"
        [[ "$(call_count "${STUB_LOG}" aws update-service)" -eq 2 ]] || fail "${scenario} must attempt exactly one candidate update and one rollback update"
        [[ "$(call_count "${STUB_LOG}" aws wait)" -eq "${expected_waits}" ]] || fail "${scenario} waiter cardinality mismatch"
        [[ "$(call_count "${STUB_LOG}" aws describe-services)" -eq "${expected_services}" ]] || fail "${scenario} service describe cardinality mismatch"
        [[ "$(call_count "${STUB_LOG}" aws describe-task-definition)" -eq "${expected_tasks}" ]] || fail "${scenario} task describe cardinality mismatch"
        if grep -F ':157' "${STUB_LOG}" >/dev/null; then fail "${scenario} selected rejected :157"; fi
    done
    echo "promotion contract passed"
}

run_round4_review() {
    python3 - "${PROMOTE_SCRIPT}" "${ARTIFACT_SCRIPT}" "${RELEASE_SCRIPT}" \
      "${WORKFLOW}" "${CI_WORKFLOW}" "${REPO_ROOT}/pensyve-mcp-gateway/models/manifest.sha256" \
      "${SCRIPT_DIR}/fetch-model-bundle.sh" "${BASH_SOURCE[0]}" <<'PY'
import re
import sys
from pathlib import Path

promote, artifact, release, workflow, ci, manifest, fetch, tests = [Path(value).read_text() for value in sys.argv[1:]]
errors = []

def require(name, condition, detail):
    if not condition:
        errors.append(f"ROUND4-RED {name}: {detail}")

# AWS DescribeTasks has no containers[].logStreamName.  Runtime streams must
# come from the reviewed task-definition awslogs options plus exact task IDs.
require("awslogs-schema", "logStreamName" not in promote,
        "promotion still reads fictitious DescribeTasks containers[].logStreamName")
for token in ("logConfiguration", "awslogs-group", "awslogs-region", "awslogs-stream-prefix"):
    require("awslogs-schema", token in promote, f"missing reviewed task-definition {token} binding")
for token in ("task_id", "stream_prefix", "log_group", "log_region"):
    require("awslogs-task-stream-uniqueness", token in promote,
            f"missing exact task/container/prefix stream derivation token {token}")
for name, token in (
    ("awslogs-absent-config", "STUB_LOG_CONFIG_MODE"),
    ("awslogs-wrong-task", "wrong-task"),
    ("awslogs-wrong-container", "wrong-container"),
    ("awslogs-wrong-prefix", "wrong-prefix"),
    ("awslogs-duplicate-stream", "duplicate-stream"),
):
    require(name, token in tests, f"named executable mutation is absent: {token}")

# The calibrated actual gateway has a 3-memory full-order inversion.  Two
# fixture-only memories can pass with the reranker disabled.
require("actual-bge-inversion", "A neutral third record mentions astronomy and the rings of Saturn." in promote,
        "third calibrated actual-gateway memory is absent")
require("actual-bge-inversion", "limit:3" in promote.replace(" ", ""),
        "Task9 recall is not the calibrated three-result proof")
require("actual-bge-inversion", "bge_calibration" in promote and "unreranked_order" in promote,
        "immutable actual-gateway dense/reranked calibration is not bound")
require("actual-bge-bypass", "unreranked" in tests,
        "reranker-bypass mutation is absent")
require("post-recall-runtime-stability", promote.count("verify_candidate_deployment") >= 3,
        "no fresh service/tasks/image/health rejection check exists after recall")

# Rollback and synthetic cleanup must live in an independent always custody
# job rather than the mutation shell's EXIT trap and job-local temp state.
require("durable-finalizer", "artifact-custodian-finalize:" in workflow,
        "independent custodian finalizer job is absent")
require("durable-finalizer", "needs.artifact-custodian-producer.result" in workflow and "always()" in workflow,
        "custodian finalizer is not bound to the producer result under always()")
require("durable-finalizer", "finalize" in workflow and "promotion-custody" in workflow,
        "finalizer does not derive custody from the sealed handoff")
require("durable-finalizer", "PROMOTION_RESULT" in workflow,
        "finalizer cannot distinguish committed success from cancellation/failure")
require("durable-finalizer", "== finalize" in promote and "finalize_custody" in promote,
        "promotion script has no separately invocable finalizer mode")
finalizer_block = workflow[workflow.find("  artifact-custodian-finalize:"):workflow.find("  artifact-cleanup:")]
finalizer_credentials = finalizer_block.find("uses: aws-actions/configure-aws-credentials@")
for token in ("verified_image_sha256", "complete_tuple_sha256", "storage_approval_sha256", "probe_entity"):
    token_index = finalizer_block.find(token)
    require("finalizer-sealed-handoff-binding",
            token_index >= 0 and finalizer_credentials >= 0 and token_index < finalizer_credentials,
            f"finalizer does not re-bind sealed handoff {token} before credentials")
require("finalizer-source-tree-immutability",
        "cp -- /tmp/pensyve-promotion-custody/verified-image.json /tmp/pensyve-reviewed-artifact/verified-image.json" not in finalizer_block,
        "finalizer mutates the exact verified source tree before tuple replay")
for token in ("CURRENT_APPROVED_GB_HOURS", "CURRENT_APPROVED_DOLLARS",
              "CURRENT_PRICE_PER_GB_MONTH", "CURRENT_BILLING_UNIT"):
    token_index = finalizer_block.find(token)
    require("finalizer-storage-authority-recheck",
            token_index >= 0 and finalizer_credentials >= 0 and token_index < finalizer_credentials,
            f"finalizer does not revalidate current storage authority {token} before credentials")
for token in ("storage-precheck", "storage-reconcile", "repository storage variable drift"):
    token_index = finalizer_block.find(token)
    require("finalizer-storage-authority-recheck",
            token_index >= 0 and finalizer_credentials >= 0 and token_index < finalizer_credentials,
            f"finalizer does not replay sealed/REST storage contract before credentials: {token}")
for token in ("after-candidate-update", "after-first-remember", "during-rollback-wait", "job-timeout"):
    require("cancellation-mutations", token in tests,
            f"missing durable cancellation boundary {token}")

# Source evidence is sealed as an exact typed topology.  Valid internal HF
# snapshot symlinks must be materialized; arbitrary/special links must fail.
for token in ("sealed-tree.json", "lstat", "S_ISREG", "S_ISDIR", "S_ISLNK", "readlink"):
    require("type-aware-seal", token in artifact, f"type-aware seal token is absent: {token}")
for token in ("materialize-model-links", "symlink cycle", "escapes artifact root"):
    require("safe-model-materialization", token in artifact,
            f"safe HF symlink materialization guard is absent: {token}")
require("seal-roundtrip", "verify-tree" in artifact and "sealed-tree.json" in workflow,
        "upload/download exact-tree replay is absent")
for name, token in (
    ("seal-symlink-retarget", "symlink-retarget"),
    ("seal-special-entry", "special-entry"),
    ("seal-mode-drift", "mode-drift"),
    ("seal-directory-drift", "directory-drift"),
    ("seal-roundtrip-mutation", "roundtrip-tree"),
):
    require(name, token in tests, f"named executable mutation is absent: {token}")

# Precheck must see the final exact upload tree, including its seal/custody
# records, before paid upload.
build_block = workflow[workflow.find("  artifact-build:"):workflow.find("  artifact-promote-preflight:")]
preflight_block = workflow[workflow.find("  artifact-promote-preflight:"):workflow.find("  artifact-promote:")]
for name, block in (("source", build_block), ("handoff", preflight_block)):
    last_seal = block.rfind("seal-tree")
    last_precheck = block.rfind("gateway-image-artifact.sh storage-precheck")
    upload = block.find("uses: actions/upload-artifact@")
    require("complete-tree-precheck", min(last_seal, last_precheck, upload) >= 0 and last_seal < last_precheck < upload,
            f"{name} storage precheck does not follow the completed sealed tree before upload")
require("near-ceiling-no-upload", "near-ceiling" in workflow,
        "near-ceiling preupload refusal is not explicit")

# Handoff carries immutable storage authority and mutation revalidates it,
# including repository variable drift and the authoritative REST object.
for token in ("storage_approval", "approved_gb_hours_ceiling", "approved_dollar_ceiling",
              "price_per_gb_month", "organization_actions_artifact_bytes",
              "organization_packages_bytes", "rest_size_in_bytes"):
    require("sealed-handoff-storage", token in workflow,
            f"sealed handoff storage binding is absent: {token}")
require("repository-variable-drift", "repository storage variable drift" in workflow,
        "promotion does not reject current repository-variable drift")

# Every action in the production/artifact workflow is an immutable upstream
# commit.  Mutable major tags and branch refs are forbidden.
uses = re.findall(r"(?m)^\s*-?\s*uses:\s*([^\s#]+)", workflow)
mutable = [value for value in uses if not re.fullmatch(r"[^@\s]+@[0-9a-f]{40}", value)]
require("immutable-action-pins", not mutable,
        "mutable external action refs remain: " + ",".join(mutable))
for action, commit in {
    "actions/upload-artifact": "ea165f8d65b6e75b540449e92b4886f43607fa02",
    "aws-actions/configure-aws-credentials": "e6de054238d6b7531b4efff3b6587d9aade6a06c",
    "aws-actions/amazon-ecr-login": "03f1aad4c6c7ffd436567f42f9384779290529bd",
}.items():
    require("immutable-action-pins", all(value == f"{action}@{commit}" for value in uses if value.startswith(action + "@")),
            f"verified {action} pin drift")

require("cleanup-cost-summary", all(token in workflow for token in (
    "GITHUB_STEP_SUMMARY", "byte_hours", "gb_hours", "dollars", "created_at", "expires_at")),
        "pre-delete durable incurred-storage summary is incomplete")
require("pinned-python312", "actions/setup-python@" in workflow and "python-version: '3.12'" in workflow,
        "artifact source gates do not pin Python 3.12 with setup-python")
require("pinned-python312", "platform.python_version()" in workflow and "platform.machine()" in workflow,
        "PyYAML wheel interpreter/platform is not asserted")
require("pinned-registry", re.search(r"registry:2@sha256:[0-9a-f]{64}", artifact) is not None,
        "loopback registry still uses mutable registry:2")

# Authoritative model-license bytes are part of the same exact manifest/cache
# contract and image verification.
require("license-custody", "license-file" in manifest and "license-file" in fetch and
        "LICENSE.pensyve.txt" in manifest and "SPDX_LICENSE_REVISION" in fetch,
        "model license file/source custody is absent")
for license_hash in (
    "074e6e32c86a4c0ef8b3ed25b721ca23aca83df277cd88106ef7177c354615ff",
    "b05785f9f18e6716bab63424b11454513b9943a222595b70411009202fc592b5",
):
    require("license-custody", license_hash in manifest,
            f"authoritative model license SHA-256 is absent: {license_hash}")
require("release-license-custody", "license" in release.lower(),
        "release verifier does not prove baked license custody")

if errors:
    print("\n".join(errors), file=sys.stderr)
    raise SystemExit(1)
print("round4 source review contract passed")
PY
}

run_round5_custodian_stubs() {
    local fixture="${TEST_ROOT}/round5-custodian" bin="${TEST_ROOT}/round5-custodian/bin"
    mkdir -p "${bin}"
    python3 - "${WORKFLOW}" "${fixture}" <<'PY'
import sys
from pathlib import Path
import yaml

workflow_path, fixture = Path(sys.argv[1]), Path(sys.argv[2])
workflow = yaml.load(workflow_path.read_text(), Loader=yaml.BaseLoader)

def extract(job, name, output, replacements=()):
    matches = [step for step in workflow["jobs"][job]["steps"] if step.get("name") == name]
    if len(matches) != 1 or not matches[0].get("run"):
        raise SystemExit(f"round5 executable workflow step is absent: {job}/{name}")
    body = matches[0]["run"]
    for old, new in replacements:
        body = body.replace(old, new)
    (fixture / output).write_text("#!/usr/bin/env bash\n" + body)

def extract_sequence(job, names, output):
    bodies = []
    for name in names:
        matches = [step for step in workflow["jobs"][job]["steps"] if step.get("name") == name]
        if len(matches) != 1 or not matches[0].get("run"):
            raise SystemExit(f"round5 executable workflow step is absent: {job}/{name}")
        bodies.append(matches[0]["run"])
    (fixture / output).write_text("#!/usr/bin/env bash\n" + "\n".join(bodies))

extract("artifact-promote-dispatch", "Dispatch one exact-ref custodian and bind its exact run",
        "dispatch.sh", (("${{ inputs.mode }}", "artifact-promote"),
                        ("${{ inputs.pull_request_number }}", "16")))
extract("artifact-promote", "Observe exact custodian terminal result without production authority",
        "observe.sh")
extract_sequence("artifact-custodian-producer",
                 ("Bind inline custodian self and parent authority before checkout",
                  "Bind global custodian uniqueness after checkout"),
                 "producer-identity.sh")
extract_sequence("artifact-custodian-finalize",
                 ("Bind inline finalizer self and parent authority before checkout",
                  "Bind global finalizer uniqueness after checkout"),
                 "finalizer-identity.sh")
PY
    chmod +x "${fixture}"/*.sh
    export STUB_LOG="${fixture}/stub.log"
    export STUB_DISPATCH_JSON="${fixture}/captured-custodian-dispatch.json"
    export STUB_CUSTODIAN_SHA="${SOURCE_SHA}"
    export STUB_CUSTODIAN_REF="strict-local-runtime"
    write_stub "${bin}/sleep" 'exit 0'
    write_stub "${bin}/date" 'printf "2026-08-29T00:05:00Z\n"'
    write_stub "${bin}/gh" '
endpoint=""
input=""
previous=""
for argument in "$@"; do
  [[ "$argument" == repos/* ]] && endpoint="$argument"
  [[ "$previous" == --input ]] && input="$argument"
  previous="$argument"
done
if [[ " $* " == *" --method POST "* && "$endpoint" == *"/actions/workflows/deploy-gateway.yml/dispatches" ]]; then
  [[ -s "$input" ]] || exit 90
  cp -- "$input" "$STUB_DISPATCH_JSON"
  jq -e --arg ref "$STUB_CUSTODIAN_REF" ".ref == \$ref and .return_run_details == true and .inputs.mode == \"artifact-custodian\" and (.inputs.custody_lease_id | test(\"^[0-9a-f]{64}\$\")) and (.inputs.custody_lease_id as \$lease | (.inputs.custody_request | fromjson | .custody_lease_id) == \$lease)" "$input" >/dev/null
  request=$(jq -r ".inputs.custody_request" "$input")
  lease=$(jq -r ".inputs.custody_lease_id" "$input")
  computed=$(jq -S -c "del(.custody_lease_id)" <<<"$request" | tr -d "\n" | sha256sum | cut -d" " -f1)
  [[ "$computed" == "$lease" ]] || exit 92
  case "${STUB_DISPATCH_RESPONSE_MODE:-204}" in
    204) printf "HTTP/2.0 204 No Content\r\n\r\n" ;;
    200) printf "HTTP/2.0 200 OK\r\n\r\n{\"workflow_run_id\":9001,\"workflow_run_url\":\"https://api.github.test/runs/9001\"}\n" ;;
    wrong-id) printf "HTTP/2.0 200 OK\r\n\r\n{\"workflow_run_id\":9002}\n" ;;
    malformed) printf "HTTP/2.0 200 OK\r\n\r\nnot-json\n" ;;
  esac
  exit 0
fi
if [[ "$endpoint" == *"/actions/workflows/deploy-gateway.yml/runs" ]]; then
  lease=$(jq -r ".inputs.custody_lease_id" "$STUB_DISPATCH_JSON")
  title="gateway-custodian-${lease}"
  lookup_mode="${STUB_IDENTITY_MODE:-${STUB_DISCOVERY_MODE:-success}}"
  case "$lookup_mode" in
    zero) jq -n "[{total_count:0,workflow_runs:[]}]" ;;
    duplicate|duplicate-custodian-run|duplicate-page2|aged-replay|boundary-start|boundary-end)
      second_created=2026-08-29T00:05:02Z
      [[ "$lookup_mode" != aged-replay ]] || second_created=2026-09-15T00:00:00Z
      [[ "$lookup_mode" != boundary-start ]] || second_created=2026-08-28T23:55:00Z
      [[ "$lookup_mode" != boundary-end ]] || second_created=2026-09-27T00:00:00Z
      jq -n --arg title "$title" --arg sha "$STUB_CUSTODIAN_SHA" --arg ref "$STUB_CUSTODIAN_REF" \
        --arg second_created "$second_created" \
        "[{total_count:2,workflow_runs:[{id:9001,run_attempt:1,status:\"queued\",display_title:\$title,repository:{full_name:\"major7apps/pensyve\"},event:\"workflow_dispatch\",head_sha:\$sha,head_branch:\$ref,path:\".github/workflows/deploy-gateway.yml\",created_at:\"2026-08-29T00:05:01Z\"}]},{total_count:2,workflow_runs:[{id:9002,run_attempt:1,status:\"queued\",display_title:\$title,repository:{full_name:\"major7apps/pensyve\"},event:\"workflow_dispatch\",head_sha:\$sha,head_branch:\$ref,path:\".github/workflows/deploy-gateway.yml\",created_at:\$second_created}]}]" ;;
    malformed) jq -n "[{total_count:1,workflow_runs:{not:\"an array\"}}]" ;;
    page-cycle) jq -n --arg title "$title" --arg sha "$STUB_CUSTODIAN_SHA" --arg ref "$STUB_CUSTODIAN_REF" \
      "[{total_count:1,workflow_runs:[{id:9001,run_attempt:1,status:\"queued\",display_title:\$title,repository:{full_name:\"major7apps/pensyve\"},event:\"workflow_dispatch\",head_sha:\$sha,head_branch:\$ref,path:\".github/workflows/deploy-gateway.yml\",created_at:\"2026-08-29T00:05:01Z\"}]},{total_count:1,workflow_runs:[{id:9001,run_attempt:1,status:\"queued\",display_title:\$title,repository:{full_name:\"major7apps/pensyve\"},event:\"workflow_dispatch\",head_sha:\$sha,head_branch:\$ref,path:\".github/workflows/deploy-gateway.yml\",created_at:\"2026-08-29T00:05:01Z\"}]}]" ;;
    *)
      head="$STUB_CUSTODIAN_SHA"; ref="$STUB_CUSTODIAN_REF"; attempt=1; run=9001
      [[ "${STUB_DISCOVERY_MODE:-success}" != wrong-head ]] || head=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
      [[ "${STUB_DISCOVERY_MODE:-success}" != wrong-ref ]] || ref=wrong-ref
      [[ "${STUB_DISCOVERY_MODE:-success}" != wrong-attempt ]] || attempt=2
      jq -n --arg title "$title" --arg sha "$head" --arg ref "$ref" --argjson attempt "$attempt" --argjson run "$run" \
        "[{total_count:1,workflow_runs:[{id:\$run,run_attempt:\$attempt,status:\"queued\",display_title:\$title,repository:{full_name:\"major7apps/pensyve\"},event:\"workflow_dispatch\",head_sha:\$sha,head_branch:\$ref,path:\".github/workflows/deploy-gateway.yml\",created_at:\"2026-08-29T00:05:01Z\"}]}]" ;;
  esac
  exit 0
fi
if [[ "$endpoint" == *"/actions/runs/9001/jobs"* ]]; then
  count=1; [[ "${STUB_IDENTITY_MODE:-success}" != duplicate-parent-job ]] || count=2
  jq -n --argjson count "$count" "{jobs:[range(0;\$count)|{name:\"Dispatch the exact leased production custodian\",steps:[{name:\"Dispatch one exact-ref custodian and bind its exact run\",status:\"completed\",conclusion:\"success\"}]}]}"
  exit 0
fi
if [[ "$endpoint" == *"/actions/runs/1234/jobs"* ]]; then
  count=1; [[ "${STUB_IDENTITY_MODE:-success}" != duplicate-parent-job ]] || count=2
  jq -n --argjson count "$count" "{jobs:[range(0;\$count)|{name:\"Dispatch the exact leased production custodian\",steps:[{name:\"Dispatch one exact-ref custodian and bind its exact run\",status:\"completed\",conclusion:\"success\"}]}]}"
  exit 0
fi
if [[ "$endpoint" == *"/actions/runs/9001" ]]; then
  status=completed; conclusion=success; sha="$STUB_CUSTODIAN_SHA"; ref="$STUB_CUSTODIAN_REF"; attempt=1; title="gateway-custodian-${STUB_EXPECTED_LEASE}"; path=.github/workflows/deploy-gateway.yml
  case "${STUB_OBSERVER_MODE:-${STUB_IDENTITY_MODE:-success}}" in
    parent-cancel|pending-replaced) conclusion=cancelled ;;
    wrong-head) sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ;;
    wrong-ref) ref=wrong-ref ;;
    wrong-path) path=.github/workflows/other.yml ;;
    wrong-attempt) attempt=2 ;;
    wrong-lease) title=gateway-custodian-wrong ;;
  esac
  jq -n --arg sha "$sha" --arg ref "$ref" --arg path "$path" --arg title "$title" --arg status "$status" \
    --arg conclusion "$conclusion" --argjson attempt "$attempt" \
    "{id:9001,run_attempt:\$attempt,status:\$status,conclusion:\$conclusion,display_title:\$title,repository:{full_name:\"major7apps/pensyve\"},event:\"workflow_dispatch\",head_sha:\$sha,head_branch:\$ref,path:\$path,created_at:\"2026-08-29T00:05:01Z\"}"
  exit 0
fi
if [[ "$endpoint" == *"/actions/runs/1234" ]]; then
  [[ "${STUB_IDENTITY_MODE:-success}" != missing-parent ]] || exit 44
  sha="$STUB_CUSTODIAN_SHA"; ref="$STUB_CUSTODIAN_REF"; attempt=2
  [[ "${STUB_IDENTITY_MODE:-success}" != wrong-parent-head ]] || sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  [[ "${STUB_IDENTITY_MODE:-success}" != wrong-parent-ref ]] || ref=wrong-ref
  [[ "${STUB_IDENTITY_MODE:-success}" != wrong-parent-attempt ]] || attempt=3
  jq -n --arg sha "$sha" --arg ref "$ref" --argjson attempt "$attempt" \
    "{id:1234,run_attempt:\$attempt,repository:{full_name:\"major7apps/pensyve\"},event:\"workflow_dispatch\",path:\".github/workflows/deploy-gateway.yml\",head_sha:\$sha,head_branch:\$ref,created_at:\"2026-08-29T00:00:00Z\"}"
  exit 0
fi
echo "unexpected round5 gh argv: $*" >&2
exit 91'

    local handoff_digest="sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    local expected_lease=""
    run_dispatch() {
      : > "${STUB_LOG}"
      rm -f -- "${STUB_DISPATCH_JSON}" "${fixture}/dispatch.outputs"
      PATH="${bin}:${PATH}" RUNNER_TEMP="${fixture}" GITHUB_OUTPUT="${fixture}/dispatch.outputs" \
        GITHUB_EVENT_NAME=workflow_dispatch INPUT_MODE=artifact-promote PR_NUMBER=16 \
        GITHUB_REPOSITORY=major7apps/pensyve \
        GITHUB_REF_NAME="${STUB_CUSTODIAN_REF}" GITHUB_SHA="${SOURCE_SHA}" GITHUB_RUN_ID=1234 \
        GITHUB_RUN_ATTEMPT=2 HANDOFF_ID=888 \
        HANDOFF_NAME="gateway-handoff-1234-2-${SOURCE_SHA}" HANDOFF_DIGEST="${handoff_digest}" \
        HANDOFF_SIZE=4096 HANDOFF_CREATED_AT=2026-08-29T00:00:00Z HANDOFF_EXPIRES_AT=2026-09-28T00:00:00Z \
        SOURCE_ARTIFACT_EXPIRES_AT=2026-09-27T00:00:00Z \
        HANDOFF_REPOSITORY=major7apps/pensyve HANDOFF_RUN_ID=1234 HANDOFF_RUN_ATTEMPT=2 \
        HANDOFF_REVIEWED_SHA="${SOURCE_SHA}" REVIEWED_REF="${STUB_CUSTODIAN_REF}" bash "${fixture}/dispatch.sh"
    }
    STUB_DISCOVERY_MODE=success run_dispatch
    expected_lease=$(sed -n 's/^custody_lease_id=//p' "${fixture}/dispatch.outputs")
    [[ "$expected_lease" =~ ^[0-9a-f]{64}$ ]] || fail "dispatcher did not produce canonical sealed lease"
    export STUB_EXPECTED_LEASE="${expected_lease}"
    grep -Fx "custodian_run_id=9001" "${fixture}/dispatch.outputs" >/dev/null || fail "exact custodian discovery lost run ID"
    grep -Fx "custody_lease_id=${expected_lease}" "${fixture}/dispatch.outputs" >/dev/null || fail "exact custodian discovery lost lease"
    [[ "$(call_count "${STUB_LOG}" gh POST)" -eq 1 ]] || fail "custodian dispatch cardinality is not exactly one"
    [[ "$(call_count "${STUB_LOG}" gh event=workflow_dispatch)" -eq 1 ]] || fail "custodian discovery cardinality is not exactly one"
    STUB_DISCOVERY_MODE=success STUB_DISPATCH_RESPONSE_MODE=200 run_dispatch
    grep -Fx 'custodian_run_id=9001' "${fixture}/dispatch.outputs" >/dev/null \
      || fail "verified 200 dispatch run ID was not bound"
    STUB_DISCOVERY_MODE=success STUB_DISPATCH_RESPONSE_MODE=wrong-id \
      capture_failure "${fixture}/dispatch-200-wrong-id.log" run_dispatch
    STUB_DISCOVERY_MODE=success STUB_DISPATCH_RESPONSE_MODE=malformed \
      capture_failure "${fixture}/dispatch-200-malformed.log" run_dispatch
    local discovery_mode
    for discovery_mode in zero duplicate wrong-head wrong-ref wrong-attempt duplicate-page2 aged-replay malformed page-cycle boundary-start boundary-end; do
      STUB_DISCOVERY_MODE="${discovery_mode}" capture_failure "${fixture}/discovery-${discovery_mode}.log" run_dispatch
    done
    STUB_DISCOVERY_MODE=success run_dispatch
    local request
    request=$(jq -r '.inputs.custody_request' "${STUB_DISPATCH_JSON}")
    run_identity() {
      local program="$1"
      : > "${STUB_LOG}"
      PATH="${bin}:${PATH}" RUNNER_TEMP="${fixture}" GITHUB_REPOSITORY=major7apps/pensyve \
        GITHUB_REF_NAME="${STUB_CUSTODIAN_REF}" GITHUB_SHA="${SOURCE_SHA}" GITHUB_RUN_ID=9001 \
        GITHUB_RUN_ATTEMPT=1 CUSTODY_REQUEST="${request}" CUSTODY_LEASE_ID="${IDENTITY_LEASE_OVERRIDE:-$expected_lease}" \
        INPUT_PR_NUMBER=16 bash "${program}"
    }
    STUB_IDENTITY_MODE=success run_identity "${fixture}/producer-identity.sh"
    STUB_IDENTITY_MODE=success run_identity "${fixture}/finalizer-identity.sh"
    local identity_mode
    for identity_mode in missing-parent wrong-parent-head wrong-parent-ref wrong-parent-attempt duplicate-parent-job \
      duplicate-page2 aged-replay malformed page-cycle boundary-start boundary-end; do
      STUB_IDENTITY_MODE="${identity_mode}" capture_failure "${fixture}/identity-${identity_mode}.log" \
        run_identity "${fixture}/producer-identity.sh"
    done
    IDENTITY_LEASE_OVERRIDE=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
      capture_failure "${fixture}/identity-wrong-lease.log" run_identity "${fixture}/producer-identity.sh"
    run_observer() {
      : > "${STUB_LOG}"
      PATH="${bin}:${PATH}" RUNNER_TEMP="${fixture}" GITHUB_REPOSITORY=major7apps/pensyve \
        GITHUB_SHA="${SOURCE_SHA}" GITHUB_REF_NAME="${STUB_CUSTODIAN_REF}" \
        CUSTODIAN_RUN_ID=9001 CUSTODY_LEASE_ID="${expected_lease}" \
        bash "${fixture}/observe.sh"
    }
    STUB_OBSERVER_MODE=success run_observer
    STUB_OBSERVER_MODE=wrong-ref capture_failure "${fixture}/observer-wrong-ref.log" run_observer
    STUB_OBSERVER_MODE=wrong-path capture_failure "${fixture}/observer-wrong-path.log" run_observer
    STUB_OBSERVER_MODE=parent-cancel capture_failure "${fixture}/observer-parent-cancel.log" run_observer
    STUB_OBSERVER_MODE=pending-replaced capture_failure "${fixture}/observer-pending-replaced.log" run_observer
    [[ "$(call_count "${STUB_LOG}" gh repos/major7apps/pensyve/actions/runs/9001)" -eq 1 ]] \
      || fail "pending replacement observer did not bind one exact run"
    STUB_IDENTITY_MODE=duplicate-custodian-run capture_failure \
      "${fixture}/identity-duplicate-custodian-producer.log" run_identity "${fixture}/producer-identity.sh"
    STUB_IDENTITY_MODE=duplicate-custodian-run capture_failure \
      "${fixture}/identity-duplicate-custodian-finalizer.log" run_identity "${fixture}/finalizer-identity.sh"
    for identity_mode in aged-replay malformed page-cycle boundary-start boundary-end; do
      STUB_IDENTITY_MODE="${identity_mode}" capture_failure "${fixture}/finalizer-${identity_mode}.log" \
        run_identity "${fixture}/finalizer-identity.sh"
    done
    echo "round5 executable custodian dispatch/identity/cancel contract passed"
}

run_round5_review() {
    local failed=0 nested_root="${TEST_ROOT}/round5-nested-model"
    local blob="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    local repository="${nested_root}/evidence/release/exact-image-model-root/models--Example--nested"
    mkdir -p "${repository}/blobs" "${repository}/snapshots/0123456789abcdef0123456789abcdef01234567"
    printf 'nested model bytes\n' > "${repository}/blobs/${blob}"
    ln -s "../../blobs/${blob}" \
      "${repository}/snapshots/0123456789abcdef0123456789abcdef01234567/model.onnx"
    if ! "${ARTIFACT_SCRIPT}" materialize-model-links --root "${nested_root}" \
      > "${TEST_ROOT}/round5-nested-materialize.log" 2>&1; then
        echo "ROUND5-RED nested-model-materialization: real evidence/release/exact-image-model-root cache failed" >&2
        failed=1
    fi
    local nested_model="${repository}/snapshots/0123456789abcdef0123456789abcdef01234567/model.onnx"
    [[ -f "${nested_model}" && ! -L "${nested_model}" &&
       "$(stat -c '%a' "${nested_model}")" == "$(stat -c '%a' "${repository}/blobs/${blob}")" ]] \
      || fail "nested model link did not materialize to an exact regular mode-preserved file"
    local nested_zip="${TEST_ROOT}/round5-nested-model.zip" nested_roundtrip="${TEST_ROOT}/round5-nested-roundtrip"
    (cd "${nested_root}" && zip -q -r "${nested_zip}" .)
    mkdir -p "${nested_roundtrip}"
    unzip -q "${nested_zip}" -d "${nested_roundtrip}"
    cmp --silent "${nested_model}" \
      "${nested_roundtrip}/evidence/release/exact-image-model-root/models--Example--nested/snapshots/0123456789abcdef0123456789abcdef01234567/model.onnx" \
      || fail "nested materialized cache failed exact ZIP roundtrip"

    local adversary_root adversary_repo adversary_link adversary_blob
    adversary_blob="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    adversary_root="${TEST_ROOT}/round5-nearest-model"
    adversary_repo="${adversary_root}/models--Outer--repo/evidence/models--Inner--repo"
    mkdir -p "${adversary_root}/models--Outer--repo/blobs" \
      "${adversary_repo}/snapshots/0123456789abcdef0123456789abcdef01234567"
    printf 'wrong repository blob\n' > "${adversary_root}/models--Outer--repo/blobs/${adversary_blob}"
    adversary_link="${adversary_repo}/snapshots/0123456789abcdef0123456789abcdef01234567/model.onnx"
    ln -s "../../../../blobs/${adversary_blob}" "${adversary_link}"
    expect_failure "exact internal blob" "${ARTIFACT_SCRIPT}" materialize-model-links --root "${adversary_root}"

    adversary_root="${TEST_ROOT}/round5-retarget-model"
    adversary_repo="${adversary_root}/nested/models--Example--retarget"
    mkdir -p "${adversary_repo}/blobs" \
      "${adversary_repo}/snapshots/0123456789abcdef0123456789abcdef01234567"
    printf 'alternate\n' > "${adversary_repo}/alternate"
    ln -s ../alternate "${adversary_repo}/blobs/${adversary_blob}"
    ln -s "../../blobs/${adversary_blob}" \
      "${adversary_repo}/snapshots/0123456789abcdef0123456789abcdef01234567/model.onnx"
    expect_failure "unsafe model snapshot symlink shape" "${ARTIFACT_SCRIPT}" materialize-model-links --root "${adversary_root}"

    adversary_root="${TEST_ROOT}/round5-special-model"
    adversary_repo="${adversary_root}/deep/models--Example--special"
    mkdir -p "${adversary_repo}/blobs" \
      "${adversary_repo}/snapshots/0123456789abcdef0123456789abcdef01234567"
    mkfifo "${adversary_repo}/blobs/${adversary_blob}"
    ln -s "../../blobs/${adversary_blob}" \
      "${adversary_repo}/snapshots/0123456789abcdef0123456789abcdef01234567/model.onnx"
    expect_failure "not a regular file" "${ARTIFACT_SCRIPT}" materialize-model-links --root "${adversary_root}"

    run_round5_custodian_stubs

    if ! python3 - "${ARTIFACT_SCRIPT}" "${PROMOTE_SCRIPT}" "${WORKFLOW}" "${BASH_SOURCE[0]}" <<'PY'
import sys
from pathlib import Path

artifact, promote, workflow, tests = [Path(value).read_text() for value in sys.argv[1:]]
errors = []

def require(name, condition, detail):
    if not condition:
        errors.append(f"ROUND5-RED {name}: {detail}")

require("nested-model-materialization", "snapshot_index != 1" not in artifact,
        "materializer still assumes models--* is the artifact-root child")
require("lost-source-upload-response", "source-upload-name-lookup" in workflow,
        "source resolver does not authenticate exact-name/current-run REST discovery after ambiguous upload")
require("lost-handoff-upload-response", "handoff-upload-name-lookup" in workflow,
        "handoff resolver does not authenticate exact-name/current-run REST discovery after ambiguous upload")

for token in ("artifact-custodian", "custody_lease_id", "custody_request"):
    require("external-custodian", token in workflow,
            f"separately dispatched durable custodian contract is absent: {token}")
require("external-custodian-readiness", "custodian-ready" in workflow and
        "Bind inline custodian self and parent authority before checkout" in workflow and
        "Bind global custodian uniqueness after checkout" in workflow,
        "custodian cannot prove exact separate-run dispatch identity before mutation")
require("external-custodian-force-cancel", "needs.artifact-promote.result" not in workflow,
        "same-run finalizer remains the only rollback/cleanup custody after force-cancel")
require("external-custodian-lease", "pensyve-production-gateway" in workflow and
        "artifact-custodian-producer" in workflow and "artifact-custodian-finalize" in workflow,
        "no separately running custodian owns the production concurrency lease")
require("external-custodian-push-main", "push-main" in workflow and
        "pensyve-production-gateway" in workflow and "Bind global custodian uniqueness after checkout" in workflow,
        "push-main does not share/refuse the active custodian lease")

cleanup = promote.find("promotion-custody cleanup verified exact forgotten_count=")
fresh_after_cleanup = promote.find("verify_candidate_deployment", cleanup + 1) if cleanup >= 0 else -1
require("post-cleanup-fresh-state", cleanup >= 0 and fresh_after_cleanup > cleanup,
        "successful forgotten_count=3 is not followed by fresh service/task/digest/target-health proof")
require("post-cleanup-drift-mutation", "cleanup-final-state-drift" in tests,
        "DELETE-time candidate drift rollback mutation is absent")

promote_block = workflow[workflow.find("  artifact-custodian-producer:"):workflow.find("  artifact-custodian-finalize:")]
finalizer_block = workflow[workflow.find("  artifact-custodian-finalize:"):workflow.find("  artifact-cleanup:")]
authority_tokens = (
    "BILLING_SNAPSHOT_AT", "CURRENT_BILLABLE_BYTES", "ORGANIZATION_ACTIONS_ARTIFACT_BYTES",
    "ORGANIZATION_PACKAGES_BYTES", "INCLUDED_SOURCE_ARTIFACT_ID",
    "INCLUDED_SOURCE_ARTIFACT_BYTES", "HANDOFF_OVERHEAD_BYTES", "PAYMENT_STATUS", "SPENDING_STATUS",
)
for block_name, block in (("promotion", promote_block), ("custodian", finalizer_block)):
    for token in authority_tokens:
        require(f"{block_name}-full-authority-recheck", token in block,
                f"{block_name} precredential authority recheck omits {token}")
    require(f"{block_name}-authority-mutations", f"{block_name}-authority-" in tests,
            f"{block_name} lacks per-variable executable authority mutations")

if errors:
    print("\n".join(errors), file=sys.stderr)
    raise SystemExit(1)
print("round5 source review contract passed")
PY
    then
        failed=1
    fi
    [[ "${failed}" -eq 0 ]] || return 1
    echo "round5 source review contract passed"
}

run_round6_review() {
    local fixture="${TEST_ROOT}/round6-review" failed=0
    mkdir -p "${fixture}"
    if ! python3 - "${WORKFLOW}" "${ARTIFACT_SCRIPT}" <<'PY'
import sys
from pathlib import Path
import yaml

workflow_path, artifact_path = map(Path, sys.argv[1:])
workflow_text = workflow_path.read_text()
artifact_text = artifact_path.read_text()
workflow = yaml.load(workflow_text, Loader=yaml.BaseLoader)
errors = []

def require(name, condition, detail):
    if not condition:
        errors.append(f"ROUND6-RED {name}: {detail}")

jobs = workflow["jobs"]
require("global-paginated-custodian-window", "verify-custodian-runs" in artifact_text,
        "artifact-owned globally unique run validator mode is absent")
require("global-paginated-custodian-window", workflow_text.count("--paginate --slurp") >= 3,
        "dispatcher/producer/finalizer do not fully paginate workflow runs")
for job_name in ("artifact-promote-dispatch", "artifact-custodian-producer", "artifact-custodian-finalize"):
    require("global-paginated-custodian-window", "--paginate --slurp" in str(jobs[job_name]),
            f"{job_name} does not fully paginate its exact workflow-run inventory")
for job_name in ("artifact-custodian-producer", "artifact-custodian-finalize"):
    require("global-paginated-custodian-window", "verify-custodian-runs" in str(jobs[job_name]),
            f"{job_name} bypasses the artifact-owned global uniqueness validator")
for token in ("parent_created_at", "dispatch_started_at", "authorization_start_at",
              "authorization_end_at", "source_artifact_expires_at"):
    require("sealed-authorization-window", token in workflow_text,
            f"sealed custody request omits {token}")
require("bounded-pagination-cycle", workflow_text.count("timeout 30 gh api") >= 3,
        "workflow-run pagination lacks a command timeout")
require("dispatch-run-details-fallback", "return_run_details" in workflow_text and
        "dispatch-response" in workflow_text and "204" in workflow_text,
        "dispatcher lacks verified run-details plus 204/no-body fallback")
for marker in ("source-upload-quiescence", "handoff-upload-quiescence"):
    require("outcome-aware-upload-quiescence", marker in workflow_text,
            f"terminal upload resolver lacks {marker}")
require("outcome-aware-upload-quiescence", "post-upload-unresolved" in workflow_text,
        "successful zero inventory can still become authoritative no-upload")
require("name-only-cleanup-custody", "cleanup-name-quiescence" in workflow_text,
        "cleanup cannot independently bind an unresolved current-run name")
require("minimal-dispatcher-permissions",
        jobs["artifact-promote-dispatch"].get("permissions") == {"actions": "write"},
        "dispatcher has permissions beyond actions:write")
require("minimal-observer-permissions",
        jobs["artifact-promote"].get("permissions") == {"actions": "read"},
        "observer has permissions beyond actions:read")

if errors:
    print("\n".join(errors), file=sys.stderr)
    raise SystemExit(1)
PY
    then
        failed=1
    fi
    [[ "${failed}" -eq 0 ]] || return 1

    local request="${fixture}/custody-request.json" pages="${fixture}/pages.json" output="${fixture}/match.json"
    jq -n --arg sha "${SOURCE_SHA}" '
      {schema_version:2,repository:"major7apps/pensyve",parent_run_id:1234,parent_run_attempt:2,
       parent_ref:"strict-local-runtime",reviewed_sha:$sha,pull_request_number:16,
       handoff_id:888,handoff_name:("gateway-handoff-1234-2-"+$sha),
       handoff_digest:("sha256:"+("c"*64)),handoff_size:4096,
       handoff_created_at:"2026-08-29T00:00:00Z",handoff_expires_at:"2026-09-28T00:00:00Z",
       source_artifact_expires_at:"2026-09-27T00:00:00Z",
       parent_created_at:"2026-08-29T00:00:00Z",dispatch_started_at:"2026-08-29T00:05:00Z",
       authorization_start_at:"2026-08-28T23:55:00Z",authorization_end_at:"2026-09-27T00:00:00Z",
       custody_lease_id:("5"*64)}' > "${request}"
    make_run() {
        local id="$1" created="$2"
        jq -n --argjson id "${id}" --arg sha "${SOURCE_SHA}" --arg created "${created}" '
          {id:$id,run_attempt:1,status:"queued",display_title:("gateway-custodian-"+("5"*64)),
           repository:{full_name:"major7apps/pensyve"},event:"workflow_dispatch",head_sha:$sha,
           head_branch:"strict-local-runtime",path:".github/workflows/deploy-gateway.yml",created_at:$created}'
    }
    jq -n --argjson run "$(make_run 9001 2026-08-29T00:05:01Z)" \
      '[{total_count:1,workflow_runs:[$run]}]' > "${pages}"
    "${ARTIFACT_SCRIPT}" verify-custodian-runs --input "${pages}" --request "${request}" --output "${output}"
    jq -e '.id == 9001 and .run_attempt == 1' "${output}" >/dev/null || fail "exact custodian run output mismatch"

    local later_request="${fixture}/later-valid-request.json" later_pages="${fixture}/later-valid-pages.json"
    jq '.dispatch_started_at="2026-08-29T03:00:00Z"' "${request}" > "${later_request}"
    jq -n --argjson run "$(make_run 9001 2026-08-29T03:00:01Z)" \
      '[{total_count:1,workflow_runs:[$run]}]' > "${later_pages}"
    "${ARTIFACT_SCRIPT}" verify-custodian-runs --input "${later_pages}" \
      --request "${later_request}" --output "${output}"
    jq -e '.id == 9001' "${output}" >/dev/null \
      || fail "later dispatch inside sealed artifact validity was rejected"

    local duplicate="${fixture}/duplicate.json"
    jq -n --argjson current "$(make_run 9001 2026-08-29T00:05:01Z)" \
      --argjson original "$(make_run 9002 2026-08-29T00:05:02Z)" \
      '[{total_count:2,workflow_runs:[$current]},{total_count:2,workflow_runs:[$original]}]' > "${duplicate}"
    expect_failure "exact custodian run identity is ambiguous" "${ARTIFACT_SCRIPT}" verify-custodian-runs \
      --input "${duplicate}" --request "${request}" --output "${output}"
    jq -n --argjson current "$(make_run 9001 2026-08-29T00:05:01Z)" \
      --argjson aged "$(make_run 9002 2026-09-15T00:00:00Z)" \
      '[{total_count:2,workflow_runs:[$current]},{total_count:2,workflow_runs:[$aged]}]' > "${duplicate}"
    expect_failure "exact custodian run identity is ambiguous" "${ARTIFACT_SCRIPT}" verify-custodian-runs \
      --input "${duplicate}" --request "${request}" --output "${output}"
    jq -n '[{total_count:1,workflow_runs:{malformed:true}}]' > "${duplicate}"
    expect_failure "malformed paginated workflow runs" "${ARTIFACT_SCRIPT}" verify-custodian-runs \
      --input "${duplicate}" --request "${request}" --output "${output}"
    local boundary
    for boundary in 2026-08-28T23:55:00Z 2026-09-27T00:00:00Z; do
        jq -n --argjson current "$(make_run 9001 2026-08-29T00:05:01Z)" \
          --argjson boundary_run "$(make_run 9002 "${boundary}")" \
          '[{total_count:2,workflow_runs:[$current]},{total_count:2,workflow_runs:[$boundary_run]}]' > "${duplicate}"
        expect_failure "exact custodian run identity is ambiguous" "${ARTIFACT_SCRIPT}" verify-custodian-runs \
          --input "${duplicate}" --request "${request}" --output "${output}"
    done
    echo "round6 global pagination/window contract passed"
}

round9_expect_rejection() {
    local label="$1" expected="$2"
    shift 2
    local output="${TEST_ROOT}/round9-${label}.log"
    if "$@" >"${output}" 2>&1; then
        echo "ROUND9 RED accepted invalid authority: ${label}" >&2
        return 1
    fi
    if ! grep -F -- "${expected}" "${output}" >/dev/null; then
        cat "${output}" >&2
        echo "ROUND9 RED wrong rejection for ${label}; expected ${expected}" >&2
        return 1
    fi
    echo "round9 rejected ${label}"
}

run_round9_review() {
    require_scripts
    local fixture="${TEST_ROOT}/round9" mutation="${TEST_ROOT}/round9-mutation.json"
    local failures=0 device available index
    make_local_fixture "${fixture}"
    jq '.storage' "${fixture}/tuple.json" > "${fixture}/storage.json"

    mutate_authority_json "${fixture}/storage.json" "${mutation}" \
      current_billable_bytes false organization_actions_artifact_bytes false
    round9_expect_rejection bool-current-organization "current billable bytes must be a non-negative integer" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/storage.json" "${mutation}" retained_source_artifact_bytes false
    round9_expect_rejection bool-excluded-retained-source-bytes "source-excluded snapshot must add the source exactly once after upload" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))

    mutate_authority_json "${fixture}/storage.json" "${mutation}" \
      archive_bytes false evidence_bytes false container_overhead_bytes false \
      handoff_overhead_bytes false runner_available_bytes true
    round9_expect_rejection bool-precheck-payload "archive bytes must be a non-negative integer" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))

    jq '.snapshot_inclusion_mode="source-included" | .retained_source_artifact_id=777 |
        .retained_source_artifact_bytes=4096 | .source_snapshot_at=(.snapshot_at | fromdateiso8601 - 1 | todateiso8601) |
        .current_billable_bytes=4096 | .organization_actions_artifact_bytes=4096 | .organization_packages_bytes=0' \
      "${fixture}/storage.json" > "${fixture}/source-included.json"
    "${ARTIFACT_SCRIPT}" storage-precheck --input "${fixture}/source-included.json" \
      --output "${fixture}/source-included-pass.json"
    mutate_authority_json "${fixture}/source-included.json" "${mutation}" \
      retained_source_artifact_id true retained_source_artifact_bytes true
    round9_expect_rejection bool-retained-source "source-included snapshot must identify retained source bytes" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))

    mutate_authority_json "${fixture}/storage.json" "${mutation}" \
      approved_gb_hours_ceiling infinity
    round9_expect_rejection nonfinite-precheck "approved gb hours ceiling must be a positive finite number" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/storage.json" "${mutation}" \
      price_per_gb_month infinity approved_dollar_ceiling infinity projected_dollars infinity
    round9_expect_rejection nonfinite-precheck-rate "price per gb month must be a positive finite number" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/storage.json" "${mutation}" \
      approved_gb_hours_ceiling infinity projected_gb_hours infinity
    round9_expect_rejection nonfinite-precheck-projection "declared projected GB-hours must be a finite non-negative number" \
      "${ARTIFACT_SCRIPT}" storage-precheck --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))

    jq '. + {rest_size_in_bytes:4096,created_at:.rest_created_at,expires_at:.rest_expires_at}' \
      "${fixture}/storage.json" > "${fixture}/reconcile.json"
    mutate_authority_json "${fixture}/reconcile.json" "${mutation}" rest_size_in_bytes true
    round9_expect_rejection bool-rest-size "REST artifact size is invalid" \
      "${ARTIFACT_SCRIPT}" storage-reconcile --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/reconcile.json" "${mutation}" retained_source_artifact_bytes false
    round9_expect_rejection bool-reconcile-excluded-retained "source-excluded reconciliation double-counts the source artifact" \
      "${ARTIFACT_SCRIPT}" storage-reconcile --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/reconcile.json" "${mutation}" \
      current_billable_bytes false organization_actions_artifact_bytes false
    round9_expect_rejection bool-reconcile-organization "current billable bytes must be a non-negative integer" \
      "${ARTIFACT_SCRIPT}" storage-reconcile --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    jq '. + {rest_size_in_bytes:4096,created_at:.rest_created_at,expires_at:.rest_expires_at}' \
      "${fixture}/source-included.json" > "${fixture}/source-included-reconcile.json"
    mutate_authority_json "${fixture}/source-included-reconcile.json" "${mutation}" \
      retained_source_artifact_id true retained_source_artifact_bytes true
    round9_expect_rejection bool-reconcile-retained "source-included reconciliation is missing the retained source identity" \
      "${ARTIFACT_SCRIPT}" storage-reconcile --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/reconcile.json" "${mutation}" \
      approved_gb_hours_ceiling infinity
    round9_expect_rejection nonfinite-reconcile "approved gb hours ceiling must be a positive finite number" \
      "${ARTIFACT_SCRIPT}" storage-reconcile --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/reconcile.json" "${mutation}" \
      approved_dollar_ceiling infinity price_per_gb_month infinity
    round9_expect_rejection nonfinite-reconcile-rate "price per gb month must be a positive finite number" \
      "${ARTIFACT_SCRIPT}" storage-reconcile --input "${mutation}" --output "${fixture}/bad.json" || failures=$((failures + 1))

    device="$(df --output=source /tmp | awk 'NR==2 {print $1}')"
    available="$(df --output=avail -B1 /tmp | awk 'NR==2 {print $1}')"
    jq -n --arg device "${device}" --argjson available "${available}" '
      {filesystems:["workspace","cargo","model_scratch","docker","tmp"] |
       to_entries | map({name:.value,path:"/tmp",device:($device+(.key|tostring)),available_bytes:$available,required_bytes:1})}' \
      > "${fixture}/disk.json"
    for index in 0 1 2 3 4; do
      mutate_authority_json "${fixture}/disk.json" "${fixture}/disk-next.json" \
        "filesystems.${index}.available_bytes" true "filesystems.${index}.required_bytes" true
      mv -- "${fixture}/disk-next.json" "${fixture}/disk.json"
    done
    round9_expect_rejection bool-disk-capacity "disk precheck filesystem identity/availability is invalid" \
      "${ARTIFACT_SCRIPT}" disk-precheck --input "${fixture}/disk.json" --output "${fixture}/bad.json" || failures=$((failures + 1))

    mutate_authority_json "${fixture}/tuple.json" "${mutation}" artifact.id true
    round9_expect_rejection bool-tuple-artifact-id "artifact id must be positive" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/tuple.json" "${mutation}" \
      storage.current_billable_bytes false storage.organization_actions_artifact_bytes false
    round9_expect_rejection bool-tuple-organization "storage current billable bytes must be a non-negative integer" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/tuple.json" "${mutation}" \
      storage.approved_gb_hours_ceiling infinity storage.approved_dollar_ceiling infinity
    round9_expect_rejection nonfinite-tuple "missing or invalid storage approved_gb_hours_ceiling" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/tuple.json" "${mutation}" \
      storage.approved_dollar_ceiling infinity storage.price_per_gb_month infinity \
      storage.projected_dollars infinity storage.actual_dollars infinity
    round9_expect_rejection nonfinite-tuple-rate "missing or invalid storage price_per_gb_month" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/tuple.json" "${mutation}" \
      storage.approved_gb_hours_ceiling infinity storage.projected_gb_hours infinity
    round9_expect_rejection nonfinite-tuple-projection "storage projected gb hours must be a finite non-negative number" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/tuple.json" "${mutation}" \
      storage.computed_projected_gb_hours infinity
    round9_expect_rejection nonfinite-tuple-computed-projection "storage computed projected gb hours must be a finite non-negative number" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/tuple.json" "${mutation}" gates.embedding_pool_size true
    round9_expect_rejection bool-tuple-count "embedding_pool_size" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))
    jq '.gates.no_egress=1' "${fixture}/tuple.json" > "${mutation}"
    round9_expect_rejection numeric-tuple-boolean "no_egress" \
      "${ARTIFACT_SCRIPT}" verify-local --tuple "${mutation}" || failures=$((failures + 1))

    make_reviewed_tuple_and_request "${fixture}/tuple.json" "${fixture}/reviewed.json" "${fixture}/request.json"
    "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed.json" \
      --request "${fixture}/request.json" --output "${fixture}/verified.json"
    mutate_authority_json "${fixture}/verified.json" "${mutation}" deployment.pending_count false
    round9_expect_rejection bool-handoff-count "Task 8 pending_count mismatch" \
      "${ARTIFACT_SCRIPT}" verify-handoff --input "${mutation}" || failures=$((failures + 1))
    mutate_authority_json "${fixture}/verified.json" "${mutation}" image.compressed_layer_bytes true
    round9_expect_rejection bool-handoff-image-bytes "image compressed layer bytes must be a positive integer" \
      "${ARTIFACT_SCRIPT}" verify-handoff --input "${mutation}" || failures=$((failures + 1))
    jq '.deployment.baseline_service_snapshot.deployment_configuration.deploymentCircuitBreaker.enable=1' \
      "${fixture}/verified.json" > "${mutation}"
    local round9_snapshot_sha
    round9_snapshot_sha="$(jq -S -c '.deployment.baseline_service_snapshot' "${mutation}" | sha256sum | cut -d' ' -f1)"
    jq --arg sha "${round9_snapshot_sha}" '.deployment.baseline_service_snapshot_sha256=$sha' \
      "${mutation}" > "${fixture}/numeric-circuit-breaker.json"
    round9_expect_rejection numeric-handoff-boolean "Task 8 canonical deployment circuit breaker is invalid" \
      "${ARTIFACT_SCRIPT}" verify-handoff --input "${fixture}/numeric-circuit-breaker.json" || failures=$((failures + 1))
    jq '.reviewed_pull_request.draft=0' "${fixture}/reviewed.json" > "${fixture}/reviewed-draft-number.json"
    jq '.reviewed_pull_request_draft=0' "${fixture}/request.json" > "${fixture}/request-draft-number.json"
    round9_expect_rejection numeric-reviewed-draft "Task 5-reviewed pull request draft mismatch" \
      "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${fixture}/reviewed-draft-number.json" \
      --request "${fixture}/request-draft-number.json" --output "${fixture}/bad-verified.json" || failures=$((failures + 1))

    if validate_workflow "${WORKFLOW}" >"${fixture}/exact-test-contract.log" 2>&1; then
      echo "round9 GTE/BGE exact selection and result gate passed"
    else
      cat "${fixture}/exact-test-contract.log" >&2
      echo "ROUND9 RED current release lacks a generalized GTE/BGE exact result gate" >&2
      failures=$((failures + 1))
    fi
    python3 - "${RELEASE_SCRIPT}" "${fixture}/verify-exact-test-result.sh" <<'PY'
from pathlib import Path
import re
import sys
source = Path(sys.argv[1]).read_text()
match = re.search(r'(?ms)^verify_exact_test_result\(\) \{.*?^\}\n', source)
if not match:
    raise SystemExit("release generalized exact result verifier is absent")
Path(sys.argv[2]).write_text(
    '#!/usr/bin/env bash\nset -euo pipefail\ndie() { echo "gateway release image error: $*" >&2; exit 1; }\n' +
    match.group(0) + '\nverify_exact_test_result "$1" "$2"\n'
)
PY
    chmod +x "${fixture}/verify-exact-test-result.sh"
    for model in gte bge; do
      local label="${model^^}"
      printf 'running 1 test\ntest exact_model_proof ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.01s\n' \
        > "${fixture}/${model}-one-pass.log"
      "${fixture}/verify-exact-test-result.sh" "${label}" "${fixture}/${model}-one-pass.log"
      printf 'running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 5 filtered out; finished in 0.00s\n' \
        > "${fixture}/${model}-zero-selected.log"
      round9_expect_rejection "${model}-zero-selected" "expected exactly one selected test" \
        "${fixture}/verify-exact-test-result.sh" "${label}" "${fixture}/${model}-zero-selected.log" \
        || failures=$((failures + 1))
    done

    [[ "${failures}" -eq 0 ]] || fail "round9 exact numeric/model authority mutations accepted: ${failures}"
    echo "round9 exact numeric and model authority contract passed"
}

run_round10_review() {
    require_scripts
    local fixture="${TEST_ROOT}/round10-review"
    local environment_sha snapshot_sha mutation output
    local failures=0
    make_local_fixture "${fixture}"
    mkdir -p "${fixture}/bin"
    printf '[{"name":"MCP_ALLOWED_HOSTS","value":"mcp.pensyve.com"}]\n' > "${fixture}/environment.json"
    environment_sha="$(jq -S -c . "${fixture}/environment.json" | sha256sum | cut -d' ' -f1)"
    jq -n '{service_name:"pensyve-prod-gateway",status:"ACTIVE",
      cluster_arn:"arn:aws:ecs:us-east-2:123456789012:cluster/pensyve-prod",
      task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",
      counts:{desired:2,running:2,pending:0},
      network_configuration:{awsvpcConfiguration:{subnets:["subnet-aaa","subnet-bbb"],securityGroups:["sg-aaa"],assignPublicIp:"DISABLED"}},
      load_balancers:[{targetGroupArn:"arn:aws:elasticloadbalancing:us-east-2:123456789012:targetgroup/pensyve-gateway/abc",containerName:"gateway",containerPort:3100}],
      deployment_configuration:{deploymentCircuitBreaker:{enable:true,rollback:true},maximumPercent:200,minimumHealthyPercent:100},
      health_grace_period_seconds:300,
      primary_deployment:{status:"PRIMARY",task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",rollout_state:"COMPLETED",desired:2,running:2,pending:0}}' \
      > "${fixture}/service-snapshot.json"
    snapshot_sha="$(jq -S -c . "${fixture}/service-snapshot.json" | sha256sum | cut -d' ' -f1)"
    jq --arg env_sha "${environment_sha}" --arg snapshot_sha "${snapshot_sha}" \
      --slurpfile snapshot "${fixture}/service-snapshot.json" \
      '{schema_version:1,cleanup_required:false,image:.image,scanner:.scanner,scan:.scan,deployment:{region:"us-east-2",ecr_registry:"123456789012.dkr.ecr.us-east-2.amazonaws.com",ecr_repository:"pensyve-gateway",cluster:"pensyve-prod",service:"pensyve-prod-gateway",gateway_container:"gateway",baseline_task_definition_arn:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",baseline_image:"123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",baseline_environment_sha256:$env_sha,baseline_service_snapshot:$snapshot[0],baseline_service_snapshot_sha256:$snapshot_sha,probe_entity:"task9-runtime-1234-2-0123456789abcdef",promotion_run_id:1234,promotion_run_attempt:2,cpu:"512",memory:"4096",desired_count:2,running_count:2,pending_count:0}}' \
      "${fixture}/tuple.json" > "${fixture}/verified-image.json"

    export STUB_LOG="${fixture}/stub.log"
    write_stub "${fixture}/bin/docker" 'exit 97'
    write_stub "${fixture}/bin/aws" 'exit 98'
    write_stub "${fixture}/bin/curl" 'exit 99'
    write_stub "${fixture}/bin/sleep" 'exit 100'

    round10_mutate() {
        local name="$1" destination="$2"
        python3 - "${fixture}/verified-image.json" "${destination}" "${name}" <<'PY'
import copy
import hashlib
import json
import sys
from pathlib import Path

source, destination, name = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
data = json.loads(source.read_text())
snapshot = data["deployment"]["baseline_service_snapshot"]

mutations = {
    "bool-schema-version": (data, "schema_version", True),
    "bool-compressed-bytes": (data["image"], "compressed_layer_bytes", True),
    "bool-uncompressed-bytes": (data["image"], "uncompressed_image_bytes", True),
    "nonfinite-compressed-bytes": (data["image"], "compressed_layer_bytes", float("inf")),
    "bool-desired-count": (data["deployment"], "desired_count", True),
    "bool-running-count": (data["deployment"], "running_count", True),
    "bool-pending-count": (data["deployment"], "pending_count", False),
    "bool-snapshot-desired": (snapshot["counts"], "desired", True),
    "bool-snapshot-running": (snapshot["counts"], "running", True),
    "bool-snapshot-pending": (snapshot["counts"], "pending", False),
    "bool-primary-desired": (snapshot["primary_deployment"], "desired", True),
    "bool-primary-running": (snapshot["primary_deployment"], "running", True),
    "bool-primary-pending": (snapshot["primary_deployment"], "pending", False),
    "bool-health-grace": (snapshot, "health_grace_period_seconds", True),
    "bool-container-port": (snapshot["load_balancers"][0], "containerPort", True),
    "numeric-circuit-enable": (snapshot["deployment_configuration"]["deploymentCircuitBreaker"], "enable", 1),
    "numeric-circuit-enable-zero": (snapshot["deployment_configuration"]["deploymentCircuitBreaker"], "enable", 0),
    "numeric-circuit-rollback": (snapshot["deployment_configuration"]["deploymentCircuitBreaker"], "rollback", 1),
    "numeric-circuit-rollback-zero": (snapshot["deployment_configuration"]["deploymentCircuitBreaker"], "rollback", 0),
    "bool-maximum-percent": (snapshot["deployment_configuration"], "maximumPercent", True),
    "bool-minimum-percent": (snapshot["deployment_configuration"], "minimumHealthyPercent", True),
    "bool-promotion-run-id": (data["deployment"], "promotion_run_id", True),
    "bool-promotion-run-attempt": (data["deployment"], "promotion_run_attempt", True),
}
if name not in mutations:
    raise SystemExit(f"unknown round10 mutation: {name}")
owner, key, value = mutations[name]
owner[key] = value
if name == "bool-promotion-run-id":
    data["deployment"]["probe_entity"] = "task9-runtime-True-2-0123456789abcdef"
if name == "bool-promotion-run-attempt":
    data["deployment"]["probe_entity"] = "task9-runtime-1234-True-0123456789abcdef"
if owner is snapshot or any(owner is value for value in (
        snapshot["counts"], snapshot["primary_deployment"], snapshot["load_balancers"][0],
        snapshot["deployment_configuration"], snapshot["deployment_configuration"]["deploymentCircuitBreaker"],
)):
    canonical = (json.dumps(snapshot, sort_keys=True, separators=(",", ":")) + "\n").encode()
    data["deployment"]["baseline_service_snapshot_sha256"] = hashlib.sha256(canonical).hexdigest()
destination.write_text(json.dumps(data, sort_keys=True, separators=(",", ":")) + "\n")
PY
    }

    round10_expect_pretool_rejection() {
        local name="$1" expected="$2" input="$3"
        output="${fixture}/${name}.log"
        : > "${STUB_LOG}"
        set +e
        env PROMOTION_CUSTODY=deferred DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" \
          CURL_BIN="${fixture}/bin/curl" SLEEP_BIN="${fixture}/bin/sleep" \
          "${PROMOTE_SCRIPT}" promote "${input}" > "${output}" 2>&1
        local status=$?
        set -e
        [[ "${status}" -ne 0 ]] || { echo "ROUND10 RED promoter mutation returned success: ${name}" >&2; failures=$((failures + 1)); return; }
        if [[ -s "${STUB_LOG}" ]]; then
            echo "ROUND10 RED promoter mutation reached external tools: ${name}" >&2
            failures=$((failures + 1))
            return
        fi
        if ! grep -F -- "${expected}" "${output}" >/dev/null; then
            cat "${output}" >&2
            echo "ROUND10 RED promoter mutation failed for wrong reason: ${name}" >&2
            failures=$((failures + 1))
            return
        fi
        echo "round10 promoter rejected before tools: ${name}"
    }

    : > "${STUB_LOG}"
    capture_failure "${fixture}/valid-reaches-docker.log" env PROMOTION_CUSTODY=deferred \
      DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" \
      SLEEP_BIN="${fixture}/bin/sleep" "${PROMOTE_SCRIPT}" promote "${fixture}/verified-image.json"
    [[ "$(call_count "${STUB_LOG}" docker load)" -eq 1 ]] \
      || fail "valid promoter fixture did not pass validation and reach exactly one Docker load"
    [[ "$(grep -Ec '^BEGIN\t(aws|curl|sleep)$' "${STUB_LOG}" || true)" -eq 0 ]] \
      || fail "valid promoter validator fixture reached a later external command"
    echo "round10 valid promoter passed validation and reached the bounded Docker sentinel"

    local -a round10_cases=(
      'bool-schema-version|fixed promotion shape'
      'bool-compressed-bytes|image compressed layer bytes must be a positive integer'
      'bool-uncompressed-bytes|image uncompressed image bytes must be a positive integer'
      'nonfinite-compressed-bytes|image compressed layer bytes must be a positive integer'
      'bool-desired-count|Task 8 counts must remain exactly 2/2/0'
      'bool-running-count|Task 8 counts must remain exactly 2/2/0'
      'bool-pending-count|Task 8 counts must remain exactly 2/2/0'
      'bool-snapshot-desired|canonical service snapshot task/count is invalid'
      'bool-snapshot-running|canonical service snapshot task/count is invalid'
      'bool-snapshot-pending|canonical service snapshot task/count is invalid'
      'bool-primary-desired|canonical primary deployment is invalid'
      'bool-primary-running|canonical primary deployment is invalid'
      'bool-primary-pending|canonical primary deployment is invalid'
      'bool-health-grace|canonical deployment configuration is invalid'
      'bool-container-port|canonical load-balancer binding is invalid'
      'numeric-circuit-enable|canonical deployment circuit breaker is invalid'
      'numeric-circuit-enable-zero|canonical deployment circuit breaker is invalid'
      'numeric-circuit-rollback|canonical deployment circuit breaker is invalid'
      'numeric-circuit-rollback-zero|canonical deployment circuit breaker is invalid'
      'bool-maximum-percent|canonical deployment percentages/grace are invalid'
      'bool-minimum-percent|canonical deployment percentages/grace are invalid'
      'bool-promotion-run-id|promotion_run_id is invalid'
      'bool-promotion-run-attempt|promotion_run_attempt is invalid'
    )
    local case_entry name expected
    for case_entry in "${round10_cases[@]}"; do
        name="${case_entry%%|*}"
        expected="${case_entry#*|}"
        mutation="${fixture}/${name}.json"
        round10_mutate "${name}" "${mutation}"
        round10_expect_pretool_rejection "${name}" "${expected}" "${mutation}"
    done

    [[ "${failures}" -eq 0 ]] || fail "round10 promoter exact-type mutations reached external tools: ${failures}"
    echo "round10 promoter exact-type validator contract passed"
}

run_round11_review() {
    require_scripts
    local failures=0 root model_root gte_repo bge_repo gte_revision bge_revision
    local gte_blob bge_blob manifest transcript roundtrip archive expectation expected_sha
    root="${TEST_ROOT}/round11-readonly-real-tree"
    model_root="${root}/evidence/release/exact-image-model-root"
    gte_repo="${model_root}/models--Alibaba-NLP--gte-base-en-v1.5"
    bge_repo="${model_root}/nested/cache/models--BAAI--bge-reranker-base"
    gte_revision="a829fd0e060bb84554da0dfd354d0de0f7712b7f"
    bge_revision="2cfc18c9415c912f9d8155881c133215df768a70"
    mkdir -p "${gte_repo}/blobs" "${gte_repo}/snapshots/${gte_revision}/onnx" \
      "${bge_repo}/blobs" "${bge_repo}/snapshots/${bge_revision}/onnx"
    printf 'round11 exact GTE blob bytes\n' > "${root}/gte.blob"
    printf 'round11 exact BGE blob bytes\n' > "${root}/bge.blob"
    gte_blob="$(sha256sum "${root}/gte.blob" | cut -d' ' -f1)"
    bge_blob="$(sha256sum "${root}/bge.blob" | cut -d' ' -f1)"
    mv "${root}/gte.blob" "${gte_repo}/blobs/${gte_blob}"
    mv "${root}/bge.blob" "${bge_repo}/blobs/${bge_blob}"
    ln -s "../../blobs/${gte_blob}" "${gte_repo}/snapshots/${gte_revision}/README.md"
    ln -s "../../../blobs/${gte_blob}" "${gte_repo}/snapshots/${gte_revision}/onnx/model.onnx"
    ln -s "../../blobs/${bge_blob}" "${bge_repo}/snapshots/${bge_revision}/README.md"
    ln -s "../../../blobs/${bge_blob}" "${bge_repo}/snapshots/${bge_revision}/onnx/model.onnx"
    chmod 0555 "${gte_repo}/snapshots/${gte_revision}" "${gte_repo}/snapshots/${gte_revision}/onnx" \
      "${bge_repo}/snapshots/${bge_revision}" "${bge_repo}/snapshots/${bge_revision}/onnx"
    [[ "$(stat -c '%a' "${gte_repo}/snapshots/${gte_revision}")" == 555 &&
       "$(stat -c '%a' "${bge_repo}/snapshots/${bge_revision}/onnx")" == 555 ]] \
      || fail "round11 fixture did not reproduce real runner-owned 0555 snapshot parents"
    manifest="${root}/sealed-files.sha256"
    transcript="${root}/seal-reverify.log"
    if ! "${ARTIFACT_SCRIPT}" materialize-model-links --root "${root}" \
      > "${TEST_ROOT}/round11-readonly-materialize.log" 2>&1; then
        cat "${TEST_ROOT}/round11-readonly-materialize.log" >&2
        echo "ROUND11-RED readonly-real-tree: actual materialize-before-seal order failed on runner-owned 0555 parents" >&2
        failures=$((failures + 1))
    else
        "${ARTIFACT_SCRIPT}" seal-tree --root "${root}" --manifest "${manifest}" --transcript "${transcript}"
        for expectation in \
          "${gte_repo}/snapshots/${gte_revision}/README.md|${gte_blob}" \
          "${gte_repo}/snapshots/${gte_revision}/onnx/model.onnx|${gte_blob}" \
          "${bge_repo}/snapshots/${bge_revision}/README.md|${bge_blob}" \
          "${bge_repo}/snapshots/${bge_revision}/onnx/model.onnx|${bge_blob}"; do
            link="${expectation%%|*}"
            expected_sha="${expectation##*|}"
            [[ -f "${link}" && ! -L "${link}" && "$(sha256sum "${link}" | cut -d' ' -f1)" == "${expected_sha}" ]] \
              || fail "round11 link did not materialize to exact blob bytes: ${link}"
        done
        [[ "$(stat -c '%a' "${gte_repo}/snapshots/${gte_revision}")" == 755 &&
           "$(stat -c '%a' "${bge_repo}/snapshots/${bge_revision}/onnx")" == 755 ]] \
          || fail "round11 materialize-then-seal did not finalize snapshot modes to 0755"
        "${ARTIFACT_SCRIPT}" verify-tree --root "${root}" --input "${root}/sealed-tree.json" \
          --transcript "${TEST_ROOT}/round11-readonly-tree.replay.log"
        archive="${TEST_ROOT}/round11-readonly-real-tree.zip"
        roundtrip="${TEST_ROOT}/round11-readonly-real-tree-roundtrip"
        (cd "${root}" && zip -q -r "${archive}" .)
        mkdir -p "${roundtrip}"
        unzip -q "${archive}" -d "${roundtrip}"
        "${ARTIFACT_SCRIPT}" verify-tree --root "${roundtrip}" \
          --input "${roundtrip}/sealed-tree.json" \
          --transcript "${TEST_ROOT}/round11-readonly-zip.replay.log"
    fi

    round11_seed_two_links() {
        local fixture_root="$1"
        ROUND11_REPOSITORY="${fixture_root}/evidence/release/exact-image-model-root/models--Example--global"
        ROUND11_REVISION="1111111111111111111111111111111111111111"
        ROUND11_SNAPSHOT="${ROUND11_REPOSITORY}/snapshots/${ROUND11_REVISION}"
        mkdir -p "${ROUND11_REPOSITORY}/blobs" "${ROUND11_SNAPSHOT}"
        printf 'round11 globally valid bytes\n' > "${fixture_root}/good.blob"
        printf 'round11 second valid bytes\n' > "${fixture_root}/bad.blob"
        ROUND11_GOOD_SHA="$(sha256sum "${fixture_root}/good.blob" | cut -d' ' -f1)"
        ROUND11_BAD_SHA="$(sha256sum "${fixture_root}/bad.blob" | cut -d' ' -f1)"
        mv "${fixture_root}/good.blob" "${ROUND11_REPOSITORY}/blobs/${ROUND11_GOOD_SHA}"
        mv "${fixture_root}/bad.blob" "${ROUND11_REPOSITORY}/blobs/${ROUND11_BAD_SHA}"
        ROUND11_GOOD_LINK="${ROUND11_SNAPSHOT}/a-good.onnx"
        ROUND11_BAD_LINK="${ROUND11_SNAPSHOT}/z-bad.onnx"
        ln -s "../../blobs/${ROUND11_GOOD_SHA}" "${ROUND11_GOOD_LINK}"
        ln -s "../../blobs/${ROUND11_BAD_SHA}" "${ROUND11_BAD_LINK}"
    }

    round11_expect_global_rejection() {
        local name="$1" expected="$2" fixture_root="$3" output good_target
        output="${TEST_ROOT}/round11-global-${name}.log"
        good_target="$(readlink "${ROUND11_GOOD_LINK}")"
        set +e
        "${ARTIFACT_SCRIPT}" materialize-model-links --root "${fixture_root}" > "${output}" 2>&1
        local status=$?
        set -e
        [[ "${status}" -ne 0 ]] || { echo "ROUND11-RED ${name}: invalid global link set passed" >&2; failures=$((failures + 1)); return; }
        if ! grep -F -- "${expected}" "${output}" >/dev/null; then
            cat "${output}" >&2
            echo "ROUND11-RED ${name}: global preflight failed for the wrong reason" >&2
            failures=$((failures + 1))
        fi
        if [[ ! -L "${ROUND11_GOOD_LINK}" || "$(readlink "${ROUND11_GOOD_LINK}")" != "${good_target}" ]]; then
            echo "ROUND11-RED ${name}: valid earlier link was partially materialized before global rejection" >&2
            failures=$((failures + 1))
        fi
        if [[ "${name}" == "temp-collision" ]]; then
            [[ "$(cat "${ROUND11_BAD_LINK}.pensyve-materialize")" == collision ]] \
              || { echo "ROUND11-RED temp-collision: pre-existing collision was altered" >&2; failures=$((failures + 1)); }
        elif find "${fixture_root}" \( -name '*.pensyve-materialize' -o -name '*.pensyve-rollback' \) | grep . >/dev/null; then
            echo "ROUND11-RED ${name}: rejected global preflight left a temporary entry" >&2
            failures=$((failures + 1))
        fi
    }

    local name fixture outside cycle_link expected
    for name in absolute escape missing cycle wrong-shape wrong-blob special temp-collision; do
        fixture="${TEST_ROOT}/round11-global-${name}"
        round11_seed_two_links "${fixture}"
        case "${name}" in
            absolute)
                rm "${ROUND11_BAD_LINK}"; ln -s /etc/passwd "${ROUND11_BAD_LINK}"
                expected="absolute symlink escapes artifact root"
                ;;
            escape)
                outside="${TEST_ROOT}/round11-outside-${name}"
                printf 'outside\n' > "${outside}"
                rm "${ROUND11_BAD_LINK}"
                ln -s "$(realpath --relative-to="${ROUND11_SNAPSHOT}" "${outside}")" "${ROUND11_BAD_LINK}"
                expected="symlink target escapes artifact root"
                ;;
            missing)
                rm "${ROUND11_BAD_LINK}" "${ROUND11_REPOSITORY}/blobs/${ROUND11_BAD_SHA}"
                ln -s "../../blobs/${ROUND11_BAD_SHA}" "${ROUND11_BAD_LINK}"
                expected="symlink cycle or missing target"
                ;;
            cycle)
                rm "${ROUND11_BAD_LINK}"
                ln -s z-cycle.onnx "${ROUND11_BAD_LINK}"
                cycle_link="${ROUND11_SNAPSHOT}/z-cycle.onnx"
                ln -s z-bad.onnx "${cycle_link}"
                expected="symlink cycle or missing target"
                ;;
            wrong-shape)
                rm "${ROUND11_BAD_LINK}"
                ROUND11_BAD_LINK="${ROUND11_REPOSITORY}/z-bad.onnx"
                ln -s "blobs/${ROUND11_BAD_SHA}" "${ROUND11_BAD_LINK}"
                expected="unsafe model snapshot symlink shape"
                ;;
            wrong-blob)
                mkdir "${ROUND11_REPOSITORY}/alternate"
                cp "${ROUND11_REPOSITORY}/blobs/${ROUND11_BAD_SHA}" \
                  "${ROUND11_REPOSITORY}/alternate/${ROUND11_BAD_SHA}"
                rm "${ROUND11_BAD_LINK}"
                ln -s "../../alternate/${ROUND11_BAD_SHA}" "${ROUND11_BAD_LINK}"
                expected="does not bind an exact internal blob"
                ;;
            special)
                rm "${ROUND11_REPOSITORY}/blobs/${ROUND11_BAD_SHA}"
                mkfifo "${ROUND11_REPOSITORY}/blobs/${ROUND11_BAD_SHA}"
                expected="not a regular file"
                ;;
            temp-collision)
                printf 'collision\n' > "${ROUND11_BAD_LINK}.pensyve-materialize"
                expected="materialization temporary path already exists"
                ;;
        esac
        round11_expect_global_rejection "${name}" "${expected}" "${fixture}"
    done

    local race_root race_repo race_snapshot first_blob large_blob first_link race_link race_log race_pid
    race_root="${TEST_ROOT}/round11-mid-materialization"
    race_repo="${race_root}/models--Example--race"
    race_snapshot="${race_repo}/snapshots/2222222222222222222222222222222222222222"
    mkdir -p "${race_repo}/blobs" "${race_snapshot}"
    printf 'first exact bytes\n' > "${race_root}/first.blob"
    first_blob="$(sha256sum "${race_root}/first.blob" | cut -d' ' -f1)"
    mv "${race_root}/first.blob" "${race_repo}/blobs/${first_blob}"
    dd if=/dev/zero of="${race_root}/large.blob" bs=1M count=256 status=none
    large_blob="$(sha256sum "${race_root}/large.blob" | cut -d' ' -f1)"
    mv "${race_root}/large.blob" "${race_repo}/blobs/${large_blob}"
    first_link="${race_snapshot}/a-first.onnx"
    race_link="${race_snapshot}/z-race.onnx"
    ln -s "../../blobs/${first_blob}" "${first_link}"
    ln -s "../../blobs/${large_blob}" "${race_link}"
    race_log="${TEST_ROOT}/round11-changed-symlink.log"
    set +e
    "${ARTIFACT_SCRIPT}" materialize-model-links --root "${race_root}" > "${race_log}" 2>&1 &
    race_pid=$!
    set -e
    local observed_temp=0
    for _ in $(seq 1 2000); do
        if [[ -e "${race_link}.pensyve-materialize" ]]; then observed_temp=1; break; fi
        kill -0 "${race_pid}" 2>/dev/null || break
        sleep 0.001
    done
    if [[ "${observed_temp}" -eq 1 ]]; then
        rm -f "${race_link}"
        ln -s "../../blobs/${first_blob}" "${race_link}"
    fi
    set +e
    wait "${race_pid}"
    local race_status=$?
    set -e
    if [[ "${observed_temp}" -ne 1 || "${race_status}" -eq 0 ||
          ! -L "${first_link}" || -e "${race_link}.pensyve-materialize" ]]; then
        cat "${race_log}" >&2
        echo "ROUND11-RED changed-symlink: mid-materialization failure was not detected and rolled back atomically" >&2
        failures=$((failures + 1))
    elif ! grep -F 'changed during materialization' "${race_log}" >/dev/null; then
        cat "${race_log}" >&2
        echo "ROUND11-RED changed-symlink: failure did not name the symlink race" >&2
        failures=$((failures + 1))
    fi

    [[ "${failures}" -eq 0 ]] || fail "round11 materialize-before-seal contracts failed: ${failures}"
    echo "round11 readonly materialize/seal and global atomicity contracts passed"
}

run_round12_review() {
    require_scripts
    local root repo snapshot moved first_sha large_sha first_link second_link log pid
    local observed_temp=0 status first_target second_target failures=0
    root="${TEST_ROOT}/round12-parent-swap"
    repo="${root}/models--Example--parent-swap"
    snapshot="${repo}/snapshots/3333333333333333333333333333333333333333"
    moved="${snapshot}.original"
    mkdir -p "${repo}/blobs" "${snapshot}"
    printf 'round12 exact first bytes\n' > "${root}/first.blob"
    first_sha="$(sha256sum "${root}/first.blob" | cut -d' ' -f1)"
    mv "${root}/first.blob" "${repo}/blobs/${first_sha}"
    dd if=/dev/zero of="${root}/large.blob" bs=1M count=512 status=none
    large_sha="$(sha256sum "${root}/large.blob" | cut -d' ' -f1)"
    mv "${root}/large.blob" "${repo}/blobs/${large_sha}"
    first_link="${snapshot}/a-first.onnx"
    second_link="${snapshot}/z-second.onnx"
    first_target="../../blobs/${first_sha}"
    second_target="../../blobs/${large_sha}"
    ln -s "${first_target}" "${first_link}"
    ln -s "${second_target}" "${second_link}"
    chmod 0555 "${snapshot}"

    log="${TEST_ROOT}/round12-parent-swap.log"
    set +e
    "${ARTIFACT_SCRIPT}" materialize-model-links --root "${root}" > "${log}" 2>&1 &
    pid=$!
    set -e
    for _ in $(seq 1 5000); do
        if [[ -e "${second_link}.pensyve-materialize" ]]; then observed_temp=1; break; fi
        kill -0 "${pid}" 2>/dev/null || break
        sleep 0.001
    done
    if [[ "${observed_temp}" -eq 1 ]]; then
        mv "${snapshot}" "${moved}"
        mkdir "${snapshot}"
        printf 'unrelated replacement bytes\n' > "${snapshot}/unrelated.txt"
        chmod 0755 "${snapshot}"
    fi
    set +e
    wait "${pid}"
    status=$?
    set -e

    [[ "${observed_temp}" -eq 1 ]] \
      || { echo "ROUND12-RED parent-swap: bounded race did not observe second materialization temporary" >&2; failures=$((failures + 1)); }
    [[ "${status}" -ne 0 ]] \
      || { echo "ROUND12-RED parent-swap: swapped parent unexpectedly passed" >&2; failures=$((failures + 1)); }
    if [[ "${observed_temp}" -eq 1 ]]; then
        printf 'original_mode=%s replacement_mode=%s first_type=%s temp_count=%s\n' \
          "$(stat -c '%a' "${moved}")" "$(stat -c '%a' "${snapshot}")" \
          "$([[ -L "${moved}/a-first.onnx" ]] && printf symlink || printf regular)" \
          "$(find "${moved}" "${snapshot}" \( -name '*.pensyve-materialize' -o -name '*.pensyve-rollback' \) | wc -l)" \
          | tee -a "${log}" >&2
        [[ "$(stat -c '%a' "${moved}")" == 555 ]] \
          || { echo "ROUND12-RED parent-swap: original parent mode was not restored to 0555" >&2; failures=$((failures + 1)); }
        [[ -L "${moved}/a-first.onnx" && "$(readlink "${moved}/a-first.onnx")" == "${first_target}" ]] \
          || { echo "ROUND12-RED parent-swap: first link in original parent was not rolled back" >&2; failures=$((failures + 1)); }
        [[ -L "${moved}/z-second.onnx" && "$(readlink "${moved}/z-second.onnx")" == "${second_target}" ]] \
          || { echo "ROUND12-RED parent-swap: second link in original parent changed" >&2; failures=$((failures + 1)); }
        if find "${moved}" "${snapshot}" \( -name '*.pensyve-materialize' -o -name '*.pensyve-rollback' \) | grep . >/dev/null; then
            echo "ROUND12-RED parent-swap: transaction left materialize/rollback residue" >&2
            failures=$((failures + 1))
        fi
        [[ "$(stat -c '%a' "${snapshot}")" == 755 && "$(cat "${snapshot}/unrelated.txt")" == 'unrelated replacement bytes' ]] \
          || { echo "ROUND12-RED parent-swap: unrelated replacement directory was mutated" >&2; failures=$((failures + 1)); }
        chmod 0755 "${moved}" "${snapshot}"
    fi

    [[ "${failures}" -eq 0 ]] || fail "round12 parent-path swap transaction contracts failed: ${failures}"
    echo "round12 parent-path swap transaction contract passed"
}

run_round13_review() {
    require_scripts
    local failures=0 mutation root repo snapshot first_sha large_sha first_link second_link
    local first_target second_target log pid observed_temp status first_type first_actual_sha
    for mutation in foreign-replace same-inode-write; do
        root="${TEST_ROOT}/round13-${mutation}"
        repo="${root}/models--Example--${mutation}"
        snapshot="${repo}/snapshots/4444444444444444444444444444444444444444"
        mkdir -p "${repo}/blobs" "${snapshot}"
        printf 'round13 exact first blob bytes\n' > "${root}/first.blob"
        first_sha="$(sha256sum "${root}/first.blob" | cut -d' ' -f1)"
        mv "${root}/first.blob" "${repo}/blobs/${first_sha}"
        dd if=/dev/zero of="${root}/large.blob" bs=1M count=512 status=none
        large_sha="$(sha256sum "${root}/large.blob" | cut -d' ' -f1)"
        mv "${root}/large.blob" "${repo}/blobs/${large_sha}"
        first_link="${snapshot}/a-first.onnx"
        second_link="${snapshot}/z-second.onnx"
        first_target="../../blobs/${first_sha}"
        second_target="../../blobs/${large_sha}"
        ln -s "${first_target}" "${first_link}"
        ln -s "${second_target}" "${second_link}"
        if [[ "${mutation}" == foreign-replace ]]; then
            printf 'round13 foreign sentinel bytes\n' > "${snapshot}/foreign.sentinel"
        fi
        chmod 0555 "${snapshot}"

        log="${TEST_ROOT}/round13-${mutation}.log"
        set +e
        "${ARTIFACT_SCRIPT}" materialize-model-links --root "${root}" > "${log}" 2>&1 &
        pid=$!
        set -e
        observed_temp=0
        for _ in $(seq 1 5000); do
            if [[ -e "${second_link}.pensyve-materialize" ]]; then observed_temp=1; break; fi
            kill -0 "${pid}" 2>/dev/null || break
            sleep 0.001
        done
        if [[ "${observed_temp}" -eq 1 && -f "${first_link}" && ! -L "${first_link}" ]]; then
            if [[ "${mutation}" == foreign-replace ]]; then
                mv "${snapshot}/foreign.sentinel" "${first_link}"
            else
                printf 'round13 same inode foreign bytes\n' > "${first_link}"
            fi
        else
            echo "ROUND13-RED ${mutation}: bounded race did not observe the installed first entry and second temporary" >&2
            failures=$((failures + 1))
        fi
        set +e
        wait "${pid}"
        status=$?
        set -e

        first_type="$([[ -L "${first_link}" ]] && printf symlink || printf regular)"
        first_actual_sha="$(sha256sum "${first_link}" | cut -d' ' -f1)"
        printf 'mutation=%s status=%s mode=%s first_type=%s first_sha=%s expected_sha=%s temp_count=%s\n' \
          "${mutation}" "${status}" "$(stat -c '%a' "${snapshot}")" "${first_type}" \
          "${first_actual_sha}" "${first_sha}" \
          "$(find "${snapshot}" \( -name '*.pensyve-materialize' -o -name '*.pensyve-rollback' \) | wc -l)" \
          | tee -a "${log}" >&2
        [[ "${status}" -ne 0 ]] \
          || { echo "ROUND13-RED ${mutation}: in-transaction content substitution returned success" >&2; failures=$((failures + 1)); }
        [[ "$(stat -c '%a' "${snapshot}")" == 555 ]] \
          || { echo "ROUND13-RED ${mutation}: snapshot mode was not restored" >&2; failures=$((failures + 1)); }
        [[ -L "${second_link}" && "$(readlink "${second_link}")" == "${second_target}" ]] \
          || { echo "ROUND13-RED ${mutation}: second link was not preserved" >&2; failures=$((failures + 1)); }
        if find "${snapshot}" \( -name '*.pensyve-materialize' -o -name '*.pensyve-rollback' \) | grep . >/dev/null; then
            echo "ROUND13-RED ${mutation}: transaction residue remains" >&2
            failures=$((failures + 1))
        fi
        if [[ "${mutation}" == foreign-replace ]]; then
            [[ -f "${first_link}" && ! -L "${first_link}" && "$(cat "${first_link}")" == 'round13 foreign sentinel bytes' ]] \
              || { echo "ROUND13-RED foreign-replace: foreign sentinel was deleted or overwritten" >&2; failures=$((failures + 1)); }
        else
            [[ -L "${first_link}" && "$(readlink "${first_link}")" == "${first_target}" ]] \
              || { echo "ROUND13-RED same-inode-write: owned installed entry was not rolled back" >&2; failures=$((failures + 1)); }
        fi
        chmod 0755 "${snapshot}"
    done

    # Integrity is tested only through materialization commit; the exclusive next seal/replay owns post-return changes.
    [[ "${failures}" -eq 0 ]] || fail "round13 in-transaction content integrity contracts failed: ${failures}"
    echo "round13 in-transaction content integrity contract passed"
}

run_round14_review() {
    require_scripts
    local fixture="${TEST_ROOT}/round14-disk" input output failures=0
    mkdir -p "${fixture}"

    for case_name in first-higher later-higher-and-lower; do
        input="${fixture}/${case_name}.json"
        output="${fixture}/${case_name}-result.json"
        case "${case_name}" in
            first-higher)
                availability='[1207970713600,1207970709504,1207970709504,1207970709504,1207970709504]'
                expected=1207970709504
                ;;
            later-higher-and-lower)
                availability='[1207970713600,1207970717696,1207970709504,1207970713600,1207970717696]'
                expected=1207970709504
                ;;
        esac
        jq -n --argjson availability "${availability}" '
          {filesystems:(["workspace","cargo","model_scratch","docker","tmp"] | to_entries |
            map({name:.value,path:"/tmp",device:"/dev/nvme0n1p2",
                 available_bytes:$availability[.key],required_bytes:1}))}' > "${input}"
        if ! "${ARTIFACT_SCRIPT}" disk-precheck --input "${input}" --output "${output}"; then
            echo "ROUND14-RED ${case_name}: conservative same-device observations were rejected" >&2
            failures=$((failures + 1))
        elif [[ "$(jq -r '.available_bytes_by_device["/dev/nvme0n1p2"]' "${output}")" != "${expected}" ||
                "$(jq -r '.required_bytes_by_device["/dev/nvme0n1p2"]' "${output}")" != 5 ]]; then
            echo "ROUND14-RED ${case_name}: output did not bind the minimum availability and summed demand" >&2
            failures=$((failures + 1))
        fi
    done

    jq '.filesystems[2].available_bytes=4' "${fixture}/later-higher-and-lower.json" \
      > "${fixture}/insufficient-lowest.json"
    if "${ARTIFACT_SCRIPT}" disk-precheck --input "${fixture}/insufficient-lowest.json" \
      --output "${fixture}/insufficient-lowest-result.json" > "${fixture}/insufficient-lowest.log" 2>&1; then
        echo "ROUND14-RED insufficient-lowest: a lowest observation below aggregate demand passed" >&2
        failures=$((failures + 1))
    elif ! grep -F 'runner disk is insufficient on filesystem /dev/nvme0n1p2' \
      "${fixture}/insufficient-lowest.log" >/dev/null; then
        cat "${fixture}/insufficient-lowest.log" >&2
        echo "ROUND14-RED insufficient-lowest: rejection did not use the conservative minimum" >&2
        failures=$((failures + 1))
    fi

    [[ "${failures}" -eq 0 ]] || fail "round14 conservative same-device disk precheck contracts failed: ${failures}"
    echo "round14 conservative same-device disk precheck contract passed"
}

run_round15_review() {
    require_scripts
    local fixture="${TEST_ROOT}/round15-shell-feedback" failures=0
    mkdir -p "${fixture}/bin"

    if ! validate_workflow "${WORKFLOW}" > "${fixture}/workflow.log" 2>&1; then
        cat "${fixture}/workflow.log" >&2
        echo "ROUND15-RED workflow: non-draft build authority or safe custodian input binding is absent" >&2
        failures=$((failures + 1))
    fi

    local source_fixture="${fixture}/source"
    make_local_fixture "${source_fixture}"
    if ! "${ARTIFACT_SCRIPT}" verify-local --tuple "${source_fixture}/tuple.json" \
      > "${fixture}/non-draft-source.log" 2>&1; then
        cat "${fixture}/non-draft-source.log" >&2
        echo "ROUND15-RED source: an open non-draft artifact-build tuple was rejected" >&2
        failures=$((failures + 1))
    fi

    local prefix_root="${fixture}/verify.[literal]"
    mkdir -p "${prefix_root}/models--Alibaba-NLP--gte-base-en-v1.5/refs"
    printf 'extra\n' > "${prefix_root}/models--Alibaba-NLP--gte-base-en-v1.5/refs/extra"
    capture_failure "${fixture}/literal-prefix.log" "${FETCH_SCRIPT}" --verify-only "${prefix_root}"
    if ! grep -F 'extra ref rejected: models--Alibaba-NLP--gte-base-en-v1.5/refs/extra' \
      "${fixture}/literal-prefix.log" >/dev/null; then
        echo "ROUND15-RED prefix: verifier did not strip a literal glob-bearing root" >&2
        failures=$((failures + 1))
    fi

    local model_fixture="${fixture}/model-fixture"
    local gte_repo="models--Alibaba-NLP--gte-base-en-v1.5"
    local gte_revision="a829fd0e060bb84554da0dfd354d0de0f7712b7f"
    local model_cache_path tokenizer_cache_path
    model_cache_path="$(awk -v snapshot="${gte_repo}/snapshots/${gte_revision}/onnx/model.onnx" '$1=="blob" && $5==snapshot {print $4}' "${REPO_ROOT}/pensyve-mcp-gateway/models/manifest.sha256")"
    tokenizer_cache_path="$(awk -v snapshot="${gte_repo}/snapshots/${gte_revision}/tokenizer.json" '$1=="blob" && $5==snapshot {print $4}' "${REPO_ROOT}/pensyve-mcp-gateway/models/manifest.sha256")"
    mkdir -p "${model_fixture}/${gte_repo}/snapshots/${gte_revision}/onnx" \
      "$(dirname -- "${model_fixture}/${model_cache_path}")" \
      "$(dirname -- "${model_fixture}/${tokenizer_cache_path}")"
    printf 'model\n' > "${model_fixture}/${model_cache_path}"
    printf 'tokenizer\n' > "${model_fixture}/${tokenizer_cache_path}"
    ln -s "$(realpath --relative-to="${model_fixture}/${gte_repo}/snapshots/${gte_revision}/onnx" "${model_fixture}/${model_cache_path}")" \
      "${model_fixture}/${gte_repo}/snapshots/${gte_revision}/onnx/model.onnx"
    ln -s "$(realpath --relative-to="${model_fixture}/${gte_repo}/snapshots/${gte_revision}" "${model_fixture}/${tokenizer_cache_path}")" \
      "${model_fixture}/${gte_repo}/snapshots/${gte_revision}/tokenizer.json"
    for model_case in missing-blob lfs-pointer; do
        if ! "${MODEL_TEST}" "${model_fixture}" "${model_case}" > "${fixture}/model-${model_case}.log" 2>&1; then
            cat "${fixture}/model-${model_case}.log" >&2
            echo "ROUND15-RED model-${model_case}: literal-root expected path drifted" >&2
            failures=$((failures + 1))
        fi
    done

    local fetch_copy="${fixture}/fetch-copy"
    mkdir -p "${fetch_copy}/scripts" "${fetch_copy}/models"
    cp -- "${FETCH_SCRIPT}" "${fetch_copy}/scripts/fetch-model-bundle.sh"
    cp -- "${REPO_ROOT}/pensyve-mcp-gateway/models/manifest.sha256" \
      "${REPO_ROOT}/pensyve-mcp-gateway/models/revisions.env" "${fetch_copy}/models/"
    local fetch_bin="${fixture}/fetch-bin"
    local fetch_driver="${fetch_copy}/scripts/fetch-guard-driver.sh"
    mkdir -p "${fetch_bin}"
    export STUB_LOG="${fixture}/fetch-stub.log"
    write_stub "${fetch_bin}/curl" 'touch "${CURL_MARKER}"; exit 97'
    write_stub "${fetch_bin}/mkdir" 'touch "${WRITE_MARKER}"; exit 97'
    python3 - "${fetch_copy}/scripts/fetch-model-bundle.sh" "${fetch_driver}" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text()
prefix = source.split("\nfetch_bundle() {", 1)[0]
driver = r'''
mode="$1"
root="$2"
status=0
case "${mode}" in
    validate) validate_manifest_paths || status=$? ;;
    license) download_licenses "${root}" || status=$? ;;
    model)
        download_model "${root}" "${GTE_REPOSITORY}" "${GTE_CACHE_REPOSITORY}" \
            "${GTE_REVISION}" || status=$?
        ;;
    *) exit 98 ;;
esac
if [[ "${status}" -eq 0 ]]; then
    touch "${CONTINUATION_MARKER}"
fi
exit "${status}"
'''
Path(sys.argv[2]).write_text(prefix + driver)
PY
    chmod +x "${fetch_driver}"
    for unsafe_case in blob-cache snapshot snapshot-nonprefix license-cache; do
        cp -- "${REPO_ROOT}/pensyve-mcp-gateway/models/manifest.sha256" "${fetch_copy}/models/manifest.sha256"
        python3 - "${fetch_copy}/models/manifest.sha256" "${unsafe_case}" <<'PY'
from pathlib import Path
import sys

path, mode = Path(sys.argv[1]), sys.argv[2]
rows = path.read_text().splitlines()
for index, row in enumerate(rows):
    fields = row.split()
    if mode in {"blob-cache", "snapshot", "snapshot-nonprefix"} and fields and fields[0] == "blob":
        if mode == "blob-cache":
            fields[3] = "../round15-escape"
        elif mode == "snapshot":
            fields[4] = "/".join(fields[4].split("/")[:3] + ["..", "round15-escape"])
        else:
            fields[4] = "../round15-escape"
        rows[index] = " ".join(fields)
        break
    if mode == "license-cache" and fields and fields[0] == "license-file":
        fields[3] = "../round15-license-escape"
        rows[index] = " ".join(fields)
        break
path.write_text("\n".join(rows) + "\n")
PY
        local curl_marker="${fixture}/${unsafe_case}-curl-called"
        local write_marker="${fixture}/${unsafe_case}-write-called"
        local continuation_marker="${fixture}/${unsafe_case}-continued"
        rm -f -- "${curl_marker}" "${write_marker}" "${continuation_marker}"
        : > "${STUB_LOG}"
        local unsafe_log="${fixture}/unsafe-${unsafe_case}.log"
        capture_failure "${unsafe_log}" env PATH="${fetch_bin}:${PATH}" \
          CURL_MARKER="${curl_marker}" WRITE_MARKER="${write_marker}" \
          "${fetch_copy}/scripts/fetch-model-bundle.sh" --output "${fixture}/unsafe-output-${unsafe_case}"
        if [[ -e "${curl_marker}" ]]; then
            echo "ROUND15-RED ${unsafe_case}: unsafe manifest data reached curl" >&2
            failures=$((failures + 1))
        fi

        local direct_root="${fixture}/direct-${unsafe_case}"
        mkdir -p "${direct_root}/${gte_repo}/refs"
        local guard_mode=validate expected_error="unsafe cache path"
        if [[ "${unsafe_case}" == snapshot || "${unsafe_case}" == snapshot-nonprefix ]]; then
            expected_error="unsafe snapshot path"
        fi
        for guard_mode in validate direct; do
            local driver_mode=validate
            if [[ "${guard_mode}" == direct ]]; then
                driver_mode=model
                if [[ "${unsafe_case}" == license-cache ]]; then
                    driver_mode=license
                    expected_error="unsafe license cache path"
                fi
            fi
            rm -f -- "${curl_marker}" "${write_marker}" "${continuation_marker}"
            set +e
            env PATH="${fetch_bin}:${PATH}" STUB_LOG="${STUB_LOG}" \
              CURL_MARKER="${curl_marker}" WRITE_MARKER="${write_marker}" \
              CONTINUATION_MARKER="${continuation_marker}" \
              bash "${fetch_driver}" "${driver_mode}" "${direct_root}" \
              > "${fixture}/${unsafe_case}-${guard_mode}.log" 2>&1
            local guard_status=$?
            set -e
            if [[ "${guard_status}" -eq 0 ]]; then
                echo "ROUND16-RED ${unsafe_case}-${guard_mode}: invalid-path guard returned zero" >&2
                failures=$((failures + 1))
            fi
            if [[ -e "${curl_marker}" || -e "${write_marker}" || -e "${continuation_marker}" ]]; then
                echo "ROUND16-RED ${unsafe_case}-${guard_mode}: invalid-path guard continued" >&2
                failures=$((failures + 1))
            fi
            if [[ "${unsafe_case}" == snapshot-nonprefix && "${guard_mode}" == direct \
              && -e "${direct_root}/${gte_repo}/refs/main" ]]; then
                echo "ROUND16-RED ${unsafe_case}-${guard_mode}: refs/main was written before validation" >&2
                failures=$((failures + 1))
            fi
            if ! grep -F -- "${expected_error}" "${fixture}/${unsafe_case}-${guard_mode}.log" >/dev/null; then
                echo "ROUND16-RED ${unsafe_case}-${guard_mode}: invalid-path error was not named" >&2
                failures=$((failures + 1))
            fi
        done
    done

    write_stub "${fetch_bin}/mktemp" 'exit 95'
    set +e
    env PATH="${fetch_bin}:${PATH}" STUB_LOG="${STUB_LOG}" \
      CURL_MARKER="${fixture}/model-test-curl" WRITE_MARKER="${fixture}/model-test-write" \
      "${MODEL_TEST}" "${model_fixture}" missing-blob > "${fixture}/model-test-mktemp.log" 2>&1
    local model_mktemp_status=$?
    set -e
    if [[ "${model_mktemp_status}" -eq 0 ]] \
      || ! grep -F 'could not create model-bundle test root' "${fixture}/model-test-mktemp.log" >/dev/null; then
        echo "ROUND16-RED model-test mktemp: failure was masked or unnamed" >&2
        failures=$((failures + 1))
    fi
    rm -f -- "${fetch_bin}/mktemp"

    export STUB_LOG="${fixture}/registry-stub.log" CURL_STATE="${fixture}/registry-curl-count"
    : > "${STUB_LOG}"
    write_stub "${fixture}/bin/git" '
if [[ "$*" == *"rev-parse HEAD"* ]]; then printf "%s\n" "${SOURCE_SHA}"; fi
exit 0'
    write_stub "${fixture}/bin/cargo" 'exit 0'
    write_stub "${fixture}/bin/uname" 'printf "arm64\n"'
    write_stub "${fixture}/bin/sleep" 'exit 0'
    write_stub "${fixture}/bin/docker" '
case "${1:-}" in
  save)
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == --output ]]; then printf "archive\n" > "$2"; break; fi
      shift
    done ;;
  image)
    case "$*" in
      *"{{.Id}}"*) printf "sha256:%064d\n" 0 ;;
      *"{{.Architecture}}"*) printf "arm64\n" ;;
      *"org.opencontainers.image.revision"*) printf "%s\n" "${SOURCE_SHA}" ;;
      *"{{.Config.User}}"*) printf "1001:1001\n" ;;
      *"{{.Config.StopSignal}}"*) printf "SIGINT\n" ;;
      *"{{.Size}}"*) printf "123\n" ;;
      *) printf "{}\n" ;;
    esac ;;
  run) printf "round15-registry\n" ;;
  port) printf "127.0.0.1:5001\n" ;;
  push) printf "pushed\n" ;;
esac
exit 0'
    write_stub "${fixture}/bin/curl" '
url="${!#}"
if [[ "$url" == "http://127.0.0.1:5001/v2/" ]]; then
  count=0; [[ ! -f "${CURL_STATE}" ]] || count=$(cat "${CURL_STATE}")
  count=$((count + 1)); printf "%s\n" "$count" > "${CURL_STATE}"
  [[ "$count" -ge 3 ]]
  exit
fi
exit 88'
    capture_failure "${fixture}/registry-build.log" env PATH="${fixture}/bin:${PATH}" \
      SOURCE_SHA="${SOURCE_SHA}" STUB_LOG="${STUB_LOG}" CURL_STATE="${CURL_STATE}" \
      "${ARTIFACT_SCRIPT}" build --source-sha "${SOURCE_SHA}" \
      --archive "${fixture}/registry-image.tar" --evidence-dir "${fixture}/registry-evidence" \
      --image-ref "pensyve-gateway:${SOURCE_SHA}"
    if [[ "$(call_count "${STUB_LOG}" curl 'http://127.0.0.1:5001/v2/')" -ne 3 ]]; then
        echo "ROUND15-RED registry: bounded /v2/ readiness did not precede the single push" >&2
        failures=$((failures + 1))
    fi

    local deployment_fixture="${fixture}/deployment"
    make_local_fixture "${deployment_fixture}"
    make_reviewed_tuple_and_request "${deployment_fixture}/tuple.json" \
      "${deployment_fixture}/reviewed.json" "${deployment_fixture}/request.json"
    if "${ARTIFACT_SCRIPT}" fetch-verify --tuple "${deployment_fixture}/reviewed.json" \
      --request "${deployment_fixture}/request.json" --output "${deployment_fixture}/verified.json"; then
        jq '.deployment.baseline_image="123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:157bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"' \
          "${deployment_fixture}/verified.json" > "${deployment_fixture}/digest-prefix.json"
        if ! "${ARTIFACT_SCRIPT}" verify-handoff --input "${deployment_fixture}/digest-prefix.json" \
          > "${fixture}/digest-prefix.log" 2>&1; then
            cat "${fixture}/digest-prefix.log" >&2
            echo "ROUND15-RED digest: valid immutable digest prefix was rejected as a task revision" >&2
            failures=$((failures + 1))
        fi
    else
        echo "ROUND15-RED digest: deployment fixture could not be sealed" >&2
        failures=$((failures + 1))
    fi

    local promotion_verified="${fixture}/promotion-verified.json" environment_sha snapshot_sha
    printf '[{"name":"MCP_ALLOWED_HOSTS","value":"mcp.pensyve.com"}]\n' > "${fixture}/environment.json"
    environment_sha="$(jq -S -c . "${fixture}/environment.json" | sha256sum | cut -d' ' -f1)"
    jq -n '{service_name:"pensyve-prod-gateway",status:"ACTIVE",
      cluster_arn:"arn:aws:ecs:us-east-2:123456789012:cluster/pensyve-prod",
      task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",
      counts:{desired:2,running:2,pending:0},
      network_configuration:{awsvpcConfiguration:{subnets:["subnet-aaa","subnet-bbb"],securityGroups:["sg-aaa"],assignPublicIp:"DISABLED"}},
      load_balancers:[{targetGroupArn:"arn:aws:elasticloadbalancing:us-east-2:123456789012:targetgroup/pensyve-gateway/abc",containerName:"gateway",containerPort:3100}],
      deployment_configuration:{deploymentCircuitBreaker:{enable:true,rollback:true},maximumPercent:200,minimumHealthyPercent:100},
      health_grace_period_seconds:300,
      primary_deployment:{status:"PRIMARY",task_definition:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",rollout_state:"COMPLETED",desired:2,running:2,pending:0}}' \
      > "${fixture}/service-snapshot.json"
    snapshot_sha="$(jq -S -c . "${fixture}/service-snapshot.json" | sha256sum | cut -d' ' -f1)"
    jq --arg env_sha "${environment_sha}" --arg snapshot_sha "${snapshot_sha}" \
      --slurpfile snapshot "${fixture}/service-snapshot.json" \
      '{schema_version:1,cleanup_required:false,image:.image,scanner:.scanner,scan:.scan,
        deployment:{region:"us-east-2",ecr_registry:"123456789012.dkr.ecr.us-east-2.amazonaws.com",ecr_repository:"pensyve-gateway",cluster:"pensyve-prod",service:"pensyve-prod-gateway",gateway_container:"gateway",baseline_task_definition_arn:"arn:aws:ecs:us-east-2:123456789012:task-definition/pensyve-prod-gateway:200",baseline_image:"123456789012.dkr.ecr.us-east-2.amazonaws.com/pensyve-gateway@sha256:157bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",baseline_environment_sha256:$env_sha,baseline_service_snapshot:$snapshot[0],baseline_service_snapshot_sha256:$snapshot_sha,probe_entity:"task9-runtime-1234-2-0123456789abcdef",promotion_run_id:1234,promotion_run_attempt:2,cpu:"512",memory:"4096",desired_count:2,running_count:2,pending_count:0}}' \
      "${deployment_fixture}/tuple.json" > "${promotion_verified}"
    export STUB_LOG="${fixture}/promotion-stub.log"
    : > "${STUB_LOG}"
    write_stub "${fixture}/bin/aws" 'exit 91'
    write_stub "${fixture}/bin/docker" 'exit 92'
    write_stub "${fixture}/bin/curl" 'exit 93'
    write_stub "${fixture}/bin/sleep" 'exit 94'
    capture_failure "${fixture}/promotion-digest.log" env DOCKER_BIN="${fixture}/bin/docker" \
      AWS_BIN="${fixture}/bin/aws" CURL_BIN="${fixture}/bin/curl" SLEEP_BIN="${fixture}/bin/sleep" \
      STUB_LOG="${STUB_LOG}" "${PROMOTE_SCRIPT}" "${promotion_verified}"
    if [[ "$(call_count "${STUB_LOG}" docker load)" -ne 1 ]]; then
        cat "${fixture}/promotion-digest.log" >&2
        echo "ROUND15-RED promotion digest: valid digest prefix did not reach promotion execution" >&2
        failures=$((failures + 1))
    fi

    write_stub "${fixture}/bin/mktemp" 'exit 95'
    : > "${STUB_LOG}"
    capture_failure "${fixture}/mktemp.log" env PATH="${fixture}/bin:${PATH}" \
      DOCKER_BIN="${fixture}/bin/docker" AWS_BIN="${fixture}/bin/aws" \
      CURL_BIN="${fixture}/bin/curl" SLEEP_BIN="${fixture}/bin/sleep" STUB_LOG="${STUB_LOG}" \
      "${PROMOTE_SCRIPT}" "${promotion_verified}"
    if ! grep -F 'could not create the promotion temporary root' "${fixture}/mktemp.log" >/dev/null; then
        echo "ROUND15-RED mktemp: promotion did not fail explicitly at temporary-root creation" >&2
        failures=$((failures + 1))
    fi
    rm -f -- "${fixture}/bin/mktemp"

    export STUB_LOG="${fixture}/relative-stub.log" STUB_CACHE_ABS="${fixture}/relative/cache"
    : > "${STUB_LOG}"
    mkdir -p "${fixture}/relative"
    write_stub "${fixture}/bin/docker" '
if [[ " $* " == *" version "* ]]; then printf "Version: 0.74.0\n"; fi
if [[ " $* " == *" --download-db-only "* ]]; then
  mkdir -p "${STUB_CACHE_ABS}/db"
  printf "{\"UpdatedAt\":\"2026-08-30T00:00:00Z\",\"DownloadedAt\":\"2026-08-30T00:00:00Z\"}\n" > "${STUB_CACHE_ABS}/db/metadata.json"
  printf "db\n" > "${STUB_CACHE_ABS}/db/trivy.db"
fi
exit 0'
    (
      cd "${fixture}/relative"
      PATH="${fixture}/bin:${PATH}" STUB_LOG="${STUB_LOG}" STUB_CACHE_ABS="${STUB_CACHE_ABS}" \
        "${RELEASE_SCRIPT}" prepare-trivy --cache-dir cache --evidence-dir evidence
    )
    if [[ "$(call_count "${STUB_LOG}" docker "type=bind,src=${STUB_CACHE_ABS},dst=/trivy-cache")" -ne 1 ]]; then
        echo "ROUND15-RED relative paths: Trivy bind source was not canonicalized" >&2
        failures=$((failures + 1))
    fi

    python3 - "${RELEASE_SCRIPT}" "${fixture}/missing-model-driver.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text()
prefix = source.split("\nrun_trivy() {", 1)[0]
Path(sys.argv[2]).write_text(prefix + '\nrun_missing_model_failure "sha256:' + ('a' * 64) + '" "base:test" "$1"\n')
PY
    chmod +x "${fixture}/missing-model-driver.sh"
    export STUB_LOG="${fixture}/mutation-cleanup-stub.log"
    : > "${STUB_LOG}"
    write_stub "${fixture}/bin/docker" '
if [[ "${1:-}" == build ]]; then exit 0; fi
if [[ "${1:-}" == run ]]; then exit 96; fi
exit 0'
    capture_failure "${fixture}/mutation-cleanup.log" env PATH="${fixture}/bin:${PATH}" \
      STUB_LOG="${STUB_LOG}" bash "${fixture}/missing-model-driver.sh" "${fixture}/missing-model-evidence"
    if [[ "$(call_count "${STUB_LOG}" docker image)" -lt 1 ]]; then
        echo "ROUND15-RED mutation cleanup: derived image was not removed by EXIT cleanup" >&2
        failures=$((failures + 1))
    fi

    [[ "${failures}" -eq 0 ]] || fail "round15 shell/workflow review contracts failed: ${failures}"
    echo "round15 shell/workflow review contract passed"
}

if [[ "${CASE}" == "structural" || "${CASE}" == "all" ]]; then run_structural; fi
if [[ "${CASE}" == "artifact" || "${CASE}" == "all" ]]; then run_artifact; fi
if [[ "${CASE}" == "seal" || "${CASE}" == "all" ]]; then run_seal; fi
if [[ "${CASE}" == "storage" || "${CASE}" == "all" ]]; then run_storage; fi
if [[ "${CASE}" == "reviewed-pr" || "${CASE}" == "all" ]]; then run_reviewed_pr; fi
if [[ "${CASE}" == "deployment" || "${CASE}" == "all" ]]; then run_deployment; fi
if [[ "${CASE}" == "release-scan" || "${CASE}" == "all" ]]; then run_release_scan; fi
if [[ "${CASE}" == "promote" || "${CASE}" == "all" ]]; then run_promote; fi
if [[ "${CASE}" == "cleanup" || "${CASE}" == "all" ]]; then run_cleanup; fi
if [[ "${CASE}" == "handoff" || "${CASE}" == "all" ]]; then run_handoff; fi
if [[ "${CASE}" == "round4-review" || "${CASE}" == "all" ]]; then run_round4_review; fi
if [[ "${CASE}" == "round5-review" || "${CASE}" == "all" ]]; then run_round5_review; fi
if [[ "${CASE}" == "round6-review" || "${CASE}" == "all" ]]; then run_round6_review; fi
if [[ "${CASE}" == "round9-review" || "${CASE}" == "all" ]]; then run_round9_review; fi
if [[ "${CASE}" == "round10-review" || "${CASE}" == "all" ]]; then run_round10_review; fi
if [[ "${CASE}" == "round11-review" || "${CASE}" == "all" ]]; then run_round11_review; fi
if [[ "${CASE}" == "round12-review" || "${CASE}" == "all" ]]; then run_round12_review; fi
if [[ "${CASE}" == "round13-review" || "${CASE}" == "all" ]]; then run_round13_review; fi
if [[ "${CASE}" == "round14-review" || "${CASE}" == "all" ]]; then run_round14_review; fi
if [[ "${CASE}" == "round15-review" || "${CASE}" == "all" ]]; then run_round15_review; fi

echo "gateway artifact flow tests passed (${CASE})"
