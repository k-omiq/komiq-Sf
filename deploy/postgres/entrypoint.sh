#!/bin/sh
set -eu
set +x
if [ "$(id -u)" = 0 ]; then
    mkdir -p /run/komika /var/run/postgresql /var/lib/postgresql/komika-backup
    chown postgres:postgres /run/komika /var/run/postgresql /var/lib/postgresql/komika-backup
    chmod 700 /run/komika /var/lib/postgresql/komika-backup
    # The shared data volume's root is needed for initdb/restore, never chown recursively.
    chown postgres:postgres /var/lib/postgresql
    # Standby TLS files (STANDBY.md), mounted read-only with any owner: private
    # postgres-owned copies, because PostgreSQL and libpq refuse a key that
    # others can read.
    if [ -d /etc/komika-postgres-tls ]; then
        mkdir -p /run/komika/tls
        for name in server.crt server.key ca.crt client.crt client.key; do
            if [ -f "/etc/komika-postgres-tls/$name" ]; then
                cp "/etc/komika-postgres-tls/$name" "/run/komika/tls/$name"
            fi
        done
        chown -R postgres:postgres /run/komika/tls
        chmod 700 /run/komika/tls
        find /run/komika/tls -type f -exec chmod 600 {} +
    fi
    exec gosu postgres "$0" "$@"
fi
exec python3 /opt/komika-postgres/hosted.py "$@"
