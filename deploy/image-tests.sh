#!/bin/sh
# Build the three images first. Smoke tests use a disposable internal network.
set -eu
tests=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
server_image=${1:-komiq-postgres-server:review}
selfhost_image=${2:-komiq-postgres-selfhost:review}
postgres_image=${3:-komika-postgres:18.6-pgbackrest-2.59}
sh "$tests/server-entrypoint.test.sh"
sh "$tests/watch.test.sh"
sh "$tests/postgres/tests.sh"
# The shared smoke runner provisions its own private PostgreSQL database.
sh "$tests/image-smoke.test.sh" "$server_image" "$selfhost_image"
docker run --rm --network none --entrypoint /bin/sh \
  --mount "type=bind,src=$tests/postgres,dst=/tests,readonly" \
  "$postgres_image" /tests/image-test.sh
