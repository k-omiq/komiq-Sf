#!/bin/sh
# Build both Dockerfiles first. Uses disposable container-local data and no network.
set -eu
tests=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
server_image=${1:-komiq-sqlite-server:review}
selfhost_image=${2:-komiq-sqlite-selfhost:review}
docker run --rm --network none --entrypoint /bin/sh \
  --mount "type=bind,src=$tests,dst=/tests,readonly" \
  -e IMAGE_KIND=server "$server_image" /tests/image-smoke.test.sh
docker run --rm --network none --entrypoint /bin/sh \
  --mount "type=bind,src=$tests,dst=/tests,readonly" \
  -e IMAGE_KIND=selfhost "$selfhost_image" /tests/image-smoke.test.sh
