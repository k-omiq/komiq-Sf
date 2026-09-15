#!/usr/bin/env bash
#
# N4.2: stage the iOS in-process JVM runtime into the generated Xcode project.
#
# WHY THIS EXISTS: the statically-linked HotSpot (Zero) derives java.home ITSELF as
# "<app executable dir>/lib" (os_bsd.cpp __IOS__ branch + is_vm_statically_linked()),
# and the boot jimage path (<java.home>/lib/modules) is fixed BEFORE -D options are
# parsed, so -Djava.home cannot relocate it. The runtime therefore CANNOT live under
# tauri's iOS resource dir (Komika.app/assets/...) — it must sit at the APP BUNDLE
# ROOT as Komika.app/lib/. That is achieved with an XcodeGen folder reference
# (gen/apple/project.yml: sources entry "jvm-runtime/lib"), and this script builds
# the folder it references:
#
#   gen/apple/jvm-runtime/lib/
#     lib/modules      <- the class-library jimage (~115 MB)
#     lib/tzdb.dat
#     lib/security/
#     lib/jfr/
#     lib/jvm.cfg
#     conf/
#     release
#
# (i.e. bundle path Komika.app/lib/lib/modules == <java.home>/lib/modules with
# java.home = Komika.app/lib. The *.a static libs are NOT staged — they are
# link-time inputs, consumed via build.rs.)
#
# gen/apple is gitignored and regenerable (`tauri ios init`), and a regeneration
# wipes BOTH this staging and the hand-added project.yml entries. Re-run this
# script after any regeneration; it verifies project.yml carries the required
# entries and fails with guidance if not. APFS clones (cp -c) keep it instant.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

JRE="${SRC_TAURI_DIR}/jre-ios/aarch64-ios"
APPLE="${SRC_TAURI_DIR}/gen/apple"
STAGE="${APPLE}/jvm-runtime/lib"

[[ -f "${JRE}/lib/modules" ]] || {
  echo "error: ${JRE}/lib/modules missing — build the N4.1 runtime first (scripts/build-ios-jvm.sh)" >&2
  exit 1
}
[[ -d "${APPLE}" ]] || {
  echo "error: ${APPLE} missing — run 'pnpm exec tauri ios init' first" >&2
  exit 1
}

rm -rf "${STAGE}"
mkdir -p "${STAGE}/lib"
cp -c "${JRE}/lib/modules" "${STAGE}/lib/modules"
cp -c "${JRE}/lib/tzdb.dat" "${STAGE}/lib/tzdb.dat"
cp -c "${JRE}/lib/jvm.cfg" "${STAGE}/lib/jvm.cfg"
cp -Rc "${JRE}/lib/security" "${STAGE}/lib/security"
cp -Rc "${JRE}/lib/jfr" "${STAGE}/lib/jfr"
cp -Rc "${JRE}/conf" "${STAGE}/conf"
cp -c "${JRE}/release" "${STAGE}/release"

# ---- Runtime-payload guards ----------------------------------------------------------
#
# Both were found on a real iPhone (2026-10-05) and neither shows at build time, so a
# staged runtime that has them is a script failure here (same checks as the end of
# build-ios-jvm.sh; keep the two in step):
#   1. jimage version. The libjimage.a linked into the app (pinned openjdk/mobile tree)
#      opens major 1 / minor 0 ONLY. Anything else and JNI_CreateJavaVM ends the whole
#      app with "Unable to load <app>/lib/lib/modules". Expected first 8 bytes: magic
#      "da da fe ca", minor "00 00", major "01 00".
#   2. Symlinks. `cp -R` above keeps links as links, and a link into the JDK build tree
#      (how lib/security/cacerts was first vendored) points at nothing inside the app:
#      no trust store, so no HTTPS. ANY link is rejected, dangling or not.
JIMAGE_VERSION_HEX="dadafeca00000100"
JIMAGE_HEAD="$(od -An -tx1 -N8 "${STAGE}/lib/modules" | tr -d ' \n')"
if [[ "${JIMAGE_HEAD}" != "${JIMAGE_VERSION_HEX}" ]]; then
  echo "error: ${STAGE}/lib/modules has jimage header ${JIMAGE_HEAD}, expected ${JIMAGE_VERSION_HEX}" >&2
  echo "  (magic + minor 0 + major 1). The vendored libjimage cannot open it; re-link lib/modules" >&2
  echo "  with a jlink that writes jimage 1.0 (see the LINK_JDK pin in scripts/build-ios-jvm.sh)." >&2
  exit 1
fi
LINKS="$(find "${STAGE}" -type l)"
if [[ -n "${LINKS}" ]]; then
  echo "error: symlinks in the staged runtime (they point outside the app bundle):" >&2
  echo "${LINKS}" | sed 's/^/  /' >&2
  echo "  replace them with real files in ${JRE} and re-run this script." >&2
  exit 1
fi
for f in cacerts blocked.certs public_suffix_list.dat; do
  [[ -s "${STAGE}/lib/security/${f}" ]] || { echo "error: ${STAGE}/lib/security/${f} missing or empty" >&2; exit 1; }
done

# ---- The gen/apple/project.yml link recipe -------------------------------------------
#
# These entries are hand-maintained because gen/apple is generated + gitignored and
# `tauri ios init` regenerates it WITHOUT them. Since this script is the only tracked
# record of the recipe, it both ASSERTS every required entry and can PRINT the whole
# thing back (print_project_yml_recipe below) so a wiped project.yml is recoverable
# from the repo alone.
#
# The 13 JDK static libs, and libzstdkmp.a (the zstd decoder of the engine's HTTP client,
# made by scripts/build-ios-zstd.sh), must be force-loaded: nothing references their
# `Java_*`/`JNI_OnLoad_*` members at link time (HotSpot dlsym's them off the app image
# at runtime), so as ordinary archive members ld drops them and JNI_CreateJavaVM dies
# during java.base bootstrap on device — with NO build-time error. See build.rs for the
# full analysis of why they cannot be bundled by cargo like libjvm.a/libffi.a are.
FORCE_LOAD_LIBS=(
  java nio net zip jimage verify prefs extnet
  syslookup fallbackLinker management management_ext rmi
  zstdkmp
)
[[ -f "${JRE}/lib/libzstdkmp.a" ]] || {
  echo "error: ${JRE}/lib/libzstdkmp.a missing; run scripts/build-ios-zstd.sh" >&2
  exit 1
}

print_project_yml_recipe() {
  cat >&2 <<'YAML'

  ---- required gen/apple/project.yml entries (merge into the app target) ----
  targets:
    <productName>_iOS:
      sources:
        # Folder reference (NOT a resource): lands at Komika.app/lib, which is the
        # java.home the statically-linked Zero VM derives as "<exe dir>/lib".
        - path: jvm-runtime/lib
          type: folder
      dependencies:
        - sdk: CoreFoundation.framework
        - sdk: Foundation.framework
      settings:
        base:
          # A stripped link re-drops the force-loaded, dlsym-only members.
          DEAD_CODE_STRIPPING: false
          # A release archive strips with STRIP_STYLE=all by default. That removes the
          # exported names the VM looks up at run time (Java_*, JNI_OnLoad_*, ...).
          STRIP_STYLE: non-global
          OTHER_LDFLAGS: >-
            -lz -liconv -lc++
YAML
  local lib
  for lib in "${FORCE_LOAD_LIBS[@]}"; do
    printf '            -Wl,-force_load,$(SRCROOT)/../../jre-ios/aarch64-ios/lib/lib%s.a\n' "${lib}" >&2
  done
  printf '  ---------------------------------------------------------------------------\n\n' >&2
}

PROJECT_YML="${APPLE}/project.yml"
MISSING=0
require_project_yml() {
  local label="$1" pattern="$2"
  grep -Eq -- "${pattern}" "${PROJECT_YML}" && return 0
  echo "error: gen/apple/project.yml is missing ${label}" >&2
  MISSING=1
}

require_project_yml "the jvm-runtime/lib folder reference (-> Komika.app/lib)" "jvm-runtime/lib"
require_project_yml "DEAD_CODE_STRIPPING: false" "DEAD_CODE_STRIPPING[[:space:]]*:[[:space:]]*[\"']?(false|NO|no)"
require_project_yml "STRIP_STYLE: non-global (a release build must keep the names the VM looks up)" "STRIP_STYLE[[:space:]]*:[[:space:]]*[\"']?non-global"
require_project_yml "the CoreFoundation framework dependency" "CoreFoundation\.framework"
require_project_yml "the Foundation framework dependency" "Foundation\.framework"
require_project_yml "-lz (libjvm's inflater)" "\-lz([^a-zA-Z0-9]|$)"
require_project_yml "-liconv (libjvm's charset conversion)" "\-liconv([^a-zA-Z0-9]|$)"
require_project_yml "-lc++ (the VM's C++ runtime)" "\-lc\+\+"
for lib in "${FORCE_LOAD_LIBS[@]}"; do
  require_project_yml "-force_load of lib${lib}.a" "\-force_load[[:space:],]+[^[:space:]]*lib${lib}\.a"
done
if [[ "${MISSING}" != "0" ]]; then
  echo "error: re-apply the N4.2 link recipe, then run 'xcodegen generate' in ${APPLE}" >&2
  print_project_yml_recipe
  exit 1
fi

# ---- Bundle-payload guards ------------------------------------------------------------
#
# Regression guard for the 218 MB .ipa: the FIRST
# `tauri ios init` baked the DESKTOP bundle.resources (jar + jlink JRE) into the Xcode
# resource-copy phase, and the narrowed `resources` in tauri.ios.conf.json does not
# rewrite an already-generated project. A staged desktop JRE here means the .ipa is
# shipping macOS mach-o binaries that cannot even load on iOS.
if [[ -e "${APPLE}/assets/jre" ]]; then
  echo "error: ${APPLE}/assets/jre exists — the DESKTOP JRE (macOS/Linux/Windows binaries) is" >&2
  echo "  staged into the iOS bundle. tauri.ios.conf.json narrows bundle.resources to the jar," >&2
  echo "  but only a gen/apple generated AFTER that override honors it: delete gen/apple, re-run" >&2
  echo "  'pnpm exec tauri ios init', re-apply the project.yml recipe, then re-run this script." >&2
  exit 1
fi

STAGE_MB=$(( $(du -sk "${STAGE}" | awk '{print $1}') / 1024 ))
STAGE_CEILING_MB=200
if (( STAGE_MB > STAGE_CEILING_MB )); then
  echo "error: staged runtime is ${STAGE_MB} MB, exceeds the ${STAGE_CEILING_MB} MB ceiling" >&2
  echo "  thin the jimage with a Suwayomi-tailored --add-modules set (scripts/build-ios-jvm.sh)." >&2
  exit 1
fi

# The runtime is only half the payload; the jar rides along as an app resource (the iOS
# copy that scripts/prune-ios-jar.sh makes from the pinned jar; counted only when it is
# there). Bound the pair the way build-jre.sh bounds the desktop JRE, so an oversized .ipa
# is a script failure rather than a codesign/App-Store surprise.
JAR="${SRC_TAURI_DIR}/suwayomi-ios/Suwayomi-Server.jar"
PAYLOAD_MB="${STAGE_MB}"
if [[ -f "${JAR}" ]]; then
  PAYLOAD_MB=$(( STAGE_MB + $(du -sk "${JAR}" | awk '{print $1}') / 1024 ))
fi
PAYLOAD_CEILING_MB=400
if (( PAYLOAD_MB > PAYLOAD_CEILING_MB )); then
  echo "error: iOS engine payload is ${PAYLOAD_MB} MB, exceeds the ${PAYLOAD_CEILING_MB} MB ceiling" >&2
  exit 1
fi

echo "ok: staged iOS JVM runtime at ${STAGE} (${STAGE_MB} MB; bundle path Komika.app/lib)"
echo "    iOS engine payload (runtime + jar): ${PAYLOAD_MB} MB of ${PAYLOAD_CEILING_MB} MB ceiling"
