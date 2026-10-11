#!/bin/sh
set -eu
set +x
export PGBACKREST_CONFIG=/run/komika/pgbackrest.conf
# The fence beside this script decides first: in a rehearsal only the kit's own
# command lines pass, and only against the rehearsal prefix.
python3 "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/safety.py" --container "$@"
exec pgbackrest --stanza=komika "$@"
