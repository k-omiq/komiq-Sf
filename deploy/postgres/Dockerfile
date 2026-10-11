FROM postgres:18.6-bookworm@sha256:afc7e2d441324c0388fa80c3d24f733b4194a4eb7f47dd8ee2b08eb1a24a647c
# PGDG is already configured by the base. One exact pgBackRest package: a build
# fails rather than selecting another version. PGDG offers only its newest
# version, so this line stops building when PGDG moves on; keep a `docker save`
# tarball of the reviewed image off the host (RESTORE.md) and change the version
# here only together with a new restore drill.
RUN set -eu; \
    printf 'Package: pgbackrest\nPin: origin apt.postgresql.org\nPin-Priority: 1001\n' > /etc/apt/preferences.d/pgbackrest; \
    apt-get update; \
    apt-get install -y --no-install-recommends 'pgbackrest=2.59.3-1.pgdg12+1' ca-certificates python3; \
    postgres --version | grep -E 'PostgreSQL\) 18\.6([[:space:]]|$)'; \
    pgbackrest version | grep -E '^pgBackRest 2\.59\.3$'; \
    test -s /etc/ssl/certs/ca-certificates.crt; \
    rm -rf /var/lib/apt/lists/*
# Python's standard library handles JSON, UTC slots, file locks and atomic status.
COPY entrypoint.sh pgbackrest.sh health.sh backup.sh hosted.py safety.py postgresql.conf /opt/komika-postgres/
RUN chmod +x /opt/komika-postgres/*.sh
ENV PGDATA=/var/lib/postgresql/18/docker \
    PGBACKREST_CONFIG=/run/komika/pgbackrest.conf
ENTRYPOINT ["/opt/komika-postgres/entrypoint.sh"]
CMD ["postgres"]
