#!/bin/sh
# Exercise the entrypoint without a server, replica credentials, or network access.
set -eu

entrypoint="${ENTRYPOINT_UNDER_TEST:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/server-entrypoint.sh}"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir "$scratch/bin"
cat > "$scratch/bin/litestream" <<'SH'
#!/bin/sh
echo "$*" >> "$ENTRYPOINT_TRACE"
case "$1" in
  restore) exit "$RESTORE_STATUS" ;;
  replicate) exit 0 ;;
  *) exit 99 ;;
esac
SH
cat > "$scratch/bin/komika-server" <<'SH'
#!/bin/sh
echo server >> "$ENTRYPOINT_TRACE"
SH
chmod +x "$scratch/bin/"*
export PATH="$scratch/bin:$PATH" ENTRYPOINT_TRACE="$scratch/trace"
export LITESTREAM_BUCKET=test LITESTREAM_ACCESS_KEY_ID=test LITESTREAM_SECRET_ACCESS_KEY=test

# Both Litestream 0.3.13 skip flags succeed when there is nothing to restore.
export RESTORE_STATUS=0
sh "$entrypoint" > "$scratch/output" 2>&1
grep -q '^restore -if-db-not-exists -if-replica-exists ' "$ENTRYPOINT_TRACE"
grep -q '^replicate ' "$ENTRYPOINT_TRACE"

# An actual restore failure must exit before replication or database creation.
: > "$ENTRYPOINT_TRACE"
export RESTORE_STATUS=7
if sh "$entrypoint" > "$scratch/output" 2>&1; then
  echo 'FAIL: a failed restore allowed the entrypoint to succeed' >&2
  exit 1
else
  status=$?
  test "$status" -eq 7
fi
if grep -Eq '^(replicate |server$)' "$ENTRYPOINT_TRACE"; then
  echo 'FAIL: started replication or the server after a failed restore' >&2
  exit 1
fi

# Optional backups remain optional for a self-hosted installation.
: > "$ENTRYPOINT_TRACE"
unset LITESTREAM_BUCKET LITESTREAM_ACCESS_KEY_ID LITESTREAM_SECRET_ACCESS_KEY
sh "$entrypoint" > "$scratch/output" 2>&1
test "$(cat "$ENTRYPOINT_TRACE")" = server
echo 'server-entrypoint: all tests passed'
