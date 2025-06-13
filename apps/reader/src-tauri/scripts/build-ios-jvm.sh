#!/usr/bin/env bash
#
# Build an interpreter-only (Zero, NO-JIT — legal on iOS) OpenJDK runtime for iOS arm64,
# capable of hosting the stock Suwayomi-Server jar in-process via the JNI Invocation API.
#
# WHY BUILD, NOT FETCH (N4.1 findings): no clean, general-purpose prebuilt iOS-arm64 Zero
# `libjvm` exists to download. Gluon publishes only GraalVM-AOT / Substrate artifacts
# (download2.gluonhq.com/mobile) which AOT-compile ONE fixed app and CANNOT host an
# arbitrary stock jar in interpreter mode. PojavLauncher's iOS JVMs rely on JIT (jailbreak/
# debugger) and are not interpreter-only. So the only reproducible, license-clean path is to
# build the HotSpot Zero interpreter from OpenJDK's official Mobile Project. See
# openjdk/mobile README "Build static image for iOS".
#
# OUTPUT: a static-libs image at ../jre-ios/aarch64-ios/ (gitignored, never committed):
#   lib/zero/libjvm.a           <- the interpreter VM
#   lib/*.a                     <- static JDK native libs (libjava.a, libnio.a, libnet.a,
#                                  libzip.a, ...) linked into the iOS app binary
#   lib/modules                 <- the class-library runtime jimage (java.home boots this)
#   lib/{tzdb.dat,security,jfr,jvm.cfg}, conf/, release  <- runtime support files
# On iOS these are STATIC libraries: the Rust/Xcode link step statically links them into the
# app binary and calls JNI_CreateJavaVM directly (iOS forbids app-loaded .dylibs for this).
#
# REQUIREMENTS (hard):
#   * Apple-Silicon macOS + Xcode (iPhoneOS SDK). Validated on Xcode 26.6 / iOS SDK 26.5.
#   * A Boot JDK whose feature version is N-2..N of the pinned tree. The pinned commit is
#     JDK 27-dev, so the boot JDK MUST be 25, 26, or 27 (configure enforces this; the desktop
#     recipe's JDK 21 is REJECTED). Point BOOT_JDK at a suitable install (e.g. Temurin 26 GA).
#     This script will fetch a pinned Temurin boot JDK if BOOT_JDK is unset and
#     KOMIKA_FETCH_BOOT_JDK=1.
#   * autoconf (brew install autoconf) — OpenJDK's configure needs it.
#   * The Gluon "mobile-support" zip (prebuilt iOS libffi + cups headers), fetched + verified
#     below. Zero needs libffi to call native functions (it has no template interpreter).
#
# Usage:
#   ./scripts/build-ios-jvm.sh                     # uses $BOOT_JDK (must be JDK 25/26/27)
#   BOOT_JDK=/path/to/jdk-26 ./scripts/build-ios-jvm.sh
#   KOMIKA_FETCH_BOOT_JDK=1 ./scripts/build-ios-jvm.sh   # auto-fetch pinned Temurin 26
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# ---- Pins (supply-chain: never float; verify SHA where we can) -----------------------------
# openjdk/mobile commit that has the iOS `static-libs-image` target (JDK-8346233) and builds
# the Zero interpreter static libjvm. Bump deliberately; a bump likely changes the required
# boot-JDK feature version (see REQUIREMENTS above).
#
# !!! PIN RATIONALE (N4.1 finding) !!!  mainline HEAD is NOT a green iOS build point.
# openjdk/mobile auto-merges jdk:master, and on 2026-03-17 the auto-merge of JDK-8359706
# ("Add file descriptor count to VM.info") introduced an UNGUARDED `#include <libproc.h>` +
# proc_pidinfo into src/hotspot/os/bsd/os_bsd.cpp — libproc.h ships in the macOS SDK but NOT
# the iPhoneOS SDK, so every mobile commit from 2026-03-17 onward fails iOS compile at
# `os_bsd.cpp: fatal error: 'libproc.h' file not found` (verified at HEAD feb2de9b27de).
#
# The pin below is the LAST mobile-project iOS fix before that drift:
#   7e8729ef9298 2026-03-16 Johan Vos "8380088: IOS: jit_write_protect_np not allowed"
# At this commit the iOS patch series is intact — the mobile project's guard macro is
# `__IOS__` (set by configure via -D__IOS__ for the ios target, flags-cflags.m4): os_bsd.cpp
# carries __IOS__ guards, os_perf_bsd.cpp is entirely `#ifndef __IOS__`, and
# memMapPrinter_macosx.cpp is `#if (defined(__APPLE__) && !defined(__IOS__))`. Tree version:
# JDK 27-dev (make/conf/version-numbers.conf: DEFAULT_ACCEPTABLE_BOOT_VERSIONS="25 26 27"),
# so the Temurin 26 boot below is valid. When bumping past this pin, either the upstream
# mobile project has re-fixed iOS (look for iOS commits after 2026-03-16) or you must carry
# an __IOS__-guard patch for the new unguarded libproc call sites.
#
# ONE vendored patch is applied on top of the pin (MOBILE_PATCHES below):
#   ios-jvm-8380150-javacallarguments.patch — VERBATIM upstream cherry-pick of mobile/jdk
#   commit 3ad532135e47 (2026-03-19, Kim Barrett, "8380150: JavaCallArguments defined too
#   late", Reviewed-by: stefank, ayang). One-line removal of `#include "runtime/os.hpp"`
#   from os_cpu/bsd_zero/atomicAccess_bsd_zero.hpp. Without it, the Zero PCH fails at the
#   pin ("os.hpp:158 unknown type name 'JavaCallArguments'") because the fix landed on
#   master two days AFTER the libproc drift — there is NO patch-free green commit on
#   mobile master (<=2026-03-16 has the PCH bug, >=2026-03-17 has the libproc drift).
#   Provenance: unmodified upstream commit from the same GPLv2+CE repo; vendoring the
#   .patch (not forking) keeps the recipe reproducible and the license posture unchanged.
MOBILE_REPO="https://github.com/openjdk/mobile.git"
MOBILE_COMMIT="7e8729ef9298b86f90ceb5fb67dcd8008b047820"   # JDK 27-dev; last green iOS point before the 2026-03-17 libproc drift
MOBILE_PATCHES=("${SCRIPT_DIR}/ios-jvm-8380150-javacallarguments.patch")

# Gluon-published prebuilt iOS libffi + cups (headers/static libs) referenced by the
# openjdk/mobile README. SHA-256 verified before use.
SUPPORT_URL="https://download2.gluonhq.com/mobile/mobile-support-20250106.zip"
SUPPORT_SHA256="5793dd8700612fe0c5a1cbbfb280464167aea5944b3c01f32b340542976e9440"

# Optional auto-fetch boot JDK (Temurin 26 GA, macOS aarch64). Only used when
# KOMIKA_FETCH_BOOT_JDK=1 and BOOT_JDK is unset.
BOOT_JDK_URL="https://github.com/adoptium/temurin26-binaries/releases/download/jdk-26.0.1%2B8/OpenJDK26U-jdk_aarch64_mac_hotspot_26.0.1_8.tar.gz"
BOOT_JDK_SHA256="10c436258e24693ce8baf13dbffce98b77275797455e8b72510967547f13234a"   # Adoptium-published sha256.txt for the release above

# "Linker JDK": a host-runnable JDK with the SAME FEATURE VERSION as the pinned tree (27),
# used ONLY to run jmod+jlink and produce the target's `lib/modules` jimage. Needed because:
#   * the ios config cannot `make jdk-image` — dynamic libjvm linking for the ios target is
#     unsupported (Apple ld rejects the GNU-only `-soname` flag; mobile only supports
#     static-libs for iOS), so the tree's own jlink never gets built/run;
#   * the boot JDK (26) cannot parse the tree's v71 (JDK 27) classfiles;
#   * a macOS-hosted build of the same tree needs the Xcode Metal toolchain component,
#     which is not a reasonable prerequisite for this recipe.
# Temurin 27 EA, macOS aarch64. Adoptium-published SHA-256.
LINK_JDK_URL="https://github.com/adoptium/temurin27-binaries/releases/download/jdk-27%2B30-ea-beta/OpenJDK27U-jdk_aarch64_mac_hotspot_27_30-ea.tar.gz"
LINK_JDK_SHA256="7c14a75ec4a0eeb7db30379975089ff8541db9209a18ab68df910143d85b41ce"

CONF_NAME="ios-aarch64-zero-release"
OUT_DIR="${SRC_TAURI_DIR}/jre-ios/aarch64-ios"
WORK_DIR="${KOMIKA_IOS_JVM_WORKDIR:-${SRC_TAURI_DIR}/.ios-jvm-build}"   # large; keep out of git

# ---- Preconditions -------------------------------------------------------------------------
[[ "$(uname -s)" == "Darwin" ]] || { echo "error: iOS JVM must be built on macOS" >&2; exit 1; }
command -v autoconf >/dev/null 2>&1 || { echo "error: autoconf missing (brew install autoconf)" >&2; exit 1; }

SYSROOT="$(xcrun --sdk iphoneos --show-sdk-path)"
[[ -d "${SYSROOT}" ]] || { echo "error: iPhoneOS SDK not found (install Xcode)" >&2; exit 1; }
echo "iPhoneOS sysroot: ${SYSROOT}"

compute_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}
verify_sha256() {
  local file="$1" want="$2" got
  got="$(compute_sha256 "${file}")"
  if [[ "${got}" != "${want}" ]]; then
    echo "error: SHA-256 mismatch for ${file}" >&2
    echo "  expected: ${want}" >&2
    echo "  actual:   ${got}" >&2
    return 1
  fi
}

mkdir -p "${WORK_DIR}"

# ---- Boot JDK ------------------------------------------------------------------------------
if [[ -z "${BOOT_JDK:-}" && "${KOMIKA_FETCH_BOOT_JDK:-0}" == "1" ]]; then
  echo "fetching pinned Temurin 26 boot JDK"
  BOOT_TGZ="${WORK_DIR}/boot-jdk.tar.gz"
  curl -fSL --retry 3 -o "${BOOT_TGZ}" "${BOOT_JDK_URL}"
  [[ -n "${BOOT_JDK_SHA256}" ]] && verify_sha256 "${BOOT_TGZ}" "${BOOT_JDK_SHA256}"
  rm -rf "${WORK_DIR}/boot-jdk"; mkdir -p "${WORK_DIR}/boot-jdk"
  tar xzf "${BOOT_TGZ}" -C "${WORK_DIR}/boot-jdk"
  BOOT_JDK="$(dirname "$(dirname "$(find "${WORK_DIR}/boot-jdk" -name java -path '*/bin/java' | head -1)")")"
fi
[[ -n "${BOOT_JDK:-}" ]] || { echo "error: set BOOT_JDK to a JDK 25/26/27 install, or pass KOMIKA_FETCH_BOOT_JDK=1" >&2; exit 1; }
[[ -x "${BOOT_JDK}/bin/java" ]] || { echo "error: no bin/java under BOOT_JDK=${BOOT_JDK}" >&2; exit 1; }
echo "boot JDK: $("${BOOT_JDK}/bin/java" -version 2>&1 | head -1)"

# ---- iOS libffi + cups support -------------------------------------------------------------
SUPPORT_ZIP="${WORK_DIR}/mobile-support.zip"
if [[ ! -f "${SUPPORT_ZIP}" ]] || ! verify_sha256 "${SUPPORT_ZIP}" "${SUPPORT_SHA256}" 2>/dev/null; then
  echo "fetching iOS libffi/cups support"
  curl -fSL --retry 3 -o "${SUPPORT_ZIP}" "${SUPPORT_URL}"
  verify_sha256 "${SUPPORT_ZIP}" "${SUPPORT_SHA256}"
fi
SUPPORT_DIR="${WORK_DIR}/support"
rm -rf "${SUPPORT_DIR}"; mkdir -p "${SUPPORT_DIR}"
unzip -q -o "${SUPPORT_ZIP}" -d "${SUPPORT_DIR}"
# The archive nests a top-level "support/" dir; locate the real one containing libffi/.
SUP="$(dirname "$(find "${SUPPORT_DIR}" -type d -name libffi | head -1)")"
[[ -d "${SUP}/libffi/include" && -d "${SUP}/libffi/libs" ]] || { echo "error: libffi not found in support zip" >&2; exit 1; }
echo "support: ${SUP}"

# ---- Clone/checkout openjdk/mobile at the pin ----------------------------------------------
# Shallow-fetch the exact pinned SHA (GitHub serves arbitrary SHAs to depth-1 fetches);
# a full openjdk/mobile clone is multi-GB and unnecessary.
MOBILE_DIR="${WORK_DIR}/mobile"
if [[ ! -d "${MOBILE_DIR}/.git" ]]; then
  echo "shallow-fetching openjdk/mobile @ ${MOBILE_COMMIT}"
  mkdir -p "${MOBILE_DIR}"
  git -C "${MOBILE_DIR}" init -q
  git -C "${MOBILE_DIR}" remote add origin "${MOBILE_REPO}"
fi
git -C "${MOBILE_DIR}" remote set-url origin "${MOBILE_REPO}"
git -C "${MOBILE_DIR}" cat-file -e "${MOBILE_COMMIT}^{commit}" 2>/dev/null \
  || git -C "${MOBILE_DIR}" fetch --depth 1 origin "${MOBILE_COMMIT}"
# -f discards any previously-applied patches, so re-runs are idempotent.
git -C "${MOBILE_DIR}" checkout -qf "${MOBILE_COMMIT}"
for p in "${MOBILE_PATCHES[@]}"; do
  echo "applying vendored patch: $(basename "${p}")"
  git -C "${MOBILE_DIR}" apply --verbose "${p}"
done

# ---- Configure (Zero is the default JVM variant for the ios target) ------------------------
echo "configuring ${CONF_NAME}"
( cd "${MOBILE_DIR}" && bash configure \
    --disable-warnings-as-errors \
    --openjdk-target=aarch64-macos-ios \
    --with-boot-jdk="${BOOT_JDK}" \
    --with-libffi-include="${SUP}/libffi/include" \
    --with-libffi-lib="${SUP}/libffi/libs" \
    --with-cups-include="${SUP}/cups-2.3.6" \
    --with-sysroot="${SYSROOT}" )

# ---- Build the static-libs image -----------------------------------------------------------
echo "building static-libs-image (this is a full JDK cross-build; expect tens of minutes)"
( cd "${MOBILE_DIR}" && make CONF="${CONF_NAME}" static-libs-image )

# ---- Collect static libs into the vendored bundle dir --------------------------------------
IMG="${MOBILE_DIR}/build/${CONF_NAME}/images/static-libs"
LIBJVM="${IMG}/lib/zero/libjvm.a"
[[ -f "${LIBJVM}" ]] || { echo "error: libjvm.a not produced at ${LIBJVM}" >&2; exit 1; }

rm -rf "${OUT_DIR}"; mkdir -p "${OUT_DIR}"
cp -R "${IMG}/lib" "${OUT_DIR}/lib"

# ---- Build the class-library runtime (lib/modules jimage) ----------------------------------
# `make static-libs-image` compiles every module's classes into the exploded image at
# build/<conf>/jdk/modules but produces no jimage (see LINK_JDK rationale above). We produce
# it with the pinned same-feature-version Linker JDK's jmod+jlink:
#   1. Package java.base as a jmod with --target-platform ios-aarch64 (the exact value the
#      real build would use: spec.gmk OPENJDK_MODULE_TARGET_PLATFORM). jlink requires the
#      ModuleTarget attribute on java.base for a cross-platform link, and exploded modules
#      lack it (it is normally injected at jmod-packaging time).
#   2. Run jlink with TWO java.base patches applied to the RUNNING jlink VM (not the image):
#      * jdk/internal/util/OperatingSystem.class from the ios build — teaches mainline jlink
#        the `ios` platform (a mobile-repo patch, JDK-8374522, not yet in mainline 27);
#      * jdk/internal/misc/resources/release.txt copied from the target — jlink refuses to
#        link when its own release string differs from the target java.base's; patching the
#        *running* side keeps the *image's* release string truthful.
#      The produced lib/modules therefore records the true ios-aarch64 target and the true
#      27-internal build string.
echo "fetching pinned Temurin 27 EA linker JDK (jmod/jlink host tooling)"
LINK_TGZ="${WORK_DIR}/link-jdk.tar.gz"
if [[ ! -f "${LINK_TGZ}" ]] || ! verify_sha256 "${LINK_TGZ}" "${LINK_JDK_SHA256}" 2>/dev/null; then
  curl -fSL --retry 3 -o "${LINK_TGZ}" "${LINK_JDK_URL}"
  verify_sha256 "${LINK_TGZ}" "${LINK_JDK_SHA256}"
fi
rm -rf "${WORK_DIR}/link-jdk"; mkdir -p "${WORK_DIR}/link-jdk"
tar xzf "${LINK_TGZ}" -C "${WORK_DIR}/link-jdk"
LINK_JDK="$(dirname "$(dirname "$(find "${WORK_DIR}/link-jdk" -name java -path '*/bin/java' | head -1)")")"

EXPLODED="${MOBILE_DIR}/build/${CONF_NAME}/jdk"
[[ -d "${EXPLODED}/modules/java.base" ]] || { echo "error: exploded modules missing at ${EXPLODED}/modules" >&2; exit 1; }

JIMAGE_WORK="${WORK_DIR}/jimage-work"
rm -rf "${JIMAGE_WORK}"; mkdir -p "${JIMAGE_WORK}/jmods" "${JIMAGE_WORK}/jb-patch/jdk/internal/util" "${JIMAGE_WORK}/jb-patch/jdk/internal/misc/resources"
cp "${EXPLODED}/modules/java.base/jdk/internal/util/OperatingSystem.class" "${JIMAGE_WORK}/jb-patch/jdk/internal/util/"
cp "${EXPLODED}/modules/java.base/jdk/internal/misc/resources/release.txt" "${JIMAGE_WORK}/jb-patch/jdk/internal/misc/resources/"
"${LINK_JDK}/bin/jmod" create --class-path "${EXPLODED}/modules/java.base" \
    --target-platform ios-aarch64 "${JIMAGE_WORK}/jmods/java.base.jmod"
"${LINK_JDK}/bin/jlink" -J--patch-module=java.base="${JIMAGE_WORK}/jb-patch" \
    --module-path "${JIMAGE_WORK}/jmods:${EXPLODED}/modules" \
    --add-modules ALL-MODULE-PATH --output "${JIMAGE_WORK}/image"
[[ -f "${JIMAGE_WORK}/image/lib/modules" ]] || { echo "error: jlink did not produce lib/modules" >&2; exit 1; }
cp "${JIMAGE_WORK}/image/lib/modules" "${OUT_DIR}/lib/modules"

# Runtime support files the VM/class-library read from java.home (tz data, security config,
# JFR profiles, jvm.cfg) — taken from the target build's exploded image, same revision.
cp -R "${EXPLODED}/lib/security" "${OUT_DIR}/lib/security"
cp -R "${EXPLODED}/lib/jfr" "${OUT_DIR}/lib/jfr"
cp "${EXPLODED}/lib/tzdb.dat" "${EXPLODED}/lib/jvm.cfg" "${OUT_DIR}/lib/"
cp -R "${EXPLODED}/conf" "${OUT_DIR}/conf"
cp "${EXPLODED}/release" "${OUT_DIR}/release"

# ---- Sanity: confirm the mach-o is arm64 iOS -----------------------------------------------
echo "=== libjvm.a arch/platform ==="
lipo -info "${LIBJVM}" || true
TMP_OBJ_DIR="$(mktemp -d)"
( cd "${TMP_OBJ_DIR}" && ar -x "${LIBJVM}" 2>/dev/null; OBJ="$(ls *.o 2>/dev/null | head -1)"; \
  [[ -n "${OBJ}" ]] && vtool -show-build "${OBJ}" 2>&1 | grep -iE 'platform|IPHONEOS|minos' || true )
rm -rf "${TMP_OBJ_DIR}"

SIZE_MB=$(( $(du -sk "${OUT_DIR}" | awk '{print $1}') / 1024 ))
echo "ok: iOS Zero JVM static-libs image at ${OUT_DIR} (${SIZE_MB} MB)"
echo "NOTE: on-device in-process boot of Suwayomi-Server.jar via JNI_CreateJavaVM is N4.2"
echo "      (device + JNI link work); it cannot be exercised on the mac host."
