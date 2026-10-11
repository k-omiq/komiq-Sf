#!/bin/sh
# The only rehearsal entry point. Fixed subcommands; nothing is passed through
# to docker, docker compose or pgBackRest. See `rehearsal.sh` without arguments.
set -eu
set +x
# -I: ignore PYTHON* variables and the user site; -B: write no bytecode here.
exec python3 -I -B "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/rehearsal.py" "$@"
