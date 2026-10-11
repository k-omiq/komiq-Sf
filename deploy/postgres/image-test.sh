#!/bin/sh
# Offline image assertions. No PostgreSQL startup or R2 traffic.
# The test files are mounted at /tests; the code under test is the image's own
# copy in /opt/komika-postgres (first on the module path), never a mounted one.
set -eu
postgres --version | grep -E 'PostgreSQL\) 18\.6([[:space:]]|$)'
pgbackrest version | grep -E '^pgBackRest 2\.59\.'
test -s /etc/ssl/certs/ca-certificates.crt
grep -Eq '^trusted[[:space:]]*=[[:space:]]*true' /usr/share/postgresql/18/extension/pg_trgm.control
for binary in gosu python3 pgbackrest postgres initdb pg_ctl psql sha256sum; do command -v "$binary"; done
cd /opt/komika-postgres
KOMIKA_IMAGE_TEST=1 PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=/tests python3 -m unittest -v test_hosted test_health test_image
