#!/usr/bin/env bash
#
# Build the zstd decoder for the iOS on-device engine.
#
# WHY: extensions ask sites for zstd (the Keiyoushi base class adds OkHttp's
# CompressionInterceptor with Zstd, and reads a zstd cache file of its own). The decoder
# in the Suwayomi jar is zstd-kmp: Kotlin classes over a JNI library. The jar carries that
# library for Linux, macOS and Windows only, and its loader throws "Unsupported OS: darwin"
# on a phone. So the first call to such a source failed (found on an iPhone, 2026-10-06).
#
# WHAT THIS MAKES, both under jre-ios/aarch64-ios (git-ignored, like the rest of it):
#
#   lib/libzstdkmp.a   The same C and JNI sources that zstd-kmp builds for desktops
#                      (zstd-kmp/build.zig at the pinned commit), compiled for iOS arm64,
#                      plus JNI_OnLoad_zstdkmp (scripts/ios-zstd/onload.c). One object;
#                      only the JNI functions are global, so zstd's own names cannot
#                      collide with another copy of zstd in the app. Linked into the app
#                      with -force_load (FORCE_LOAD_LIBS in stage-ios-jvm-runtime.sh).
#   zstd/com/squareup/zstd/Zstd__JvmZstdKt.class
#                      The loader for iOS (scripts/ios-zstd/…/Zstd__JvmZstdKt.java).
#                      scripts/prune-ios-jar.sh puts it into the iOS jar.
#
# The Android engine has the same gap and closes it with zstd-kmp's own Android library
# (build-android-jvm.sh). zstd-kmp publishes no JNI library for iOS, so this one is built.
#
# Usage:  ./scripts/build-ios-zstd.sh
# Needs:  Xcode (iphoneos SDK) and the pinned linker JDK that build-ios-jvm.sh fetches
#         (.ios-jvm-build/link-jdk), or JAVAC=/path/to/javac.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT_DIR="${SRC_TAURI_DIR}/jre-ios/aarch64-ios"
WORK_DIR="${KOMIKA_IOS_JVM_WORKDIR:-${SRC_TAURI_DIR}/.ios-jvm-build}/zstd"
SHIM_DIR="${SCRIPT_DIR}/ios-zstd"

# Must be the zstd-kmp version the pinned Suwayomi jar carries (okhttp-zstd 5.4.0 -> 0.4.0,
# tag parent-0.4.0), and the zstd commit that tag has as its submodule. The last step
# checks the result against the jar's classes, so a wrong pin fails here.
ZSTD_KMP_COMMIT="4fbdf35df030888d2a0cce4a84329d107438134f"
ZSTD_KMP_SHA256="a6634bab3eb4a006dd89c7b1994617ff0bfd8fc291d4c69d270fccfbbb5843e4"
ZSTD_COMMIT="1dbc2e09084e843f6c0dcc2d0791610015c50979"
ZSTD_SHA256="b1bfea04c5763b1dc77ef3cdd6803daf4bf5d166285af8a64f53139e439c5bce"
IOS_TARGET="arm64-apple-ios15.0"

sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
fetch() {
  local url="$1" file="$2" want="$3"
  if [[ ! -f "${file}" || "$(sha256_of "${file}")" != "${want}" ]]; then
    curl -fSL --retry 3 -o "${file}" "${url}"
  fi
  local got
  got="$(sha256_of "${file}")"
  [[ "${got}" == "${want}" ]] || { echo "error: sha256 of ${file} is ${got}, want ${want}" >&2; exit 1; }
}

[[ -d "${OUT_DIR}/lib" ]] || {
  echo "error: ${OUT_DIR}/lib missing; build the iOS runtime first (scripts/build-ios-jvm.sh)" >&2
  exit 1
}
JAVAC="${JAVAC:-$(find "$(dirname "${WORK_DIR}")/link-jdk" -name javac -path '*/bin/javac' 2>/dev/null | head -1)}"
[[ -x "${JAVAC}" ]] || {
  echo "error: no javac. Run scripts/build-ios-jvm.sh --relink-modules once (it fetches the" >&2
  echo "  pinned linker JDK), or set JAVAC=/path/to/javac." >&2
  exit 1
}

mkdir -p "${WORK_DIR}/dl"
fetch "https://github.com/square/zstd-kmp/archive/${ZSTD_KMP_COMMIT}.tar.gz" "${WORK_DIR}/dl/zstd-kmp.tar.gz" "${ZSTD_KMP_SHA256}"
fetch "https://github.com/facebook/zstd/archive/${ZSTD_COMMIT}.tar.gz" "${WORK_DIR}/dl/zstd.tar.gz" "${ZSTD_SHA256}"
rm -rf "${WORK_DIR}/src" "${WORK_DIR}/obj" "${WORK_DIR}/classes"
mkdir -p "${WORK_DIR}/src" "${WORK_DIR}/obj" "${WORK_DIR}/classes"
tar -xzf "${WORK_DIR}/dl/zstd-kmp.tar.gz" -C "${WORK_DIR}/src"
tar -xzf "${WORK_DIR}/dl/zstd.tar.gz" -C "${WORK_DIR}/src"
KMP="${WORK_DIR}/src/zstd-kmp-${ZSTD_KMP_COMMIT}/zstd-kmp"
ZSTD="${WORK_DIR}/src/zstd-${ZSTD_COMMIT}/lib"

# The C files zstd-kmp/build.zig lists (no dictBuilder, no legacy), less the amd64 .S file.
C_SOURCES=()
while IFS= read -r rel; do
  [[ -f "${ZSTD}/${rel}" ]] || { echo "error: lib/${rel} (from build.zig) is not in the zstd sources" >&2; exit 1; }
  C_SOURCES+=("${ZSTD}/${rel}")
done < <(sed -n 's/^ *"\.\.\/zstd\/lib\/\([^"]*\.c\)",$/\1/p' "${KMP}/build.zig")
(( ${#C_SOURCES[@]} >= 20 )) || { echo "error: read ${#C_SOURCES[@]} C files from build.zig, expected 26" >&2; exit 1; }

CC=(xcrun --sdk iphoneos clang -target "${IOS_TARGET}" -Os -fvisibility=hidden -fno-common)
INCLUDES=(-I "${KMP}/native/include/share" -I "${KMP}/native/include/unix" -I "${ZSTD}")
echo "compile ${#C_SOURCES[@]} zstd files + ZstdKmp.cpp + onload.c (${IOS_TARGET})"
for src in "${C_SOURCES[@]}"; do
  "${CC[@]}" -std=gnu99 "${INCLUDES[@]}" -c "${src}" -o "${WORK_DIR}/obj/$(basename "${src%.c}").o"
done
"${CC[@]}" -x c++ -std=c++11 "${INCLUDES[@]}" -c "${KMP}/native/ZstdKmp.cpp" -o "${WORK_DIR}/obj/ZstdKmp.o"
"${CC[@]}" -std=gnu99 "${INCLUDES[@]}" -c "${SHIM_DIR}/onload.c" -o "${WORK_DIR}/obj/onload.o"

# One object. With -r, ld makes every name that is not exported local, so only the JNI
# functions stay global: nothing else in the app can clash with zstd's names.
"${CC[@]}" -r -nostdlib \
  -Wl,-exported_symbol,'_Java_com_squareup_zstd_*' -Wl,-exported_symbol,_JNI_OnLoad_zstdkmp \
  "${WORK_DIR}"/obj/*.o -o "${WORK_DIR}/zstdkmp.o"
rm -f "${OUT_DIR}/lib/libzstdkmp.a"
xcrun --sdk iphoneos ar crs "${OUT_DIR}/lib/libzstdkmp.a" "${WORK_DIR}/zstdkmp.o"

"${JAVAC}" --release 17 -d "${WORK_DIR}/classes" "${SHIM_DIR}/com/squareup/zstd/Zstd__JvmZstdKt.java"
rm -rf "${OUT_DIR}/zstd"
mkdir -p "${OUT_DIR}/zstd/com/squareup/zstd"
cp "${WORK_DIR}/classes/com/squareup/zstd/Zstd__JvmZstdKt.class" "${OUT_DIR}/zstd/com/squareup/zstd/"

# ---- Check against the jar ---------------------------------------------------------------
# Every `native` method of the jar's com.squareup.zstd classes must have its function in
# the library, and the library must export nothing but those and JNI_OnLoad_zstdkmp.
GLOBALS="$(xcrun nm -gU "${OUT_DIR}/lib/libzstdkmp.a" | awk 'NF==3 {print $3}' | sort)"
EXTRA="$(grep -v -e '^_Java_com_squareup_zstd_' -e '^_JNI_OnLoad_zstdkmp$' <<<"${GLOBALS}" || true)"
[[ -z "${EXTRA}" ]] || { echo "error: libzstdkmp.a exports more than the JNI functions:" >&2; echo "${EXTRA}" | head >&2; exit 1; }
grep -qx '_JNI_OnLoad_zstdkmp' <<<"${GLOBALS}" || { echo "error: libzstdkmp.a has no JNI_OnLoad_zstdkmp" >&2; exit 1; }

JAR="${SRC_TAURI_DIR}/suwayomi/Suwayomi-Server.jar"
if [[ -f "${JAR}" ]]; then
  rm -rf "${WORK_DIR}/jar"
  mkdir -p "${WORK_DIR}/jar"
  unzip -q -o "${JAR}" 'com/squareup/zstd/*.class' -d "${WORK_DIR}/jar"
  CLASSES=()
  while IFS= read -r f; do
    f="${f#"${WORK_DIR}/jar/"}"
    CLASSES+=("$(tr '/' '.' <<<"${f%.class}")")
  done < <(find "${WORK_DIR}/jar/com/squareup/zstd" -maxdepth 1 -name '*.class')
  WANT="$("$(dirname "${JAVAC}")/javap" -p -cp "${WORK_DIR}/jar" "${CLASSES[@]}" | awk '
    /^(public |final |abstract )*(class|interface) / { for (i = 1; i <= NF; i++) if ($i == "class" || $i == "interface") cls = $(i + 1) }
    / native / { name = $0; sub(/\(.*/, "", name); sub(/.* /, "", name); gsub(/\./, "_", cls); print "_Java_" cls "_" name }' | sort -u)"
  N_WANT="$(grep -c . <<<"${WANT}" || true)"
  (( N_WANT > 0 )) || { echo "error: found no native method in the jar's com.squareup.zstd classes" >&2; exit 1; }
  MISSING="$(comm -23 <(echo "${WANT}") <(echo "${GLOBALS}"))"
  [[ -z "${MISSING}" ]] || { echo "error: the jar needs these, libzstdkmp.a lacks them (wrong zstd-kmp pin?):" >&2; echo "${MISSING}" >&2; exit 1; }
  echo "ok: all ${N_WANT} native methods of the jar's zstd classes are in libzstdkmp.a"
else
  echo "warning: ${JAR} missing; the library was not checked against the jar" >&2
fi

echo "ok: ${OUT_DIR}/lib/libzstdkmp.a ($(( $(wc -c < "${OUT_DIR}/lib/libzstdkmp.a") / 1000 )) kB)"
echo "ok: ${OUT_DIR}/zstd/com/squareup/zstd/Zstd__JvmZstdKt.class"
echo "next: scripts/prune-ios-jar.sh, then scripts/stage-ios-jvm-runtime.sh"
