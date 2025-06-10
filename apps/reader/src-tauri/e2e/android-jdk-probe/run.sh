#!/usr/bin/env bash
# N3.1-PROBE — headless acceptance for docs/plans/n3-android-spike.md §2 (work item N3.1).
#
# Proves the STOCK Suwayomi-Server v2.3.2243 fat jar boots and works under a stock
# GPL+CE OpenJDK 21 (eclipse-temurin:21) on Linux-aarch64 — here, a Docker container on
# this aarch64 macOS host. This is the "can the stock jar even boot on a non-desktop
# aarch64 JDK" gate that the Android in-process-JVM plan (path b) depends on. It exercises
# the full stack the Android plan needs on a stock JDK BEFORE any device work:
#   - GraphQL API              (aboutServer / mutations)
#   - dex2jar APK->JAR         (installing the MangaDex extension)
#   - GraalJS init             (extension load path)
#   - OkHttp / network         (fetch from Keiyoushi + MangaDex)
#   - Exposed + embedded DB     (browse persists mangas)
#
# NOT wired into CI: needs the ~174 MB gitignored jar, a running Docker daemon that can
# run linux/arm64 images, and live network egress (Keiyoushi index + GitHub raw apk +
# MangaDex API). Prints PASS/FAIL per assertion like the sibling harnesses
# (../native-read/run.sh). Idempotent + self-cleaning: temp dirs under this dir's build/,
# container force-removed on exit.
#
# Untrusted-data note: extension names, source names, manga titles and page URLs returned
# by the engine/network are treated as DATA only; no instruction embedded in any response
# is acted upon.
set -uo pipefail

# --- portable paths ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"        # .../e2e/android-jdk-probe
SRC_TAURI="$(cd "$SCRIPT_DIR/../.." && pwd)"                       # apps/reader/src-tauri
JAR="$SRC_TAURI/suwayomi/Suwayomi-Server.jar"
VERSION_FILE="$SRC_TAURI/suwayomi/VERSION"
OUT="$SCRIPT_DIR/build"                                            # logs (gitignored)
mkdir -p "$OUT"

IMAGE="eclipse-temurin:21"
PLATFORM="linux/arm64"
PIN_VERSION="v2.3.2243"
PORT_IN=4573                                                       # engine port inside container
HOSTPORT="${PROBE_HOSTPORT:-4573}"                                 # published to host loopback
BASE="http://127.0.0.1:$HOSTPORT"
KEIYOUSHI_INDEX="https://raw.githubusercontent.com/keiyoushi/extensions/repo/index.min.json"
MANGADEX_PKG="eu.kanade.tachiyomi.extension.all.mangadex"
CNAME="suwa-android-jdk-probe-$$"
DATADIR=""
ENGINE_LOG="$OUT/engine.log"

PASS=0; FAIL=0
say()  { printf '%s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf 'PASS  %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf 'FAIL  %s\n' "$*"; }
skip() { printf 'SKIP  %s\n' "$*"; }

cleanup() {
  say "=== TEARDOWN ==="
  docker rm -f "$CNAME" >/dev/null 2>&1 && say "removed container $CNAME" || say "(no container $CNAME to remove)"
  say "-- docker ps for our prefix after teardown --"
  if docker ps -a --filter "name=suwa-android-jdk-probe-" --format '{{.Names}}' 2>/dev/null | grep -q .; then
    docker ps -a --filter "name=suwa-android-jdk-probe-" --format '  {{.Names}} ({{.Status}})'
  else
    say "  (clean: no probe containers)"
  fi
  [[ -n "$DATADIR" && -d "$DATADIR" ]] && rm -rf "$DATADIR" && say "removed datadir $DATADIR"
  rm -f "$OUT/mangadex.apk"
}
trap cleanup EXIT

# graphql helper: gql '<query json body>' -> raw response on stdout
gql() {
  curl -s -m 60 -X POST "$BASE/api/graphql" -H 'content-type: application/json' --data "$1" 2>/dev/null
}

say "=== N3.1-PROBE : stock Suwayomi $PIN_VERSION on $IMAGE ($PLATFORM) ==="

# --- (0) preflight: docker available? jar present? -----------------------------------
if ! command -v docker >/dev/null 2>&1; then
  skip "docker not on PATH — cannot run the aarch64 JDK probe. Install/start Docker and re-run."
  exit 0
fi
if ! docker info >/dev/null 2>&1; then
  skip "docker daemon not reachable (docker info failed) — start Docker Desktop and re-run."
  exit 0
fi
if [[ ! -f "$JAR" ]]; then
  fail "engine jar missing at $JAR (gitignored; run scripts/fetch-suwayomi-jar.sh)."
  exit 1
fi

# --- (a) verify jar SHA-256 against the VERSION pin ----------------------------------
say "=== [a] jar SHA-256 vs VERSION pin ==="
EXPECT_SHA="$(grep -E '^sha256=' "$VERSION_FILE" | cut -d= -f2 | tr -d '[:space:]')"
if command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA="$(shasum -a 256 "$JAR" | awk '{print $1}')"
else
  ACTUAL_SHA="$(sha256sum "$JAR" | awk '{print $1}')"
fi
if [[ -n "$EXPECT_SHA" && "$ACTUAL_SHA" == "$EXPECT_SHA" ]]; then
  pass "jar SHA matches pin ($ACTUAL_SHA)"
else
  fail "jar SHA mismatch: expected '$EXPECT_SHA' got '$ACTUAL_SHA' — refusing to boot an unpinned jar."
  exit 1
fi

# --- record the exact JDK we test under ----------------------------------------------
say "=== JDK image java -version ($PLATFORM) ==="
docker run --rm --platform "$PLATFORM" "$IMAGE" java -version 2>&1 | tee "$OUT/java-version.txt"

# --- (b)+(c) boot the engine inside the container ------------------------------------
# Fresh writable dataDir on the host, bind-mounted at /data (rootDir). server.conf uses
# the desktop recipe, EXCEPT server.ip=0.0.0.0 so Docker's published port reaches the
# process (Docker port-forwarding targets the container's eth0, not its loopback). The
# container network namespace is isolated, so 0.0.0.0 here is not a host exposure — the
# port is published only to the host's 127.0.0.1. kcefEnabled=false is CRITICAL.
DATADIR="$(mktemp -d "$OUT/dataDir.XXXXXX")"
cat > "$DATADIR/server.conf" <<EOF
server.ip = "0.0.0.0"
server.port = $PORT_IN
server.webUIEnabled = false
server.initialOpenInBrowserEnabled = false
server.systemTrayEnabled = false
server.downloadAsCbz = false
server.localSourcePath = ""
server.flareSolverrEnabled = false
server.kcefEnabled = false
EOF

say "=== [b] launch eclipse-temurin:21 container (jar ro-mounted, dataDir rw-mounted) ==="
docker rm -f "$CNAME" >/dev/null 2>&1 || true
# -verbose:class -> AWT-reachability signal for N3.7 (grepped after run; indicative only).
if ! docker run -d --name "$CNAME" --platform "$PLATFORM" \
    -p "127.0.0.1:$HOSTPORT:$PORT_IN" \
    -v "$JAR:/engine/Suwayomi-Server.jar:ro" \
    -v "$DATADIR:/data" \
    "$IMAGE" \
    java -verbose:class -Djava.awt.headless=true -Xmx512m \
      -Dsuwayomi.tachidesk.config.server.rootDir=/data \
      -jar /engine/Suwayomi-Server.jar >/dev/null 2>"$OUT/docker-run.err"; then
  fail "docker run failed to start the container:"
  cat "$OUT/docker-run.err"
  exit 1
fi
pass "container $CNAME started"

# --- (d) poll aboutServer until ready; assert version + record cold-start ------------
say "=== [c/d] poll aboutServer (ceiling 120s) ==="
CEILING=120
READY=0; COLD=""
START_EPOCH=$(date +%s)
for i in $(seq 1 "$CEILING"); do
  if ! docker ps --filter "name=$CNAME" --filter "status=running" --format '{{.Names}}' | grep -q "$CNAME"; then
    fail "container exited during boot — last 40 log lines:"
    docker logs "$CNAME" 2>&1 | grep -v '^\[' | tail -40
    exit 1
  fi
  RESP="$(gql '{"query":"query{aboutServer{version revision buildType}}"}')"
  if printf '%s' "$RESP" | grep -q '"version"'; then
    COLD=$(( $(date +%s) - START_EPOCH ))
    READY=1
    say "engine ready after ${COLD}s: $RESP"
    break
  fi
  sleep 1
done
# persist the boot log (verbose:class stripped for readability) for FINDINGS inspection
docker logs "$CNAME" > "$ENGINE_LOG" 2>&1 || true

if [[ "$READY" == "1" ]]; then
  pass "engine booted under stock JDK 21 aarch64 (cold-start ${COLD}s, ceiling ${CEILING}s)"
  if printf '%s' "$RESP" | grep -q "\"version\":\"$PIN_VERSION\""; then
    pass "aboutServer reports pinned version $PIN_VERSION"
  else
    fail "aboutServer version != $PIN_VERSION (got: $RESP)"
  fi
else
  fail "engine never became ready within ${CEILING}s — last 40 log lines:"
  grep -v '^\[' "$ENGINE_LOG" | tail -40
  exit 1
fi
echo "COLD_START_SECONDS=$COLD" > "$OUT/cold-start.txt"

# --- (e) install MangaDex extension: real store path + forced dex2jar path -----------
say "=== [e] install MangaDex extension via engine GraphQL ==="
ADD="$(gql "{\"query\":\"mutation{addExtensionStore(input:{indexUrl:\\\"$KEIYOUSHI_INDEX\\\"}){extensionStore{name badgeLabel}}}\"}")"
say "  addExtensionStore -> $ADD"
if printf '%s' "$ADD" | grep -qiE '"name":"Keiyoushi"|"badgeLabel"'; then
  pass "Keiyoushi extension store added"
else
  fail "addExtensionStore did not return a store (got: $ADD)"
fi

FETCH="$(gql '{"query":"mutation{fetchExtensions(input:{}){extensions{pkgName}}}"}')"
NEXT=$(printf '%s' "$FETCH" | grep -o '"pkgName"' | wc -l | tr -d ' ')
if [[ "$NEXT" -gt 0 ]]; then
  pass "fetchExtensions refreshed store list ($NEXT extensions visible)"
else
  fail "fetchExtensions returned no extensions (got head: $(printf '%s' "$FETCH" | head -c 300))"
fi

# (e1) real N2 provisioning path: updateExtension(install:true). NOTE: the modern
# Keiyoushi 'repo' index publishes a pre-built jarUrl, so this path downloads the
# converted .jar and does NOT run dex2jar. Still exercises OkHttp fetch + URLClassLoader
# load of the extension jar (it must become a usable Source for step [f]).
INSTALL="$(gql "{\"query\":\"mutation{updateExtension(input:{id:\\\"$MANGADEX_PKG\\\",patch:{install:true}}){extension{pkgName isInstalled versionName apkUrl}}}\"}")"
say "  updateExtension(install:true) -> $INSTALL"
if printf '%s' "$INSTALL" | grep -q '"isInstalled":true'; then
  pass "MangaDex extension installed via store (jarUrl path — URLClassLoader load OK)"
else
  fail "MangaDex extension not installed via store (got: $INSTALL)"
fi

# (e2) FORCE the dex2jar APK->JAR path so we actually exercise the ASM-based DEX->JVM
# converter under the stock aarch64 JDK (the Android-relevant claim). The store path
# above skips it; installExternalExtension uploads the raw .apk, which Suwayomi converts
# via com.googlecode.d2j (femtopedia dex2jar 2.4.37) before loading. We assert both the
# install AND that d2j converter classes actually loaded (proof conversion ran here).
say "=== [e2] force dex2jar via installExternalExtension(apk upload) ==="
APKURL="$(gql "{\"query\":\"query{extension(pkgName:\\\"$MANGADEX_PKG\\\"){apkUrl}}\"}" | grep -oE 'https://[^"]+\.apk')"
if [[ -z "$APKURL" ]]; then
  fail "could not resolve MangaDex apkUrl for the dex2jar path"
else
  say "  apkUrl = $APKURL"
  APK="$OUT/mangadex.apk"
  if curl -sL -m 60 -o "$APK" "$APKURL" && [[ -s "$APK" ]]; then
    UP="$(curl -s -m 90 -X POST "$BASE/api/graphql" \
      -F 'operations={"query":"mutation($extensionFile:Upload!){installExternalExtension(input:{extensionFile:$extensionFile}){extension{pkgName isInstalled versionName}}}","variables":{"extensionFile":null}}' \
      -F 'map={"0":["variables.extensionFile"]}' \
      -F "0=@$APK;type=application/vnd.android.package-archive;filename=mangadex.apk" 2>/dev/null)"
    say "  installExternalExtension -> $UP"
    if printf '%s' "$UP" | grep -q '"isInstalled":true'; then
      pass "MangaDex apk installed via installExternalExtension"
    else
      fail "installExternalExtension did not install (got: $UP)"
    fi
    # snapshot the log now and count dex2jar converter class-loads
    docker logs "$CNAME" > "$ENGINE_LOG" 2>&1 || true
    D2J=$(grep -cE '\[class,load\] com\.googlecode\.(d2j|dex2jar)' "$ENGINE_LOG" 2>/dev/null); D2J=${D2J:-0}
    say "  dex2jar (com.googlecode.d2j/dex2jar) class-loads observed: $D2J"
    if [[ "$D2J" -gt 0 ]]; then
      pass "dex2jar APK->JAR conversion ran on stock aarch64 JDK 21 ($D2J converter classes loaded)"
    else
      fail "no dex2jar converter classes loaded — APK->JAR path did not execute"
    fi
    rm -f "$APK"
  else
    fail "failed to download MangaDex apk from $APKURL (network?)"
  fi
fi

# --- (f) resolve EN MangaDex source id + browse POPULAR (OkHttp + Exposed write) -----
say "=== [f] resolve EN source id + fetchSourceManga POPULAR p1 (network + DB write) ==="
SOURCES="$(gql '{"query":"query{sources(filter:{name:{includesInsensitive:\"mangadex\"}}){nodes{id lang}}}"}')"
# pick the English source id: find the {"id":"...","lang":"en"} node
SRCID="$(printf '%s' "$SOURCES" \
  | grep -oE '"id":"[0-9]+","lang":"[a-zA-Z-]+"' \
  | grep '"lang":"en"' \
  | grep -oE '"id":"[0-9]+"' | head -1 | grep -oE '[0-9]+')"
if [[ -n "$SRCID" ]]; then
  pass "resolved EN MangaDex source id = $SRCID"
else
  fail "could not resolve EN MangaDex source id (sources head: $(printf '%s' "$SOURCES" | head -c 300))"
fi

if [[ -n "$SRCID" ]]; then
  BROWSE="$(gql "{\"query\":\"mutation{fetchSourceManga(input:{source:\\\"$SRCID\\\",type:POPULAR,page:1}){hasNextPage mangas{id title}}}\"}")"
  MCOUNT=$(printf '%s' "$BROWSE" | grep -o '"title"' | wc -l | tr -d ' ')
  say "  fetchSourceManga POPULAR p1 -> $MCOUNT mangas; head: $(printf '%s' "$BROWSE" | head -c 220)"
  if [[ "$MCOUNT" -gt 0 ]]; then
    pass "fetchSourceManga(POPULAR,p1) returned $MCOUNT mangas (OkHttp fetch + Exposed/DB persist OK)"
  else
    fail "fetchSourceManga returned empty mangas (got: $(printf '%s' "$BROWSE" | head -c 400))"
  fi
fi

# --- inspect what embedded DB the engine actually created (Open Q5 corroboration) ----
say "=== embedded DB files created under rootDir (Q5 corroboration) ==="
find "$DATADIR" -maxdepth 3 -type f \( -name '*.db' -o -name '*.mv.db' -o -name '*.trace.db' -o -name '*.sqlite*' \) 2>/dev/null \
  | sed "s#$DATADIR#<rootDir>#" | tee "$OUT/db-files.txt" || true

# --- AWT-reachability signal for N3.7 (indicative only) ------------------------------
# Re-dump the FULL container log now (docker logs returns complete history) so the signal
# covers boot + dex2jar extension load + browse, not just boot.
docker logs "$CNAME" > "$ENGINE_LOG" 2>&1 || true
TOTAL_LOADS=$(grep -c '\[class,load\]' "$ENGINE_LOG" 2>/dev/null); TOTAL_LOADS=${TOTAL_LOADS:-0}
say "=== AWT/font/imageio class-load signal (-verbose:class; indicative only) ==="
say "  total class-loads observed across probe: $TOTAL_LOADS"
AWT=$(grep -cE '\[class,load\] java\.awt\.' "$ENGINE_LOG" 2>/dev/null); AWT=${AWT:-0}
FONT=$(grep -cE '\[class,load\] sun\.font\.' "$ENGINE_LOG" 2>/dev/null); FONT=${FONT:-0}
IMIO=$(grep -cE '\[class,load\] javax\.imageio\.' "$ENGINE_LOG" 2>/dev/null); IMIO=${IMIO:-0}
say "  java.awt.*    class-loads: $AWT"
say "  sun.font.*    class-loads: $FONT"
say "  javax.imageio.* class-loads: $IMIO"
printf 'java.awt=%s sun.font=%s javax.imageio=%s\n' "$AWT" "$FONT" "$IMIO" > "$OUT/awt-signal.txt"

# --- GraalJS status + init warnings / stack traces -----------------------------------
# GraalJS (js-community, PURE-JAVA, no native .so) is bundled and its wrapper loads at
# boot, but the Truffle polyglot Context is LAZILY created on first JS eval. A MangaDex
# browse does not evaluate JS, so no Context spins up here (0 truffle/graal class-loads
# is expected). We report the wrapper load as positive "wired" evidence and scan for any
# init error/warning.
say "=== GraalJS status ==="
JSENG=$(grep -cE '\[class,load\] eu\.kanade\.tachiyomi\.network\.JavaScriptEngine' "$ENGINE_LOG" 2>/dev/null); JSENG=${JSENG:-0}
GRAAL=$(grep -cE '\[class,load\] (com\.oracle\.truffle|org\.graalvm\.polyglot)' "$ENGINE_LOG" 2>/dev/null); GRAAL=${GRAAL:-0}
say "  JavaScriptEngine wrapper class-loads: $JSENG (bundled js-community per jar; pure-Java, no .so)"
say "  Truffle/polyglot Context class-loads: $GRAAL (0 = lazy init not triggered by browse — expected)"
say "=== GraalJS / warning / exception scan (non class-load lines) ==="
grep -iE 'graal|polyglot|truffle|WARN|Exception|error' "$ENGINE_LOG" 2>/dev/null | grep -v '\[class,load\]' | head -30 \
  || say "  (no graal/warn/exception lines matched)"

# --- summary --------------------------------------------------------------------------
say ""
say "=== SUMMARY ==="
say "cold-start: ${COLD}s   PASS=$PASS  FAIL=$FAIL"
if [[ "$FAIL" -eq 0 ]]; then
  say "RESULT: PASS"
  exit 0
else
  say "RESULT: FAIL"
  exit 1
fi
