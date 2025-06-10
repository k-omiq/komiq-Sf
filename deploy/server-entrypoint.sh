#!/bin/sh
# Server entrypoint. If Litestream is configured (LITESTREAM_BUCKET + both
# LITESTREAM_ACCESS_KEY_ID and LITESTREAM_SECRET_ACCESS_KEY),
# restore the DB from the replica on a fresh volume, then run the server under
# Litestream so the WAL is continuously backed up. Otherwise run the server plain.
set -eu

DB="/data/komika.sqlite3"

if [ -n "${LITESTREAM_BUCKET:-}" ] && [ -n "${LITESTREAM_ACCESS_KEY_ID:-}" ] && [ -n "${LITESTREAM_SECRET_ACCESS_KEY:-}" ]; then
  echo "[entrypoint] Litestream backup ENABLED → bucket=${LITESTREAM_BUCKET} endpoint=${LITESTREAM_ENDPOINT:-default}"
  # Restore the latest snapshot only if this volume has no DB yet AND a replica
  # exists (first boot on a fresh host / after a wipe). No-op on an existing DB.
  litestream restore -if-db-not-exists -if-replica-exists -config /etc/litestream.yml "$DB" \
    || echo "[entrypoint] nothing to restore (fresh replica or first ever run)"
  # Replicate continuously while running the server as a supervised child; on
  # SIGTERM Litestream flushes a final snapshot before exiting.
  exec litestream replicate -config /etc/litestream.yml -exec "komika-server"
else
  # Warn loudly on a partial config, so an operator who set some but not all of
  # the required vars isn't silently left without backups.
  if [ -n "${LITESTREAM_BUCKET:-}${LITESTREAM_ACCESS_KEY_ID:-}${LITESTREAM_SECRET_ACCESS_KEY:-}" ]; then
    echo "[entrypoint] WARNING: Litestream is PARTIALLY configured — backup is DISABLED."
    echo "[entrypoint] Need all of LITESTREAM_BUCKET, LITESTREAM_ACCESS_KEY_ID, LITESTREAM_SECRET_ACCESS_KEY."
  else
    echo "[entrypoint] Litestream NOT configured — running WITHOUT backup."
    echo "[entrypoint] Set LITESTREAM_* in deploy/.env to enable continuous backup + restore."
  fi
  exec komika-server
fi
