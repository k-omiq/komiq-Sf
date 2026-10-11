#!/bin/sh
# Host: sh deploy/image-smoke.test.sh [server-image] [selfhost-image].
# Inner mode: IMAGE_KIND=server|selfhost, DATABASE_URL supplied by the host runner.
# Only a disposable internal Docker network, no published ports or application data.
set -eu
if [ -z "${IMAGE_KIND:-}" ]; then
  tests=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
  . "$tests/selfhost/test-resources.sh"
  smoke_root=$(mktemp -d)
  resource_init "$smoke_root/resources"
  token=komiq-smoke-$run_id
  postgres_image=postgres:18.6-bookworm@sha256:afc7e2d441324c0388fa80c3d24f733b4194a4eb7f47dd8ee2b08eb1a24a647c
  smoke_password=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
  smoke_env=$(mktemp)
  chmod 600 "$smoke_env"
  cleanup_host() {
    resource_cleanup
    rm -rf "$smoke_root"
    rm -f "$smoke_env"
  }
  trap cleanup_host EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  trap 'exit 129' HUP
  printf 'POSTGRES_PASSWORD=%s\nKOMIQ_DB_PASSWORD=%s\n' "$smoke_password" "$smoke_password" > "$smoke_env"
  resource_refuse_name network "$token"
  for service in postgres server selfhost; do
    resource_refuse_name container "$token-$service"
  done
  resource_network "$token"
  resource_container --name "$token-postgres" --network "$token" --network-alias postgres \
    --env-file "$smoke_env" \
    -e POSTGRES_INITDB_ARGS='--encoding=UTF8 --locale-provider=builtin --builtin-locale=C.UTF-8 --lc-collate=C --lc-ctype=C --auth-host=scram-sha-256' \
    --mount "type=bind,src=$tests/selfhost/postgres-init.sh,dst=/docker-entrypoint-initdb.d/10-komiq.sh,readonly" \
    --tmpfs /var/lib/postgresql:rw "$postgres_image"
  docker start "$resource_id" >/dev/null
  ready=0
  for attempt in $(seq 1 120); do
    if docker exec "$token-postgres" sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" psql -h 127.0.0.1 -U komiq -d komiq -Atqc "SELECT 1"' 2>/dev/null | grep -qx 1; then ready=1; break; fi
    sleep 1
  done
  [ "$ready" = 1 ] || { docker logs "$token-postgres"; exit 1; }
  # Secrets go through a private env file, never through Docker command arguments.
  printf 'DATABASE_URL=postgres://komiq:%s@postgres:5432/komiq\n' "$smoke_password" > "$smoke_env"
  server_image=${1:-komiq-postgres-server:review}
  selfhost_image=${2:-komiq-postgres-selfhost:review}
  resource_container --name "$token-server" --network "$token" --tmpfs /data:rw,uid=10001,gid=10001 --entrypoint /bin/sh \
    --env-file "$smoke_env" --mount "type=bind,src=$tests,dst=/tests,readonly" \
    -e IMAGE_KIND=server "$server_image" /tests/image-smoke.test.sh
  docker start -a "$resource_id"
  [ "$(docker inspect --format '{{.State.ExitCode}}' "$resource_id")" = 0 ]
  resource_container --name "$token-selfhost" --network "$token" --tmpfs /data:rw,uid=10001,gid=10001 --entrypoint /bin/sh \
    --env-file "$smoke_env" --mount "type=bind,src=$tests,dst=/tests,readonly" \
    -e IMAGE_KIND=selfhost "$selfhost_image" /tests/image-smoke.test.sh
  docker start -a "$resource_id"
  [ "$(docker inspect --format '{{.State.ExitCode}}' "$resource_id")" = 0 ]
  exit 0
fi
test "$(id -u)" = 10001
: "${DATABASE_URL:?Run deploy/image-smoke.test.sh on the Docker host to provision PostgreSQL}"
scratch=$(mktemp -d)
server_pid=
cleanup() {
  if [ -n "$server_pid" ]; then
    kill -KILL "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM

case "$IMAGE_KIND" in
  server)
    cmp /usr/local/bin/komika-entrypoint.sh /tests/server-entrypoint.sh
    ENTRYPOINT_UNDER_TEST=/usr/local/bin/komika-entrypoint.sh sh /tests/server-entrypoint.test.sh
    start_server() { exec /usr/local/bin/komika-entrypoint.sh; }
    ;;
  selfhost)
    # This image directly executes the server; it does not include Litestream.
    # Exercise the shell test in its runtime too, then test its real entrypoint.
    sh /tests/server-entrypoint.test.sh
    test -f /srv/web/reader/index.html
    test -f /srv/web/admin/index.html
    cloudflared --version
    start_server() { exec /usr/local/bin/komika-server; }
    ;;
  *) exit 2 ;;
esac

export DATABASE_URL
export COVERS_DATABASE_URL="sqlite://$scratch/covers.sqlite3"
export CATALOGUE_SYNC=off SUWAYOMI_URL=http://127.0.0.1:9
export BIND_ADDR=127.0.0.1 PORT=18087 RUST_LOG=komika_server=info
unset KOMIKA_SQLITE_IMPORT_PATH
start_server > "$scratch/first.log" 2>&1 &
server_pid=$!
ready=0
for attempt in $(seq 1 100); do
  if curl --max-time 1 --connect-timeout 1 -fsS http://127.0.0.1:18087/health/ready > /dev/null 2>&1; then
    ready=1
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    cat "$scratch/first.log"
    echo 'FAIL: first server exited before readiness' >&2
    exit 1
  fi
  sleep 0.1
done
if [ "$ready" != 1 ]; then
  cat "$scratch/first.log"
  echo 'FAIL: server did not become ready' >&2
  exit 1
fi
test -f "$scratch/.komiq-server.lock"
curl --max-time 1 -fsS http://127.0.0.1:18087/health > /dev/null

# Different port avoids confusing a bind conflict with a data-directory lock.
second_status=0
PORT=18088 timeout --kill-after=2s 10s /usr/local/bin/komika-server > "$scratch/second.log" 2>&1 || second_status=$?
if [ "$second_status" = 0 ]; then
  echo 'FAIL: second server acquired the live data directory' >&2
  exit 1
fi
if [ "$second_status" != 1 ]; then
  cat "$scratch/second.log"
  echo "FAIL: expected a normal lock rejection, got exit $second_status" >&2
  exit 1
fi
grep -q 'another server may be using this data directory' "$scratch/second.log"
curl --max-time 1 -fsS http://127.0.0.1:18087/health/ready > /dev/null

stop_server() {
  kill -TERM "$server_pid"
  if ! timeout --kill-after=2s 25s tail --pid="$server_pid" -f /dev/null; then
    echo 'FAIL: server did not stop within its shutdown deadline' >&2
    exit 1
  fi
  wait "$server_pid"
  server_pid=
}

stop_server

# The same entrypoint can immediately reuse the directory after clean shutdown.
start_server > "$scratch/restart.log" 2>&1 &
server_pid=$!
ready=0
for attempt in $(seq 1 100); do
  if curl --max-time 1 --connect-timeout 1 -fsS http://127.0.0.1:18087/health/ready > /dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.1
done
if [ "$ready" != 1 ]; then
  cat "$scratch/restart.log"
  echo 'FAIL: data-directory lock did not release on shutdown' >&2
  exit 1
fi
stop_server
echo "image-smoke: $IMAGE_KIND entrypoint, readiness, lock exclusion and restart passed"
