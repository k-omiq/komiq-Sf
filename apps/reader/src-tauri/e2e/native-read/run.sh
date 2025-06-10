#!/usr/bin/env bash
# Orchestrator: boot embedded Suwayomi -> bundle harness -> drive composite/local -> teardown.
#
# Manual/local native E2E regression tool. NOT wired into CI: it needs the fetched
# ~174 MB Suwayomi jar + a jlink'd JRE (both gitignored) AND live network egress
# (installs the MangaDex extension from Keiyoushi + reads MangaDex). See README.md.
set -uo pipefail

# --- portable paths: everything is derived from this script's location ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"        # .../e2e/native-read
SRC_TAURI="$(cd "$SCRIPT_DIR/../.." && pwd)"                       # apps/reader/src-tauri
REPO="$(cd "$SRC_TAURI/../../.." && pwd)"                          # repo root
OUT="$SCRIPT_DIR/build"                                            # generated bundle + logs + pidfile (gitignored)
mkdir -p "$OUT"

# --- JRE slug detection: mirror scripts/build-jre.sh (Rust ARCH-OS, e.g. aarch64-macos) ---
uname_m="$(uname -m)"
case "$uname_m" in
  arm64 | aarch64) ARCH="aarch64" ;;
  x86_64 | amd64)  ARCH="x86_64" ;;
  *) echo "error: unsupported arch '$uname_m'" >&2; exit 1 ;;
esac
uname_s="$(uname -s)"
case "$uname_s" in
  Darwin)                                OS="macos" ;;
  Linux)                                 OS="linux" ;;
  MINGW* | MSYS* | CYGWIN* | Windows_NT) OS="windows" ;;
  *) echo "error: unsupported OS '$uname_s'" >&2; exit 1 ;;
esac
SLUG="${ARCH}-${OS}"
JAVA="$SRC_TAURI/jre/$SLUG/bin/java"
[[ "$OS" == "windows" ]] && JAVA="$SRC_TAURI/jre/$SLUG/bin/java.exe"
JAR="$SRC_TAURI/suwayomi/Suwayomi-Server.jar"

# --- preflight: the jar + JRE are gitignored; fail loud before booting anything ---
if [[ ! -x "$JAVA" || ! -f "$JAR" ]]; then
  echo "error: missing embedded engine assets (jar + JRE are gitignored)." >&2
  [[ ! -x "$JAVA" ]] && echo "  missing JRE launcher: $JAVA" >&2
  [[ ! -f "$JAR"  ]] && echo "  missing server jar:   $JAR" >&2
  echo "  Run 'apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh' and" >&2
  echo "      'apps/reader/src-tauri/scripts/build-jre.sh' first." >&2
  exit 1
fi

# --- esbuild resolver: bundle the TS entry (aliasing the Tauri core to our HTTP stub).
# pnpm hoists esbuild's bin under .pnpm; `pnpm exec esbuild` does NOT surface it as a
# runnable bin in this repo, so probe the concrete .bin locations first, then fall back
# to npx. The entry lives under apps/reader/**, so esbuild resolves `@komika/api` by
# walking up to apps/reader/node_modules (the workspace symlink) — no --resolve-dir needed.
if [[ -x "$REPO/node_modules/.bin/esbuild" ]]; then
  ESBUILD=("$REPO/node_modules/.bin/esbuild")
elif [[ -x "$REPO/node_modules/.pnpm/node_modules/.bin/esbuild" ]]; then
  ESBUILD=("$REPO/node_modules/.pnpm/node_modules/.bin/esbuild")
else
  ESBUILD=(npx --yes esbuild)
fi

PORT="${SUWA_PORT:-4572}"
export SUWA_PORT="$PORT"

DATADIR="$(mktemp -d "${TMPDIR:-/tmp}/suwa-harness.XXXXXX")"
PIDFILE="$OUT/engine.pid"
ENGINE_LOG="$OUT/engine.log"
BUNDLE="$OUT/harness.mjs"
ENGINE_PID=""

cleanup() {
  echo "=== TEARDOWN ==="
  if [[ -n "$ENGINE_PID" ]] && kill -0 "$ENGINE_PID" 2>/dev/null; then
    kill "$ENGINE_PID" 2>/dev/null
    for i in $(seq 1 20); do kill -0 "$ENGINE_PID" 2>/dev/null || break; sleep 0.3; done
    kill -9 "$ENGINE_PID" 2>/dev/null || true
  fi
  # Belt-and-suspenders: nuke any stragglers rooted at OUR datadir only.
  pkill -f "suwayomi.tachidesk.config.server.rootDir=$DATADIR" 2>/dev/null || true
  sleep 0.5
  echo "-- pgrep -fl Suwayomi after teardown --"
  pgrep -fl Suwayomi || echo "(clean: no Suwayomi processes)"
  rm -rf "$DATADIR" "$PIDFILE"
  echo "removed datadir $DATADIR"
}
trap cleanup EXIT

echo "=== BOOT === datadir=$DATADIR port=$PORT slug=$SLUG"
cat > "$DATADIR/server.conf" <<EOF
server.ip = "127.0.0.1"
server.port = $PORT
server.webUIEnabled = false
server.initialOpenInBrowserEnabled = false
server.systemTrayEnabled = false
server.downloadAsCbz = false
server.localSourcePath = ""
server.flareSolverrEnabled = false
EOF

"$JAVA" -Djava.awt.headless=true -Xmx512m \
  -Dsuwayomi.tachidesk.config.server.rootDir="$DATADIR" \
  -jar "$JAR" > "$ENGINE_LOG" 2>&1 &
ENGINE_PID=$!
echo "$ENGINE_PID" > "$PIDFILE"
echo "engine pid=$ENGINE_PID (log: $ENGINE_LOG)"

# Poll aboutServer until ready (up to ~60s).
READY=0
for i in $(seq 1 60); do
  if ! kill -0 "$ENGINE_PID" 2>/dev/null; then echo "engine died early"; tail -30 "$ENGINE_LOG"; exit 3; fi
  RESP="$(curl -s -m 3 -X POST "http://127.0.0.1:$PORT/api/graphql" \
    -H 'content-type: application/json' \
    --data '{"query":"query{aboutServer{version buildType}}"}' 2>/dev/null)"
  if echo "$RESP" | grep -q '"version"'; then
    READY=1
    echo "engine ready after ${i}s: $RESP"
    break
  fi
  sleep 1
done
if [[ "$READY" != "1" ]]; then echo "engine never became ready"; tail -40 "$ENGINE_LOG"; exit 4; fi

echo "=== BUNDLE === (${ESBUILD[*]})"
"${ESBUILD[@]}" "$SCRIPT_DIR/harness.entry.ts" \
  --bundle --platform=node --format=esm \
  --alias:@tauri-apps/api/core="$SCRIPT_DIR/tauri-core-stub.mjs" \
  --outfile="$BUNDLE" 2>&1 || { echo "esbuild failed"; exit 5; }

echo "-- verify bundle contains the stub, not the real package --"
if grep -q "tauri-core-stub: unhandled invoke" "$BUNDLE"; then
  echo "OK: stub source is inlined in bundle"
else
  echo "WARN: stub marker not found in bundle"
fi
if grep -q "require(\"@tauri-apps/api/core\")\|from \"@tauri-apps/api/core\"" "$BUNDLE"; then
  echo "WARN: bundle still references real @tauri-apps/api/core"
else
  echo "OK: no residual real @tauri-apps/api/core import"
fi

echo "=== RUN HARNESS ==="
node "$BUNDLE"
RC=$?
echo "=== harness exit code: $RC ==="
exit $RC
