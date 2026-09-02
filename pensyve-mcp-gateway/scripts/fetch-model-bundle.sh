#!/usr/bin/env bash

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly MODELS_DIR="$(cd -- "${SCRIPT_DIR}/../models" && pwd)"
readonly MANIFEST="${MODELS_DIR}/manifest.sha256"
readonly REVISIONS="${MODELS_DIR}/revisions.env"
readonly GTE_REPOSITORY="Alibaba-NLP/gte-base-en-v1.5"
readonly GTE_CACHE_REPOSITORY="models--Alibaba-NLP--gte-base-en-v1.5"
readonly BGE_REPOSITORY="BAAI/bge-reranker-base"
readonly BGE_CACHE_REPOSITORY="models--BAAI--bge-reranker-base"
readonly SPDX_LICENSE_REVISION="c4a7237ec8f4654e867546f9f409749300f1bf4c"
readonly SPDX_LICENSE_BASE="https://raw.githubusercontent.com/spdx/license-list-data/${SPDX_LICENSE_REVISION}/text"

# shellcheck disable=SC1090
source "${REVISIONS}"

fail() {
    echo "model bundle error: $*" >&2
    return 1
}

validate_revision() {
    local name="$1"
    local revision="$2"

    if [[ ! "${revision}" =~ ^[0-9a-f]{40}$ ]]; then
        fail "${name} must be an exact lowercase 40-hex revision, got ${revision}"
    fi
}

safe_relative_path() {
    local path="$1"

    [[ -n "${path}" && "${path}" != /* && "${path}" != *".."* \
        && "${path}" != *"://"* && "${path}" != *"@"* ]]
}

validate_manifest_paths() {
    local kind expected_sha expected_bytes cache_path snapshot_path extra

    while read -r kind expected_sha expected_bytes cache_path snapshot_path extra; do
        [[ -n "${kind}" && "${kind}" != \#* ]] || continue
        [[ -z "${extra:-}" ]] || { fail "invalid manifest row: ${cache_path}"; return 1; }
        safe_relative_path "${cache_path}" \
            || { fail "unsafe cache path: ${cache_path}"; return 1; }
        if [[ "${kind}" == "blob" ]]; then
            safe_relative_path "${snapshot_path}" \
                || { fail "unsafe snapshot path: ${snapshot_path}"; return 1; }
        fi
    done < "${MANIFEST}"
}

file_sha256() {
    sha256sum -- "$1" | cut -d' ' -f1
}

verify_file() {
    local root="$1"
    local expected_sha="$2"
    local expected_bytes="$3"
    local relative_path="$4"
    local absolute_path="${root}/${relative_path}"
    local errors=0

    if [[ ! -f "${absolute_path}" ]]; then
        echo "missing required cache path: ${relative_path}" >&2
        return 1
    fi
    if [[ "$(wc -c < "${absolute_path}")" != "${expected_bytes}" ]]; then
        echo "byte-count mismatch: ${relative_path}" >&2
        errors=1
    fi
    if [[ "$(file_sha256 "${absolute_path}")" != "${expected_sha}" ]]; then
        echo "SHA-256 mismatch: ${relative_path}" >&2
        errors=1
    fi
    if head -c 200 -- "${absolute_path}" \
        | grep -F "version https://git-lfs.github.com/spec/v1" >/dev/null; then
        echo "Git LFS pointer rejected: ${relative_path}" >&2
        errors=1
    fi
    return "${errors}"
}

verify_bundle() {
    local root="$1"
    local errors=0
    local ref_count=0
    local blob_count=0
    local license_count=0
    local kind expected_sha expected_bytes cache_path snapshot_path
    local expected_ref_paths=""
    local expected_blob_paths=""
    local expected_snapshot_paths=""

    if [[ ! -d "${root}" ]]; then
        fail "bundle root does not exist: ${root}"
        return 1
    fi

    while read -r kind expected_sha expected_bytes cache_path snapshot_path extra; do
        [[ -z "${kind}" || "${kind}" == \#* ]] && continue
        if [[ -n "${extra:-}" ]]; then
            echo "invalid manifest row with extra fields: ${cache_path}" >&2
            errors=1
            continue
        fi
        if [[ ! "${expected_sha}" =~ ^[0-9a-f]{64}$ \
            || ! "${expected_bytes}" =~ ^[0-9]+$ \
            || "${cache_path}" == *MiniLM* ]] \
            || ! safe_relative_path "${cache_path}"; then
            echo "unsafe manifest row: ${cache_path}" >&2
            errors=1
            continue
        fi

        case "${kind}" in
            ref)
                ref_count=$((ref_count + 1))
                expected_ref_paths+="${cache_path}"$'\n'
                if [[ "${snapshot_path}" != "-" ]]; then
                    echo "ref row has a snapshot path: ${cache_path}" >&2
                    errors=1
                fi
                verify_file "${root}" "${expected_sha}" "${expected_bytes}" "${cache_path}" \
                    || errors=1
                ;;
            blob)
                blob_count=$((blob_count + 1))
                expected_blob_paths+="${cache_path}"$'\n'
                expected_snapshot_paths+="${snapshot_path}"$'\n'
                if ! safe_relative_path "${snapshot_path}" \
                    || [[ "${snapshot_path}" == *MiniLM* ]]; then
                    echo "unsafe snapshot path: ${snapshot_path}" >&2
                    errors=1
                    continue
                fi
                verify_file "${root}" "${expected_sha}" "${expected_bytes}" "${cache_path}" \
                    || errors=1
                if [[ ! -L "${root}/${snapshot_path}" ]]; then
                    echo "snapshot path is not a symlink: ${snapshot_path}" >&2
                    errors=1
                elif [[ ! -e "${root}/${snapshot_path}" ]]; then
                    echo "snapshot symlink target is missing: ${snapshot_path}" >&2
                    errors=1
                elif [[ "$(realpath -- "${root}/${snapshot_path}")" \
                    != "$(realpath -- "${root}/${cache_path}")" ]]; then
                    echo "snapshot symlink does not resolve to manifest blob: ${snapshot_path}" >&2
                    errors=1
                fi
                ;;
            license-file)
                license_count=$((license_count + 1))
                expected_snapshot_paths+="${cache_path}"$'\n'
                case "${snapshot_path}" in
                    "spdx-license-list-data@${SPDX_LICENSE_REVISION}/Apache-2.0.txt"|\
                    "spdx-license-list-data@${SPDX_LICENSE_REVISION}/MIT.txt") ;;
                    *)
                        echo "unapproved authoritative license source: ${snapshot_path}" >&2
                        errors=1
                        continue
                        ;;
                esac
                verify_file "${root}" "${expected_sha}" "${expected_bytes}" "${cache_path}" \
                    || errors=1
                ;;
            *)
                echo "unknown manifest row kind ${kind}: ${cache_path}" >&2
                errors=1
                ;;
        esac
    done < "${MANIFEST}"

    if [[ "${ref_count}" -ne 2 || "${blob_count}" -ne 12 || "${license_count}" -ne 2 ]]; then
        echo "manifest must contain exactly 2 refs, 12 blobs, and 2 license-file records" >&2
        errors=1
    fi

    local repository revision actual_revision
    for repository in "${GTE_CACHE_REPOSITORY}" "${BGE_CACHE_REPOSITORY}"; do
        if [[ "${repository}" == "${GTE_CACHE_REPOSITORY}" ]]; then
            revision="${GTE_REVISION}"
        else
            revision="${BGE_REVISION}"
        fi
        if [[ ! -f "${root}/${repository}/refs/main" ]]; then
            echo "missing required ref: ${repository}/refs/main" >&2
            errors=1
        else
            actual_revision="$(cat -- "${root}/${repository}/refs/main")"
            if [[ "${actual_revision}" != "${revision}" ]]; then
                echo "floating or mismatched ref rejected: ${repository}/refs/main" >&2
                errors=1
            fi
        fi

        if [[ -d "${root}/${repository}/refs" ]]; then
            while IFS= read -r path; do
                path="${path#"${root}"/}"
                if ! grep -Fx -- "${path}" <<<"${expected_ref_paths}" >/dev/null; then
                    echo "extra ref rejected: ${path}" >&2
                    errors=1
                fi
            done < <(find "${root}/${repository}/refs" -mindepth 1 \( -type f -o -type l \) -print)
        fi
    done

    while IFS= read -r path; do
        path="${path#"${root}"/}"
        if ! grep -Fx -- "${path}" <<<"${expected_blob_paths}" >/dev/null; then
            echo "unmanifested blob rejected: ${path}" >&2
            errors=1
        fi
    done < <(find "${root}" -path '*/blobs/*' -type f -print)

    while IFS= read -r path; do
        path="${path#"${root}"/}"
        if ! grep -Fx -- "${path}" <<<"${expected_snapshot_paths}" >/dev/null; then
            echo "unmanifested snapshot path rejected: ${path}" >&2
            errors=1
        fi
    done < <(find "${root}" -path '*/snapshots/*' \( -type f -o -type l \) -print)

    while IFS= read -r path; do
        path="${path#"${root}"/}"
        case "${path}" in
            "${GTE_CACHE_REPOSITORY}" | "${BGE_CACHE_REPOSITORY}") ;;
            *)
                echo "unapproved cache repository rejected: ${path}" >&2
                errors=1
                ;;
        esac
    done < <(find "${root}" -mindepth 1 -maxdepth 1 -type d -print)

    if [[ "${errors}" -ne 0 ]]; then
        return 1
    fi
    echo "model bundle verified: ${root}"
}

download_licenses() {
    local root="$1"
    local kind expected_sha expected_bytes cache_path source extra source_name output

    while read -r kind expected_sha expected_bytes cache_path source extra; do
        [[ "${kind}" == "license-file" ]] || continue
        [[ -z "${extra:-}" ]] \
            || { fail "invalid license manifest row: ${cache_path}"; return 1; }
        safe_relative_path "${cache_path}" \
            || { fail "unsafe license cache path: ${cache_path}"; return 1; }
        case "${source}" in
            "spdx-license-list-data@${SPDX_LICENSE_REVISION}/Apache-2.0.txt") source_name="Apache-2.0.txt" ;;
            "spdx-license-list-data@${SPDX_LICENSE_REVISION}/MIT.txt") source_name="MIT.txt" ;;
            *) fail "unapproved authoritative license source: ${source}" ; return 1 ;;
        esac
        output="${root}/${cache_path}"
        mkdir -p -- "$(dirname -- "${output}")"
        echo "downloading authoritative license ${source}"
        curl --fail --silent --show-error --location \
            --proto '=https' --proto-redir '=https' \
            --retry 3 --retry-all-errors \
            "${SPDX_LICENSE_BASE}/${source_name}" \
            --output "${output}.download"
        verify_file "${root}" "${expected_sha}" "${expected_bytes}" "${cache_path}.download"
        mv -- "${output}.download" "${output}"
    done < "${MANIFEST}"
}

download_model() {
    local root="$1"
    local repository="$2"
    local cache_repository="$3"
    local revision="$4"
    local kind expected_sha expected_bytes cache_path snapshot_path source_path blob_path

    validate_manifest_paths || return 1
    printf '%s' "${revision}" > "${root}/${cache_repository}/refs/main"
    while read -r kind expected_sha expected_bytes cache_path snapshot_path extra; do
        [[ "${kind}" != "blob" || "${snapshot_path}" != "${cache_repository}/snapshots/${revision}/"* ]] \
            && continue
        safe_relative_path "${cache_path}" \
            || { fail "unsafe cache path: ${cache_path}"; return 1; }
        safe_relative_path "${snapshot_path}" \
            || { fail "unsafe snapshot path: ${snapshot_path}"; return 1; }
        source_path="${snapshot_path#"${cache_repository}/snapshots/${revision}/"}"
        blob_path="${root}/${cache_path}"
        mkdir -p -- "$(dirname -- "${blob_path}")" \
            "$(dirname -- "${root}/${snapshot_path}")"
        echo "downloading ${repository}@${revision}/${source_path}"
        curl --fail --silent --show-error --location \
            --proto '=https' --proto-redir '=https' \
            --retry 3 --retry-all-errors \
            "https://huggingface.co/${repository}/resolve/${revision}/${source_path}?download=true" \
            --output "${blob_path}.download"
        verify_file "${root}" "${expected_sha}" "${expected_bytes}" \
            "${cache_path}.download"
        mv -- "${blob_path}.download" "${blob_path}"
        ln -s -- "$(realpath --relative-to="$(dirname -- "${root}/${snapshot_path}")" "${blob_path}")" \
            "${root}/${snapshot_path}"
    done < "${MANIFEST}"
}

fetch_bundle() {
    local output="$1"
    local parent
    local staging

    validate_manifest_paths
    parent="$(dirname -- "${output}")"
    mkdir -p -- "${parent}"
    if [[ -e "${output}" ]]; then
        if [[ ! -d "${output}" || -n "$(find "${output}" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
            fail "output must not exist or must be an empty directory: ${output}"
            return 1
        fi
        rmdir -- "${output}"
    fi

    staging="$(mktemp -d "${parent}/.pensyve-models.XXXXXX")"
    trap 'rm -rf -- "${staging}"' EXIT
    mkdir -p -- \
        "${staging}/${GTE_CACHE_REPOSITORY}/refs" \
        "${staging}/${BGE_CACHE_REPOSITORY}/refs"

    download_model "${staging}" "${GTE_REPOSITORY}" "${GTE_CACHE_REPOSITORY}" "${GTE_REVISION}"
    download_model "${staging}" "${BGE_REPOSITORY}" "${BGE_CACHE_REPOSITORY}" "${BGE_REVISION}"
    download_licenses "${staging}"
    verify_bundle "${staging}"
    find "${staging}" -type d -exec chmod 0555 {} +
    find "${staging}" -type f -exec chmod 0444 {} +
    mv -- "${staging}" "${output}"
    trap - EXIT
    echo "model bundle written: ${output}"
}

validate_revision GTE_REVISION "${GTE_REVISION}"
validate_revision BGE_REVISION "${BGE_REVISION}"

case "${1:-}" in
    --verify-only)
        [[ $# -eq 2 ]] || { fail "usage: $0 --verify-only ROOT"; exit 2; }
        verify_bundle "$2"
        ;;
    --output)
        [[ $# -eq 2 ]] || { fail "usage: $0 --output ROOT"; exit 2; }
        fetch_bundle "$2"
        ;;
    *)
        fail "usage: $0 --verify-only ROOT | --output ROOT"
        exit 2
        ;;
esac
