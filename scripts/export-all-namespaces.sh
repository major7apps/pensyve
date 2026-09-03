#!/usr/bin/env bash
# Encrypt and upload a bulk namespace export (MAJ-374, 2026-10-01 shutdown).
#
# `pensyve-mcp-gateway export-namespace --all --out-dir <dir>` writes one
# plain `<namespace>.db` per namespace plus `manifest.json`. This script is the
# transport half: it encrypts each artifact with GPG (AES-256, symmetric) and
# uploads the ciphertext to a private S3 prefix.
#
# Encryption and AWS deliberately live here rather than in the gateway binary:
# Pensyve is entering maintenance mode as an OSS single binary, and neither an
# AWS SDK nor a crypto stack belongs in what customers self-host. This is the
# same shape the MAJ-369 delivery used for Jeremy Chu's export.
#
# The passphrase is read from PENSYVE_EXPORT_PASSPHRASE and is never echoed,
# never written to disk, and never passed as an argv value (which would expose
# it in `ps`). It goes to gpg on a file descriptor. Store it in a password
# manager and hand it over out of band — never in git, Linear, or the vault.
#
# Usage:
#   PENSYVE_EXPORT_PASSPHRASE=... \
#     scripts/export-all-namespaces.sh <export-dir> s3://bucket/prefix
#
# Canonical destination for the 2026-10-01 run (MAJ-374):
#
#   s3://pensyve-prod-exports-use2/namespaces/2026-10-01/
#
# It is still passed as an argument rather than defaulted, so a mistyped or
# not-yet-created bucket fails loudly instead of silently uploading a customer
# data set somewhere unintended.
#
# Prerequisites:
#   - The export directory has already been produced by the gateway's
#     `--all` mode, with the gateway scaled to zero so nothing is writing.
#   - The destination bucket exists in us-east-2, with all four public-access
#     blocks on, default encryption enabled, and a 90-day expiry lifecycle rule
#     (the sunset decision promises 90-day retention). It is NOT one of the
#     buckets the teardown keeps — it is created for this run and deleted with
#     the rest of the 12-18 cleanup.
#   - `aws` is configured for the profile that can write to that bucket.

set -euo pipefail

if (( $# != 2 )); then
  echo "usage: $0 <export-dir> s3://bucket/prefix" >&2
  exit 2
fi

export_dir="$1"
destination="${2%/}"

if [[ -z "${PENSYVE_EXPORT_PASSPHRASE:-}" ]]; then
  echo "PENSYVE_EXPORT_PASSPHRASE is not set; refusing to upload plaintext" >&2
  exit 2
fi

if [[ "$destination" != s3://* ]]; then
  echo "destination must be an s3:// URI, got: $destination" >&2
  exit 2
fi

manifest="$export_dir/manifest.json"
if [[ ! -f "$manifest" ]]; then
  echo "no manifest at $manifest — run the gateway's --all export first" >&2
  exit 1
fi

# Refuse a manifest that records failures. The store is deleted after this
# runbook step, so uploading a knowingly partial export is unrecoverable.
failed_count="$(python3 -c '
import json, sys
with open(sys.argv[1]) as handle:
    print(len(json.load(handle).get("failed", [])))
' "$manifest")"
if [[ "$failed_count" != "0" ]]; then
  echo "manifest records $failed_count failed namespace(s); fix and re-run before uploading" >&2
  exit 1
fi

encrypt() {
  # --batch/--yes so a re-run cannot hang on a prompt; the passphrase arrives
  # on fd 3 so it never appears in the process table.
  gpg --batch --yes --quiet \
      --symmetric --cipher-algo AES256 \
      --passphrase-fd 3 \
      --output "$2" "$1" 3<<<"$PENSYVE_EXPORT_PASSPHRASE"
}

shopt -s nullglob
uploaded=0
for database in "$export_dir"/*.db; do
  name="$(basename "$database")"
  encrypted="$database.gpg"
  echo "encrypting $name"
  encrypt "$database" "$encrypted"

  echo "uploading $name.gpg"
  aws s3 cp "$encrypted" "$destination/$name.gpg" \
    --sse AES256 \
    --only-show-errors
  uploaded=$(( uploaded + 1 ))
done

# The manifest is counts and digests only (no namespace names, no memory
# content), but it still describes a customer data set, so it is encrypted
# alongside the exports rather than uploaded in the clear.
echo "encrypting and uploading manifest.json"
encrypt "$manifest" "$manifest.gpg"
aws s3 cp "$manifest.gpg" "$destination/manifest.json.gpg" \
  --sse AES256 \
  --only-show-errors

echo "uploaded $uploaded namespace export(s) + manifest to $destination"
echo
echo "Verify before teardown:"
echo "  aws s3 ls $destination/ --recursive --summarize | tail -3"
echo "Then compare the object count against \"namespaces\" in the manifest."
echo
echo "IMPORTANT: $export_dir still holds PLAINTEXT customer memories."
echo "The gateway writes them unencrypted and this script encrypts on the way"
echo "out, so every namespace sits in the clear on this box until you remove"
echo "them. Deliberately not deleted here — verify the upload first, then:"
echo "  shred -u $export_dir/*.db && rm -rf $export_dir"
echo
echo "Keep the passphrase in the password manager; it is not recorded anywhere here."
