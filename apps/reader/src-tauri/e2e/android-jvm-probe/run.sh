#!/usr/bin/env bash
#
# Prove the Android JVM runtime on a device or emulator, with no reader code involved.
#
# Builds a small debuggable app that holds only the libraries from ../../jre-android
# (scripts/build-android-jvm.sh) and probe.c, installs it, copies the runtime and the
# engine jar into its private storage, starts it, and then drives the engine over
# `adb forward`: version, extension install, chapter list, page list, one page image from
# a source that is not MangaDex. It checks what the reader depends on: `dlopen` of
# libjvm.so from the app's library dir on bionic, JNI_CreateJavaVM beside ART, the app
# sandbox (SELinux, 16 KB pages if the device has them), and the zstd overlay.
#
# The older ../android-jdk-probe ran `java -jar` on glibc in Docker. This one replaces it
# as evidence for Android.
#
# Usage:  ./run.sh [pruned|stock]      (device: $ANDROID_SERIAL, or the only one attached)
#   pruned  the jar the app ships (default)
#   stock   the pinned jar as downloaded
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${HERE}/../.." && pwd)"
RUNTIME="${SRC_TAURI_DIR}/jre-android"
ENGINE="${RUNTIME}/assets/engine"
case "${1:-pruned}" in
  pruned) JAR="${ENGINE}/Suwayomi-Server.jar" ;;
  stock) JAR="${SRC_TAURI_DIR}/suwayomi/Suwayomi-Server.jar" ;;
  *) echo "usage: run.sh [pruned|stock]" >&2; exit 64 ;;
esac

ANDROID_HOME="${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}"
NDK_HOME="${NDK_HOME:-$(ls -d "${ANDROID_HOME}"/ndk/* | tail -n1)}"
JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21}"
BUILD_TOOLS="$(ls -d "${ANDROID_HOME}"/build-tools/* | tail -n1)"
ANDROID_JAR="$(ls -d "${ANDROID_HOME}"/platforms/android-* | tail -n1)/android.jar"
CLANG="$(echo "${NDK_HOME}"/toolchains/llvm/prebuilt/*/bin)/aarch64-linux-android28-clang"
ADB="${ANDROID_HOME}/platform-tools/adb"
PKG=app.komika.jvmprobe
PORT=45679
TMP=/data/local/tmp/komiq-jvm-probe

[[ -f "${ENGINE}/runtime/lib/modules" ]] || { echo "error: run scripts/build-android-jvm.sh first" >&2; exit 1; }
[[ -f "${JAR}" ]] || { echo "error: ${JAR} missing" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
mkdir -p "${WORK}/classes" "${WORK}/pkg/lib/arm64-v8a"

echo "== build the probe app"
"${CLANG}" -O2 -Wall -Werror -shared -Wl,-soname,libprobe.so -o "${WORK}/pkg/lib/arm64-v8a/libprobe.so" "${HERE}/probe.c" -ldl -llog
cp "${RUNTIME}"/jniLibs/arm64-v8a/*.so "${WORK}/pkg/lib/arm64-v8a/"
"${JAVA_HOME}/bin/javac" -source 8 -target 8 -nowarn -Xlint:-options -cp "${ANDROID_JAR}" -d "${WORK}/classes" \
  "${HERE}/src/app/komika/jvmprobe/MainActivity.java"
PATH="${JAVA_HOME}/bin:${PATH}" "${BUILD_TOOLS}/d8" --lib "${ANDROID_JAR}" --min-api 28 --output "${WORK}/pkg" \
  $(find "${WORK}/classes" -name '*.class')
"${BUILD_TOOLS}/aapt2" link -o "${WORK}/base.apk" --manifest "${HERE}/AndroidManifest.xml" -I "${ANDROID_JAR}"
(cd "${WORK}/pkg" && zip -q -r ../base.apk classes.dex lib)
"${BUILD_TOOLS}/zipalign" -f -P 16 4 "${WORK}/base.apk" "${WORK}/aligned.apk"
PATH="${JAVA_HOME}/bin:${PATH}" "${BUILD_TOOLS}/apksigner" sign --ks "${HOME}/.android/debug.keystore" \
  --ks-pass pass:android --out "${WORK}/probe.apk" "${WORK}/aligned.apk"

echo "== device"
for prop in ro.product.manufacturer ro.product.model ro.build.version.release ro.build.version.sdk ro.product.cpu.abilist; do
  echo "${prop}=$("${ADB}" shell getprop "${prop}" | tr -d '\r')"
done
echo "page size=$("${ADB}" shell getconf PAGE_SIZE | tr -d '\r')"
"${ADB}" shell dumpsys webviewupdate | grep -m1 'Current WebView package' || true
"${ADB}" shell grep MemTotal /proc/meminfo

echo "== install and copy the runtime (jar: ${1:-pruned})"
"${ADB}" uninstall "${PKG}" >/dev/null 2>&1 || true
"${ADB}" install "${WORK}/probe.apk" | tail -n1
"${ADB}" shell "rm -rf ${TMP} && mkdir -p ${TMP}"
"${ADB}" push "${ENGINE}/runtime" "${TMP}/home" | tail -n1
"${ADB}" push "${ENGINE}/zstd-android.jar" "${TMP}/zstd-android.jar" | tail -n1
"${ADB}" push "${JAR}" "${TMP}/Suwayomi-Server.jar" | tail -n1
"${ADB}" shell "chmod -R a+rX ${TMP} && run-as ${PKG} sh -c 'mkdir -p files && cp -r ${TMP}/home ${TMP}/zstd-android.jar ${TMP}/Suwayomi-Server.jar files/' && rm -rf ${TMP}"

echo "== start"
"${ADB}" logcat -c
"${ADB}" shell am start -n "${PKG}/.MainActivity" >/dev/null
for _ in $(seq 1 150); do
  LOG="$("${ADB}" logcat -d -s 'jvmprobe:*')"
  grep -qE 'READY|FAIL|TIMEOUT' <<<"${LOG}" && break
  sleep 2
done
grep -E 'nativeLibraryDir|OK |PROP |MEM |READY|FAIL|TIMEOUT' <<<"${LOG}" | sed 's/^.*jvmprobe: //'
grep -q READY <<<"${LOG}" || { echo "error: the engine did not answer" >&2; exit 1; }

echo "== content path (MangaPill)"
"${ADB}" forward "tcp:${PORT}" "tcp:${PORT}" >/dev/null
"${HERE}/content.sh" "${PORT}" eu.kanade.tachiyomi.extension.en.mangapill mangapill "${WORK}"

echo "== after the content path"
PID="$("${ADB}" shell pidof "${PKG}" | tr -d '\r')"
"${ADB}" shell "grep -E 'VmRSS|VmHWM|Threads' /proc/${PID}/status"
"${ADB}" shell dumpsys meminfo "${PKG}" | grep -E 'TOTAL PSS' || true
"${ADB}" shell "run-as ${PKG} sh -c 'du -sh files/home files/Suwayomi-Server.jar files/data; grep -cE \"UnsatisfiedLinkError|ExceptionInInitializerError|NoClassDefFoundError\" files/data.stdout.log'" || true
"${ADB}" shell "du -sh \$(dirname \$(pm path ${PKG} | sed 's/package://'))" || true
echo "avc denials for the app: $("${ADB}" logcat -d | grep 'avc: denied' | grep -c "${PKG}" || true)"
"${ADB}" forward --remove "tcp:${PORT}" || true
