#!/bin/sh
# PostgreSQL startup/restore is owned by the database container.
set -eu
exec komika-server "$@"
