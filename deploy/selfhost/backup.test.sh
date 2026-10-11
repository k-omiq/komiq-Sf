#!/usr/bin/env bash
# Offline checks for publication, retention and dump failure safety.
set -euo pipefail
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/dumps"
cat > "$scratch/bin/docker" <<'SH'
#!/bin/sh
case "${DUMP_RESULT:-ok}" in
  ok) printf 'fixture custom-format dump\n' ;;
  fail) printf 'partial dump\n'; exit 1 ;;
  empty) exit 0 ;;
  wait) touch "$DUMP_STARTED"; sleep 30; printf "interrupted dump\n" ;;
esac
SH
chmod +x "$scratch/bin/docker"
export PATH="$scratch/bin:$PATH"
for n in 1 2 3 4 5 6 7 8; do printf 'old dump\n' > "$scratch/dumps/komiq-2000010${n}T020000Z.dump"; done
bash "$source_dir/backup.sh" "$scratch/dumps"
python3 - "$scratch/dumps" <<'PY'
import pathlib, stat, sys
root=pathlib.Path(sys.argv[1])
files=sorted(root.glob('komiq-*.dump'))
assert len(files)==7
assert not (root/'komiq-20000101T020000Z.dump').exists()
assert not (root/'komiq-20000102T020000Z.dump').exists()
assert stat.S_IMODE(files[-1].stat().st_mode)==0o600
assert files[-1].read_text()=='fixture custom-format dump\n'
assert not list(root.glob('.dump.*'))
assert (root/'.backup-lock').is_file()
PY
for result in fail empty; do
    if DUMP_RESULT=$result bash "$source_dir/backup.sh" "$scratch/dumps" > "$scratch/output" 2>&1; then
        echo "FAIL: accepted $result dump" >&2; exit 1
    fi
    python3 - "$scratch/dumps" <<'PY'
import pathlib, sys
root=pathlib.Path(sys.argv[1])
assert len(list(root.glob('komiq-*.dump')))==7
assert not list(root.glob('.dump.*'))
assert (root/'.backup-lock').is_file()
PY
done
echo 'PASS: backup publication, seven-dump retention, private permissions and failure preservation'

python3 - "$source_dir/backup.sh" "$scratch" <<'PYTEST'
import os, pathlib, signal, subprocess, time, sys
script=sys.argv[1]; root=pathlib.Path(sys.argv[2]); marker=root/'dump-started'
env=dict(os.environ,DUMP_RESULT='wait',DUMP_STARTED=str(marker))
with open(root/'killed.log','w') as output:
    child=subprocess.Popen(['bash',script,str(root/'interrupted')],env=env,stdout=output,stderr=output,start_new_session=True)
    try:
        deadline=time.monotonic()+5
        while not marker.exists() and time.monotonic()<deadline:
            if child.poll() is not None: raise AssertionError('backup did not start')
            time.sleep(.02)
        assert marker.exists()
        concurrent=subprocess.run(['bash',script,str(root/'interrupted')],capture_output=True,text=True,timeout=5)
        assert concurrent.returncode != 0 and 'Backup already running' in concurrent.stderr
        os.kill(child.pid,signal.SIGKILL); child.wait(timeout=5)
    finally:
        if child.poll() is None: os.killpg(child.pid,signal.SIGKILL); child.wait()
    restarted=subprocess.run(['bash',script,str(root/'interrupted')],capture_output=True,text=True,timeout=5)
    assert restarted.returncode==0, restarted.stderr
    assert len(list((root/'interrupted').glob('komiq-*.dump')))==1
    try: os.killpg(child.pid,signal.SIGKILL)
    except ProcessLookupError: pass
# Default destination must be private and outside the checkout.
env=dict(os.environ,HOME=str(root/'home'))
subprocess.run(['bash',script],env=env,check=True)
assert (root/'home/komiq-backups').is_dir()
assert (root/'home/komiq-backups').stat().st_mode & 0o777 == 0o700
PYTEST
echo 'PASS: SIGKILL releases backup lock; next nightly job succeeds; private outside-checkout default'
