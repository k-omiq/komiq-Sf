#!/usr/bin/env bash
# Daemon-free collision, ownership and assertion regression checks.
set -euo pipefail
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"
cat > "$scratch/bin/od" <<'SH'
#!/bin/sh
# Fix only the random ID, so collision names are deterministic in this stub.
case "$*" in *-N16*) printf '0123456789abcdef0123456789abcdef\n' ;; *) exec /usr/bin/od "$@" ;; esac
SH
cat > "$scratch/bin/docker" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "$RESOURCE_CALLS"
case "$*" in
  'create '*|*' create '*)
    case "${COLLISION:-}" in returned-*) echo foreign-returned ;; owned-create) echo owned-created ;; *) exit 99 ;; esac ;;
  'ps -aq '*) exit 0 ;;
  'container ls -aq --filter label='*) printf '%s\n' owned-container foreign-container ;;
  'network ls -q --filter label='*) echo owned-network ;;
  'volume ls -q --filter label='*) echo owned-volume ;;
  *'inspect --format '*)
    case "$*" in *foreign*) echo another-run ;; *) echo 0123456789abcdef0123456789abcdef ;; esac ;;
  *' inspect '*)
    case "$COLLISION:$*" in
      network:*'network inspect '*) exit 0 ;;
      container:*'container inspect '*) exit 0 ;;
      volume:*'volume inspect '*) exit 0 ;;
      *) exit 1 ;;
    esac ;;
  *' rm '*) exit 0 ;;
  *) echo "Unexpected Docker mutation: $*" >&2; exit 99 ;;
esac
SH
chmod +x "$scratch/bin/od" "$scratch/bin/docker"
export PATH="$scratch/bin:$PATH" RESOURCE_CALLS="$scratch/calls"
for runner in real smoke; do
    for COLLISION in network container volume; do
        [ "$runner:$COLLISION" != smoke:volume ] || continue
        export COLLISION
        : > "$RESOURCE_CALLS"
        if [ "$runner" = real ]; then
            command=(bash "$source_dir/install.real.test.sh")
        else
            command=(sh "$source_dir/../image-smoke.test.sh")
        fi
        if "${command[@]}" > "$scratch/output" 2>&1; then echo "FAIL: $runner accepted $COLLISION collision" >&2; exit 1; fi
        grep -q 'resource already exists' "$scratch/output"
        if grep -E '(^| )(create|up|down|rm|start)( |$)' "$RESOURCE_CALLS"; then
            echo "FAIL: $runner changed resources after collision" >&2; exit 1
        fi
        echo "PASS: $runner $COLLISION name exists: refuse and remove nothing"
    done
done
source "$source_dir/test-resources.sh"
resource_init "$scratch/ledger"
resource_record container owned-container
resource_record container foreign-container
resource_record network owned-network
resource_record volume owned-volume
: > "$RESOURCE_CALLS"
resource_cleanup
grep -qx 'container rm -f owned-container' "$RESOURCE_CALLS"
grep -qx 'network rm owned-network' "$RESOURCE_CALLS"
grep -qx 'volume rm owned-volume' "$RESOURCE_CALLS"
if grep -q '^container rm .*foreign' "$RESOURCE_CALLS"; then echo 'FAIL: removed foreign ownership' >&2; exit 1; fi
echo 'PASS: cleanup removes only recorded IDs with the matching run label'
# Execute the real rename assertion locally against both correct and broken output.
python3 - "$source_dir/install.real.test.sh" "$scratch" <<'PY'
import pathlib, re, subprocess, sys
script = pathlib.Path(sys.argv[1]).read_text()
root = pathlib.Path(sys.argv[2])
(root / 'data').mkdir()
(root / 'fixture').mkdir()
(root / 'fixture/komika.sqlite3').write_bytes(b'original bytes')
(root / 'data/komika.sqlite3.imported-test').write_bytes(b'original bytes')
assertion = re.search(r"-c '(test ! -e /data/komika.sqlite3[^\n]+)'", script)[1]
assertion = assertion.replace('/data/', str(root / 'data') + '/').replace('/fixture/', str(root / 'fixture') + '/')
assert subprocess.run(['sh', '-c', assertion]).returncode == 0
(root / 'data/komika.sqlite3').write_bytes(b'original bytes')
assert subprocess.run(['sh', '-c', assertion]).returncode != 0, 'original plus copy must fail'
# Also prove the actual failure exit-code guard rejects exit 0.
guard = re.search(r'\[ "\$exit_code" -ne 0 \]', script)[0]
assert subprocess.run(['sh', '-c', 'exit_code=0; ' + guard]).returncode != 0
assert subprocess.run(['sh', '-c', 'exit_code=1; ' + guard]).returncode == 0
PY
echo 'PASS: real rename assertion rejects original plus copy; failure guard rejects exit zero'

# A foreign resource appears after the preflight check but before create returns.
for kind in volume network container; do
  export COLLISION=returned-$kind
  resource_init "$scratch/ledger-$kind"
  : > "$RESOURCE_CALLS"
  if resource_$kind candidate > "$scratch/output" 2>&1; then
    echo "FAIL: accepted foreign $kind returned by create" >&2; exit 1
  fi
  grep -q 'resource ownership mismatch' "$scratch/output"
  [ ! -s "$resource_root/$kind" ] || { echo 'FAIL: recorded foreign resource' >&2; exit 1; }
  resource_cleanup
  if grep -E '(^| )(rm|start)( |$)' "$RESOURCE_CALLS"; then exit 1; fi
  echo "PASS: foreign $kind returned by create is refused before recording/use"
done

: > "$RESOURCE_CALLS"
sh "$source_dir/cleanup-test-run.sh" 0123456789abcdef0123456789abcdef
for expected in 'container rm -f owned-container' 'network rm owned-network' 'volume rm owned-volume'; do
  grep -qx "$expected" "$RESOURCE_CALLS"
done
if grep -q '^container rm .*foreign' "$RESOURCE_CALLS"; then echo 'FAIL: orphan cleanup removed foreign resource' >&2; exit 1; fi
: > "$RESOURCE_CALLS"
if sh "$source_dir/cleanup-test-run.sh" invalid > "$scratch/output" 2>&1; then exit 1; fi
[ ! -s "$RESOURCE_CALLS" ]
echo 'PASS: label-based orphan recovery validates run ID and ownership before removal'

for kind in volume network container; do
  export COLLISION=owned-create
  resource_init "$scratch/owned-$kind"
  resource_$kind candidate
  [ "$resource_id" = owned-created ]
  grep -qx owned-created "$resource_root/$kind"
  echo "PASS: $kind created with this run label is verified and recorded"
done
