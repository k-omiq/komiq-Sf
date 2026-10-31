#!/usr/bin/env bash
# Run every target through the three solvers, one site at a time, and append one
# JSON line per (solver, site) to $OUT (default out/results.jsonl). Run from this
# directory on the test host after `docker compose up -d --build`.
# WARP=1 sends the solvers, the browser and both replays through the warp service.
# Usage: [WARP=1] [OUT=file] ./run.sh [targets.tsv] [solver ...]
#        (solvers: fs-pinned fs-latest bw)
# targets.tsv stays local and out of git: one name<TAB>url line per site.
set -uo pipefail
cd "$(dirname "$0")"
targets=${1:-targets.tsv}
shift || true
solvers=("$@")
[ ${#solvers[@]} -gt 0 ] || solvers=(fs-pinned fs-latest bw)
[ "${WARP:-0}" = 1 ] && EGRESS=warp
if [ "${EGRESS:-direct}" = warp ]; then
  export ROUTE=warp PROXY=socks5://warp:1080 HOST_PROXY=127.0.0.1:11080 JVM_PROXY=warp:1080
  bw_env=(-e PROXY="$PROXY")
elif [ "${EGRESS:-direct}" = router ]; then
  # Chromium cannot authenticate to a SOCKS proxy, so the browser uses the router's
  # HTTP port; the router itself holds the upstream proxy's login.
  export ROUTE=router PROXY=http://router:8080 HOST_PROXY=127.0.0.1:11081 JVM_PROXY=router:8080
  bw_env=(-e PROXY="$PROXY")
else
  export ROUTE=direct
  unset PROXY HOST_PROXY JVM_PROXY
  bw_env=()
fi
mkdir -p out
while IFS=$'\t' read -r _ url; do
  [ -n "$url" ] || continue
  for s in "${solvers[@]}"; do
    case $s in
      fs-pinned) python3 fs-solve.py fs-pinned http://127.0.0.1:18191 "$url" ;;
      fs-latest) python3 fs-solve.py fs-latest http://127.0.0.1:18192 "$url" ;;
      bw) timeout 150 docker compose exec -T "${bw_env[@]}" bw node bw-solve.mjs "$url" < /dev/null ||
        printf '{"solver":"betterwright","url":"%s","ok":false,"error":"runner timeout"}\n' "$url" ;;
    esac | python3 replay-all.py >> "${OUT:-out/results.jsonl}"
  done
  echo "done $url" >&2
done < "$targets"
