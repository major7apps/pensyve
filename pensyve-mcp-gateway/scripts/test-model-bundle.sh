#!/usr/bin/env bash

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly VERIFY_SCRIPT="${SCRIPT_DIR}/fetch-model-bundle.sh"
readonly GTE_REPOSITORY="models--Alibaba-NLP--gte-base-en-v1.5"
readonly GTE_REVISION="a829fd0e060bb84554da0dfd354d0de0f7712b7f"

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "usage: $0 COMPLETE_PINNED_CACHE_ROOT [CASE]" >&2
    exit 2
fi

readonly FIXTURE_ROOT="$1"
readonly TEST_CASE="${2:-all}"
readonly GTE_SNAPSHOT="${GTE_REPOSITORY}/snapshots/${GTE_REVISION}"

case "${TEST_CASE}" in
    empty | missing-blob | floating-ref | extra-ref | lfs-pointer | extra-repository \
        | missing-license | complete | all) ;;
    *)
        echo "unknown test case: ${TEST_CASE}" >&2
        exit 2
        ;;
esac

if [[ ! -d "${FIXTURE_ROOT}/${GTE_SNAPSHOT}" ]]; then
    echo "complete pinned fixture is missing ${FIXTURE_ROOT}/${GTE_SNAPSHOT}" >&2
    exit 2
fi

run_verifier() {
    local root="$1"
    local output_file="$2"

    set +e
    "${VERIFY_SCRIPT}" --verify-only "${root}" >"${output_file}" 2>&1
    local status=$?
    set -e
    return "${status}"
}

copy_fixture() {
    local destination="$1"

    mkdir -p -- "${destination}"
    cp -al -- "${FIXTURE_ROOT}/." "${destination}"
    find "${destination}" -type d -exec chmod u+w {} +
}

expect_failure_naming() {
    local root="$1"
    local expected_one="$2"
    local expected_two="${3:-}"
    local output_file="$4"

    if run_verifier "${root}" "${output_file}"; then
        echo "expected verification failure for ${root}" >&2
        cat "${output_file}" >&2
        return 1
    fi
    if ! grep -F -- "${expected_one}" "${output_file}" >/dev/null; then
        echo "verification failure did not name ${expected_one}" >&2
        cat "${output_file}" >&2
        return 1
    fi
    if [[ -n "${expected_two}" ]] \
        && ! grep -F -- "${expected_two}" "${output_file}" >/dev/null; then
        echo "verification failure did not name ${expected_two}" >&2
        cat "${output_file}" >&2
        return 1
    fi
}

readonly TEST_ROOT="$(mktemp -d "$(dirname -- "${FIXTURE_ROOT}")/.pensyve-model-test.[literal].XXXXXX")"
trap 'find "${TEST_ROOT}" -type d -exec chmod u+w {} + 2>/dev/null || true; rm -rf -- "${TEST_ROOT}"' EXIT

readonly EMPTY_ROOT="${TEST_ROOT}/empty"
if [[ "${TEST_CASE}" == "empty" || "${TEST_CASE}" == "all" ]]; then
    mkdir -p "${EMPTY_ROOT}"
    expect_failure_naming \
        "${EMPTY_ROOT}" \
        "models--Alibaba-NLP--gte-base-en-v1.5" \
        "models--BAAI--bge-reranker-base" \
        "${TEST_ROOT}/empty.log"
fi

readonly MISSING_BLOB_ROOT="${TEST_ROOT}/missing-blob"
if [[ "${TEST_CASE}" == "missing-blob" || "${TEST_CASE}" == "all" ]]; then
    copy_fixture "${MISSING_BLOB_ROOT}"
    readonly GTE_MODEL_LINK="${MISSING_BLOB_ROOT}/${GTE_SNAPSHOT}/onnx/model.onnx"
    readonly GTE_MODEL_BLOB="$(realpath -- "${GTE_MODEL_LINK}")"
    rm -- "${GTE_MODEL_BLOB}"
    expect_failure_naming \
        "${MISSING_BLOB_ROOT}" \
        "${GTE_MODEL_BLOB#"${MISSING_BLOB_ROOT}"/}" \
        "" \
        "${TEST_ROOT}/missing-blob.log"
fi

if [[ "${TEST_CASE}" == "floating-ref" || "${TEST_CASE}" == "all" ]]; then
    readonly FLOATING_REF_ROOT="${TEST_ROOT}/floating-ref"
    copy_fixture "${FLOATING_REF_ROOT}"
    rm -- "${FLOATING_REF_ROOT}/${GTE_REPOSITORY}/refs/main"
    printf '%s' main > "${FLOATING_REF_ROOT}/${GTE_REPOSITORY}/refs/main"
    expect_failure_naming \
        "${FLOATING_REF_ROOT}" \
        "${GTE_REPOSITORY}/refs/main" \
        "" \
        "${TEST_ROOT}/floating-ref.log"
fi

if [[ "${TEST_CASE}" == "extra-ref" || "${TEST_CASE}" == "all" ]]; then
    readonly EXTRA_REF_ROOT="${TEST_ROOT}/extra-ref"
    copy_fixture "${EXTRA_REF_ROOT}"
    mkdir -p -- "${EXTRA_REF_ROOT}/${GTE_REPOSITORY}/refs/tags"
    printf '%s' "${GTE_REVISION}" > "${EXTRA_REF_ROOT}/${GTE_REPOSITORY}/refs/tags/latest"
    expect_failure_naming \
        "${EXTRA_REF_ROOT}" \
        "${GTE_REPOSITORY}/refs/tags/latest" \
        "" \
        "${TEST_ROOT}/extra-ref.log"
fi

if [[ "${TEST_CASE}" == "lfs-pointer" || "${TEST_CASE}" == "all" ]]; then
    readonly LFS_ROOT="${TEST_ROOT}/lfs-pointer"
    copy_fixture "${LFS_ROOT}"
    readonly LFS_LINK="${LFS_ROOT}/${GTE_SNAPSHOT}/tokenizer.json"
    readonly LFS_BLOB="$(realpath -- "${LFS_LINK}")"
    rm -- "${LFS_BLOB}"
    printf '%s\n' \
        'version https://git-lfs.github.com/spec/v1' \
        'oid sha256:cb374d6bc042c22455946f4e09a89d29882a199fdaf8fb25be00dc8b8857a448' \
        'size 711661' > "${LFS_BLOB}"
    expect_failure_naming \
        "${LFS_ROOT}" \
        "${LFS_BLOB#"${LFS_ROOT}"/}" \
        "" \
        "${TEST_ROOT}/lfs-pointer.log"
fi

if [[ "${TEST_CASE}" == "extra-repository" || "${TEST_CASE}" == "all" ]]; then
    readonly EXTRA_REPOSITORY_ROOT="${TEST_ROOT}/extra-repository"
    copy_fixture "${EXTRA_REPOSITORY_ROOT}"
    mkdir -p -- "${EXTRA_REPOSITORY_ROOT}/models--Qdrant--all-MiniLM-L6-v2-onnx"
    expect_failure_naming \
        "${EXTRA_REPOSITORY_ROOT}" \
        "models--Qdrant--all-MiniLM-L6-v2-onnx" \
        "" \
        "${TEST_ROOT}/extra-repository.log"
fi

if [[ "${TEST_CASE}" == "missing-license" || "${TEST_CASE}" == "all" ]]; then
    readonly MISSING_LICENSE_ROOT="${TEST_ROOT}/missing-license"
    copy_fixture "${MISSING_LICENSE_ROOT}"
    rm -- "${MISSING_LICENSE_ROOT}/${GTE_SNAPSHOT}/LICENSE.pensyve.txt"
    expect_failure_naming \
        "${MISSING_LICENSE_ROOT}" \
        "${GTE_SNAPSHOT}/LICENSE.pensyve.txt" \
        "" \
        "${TEST_ROOT}/missing-license.log"
fi

if [[ "${TEST_CASE}" == "complete" || "${TEST_CASE}" == "all" ]]; then
    if ! run_verifier "${FIXTURE_ROOT}" "${TEST_ROOT}/complete.log"; then
        echo "expected complete pinned cache verification to succeed" >&2
        cat "${TEST_ROOT}/complete.log" >&2
        exit 1
    fi
    if ! grep -F -- "model bundle verified" "${TEST_ROOT}/complete.log" >/dev/null; then
        echo "successful verification did not report model bundle verified" >&2
        cat "${TEST_ROOT}/complete.log" >&2
        exit 1
    fi
fi

echo "model bundle tests passed"
