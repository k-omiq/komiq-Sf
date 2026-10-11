#!/usr/bin/env bash
#
# The installer gate in a Linux container (aarch64 Linux on the owner's ARM server; any
# Debian or Ubuntu container works). NEVER run this on a machine that is not thrown away:
# it installs and removes a system package with dpkg.
#
# Two calls, in one container that has the repository mounted and the Rust toolchain and
# the Tauri build packages of the lifecycle gate (libwebkit2gtk-4.1-dev, libgtk-3-dev, ...):
#
#   1. as root:
#        bash apps/admin/src-tauri/scripts/installer-gate-container.sh root-setup <uid>
#      <uid>: the user that runs call 2 (default: the owner of the repository folder).
#      Installs sudo, curl and xz, gives that user sudo without a password (only dpkg
#      needs it), a passwd entry when it has none, Node 22 when the container has none
#      that is new enough for pnpm 11.9, and pnpm 11.9.0.
#   2. as that user (not root: PostgreSQL does not run as root):
#        bash apps/admin/src-tauri/scripts/installer-gate-container.sh run <output folder>
#      Installs the workspace's Node packages, builds the unsigned `.deb`
#      (installer-gate-build.sh), and runs the test `desktop_installer_gate` with a scratch
#      home folder below <output folder>. Leaves there: build.log, build-facts.json,
#      desktop-installer-gate-<arch>-linux.json and .log.
#
# Env for call 2 (all optional): KOMIKA_SERVER_PROFILE (default here: debug, the profile
# the lifecycle gate used on this host; `release` is the product's), KOMIKA_POSTGRES_ARCHIVE
# (an archive that is already there, for staging without a download), KOMIKA_PYTHON
# (Python >= 3.12), KOMIKA_SKIP_ENGINE (default 1).
set -euo pipefail

MODE="${1:?usage: installer-gate-container.sh root-setup [uid] | run <output folder>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
NODE_VERSION="22.23.3"
PNPM_VERSION="11.9.0"

node_is_new_enough() {
  command -v node >/dev/null 2>&1 &&
    node -e 'const [a, b] = process.versions.node.split(".").map(Number); process.exit(a > 22 || (a === 22 && b >= 13) ? 0 : 1)'
}

case "${MODE}" in
  root-setup)
    [[ "$(id -u)" == "0" ]] || {
      echo "error: root-setup must run as root" >&2
      exit 1
    }
    [[ -f /.dockerenv || -n "${KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE:-}" ]] || {
      echo "error: this is not a container. Set KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE=1 only on a machine that is thrown away." >&2
      exit 1
    }
    USER_ID="${2:-$(stat -c %u "${REPO}")}"
    [[ "${USER_ID}" != "0" ]] || {
      echo "error: the user of the run must not be root (give its uid)" >&2
      exit 1
    }
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    # The two libraries are what Tauri's .deb depends on; the -dev packages of the build
    # bring them, so this is a no-op in the gate's image.
    apt-get install -y --no-install-recommends sudo curl xz-utils ca-certificates \
      libwebkit2gtk-4.1-0 libgtk-3-0
    if ! getent passwd "${USER_ID}" >/dev/null; then
      useradd --uid "${USER_ID}" --no-create-home --shell /bin/bash installer-gate
    fi
    USER_NAME="$(getent passwd "${USER_ID}" | cut -d: -f1)"
    echo "${USER_NAME} ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/installer-gate
    chmod 0440 /etc/sudoers.d/installer-gate
    if ! node_is_new_enough; then
      case "$(uname -m)" in
        aarch64 | arm64) NODE_ARCH=arm64 ;;
        x86_64) NODE_ARCH=x64 ;;
        *)
          echo "error: no Node build known for $(uname -m)" >&2
          exit 1
          ;;
      esac
      NODE_FILE="node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz"
      WORK="$(mktemp -d)"
      curl -fsSL -o "${WORK}/${NODE_FILE}" "https://nodejs.org/dist/v${NODE_VERSION}/${NODE_FILE}"
      curl -fsSL -o "${WORK}/SHASUMS256.txt" "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt"
      (cd "${WORK}" && grep " ${NODE_FILE}\$" SHASUMS256.txt | sha256sum -c -)
      tar -xJf "${WORK}/${NODE_FILE}" -C /usr/local --strip-components=1
      rm -rf "${WORK}"
    fi
    node --version
    if [[ "$(pnpm --version 2>/dev/null || true)" != "${PNPM_VERSION}" ]]; then
      npm install -g --no-audit --no-fund "pnpm@${PNPM_VERSION}"
    fi
    pnpm --version
    echo "ok: root-setup done for uid ${USER_ID}"
    ;;

  run)
    OUT="${2:?usage: installer-gate-container.sh run <output folder>}"
    [[ "$(id -u)" != "0" ]] || {
      echo "error: run must not be root (PostgreSQL does not run as root)" >&2
      exit 1
    }
    [[ -f /.dockerenv || -n "${KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE:-}" ]] || {
      echo "error: this is not a container. Set KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE=1 only on a machine that is thrown away." >&2
      exit 1
    }
    sudo -n true
    mkdir -p "${OUT}"
    OUT="$(cd "${OUT}" && pwd)"
    ARCH="$(uname -m)"
    [[ "${ARCH}" != "arm64" ]] || ARCH=aarch64
    if [[ -z "${KOMIKA_PYTHON:-}" ]]; then
      for candidate in python3.14 python3.13 python3.12 python3 python; do
        if command -v "${candidate}" >/dev/null 2>&1 &&
          "${candidate}" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 12) else 1)'; then
          KOMIKA_PYTHON="${candidate}"
          break
        fi
      done
    fi
    [[ -n "${KOMIKA_PYTHON:-}" ]] || {
      echo "error: no Python >= 3.12 found; set KOMIKA_PYTHON" >&2
      exit 1
    }
    export KOMIKA_PYTHON
    export KOMIKA_SERVER_PROFILE="${KOMIKA_SERVER_PROFILE:-debug}"
    export CARGO_INCREMENTAL=0
    export CI=true
    # pnpm keeps its store, cache and state below the home folder. A uid that was given
    # to `docker exec` may have a home folder that does not exist: then they go below the
    # output folder. The test itself sets its own scratch HOME and drops XDG_DATA_HOME.
    if [[ ! -d "${HOME:-/nonexistent}" || ! -w "${HOME:-/nonexistent}" ]]; then
      export XDG_DATA_HOME="${OUT}/xdg/data" XDG_CACHE_HOME="${OUT}/xdg/cache" \
        XDG_STATE_HOME="${OUT}/xdg/state" XDG_CONFIG_HOME="${OUT}/xdg/config"
      mkdir -p "${XDG_DATA_HOME}" "${XDG_CACHE_HOME}" "${XDG_STATE_HOME}" "${XDG_CONFIG_HOME}"
    fi
    cd "${REPO}"
    {
      echo "=== $(date -u +%Y-%m-%dT%H:%M:%SZ) uid $(id -u) $(uname -srm)"
      rustc --version
      node --version
      pnpm --version
      "${KOMIKA_PYTHON}" --version
      pnpm install --frozen-lockfile
      bash "${SCRIPT_DIR}/installer-gate-build.sh" "${OUT}/build-facts.json"
    } 2>&1 | tee "${OUT}/build.log"
    # A new root every run, with a space and non-ASCII characters; the test creates it.
    ROOT="${OUT}/installer gate root é 漢字 $(date -u +%Y%m%d%H%M%S)"
    JSON="${OUT}/desktop-installer-gate-${ARCH}-linux.json"
    set +e
    TAURI_CONFIG='{"bundle":{"externalBin":[]}}' \
      KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE=1 \
      KOMIKA_INSTALLER_GATE_JSON="${JSON}" \
      KOMIKA_INSTALLER_GATE_ROOT="${ROOT}" \
      KOMIKA_INSTALLER_GATE_BUILD_JSON="${OUT}/build-facts.json" \
      cargo test --locked --manifest-path apps/admin/src-tauri/Cargo.toml \
      --features desktop-gate --test desktop_postgres_gate \
      -- --ignored --exact desktop_installer_gate --nocapture 2>&1 |
      tee "${OUT}/desktop-installer-gate-${ARCH}-linux.log"
    RC="${PIPESTATUS[0]}"
    set -e
    echo "=== installer gate exit status ${RC}; evidence: ${JSON}"
    echo "=== processes of this user that are left (none expected besides this shell):"
    ps -u "$(id -u)" -o pid,ppid,stat,args | grep -E 'postgres|komika-server' | grep -v grep || true
    exit "${RC}"
    ;;

  *)
    echo "usage: installer-gate-container.sh root-setup [uid] | run <output folder>" >&2
    exit 1
    ;;
esac
