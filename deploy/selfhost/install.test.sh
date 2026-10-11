#!/usr/bin/env bash
# Installer checks in disposable copies. Every Docker and firewall call is fake.
# Run with bash deploy/selfhost/install.test.sh. No containers are started.
set -euo pipefail

source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ "${1:-}" = --real ]; then
    shift
    exec bash "$source_dir/install.real.test.sh" "$@"
fi
bash_bin=${BASH:-/bin/bash}
test_root=$(mktemp -d "${TMPDIR:-/tmp}/komiq-install-test.XXXXXX")
trap 'rm -rf "$test_root"' EXIT
mkdir -p "$test_root/bin" "$test_root/no-docker"
cat > "$test_root/bin/stub" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
tool=${0##*/}
printf '%s' "$tool" >> "$STUB_LOG"
printf ' %s' "$@" >> "$STUB_LOG"
printf '\n' >> "$STUB_LOG"
case "$tool" in
    docker)
        case "$*" in
            info) [ "${STUB_DAEMON:-yes}" = yes ] ;;
            'info --format {{.Architecture}}') printf '%s\n' "${STUB_ARCH:-amd64}" ;;
            'info --format {{.MemTotal}}') printf '%s\n' "${STUB_MEMORY:-8589934592}" ;;
            'info --format {{.DockerRootDir}}') printf '%s\n' "$STUB_CASE" ;;
            'compose version --short') printf '%s\n' "${STUB_COMPOSE:-2.39.0}" ;;
            'compose ps -aq server'|'compose ps -q server')
                if [ -f "$STUB_CASE/started" ] || [ "${STUB_EXISTING:-no}" = yes ]; then
                    printf '%s\n' fake-server
                fi ;;
            'ps -aq --filter '*com.docker.compose.service=server)
                [ "${STUB_EXISTING:-no}" != yes ] || printf '%s\n' fake-server ;;
            'compose up -d --wait --wait-timeout 300 postgres') printf '%s\n' 'fake postgres ready' ;;
            'volume ls -q --filter '*) [ "${STUB_SQLITE:-no}" != yes ] || printf '%s\n' fake-volume ;;
            'compose images -q postgres') printf '%s\n' fake-postgres-image ;;
            'run --rm --network none --pull never --entrypoint sh '*) printf '%s\n' "${STUB_BYTES:-1048576}" ;;
            'inspect --format {{.Image}} '*) printf '%s\n' "${STUB_IMAGE:-sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}" ;;
            'inspect --format '*Config.Env*)
                if [ "${STUB_ENGINE:-sqlite}" = sqlite ]; then
                    url=${STUB_URL:-DATABASE_URL=sqlite:///data/komika.sqlite3}
                    case "$*:$url" in
                        *slice*:DATABASE_URL=sqlite://*) echo sqlite ;;
                        *:DATABASE_URL=sqlite:///data/komika.sqlite3|*:DATABASE_URL=sqlite://komika.sqlite3) echo sqlite ;;
                    esac
                fi ;;
            'image inspect --format '*Config.Env*)
                case "${@: -1}" in
                    sha256:aaaaaaaa*) echo sqlite ;;
                    *) exit 0 ;;
                esac ;;
            'image inspect komiq-selfhost:pre-postgres') [ "${STUB_GLOBAL_TAG:-no}" = yes ] ;;
            'image tag sha256:'*) exit 0 ;;
            'compose stop server') exit 0 ;;
            'inspect --format {{.State.Status}} '*) printf '%s\n' "${STUB_STATE:-running}" ;;
            'inspect --format {{.RestartCount}} '*) printf '%s\n' "${STUB_RESTARTS:-0}" ;;
            'compose config --quiet')
                if ! grep -Eq '^KOMIQ_DB_PASSWORD=.+$' "$STUB_CASE/.env" 2>/dev/null; then
                    echo 'Run install.sh to generate KOMIQ_DB_PASSWORD in .env' >&2; exit 1
                fi ;;
            'compose up -d --build')
                [ "${STUB_BUILD:-yes}" = yes ] || exit 1
                touch "$STUB_CASE/started"
                printf '%s\n' 'fake compose up' ;;
            'inspect --format '*NetworkSettings*) printf '%s\n' "${STUB_BINDINGS:-}" ;;
            'inspect --format '*State.Health*)
                ticks=$(cat "$STUB_CASE/ticks" 2>/dev/null || echo 0)
                ticks=$((ticks + 1)); echo "$ticks" > "$STUB_CASE/ticks"
                if [ "${STUB_REPEAT:-no}" = yes ] && [ "$ticks" -gt 740 ]; then echo healthy
                elif [ "$ticks" -le "${STUB_LONG:-0}" ]; then echo starting
                else printf '%s\n' "${STUB_HEALTH:-healthy}"; fi ;;
            'compose logs --no-color server'|'compose logs --no-color --since '*' server')
                if [ "${STUB_IMPORT_FAIL:-no}" = yes ]; then
                    echo 'server | sqlite import: failed: fixture verification mismatch'
                elif [ "${STUB_REPEAT:-no}" = yes ]; then
                    ticks=$(cat "$STUB_CASE/ticks")
                    count=$((ticks / 10 + 1))
                    [ "${STUB_ROTATE:-no}" != yes ] || count=$((count % 3 + 1))
                    for ((i=0; i<count; i++)); do echo 'server | sqlite import: table users: 10 rows sent, 5.000s'; done
                elif [ "${STUB_LONG:-0}" -gt 0 ]; then
                    ticks=$(cat "$STUB_CASE/ticks")
                    for ((i=0; i<=ticks/10; i++)); do echo "server | sqlite import: table users: $i rows sent, 5.000s"; done
                fi
                if [ "${STUB_CODE:-no}" = yes ]; then
                    printf '%s\n' \
                        'server | WARN enter setup code OLD1-OLD2-OLD3 (valid until the owner account is created)' \
                        'server | WARN no admin account yet: open the setup screen and enter setup code ABCD-EFGH-JKLM (valid until the owner account is created; a restart issues a new code)' \
                        'server | INFO listening on port 8080'
                else
                    printf '%s\n' 'server | INFO ready'
                fi ;;
            'compose exec server komika-server setup --setup-code ABCD-EFGH-JKLM --access '*) printf '%s\n' 'fake setup finished' ;;
            *) printf 'Unexpected fake Docker call: %s\n' "$*" >&2; exit 1 ;;
        esac ;;
    sudo) "$@" ;;
    ufw)
        case "$*" in
            status) printf 'Status: %s\n' "${STUB_UFW:-active}" ;;
            allow*) exit 0 ;;
            *) exit 1 ;;
        esac ;;
    firewall-cmd)
        case "$*" in
            --state) printf '%s\n' "${STUB_FIREWALLD:-not-running}" ;;
            --permanent*|--reload) exit 0 ;;
            *) exit 1 ;;
        esac ;;
    ss)
        [ "${STUB_PORT_TOOL:-ss}" = ss ] || exit 1
        printf '%s\n' 'State Recv-Q Send-Q Local Address:Port Peer Address:Port'
        for p in ${STUB_TAKEN:-}; do printf 'LISTEN 0 128 0.0.0.0:%s 0.0.0.0:*\n' "$p"; done ;;
    lsof)
        [ "${STUB_PORT_TOOL:-ss}" = lsof ] || exit 2
        for p in ${STUB_TAKEN:-}; do printf 'p100\nn*:%s\n' "$p"; done ;;
    netstat)
        for p in ${STUB_TAKEN:-}; do printf 'tcp4 0 0 *.%s *.* LISTEN\n' "$p"; done ;;
    uname)
        case "$*" in -s) printf '%s\n' "${STUB_OS:-Linux}" ;; -m) printf '%s\n' amd64 ;; *) exit 1 ;; esac ;;
    df) printf '%s\n' 'Filesystem 1024-blocks Used Available Capacity Mounted on' "disk 20000000 100000 ${STUB_DISK:-19000000} 1% /" ;;
    hostname) printf '%s\n' '192.168.1.20' ;;
    ipconfig) printf '%s\n' '192.168.1.21' ;;
    sleep) exit 0 ;;
    *) exit 1 ;;
esac
EOF
chmod +x "$test_root/bin/stub"
for tool in docker sudo ufw firewall-cmd ss lsof netstat uname df hostname ipconfig sleep; do
    ln -s stub "$test_root/bin/$tool"
done
ln -s "$(command -v dirname)" "$test_root/no-docker/dirname"
export PATH="$test_root/bin:$PATH"

new_case() {
    STUB_CASE=$test_root/$1
    mkdir -p "$STUB_CASE"
    cp "$source_dir/install.sh" "$source_dir/install-progress.awk" "$source_dir/.env.example" "$source_dir/docker-compose.yml" \
        "$source_dir/docker-compose.https.yml" "$source_dir/docker-compose.acme.yml" "$STUB_CASE/"
    STUB_LOG=$STUB_CASE/calls
    : > "$STUB_LOG"
    export STUB_CASE STUB_LOG
    unset STUB_DAEMON STUB_ARCH STUB_MEMORY STUB_COMPOSE STUB_EXISTING STUB_BINDINGS \
        STUB_BUILD STUB_HEALTH STUB_CODE STUB_UFW STUB_FIREWALLD STUB_PORT_TOOL \
        STUB_TAKEN STUB_OS STUB_DISK STUB_SQLITE STUB_LONG STUB_IMPORT_FAIL STUB_STATE STUB_RESTARTS STUB_ENGINE STUB_IMAGE STUB_GLOBAL_TAG STUB_BYTES STUB_URL STUB_REPEAT STUB_ROTATE COMPOSE_PROJECT_NAME
}
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    cat "$STUB_CASE/output" >&2
    exit 1
}
pass() { printf 'PASS: %s\n' "$*"; }
contains() { grep -F -- "$2" "$1" >/dev/null || fail "Missing '$2' in ${1##*/}"; }
absent() { if grep -F -- "$2" "$1" >/dev/null; then fail "Unexpected '$2' in ${1##*/}"; fi; }
run_ok() { "$bash_bin" "$STUB_CASE/install.sh" "$@" > "$STUB_CASE/output" 2>&1 || fail "Installer failed"; }
run_bad() { if "$bash_bin" "$STUB_CASE/install.sh" "$@" > "$STUB_CASE/output" 2>&1; then fail "Installer should have failed"; fi; }

for mode in local tunnel public-ip direct; do
    new_case "fresh-$mode"
    run_ok --yes --access "$mode"
    contains "$STUB_CASE/.env" 'KOMIQ_PORT=8080'
    contains "$STUB_LOG" 'docker compose up -d --build'
    contains "$STUB_CASE/output" 'Server is healthy.'
    contains "$STUB_CASE/output" 'The server is already set up.'
    contains "$STUB_CASE/output" "If this is a cloud server, also open the same ports in your provider's firewall (security group / security list)."
    case "$mode" in
        local)
            contains "$STUB_CASE/.env" 'KOMIQ_BIND=0.0.0.0'
            if grep '^COMPOSE_FILE=' "$STUB_CASE/.env" >/dev/null; then fail 'Local mode set overrides'; fi
            contains "$STUB_LOG" 'sudo ufw allow 8080/tcp' ;;
        tunnel)
            contains "$STUB_CASE/.env" 'KOMIQ_BIND=127.0.0.1'
            if grep '^COMPOSE_FILE=' "$STUB_CASE/.env" >/dev/null; then fail 'Tunnel mode set overrides'; fi
            absent "$STUB_LOG" 'sudo'
            absent "$STUB_LOG" 'ufw status' ;;
        public-ip)
            contains "$STUB_CASE/.env" 'COMPOSE_FILE=docker-compose.yml:docker-compose.https.yml'
            if grep '^COMPOSE_FILE=.*docker-compose.acme.yml' "$STUB_CASE/.env" >/dev/null; then fail 'Public IP mode enabled ACME'; fi
            contains "$STUB_LOG" 'sudo ufw allow 443/tcp'
            # The plain HTTP port stays on this machine in the internet modes.
            contains "$STUB_CASE/.env" 'KOMIQ_BIND=127.0.0.1'
            absent "$STUB_LOG" 'sudo ufw allow 8080/tcp' ;;
        direct)
            contains "$STUB_CASE/.env" 'COMPOSE_FILE=docker-compose.yml:docker-compose.https.yml:docker-compose.acme.yml'
            contains "$STUB_LOG" 'sudo ufw allow 443/tcp'
            contains "$STUB_LOG" 'sudo ufw allow 80/tcp' ;;
    esac
    pass "fresh $mode (--yes)"
done

# The server listens on 8080 inside its container whatever KOMIQ_PORT says. The compose
# file hands it the published port, so the addresses it suggests name the right one.
new_case published-port
contains "$STUB_CASE/docker-compose.yml" "- '\${KOMIQ_BIND:-0.0.0.0}:\${KOMIQ_PORT:-8080}:8080'"
contains "$STUB_CASE/docker-compose.yml" 'KOMIKA_PUBLISHED_PORT: ${KOMIQ_PORT:-8080}'
run_ok --yes --port 9000 --access direct --encrypt no --firewall no --no-setup
contains "$STUB_CASE/.env" 'KOMIQ_PORT=9000'
contains "$STUB_CASE/output" 'Internet: use http://<public-IP>:9000/ and http://<public-IP>:9000/admin.'
pass 'the server is told the published HTTP port'

# "Only my devices": the server itself refuses plain HTTP from the internet. The line
# does not say the server is in that mode: this run may not have set it (--no-setup, or
# a server that is set up already keeps its own choice).
new_case local-closed
run_ok --yes --access local --firewall no --no-setup
contains "$STUB_CASE/output" 'Internet: closed when the server is set to "Only my devices". It then refuses plain HTTP from the internet.'
pass 'local says the internet is closed'

# Without a domain and without HTTPS, readers use the HTTP port itself: it answers
# every address, and ports 443 and 80 stay closed.
new_case fresh-direct-plain
run_ok --yes --access direct --encrypt no
contains "$STUB_CASE/.env" 'KOMIQ_BIND=0.0.0.0'
if grep '^COMPOSE_FILE=' "$STUB_CASE/.env" >/dev/null; then fail 'Plain direct mode set overrides'; fi
contains "$STUB_LOG" 'sudo ufw allow 8080/tcp'
absent "$STUB_LOG" 'sudo ufw allow 443/tcp'
absent "$STUB_LOG" 'sudo ufw allow 80/tcp'
contains "$STUB_CASE/output" 'warning: Without HTTPS, readers use HTTP port 8080.'
contains "$STUB_CASE/output" 'Internet: use http://<public-IP>:8080/ and http://<public-IP>:8080/admin. This connection is not encrypted.'
absent "$STUB_CASE/output" 'home network connect over HTTP'
pass 'fresh direct without HTTPS opens only the HTTP port'

new_case rerun
cat > "$STUB_CASE/.env" <<'EOF'
# Keep this comment.
KOMIQ_PORT='9000' # old port
KOMIQ_BIND="127.0.0.1"
TZ=America/New_York
COMPOSE_PROFILES=flaresolverr
FLARESOLVERR_ENABLED=true
CUSTOM_VALUE='spaces and $literal; untouched'
EOF
cp "$STUB_CASE/.env" "$STUB_CASE/original"
run_ok --yes --no-setup
contains "$STUB_CASE/output" 'HTTP port [9000]: 9000'
contains "$STUB_CASE/output" '(yes/no) [no]: no'
contains "$STUB_CASE/.env" 'KOMIQ_BIND=127.0.0.1'
diff <(sed '/^KOMIQ_PORT=/d; /^KOMIQ_BIND=/d; /^COMPOSE_FILE=/d' "$STUB_CASE/original") \
    <(sed '/^KOMIQ_PORT=/d; /^KOMIQ_BIND=/d; /^COMPOSE_FILE=/d; /^KOMIQ_DB_PASSWORD=/d; /^KOMIQ_IMPORT_START_PERIOD=/d' "$STUB_CASE/.env") >/dev/null || fail 'Other .env lines changed'
cmp "$STUB_CASE/original" "$STUB_CASE/.env.bak" || fail 'Backup differs'
run_ok --yes --port 9001 --access direct --firewall no --no-setup
contains "$STUB_CASE/.env" 'KOMIQ_PORT=9001'
run_ok --yes --no-setup --firewall no
contains "$STUB_CASE/output" 'komiq? (number or name) [direct]: direct'
cmp "$STUB_CASE/original" "$STUB_CASE/.env.bak" || fail 'Backup was overwritten'
run_ok --yes --access local --lan no --no-setup --firewall no
if grep '^COMPOSE_FILE=' "$STUB_CASE/.env" >/dev/null; then fail 'Local mode kept overrides'; fi
contains "$STUB_LOG" 'docker compose logs --no-color --since'
pass 're-run preserves other settings, current defaults and the first backup; mode switch removes overrides'

new_case tunnel-rerun
run_ok --yes --access tunnel --lan yes --firewall no --no-setup
run_ok --yes --access tunnel --firewall no --no-setup
contains "$STUB_CASE/output" '(yes/no) [yes]: yes'
contains "$STUB_CASE/.env" 'KOMIQ_BIND=0.0.0.0'
pass 'tunnel re-run keeps the existing home network choice'

new_case exported-env
cat > "$STUB_CASE/.env" <<'EOF'
export KOMIQ_PORT=9090
export KOMIQ_BIND=127.0.0.1
export COMPOSE_FILE=docker-compose.yml:docker-compose.https.yml
TZ=Europe/London
EOF
run_ok --yes --no-setup --firewall no
contains "$STUB_CASE/output" 'HTTP port [9090]: 9090'
contains "$STUB_CASE/output" 'komiq? (number or name) [public-ip]: public-ip'
contains "$STUB_CASE/.env" 'TZ=Europe/London'
absent "$STUB_CASE/.env" 'export KOMIQ_'
pass 'quoted and export-prefixed installer keys are read safely'

new_case unattended-flags
run_ok --port 9080 --access local --lan no --firewall no --no-setup
contains "$STUB_CASE/.env" 'KOMIQ_PORT=9080'
contains "$STUB_CASE/.env" 'KOMIQ_BIND=127.0.0.1'
pass 'complete flags run unattended without --yes'

new_case taken-port
export STUB_TAKEN='8080 8081'
run_bad --yes
contains "$STUB_CASE/output" 'Port 8080 is in use. Try --port 8082.'
absent "$STUB_LOG" 'compose up'
[ ! -e "$STUB_CASE/.env" ] || fail 'Busy port wrote .env'
pass 'taken HTTP port stops and offers the next free port'

for p in 443 80; do
    new_case "taken-$p"
    export STUB_TAKEN=$p
    run_bad --yes --access direct
    contains "$STUB_CASE/output" "Port $p is in use. Free it before continuing."
    absent "$STUB_LOG" 'compose up'
    pass "taken port $p stops"
done

new_case existing-server
export STUB_EXISTING=yes STUB_BINDINGS='0.0.0.0 8080' STUB_TAKEN=8080
run_ok --yes --no-setup
contains "$STUB_CASE/output" 'Port 8080 is already used by this komiq server.'
pass 'running stack can be re-installed on its own published port'

new_case wrong-bind
export STUB_EXISTING=yes STUB_BINDINGS='127.0.0.1 8080' STUB_TAKEN=8080
run_bad --yes --lan yes
contains "$STUB_CASE/output" 'Port 8080 is in use.'
pass 'another bind is not treated as a free port'

new_case no-docker
if PATH="$test_root/no-docker" "$bash_bin" "$STUB_CASE/install.sh" --yes > "$STUB_CASE/output" 2>&1; then fail 'Missing Docker should fail'; fi
contains "$STUB_CASE/output" 'Docker is missing. Install Docker Engine or Docker Desktop'
pass 'missing Docker gives an install instruction'

new_case setup-pipe
export STUB_CODE=yes
# The case is the installer WITHOUT a terminal, also when this test runs in one.
run_ok --yes --access tunnel < /dev/null
contains "$STUB_CASE/output" 'docker compose exec server komika-server setup --setup-code ABCD-EFGH-JKLM --access tunnel'
absent "$STUB_LOG" 'docker compose exec server komika-server setup'
pass 'non-TTY prints the command with only the latest setup code'

# Give the installer a terminal on stdin; the fake wizard records its arguments.
command -v python3 >/dev/null 2>&1 || fail 'python3 is needed for the terminal check'
new_case setup-tty
export STUB_CODE=yes
python3 - "$bash_bin" "$STUB_CASE/install.sh" "$STUB_CASE/output" <<'PY'
import os
import pty
import subprocess
import sys
master, slave = pty.openpty()
try:
    with open(sys.argv[3], 'wb') as output:
        result = subprocess.run([sys.argv[1], sys.argv[2], '--yes', '--access', 'direct'],
                                stdin=slave, stdout=output, stderr=subprocess.STDOUT)
    if result.returncode:
        print(open(sys.argv[3]).read())
        sys.exit(result.returncode)
finally:
    os.close(slave)
    os.close(master)
PY
contains "$STUB_LOG" 'docker compose exec server komika-server setup --setup-code ABCD-EFGH-JKLM --access direct --encrypt yes'
contains "$STUB_CASE/output" 'fake setup finished'
pass 'TTY passes the extracted setup code and the choices to docker compose exec'

new_case firewalld
export STUB_UFW=inactive STUB_FIREWALLD=running
run_ok --yes --access direct
for p in 443 80; do contains "$STUB_LOG" "sudo firewall-cmd --permanent --add-port=$p/tcp"; done
absent "$STUB_LOG" 'add-port=8080/tcp'
contains "$STUB_LOG" 'sudo firewall-cmd --reload'
pass 'firewalld adds the needed ports and reloads'

new_case firewall-no
run_ok --yes --firewall no
absent "$STUB_LOG" 'sudo'
pass '--firewall no never changes the firewall'

new_case macos
export STUB_OS=Darwin
run_ok --yes --access direct
absent "$STUB_LOG" 'sudo'
absent "$STUB_LOG" 'ufw'
absent "$STUB_LOG" 'firewall-cmd'
contains "$STUB_CASE/output" 'Host firewall changes are skipped on macOS.'
pass 'macOS never touches a firewall'

new_case no-firewall
export STUB_UFW=inactive STUB_FIREWALLD=not-running
run_ok --yes
contains "$STUB_CASE/output" 'No active host firewall was found'
absent "$STUB_LOG" 'sudo'
pass 'no active host firewall is reported'

new_case low-memory
export STUB_MEMORY=1073741824
run_bad --port 8080 --access local --lan yes --firewall no
contains "$STUB_CASE/output" 'Under 2 GB of memory.'
run_ok --yes --firewall no
contains "$STUB_CASE/output" 'Add swap on Linux'
pass 'under 2 GB stops unless --yes; memory warning explains options'

new_case disk-warning
export STUB_DISK=1000
run_ok --yes --firewall no
contains "$STUB_CASE/output" 'Under 5 GB of disk space'
pass 'low disk space warns'

for failure in daemon compose cpu; do
    new_case "bad-$failure"
    case "$failure" in daemon) export STUB_DAEMON=no ;; compose) export STUB_COMPOSE=1.29.2 ;; cpu) export STUB_ARCH=armv7 ;; esac
    run_bad --yes
    absent "$STUB_LOG" 'compose up'
    pass "unsupported $failure stops before building"
done

new_case health-timeout
export STUB_HEALTH=starting
run_bad --yes --no-setup --firewall no
contains "$STUB_CASE/output" 'Server did not become healthy.'
contains "$STUB_CASE/output" 'docker compose logs server'
pass 'health timeout points to logs (sleep is stubbed)'

for flag in '--unknown' '--port' '--access invalid' '--lan maybe' '--port 0' '--port 70000' '--port xyz' '--firewall invalid' \
    '--encrypt' '--access tunnel --encrypt no' '--access direct --encrypt maybe' '--access direct --encrypt no --lan no'; do
    new_case invalid-flag
    # Each test value is a literal option string, deliberately split into words.
    # shellcheck disable=SC2086
    run_bad --yes $flag
    absent "$STUB_LOG" 'compose up'
done
pass 'unknown flags, missing values and invalid answers fail'

new_case help
run_ok --help
contains "$STUB_CASE/output" 'Usage:'
[ ! -s "$STUB_LOG" ] || fail 'Help ran a dependency check'
pass '--help needs no Docker'

new_case lsof
export STUB_PORT_TOOL=lsof STUB_TAKEN=8080
run_bad --yes
contains "$STUB_CASE/output" 'Port 8080 is in use.'
pass 'lsof detects taken ports when ss fails'

new_case netstat
export STUB_PORT_TOOL=netstat STUB_TAKEN=8080
run_bad --yes
contains "$STUB_CASE/output" 'Port 8080 is in use.'
pass 'netstat detects taken ports when ss and lsof fail'

new_case postgres-password
run_ok --yes --no-setup --firewall no
python3 - "$STUB_CASE" <<'PYTEST'
import pathlib, re, stat, sys
root = pathlib.Path(sys.argv[1])
env = (root / '.env').read_text()
secret = re.search(r'^KOMIQ_DB_PASSWORD=(.+)$', env, re.M)[1]
assert re.fullmatch('[0-9a-f]{64}', secret), 'random password format'
assert stat.S_IMODE((root / '.env').stat().st_mode) == 0o600, 'env mode'
assert secret not in (root / 'output').read_text(), 'password leaked to output'
assert secret not in (root / 'calls').read_text(), 'password leaked in arguments'
(root / 'password-before').write_text(secret)
calls = (root / 'calls').read_text()
assert calls.index('compose up -d --wait') < calls.index('compose up -d --build'), 'service order'
PYTEST
run_ok --yes --no-setup --firewall no
python3 - "$STUB_CASE" <<'PYTEST'
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
assert re.search(r'^KOMIQ_DB_PASSWORD=(.+)$', (root / '.env').read_text(), re.M)[1] == (root / 'password-before').read_text(), 'password changed'
PYTEST
pass 'new install generates one private password without printing it and starts postgres first'

new_case missing-password
contains "$STUB_CASE/docker-compose.yml" '${KOMIQ_DB_PASSWORD:?Run install.sh to generate KOMIQ_DB_PASSWORD in .env}'
if docker compose config --quiet > "$STUB_CASE/output" 2>&1; then fail 'Missing password accepted'; fi
contains "$STUB_CASE/output" 'Run install.sh to generate KOMIQ_DB_PASSWORD in .env'
pass 'manual Compose requires a password (interpolation contract; real mode checks Compose)'

new_case sqlite-upgrade
export STUB_SQLITE=yes STUB_EXISTING=yes STUB_LONG=730
printf '# legacy settings\nCUSTOM=keep-this\n' > "$STUB_CASE/.env"
run_ok --yes --no-setup --firewall no
contains "$STUB_CASE/.env" 'CUSTOM=keep-this'
contains "$STUB_CASE/output" 'The first start converts your SQLite database to PostgreSQL'
contains "$STUB_CASE/output" 'sqlite import: table users: 73 rows sent'
contains "$STUB_LOG" 'docker image tag sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa komiq-selfhost:komiq-pre-postgres'
pass 'SQLite upgrade detects the volume and survives a long progressing import'

new_case sqlite-failure
export STUB_SQLITE=yes STUB_IMPORT_FAIL=yes
run_bad --yes --no-setup --firewall no
contains "$STUB_CASE/output" 'server | sqlite import: failed: fixture verification mismatch'
contains "$STUB_CASE/output" 'No SQLite image was saved.'
contains "$STUB_CASE/output" 'docker-compose.rollback.yml'
contains "$STUB_LOG" 'docker compose stop server'
pass 'failed import preserves the exact failure and gives rollback steps'

new_case sqlite-stalled
export STUB_SQLITE=yes STUB_HEALTH=starting
run_bad --yes --no-setup --firewall no
contains "$STUB_CASE/output" 'No new import progress arrived'
[ "$(cat "$STUB_CASE/ticks")" = 720 ] || fail 'Import floor must allow one hour'
pass 'stalled import has a finite one-hour inactivity deadline'

new_case server-restart
export STUB_RESTARTS=1
run_bad --yes --no-setup --firewall no
contains "$STUB_LOG" 'docker compose stop server'
pass 'restart policy cannot hide startup failure'

new_case other-project-tag
export STUB_SQLITE=yes STUB_EXISTING=yes STUB_GLOBAL_TAG=yes COMPOSE_PROJECT_NAME=home-server
run_ok --yes --no-setup --firewall no
contains "$STUB_LOG" 'docker image tag sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa komiq-selfhost:home-server-pre-postgres'
absent "$STUB_LOG" 'image inspect komiq-selfhost:pre-postgres'
[ "$(cat "$STUB_CASE/.sqlite-image-home-server")" = sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ] || fail 'Missing original image identity'
pass "another project's global tag cannot select the rollback image"

new_case sqlite-failed-retry
export STUB_SQLITE=yes STUB_EXISTING=yes STUB_IMPORT_FAIL=yes
run_bad --yes --no-setup --firewall no
cp "$STUB_CASE/.sqlite-image-komiq" "$STUB_CASE/original-image"
export STUB_ENGINE=postgres STUB_IMAGE=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb STUB_IMPORT_FAIL=no
run_ok --yes --no-setup --firewall no
cmp "$STUB_CASE/original-image" "$STUB_CASE/.sqlite-image-komiq" || fail 'Recorded SQLite identity overwritten'
absent "$STUB_LOG" 'image tag sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
contains "$STUB_CASE/output" 'Server is healthy.'
contains "$STUB_LOG" 'docker image tag sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa komiq-selfhost:komiq-pre-postgres'
pass 'failed upgrade retry keeps the recorded SQLite image'

new_case sqlite-no-container-retry
export STUB_SQLITE=yes STUB_IMPORT_FAIL=yes
run_bad --yes --no-setup --firewall no
contains "$STUB_CASE/output" 'published SQLite image tag for the release you previously installed'
export STUB_EXISTING=yes STUB_ENGINE=postgres STUB_IMAGE=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb STUB_IMPORT_FAIL=no
run_ok --yes --no-setup --firewall no
[ ! -e "$STUB_CASE/.sqlite-image-komiq" ] || fail 'Retry recorded a PostgreSQL image'
absent "$STUB_LOG" 'docker image tag'
contains "$STUB_CASE/output" 'No SQLite image was saved:'
contains "$STUB_CASE/output" 'Server is healthy.'
pass 'retry without an original container refuses PostgreSQL as rollback'

new_case large-import-grace
export STUB_SQLITE=yes STUB_BYTES=137500000000
run_ok --yes --no-setup --firewall no
contains "$STUB_CASE/.env" 'KOMIQ_IMPORT_START_PERIOD=180000s'
contains "$STUB_CASE/docker-compose.yml" 'start_period: ${KOMIQ_IMPORT_START_PERIOD:-24h}'
pass 'large source health grace scales to one hour per 2.75 GB'

# Each control runs in isolation so all round-2 regressions report on old code.
round2_failed=0
for control in state-postgres state-mutable overlay-postgres sqlite-query state-mode rollback-base rollback-https rollback-acme repeated-progress rotated-progress huge-source zero-source; do
  set +e
  (
    set -e
    new_case "$control"
    export STUB_SQLITE=yes
    case "$control" in
      state-postgres|state-mutable)
        image=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
        [ "$control" != state-mutable ] || image=sha256:latest
        printf '%s\n' "$image" > "$STUB_CASE/.sqlite-image-komiq"
        run_bad --yes --no-setup --firewall no
        absent "$STUB_LOG" 'docker image tag'
        absent "$STUB_LOG" 'compose up -d --build' ;;
      overlay-postgres)
        export STUB_EXISTING=yes STUB_ENGINE=sqlite STUB_IMAGE=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
        run_ok --yes --no-setup --firewall no
        [ ! -e "$STUB_CASE/.sqlite-image-komiq" ] || fail 'Overlay disguised PostgreSQL image'
        absent "$STUB_LOG" 'docker image tag' ;;
      sqlite-query|state-mode)
        export STUB_EXISTING=yes STUB_URL='DATABASE_URL=sqlite:///data/komika.sqlite3?mode=rwc'
        if [ "$control" = state-mode ]; then
          printf '%s\n' sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa > "$STUB_CASE/.sqlite-image-komiq"
          chmod 644 "$STUB_CASE/.sqlite-image-komiq"
        fi
        run_ok --yes --no-setup --firewall no
        contains "$STUB_LOG" 'docker image tag sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
        python3 - "$STUB_CASE/.sqlite-image-komiq" <<'CHECK'
import os, stat, sys
assert stat.S_IMODE(os.stat(sys.argv[1]).st_mode) == 0o600
CHECK
        if [ "$control" = state-mode ]; then
          chmod 644 "$STUB_CASE/.sqlite-image-komiq"
          export STUB_SQLITE=no
          run_ok --yes --no-setup --firewall no
          python3 - "$STUB_CASE/.sqlite-image-komiq" <<'CHECK'
import os, stat, sys
assert stat.S_IMODE(os.stat(sys.argv[1]).st_mode) == 0o600
CHECK
        fi
        ;;
      rollback-*)
        export STUB_IMPORT_FAIL=yes STUB_EXISTING=yes
        mode=local files='-f docker-compose.yml'
        case "$control" in
          rollback-https) mode=public-ip; files="$files -f docker-compose.https.yml" ;;
          rollback-acme) mode=direct; files="$files -f docker-compose.https.yml -f docker-compose.acme.yml" ;;
        esac
        run_bad --yes --no-setup --firewall no --access "$mode"
        contains "$STUB_CASE/output" "docker compose $files -f docker-compose.rollback.yml up -d --no-build --no-deps server"
        # The no-saved-image recovery must preserve overlays too.
        rm "$STUB_CASE/.sqlite-image-komiq"
        export STUB_EXISTING=no
        run_bad --yes --no-setup --firewall no --access "$mode"
        contains "$STUB_CASE/output" "docker compose $files -f docker-compose.rollback.yml up -d --no-build --no-deps server" ;;
      *progress)
        export STUB_REPEAT=yes STUB_HEALTH=starting
        [ "$control" != rotated-progress ] || export STUB_ROTATE=yes
        # Bound broken implementations without waiting indefinitely.
        export STUB_LONG=740
        run_bad --yes --no-setup --firewall no
        contains "$STUB_CASE/output" 'No new import progress arrived'
        [ "$(cat "$STUB_CASE/ticks")" -le 722 ] || fail 'Repeated progress renewed stall allowance' ;;
      huge-source|zero-source)
        export STUB_BYTES=2562047788010000
        expected=3353953468
        [ "$control" != zero-source ] || { export STUB_BYTES=0; expected=86400; }
        run_ok --yes --no-setup --firewall no
        contains "$STUB_CASE/.env" "KOMIQ_IMPORT_START_PERIOD=${expected}s"
        if [ "$control" = zero-source ]; then
          contains "$STUB_CASE/output" 'The first start converts'
        else
          # Check inactivity calculation as well as persisted health grace.
          python3 - "$STUB_CASE/install.sh" <<'CHECK'
import pathlib, subprocess, sys
s=pathlib.Path(sys.argv[1]).read_text()
a=s.index('# Timing rule:')
b=s.index("'Waiting for readiness", a)
b=s.rfind("printf", a, b)
# The preceding size helper is executed with the actual inactivity block.
helper=s[s.index('import_seconds()'):s.index('# Compose defaults')]
code=helper + '\nlegacy_found=yes; legacy_bytes=2562047788010000\n' + s[a:b] + '\nprintf "%s" "$idle_seconds"'
assert subprocess.check_output(['bash', '-c', code], text=True) == '3353953468'
CHECK
        fi ;;
    esac
    pass "round 2: $control"
  )
  result=$?
  set -e
  [ "$result" = 0 ] || round2_failed=1
done
[ "$round2_failed" = 0 ] || { echo 'FAIL: Round-2 regressions failed' >&2; exit 1; }
bash "$source_dir/install-progress.test.sh"
bash "$source_dir/privacy.test.sh"
bash "$source_dir/test-resources.test.sh"
printf '%s\n' 'All installer stub tests passed.'
