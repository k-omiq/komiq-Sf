#!/usr/bin/env bash
# Deterministic fallback-ladder memo check (N2.2). No Tauri, no engine, no network.
# esbuild-bundles the REAL @komika/api and runs fallback-ladder.check.ts under node,
# printing PASS/FAIL per assertion and exiting non-zero on any failure.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"        # .../e2e/fallback-ladder
REPO="$(cd "$SCRIPT_DIR/../../../../.." && pwd)"                   # repo root
OUT="$SCRIPT_DIR/build"                                            # generated bundle (gitignored)
mkdir -p "$OUT"

# esbuild resolver: mirror offline-queue/run-offline-check.sh — probe the hoisted .bin, else npx.
if [[ -x "$REPO/node_modules/.bin/esbuild" ]]; then
  ESBUILD=("$REPO/node_modules/.bin/esbuild")
elif [[ -x "$REPO/node_modules/.pnpm/node_modules/.bin/esbuild" ]]; then
  ESBUILD=("$REPO/node_modules/.pnpm/node_modules/.bin/esbuild")
else
  ESBUILD=(npx --yes esbuild)
fi

BUNDLE="$OUT/fallback-ladder.check.mjs"

echo "=== BUNDLE === (${ESBUILD[*]})"
# Entry lives under apps/reader/**, so esbuild resolves @komika/api + @komika/types
# via the workspace symlinks in apps/reader/node_modules — no --resolve-dir needed.
"${ESBUILD[@]}" "$SCRIPT_DIR/fallback-ladder.check.ts" \
  --bundle --platform=node --format=esm \
  --outfile="$BUNDLE" 2>&1 || { echo "esbuild failed"; exit 5; }

echo "=== RUN CHECK ==="
node "$BUNDLE"
RC=$?
echo "=== check exit code: $RC ==="
exit $RC
