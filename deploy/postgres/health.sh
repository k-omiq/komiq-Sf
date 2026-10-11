#!/bin/sh
set -eu
set +x
# TCP forces SCRAM as the application role, rather than local peer auth.
export PGPASSWORD="${KOMIKA_POSTGRES_PASSWORD:?}"
result=$(psql -X -w -h "$HOSTNAME" -U komika -d komika -v ON_ERROR_STOP=1 -Atc \
    "SELECT 1 WHERE NOT pg_is_in_recovery() AND current_user = 'komika' AND NOT (SELECT rolsuper FROM pg_roles WHERE rolname = current_user)")
[ "$result" = 1 ]
