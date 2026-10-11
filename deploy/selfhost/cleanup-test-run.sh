#!/bin/sh
# Explicit orphan recovery after a SIGKILL of either opt-in Docker test runner.
set -eu
run_id=${1:-}
case "$run_id" in ''|*[!0-9a-f]*) echo 'Usage: sh deploy/selfhost/cleanup-test-run.sh <32-hex-run-id>' >&2; exit 1 ;; esac
[ "${#run_id}" -eq 32 ] || { echo 'Expected exactly 32 hexadecimal characters.' >&2; exit 1; }
tests=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$tests/test-resources.sh"
run_label=cc.komiq.test-run
for kind in container network volume; do
    if [ "$kind" = container ]; then
        ids=$(docker container ls -aq --filter "label=$run_label=$run_id")
    else
        ids=$(docker "$kind" ls -q --filter "label=$run_label=$run_id")
    fi
    for id in $ids; do
        # Recheck immediately before removal, including each volume's owner.
        resource_verify "$kind" "$id" || continue
        if [ "$kind" = container ]; then docker container rm -f "$id"
        else docker "$kind" rm "$id"; fi
    done
done
