#!/bin/sh
# Hosted recovery only: needs --hosted-recovery and the hosted Compose selection.
# It refuses every rehearsal selection; a rehearsal drill is `rehearsal.sh drill`.
set -eu
set +x
exec python3 "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/drill.py" "$@"
