#!/bin/sh
# Read-only watch after a deploy. Each round: the health answer, restarts and
# memory kills of the project's containers, server log severities and
# PostgreSQL ERROR/FATAL/PANIC/dropped-WAL lines since the watch began, and
# the backup side's gap and archiver state (`hosted.py status --local`, no
# repository request). It runs to a wall-clock deadline, not a round count,
# and ends with the seconds it really covered and every gap between rounds.
#
#   deploy/watch.sh --url URL [--minutes N | --seconds N] [--interval S] [--project NAME] [--production]
#
# It never changes a container and never prints configuration. STOP lines
# mark hard failures; the exit status is 1 when any was printed. Only an
# http(s) URL without user information is watched; production (komiq.cc, its
# subdomains, the configured production addresses) and a Docker daemon that is
# not the local default need --production.
#
# The production addresses are not in the source. They come from
# KOMIKA_PRODUCTION_IPS (IPv4 addresses separated by commas or spaces) and the
# git-ignored deploy/production-ips.local next to this script (one IPv4 address
# per line, # comments), joined; apps/server/bench/capacity.mjs reads the same
# two. A bad entry stops the watch. With neither set only the komiq.cc names
# are refused, and the watch says so once.
set -eu
set +x

usage() {
    echo "usage: watch.sh --url URL [--minutes N | --seconds N] [--interval SECONDS] [--project NAME] [--production]" >&2
    exit 2
}
number() { case $1 in '' | *[!0-9]*) usage ;; esac; }

url= seconds=3600 interval=30 project=komika production=
while [ $# -gt 0 ]; do
    case $1 in
        --url) [ $# -ge 2 ] || usage; url=$2; shift 2 ;;
        --minutes) [ $# -ge 2 ] || usage; number "$2"; seconds=$(($2 * 60)); shift 2 ;;
        --seconds) [ $# -ge 2 ] || usage; number "$2"; seconds=$2; shift 2 ;;
        --interval) [ $# -ge 2 ] || usage; number "$2"; interval=$2; shift 2 ;;
        --project) [ $# -ge 2 ] || usage; project=$2; shift 2 ;;
        --production) production=1; shift ;;
        *) usage ;;
    esac
done
[ -n "$url" ] || usage
[ "$interval" -gt 0 ] || usage
case $project in '' | *[!a-z0-9_-]*) usage ;; esac
# The production addresses are compared as bytes in every spelling curl accepts.
here=$(CDPATH= cd -- "$(dirname -- "$(readlink -f -- "$0")")" && pwd)
verdict=$(python3 -I -c '
import ipaddress, os, re, socket, sys, urllib.parse
url, ips_file = sys.argv[1], sys.argv[2]
prefix = ""
def say(text):
    print(prefix + text)
    sys.exit(0)
# KOMIKA_PRODUCTION_IPS and the local file, joined; a bad entry fails closed.
def ipv4(entry):
    if not re.fullmatch(r"(0|[1-9][0-9]{0,2})(\.(0|[1-9][0-9]{0,2})){3}", entry):
        return None
    return ipaddress.IPv4Address(entry) if all(int(x) <= 255 for x in entry.split(".")) else None
production, bad = set(), []
def add(entry, where):
    if ipv4(entry):
        production.add(ipv4(entry))
    else:
        bad.append("%s: %r" % (where, entry))
for entry in re.split(r"[\s,]+", os.environ.get("KOMIKA_PRODUCTION_IPS", "")):
    if entry:
        add(entry, "KOMIKA_PRODUCTION_IPS")
try:
    with open(ips_file, encoding="utf-8") as f:
        lines = f.read().splitlines()
except FileNotFoundError:
    lines = []
except (OSError, UnicodeError) as e:
    say("refused: production address configuration: %s cannot be read (%s)" % (ips_file, type(e).__name__))
for number, line in enumerate(lines, 1):
    entry = line.split("#", 1)[0].strip()
    if entry:
        add(entry, "%s:%d" % (ips_file, number))
if bad:
    say("refused: production address configuration: not an IPv4 address: " + ", ".join(bad))
if not production:
    prefix = "unconfigured "
if any(c.isspace() or ord(c) < 32 or ord(c) == 127 for c in url):
    say("refused: the URL has spaces or control characters")
try:
    parts = urllib.parse.urlsplit(url)
    parts.port
except ValueError:
    say("refused: the URL does not parse")
if parts.scheme.lower() not in ("http", "https"):
    say("refused: only http and https URLs are watched")
if "@" in parts.netloc:
    say("refused: the URL carries user information")
if "%" in parts.netloc:
    say("refused: the URL host carries a % escape")
host = (parts.hostname or "").rstrip(".")
if not host:
    say("refused: the URL has no host")
if host == "komiq.cc" or host.endswith(".komiq.cc"):
    say("production " + host)
try:
    address = ipaddress.ip_address(host)
except ValueError:
    try:
        address = ipaddress.ip_address(socket.inet_aton(host))  # One number, hex or octal parts.
    except (OSError, ValueError):
        address = None
if address is not None and address.version == 6:
    carried = [address.ipv4_mapped, address.sixtofour] + [
        ipaddress.ip_address(address.packed[12:]) for net in ("::/96", "::ffff:0:0:0/96", "64:ff9b::/96")
        if address in ipaddress.ip_network(net)]
    address = next((a for a in carried if a in production), address)
say("production " + host if address in production else "ok")
' "$url" "$here/production-ips.local" 2> /dev/null) || verdict="refused: the URL could not be checked"
case $verdict in
    "unconfigured "*)
        echo "watch.sh: warning: no production address is configured (KOMIKA_PRODUCTION_IPS or deploy/production-ips.local); only komiq.cc and *.komiq.cc are refused" >&2
        verdict=${verdict#unconfigured } ;;
esac
case $verdict in
    ok) ;;
    "production "*)
        [ -n "$production" ] || { echo "watch.sh: ${verdict#production } is production; pass --production to watch it" >&2; exit 2; } ;;
    *) echo "watch.sh: $verdict" >&2; exit 2 ;;
esac
if [ -z "$production" ]; then
    case ${DOCKER_HOST:-} in
        '' | unix://*) ;;
        *) echo "watch.sh: DOCKER_HOST is not a local socket; pass --production to watch through it" >&2; exit 2 ;;
    esac
    context=$(docker context show 2> /dev/null || echo unknown)
    [ "$context" = default ] ||
        { echo "watch.sh: docker context '$context' is not the local default; pass --production to watch through it" >&2; exit 2; }
fi

scratch=$(mktemp -d)
esc=$(printf '\033')
stops=0 health_bad=0 backup_bad=0 round=0 previous= gaps=0
stop() { echo "STOP: $*"; stops=$((stops + 1)); }
decolor() { sed "s/${esc}\\[[0-9;?]*[A-Za-z]//g"; }
one() {
    docker ps -aq --no-trunc --filter "label=com.docker.compose.project=$project" \
        --filter "label=com.docker.compose.oneoff=False" --filter "label=com.docker.compose.service=$1" | head -n 1
}
containers() {
    # NAME ID RESTARTS STATUS OOM, one line per container of the project.
    ids=$(docker ps -aq --no-trunc --filter "label=com.docker.compose.project=$project" \
        --filter "label=com.docker.compose.oneoff=False")
    [ -z "$ids" ] || docker inspect --format '{{.Name}} {{.Id}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' $ids | sort
}
severities() {
    # Server log: JSON "level" or the first level word of a (decolorized) text line.
    decolor | awk '{
        level = ""
        if (match($0, /"level" *: *"[A-Za-z]+"/)) {
            level = substr($0, RSTART, RLENGTH); sub(/^.*: *"/, "", level); sub(/"$/, "", level)
        } else for (i = 1; i <= NF && i <= 4; i++) if (toupper($i) ~ /^(TRACE|DEBUG|INFO|WARN|WARNING|ERROR)$/) { level = $i; break }
        level = toupper(level)
        if (level == "ERROR") e++; else if (level == "WARN" || level == "WARNING") w++
        n++
    } END { printf "error=%d warn=%d lines=%d\n", e, w, n }'
}
postgres_lines() {
    decolor | awk '/(^| )ERROR: / {e++} /(^| )FATAL: / {f++} /(^| )PANIC: / {p++} /dropped WAL file/ {d++}
        END { printf "error=%d fatal=%d panic=%d dropped_wal=%d\n", e, f, p, d }'
}
finish() {
    end=$(date +%s)
    if [ -n "$previous" ] && [ $((end - previous)) -gt $((2 * interval)) ]; then
        echo "gap from=$previous to=$end seconds=$((end - previous))" >> "$scratch/gaps"
        gaps=$((gaps + 1))
    fi
    echo "covered_seconds=$((end - start)) requested_seconds=$seconds rounds=$round gaps_over_$((2 * interval))s=$gaps"
    [ ! -s "$scratch/gaps" ] || cat "$scratch/gaps"
    [ $((end - start)) -ge "$seconds" ] || stop "the watch ended after $((end - start)) of $seconds seconds"
}
trap 'rm -rf "$scratch"' EXIT
trap 'finish; exit 130' INT TERM

start=$(date +%s)
deadline=$((start + seconds))
containers > "$scratch/baseline" || :
[ -s "$scratch/baseline" ] || stop "project $project has no containers"
echo "watch start=$(date -u +%Y-%m-%dT%H:%M:%SZ) seconds=$seconds interval=$interval project=$project url=$url"

while :; do
    now=$(date +%s)
    # The round that starts at or after the deadline is the last observation.
    final=
    [ "$now" -lt "$deadline" ] || final=1
    round=$((round + 1))
    if [ -n "$previous" ] && [ $((now - previous)) -gt $((2 * interval)) ]; then
        echo "gap from=$previous to=$now seconds=$((now - previous))" >> "$scratch/gaps"
        gaps=$((gaps + 1))
    fi
    previous=$now
    echo "round=$round at=$(date -u +%Y-%m-%dT%H:%M:%SZ) elapsed=$((now - start))"

    answer=$(curl -o /dev/null -sS -m 10 -w '%{http_code} %{time_total}' -- "$url" 2> /dev/null || :)
    set -- $answer
    echo "health code=${1:-000} seconds=${2:-none}"
    if [ "${1:-000}" = 200 ]; then health_bad=0; else health_bad=$((health_bad + 1)); fi
    [ "$health_bad" -lt 3 ] || stop "health answer not 200 in $health_bad rounds in a row"

    containers > "$scratch/now" || :
    awk -v b="$scratch/baseline" '
        BEGIN { while ((getline line < b) > 0) { split(line, f, " "); id[f[1]] = f[2]; restarts[f[1]] = f[3] } }
        { seen[$1] = 1
          printf "container %s restarts=%s status=%s oom=%s\n", substr($1, 2), $3, $4, $5
          if ($1 in id && $2 != id[$1]) print "STOP: " substr($1, 2) " was recreated"
          else if ($1 in id && $3 > restarts[$1]) print "STOP: " substr($1, 2) " restarted"
          if ($4 != "running") print "STOP: " substr($1, 2) " is " $4
          if ($5 == "true") print "STOP: " substr($1, 2) " was killed for memory" }
        END { for (name in id) if (!(name in seen)) print "STOP: " substr(name, 2) " is gone" }' "$scratch/now" > "$scratch/rows"
    cat "$scratch/rows"
    while IFS= read -r line; do
        case $line in "STOP: "*) stops=$((stops + 1)) ;; esac
    done < "$scratch/rows"

    server=$(one server)
    if [ -n "$server" ]; then
        echo "server_log $(docker logs --since "$start" "$server" 2>&1 | severities)"
    else
        echo "server_log unavailable (no server container)"
    fi
    postgres=$(one postgres)
    if [ -n "$postgres" ]; then
        counts=$(docker logs --since "$start" "$postgres" 2>&1 | postgres_lines)
        echo "postgres_log $counts"
        case $counts in *" panic=0 "*) ;; *) stop "PostgreSQL logged PANIC" ;; esac
        case $counts in *" dropped_wal=0") ;; *) stop "pgBackRest dropped WAL: PITR has a gap until a new full backup" ;; esac
    else
        echo "postgres_log unavailable (no postgres container)"
    fi

    backup=$(one postgres-backup)
    state=
    [ -z "$backup" ] || state=$(docker exec --user postgres "$backup" python3 /opt/komika-postgres/hosted.py status --local 2> /dev/null |
        python3 -I -c '
import json, sys
d = json.load(sys.stdin)
a, w = d.get("archiver"), d.get("dropped_wal")
def v(x): return "null" if x is None else str(x).lower()
print("gap_open=%s archiver_failing=%s archiver_stalled=%s failed_count=%s ready_files=%s dropped_wal_events=%s "
      "free_bytes=%s" % (v(d.get("gap_open")), v(a and a.get("failing")), v(a and a.get("stalled")),
                         v(a and a.get("failed_count")), v(d.get("ready_files")), v(w and w.get("events_recorded")),
                         v(d.get("free_bytes"))))' 2> /dev/null || :)
    [ -z "$state" ] || echo "backup $state"
    # A required value that is null (unknown) is as unavailable as no answer.
    case $state in
        '' | *"gap_open=null"* | *"archiver_failing=null"* | *"archiver_stalled=null"*)
            backup_bad=$((backup_bad + 1))
            echo "backup unavailable (hosted.py status unreadable, or gap or archiver state unknown)"
            [ "$backup_bad" -lt 3 ] || stop "backup state unavailable in $backup_bad rounds in a row" ;;
        *) backup_bad=0 ;;
    esac
    case $state in *"gap_open=true"*) stop "dropped WAL since the latest full backup (gap_open)" ;; esac
    case $state in *"archiver_failing=true"*) stop "WAL archiving has failed for more than 15 minutes" ;; esac
    case $state in *"archiver_stalled=true"*) stop "WAL waits to be archived with no progress for 15 minutes" ;; esac

    [ -z "$final" ] || break
    after=$(date +%s)
    pause=$((now + interval - after))
    [ "$pause" -le $((deadline - after)) ] || pause=$((deadline - after))
    [ "$pause" -le 0 ] || sleep "$pause"
done
finish
[ "$stops" -eq 0 ]
