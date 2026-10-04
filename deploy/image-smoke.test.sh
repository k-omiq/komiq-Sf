#!/bin/sh
# Run by image-tests.sh INSIDE the final runtime images as their default user.
# No network, replica credentials, mounted application data or production access.
set -eu
test "$(id -u)" = 10001
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
    litestream version
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

export DATABASE_URL="sqlite://$scratch/main.sqlite3"
export COVERS_DATABASE_URL="sqlite://$scratch/covers.sqlite3"
export CATALOGUE_SYNC=off SUWAYOMI_URL=http://127.0.0.1:9
export BIND_ADDR=127.0.0.1 PORT=18087 RUST_LOG=komika_server=info
unset LITESTREAM_BUCKET LITESTREAM_ACCESS_KEY_ID LITESTREAM_SECRET_ACCESS_KEY
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
