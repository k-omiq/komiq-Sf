#!/usr/bin/env bash
#
# Make the copy of the Suwayomi-Server jar that goes into the iOS app.
#
# The pinned jar (suwayomi/Suwayomi-Server.jar, fetched and SHA-256-verified by
# fetch-suwayomi-jar.sh) is built for desktops. It carries files that an iPhone cannot use.
# This script copies it to suwayomi-ios/Suwayomi-Server.jar without them. tauri.ios.conf.json
# bundles that copy under the usual name (Komiq.app/assets/suwayomi/Suwayomi-Server.jar), so
# suwayomi_inprocess_ios.rs finds it unchanged. The desktop apps still ship the pinned jar as it is.
# The copy stays outside suwayomi/ because the desktop apps bundle that whole directory.
#
# What goes, and why it is safe (checked 2026-10-06 against v2.3.2243; check again when the
# pin moves, each entry is named by exactly the classes listed):
#
#   font/**      38 MB  NotoSans files. Only android.graphics.Typeface reads them, to draw
#                       text with AWT. The iOS runtime has no AWT native library at all
#                       (no libawt, no libfontmanager), so that path fails with or without
#                       the files.
#   native/**     7 MB  webp-imageio for Linux, macOS and Windows (com.luciad.imageio.webp).
#   jni/**        4 MB  zstd-kmp for Linux, macOS and Windows (com.squareup.zstd).
#                       iOS does not let an app load a library it unpacks at run time, and
#                       neither set has an iOS build, so they never load on a phone.
#   WebUI.zip     3 MB  The bundled web UI (WebInterfaceManager). server.conf sets
#                       server.webUIEnabled = false.
#
# Only resources go. No class is removed: extensions load classes by name, so a missing
# class would show up only when a user opens the one source that needs it.
#
# One class is REPLACED: com/squareup/zstd/Zstd__JvmZstdKt.class, the loader of zstd-kmp's
# native library. The one in the jar throws "Unsupported OS: darwin" on a phone, and then
# every source whose site answers with zstd fails. The iOS loader takes the library that is
# linked into the app instead. scripts/build-ios-zstd.sh makes both; see that script.
#
# Idempotent: nothing is done when the copy is newer than the pinned jar, the iOS loader
# class and this script.
#
# Usage:  ./scripts/prune-ios-jar.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
SRC_JAR="${SRC_TAURI_DIR}/suwayomi/Suwayomi-Server.jar"
OUT_DIR="${SRC_TAURI_DIR}/suwayomi-ios"
OUT_JAR="${OUT_DIR}/Suwayomi-Server.jar"
# The iOS loader for zstd-kmp, made by scripts/build-ios-zstd.sh.
ZSTD_DIR="${SRC_TAURI_DIR}/jre-ios/aarch64-ios/zstd"
ZSTD_CLASS="com/squareup/zstd/Zstd__JvmZstdKt.class"

# Zip entry patterns. `zip -d` matches `*` across `/`.
PRUNE=('font/*' 'native/*' 'jni/*' 'WebUI.zip')
# Entries that must still be there afterwards: the Main-Class and the manifest.
KEEP=('suwayomi/tachidesk/MainKt.class' 'META-INF/MANIFEST.MF' "${ZSTD_CLASS}")

[[ -f "${SRC_JAR}" ]] || {
  echo "error: ${SRC_JAR} missing; run scripts/fetch-suwayomi-jar.sh first" >&2
  exit 1
}

[[ -f "${ZSTD_DIR}/${ZSTD_CLASS}" ]] || {
  echo "error: ${ZSTD_DIR}/${ZSTD_CLASS} missing; run scripts/build-ios-zstd.sh first" >&2
  exit 1
}

if [[ -f "${OUT_JAR}" && "${OUT_JAR}" -nt "${SRC_JAR}" && "${OUT_JAR}" -nt "${BASH_SOURCE[0]}" &&
  "${OUT_JAR}" -nt "${ZSTD_DIR}/${ZSTD_CLASS}" ]]; then
  echo "ok: ${OUT_JAR} is up to date"
  exit 0
fi

mkdir -p "${OUT_DIR}"
TMP_JAR="${OUT_JAR}.tmp"
rm -f "${TMP_JAR}"
cp "${SRC_JAR}" "${TMP_JAR}"
# -d rewrites the archive without the named entries; the other entries keep their bytes.
zip -q -d "${TMP_JAR}" "${PRUNE[@]}"
# Put the iOS loader in place of the one in the jar (zip replaces an entry of the same name).
unzip -Z1 "${TMP_JAR}" "${ZSTD_CLASS}" >/dev/null 2>&1 || {
  echo "error: ${ZSTD_CLASS} is not in the pinned jar; zstd-kmp changed, check build-ios-zstd.sh" >&2
  rm -f "${TMP_JAR}"
  exit 1
}
(cd "${ZSTD_DIR}" && zip -q -X "${TMP_JAR}" "${ZSTD_CLASS}")
unzip -p "${TMP_JAR}" "${ZSTD_CLASS}" | cmp -s - "${ZSTD_DIR}/${ZSTD_CLASS}" || {
  echo "error: ${ZSTD_CLASS} in the pruned jar is not the iOS loader" >&2
  rm -f "${TMP_JAR}"
  exit 1
}

LISTING="$(unzip -Z1 "${TMP_JAR}")"
for pattern in "${PRUNE[@]}"; do
  prefix="${pattern%\*}"
  if grep -q "^${prefix}" <<<"${LISTING}"; then
    echo "error: ${pattern} is still in the pruned jar" >&2
    rm -f "${TMP_JAR}"
    exit 1
  fi
done
for entry in "${KEEP[@]}"; do
  grep -qx "${entry}" <<<"${LISTING}" || {
    echo "error: ${entry} is missing from the pruned jar" >&2
    rm -f "${TMP_JAR}"
    exit 1
  }
done
mv "${TMP_JAR}" "${OUT_JAR}"

mb() { echo $(( $(wc -c < "$1") / 1000000 )); }
echo "ok: ${OUT_JAR} is $(mb "${OUT_JAR}") MB (pinned jar: $(mb "${SRC_JAR}") MB)"
