#!/usr/bin/env bash
#
# Prepare everything the desktop app bundles besides its own window (tauri.conf.json
# runs this in `beforeBuildCommand`, before the console's `desktop` build):
#
#   1. the source engine: the pinned Suwayomi-Server jar and a jlink'd JRE, made by the
#      READER's scripts into apps/reader/src-tauri/{suwayomi,jre}/, which this app's
#      tauri.conf.json bundles from there (one copy, one pin, for both apps);
#   2. the server: `komika-server`, built from apps/server and placed where Tauri's
#      `externalBin` expects it, binaries/komika-server-<target-triple>[.exe];
#   3. the web apps the server serves (KOMIKA_WEB_DIR): web/reader and web/admin.
#
# Usage:  bash src-tauri/scripts/prepare-bundle.sh          (from apps/admin)
# Env:
#   KOMIKA_SKIP_ENGINE=1        skip step 1 (no JDK here; the app then reports the
#                               engine as missing, or run it with KOMIKA_DESKTOP_ENGINE=off)
#   KOMIKA_SERVER_PROFILE=debug build the server in debug (default: release)
#   KOMIKA_SERVER_TARGET_DIR    cargo target dir for the server (default: CARGO_TARGET_DIR,
#                               else apps/server/target)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI="$(cd "${SCRIPT_DIR}/.." && pwd)"
ADMIN="$(cd "${SRC_TAURI}/.." && pwd)"
REPO="$(cd "${ADMIN}/../.." && pwd)"
READER_TAURI="${REPO}/apps/reader/src-tauri"

# The Tauri CLI exports these for its hooks; nested app builds must not inherit them
# (the reader's and the console's vite configs read TAURI_ENV_PLATFORM).
TARGET_TRIPLE="${TAURI_ENV_TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}"
unset TAURI_ENV_PLATFORM TAURI_ENV_ARCH TAURI_ENV_FAMILY TAURI_ENV_PLATFORM_VERSION \
  TAURI_ENV_PLATFORM_TYPE TAURI_ENV_DEBUG TAURI_ENV_TARGET_TRIPLE

# ---- 1. engine ------------------------------------------------------------------------
if [[ "${KOMIKA_SKIP_ENGINE:-}" == "1" ]]; then
  echo "skip: engine (KOMIKA_SKIP_ENGINE=1)"
else
  bash "${READER_TAURI}/scripts/fetch-suwayomi-jar.sh"
  bash "${READER_TAURI}/scripts/build-jre.sh"
fi

# ---- 2. server ------------------------------------------------------------------------
PROFILE="${KOMIKA_SERVER_PROFILE:-release}"
HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
SERVER_TARGET_DIR="${KOMIKA_SERVER_TARGET_DIR:-${CARGO_TARGET_DIR:-${REPO}/apps/server/target}}"
CARGO_FLAGS=(--manifest-path "${REPO}/apps/server/Cargo.toml")
OUT="${SERVER_TARGET_DIR}"
# Cross builds only: `--target <host>` would rebuild everything in a second directory.
if [[ "${TARGET_TRIPLE}" != "${HOST_TRIPLE}" ]]; then
  CARGO_FLAGS+=(--target "${TARGET_TRIPLE}")
  OUT="${OUT}/${TARGET_TRIPLE}"
fi
[[ "${PROFILE}" == "release" ]] && CARGO_FLAGS+=(--release)
[[ "${PROFILE}" == "debug" ]] || [[ "${PROFILE}" == "release" ]] || {
  echo "error: KOMIKA_SERVER_PROFILE must be debug or release" >&2
  exit 1
}
echo "building komika-server (${PROFILE}, ${TARGET_TRIPLE})"
CARGO_TARGET_DIR="${SERVER_TARGET_DIR}" cargo build "${CARGO_FLAGS[@]}"
EXE=""
[[ "${TARGET_TRIPLE}" == *windows* ]] && EXE=".exe"
mkdir -p "${SRC_TAURI}/binaries"
cp "${OUT}/${PROFILE}/komika-server${EXE}" \
  "${SRC_TAURI}/binaries/komika-server-${TARGET_TRIPLE}${EXE}"
echo "ok: binaries/komika-server-${TARGET_TRIPLE}${EXE}"

# ---- 3. web apps ----------------------------------------------------------------------
WEB="${SRC_TAURI}/web"
rm -rf "${WEB}/reader" "${WEB}/admin"

# Reader: its `embedded` build, a static SPA calling the API on its own origin.
echo "building the reader (served at /)"
KOMIKA_TARGET=embedded pnpm -C "${REPO}/apps/reader" build
cp -R "${REPO}/apps/reader/build" "${WEB}/reader"

# Console: the `embedded` build under /admin, for browsers on the home network. The
# window's own `desktop` build runs after this script and replaces apps/admin/build.
echo "building the admin console (served at /admin)"
ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin PUBLIC_KOMIKA_API= pnpm -C "${ADMIN}" build
cp -R "${ADMIN}/build" "${WEB}/admin"

echo "ok: web/reader and web/admin ready"
