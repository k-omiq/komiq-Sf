#!/usr/bin/env bash
# Host cron job. Custom-format PostgreSQL dumps; retain the newest seven.
set -euo pipefail
# cron gives a short PATH (/usr/bin:/bin on macOS); add the usual Docker CLI places.
PATH="$PATH:/usr/local/bin:/opt/homebrew/bin"
command -v docker >/dev/null 2>&1 || { echo 'Backup needs the docker command on PATH.' >&2; exit 1; }
cd "$(dirname "${BASH_SOURCE[0]}")"
umask 077
backup_dir=${1:-$HOME/komiq-backups}
mkdir -p "$backup_dir"
chmod 700 "$backup_dir"
# Keep the inode: removing a lock file would let another process lock a new one.
# FD 9 belongs to this shell; child commands close it. SIGKILL releases the lock.
exec 9>"$backup_dir/.backup-lock"
if command -v flock >/dev/null 2>&1; then
    flock -n 9 || { echo 'Backup already running.' >&2; exit 1; }
else
    # macOS ships Perl with core flock. Lock the inherited open description;
    # the parent retains it after Perl exits. No extra module is required.
    command -v perl >/dev/null 2>&1 || { echo 'Backup needs flock or Perl.' >&2; exit 1; }
    perl -e 'open(my $lock, ">&=9") or die "lock descriptor: $!"; flock($lock, 6) or exit 1' || {
        echo 'Backup already running.' >&2; exit 1;
    }
fi
temporary=
cleanup() {
    [ -z "$temporary" ] || rm -f "$temporary" 9>&-
}
trap cleanup EXIT
temporary=$(mktemp "$backup_dir/.dump.XXXXXX" 9>&-)
docker compose exec -T postgres sh -c \
    'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec pg_dump -h 127.0.0.1 -U komiq -d komiq -Fc' 9>&- > "$temporary"
[ -s "$temporary" ] || { echo 'Backup produced an empty dump.' >&2; exit 1; }
mv "$temporary" "$backup_dir/komiq-$(date -u +%Y%m%dT%H%M%SZ 9>&-).dump" 9>&-
temporary=
shopt -s nullglob
dumps=("$backup_dir"/komiq-*.dump)
for ((i=0; i<${#dumps[@]}-7; i++)); do rm -f "${dumps[i]}" 9>&-; done
