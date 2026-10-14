#!/bin/bash
# Test host only (never production). Measure one window: CPU per container, memory, WAL,
# WAL archiver, commits and top statements.
# usage: measure.sh [--json] <label> <window_seconds> [command to run during the window]
#   --json (or MEASURE_JSON=1) prints ONE JSON object on stdout instead of the text
#   report, and sends the optional command's output to stderr. `capacity.mjs --host-cmd`
#   merges that object into its step, e.g.
#     --host-cmd 'ssh testhost ~/measure.sh --json {label} {seconds}'
#   A line `READY` goes to stderr once the baseline is taken, exactly when the window
#   starts; capacity.mjs opens its measured window on it.
# env: PG_CONTAINER (komika-opt-postgres-1), PG_DB (komika), CONTAINERS (space-separated
#   names), TOPN (12 statements), TAILN (14 lines of the command's output, text mode).
# Counters are taken at the window edges: start = baseline just before READY, end = the
# moment the window ends (CPU first, then PostgreSQL in one query). A statistics reset
# inside the window (stats_reset moved, or a counter went down) gives null + reset: true.
JSON=${MEASURE_JSON:-0}
if [ "$1" = "--json" ]; then JSON=1; shift; fi
LABEL=${1:-idle}; WIN=${2:-300}; shift 2 || true
case "$LABEL" in *[!A-Za-z0-9_.-]*) echo "measure.sh: label is letters, digits, _ . - only" >&2; exit 2;; esac
case "$WIN" in ''|*[!0-9]*) echo "measure.sh: window is whole seconds" >&2; exit 2;; esac
PG=${PG_CONTAINER:-komika-opt-postgres-1}
DB=${PG_DB:-komika}
TOPN=${TOPN:-12}
CS=${CONTAINERS:-"komika-opt-server-1 komika-opt-postgres-1 komika-opt-suwayomi-1 komika-opt-flaresolverr-1"}
NCPU=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
q() { sudo -n docker exec -u postgres "$PG" psql -d "$DB" -X -At -F ' | ' -P pager=off -c "$1" 2>&1; }
# One JSON value from psql, or nothing when the query failed.
qj() { sudo -n docker exec -u postgres "$PG" psql -d "$DB" -X -At -P pager=off -c "$1" 2>/dev/null | head -1; }
# Seconds with a fraction (GNU date); whole seconds where %N is not supported.
now() { local t; t=$(date +%s.%N); case "$t" in *N) date +%s;; *) echo "$t";; esac; }
iso() { date -u +%Y-%m-%dT%H:%M:%SZ; }
num() { case "$1" in ''|-|*[!0-9.eE+-]*) printf null;; *) printf '%s' "$1";; esac; }
jstr() { printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | tr '\t\r\n' '   ')"; }
jobj() { case "$1" in \{*\}|\[*\]) printf '%s' "$1";; *) printf null;; esac; }
field() { echo "$1" | awk -F' [|] ' -v i="$2" '{print $i}'; }
hoststat() { [ -r /proc/stat ] && awk '/^cpu /{print $2+$3+$4+$7+$8, $2+$3+$4+$5+$6+$7+$8}' /proc/stat; }

# Containers in plain indexed arrays: names may hold dots and dashes, and no variable
# name is built from them (bash 3.2 on macOS, bash 5 on the test host).
NAMES=($CS)
STATF=()
for i in "${!NAMES[@]}"; do
  id=$(sudo -n docker inspect -f '{{.Id}}' "${NAMES[$i]}" 2>/dev/null)
  f=''
  [ -n "$id" ] && f=$(sudo -n find /sys/fs/cgroup -path "*$id*" -name cpu.stat 2>/dev/null | head -1)
  STATF[$i]=$f
done
cpu_all() { # one line: usage_usec per container, '-' when unknown
  local out='' i v
  for i in "${!NAMES[@]}"; do
    v=-
    [ -n "${STATF[$i]}" ] && v=$(sudo -n awk '/^usage_usec/{print $2}' "${STATF[$i]}" 2>/dev/null)
    out="$out ${v:--}"
  done
  echo "$out"
}
PGSNAP="select w.wal_bytes, w.wal_fpi, w.wal_records, coalesce(w.stats_reset::text, '-'), d.xact_commit, coalesce(d.stats_reset::text, '-'), a.archived_count, a.failed_count, coalesce(a.stats_reset::text, '-'), pg_database_size(current_database()), (select coalesce(sum(size), 0) from pg_ls_waldir()) from pg_stat_wal w, pg_stat_archiver a, pg_stat_database d where d.datname = current_database()"
ARCH_ROW="select row_to_json(a) from (select archived_count, failed_count, last_archived_wal, last_archived_time, last_failed_wal, last_failed_time, stats_reset from pg_stat_archiver) a"

q "create extension if not exists pg_stat_statements" >/dev/null
q "select pg_stat_statements_reset()" >/dev/null
ar0=$(qj "$ARCH_ROW")
p0=$(q "$PGSNAP")
c0=$(cpu_all); h0=$(hoststat); ts0=$(now); start_iso=$(iso)
echo READY >&2
[ "$JSON" = 1 ] || echo "## $LABEL window=${WIN}s start=$(date -u +%H:%M:%S)"
cmd_exit=null
if [ $# -gt 0 ]; then
  if [ "$JSON" = 1 ]; then "$@" >&2; cmd_exit=$?; else "$@" 2>&1 | tail -"${TAILN:-14}"; cmd_exit=${PIPESTATUS[0]}; fi
fi
rest=$(awk -v a="$ts0" -v b="$(now)" -v w="$WIN" 'BEGIN{ r = w - (b - a); if (r > 0) printf "%.3f", r; else print 0 }')
[ "$rest" != 0 ] && sleep "$rest"
# The window edge: CPU and host first (cheap), then PostgreSQL in one query.
c1=$(cpu_all); h1=$(hoststat); ts1=$(now); end_iso=$(iso)
p1=$(q "$PGSNAP")
ar1=$(qj "$ARCH_ROW")
el=$(awk -v a="$ts0" -v b="$ts1" 'BEGIN{ e = b - a; if (e <= 0) e = 0.001; printf "%.3f", e }')

busy=null
[ -n "$h0" ] && [ -n "$h1" ] && busy=$(echo "$h0 $h1" | awk -v n="$NCPU" '{d=$4-$2; if (d>0) printf "%.3f", ($3-$1)/d*n; else print "null"}')
# Delta of field $3 of the snapshots, or empty when either side is not a number.
dlt() { awk -v a="$(field "$p0" "$1")" -v b="$(field "$p1" "$1")" 'BEGIN{ if (a ~ /^[0-9.]+$/ && b ~ /^[0-9.]+$/) printf "%.0f", b-a; else print "" }'; }
# A group is reset when its stats_reset moved or any of its deltas is negative.
reset_of() { # $1 = stats_reset field, then delta values
  local r=false f=$1; shift
  [ "$(field "$p0" "$f")" != "$(field "$p1" "$f")" ] && r=true
  for v in "$@"; do case "$v" in -*) r=true;; esac; done
  echo $r
}
wal_bytes=$(dlt 1); wal_fpi=$(dlt 2); wal_rec=$(dlt 3)
wal_reset=$(reset_of 4 "$wal_bytes" "$wal_fpi" "$wal_rec")
commits=$(dlt 5); db_reset=$(reset_of 6 "$commits")
archived=$(dlt 7); arch_failed=$(dlt 8); arch_reset=$(reset_of 9 "$archived" "$arch_failed")
if [ "$wal_reset" = true ]; then wal_bytes=''; wal_fpi=''; wal_rec=''; fi
if [ "$db_reset" = true ]; then commits=''; fi
if [ "$arch_reset" = true ]; then archived=''; arch_failed=''; fi
cps=$(awk -v c="$commits" -v t="$el" 'BEGIN{ if (c != "") printf "%.1f", c/t }')
db_delta=$(dlt 10)
ready=$(q "select count(*) from pg_ls_archive_statusdir() where name like '%.ready'")
backends=$(q "select count(*) from pg_stat_activity where backend_type='client backend'")
# Container CPU cores over the window, from the two edge snapshots.
cores_of() { # $1 = index
  awk -v a="$(echo "$c0" | awk -v i=$(( $1 + 1 )) '{print $i}')" -v b="$(echo "$c1" | awk -v i=$(( $1 + 1 )) '{print $i}')" -v t="$el" \
    'BEGIN{ if (a ~ /^[0-9]+$/ && b ~ /^[0-9]+$/) printf "%.4f", (b-a)/1e6/t; else print "" }'
}
STMT_SQL="select round((total_exec_time+total_plan_time)::numeric) as total_ms, calls, round(((total_exec_time+total_plan_time)/calls)::numeric,2) as mean_ms, rows, round(wal_bytes/1024.0) as wal_kb, left(regexp_replace(query, E'\\\\s+', ' ', 'g'), 160) as query from pg_stat_statements where calls > 0 and query not like '%pg_stat%' order by total_exec_time+total_plan_time desc limit $TOPN"

if [ "$JSON" != 1 ]; then
  echo "host_busy_cores=$busy of $NCPU (window ${el}s)"
  for i in "${!NAMES[@]}"; do c=$(cores_of "$i"); echo "cpu ${NAMES[$i]} avg_core_pct=$(awk -v c="$c" 'BEGIN{ if (c == "") print "-"; else printf "%.2f", c*100 }')"; done
  sudo -n docker stats --no-stream --format 'mem {{.Name}} {{.MemUsage}} pids={{.PIDs}}' $CS
  echo "wal before: $(field "$p0" 1) | $(field "$p0" 2) | $(field "$p0" 3)"
  echo "wal after:  $(field "$p1" 1) | $(field "$p1" 2) | $(field "$p1" 3)"
  if [ "$wal_reset" = true ]; then echo "wal: statistics were reset inside the window, no delta"
  else awk -v b="$wal_bytes" -v f="$wal_fpi" -v r="$wal_rec" -v t="$el" 'BEGIN{printf "wal_mb=%.1f  wal_mb_per_day=%.0f  fpi=%d  records=%d\n", b/1048576, b/1048576*86400/t, f, r}'; fi
  echo "archiver archived=${archived:-reset} failed=${arch_failed:-reset}; .ready files: $ready"
  echo "db_bytes before=$(field "$p0" 10) after=$(field "$p1" 10)"
  echo "commits_per_s=${cps:-reset}"
  echo "backends: $backends"
  echo "-- top statements by time (total_ms incl. planning | calls | mean_ms | rows | wal_kb | text)"
  q "select round((total_exec_time+total_plan_time)::numeric) as ms, calls, round(((total_exec_time+total_plan_time)/calls)::numeric,2), rows, round(wal_bytes/1024.0), left(regexp_replace(query, E'\\\\s+', ' ', 'g'), 120) from pg_stat_statements where calls > 0 and query not like '%pg_stat%' order by total_exec_time+total_plan_time desc limit $TOPN"
  echo "-- statement time total: $(q "select round(sum(total_exec_time+total_plan_time)::numeric) || ' ms = ' || round((sum(total_exec_time+total_plan_time)/10/$el)::numeric,2) || ' % of one core; calls=' || sum(calls) from pg_stat_statements where query not like '%pg_stat%'")"
  echo "## end $LABEL"
  exit 0
fi

# JSON mode. Memory is docker's own figure ("12.3MiB / 4GiB"), turned into bytes.
tobytes() { echo "$1" | awk '{ v=$1; u=v; sub(/^[0-9.]+/, "", u); sub(/[A-Za-z]+$/, "", v);
  m["B"]=1; m["kB"]=1e3; m["KB"]=1e3; m["MB"]=1e6; m["GB"]=1e9; m["TB"]=1e12;
  m["KiB"]=1024; m["MiB"]=1048576; m["GiB"]=1073741824; m["TiB"]=1099511627776;
  if (v ~ /^[0-9.]+$/ && (u in m)) printf "%.0f", v*m[u]; }'; }
MEM=$(sudo -n docker stats --no-stream --format '{{.Name}}|{{.MemUsage}}|{{.PIDs}}' $CS 2>/dev/null)
{
  printf '{"label":%s,"window_seconds":%s,"started_at":"%s","ended_at":"%s",' "$(jstr "$LABEL")" "$el" "$start_iso" "$end_iso"
  printf '"host":{"hostname":%s,"cpus":%s,"busy_cores":%s,"loadavg":%s},' "$(jstr "$(hostname)")" "$(num "$NCPU")" "$busy" "$(jstr "$(cat /proc/loadavg 2>/dev/null)")"
  printf '"containers":{'
  sep=''
  for i in "${!NAMES[@]}"; do
    c=${NAMES[$i]}
    line=$(echo "$MEM" | awk -F'|' -v n="$c" '$1==n')
    usage=$(echo "$line" | awk -F'|' '{split($2, p, " / "); print p[1]}')
    limit=$(echo "$line" | awk -F'|' '{split($2, p, " / "); print p[2]}')
    pids=$(echo "$line" | awk -F'|' '{print $3}')
    printf '%s%s:{"cpu_cores":%s,"mem_bytes":%s,"mem_limit_bytes":%s,"pids":%s}' "$sep" "$(jstr "$c")" "$(num "$(cores_of "$i")")" "$(num "$(tobytes "$usage")")" "$(num "$(tobytes "$limit")")" "$(num "$pids")"
    sep=','
  done
  printf '},'
  mbday=$(awk -v b="$wal_bytes" -v t="$el" 'BEGIN{ if (b != "") printf "%.1f", b/1048576*86400/t }')
  printf '"wal":{"bytes":%s,"fpi":%s,"records":%s,"mb_per_day":%s,"reset":%s,"dir_bytes_before":%s,"dir_bytes_after":%s},' \
    "$(num "$wal_bytes")" "$(num "$wal_fpi")" "$(num "$wal_rec")" "$(num "$mbday")" "$wal_reset" "$(num "$(field "$p0" 11)")" "$(num "$(field "$p1" 11)")"
  printf '"archiver":{"archived":%s,"failed":%s,"reset":%s,"ready_files_after":%s,"before":%s,"after":%s},' \
    "$(num "$archived")" "$(num "$arch_failed")" "$arch_reset" "$(num "$ready")" "$(jobj "$ar0")" "$(jobj "$ar1")"
  printf '"database":{"bytes_before":%s,"bytes_after":%s,"bytes_delta":%s},' "$(num "$(field "$p0" 10)")" "$(num "$(field "$p1" 10)")" "$(num "$db_delta")"
  printf '"commits_per_s":%s,"commits_reset":%s,"backends":%s,' "$(num "$cps")" "$db_reset" "$(num "$backends")"
  # jsonb: json_agg puts a newline between elements, and qj keeps only the first line.
  top=$(qj "select coalesce(jsonb_agg(t), '[]') from ($STMT_SQL) t")
  tot=$(qj "select json_build_object('total_ms', round(sum(total_exec_time+total_plan_time)::numeric), 'calls', sum(calls)) from pg_stat_statements where query not like '%pg_stat%'")
  printf '"statements":{"summary":%s,"top":%s},' "$(jobj "$tot")" "$(jobj "$top")"
  printf '"command":{"given":%s,"exit":%s}}\n' "$([ $# -gt 0 ] && echo true || echo false)" "$cmd_exit"
}
