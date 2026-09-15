#!/usr/bin/env bash
#
# Build the JVM runtime for the Android reader's on-device engine (arm64 only).
#
# Android has no `java` to start and an app may not exec one, so the reader loads a JVM
# into its own process (`dlopen` of libjvm.so, then JNI_CreateJavaVM) and runs the pinned
# Suwayomi jar there (src/suwayomi_inprocess_android.rs). This script makes what that
# needs, under ../jre-android/ (git-ignored). gen/android/app/build.gradle.kts adds both
# directories to the app when they exist; without them the APK has no engine.
#
#   jniLibs/arm64-v8a/*.so   HotSpot and the JDK's native libraries. Gradle packs them as
#                            the app's native libraries.
#   assets/engine/           Packed as APK assets, unpacked by the app on first start:
#     runtime/               java.home without the libraries: lib/modules (jlink, the same
#                            module set as the desktop runtime), tzdb, security, conf.
#     zstd-android.jar       One resource: the zstd-kmp native library built for Android.
#                            It goes on the class path BEFORE the Suwayomi jar (see below).
#     Suwayomi-Server.jar    The pinned jar (scripts/fetch-suwayomi-jar.sh) without the
#                            resources a phone cannot use. No class is removed or changed.
#
# Where the JVM comes from: Termux's `openjdk-21` package. It is OpenJDK 21 (GPLv2 with
# the Classpath Exception) plus about 40 patches for bionic, built with the NDK as a
# server VM with the JIT compilers, and its libraries are 16 KB aligned. The packages are
# pinned by SHA-256 below. Termux's package pool keeps only the newest build, so a 404
# here means the pin is old: move all four pins together and run the phone checks again.
#
# What this script changes in those files, and why:
#
#   * libandroid-spawn.so is replaced by scripts/android-jvm-shim.c. Termux's own copy is
#     4 KB aligned and needs libc++_shared.so. The stand-in also answers `dladdr` for
#     libjvm.so; that file says why.
#   * `libz.so.1` in the dependency list of libzip.so becomes `libz.so`, the system zlib.
#     An APK can only pack files named lib*.so.
#   * AWT, fonts, sound and the debug agents are left out. They need X11, FreeType or ALSA
#     libraries that no Android app has.
#
# zstd: OkHttp in the Suwayomi jar asks servers for zstd, and unpacks the answer with a
# native library it reads from the jar at jni/aarch64/libzstd-kmp.so. The one in the jar
# is built for glibc and cannot load on Android. zstd-kmp publishes the same library for
# Android (same version, same JNI functions); zstd-android.jar holds it at that path, so
# the class loader finds it first and the Suwayomi jar stays byte for byte as pinned.
#
# Requires: JDK 21 (jlink), the Android NDK (clang), curl, perl, zip, tar.
#
# Usage:  ./scripts/build-android-jvm.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT_DIR="${SRC_TAURI_DIR}/jre-android"
WORK_DIR="${SRC_TAURI_DIR}/.android-jvm-build"

TERMUX_REPO="https://packages.termux.dev/apt/termux-main"
JDK_VERSION="21.0.12"
# name|path under the repo|sha256
PACKAGES=(
  "openjdk-21|pool/main/o/openjdk-21/openjdk-21_${JDK_VERSION}_aarch64.deb|05b08dd961c30928992f87632539c3704bf082e2984aee11441c6127f9fd7884"
  "openjdk-21-x|pool/main/o/openjdk-21-x/openjdk-21-x_${JDK_VERSION}_aarch64.deb|e974be513dfd355bda6c9a793e69636298d3ac5f41e561fb8557e1909e979485"
  "libandroid-shmem|pool/main/liba/libandroid-shmem/libandroid-shmem_0.7_aarch64.deb|0da3a24d558b93c92bcf8d611e0826a99ff96e396b148e6cdf33b47c47c57ff6"
)
# Must be the zstd-kmp version the pinned Suwayomi jar carries (okhttp-zstd 5.4.0 -> 0.4.0).
ZSTD_AAR_URL="https://repo1.maven.org/maven2/com/squareup/zstd/zstd-kmp-android/0.4.0/zstd-kmp-android-0.4.0.aar"
ZSTD_AAR_SHA256="2778570454ae78026df42ace7168b8a6500f2521a5d497f2ab2fc0f804247826"

# The JDK libraries the engine loads. Everything else in the package is left out.
JDK_LIBS=(java jimage net nio verify zip extnet prefs management management_ext rmi syslookup jaas j2gss j2pkcs11)
# Keep in step with MODULES in build-jre.sh: one validated set for every platform.
MODULES="java.base,java.desktop,java.sql,java.sql.rowset,java.naming,java.management,java.instrument,java.logging,java.net.http,java.scripting,java.security.jgss,java.security.sasl,java.xml,java.xml.crypto,java.transaction.xa,java.datatransfer,java.compiler,java.rmi,java.prefs,jdk.crypto.ec,jdk.crypto.cryptoki,jdk.unsupported,jdk.zipfs,jdk.net,jdk.httpserver,jdk.management,jdk.jsobject,jdk.dynalink,jdk.charsets,jdk.localedata,jdk.security.auth,jdk.security.jgss"
# posix_spawn is in bionic from API 28 (the stand-in for libandroid-spawn.so relies on it).
ANDROID_API=28

JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21}"
JLINK="${JAVA_HOME}/bin/jlink"
[[ -x "${JLINK}" ]] || { echo "error: jlink not found at ${JLINK}; set JAVA_HOME to a JDK 21" >&2; exit 1; }
JDK_FEATURE="$("${JAVA_HOME}/bin/java" -version 2>&1 | head -n1 | sed -E 's/^.*version "([0-9]+).*$/\1/')"
[[ "${JDK_FEATURE}" == "21" ]] || { echo "error: JDK 21 required for jlink, got ${JDK_FEATURE}" >&2; exit 1; }

NDK_HOME="${NDK_HOME:-${ANDROID_NDK_HOME:-}}"
[[ -n "${NDK_HOME}" ]] || { echo "error: set NDK_HOME to the Android NDK" >&2; exit 1; }
NDK_BIN="$(echo "${NDK_HOME}"/toolchains/llvm/prebuilt/*/bin)"
CLANG="${NDK_BIN}/aarch64-linux-android${ANDROID_API}-clang"
READELF="${NDK_BIN}/llvm-readelf"
[[ -x "${CLANG}" ]] || { echo "error: NDK clang not found at ${CLANG}" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
else
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
fi

# fetch <url> <file> <sha256>: download unless a matching file is already there.
fetch() {
  local url="$1" file="$2" want="$3"
  if [[ ! -f "${file}" || "$(sha256_of "${file}")" != "${want}" ]]; then
    echo "fetch ${url}"
    curl -fL --retry 3 -o "${file}.part" "${url}"
    mv "${file}.part" "${file}"
  fi
  local got
  got="$(sha256_of "${file}")"
  [[ "${got}" == "${want}" ]] || { echo "error: sha256 of ${file} is ${got}, want ${want}" >&2; exit 1; }
}

mkdir -p "${WORK_DIR}/dl"
PREFIX="data/data/com.termux/files/usr"
for entry in "${PACKAGES[@]}"; do
  IFS='|' read -r name path sha <<<"${entry}"
  deb="${WORK_DIR}/dl/$(basename "${path}")"
  fetch "${TERMUX_REPO}/${path}" "${deb}" "${sha}"
  rm -rf "${WORK_DIR}/${name}"
  mkdir -p "${WORK_DIR}/${name}"
  # A .deb is an `ar` archive; bsdtar and GNU tar both read it.
  tar -xf "${deb}" -C "${WORK_DIR}/${name}" data.tar.xz
  tar -xf "${WORK_DIR}/${name}/data.tar.xz" -C "${WORK_DIR}/${name}"
  rm "${WORK_DIR}/${name}/data.tar.xz"
done
fetch "${ZSTD_AAR_URL}" "${WORK_DIR}/dl/zstd-kmp-android.aar" "${ZSTD_AAR_SHA256}"

JDK="${WORK_DIR}/openjdk-21/${PREFIX}/lib/jvm/java-21-openjdk"
JDK_X="${WORK_DIR}/openjdk-21-x/${PREFIX}/lib/jvm/java-21-openjdk"

echo "jlink (${JDK_VERSION}, linux-aarch64 modules)"
rm -rf "${WORK_DIR}/image"
"${JLINK}" \
  --module-path "${JDK}/jmods:${JDK_X}/jmods" \
  --add-modules "${MODULES}" \
  --strip-debug \
  --no-header-files \
  --no-man-pages \
  --compress=zip-9 \
  --output "${WORK_DIR}/image"
chmod -R u+w "${WORK_DIR}/image"

rm -rf "${OUT_DIR:?}/jniLibs" "${OUT_DIR:?}/assets"
LIBS="${OUT_DIR}/jniLibs/arm64-v8a"
ENGINE="${OUT_DIR}/assets/engine"
mkdir -p "${LIBS}" "${ENGINE}/runtime/lib"

# From the package, not from the jlink image: the copies inside the jmods are not stripped
# (libjvm.so is 5 MB larger there).
cp "${JDK}/lib/server/libjvm.so" "${LIBS}/"
for lib in "${JDK_LIBS[@]}"; do
  cp "${JDK}/lib/lib${lib}.so" "${LIBS}/"
done
cp "${WORK_DIR}/libandroid-shmem/${PREFIX}/lib/libandroid-shmem.so" "${LIBS}/"
"${CLANG}" -O2 -Wall -Werror -shared -Wl,-soname,libandroid-spawn.so \
  -o "${LIBS}/libandroid-spawn.so" "${SCRIPT_DIR}/android-jvm-shim.c" -ldl

# libz.so.1 -> libz.so, in place: the new name is shorter, so the string table keeps its size.
for lib in "${LIBS}"/*.so; do
  if "${READELF}" -d "${lib}" | grep -q 'NEEDED.*\[libz\.so\.1\]'; then
    perl -0pi -e 's/libz\.so\.1\0/libz.so\0\0\0/' "${lib}"
  fi
done
chmod 644 "${LIBS}"/*.so

# Every dependency must be one of these libraries or a library Android gives to apps, and
# every library must load on a 16 KB page phone.
ALLOWED=" libc.so libm.so libdl.so liblog.so libz.so "
for lib in "${LIBS}"/*.so; do
  for need in $("${READELF}" -d "${lib}" | sed -n 's/.*NEEDED.*\[\(.*\)\]/\1/p'); do
    if [[ ! -f "${LIBS}/${need}" && "${ALLOWED}" != *" ${need} "* ]]; then
      echo "error: $(basename "${lib}") needs ${need}, which the app will not have" >&2
      exit 1
    fi
  done
  for align in $("${READELF}" -lW "${lib}" | awk '$1=="LOAD"{print $NF}' | sort -u); do
    if (( align < 0x4000 )); then
      echo "error: $(basename "${lib}") has a LOAD segment aligned to ${align} (16 KB pages need 0x4000)" >&2
      exit 1
    fi
  done
done

cp "${WORK_DIR}/image/lib/modules" "${WORK_DIR}/image/lib/tzdb.dat" "${ENGINE}/runtime/lib/"
cp -R "${WORK_DIR}/image/lib/security" "${ENGINE}/runtime/lib/"
cp -R "${WORK_DIR}/image/conf" "${ENGINE}/runtime/"
# -L: jlink links repeated licence files to one copy, and an APK cannot hold links.
cp -RL "${WORK_DIR}/image/legal" "${ENGINE}/runtime/"
cp "${WORK_DIR}/image/release" "${ENGINE}/runtime/"

rm -rf "${WORK_DIR}/zstd"
mkdir -p "${WORK_DIR}/zstd/jni/aarch64"
unzip -q -o -j "${WORK_DIR}/dl/zstd-kmp-android.aar" 'jni/arm64-v8a/libzstd-kmp.so' -d "${WORK_DIR}/zstd/jni/aarch64"
(cd "${WORK_DIR}/zstd" && zip -q -X -r "${ENGINE}/zstd-android.jar" jni)

# The engine jar: the pinned jar minus resources that are dead weight on Android. The
# same four go from the iOS copy (scripts/prune-ios-jar.sh lists who reads each one):
#   font/**    38 MB  drawn with AWT only, and this runtime has no AWT libraries
#   native/**   7 MB  webp-imageio for desktop Linux (glibc), macOS and Windows
#   jni/**      4 MB  zstd-kmp for the same three; zstd-android.jar has the Android one
#   WebUI.zip   3 MB  the web UI, which server.conf switches off
# The iOS copy cannot be used here: it also swaps the zstd loader class for an iOS one.
PINNED_JAR="${SRC_TAURI_DIR}/suwayomi/Suwayomi-Server.jar"
[[ -f "${PINNED_JAR}" ]] || { echo "error: ${PINNED_JAR} missing; run scripts/fetch-suwayomi-jar.sh first" >&2; exit 1; }
cp "${PINNED_JAR}" "${ENGINE}/Suwayomi-Server.jar"
# `zip -d` matches `*` across `/`, and rewrites the archive without those entries.
zip -q -d "${ENGINE}/Suwayomi-Server.jar" 'font/*' 'native/*' 'jni/*' 'WebUI.zip'
unzip -Z1 "${ENGINE}/Suwayomi-Server.jar" 'suwayomi/tachidesk/MainKt.class' >/dev/null ||
  { echo "error: the pruned jar lost its main class" >&2; exit 1; }

# aapt leaves out asset files and directories whose name starts with `.` or `_`.
SKIPPED="$(find "${ENGINE}" \( -name '.*' -o -name '_*' \) | head -n1)"
[[ -z "${SKIPPED}" ]] || { echo "error: aapt will not pack ${SKIPPED}" >&2; exit 1; }

{
  printf 'jdk=%s (Termux openjdk-21)\n' "${JDK_VERSION}"
  for entry in "${PACKAGES[@]}"; do
    IFS='|' read -r name path sha <<<"${entry}"
    printf 'package=%s sha256=%s\n' "$(basename "${path}")" "${sha}"
  done
  printf 'zstd=%s sha256=%s\n' "$(basename "${ZSTD_AAR_URL}")" "${ZSTD_AAR_SHA256}"
  printf 'modules=%s\n' "${MODULES}"
} >"${OUT_DIR}/MANIFEST"

mb() { du -sk "$1" | awk '{printf "%.1f", $1/1024}'; }
echo "ok: ${OUT_DIR}"
echo "  jniLibs/arm64-v8a  $(mb "${LIBS}") MB ($(ls "${LIBS}" | wc -l | tr -d ' ') libraries)"
echo "  assets/engine      $(mb "${ENGINE}") MB (runtime $(mb "${ENGINE}/runtime") MB, jar $(mb "${ENGINE}/Suwayomi-Server.jar") MB)"
