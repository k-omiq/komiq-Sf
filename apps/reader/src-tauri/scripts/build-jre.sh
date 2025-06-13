#!/usr/bin/env bash
#
# Build a minimal JRE for the embedded Suwayomi-Server sidecar via jlink.
#
# Output lands at ../jre/<slug>/ where <slug> matches Rust's std::env::consts pattern
# "<ARCH>-<OS>" (e.g. aarch64-macos) — exactly the path suwayomi.rs probes at runtime
# (<resource_dir>/jre/<slug>/bin/java). Only the host-target JRE is built per invocation;
# each platform's CI build produces and bundles its own.
#
# Requires JDK 21 with jlink. Defaults to Homebrew's openjdk@21; override with JAVA_HOME.
#
# Usage:  ./scripts/build-jre.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# tauri.conf.json chains this script into `beforeBuildCommand`, which runs for EVERY
# target — including the mobile ones. jlink emits a HOST-NATIVE runtime, so producing
# it for an iOS/Android build only risks re-creating the 218 MB mach-o leak recorded in
# Mobile carries its own runtime (jre-ios/, staged
# by scripts/stage-ios-jvm-runtime.sh), so no-op there. TAURI_ENV_PLATFORM is set by the
# tauri CLI for build hooks only; a direct or CI invocation leaves it unset and builds.
case "${TAURI_ENV_PLATFORM:-}" in
  ios | android)
    echo "skip: TAURI_ENV_PLATFORM=${TAURI_ENV_PLATFORM} ships its own runtime; no desktop JRE built"
    exit 0
    ;;
esac

JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21}"
JLINK="${JAVA_HOME}/bin/jlink"
JAVA="${JAVA_HOME}/bin/java"
if [[ ! -x "${JLINK}" ]]; then
  echo "error: jlink not found at ${JLINK}" >&2
  echo "  set JAVA_HOME to a JDK 21 install (has bin/jlink)." >&2
  exit 1
fi
if [[ ! -x "${JAVA}" ]]; then
  echo "error: java not found at ${JAVA}" >&2
  exit 1
fi

# Pin the toolchain that produces the runtime. The jar half of the sidecar is SHA-256
# pinned (suwayomi/VERSION + fetch-suwayomi-jar.sh), but the JRE that EXECUTES it used
# to be whatever JAVA_HOME pointed at: a JDK 17 or 24 silently yields a different module
# set / class files from the one MODULES below was validated against. The feature version
# is a hard requirement; the vendor is only recorded in the manifest below, because CI
# uses Temurin while the documented local default is Homebrew's openjdk@21.
JDK_FEATURE_REQUIRED=21
JAVA_VERSION_RAW="$("${JAVA}" -version 2>&1 | head -n1)"
JDK_FEATURE="$(printf '%s' "${JAVA_VERSION_RAW}" | sed -E 's/^.*version "([0-9]+).*$/\1/')"
if [[ "${JDK_FEATURE}" != "${JDK_FEATURE_REQUIRED}" ]]; then
  echo "error: JDK ${JDK_FEATURE_REQUIRED} required, got: ${JAVA_VERSION_RAW}" >&2
  echo "  JAVA_HOME=${JAVA_HOME}" >&2
  exit 1
fi

# Portable sha256 front-end (macOS ships shasum, Linux/Git-Bash ship sha256sum).
if command -v sha256sum >/dev/null 2>&1; then
  SHA256_CMD=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  SHA256_CMD=(shasum -a 256)
else
  echo "error: neither sha256sum nor shasum is available" >&2
  exit 1
fi

# Derive the target slug the way suwayomi.rs does: Rust ARCH + Rust OS.
uname_m="$(uname -m)"
case "${uname_m}" in
  arm64 | aarch64) ARCH="aarch64" ;;
  x86_64 | amd64)  ARCH="x86_64" ;;
  *) echo "error: unsupported arch '${uname_m}'" >&2; exit 1 ;;
esac
uname_s="$(uname -s)"
case "${uname_s}" in
  Darwin)             OS="macos" ;;
  Linux)              OS="linux" ;;
  MINGW* | MSYS* | CYGWIN* | Windows_NT) OS="windows" ;;
  *) echo "error: unsupported OS '${uname_s}'" >&2; exit 1 ;;
esac
SLUG="${ARCH}-${OS}"
OUT_DIR="${SRC_TAURI_DIR}/jre/${SLUG}"

echo "building minimal JRE"
echo "  JAVA_HOME: ${JAVA_HOME}"
echo "  target:    ${SLUG}"
echo "  output:    ${OUT_DIR}"

# jlink refuses to write into an existing directory.
rm -rf "${OUT_DIR}"
mkdir -p "$(dirname "${OUT_DIR}")"

# Module set for Suwayomi-Server: it runs Tachiyomi extensions and needs desktop (awt is
# loaded even headless), sql (H2), xml, scripting (Nashorn/JS via extensions), crypto,
# HTTP client, logging, management, naming, and instrumentation. This curated set was
# validated to boot the pinned jar (see the #[ignore]d sidecar_boots integration test).
# If a future Suwayomi release needs more, widen here (fallback: --add-modules ALL-MODULE-PATH).
MODULES="java.base,java.desktop,java.sql,java.sql.rowset,java.naming,java.management,java.instrument,java.logging,java.net.http,java.scripting,java.security.jgss,java.security.sasl,java.xml,java.xml.crypto,java.transaction.xa,java.datatransfer,java.compiler,java.rmi,java.prefs,jdk.crypto.ec,jdk.crypto.cryptoki,jdk.unsupported,jdk.zipfs,jdk.net,jdk.httpserver,jdk.management,jdk.jsobject,jdk.dynalink,jdk.charsets,jdk.localedata,jdk.security.auth,jdk.security.jgss"

"${JLINK}" \
  --add-modules "${MODULES}" \
  --strip-debug \
  --no-header-files \
  --no-man-pages \
  --compress=zip-9 \
  --output "${OUT_DIR}"

# jlink writes module legal notices read-only (mode 444). Tauri's bundler copies this
# tree into target/<profile>/jre and, on a later rebuild, `fs::copy` fails to overwrite
# those read-only files ("Permission denied"). Make the whole output user-writable so
# every re-copy succeeds.
chmod -R u+w "${OUT_DIR}"

# Sanity: the launcher the supervisor probes must exist.
JAVA_BIN="${OUT_DIR}/bin/java"
[[ "${OS}" == "windows" ]] && JAVA_BIN="${OUT_DIR}/bin/java.exe"
if [[ ! -x "${JAVA_BIN}" ]]; then
  echo "error: expected launcher missing after jlink: ${JAVA_BIN}" >&2
  exit 1
fi

# Report size and enforce a hard ceiling.
SIZE_KB="$(du -sk "${OUT_DIR}" | awk '{print $1}')"
SIZE_MB=$(( SIZE_KB / 1024 ))
echo "JRE built: ${SIZE_MB} MB (uncompressed on disk) at ${OUT_DIR}"

HARD_CEILING_MB=150
ASPIRATIONAL_MB=60
if (( SIZE_MB > HARD_CEILING_MB )); then
  echo "error: JRE is ${SIZE_MB} MB, exceeds hard ceiling of ${HARD_CEILING_MB} MB" >&2
  exit 1
fi
if (( SIZE_MB > ASPIRATIONAL_MB )); then
  echo "note: ${SIZE_MB} MB exceeds the aspirational ~${ASPIRATIONAL_MB} MB target; future module thinning could shrink it." >&2
fi

# Content digest of the jlink output: one hash over the sorted per-file digests of the
# whole tree. jlink is only as reproducible as the JDK that ran it, so this is what makes
# drift between builders VISIBLE (and, when a known-good value is supplied, fatal). xargs
# may batch, but the batches inherit the sorted order, so the line sequence is stable.
TREE_SHA256="$(
  cd "${OUT_DIR}" \
    && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 "${SHA256_CMD[@]}" | "${SHA256_CMD[@]}" | awk '{print $1}'
)"

# Provenance record for the built runtime, written OUTSIDE ${OUT_DIR} so it is never part
# of the tree it describes (re-hashing must reproduce the same digest). jre/ is gitignored,
# so this is a build artifact, not a committed pin; CI supplies the known-good value.
MANIFEST="${SRC_TAURI_DIR}/jre/${SLUG}.manifest"
{
  printf 'slug=%s\n' "${SLUG}"
  printf 'jdk=%s\n' "${JAVA_VERSION_RAW}"
  printf 'java_home=%s\n' "${JAVA_HOME}"
  printf 'size_mb=%s\n' "${SIZE_MB}"
  printf 'sha256=%s\n' "${TREE_SHA256}"
  printf 'modules=%s\n' "${MODULES}"
} > "${MANIFEST}"
echo "JRE manifest sha256=${TREE_SHA256} (${MANIFEST})"

# Opt-in enforcement: export KOMIKA_JRE_MANIFEST_SHA256 (per slug, from a previous known-
# good run) to turn silent toolchain drift into a build failure.
if [[ -n "${KOMIKA_JRE_MANIFEST_SHA256:-}" && "${KOMIKA_JRE_MANIFEST_SHA256}" != "${TREE_SHA256}" ]]; then
  echo "error: JRE manifest digest mismatch for ${SLUG}" >&2
  echo "  expected: ${KOMIKA_JRE_MANIFEST_SHA256}" >&2
  echo "  actual:   ${TREE_SHA256}  (built with: ${JAVA_VERSION_RAW})" >&2
  exit 1
fi

echo "ok: minimal JRE for ${SLUG} ready"
