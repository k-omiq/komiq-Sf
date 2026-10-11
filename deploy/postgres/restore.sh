#!/bin/sh
# Hosted recovery only: needs --hosted-recovery; --restore-only needs no running
# source. It refuses every rehearsal selection; use `rehearsal.sh restore` there.
set -eu
set +x
exec python3 "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/drill.py" --restore-only "$@"
