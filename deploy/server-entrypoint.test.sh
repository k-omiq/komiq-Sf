#!/bin/sh
# No server, database or network needed.
set -eu
entrypoint="${ENTRYPOINT_UNDER_TEST:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/server-entrypoint.sh}"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir "$scratch/bin"
cat > "$scratch/bin/komika-server" <<'SH'
#!/bin/sh
printf 'server' > "$ENTRYPOINT_TRACE"
printf ' <%s>' "$@" >> "$ENTRYPOINT_TRACE"
exit "${SERVER_STATUS:-0}"
SH
chmod +x "$scratch/bin/komika-server"
export PATH="$scratch/bin:$PATH" ENTRYPOINT_TRACE="$scratch/trace"
sh "$entrypoint" setup --setup-code 'code with spaces'
test "$(cat "$ENTRYPOINT_TRACE")" = 'server <setup> <--setup-code> <code with spaces>'
export SERVER_STATUS=7
if sh "$entrypoint"; then exit 1; else test "$?" -eq 7; fi
echo 'server-entrypoint: exec_arguments and exit_status passed'
