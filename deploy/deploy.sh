#!/usr/bin/env bash
#
# Komika — one-command deploy.
#
# Stands up the FULL public stack (Suwayomi + FlareSolverr + Komika server +
# reader), then bootstraps it into a working "hosted Mihon/Tachiyomi with a public
# social layer and an auto-updating catalog": installs the MangaDex source, turns
# on the Cloudflare bypass, creates the admin, and seeds the library.
#
#   ./deploy.sh                    build + start + bootstrap  (default)
#   ./deploy.sh up                 same as default
#   ./deploy.sh up --with-reader   also run the bundled nginx reader container
#   ./deploy.sh bootstrap          (re)run the bootstrap only (stack already up)
#   ./deploy.sh down               stop the stack (keeps data volumes)
#   ./deploy.sh destroy            stop and DELETE the data volumes
#   ./deploy.sh logs               tail all service logs
#
# The `reader` service sits behind the compose profile `selfhost` and is OFF by
# default: the production reader is a Cloudflare Worker (CLOUDFLARE-RUNBOOK.md),
# so starting the container would publish an unused world-bound host port. Pass
# --with-reader (or export COMPOSE_PROFILES=selfhost) to run the nginx SPA.
#
set -euo pipefail
set +x
cd "$(dirname "$0")"

cy='\033[36m'; gn='\033[32m'; yl='\033[33m'; rd='\033[31m'; bd='\033[1m'; z='\033[0m'
say(){ printf "${cy}▸ %s${z}\n" "$*"; }
die(){ printf "${rd}✗ %s${z}\n" "$*" >&2; exit 1; }

refuse_rehearsal(){
  [ -z "$(compgen -A variable KOMIKA_REHEARSAL_)" ] || die "deploy.sh refuses rehearsal variables; use postgres/rehearsal.sh."
  case "${KOMIKA_COMPOSE_FILE:-}:${COMPOSE_FILE:-}:${KOMIKA_COMPOSE_PROJECT:-}:${COMPOSE_PROJECT_NAME:-}" in
    *rehearsal*) die "deploy.sh refuses rehearsal Compose selection; use postgres/rehearsal.sh." ;;
  esac
  for selection in KOMIKA_COMPOSE_FILE COMPOSE_FILE; do
    if [ -n "${!selection:-}" ]; then
      python3 -c 'from pathlib import Path; import sys; sys.exit(Path(sys.argv[1]).resolve() != Path(sys.argv[2]).resolve())' \
        "${!selection}" "$PWD/docker-compose.yml" || die "deploy.sh supports only the hosted Compose file; use postgres/rehearsal.sh for rehearsals."
    fi
  done
}
refuse_rehearsal

# ---- prerequisites ------------------------------------------------------------
command -v docker >/dev/null 2>&1 || die "docker is not installed — install Docker first."
if docker compose version >/dev/null 2>&1; then DC="docker compose";
elif command -v docker-compose >/dev/null 2>&1; then DC="docker-compose";
else die "docker compose is not available."; fi
command -v python3 >/dev/null 2>&1 || die "python3 is required to run the bootstrap."

# ---- config -------------------------------------------------------------------
if [ ! -f .env ]; then
  ( umask 077 && cp .env.example .env )
  printf "${yl}! created deploy/.env from the template.${z}\n"
  die "review deploy/.env before deploying — set a real KOMIKA_ADMIN_PASSWORD (the template ships a public placeholder) and PUBLIC_HOST, then re-run ./deploy.sh."
fi
# .env holds KOMIKA_ADMIN_PASSWORD and the KOMIKA_R2_* and PostgreSQL credentials: enforce
# owner-only permissions (a template-copied file inherits the repo's mode).
[ "$(stat -c '%a' .env)" = "600" ] || { chmod 600 .env; printf "${yl}! tightened deploy/.env permissions to 0600.${z}\n"; }
set -a; . ./.env 2>/dev/null || true; set +a
refuse_rehearsal
PUBLIC_HOST="${PUBLIC_HOST:-localhost}"
READER_PORT="${READER_PORT:-8081}"; SERVER_PORT="${SERVER_PORT:-8080}"; SUWAYOMI_PORT="${SUWAYOMI_PORT:-4567}"

cmd="${1:-up}"

# `reader` is gated behind the `selfhost` compose profile (see header). Opt in
# with `--with-reader` in any argument position, or by pre-setting
# COMPOSE_PROFILES yourself; both work because compose reads COMPOSE_PROFILES
# from the environment. Note `. ./.env` above ran with `set -a`, so a
# COMPOSE_PROFILES line in deploy/.env is honored too.
want_reader=""
for a in "$@"; do [ "$a" = "--with-reader" ] && want_reader=1; done
if [ -n "$want_reader" ]; then
  export COMPOSE_PROFILES="${COMPOSE_PROFILES:+$COMPOSE_PROFILES,}selfhost"
fi
case "${COMPOSE_PROFILES:-}" in *selfhost*) want_reader=1 ;; esac
# Drop the flag so it can't be mistaken for a subcommand.
[ "$cmd" = "--with-reader" ] && cmd="up"

case "$cmd" in
  down)    say "stopping the stack (data volumes kept)"; $DC down; exit 0 ;;
  destroy) say "stopping the stack and DELETING data volumes"; $DC down -v; exit 0 ;;
  logs)    exec $DC logs -f ;;
  bootstrap) python3 bootstrap.py; exit $? ;;
  up|"") : ;;
  *) die "unknown command '$cmd' (use: up | bootstrap | down | destroy | logs)" ;;
esac

# Main URL is interpolated by Compose: keep the password URI-safe (random hex).
[[ "${KOMIKA_POSTGRES_PASSWORD:-}" =~ ^[A-Za-z0-9_.~-]{24,}$ ]] || die "set KOMIKA_POSTGRES_PASSWORD to >=24 URI-safe random characters in deploy/.env."
backup_count=0
for key in KOMIKA_R2_ENDPOINT KOMIKA_R2_BUCKET KOMIKA_R2_PREFIX KOMIKA_R2_KEY KOMIKA_R2_SECRET; do
  [ -z "${!key:-}" ] || backup_count=$((backup_count + 1))
done
[ "$backup_count" -eq 0 ] || [ "$backup_count" -eq 5 ] || die "partial R2 configuration: all five KOMIKA_R2_* variables are required."

# ---- preflight: TRUSTED_PROXY_CIDRS vs. the live bridge gateway ---------------
# The auth rate-limiter honours X-Forwarded-For / CF-Connecting-IP only when the
# socket peer is inside TRUSTED_PROXY_CIDRS. With Docker's userland proxy enabled
# that peer is NOT 127.0.0.1 — docker-proxy re-originates the connection from the
# komika bridge GATEWAY (today 172.21.0.1). Docker allocates that subnet
# dynamically out of its default address pool, so the gateway can move whenever
# the network is recreated: a `./deploy.sh down` + `up`, or a host reboot that
# happens to start the other compose stacks on this box first.
#
# The failure is SILENT. A stale CIDR means X-Forwarded-For is discarded and every
# request keys to the same address — one shared rate-limit bucket, where 10 failed
# logins lock out login and registration for every user for 5 minutes. Nothing in
# the logs says so. So check it here, before anything starts, and fail loudly.
#
# Also asserts the other half of the pair: TRUSTED_PROXY_CIDRS is only safe while
# the API port is loopback-bound. On a 0.0.0.0 publish any direct client could
# forge the header straight past the limiter.
#
# Set SKIP_PROXY_CIDR_CHECK=1 to bypass (e.g. a deliberately exotic proxy setup).
if [ "${SKIP_PROXY_CIDR_CHECK:-}" != "1" ]; then
  # Empty when the network does not exist yet (a first deploy) — nothing to check.
  gw=""
  if docker network inspect komika_komika >/dev/null 2>&1; then
    gw=$(docker network inspect komika_komika -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null || true)
  fi
  preflight=$($DC config --format json 2>/dev/null | GW="$gw" python3 -c '
import json, os, sys

gw = os.environ.get("GW", "")
try:
    svc = json.load(sys.stdin)["services"]["server"]
except Exception:
    sys.exit(0)  # cannot parse -- let compose itself report the problem

cidrs = [c.strip() for c in (svc.get("environment", {}).get("TRUSTED_PROXY_CIDRS") or "").split(",") if c.strip()]
ports = svc.get("ports") or [{}]
host_ip = ports[0].get("host_ip") or "0.0.0.0"

if cidrs and host_ip not in ("127.0.0.1", "::1"):
    print("TRUSTED_PROXY_CIDRS is set (%s) but the API port is published on %s, not loopback.\n"
          "  Any direct-port client could forge X-Forwarded-For and walk past the auth rate\n"
          "  limiter. Blank TRUSTED_PROXY_CIDRS in deploy/.env, or restore the loopback bind\n"
          "  in docker-compose.yml. The two settings must always move together."
          % (",".join(cidrs), host_ip))
    sys.exit(1)

if cidrs and gw:
    # Accept the gateway written as a /32, or inside the /24 or /16 that holds it.
    o = gw.split(".")
    ok = {"%s/32" % gw, "%s.%s.%s.0/24" % (o[0], o[1], o[2]), "%s.%s.0.0/16" % (o[0], o[1])}
    if not (ok & set(cidrs)):
        print("TRUSTED_PROXY_CIDRS does not cover the komika bridge gateway.\n"
              "    gateway now: %s   (docker network inspect komika_komika)\n"
              "    .env says:   %s\n"
              "  Docker re-allocates this subnet when the network is recreated, so this drifts\n"
              "  silently. Left as-is, X-Forwarded-For is discarded and EVERY request shares one\n"
              "  rate-limit bucket: 10 failed logins lock out login + registration for all users.\n"
              "  Fix: set TRUSTED_PROXY_CIDRS=127.0.0.1/32,%s/32 in deploy/.env and re-run."
              % (gw, ",".join(cidrs), gw))
        sys.exit(1)
' ) || die "$preflight"
fi

# ---- build + start ------------------------------------------------------------
say "building images and starting services (this can take a few minutes the first time)…"
$DC up --build -d

say "waiting for services to report healthy…"
# Give the stack up to ~5 min to go healthy (Suwayomi's first boot is slow).
# Track whether we broke out because everything went healthy vs. timed out, so a
# persistently-unhealthy stack fails loudly instead of bootstrapping half-up.
healthy=""
for _ in $(seq 1 100); do
  unhealthy=$($DC ps --format '{{.Service}} {{.Health}}' 2>/dev/null | awk '$2!="" && $2!="healthy"{print $1}' | tr '\n' ' ')
  if [ -z "${unhealthy// }" ]; then healthy=1; break; fi
  sleep 3
done
# Timed out with services still not healthy → don't bootstrap a partial stack.
# postgres-backup health also covers an open WAL gap, the archiver and the 4 GiB
# disk reserve: name its reasons (fixed words, no configuration) before stopping.
if [ -z "$healthy" ] && [[ " $unhealthy" == *" postgres-backup "* ]]; then
  reasons=$($DC exec -T --user postgres postgres-backup python3 /opt/komika-postgres/hosted.py status --local 2>/dev/null |
    python3 -c 'import json, sys; print("\n".join("  - " + r for r in json.load(sys.stdin).get("health_problems") or []))' 2>/dev/null || true)
  printf "${yl}! postgres-backup is unhealthy:${z}\n%s\n" "${reasons:-  - status unreadable (hosted.py status --local)}" >&2
fi
[ -n "$healthy" ] || die "services did not become healthy: ${unhealthy% }. On a new R2 prefix, create the stanza and first full explicitly (postgres/RESTORE.md); inspect postgres-backup status."

# ---- bootstrap ----------------------------------------------------------------
say "bootstrapping (extensions, Cloudflare bypass, admin, library seed)…"
python3 bootstrap.py

# ---- done ---------------------------------------------------------------------
printf "\n${gn}${bd}Komika is up.${z}\n"
if [ -n "$want_reader" ]; then
  printf "  ${bd}Reader${z}      http://%s:%s\n" "$PUBLIC_HOST" "$READER_PORT"
else
  printf "  ${bd}Reader${z}      not started (profile 'selfhost'); prod reader is the Cloudflare Worker.\n"
  printf "              run the bundled nginx SPA with: ./deploy.sh up --with-reader\n"
fi
# The API and Suwayomi ports are published on 127.0.0.1 ONLY (see the port
# comments in docker-compose.yml). These URLs are reachable from the VPS itself;
# from outside, the API is served through the Cloudflare Tunnel at
# https://api.komiq.cc and Suwayomi is not reachable at all, by design.
printf "  ${bd}API${z}         http://127.0.0.1:%s/graphql   (loopback-only; public via the tunnel)\n" "$SERVER_PORT"
printf "  ${bd}Suwayomi${z}    http://127.0.0.1:%s        (loopback-only; source management / images)\n" "$SUWAYOMI_PORT"
printf "  admin login: ${bd}%s${z} / (KOMIKA_ADMIN_PASSWORD in deploy/.env)\n" "${KOMIKA_ADMIN_USERS%%,*}"
printf "\n  Manage:  ./deploy.sh logs   ·   ./deploy.sh down   ·   ./deploy.sh destroy\n"
printf "  The adaptive scanner auto-updates the seeded library — no manual refresh needed.\n"

# ---- backup status ------------------------------------------------------------
if [ -n "${KOMIKA_R2_BUCKET:-}" ] && [ -n "${KOMIKA_R2_ENDPOINT:-}" ] && [ -n "${KOMIKA_R2_PREFIX:-}" ] && [ -n "${KOMIKA_R2_KEY:-}" ] && [ -n "${KOMIKA_R2_SECRET:-}" ]; then
  printf "\n${gn}✓ pgBackRest configured${z} — weekly full, daily incremental, continuous WAL.\n"
  printf "  Check postgres-backup health and pg_stat_archiver; see postgres/RESTORE.md.\n"
else
  printf "\n${rd}${bd}⚠ NO BACKUP CONFIGURED${z} — volume loss is unrecoverable.\n"
  printf "  Set all KOMIKA_R2_* variables in deploy/.env; see postgres/RESTORE.md.\n"
fi
