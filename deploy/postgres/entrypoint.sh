#!/bin/sh
set -eu
set +x
if [ "$(id -u)" = 0 ]; then
    mkdir -p /run/komika /var/run/postgresql /var/lib/postgresql/komika-backup
    chown postgres:postgres /run/komika /var/run/postgresql /var/lib/postgresql/komika-backup
    chmod 700 /run/komika /var/lib/postgresql/komika-backup
    # The shared data volume's root is needed for initdb/restore, never chown recursively.
    chown postgres:postgres /var/lib/postgresql
    exec gosu postgres "$0" "$@"
fi
exec python3 /opt/komika-postgres/hosted.py "$@"
