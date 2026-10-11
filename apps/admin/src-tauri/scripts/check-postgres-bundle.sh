#!/usr/bin/env bash
# Bundle-only commands can bypass both beforeBuildCommand and Cargo build.rs.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
TARGET_TRIPLE="${TAURI_ENV_TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}"
"${KOMIKA_PYTHON:-python3}" "${REPO}/scripts/postgres-desktop-bundle.py" --check --triple "${TARGET_TRIPLE}"
