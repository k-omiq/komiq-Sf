#!/usr/bin/env bash
#
# Fetch + SHA-256-verify the pinned Suwayomi-Server jar for the embedded sidecar.
#
# Supply-chain rule (native plan §3.1): the jar is NEVER committed (174 MB) and NEVER
# fetched as a floating tag. This script reads the exact version/url/sha256 pin from
# ../suwayomi/VERSION, downloads to the gitignored bundle path, and FAILS LOUDLY if the
# SHA-256 does not match. It is idempotent: an already-present, matching jar is a no-op.
#
# Usage:  ./scripts/fetch-suwayomi-jar.sh
set -euo pipefail

# Resolve paths relative to this script so it works from any CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
VERSION_FILE="${SRC_TAURI_DIR}/suwayomi/VERSION"
JAR_PATH="${SRC_TAURI_DIR}/suwayomi/Suwayomi-Server.jar"

if [[ ! -f "${VERSION_FILE}" ]]; then
  echo "error: pin file not found: ${VERSION_FILE}" >&2
  exit 1
fi

# Read key=value pairs from VERSION (ignore comments/blanks).
read_pin() {
  local key="$1"
  local val
  val="$(grep -E "^${key}=" "${VERSION_FILE}" | head -n1 | cut -d= -f2- || true)"
  if [[ -z "${val}" ]]; then
    echo "error: '${key}' not set in ${VERSION_FILE}" >&2
    exit 1
  fi
  printf '%s' "${val}"
}

VERSION="$(read_pin version)"
URL="$(read_pin url)"
SHA256_EXPECTED="$(read_pin sha256)"

# Portable sha256 (macOS: shasum -a 256; Linux: sha256sum).
compute_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${file}" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${file}" | awk '{print $1}'
  else
    echo "error: neither sha256sum nor shasum is available" >&2
    exit 1
  fi
}

verify() {
  local actual
  actual="$(compute_sha256 "${JAR_PATH}")"
  if [[ "${actual}" != "${SHA256_EXPECTED}" ]]; then
    echo "error: SHA-256 mismatch for ${JAR_PATH}" >&2
    echo "  expected: ${SHA256_EXPECTED}" >&2
    echo "  actual:   ${actual}" >&2
    return 1
  fi
  return 0
}

# Idempotent: if the jar is already present AND matches, we're done.
if [[ -f "${JAR_PATH}" ]]; then
  if verify; then
    echo "ok: Suwayomi-Server.jar already present and verified (${VERSION})"
    exit 0
  fi
  echo "warn: existing jar failed verification; re-downloading" >&2
  rm -f "${JAR_PATH}"
fi

echo "fetching Suwayomi-Server ${VERSION}"
echo "  url: ${URL}"
mkdir -p "$(dirname "${JAR_PATH}")"

# Download to a temp file, then move into place only after SHA passes, so a failed
# download never leaves a half-written jar at the bundle path.
TMP_JAR="${JAR_PATH}.download"
trap 'rm -f "${TMP_JAR}"' EXIT
if command -v curl >/dev/null 2>&1; then
  curl -fSL --retry 3 -o "${TMP_JAR}" "${URL}"
elif command -v wget >/dev/null 2>&1; then
  wget -O "${TMP_JAR}" "${URL}"
else
  echo "error: neither curl nor wget is available" >&2
  exit 1
fi

mv "${TMP_JAR}" "${JAR_PATH}"
trap - EXIT

if ! verify; then
  echo "error: downloaded jar failed SHA-256 verification; refusing to keep it" >&2
  rm -f "${JAR_PATH}"
  exit 1
fi

echo "ok: verified Suwayomi-Server.jar (${VERSION}) sha256=${SHA256_EXPECTED}"
