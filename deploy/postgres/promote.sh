#!/bin/sh
# Manual promotion on the STANDBY host (STANDBY.md). Run from the deploy
# directory with the same Compose options the stack uses, for example:
#   postgres/promote.sh --old-primary-stopped node-a -f docker-compose.yml -f docker-compose.standby.yml
# --old-primary-stopped OLD_NODE is your word that OLD_NODE, the primary that
# fence.json names now, cannot take writes: its host is powered off, or its
# PostgreSQL is confirmed stopped. An unreachable port is not proof. Nothing
# here promotes automatically.
set -eu
if [ "${1:-}" != --old-primary-stopped ] || [ -z "${2:-}" ]; then
    echo "usage: postgres/promote.sh --old-primary-stopped OLD_NODE [docker compose options]" >&2
    exit 2
fi
old=$2
shift 2
docker compose "$@" exec -T --user postgres postgres \
    python3 /opt/komika-postgres/hosted.py promote --old-primary-stopped "$old"
docker compose "$@" up -d
