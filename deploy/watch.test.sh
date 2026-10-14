#!/bin/sh
# watch.sh against stand-in docker and curl: no Docker, no network.
# Proves decolorized/JSON severity counting, the wall-clock deadline with gap
# reporting and a final observation, STOP lines, unknown backup state counted
# as unavailable, the URL, production-host and Docker-daemon refusals, and the
# production addresses read from KOMIKA_PRODUCTION_IPS and production-ips.local.
# About 15 s.
set -eu
source_watch="${WATCH_UNDER_TEST:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/watch.sh}"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
# A copy in its own directory: its production-ips.local is ours, and the real
# one (present or not) plays no part. 203.0.113.207 (RFC 5737) stands in for
# the production address.
mkdir "$scratch/bin" "$scratch/deploy"
cp "$source_watch" "$scratch/deploy/watch.sh"
watch="$scratch/deploy/watch.sh"
ips_file="$scratch/deploy/production-ips.local"
export KOMIKA_PRODUCTION_IPS=203.0.113.207
cat > "$scratch/bin/docker" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "$STUB_TRACE"
case $1 in
  context) echo "${DOCKER_CONTEXT:-default}" ;;
  ps)
    case "$*" in
      *service=server*) echo c1 ;;
      *service=postgres-backup*) echo c3 ;;
      *service=postgres*) echo c2 ;;
      *) printf 'c1\nc2\nc3\n' ;;
    esac ;;
  inspect)
    restarts=0
    if [ -n "${STUB_RESTART:-}" ] && [ -e "$STUB_STATE/inspected" ]; then restarts=1; fi
    touch "$STUB_STATE/inspected"
    printf '/komika-server-1 c1 %s running false\n/komika-postgres-1 c2 0 running false\n/komika-postgres-backup-1 c3 0 running false\n' "$restarts" ;;
  logs)
    case "$*" in
      *c1*)
        e=$(printf '\033')
        printf '%s[2m2026-10-14T12:00:00.1Z%s[0m %s[31mERROR%s[0m komika_server::graphql: search timed out\n' "$e" "$e" "$e" "$e"
        printf '%s[2m2026-10-14T12:00:01.1Z%s[0m %s[1;33m WARN%s[0m komika_server: slow source\n' "$e" "$e" "$e" "$e"
        printf '2026-10-14T12:00:02.1Z  INFO komika_server: retried after ERROR from upstream\n'
        printf '{"timestamp":"2026-10-14T12:00:03Z","level":"error","fields":{"message":"boom"}}\n'
        printf '{"timestamp":"2026-10-14T12:00:04Z","level":"INFO","fields":{"message":"ERROR word only"}}\n' ;;
      *c2*)
        printf '2026-10-14 12:00:00.000 UTC [7] ERROR:  canceling statement due to statement timeout\n'
        printf '2026-10-14 12:00:01.000 UTC [8] FATAL:  terminating connection due to administrator command\n'
        printf '2026-10-14 12:00:02.000 UTC [9] LOG:  statement: SELECT 1 -- ERROR is only text here\n'
        [ -z "${STUB_DROPPED:-}" ] || printf "P00   WARN: dropped WAL file '0000000100000000000000A1' because archive queue exceeded 1GB\n" ;;
    esac ;;
  exec)
    printf '{"gap_open": %s, "archiver": {"failing": false, "stalled": %s, "failed_count": 0}, "ready_files": 0, "dropped_wal": {"events_recorded": 0}, "free_bytes": 99}\n' "${STUB_GAP:-false}" "${STUB_STALLED:-false}" ;;
  *) exit 9 ;;
esac
SH
cat > "$scratch/bin/curl" <<'SH'
#!/bin/sh
printf 'curl\n' >> "$STUB_TRACE"
count=$(($(cat "$STUB_STATE/curl" 2>/dev/null || echo 0) + 1))
echo "$count" > "$STUB_STATE/curl"
# The second round stalls: the watch must report it as a gap, not lose time.
[ "$count" != 2 ] || sleep "${STUB_STALL:-0}"
printf '200 0.120'
SH
chmod +x "$scratch/bin/docker" "$scratch/bin/curl"
export PATH="$scratch/bin:$PATH" STUB_TRACE="$scratch/trace" STUB_STATE="$scratch/state"
mkdir "$scratch/state"
fail() { echo "FAIL: $*" >&2; cat "$scratch/out" >&2; exit 1; }

# 1. A clean watch: deadline by wall clock, one stalled round reported as a gap.
STUB_STALL=3 sh "$watch" --url http://127.0.0.1:9/health/ready --seconds 6 --interval 1 > "$scratch/out" || fail "clean watch exited non-zero"
grep -qx 'server_log error=2 warn=1 lines=5' "$scratch/out" || fail "decolorized/JSON server severities"
grep -qx 'postgres_log error=1 fatal=1 panic=0 dropped_wal=0' "$scratch/out" || fail "PostgreSQL severities"
grep -q '^backup gap_open=false archiver_failing=false archiver_stalled=false ' "$scratch/out" || fail "backup status line"
grep -q '^STOP' "$scratch/out" && fail "clean watch printed STOP"
covered=$(sed -n 's/^covered_seconds=\([0-9]*\) requested_seconds=6 .*/\1/p' "$scratch/out")
[ -n "$covered" ] && [ "$covered" -ge 6 ] && [ "$covered" -le 8 ] || fail "covered_seconds $covered not about 6"
rounds=$(grep -c '^round=' "$scratch/out")
# A round counter would run 6 rounds and take 9 s; the deadline cuts it shorter.
[ "$rounds" -lt 6 ] || fail "$rounds rounds: the stall was not absorbed by the deadline"
grep -q '^gap from=[0-9]* to=[0-9]* seconds=[3-9]$' "$scratch/out" || fail "the stalled round is not reported as a gap"
grep -q 'gaps_over_2s=1' "$scratch/out" || fail "gap count"
# The last round observes at or after the deadline, before success is decided.
last=$(sed -n 's/^round=[0-9]* at=[^ ]* elapsed=\([0-9]*\)$/\1/p' "$scratch/out" | tail -n 1)
[ -n "$last" ] && [ "$last" -ge 6 ] || fail "no observation at or after the deadline (last elapsed=$last)"
grep -c '^health code=' "$scratch/out" | grep -qx "$rounds" || fail "a round without a health observation"

# 2. STOP lines: a restart, dropped WAL and an open gap; exit status 1.
rm -f "$scratch/state/"*
if STUB_RESTART=1 STUB_DROPPED=1 STUB_GAP=true sh "$watch" --url http://127.0.0.1:9/ --seconds 2 --interval 1 > "$scratch/out"; then
    fail "a failing watch exited 0"
fi
grep -q '^STOP: komika-server-1 restarted$' "$scratch/out" || fail "restart STOP"
grep -q '^STOP: pgBackRest dropped WAL' "$scratch/out" || fail "dropped-WAL STOP"
grep -q '^STOP: dropped WAL since the latest full backup (gap_open)$' "$scratch/out" || fail "gap STOP"

# 2b. A stalled archiver stops; unknown gap or archiver state is unavailable
# and never resets the count of unavailable rounds.
rm -f "$scratch/state/"*
if STUB_STALLED=true sh "$watch" --url http://127.0.0.1:9/ --seconds 1 --interval 1 > "$scratch/out"; then
    fail "a stalled archiver exited 0"
fi
grep -q '^STOP: WAL waits to be archived with no progress for 15 minutes$' "$scratch/out" || fail "stall STOP"
for unknown in "STUB_GAP=null" "STUB_STALLED=null"; do
    rm -f "$scratch/state/"*
    if env "$unknown" sh "$watch" --url http://127.0.0.1:9/ --seconds 2 --interval 1 > "$scratch/out"; then
        fail "$unknown exited 0"
    fi
    grep -q '^backup unavailable' "$scratch/out" || fail "$unknown not counted as unavailable"
    grep -q '^STOP: backup state unavailable in 3 rounds in a row$' "$scratch/out" || fail "$unknown reset the count"
done

# 3. Production hosts are refused before anything runs, unless --production.
: > "$scratch/trace"
for url in https://api.komiq.cc/health https://KOMIQ.CC./ http://Api.Komiq.CC..:8080/x http://203.0.113.207/ \
    http://3405803983/ http://0xcb.0x0.0x71.0xcf/ http://0313.0.0161.0317/ 'http://[::ffff:203.0.113.207]/' \
    'http://[64:ff9b::cb00:71cf]/' 'http://[2002:cb00:71cf::1]/' 'http://[::203.0.113.207]/'; do
    if sh "$watch" --url "$url" --seconds 1 > "$scratch/out" 2>&1; then fail "accepted $url"; fi
    grep -q -- 'is production; pass --production' "$scratch/out" || fail "refusal of $url does not name --production"
done
# Never watched, not even with --production: other schemes, user information, % in the host.
for url in ftp://127.0.0.1/ file:///etc/passwd http://user@127.0.0.1/ http://user:pw@komiq.cc/ \
    http://127.0.0.1%2e/ 'http://[fe80::1%25en0]/' -K/etc/passwd 'http://127.0.0.1/ x'; do
    if sh "$watch" --url "$url" --seconds 1 --production > "$scratch/out" 2>&1; then fail "accepted $url"; fi
    grep -q '^watch.sh: refused: ' "$scratch/out" || fail "refusal of $url has no reason"
done
[ ! -s "$scratch/trace" ] || fail "a refused watch ran docker or curl"
sh "$watch" --url https://api.komiq.cc/health --seconds 1 --interval 1 --production > "$scratch/out" || fail "--production watch"
grep -qx -- 'curl' "$scratch/trace" || fail "the --production watch did not observe"

# 4. A remote Docker daemon is refused unless --production.
for daemon in "DOCKER_HOST=tcp://10.0.0.5:2375" "DOCKER_HOST=ssh://admin@vps" "DOCKER_CONTEXT=vps"; do
    : > "$scratch/trace"
    if env "$daemon" sh "$watch" --url http://127.0.0.1:9/ --seconds 1 > "$scratch/out" 2>&1; then fail "accepted $daemon"; fi
    grep -q -- 'pass --production to watch through it' "$scratch/out" || fail "refusal of $daemon"
    grep -q '^curl$' "$scratch/trace" && fail "$daemon: curl ran after the refusal"
    env "$daemon" sh "$watch" --url http://127.0.0.1:9/ --seconds 1 --interval 1 --production > "$scratch/out" ||
        fail "$daemon with --production"
done
DOCKER_HOST=unix:///var/run/docker.sock sh "$watch" --url http://127.0.0.1:9/ --seconds 1 --interval 1 > "$scratch/out" ||
    fail "local DOCKER_HOST socket refused"

# 5. Production addresses: the environment and production-ips.local next to the
# script, joined; a bad entry fails closed; with neither, one warning and only
# the komiq.cc names are refused.
refused() { # URL [VAR=VALUE]: refused as production, before docker or curl ran.
    : > "$scratch/trace"
    if env ${2:+"$2"} sh "$watch" --url "$1" --seconds 1 > "$scratch/out" 2>&1; then fail "accepted $1 ${2:-}"; fi
    grep -q -- 'is production; pass --production' "$scratch/out" || fail "refusal of $1 ${2:-} is not the production one"
    [ ! -s "$scratch/trace" ] || fail "$1 ${2:-}: docker or curl ran"
}
watched() { # URL [VAR=VALUE]: not production, the watch ran.
    : > "$scratch/trace"
    env ${2:+"$2"} sh "$watch" --url "$1" --seconds 1 --interval 1 > "$scratch/out" 2> "$scratch/err" || fail "refused $1 ${2:-}"
    grep -q '^curl$' "$scratch/trace" || fail "$1 ${2:-}: the watch did not observe"
}
watched http://203.0.113.206/
watched 'http://[::ffff:cb00:71ce]/'
watched http://198.51.100.6/
[ ! -s "$scratch/err" ] || fail "a configured watch warned: $(cat "$scratch/err")"
refused http://198.51.100.6/ 'KOMIKA_PRODUCTION_IPS=198.51.100.5, 198.51.100.6'
refused http://198.51.100.5/ 'KOMIKA_PRODUCTION_IPS=198.51.100.5	198.51.100.6'
printf '# stand-in production hosts\n198.51.100.7\n\n  198.51.100.8  # second\n#198.51.100.9\n' > "$ips_file"
refused http://198.51.100.7/ KOMIKA_PRODUCTION_IPS=
refused 'http://[::ffff:198.51.100.8]/' KOMIKA_PRODUCTION_IPS=
watched http://198.51.100.9/ KOMIKA_PRODUCTION_IPS=
refused http://203.0.113.207/   # the environment and the file, joined
refused http://198.51.100.8/
for bad in production.example 203.0.113.256 203.0.113.07 0xcb0071cf '::ffff:cb00:71cf'; do
    # Even with --production: a bad entry is never skipped.
    : > "$scratch/trace"
    if KOMIKA_PRODUCTION_IPS="203.0.113.207,$bad" sh "$watch" --url http://127.0.0.1:9/ --seconds 1 --production > "$scratch/out" 2>&1; then
        fail "accepted $bad"
    fi
    grep -qF "watch.sh: refused: production address configuration: not an IPv4 address: KOMIKA_PRODUCTION_IPS: '$bad'" "$scratch/out" ||
        fail "refusal of $bad does not name the entry"
    [ ! -s "$scratch/trace" ] || fail "$bad: docker or curl ran"
done
printf '198.51.100.7\n198.51.100.300 # typo\n' > "$ips_file"
if sh "$watch" --url http://127.0.0.1:9/ --seconds 1 > "$scratch/out" 2>&1; then fail "accepted a bad production-ips.local"; fi
grep -q "production-ips.local:2: '198.51.100.300'" "$scratch/out" || fail "refusal of the bad file line"
rm -f "$ips_file"
unset KOMIKA_PRODUCTION_IPS
: > "$scratch/trace"
if sh "$watch" --url https://api.komiq.cc/ --seconds 1 > "$scratch/out" 2>&1; then fail "unconfigured: accepted komiq.cc"; fi
grep -q -- 'is production; pass --production' "$scratch/out" || fail "unconfigured: komiq.cc refusal"
[ "$(grep -c 'warning: no production address is configured' "$scratch/out")" = 1 ] || fail "unconfigured: not exactly one warning"
watched http://203.0.113.207/
[ "$(grep -c 'warning: no production address is configured' "$scratch/err")" = 1 ] || fail "unconfigured watch: not exactly one warning"
echo 'watch: decolorized_counts elapsed_deadline final_round gap_report stop_lines unknown_state url_refusal production_refusal docker_daemon production_config passed'
