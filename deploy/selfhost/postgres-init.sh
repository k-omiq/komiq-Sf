#!/bin/sh
# Stock postgres entrypoint runs this only for a new, empty cluster.
set -eu
case "$KOMIQ_DB_PASSWORD" in
  ''|*[!A-Za-z0-9_.-]*) echo 'KOMIQ_DB_PASSWORD must be nonempty and URL-safe (letters, digits, _, . or -).' >&2; exit 1 ;;
esac
# Disable statement logging for this session so the password cannot appear in logs.
PGOPTIONS='-c log_statement=none -c log_min_duration_statement=-1 -c log_min_error_statement=panic' \
psql -v ON_ERROR_STOP=1 --username postgres --dbname postgres <<'SQL'
\getenv app_password KOMIQ_DB_PASSWORD
CREATE ROLE komiq LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD :'app_password';
CREATE DATABASE komiq OWNER komiq TEMPLATE template0 ENCODING 'UTF8'
  LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C';
REVOKE ALL ON DATABASE komiq FROM PUBLIC;
SQL
# The server never gets a remotely usable superuser. No trust or md5 entries remain.
cat > "$PGDATA/pg_hba.conf" <<'HBA'
local all postgres peer
local all all scram-sha-256
host all postgres all reject
host komiq komiq all scram-sha-256
host all all all reject
HBA
