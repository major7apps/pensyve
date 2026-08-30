#!/usr/bin/env bash

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly MODEL_TEST="${SCRIPT_DIR}/test-model-bundle.sh"
readonly ARTIFACT_SCRIPT="${SCRIPT_DIR}/gateway-image-artifact.sh"
readonly GTE_REVISION="a829fd0e060bb84554da0dfd354d0de0f7712b7f"
readonly BGE_REVISION="2cfc18c9415c912f9d8155881c133215df768a70"
readonly TRIVY_VERSION="0.74.0"
readonly TRIVY_IMAGE_DIGEST="sha256:55ad20f8a239a3e95427e60b8aaea38788550c18a3f1772976bebf732e6ae166"
readonly TRIVY_IMAGE="ghcr.io/aquasecurity/trivy@${TRIVY_IMAGE_DIGEST}"
readonly MEMORY_LIMIT_BYTES="4294967296"
ACTIVE_CONTAINER=""
ACTIVE_MUTATION_IMAGE=""

cleanup_active_resources() {
    if [[ -n "${ACTIVE_CONTAINER}" ]]; then
        docker rm -f "${ACTIVE_CONTAINER}" >/dev/null 2>&1 || true
    fi
    if [[ -n "${ACTIVE_MUTATION_IMAGE}" ]]; then
        docker image rm -f "${ACTIVE_MUTATION_IMAGE}" >/dev/null 2>&1 || true
    fi
}
trap cleanup_active_resources EXIT

die() {
    echo "gateway release image error: $*" >&2
    exit 1
}

need_arg() {
    [[ -n "${2:-}" ]] || die "missing value for $1"
}

sha256_file() {
    sha256sum "$1" | cut -d' ' -f1
}

prepare_trivy() {
    local cache_dir="$1" evidence_dir="$2"
    mkdir -p -- "${cache_dir}" "${evidence_dir}"
    cache_dir="$(realpath -- "${cache_dir}")"
    evidence_dir="$(realpath -- "${evidence_dir}")"
    docker pull "${TRIVY_IMAGE}" > "${evidence_dir}/trivy-pull.log"
    docker image inspect "${TRIVY_IMAGE}" > "${evidence_dir}/trivy-image-inspect.json"
    docker run --rm "${TRIVY_IMAGE}" version > "${evidence_dir}/trivy-version.txt"
    grep -F "Version: ${TRIVY_VERSION}" "${evidence_dir}/trivy-version.txt" >/dev/null \
        || die "pinned Trivy container did not report version ${TRIVY_VERSION}"
    date -u +'%Y-%m-%dT%H:%M:%SZ' > "${evidence_dir}/db-download-started-at.txt"
    docker run --rm \
        --user "$(id -u):$(id -g)" \
        --env TRIVY_CACHE_DIR=/trivy-cache \
        --mount "type=bind,src=${cache_dir},dst=/trivy-cache" \
        "${TRIVY_IMAGE}" image --download-db-only --no-progress \
        > "${evidence_dir}/db-update.log" 2>&1
    date -u +'%Y-%m-%dT%H:%M:%SZ' > "${evidence_dir}/db-downloaded-at.txt"
    local metadata db_file
    metadata="$(find "${cache_dir}" -type f -path '*/db/metadata.json' -print -quit)"
    db_file="$(find "${cache_dir}" -type f -path '*/db/trivy.db' -print -quit)"
    [[ -n "${metadata}" && -n "${db_file}" ]] || die "Trivy DB update did not produce metadata and trivy.db"
    cp -- "${metadata}" "${evidence_dir}/db-metadata.json"
    sha256sum "${db_file}" > "${evidence_dir}/trivy.db.sha256"
    printf '%s\n' "${TRIVY_IMAGE}" > "${evidence_dir}/scanner-pin.txt"
    # Trivy does not expose the pulled database OCI digest in this mode. Keep
    # that limitation explicit; the DB file hash and timestamps remain bound.
    printf '%s\n' "unavailable" > "${evidence_dir}/db-oci-digest.txt"
}

wait_healthy() {
    local container="$1" run_dir="$2" start_ns="$3"
    local healthy=0 attempt state
    : > "${run_dir}/health-attempts.tsv"
    for attempt in $(seq 1 240); do
        state="$(docker inspect --format '{{.State.Status}}' "${container}")"
        printf '%s\t%s\t%s\n' "$(date -u +'%Y-%m-%dT%H:%M:%S.%NZ')" "${attempt}" "${state}" \
            >> "${run_dir}/health-attempts.tsv"
        if [[ "${state}" == exited || "${state}" == dead ]]; then break; fi
        if docker exec "${container}" curl -fsS http://127.0.0.1:3000/health \
            > "${run_dir}/health-response.json" 2> "${run_dir}/health-last-error.txt"; then
            healthy=1
            break
        fi
        sleep 1
    done
    local health_ms
    health_ms=$(( ($(date +%s%N) - start_ns) / 1000000 ))
    printf '%s\n' "${health_ms}" > "${run_dir}/health-elapsed-ms.txt"
    [[ "${healthy}" -eq 1 && "${health_ms}" -le 240000 ]] || die "strict container did not become healthy within 240 seconds"
}

assert_clean_logs() {
    local log="$1"
    grep -F 'model runtime initialized' "${log}" >/dev/null
    grep -F "${GTE_REVISION}" "${log}" >/dev/null
    grep -F "${BGE_REVISION}" "${log}" >/dev/null
    if grep -Ei 'download|MiniLM|mock embedder|fallback|lazy.load' "${log}" >/dev/null; then
        die "strict startup emitted a forbidden network/model fallback warning"
    fi
}

default_stop() {
    local container="$1" run_dir="$2" events_cgroup_dir="$3" events_initial="$4"
    printf 'docker stop %q\n' "${container}" > "${run_dir}/default-stop-command.txt"
    docker stop "${container}" > "${run_dir}/default-stop-output.txt"
    [[ -d "${events_cgroup_dir}" ]] || die "persistent evidence parent cgroup disappeared after default docker stop"
    cat "${events_cgroup_dir}/memory.events" > "${run_dir}/memory.events.post-stop.txt"
    local event before after
    : > "${run_dir}/memory.events.post-stop-deltas.txt"
    for event in oom oom_kill oom_group_kill; do
        before="$(awk -v event="${event}" '$1==event {print $2}' "${events_initial}")"
        after="$(awk -v event="${event}" '$1==event {print $2}' "${run_dir}/memory.events.post-stop.txt")"
        [[ "${before}" =~ ^[0-9]+$ && "${after}" =~ ^[0-9]+$ ]] \
            || die "post-stop memory.events is missing ${event}"
        printf '%s\t%s\t%s\t%s\n' "${event}" "${before}" "${after}" "$((after - before))" \
            >> "${run_dir}/memory.events.post-stop-deltas.txt"
        [[ "${after}" -eq "${before}" ]] || die "post-stop OOM-event delta is nonzero: ${event}"
    done
    docker inspect "${container}" > "${run_dir}/inspect-after-stop.json"
    local exit_code oom_killed
    exit_code="$(docker inspect --format '{{.State.ExitCode}}' "${container}")"
    oom_killed="$(docker inspect --format '{{.State.OOMKilled}}' "${container}")"
    printf '%s\n' "${exit_code}" > "${run_dir}/exit-code.txt"
    [[ "${exit_code}" == "0" && "${oom_killed}" == "false" ]] || die "default docker stop was not graceful"
}

run_one_sizing() {
    local image_id="$1" evidence_root="$2" run="$3"
    local run_dir="${evidence_root}/sizing/run-${run}"
    local container="pensyve-release-${$}-${run}"
    local cgroup_parent="pensyve-task4-release-${$}-${run}-${RANDOM}.slice"
    ACTIVE_CONTAINER="${container}"
    mkdir -p -- "${run_dir}"
    local start_ns container_id pid cgroup_rel cgroup_dir events_cgroup_dir idle_max peak
    start_ns="$(date +%s%N)"
    container_id="$(docker run -d --name "${container}" --network none --read-only --no-healthcheck \
        --cpus 0.5 --memory 4g --cgroup-parent "${cgroup_parent}" \
        --tmpfs /home/pensyve/data:rw,uid=1001,gid=1001,mode=0700 \
        -e PENSYVE_REQUIRE_LOCAL_MODELS=1 -e PENSYVE_EMBEDDING_POOL_SIZE=1 "${image_id}")"
    printf '%s\n' "${container_id}" > "${run_dir}/container-id.txt"
    pid=0
    for _ in $(seq 1 30); do
        pid="$(docker inspect --format '{{.State.Pid}}' "${container}")"
        [[ "${pid}" =~ ^[1-9][0-9]*$ ]] && break
        sleep 1
    done
    [[ "${pid}" =~ ^[1-9][0-9]*$ ]] || die "container host PID is unavailable"
    cat "/proc/${pid}/cgroup" > "${run_dir}/proc-cgroup.txt"
    cgroup_rel="$(awk -F: '$1 == "0" { print $3 }' "${run_dir}/proc-cgroup.txt")"
    cgroup_dir="/sys/fs/cgroup${cgroup_rel}"
    [[ -d "${cgroup_dir}" ]] || die "container cgroup-v2 path is unavailable"
    events_cgroup_dir="$(dirname -- "${cgroup_dir}")"
    [[ -f "${events_cgroup_dir}/memory.events" ]] || die "persistent evidence parent cgroup is unavailable"
    printf '%s\n' "${cgroup_dir}" > "${run_dir}/cgroup-path.txt"
    printf '%s\n' "${events_cgroup_dir}" > "${run_dir}/events-cgroup-path.txt"
    cat "${cgroup_dir}/cpu.max" > "${run_dir}/cpu.max.txt"
    cat "${cgroup_dir}/memory.max" > "${run_dir}/memory.max.txt"
    cat "${cgroup_dir}/memory.peak" > "${run_dir}/memory.peak.initial.txt"
    cat "${cgroup_dir}/memory.events" > "${run_dir}/memory.events.initial.txt"
    cat "${events_cgroup_dir}/memory.events" > "${run_dir}/memory.events.parent.initial.txt"
    [[ "$(cat "${run_dir}/cpu.max.txt")" == "50000 100000" ]] || die "cpu.max is not the exact authoritative 0.5 ratio"
    [[ "$(cat "${run_dir}/memory.max.txt")" == "${MEMORY_LIMIT_BYTES}" ]] || die "memory.max is not exactly 4294967296"
    wait_healthy "${container}" "${run_dir}" "${start_ns}"
    cat "${cgroup_dir}/memory.peak" > "${run_dir}/memory.peak.at-health.txt"
    printf 'sample\ttimestamp_utc\tmemory_current_bytes\n' > "${run_dir}/memory-current-1s.tsv"
    date -u +'%Y-%m-%dT%H:%M:%S.%NZ' > "${run_dir}/idle-window-start.txt"
    local sample
    for sample in $(seq 1 60); do
        sleep 1
        printf '%s\t%s\t%s\n' "${sample}" "$(date -u +'%Y-%m-%dT%H:%M:%S.%NZ')" \
            "$(cat "${cgroup_dir}/memory.current")" >> "${run_dir}/memory-current-1s.tsv"
    done
    date -u +'%Y-%m-%dT%H:%M:%S.%NZ' > "${run_dir}/idle-window-end.txt"
    cat "${cgroup_dir}/memory.peak" > "${run_dir}/memory.peak.final.txt"
    cat "${cgroup_dir}/memory.events" > "${run_dir}/memory.events.final.txt"
    idle_max="$(awk -F '\t' 'NR > 1 && $3 > max {max=$3} END {print max+0}' "${run_dir}/memory-current-1s.tsv")"
    peak="$(cat "${run_dir}/memory.peak.final.txt")"
    printf '%s\n' "${idle_max}" > "${run_dir}/idle-max-bytes.txt"
    awk -v n="${idle_max}" -v d="${MEMORY_LIMIT_BYTES}" 'BEGIN {printf "%.9f\n",n/d; exit !(n/d<0.70)}' > "${run_dir}/idle-max-ratio.txt"
    awk -v n="${peak}" -v d="${MEMORY_LIMIT_BYTES}" 'BEGIN {printf "%.9f\n",n/d; exit !(n/d<0.70)}' > "${run_dir}/peak-ratio.txt"
    [[ "$(awk '$1=="oom" {print $2}' "${run_dir}/memory.events.final.txt")" == "0" ]] || die "OOM event occurred"
    [[ "$(awk '$1=="oom_kill" {print $2}' "${run_dir}/memory.events.final.txt")" == "0" ]] || die "OOM kill occurred"
    [[ "$(awk '$1=="oom_group_kill" {print $2}' "${run_dir}/memory.events.final.txt")" == "0" ]] || die "OOM group kill occurred"
    [[ "$(awk '$1=="oom" {print $2}' "${run_dir}/memory.events.final.txt")" == \
       "$(awk '$1=="oom" {print $2}' "${run_dir}/memory.events.initial.txt")" ]] || die "OOM-event delta is nonzero"
    [[ "$(awk '$1=="oom_kill" {print $2}' "${run_dir}/memory.events.final.txt")" == \
       "$(awk '$1=="oom_kill" {print $2}' "${run_dir}/memory.events.initial.txt")" ]] || die "OOM-kill delta is nonzero"
    [[ "$(awk '$1=="oom_group_kill" {print $2}' "${run_dir}/memory.events.final.txt")" == \
       "$(awk '$1=="oom_group_kill" {print $2}' "${run_dir}/memory.events.initial.txt")" ]] || die "OOM-group-kill delta is nonzero"
    docker logs "${container}" > "${run_dir}/container.log" 2>&1
    docker inspect "${container}" > "${run_dir}/inspect-before-stop.json"
    assert_clean_logs "${run_dir}/container.log"
    default_stop "${container}" "${run_dir}" "${events_cgroup_dir}" \
        "${run_dir}/memory.events.parent.initial.txt"
    docker rm "${container}" > "${run_dir}/remove-output.txt"
    ACTIVE_CONTAINER=""
    printf 'run=%s health_ms=%s idle_max=%s idle_ratio=%s peak=%s peak_ratio=%s exit=0\n' \
        "${run}" "$(cat "${run_dir}/health-elapsed-ms.txt")" "${idle_max}" \
        "$(cat "${run_dir}/idle-max-ratio.txt")" "${peak}" "$(cat "${run_dir}/peak-ratio.txt")" \
        > "${run_dir}/summary.txt"
}

run_standalone_lifecycle() {
    local image_id="$1" evidence_dir="$2"
    local run_dir="${evidence_dir}/standalone-default-stop"
    local container="pensyve-release-${$}-standalone"
    local cgroup_parent="pensyve-task4-release-${$}-standalone-${RANDOM}.slice"
    ACTIVE_CONTAINER="${container}"
    mkdir -p -- "${run_dir}"
    local start_ns
    start_ns="$(date +%s%N)"
    docker run -d --name "${container}" --network none --read-only --no-healthcheck \
        --cpus 0.5 --memory 4g --cgroup-parent "${cgroup_parent}" \
        --tmpfs /home/pensyve/data:rw,uid=1001,gid=1001,mode=0700 \
        -e PENSYVE_REQUIRE_LOCAL_MODELS=1 -e PENSYVE_EMBEDDING_POOL_SIZE=1 "${image_id}" \
        > "${run_dir}/container-id.txt"
    wait_healthy "${container}" "${run_dir}" "${start_ns}"
    local pid cgroup_rel cgroup_dir events_cgroup_dir
    pid=0
    for _ in $(seq 1 30); do
        pid="$(docker inspect --format '{{.State.Pid}}' "${container}")"
        [[ "${pid}" =~ ^[1-9][0-9]*$ ]] && break
        sleep 1
    done
    [[ "${pid}" =~ ^[1-9][0-9]*$ ]] || die "standalone container host PID is unavailable"
    cat "/proc/${pid}/cgroup" > "${run_dir}/proc-cgroup.txt"
    cgroup_rel="$(awk -F: '$1 == "0" { print $3 }' "${run_dir}/proc-cgroup.txt")"
    cgroup_dir="/sys/fs/cgroup${cgroup_rel}"
    events_cgroup_dir="$(dirname -- "${cgroup_dir}")"
    [[ -f "${events_cgroup_dir}/memory.events" ]] || die "standalone persistent evidence parent cgroup is unavailable"
    printf '%s\n' "${events_cgroup_dir}" > "${run_dir}/events-cgroup-path.txt"
    cat "${events_cgroup_dir}/memory.events" > "${run_dir}/memory.events.parent.initial.txt"
    cat "${cgroup_dir}/memory.events" > "${run_dir}/memory.events.before-stop.txt"
    docker logs "${container}" > "${run_dir}/container.log" 2>&1
    docker inspect "${container}" > "${run_dir}/inspect-before-stop.json"
    assert_clean_logs "${run_dir}/container.log"
    [[ "$(awk '$1=="oom" {print $2}' "${run_dir}/memory.events.before-stop.txt")" == "0" ]] || die "standalone lifecycle recorded an OOM event"
    [[ "$(awk '$1=="oom_kill" {print $2}' "${run_dir}/memory.events.before-stop.txt")" == "0" ]] || die "standalone lifecycle recorded an OOM kill"
    [[ "$(awk '$1=="oom_group_kill" {print $2}' "${run_dir}/memory.events.before-stop.txt")" == "0" ]] || die "standalone lifecycle recorded an OOM group kill"
    default_stop "${container}" "${run_dir}" "${events_cgroup_dir}" \
        "${run_dir}/memory.events.parent.initial.txt"
    docker rm "${container}" > "${run_dir}/remove-output.txt"
    ACTIVE_CONTAINER=""
}

run_missing_model_failure() {
    local image_id="$1" base_ref="$2" evidence_dir="$3"
    local run_dir="${evidence_dir}/missing-model"
    local mutation_image="pensyve-gateway-missing-model:${image_id#sha256:}"
    local container="pensyve-release-${$}-missing"
    ACTIVE_CONTAINER="${container}"
    ACTIVE_MUTATION_IMAGE="${mutation_image}"
    mkdir -p -- "${run_dir}"
    docker build --build-arg "BASE_IMAGE=${base_ref}" --tag "${mutation_image}" --file - "${REPO_ROOT}" \
        > "${run_dir}/derived-image-build.log" 2>&1 <<'DOCKERFILE'
ARG BASE_IMAGE
FROM ${BASE_IMAGE}
USER root
RUN rm /opt/pensyve/models/models--BAAI--bge-reranker-base/blobs/15b9a8c3da82eddf263df571281166e00e9308fe19d077084b642ebfcaf06d2b
USER 1001:1001
DOCKERFILE
    docker run -d --name "${container}" --network none --read-only --no-healthcheck \
        --tmpfs /home/pensyve/data:rw,uid=1001,gid=1001,mode=0700 \
        -e PENSYVE_REQUIRE_LOCAL_MODELS=1 -e PENSYVE_EMBEDDING_POOL_SIZE=1 "${mutation_image}" \
        > "${run_dir}/container-id.txt"
    local state
    for _ in $(seq 1 120); do
        state="$(docker inspect --format '{{.State.Status}}' "${container}")"
        [[ "${state}" == exited || "${state}" == dead ]] && break
        sleep 1
    done
    docker logs "${container}" > "${run_dir}/container.log" 2>&1
    docker inspect "${container}" > "${run_dir}/inspect.json"
    [[ "$(docker inspect --format '{{.State.Status}}' "${container}")" == "exited" ]] || die "missing-model image did not fail startup"
    [[ "$(docker inspect --format '{{.State.ExitCode}}' "${container}")" != "0" ]] || die "missing-model startup returned zero"
    [[ "$(docker inspect --format '{{.State.OOMKilled}}' "${container}")" == "false" ]] || die "missing-model proof was OOM-killed"
    grep -F 'models--BAAI--bge-reranker-base' "${run_dir}/container.log" >/dev/null
    grep -F 'onnx/model.onnx' "${run_dir}/container.log" >/dev/null
    docker rm "${container}" > "${run_dir}/remove-output.txt"
    ACTIVE_CONTAINER=""
    docker image rm "${mutation_image}" > "${run_dir}/derived-image-remove.txt"
    ACTIVE_MUTATION_IMAGE=""
}

run_trivy() {
    local archive="$1" image_id="$2" cache_dir="$3" evidence_dir="$4"
    archive="$(realpath -- "${archive}")"
    cache_dir="$(realpath -- "${cache_dir}")"
    evidence_dir="$(realpath -- "${evidence_dir}")"
    local scan_dir="${evidence_dir}/trivy"
    mkdir -p -- "${scan_dir}"
    scan_dir="$(realpath -- "${scan_dir}")"
    local db_metadata db_file db_updated db_downloaded db_sha report report_sha policy_sha
    db_metadata="$(find "${cache_dir}" -type f -path '*/db/metadata.json' -print -quit)"
    db_file="$(find "${cache_dir}" -type f -path '*/db/trivy.db' -print -quit)"
    [[ -n "${db_metadata}" && -n "${db_file}" ]] || die "prepared Trivy DB is absent"
    db_updated="$(jq -r '.UpdatedAt // .updated_at // empty' "${db_metadata}")"
    [[ -n "${db_updated}" ]] || die "Trivy DB UpdatedAt is absent"
    db_downloaded="$(jq -r '.DownloadedAt // .downloaded_at // empty' "${db_metadata}")"
    [[ -n "${db_downloaded}" ]] || die "Trivy DB DownloadedAt is absent"
    db_sha="$(sha256_file "${db_file}")"
    report="${scan_dir}/scan-report.json"
    printf '%s\n' "trivy image --input ${archive} --offline-scan --skip-db-update --skip-check-update --scanners vuln,secret,misconfig --severity UNKNOWN,LOW,MEDIUM,HIGH,CRITICAL --exit-code 0 --format json --output ${report}" \
        > "${scan_dir}/scan-command.txt"
    docker run --rm --pull never --network none \
        --user "$(id -u):$(id -g)" \
        --env TRIVY_CACHE_DIR=/trivy-cache \
        --mount "type=bind,src=${cache_dir},dst=/trivy-cache" \
        --mount "type=bind,src=${archive},dst=${archive},readonly" \
        --mount "type=bind,src=${scan_dir},dst=${scan_dir}" \
        "${TRIVY_IMAGE}" image --input "${archive}" \
        --offline-scan --skip-db-update --skip-check-update \
        --scanners vuln,secret,misconfig --severity UNKNOWN,LOW,MEDIUM,HIGH,CRITICAL \
        --exit-code 0 --format json --output "${report}" \
        > "${scan_dir}/scan.log" 2>&1
    [[ "$(sha256_file "${db_file}")" == "${db_sha}" ]] \
        || die "Trivy DB changed during no-network scan"
    report_sha="$(sha256_file "${report}")"
    policy_sha="$(sha256_file "${ARTIFACT_SCRIPT}")"
    local scanned_at
    scanned_at="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    jq -n --arg archive "${archive}" --arg archive_sha "$(sha256_file "${archive}")" \
        --arg image_id "${image_id}" --arg scanner_digest "${TRIVY_IMAGE_DIGEST}" \
        --arg scanner_version "${TRIVY_VERSION}" --arg db_updated "${db_updated}" \
        --arg db_downloaded "${db_downloaded}" --arg db_sha "${db_sha}" --arg db_path "${db_file}" \
        --arg report "${report}" --arg report_sha "${report_sha}" \
        --arg scanned_at "${scanned_at}" --arg policy "${ARTIFACT_SCRIPT}" --arg policy_sha "${policy_sha}" \
        '{image:{archive_path:$archive,archive_sha256:$archive_sha,config_id:$image_id},
          scanner:{image_digest:$scanner_digest,version:$scanner_version,
            argv:["trivy","image","--input",$archive,"--offline-scan","--skip-db-update","--skip-check-update","--scanners","vuln,secret,misconfig","--severity","UNKNOWN,LOW,MEDIUM,HIGH,CRITICAL","--exit-code","0","--format","json","--output",$report],
            db_updated_at:$db_updated,db_downloaded_at:$db_downloaded,db_sha256:$db_sha,db_path:$db_path,db_oci_digest:null},
          scan:{report_path:$report,report_sha256:$report_sha,archive_sha256:$archive_sha,config_id:$image_id,
            scanned_at:$scanned_at,policy_path:$policy,policy_version:"1",policy_sha256:$policy_sha,policy_result:"pass"}}' \
        > "${scan_dir}/scan-tuple.json"
    "${ARTIFACT_SCRIPT}" verify-scan-preupload --tuple "${scan_dir}/scan-tuple.json" > "${scan_dir}/policy.log"
}

verify_exact_test_result() {
    local label="$1" log="$2"
    [[ -f "${log}" ]] || die "${label} exact test selection is invalid: output is absent"
    [[ "$(grep -Ec '^running 1 test$' "${log}" || true)" -eq 1 ]] \
        || die "${label} exact test selection is invalid: expected exactly one selected test"
    [[ "$(grep -Ec '^test result: ok\. 1 passed; 0 failed; 0 ignored; [0-9]+ measured; [0-9]+ filtered out; finished in ' "${log}" || true)" -eq 1 ]] \
        || die "${label} exact test selection is invalid: expected exactly 1 passed; 0 failed; 0 ignored"
    if grep -Eiq 'skipping' "${log}"; then
        die "${label} exact test selection is invalid: manual skipping message present"
    fi
}

prove_archive() {
    local archive="$1" source_sha="$2" evidence_dir="$3" trivy_cache="$4"
    [[ "${source_sha}" =~ ^[0-9a-f]{40}$ ]] || die "source SHA must be 40 lowercase hex"
    [[ -f "${archive}" ]] || die "release archive is absent"
    archive="$(realpath -- "${archive}")"
    [[ "$(uname -m)" == "aarch64" || "$(uname -m)" == "arm64" ]] || die "release proof requires native ARM64"
    [[ "$(stat -fc '%T' /sys/fs/cgroup)" == "cgroup2fs" ]] || die "authoritative cgroup-v2 is unavailable"
    mkdir -p -- "${evidence_dir}" "${trivy_cache}"
    evidence_dir="$(realpath -- "${evidence_dir}")"
    trivy_cache="$(realpath -- "${trivy_cache}")"
    printf '%s\n' "${source_sha}" > "${evidence_dir}/source-sha.txt"
    sha256sum "${archive}" > "${evidence_dir}/archive.sha256"
    docker load --input "${archive}" > "${evidence_dir}/docker-load.log"
    local archive_manifest config_member config_hex image_id archive_ref label architecture stop_signal
    archive_manifest="$(tar -xOf "${archive}" manifest.json)"
    config_member="$(jq -r 'if length == 1 then .[0].Config else empty end' <<<"${archive_manifest}")"
    [[ "${config_member}" =~ ^blobs/sha256/[0-9a-f]{64}$ ]] \
        || die "archive config member is not one exact OCI-layout digest blob"
    archive_ref="$(jq -r --arg suffix ":${source_sha}" \
        'if length == 1 and (.[0].RepoTags | length) == 1 and (.[0].RepoTags[0] | endswith($suffix)) then .[0].RepoTags[0] else empty end' \
        <<<"${archive_manifest}")"
    [[ -n "${archive_ref}" ]] || die "archive must contain one tag bound to the exact source SHA"
    config_hex="${config_member##*/}"
    image_id="sha256:${config_hex}"
    [[ "$(sha256sum <(tar -xOf "${archive}" "${config_member}") | cut -d' ' -f1)" == "${config_hex}" ]] \
        || die "archive config hash mismatch"
    docker image inspect "${image_id}" > "${evidence_dir}/image-inspect.json"
    label="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "${image_id}")"
    architecture="$(docker image inspect --format '{{.Architecture}}' "${image_id}")"
    stop_signal="$(docker image inspect --format '{{.Config.StopSignal}}' "${image_id}")"
    [[ "${label}" == "${source_sha}" && "${architecture}" == "arm64" && "${stop_signal}" == "SIGINT" ]] \
        || die "loaded archive source/platform/default stop identity mismatch"
    printf '%s\n' "${image_id}" > "${evidence_dir}/image-id.txt"

    tar -tvf "${archive}" > "${evidence_dir}/archive-members.txt"
    stat -c '%s' "${archive}" > "${evidence_dir}/archive-bytes.txt"
    docker image inspect --format '{{.Size}}' "${image_id}" > "${evidence_dir}/uncompressed-image-bytes.txt"

    local extract_container model_root
    model_root="${evidence_dir}/exact-image-model-root"
    mkdir -p -- "${model_root}"
    extract_container="$(docker create "${image_id}")"
    docker cp "${extract_container}:/opt/pensyve/models/." - \
        | tar --extract --no-same-owner --no-same-permissions --directory "${model_root}"
    docker rm "${extract_container}" > "${evidence_dir}/extract-remove.txt"
    "${MODEL_TEST}" "${model_root}" all > "${evidence_dir}/bundle-tests.log" 2>&1
    find "${model_root}" -path '*/snapshots/*/LICENSE.pensyve.txt' -type f -print0 \
        | sort -z | xargs -0 -r sha256sum > "${evidence_dir}/model-license-custody.sha256"
    [[ "$(wc -l < "${evidence_dir}/model-license-custody.sha256")" -eq 2 ]] \
        || die "exact two authoritative model license files are not baked"
    (
        cd "${REPO_ROOT}"
        HF_HOME="${model_root}" FASTEMBED_CACHE_DIR="${model_root}" HF_HUB_OFFLINE=1 \
          PENSYVE_NETWORK_POLICY=disabled cargo test --locked -p pensyve-core \
            embedding::tests::disabled_gte_constructs_from_complete_real_seeded_cache \
            -- --ignored --exact --nocapture --test-threads=1
    ) > "${evidence_dir}/real-gte-inference.log" 2>&1
    verify_exact_test_result "GTE" "${evidence_dir}/real-gte-inference.log"
    (
        cd "${REPO_ROOT}"
        HF_HOME="${model_root}" FASTEMBED_CACHE_DIR="${model_root}" HF_HUB_OFFLINE=1 \
          PENSYVE_NETWORK_POLICY=disabled cargo test --locked -p pensyve-core \
            --test test_no_network_invariants reranker_does_not_make_network_calls \
            -- --exact --nocapture --test-threads=1
    ) > "${evidence_dir}/real-bge-inference.log" 2>&1
    verify_exact_test_result "BGE" "${evidence_dir}/real-bge-inference.log"
    cat "${evidence_dir}/real-gte-inference.log" "${evidence_dir}/real-bge-inference.log" \
        > "${evidence_dir}/real-model-inference.log"

    run_standalone_lifecycle "${image_id}" "${evidence_dir}"
    run_missing_model_failure "${image_id}" "${archive_ref}" "${evidence_dir}"

    mkdir -p -- "${evidence_dir}/sizing"
    uname -a > "${evidence_dir}/sizing/uname.txt"
    cat /sys/fs/cgroup/cgroup.controllers > "${evidence_dir}/sizing/cgroup-controllers.txt"
    local run
    for run in 1 2 3 4 5; do run_one_sizing "${image_id}" "${evidence_dir}" "${run}"; done
    cat "${evidence_dir}"/sizing/run-*/summary.txt > "${evidence_dir}/sizing/summary.txt"
    [[ "$(sort -u "${evidence_dir}"/sizing/run-*/cgroup-path.txt | wc -l)" -eq 5 ]] \
        || die "five sizing runs did not use five fresh cgroups"

    run_trivy "${archive}" "${image_id}" "${trivy_cache}" "${evidence_dir}"
    jq -n --arg source_sha "${source_sha}" --arg archive "${archive}" \
        --arg archive_sha "$(sha256_file "${archive}")" --arg image_id "${image_id}" \
        --arg platform "linux/arm64" --arg sizing "${evidence_dir}/sizing/summary.txt" \
        --arg sizing_sha "$(sha256_file "${evidence_dir}/sizing/summary.txt")" \
        --slurpfile scan "${evidence_dir}/trivy/scan-tuple.json" \
        '{source_sha:$source_sha,archive_path:$archive,archive_sha256:$archive_sha,config_id:$image_id,
          platform:$platform,scanner:$scan[0].scanner,scan:$scan[0].scan,
          gates:{bundle:"pass",gte:"pass",bge:"pass",default_stop:"pass",missing_model:"pass",
            five_cgroups:"pass",no_egress:true,read_only_root:true,embedding_pool_size:1,
            sizing_summary_path:$sizing,sizing_summary_sha256:$sizing_sha}}' \
        > "${evidence_dir}/release-evidence.json"
}

usage() {
    cat >&2 <<'EOF'
usage:
  test-gateway-release-image.sh prepare-trivy --cache-dir DIR --evidence-dir DIR
  test-gateway-release-image.sh prove --archive FILE --source-sha SHA --evidence-dir DIR --trivy-cache DIR
EOF
    exit 2
}

mode="${1:-}"
[[ -n "${mode}" ]] || usage
shift
cache_dir="" evidence_dir="" archive="" source_sha="" trivy_cache=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --cache-dir) need_arg "$1" "${2:-}"; cache_dir="$2"; shift 2 ;;
        --evidence-dir) need_arg "$1" "${2:-}"; evidence_dir="$2"; shift 2 ;;
        --archive) need_arg "$1" "${2:-}"; archive="$2"; shift 2 ;;
        --source-sha) need_arg "$1" "${2:-}"; source_sha="$2"; shift 2 ;;
        --trivy-cache) need_arg "$1" "${2:-}"; trivy_cache="$2"; shift 2 ;;
        *) usage ;;
    esac
done

case "${mode}" in
    prepare-trivy)
        [[ -n "${cache_dir}" && -n "${evidence_dir}" ]] || usage
        prepare_trivy "${cache_dir}" "${evidence_dir}"
        ;;
    prove)
        [[ -n "${archive}" && -n "${source_sha}" && -n "${evidence_dir}" && -n "${trivy_cache}" ]] || usage
        prove_archive "${archive}" "${source_sha}" "${evidence_dir}" "${trivy_cache}"
        ;;
    *) usage ;;
esac
