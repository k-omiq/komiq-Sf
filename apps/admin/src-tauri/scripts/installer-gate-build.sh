#!/usr/bin/env bash
#
# Build the UNSIGNED package that the installer gate installs
# (tests/desktop_postgres_gate.rs, test `desktop_installer_gate`): the `.deb` on Linux,
# the NSIS `-setup.exe` on Windows, the `.app` on macOS.
#
# It is the product's own build: `tauri build` with tauri.conf.json as it is (the
# identifier, the installer settings, the stock installer templates and both hooks:
# prepare-bundle.sh and check-postgres-bundle.sh). There is no `--config` override, and
# the `TAURI_CONFIG` that the cargo test commands of this crate set (it removes
# `externalBin`) is taken out of the environment here. What differs from a release
# bundle, all through switches that exist today:
#
#   * KOMIKA_SKIP_ENGINE=1 (prepare-bundle.sh): the source engine (jar and JRE) is not
#     fetched or built. Whatever lies in apps/reader/src-tauri/{suwayomi,jre} is bundled:
#     the placeholders of a clean checkout, or the full engine where it was built before.
#     Set KOMIKA_SKIP_ENGINE=0 to build it.
#   * `--bundles <one kind>`: only the kind the gate installs, not every kind of
#     `"targets": "all"`.
#   * Nothing is signed: no signing identity, certificate or key is configured or given.
#
# Usage:  bash apps/admin/src-tauri/scripts/installer-gate-build.sh <facts.json>
# Writes <facts.json>: the package path, its kind, and what was and was not built.
# Env (all optional): KOMIKA_SKIP_ENGINE, KOMIKA_SERVER_PROFILE, KOMIKA_POSTGRES_ARCHIVE,
#   KOMIKA_PYTHON, KOMIKA_SERVER_TARGET_DIR (see prepare-bundle.sh); CARGO_TARGET_DIR.
set -euo pipefail

FACTS="${1:?usage: installer-gate-build.sh <facts.json>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI="$(cd "${SCRIPT_DIR}/.." && pwd)"
ADMIN="$(cd "${SRC_TAURI}/.." && pwd)"
REPO="$(cd "${ADMIN}/../.." && pwd)"

case "$(uname -s)" in
  Linux) KIND=deb; FOLDER=deb ;;
  Darwin) KIND=app; FOLDER=macos ;;
  MINGW* | MSYS* | CYGWIN*)
    KIND=nsis
    FOLDER=nsis
    # Tauri starts the build hook through cmd.exe, and the hook's first word is `bash`.
    # It must be this Git Bash and not C:\Windows\System32\bash.exe (the WSL launcher):
    # the folder of the running bash goes to the front of the path.
    PATH="$(dirname "$(command -v bash)"):${PATH}"
    export PATH
    ;;
  *)
    echo "error: no installer kind for $(uname -s)" >&2
    exit 1
    ;;
esac

# The cargo test commands of this crate run with TAURI_CONFIG='{"bundle":{"externalBin":[]}}'.
# The bundle must be built with the product's configuration.
unset TAURI_CONFIG
export KOMIKA_SKIP_ENGINE="${KOMIKA_SKIP_ENGINE:-1}"
# No prompt of pnpm, corepack or the Tauri CLI may wait for an answer.
export CI="${CI:-true}"
export COREPACK_ENABLE_DOWNLOAD_PROMPT=0
# The desktop shell reads PUBLIC_KOMIKA_API at build time (apps/admin/src/lib/config.ts) and
# the build fails without it. A release build takes it from apps/admin/.env.production,
# which is not committed, so a clean checkout has none: use the code's own localhost
# default then, as .github/workflows/ci.yml does. The local server ignores this value.
if [[ -z "${PUBLIC_KOMIKA_API+x}" && ! -f "${ADMIN}/.env" && ! -f "${ADMIN}/.env.production" ]]; then
  export PUBLIC_KOMIKA_API="http://localhost:8080/graphql"
  PUBLIC_API_SOURCE="the localhost default of this script (no .env file in the checkout)"
else
  PUBLIC_API_SOURCE="the environment or an .env file of the checkout"
fi
export PUBLIC_API_SOURCE

TARGET_DIR="${CARGO_TARGET_DIR:-${SRC_TAURI}/target}"
BUNDLE_DIR="${TARGET_DIR}/release/bundle/${FOLDER}"
# Exactly one package may lie there afterwards: a package of an earlier build is removed.
rm -rf "${BUNDLE_DIR}"

cd "${ADMIN}"
CLI_VERSION="$(pnpm exec tauri --version)"
echo "building the unsigned ${KIND} bundle with ${CLI_VERSION} (KOMIKA_SKIP_ENGINE=${KOMIKA_SKIP_ENGINE})"
pnpm exec tauri build --ci --bundles "${KIND}"

KIND="${KIND}" BUNDLE_DIR="${BUNDLE_DIR}" TARGET_DIR="${TARGET_DIR}" REPO="${REPO}" \
  CLI_VERSION="${CLI_VERSION}" FACTS="${FACTS}" node -e '
const fs = require("fs");
const path = require("path");
const env = process.env;
const suffix = { deb: ".deb", nsis: "-setup.exe", app: ".app" }[env.KIND];
const found = fs.readdirSync(env.BUNDLE_DIR).filter((name) => name.endsWith(suffix));
if (found.length !== 1) {
  console.error("error: expected one " + suffix + " in " + env.BUNDLE_DIR + ", found " + JSON.stringify(found));
  process.exit(1);
}
// Files below a folder, not counting the placeholders that are in git.
const payload = (folder) => {
  let files = 0;
  let bytes = 0;
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name !== ".gitkeep" && entry.name !== "VERSION") {
        files += 1;
        bytes += fs.statSync(full).size;
      }
    }
  };
  if (fs.existsSync(folder)) walk(folder);
  return { files, bytes };
};
const reader = path.join(env.REPO, "apps", "reader", "src-tauri");
const jar = payload(path.join(reader, "suwayomi"));
const jre = payload(path.join(reader, "jre"));
const nsis = path.join(env.TARGET_DIR, "release", "nsis", "x64", "installer.nsi");
const facts = {
  kind: env.KIND,
  package: path.resolve(env.BUNDLE_DIR, found[0]),
  tauri_cli: env.CLI_VERSION,
  command: "pnpm exec tauri build --ci --bundles " + env.KIND,
  config_override: null,
  tauri_config_environment_variable: "unset for the build",
  signed: false,
  skip_engine: env.KOMIKA_SKIP_ENGINE,
  server_profile: env.KOMIKA_SERVER_PROFILE || "release",
  engine_payload_in_the_bundle: { jar, jre, complete: jar.files > 0 && jre.files > 0 },
  generated_nsis_script: env.KIND === "nsis" && fs.existsSync(nsis) ? nsis : null,
  differs_from_a_release_bundle: [
    env.KOMIKA_SKIP_ENGINE === "1"
      ? "KOMIKA_SKIP_ENGINE=1: the engine was not fetched or built; the engine folders were bundled as they lay there (" + jar.files + " jar files, " + jre.files + " JRE files)"
      : "the engine was fetched and built",
    "only the " + env.KIND + " bundle was made (--bundles), not every kind of targets: all",
    "nothing is signed or notarized",
    "PUBLIC_KOMIKA_API of the desktop shell came from " + env.PUBLIC_API_SOURCE,
  ].concat(env.KOMIKA_SERVER_PROFILE && env.KOMIKA_SERVER_PROFILE !== "release" ? ["the server was built with the " + env.KOMIKA_SERVER_PROFILE + " profile (KOMIKA_SERVER_PROFILE)"] : []),
};
fs.mkdirSync(path.dirname(path.resolve(env.FACTS)), { recursive: true });
fs.writeFileSync(env.FACTS, JSON.stringify(facts, null, 2) + "\n");
console.log(JSON.stringify(facts, null, 2));
'
