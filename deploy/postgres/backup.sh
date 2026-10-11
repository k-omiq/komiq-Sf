#!/bin/sh
set -eu
exec python3 /opt/komika-postgres/hosted.py job "$@"
