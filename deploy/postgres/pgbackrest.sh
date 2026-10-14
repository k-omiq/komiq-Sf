#!/bin/sh
set -eu
set +x
export PGBACKREST_CONFIG=/run/komika/pgbackrest.conf
# The fence beside this script decides first: in a rehearsal only the kit's own
# command lines pass, and only against the rehearsal prefix.
python3 "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/safety.py" --container "$@"
if [ "${1:-}" != archive-push ]; then
    exec pgbackrest --stanza=komika "$@"
fi
# archive_command: pgBackRest's stdout, stderr (byte for byte, as it streams)
# and exit status are PostgreSQL's unchanged. A full queue makes pgBackRest
# acknowledge and DROP WAL; each such warning also appends one line (UTC time
# with microseconds, segment) for `hosted.py status` and health. Recording never
# logs or fails anything itself. PostgreSQL's archiver runs archive_command one
# at a time, so the record needs no lock. The detached async process closes 0-2
# itself; it never holds fd 3 (stdout) or 4 (the status pipe).
exec 3>&1
set +e
status=$({ { pgbackrest --stanza=komika "$@" 2>&1 >&3 3>&- 4>&-; echo $? >&4; } |
    python3 -I -S -c '
import datetime, os, re, tempfile
state = os.environ.get("KOMIKA_BACKUP_STATE", "/var/lib/postgresql/komika-backup")
record, cap, out, line = os.path.join(state, "dropped-wal.log"), 10000, True, b""

def note(text):
    if b"dropped WAL file" not in text or not os.path.isdir(state):
        return
    try:
        found = re.search(rb"[0-9A-F]{24}", text)
        segment = found.group().decode() if found else "-"
        now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
        with open(record, "a") as f:
            f.write(now + " " + segment + "\n")
        with open(record) as f:
            lines = f.readlines()
        if len(lines) > cap:
            fd, name = tempfile.mkstemp(prefix=".dropped-", dir=state)
            try:
                with os.fdopen(fd, "w") as f:
                    f.writelines(lines[-cap:])
                os.replace(name, record)  # Readers see the old or the new file whole.
            finally:
                if os.path.exists(name):
                    os.unlink(name)
    except Exception:
        pass

while True:
    chunk = os.read(0, 65536)
    if not chunk:
        break
    rest = chunk
    while out and rest:
        try:
            rest = rest[os.write(2, rest):]
        except OSError:
            out = False  # Keep reading: pgBackRest must never block on stderr.
    *done, line = (line + chunk).split(b"\n")
    for text in done:
        note(text)
note(line)
' 3>&- 4>&- >&2; } 4>&1)
set -e
exec 3>&-
exit "${status:-1}"
