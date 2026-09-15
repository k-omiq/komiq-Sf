#!/usr/bin/env bash
# komiq self-host installer. Run from anywhere with bash deploy/selfhost/install.sh.
# Settings stay in .env; accounts and internet access are chosen in the wizard.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

usage() {
    cat <<'EOF'
Usage: bash deploy/selfhost/install.sh [options]
  --port PORT                 HTTP port (default: 8080 or current .env value)
  --access MODE               local, tunnel, public-ip or direct (default: local)
  --encrypt yes|no            With --access direct: HTTPS with a free certificate,
                              or plain HTTP on the HTTP port (default: yes)
  --lan yes|no                Open the HTTP port to your home network
                              (default: yes for local, else no, or current bind)
  --firewall yes|no|ask        Open needed host ports (default: ask)
  --yes                       Accept defaults; allow a build with under 2 GB
  --no-setup                  Skip the first-run wizard
  --help                      Show this help
Existing port, bind and Compose files supply defaults on a re-run.
Without a terminal, supply --yes or all needed answers as flags.
The setup wizard still needs a terminal for the owner account and other answers.
EOF
}

problem() { printf 'problem: %s\n' "$*" >&2; exit 1; }
ok() { printf 'ok: %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*"; }

port_arg= access_arg= encrypt_arg= lan_arg= firewall=ask
yes=no no_setup=no
while [ "$#" -gt 0 ]; do
    case "$1" in
        --port|--access|--encrypt|--lan|--firewall)
            [ "$#" -ge 2 ] || problem "$1 needs a value. See --help."
            case "$1" in
                --port) port_arg=$2 ;;
                --access) access_arg=$2 ;;
                --encrypt) encrypt_arg=$2 ;;
                --lan) lan_arg=$2 ;;
                --firewall) firewall=$2 ;;
            esac
            [ -n "$2" ] || problem "$1 needs a value. See --help."
            shift 2 ;;
        --yes) yes=yes; shift ;;
        --no-setup) no_setup=yes; shift ;;
        --help|-h) usage; exit 0 ;;
        *) problem "Unknown option: $1. See --help." ;;
    esac
done
case "$firewall" in yes|no|ask) ;; *) problem "--firewall must be yes, no or ask." ;; esac

# Read only the installer keys, never execute .env as shell code.
env_value() {
    [ -f .env ] || return 0
    awk -v key="$1" '
        $0 ~ "^[[:space:]]*(export[[:space:]]+)?" key "[[:space:]]*=" {
            value=$0
            sub(/^[^=]*=[[:space:]]*/, "", value)
            sub(/[[:space:]]+#.*$/, "", value)
            sub(/[[:space:]]*$/, "", value)
            if (value ~ /^".*"$/ || value ~ /^\047.*\047$/)
                value=substr(value, 2, length(value)-2)
            found=value
        }
        END { print found }
    ' .env
}

ask() {
    local answer
    if [ "$yes" = yes ]; then
        REPLY=$2
        printf '%s [%s]: %s\n' "$1" "$2" "$2"
    else
        [ -t 0 ] || problem "No terminal for '$1'. Supply its flag or --yes."
        printf '%s [%s]: ' "$1" "$2"
        IFS= read -r answer || problem "No answer received."
        REPLY=${answer:-$2}
    fi
}

command -v docker >/dev/null 2>&1 || problem "Docker is missing. Install Docker Engine or Docker Desktop, then try again."
ok "Docker is installed."
docker info >/dev/null 2>&1 || problem "Docker is not running. Start Docker or check your Docker permissions."
ok "Docker is running."
compose_version=$(docker compose version --short 2>/dev/null) || problem "Compose v2 is missing. Install the Docker Compose plugin."
case "$compose_version" in
    v2.*|2.*|v[3-9].*|[3-9].*) ok "Docker Compose is available ($compose_version)." ;;
    *) problem "Compose v2 or newer is needed. Update the Docker Compose plugin." ;;
esac
arch=$(docker info --format '{{.Architecture}}' 2>/dev/null) || arch=$(uname -m)
case "$arch" in
    x86_64|amd64|aarch64|arm64) ok "CPU is supported ($arch)." ;;
    *) problem "CPU '$arch' is unsupported. Use an amd64 or arm64 Docker machine." ;;
esac
memory=$(docker info --format '{{.MemTotal}}' 2>/dev/null) || memory=
case "$memory" in
    ''|*[!0-9]*) warn "Could not check Docker memory. The build needs 4 GB; check Docker's memory limit." ;;
    *)
        if [ "$memory" -lt 4294967296 ]; then
            warn "Docker has under 4 GB of memory. Add swap on Linux, raise Docker Desktop's memory limit, or build elsewhere."
            printf '%s\n' 'To copy a build: docker save komiq-selfhost:local | ssh <server> docker load'
            if [ "$memory" -lt 2147483648 ] && [ "$yes" != yes ]; then
                problem "Under 2 GB of memory. Add memory or swap, or use --yes to continue."
            fi
        else
            ok "Docker has at least 4 GB of memory."
        fi ;;
esac
docker_root=$(docker info --format '{{.DockerRootDir}}' 2>/dev/null) || docker_root=.
[ -d "$docker_root" ] && [ -r "$docker_root" ] || docker_root=.
disk=$(df -Pk "$docker_root" 2>/dev/null | awk 'END { print $4 }') || disk=
case "$disk" in
    ''|*[!0-9]*) warn "Could not check free disk space. Keep at least 5 GB free for the build." ;;
    *)
        if [ "$disk" -lt 5242880 ]; then
            warn "Under 5 GB of disk space in $docker_root. Free space before building."
        else
            ok "At least 5 GB of disk space is free in $docker_root."
        fi ;;
esac

current_port=$(env_value KOMIQ_PORT)
current_bind=$(env_value KOMIQ_BIND)
current_files=$(env_value COMPOSE_FILE)
port=${port_arg:-}
if [ -z "$port" ]; then
    ask 'HTTP port' "${current_port:-8080}"
    port=$REPLY
fi
case "$port" in ''|*[!0-9]*) problem "Choose a port number from 1 to 65535." ;; esac
[ "${#port}" -le 5 ] || problem "Choose a port number from 1 to 65535."
port=$((10#$port))
[ "$port" -ge 1 ] && [ "$port" -le 65535 ] || problem "Choose a port number from 1 to 65535."
access_default=local
case "$current_files" in
    *docker-compose.acme.yml*) access_default=direct ;;
    *docker-compose.https.yml*) access_default=public-ip ;;
esac
access=${access_arg:-}
if [ -z "$access" ]; then
    printf '%s\n' \
        '1) local: This machine or your home network, using HTTP.' \
        '2) tunnel: A domain through Cloudflare Tunnel; no inbound internet ports.' \
        '3) public-ip: A domain through Cloudflare DNS; needs HTTPS port 443.' \
        '4) direct: Your public IP without a domain; needs ports 443 and 80.'
    ask 'How should people reach komiq? (number or name)' "$access_default"
    case "$REPLY" in 1) access=local ;; 2) access=tunnel ;; 3) access=public-ip ;; 4) access=direct ;; *) access=$REPLY ;; esac
fi
case "$access" in local|tunnel|public-ip|direct) ;; *) problem "--access must be local, tunnel, public-ip or direct." ;; esac
case "$lan_arg" in ''|yes|no) ;; *) problem "--lan must be yes or no." ;; esac
case "$encrypt_arg" in ''|yes|no) ;; *) problem "--encrypt must be yes or no." ;; esac
[ -z "$encrypt_arg" ] || [ "$access" = direct ] || problem "--encrypt needs --access direct."
# Without a domain: HTTPS from a free certificate (ports 443 and 80), or plain HTTP,
# where readers use the HTTP port itself.
encrypt=yes
if [ "$access" = direct ]; then
    encrypt=$encrypt_arg
    if [ -z "$encrypt" ]; then
        ask 'Encrypt the connection with a free certificate (HTTPS)? Needs ports 443 and 80. (yes/no)' yes
        encrypt=$REPLY
    fi
    case "$encrypt" in yes|no) ;; *) problem "--encrypt must be yes or no." ;; esac
fi
# What the choice needs from the host: port 443 (HTTPS), port 80 (certificate checks).
needs_https=no needs_acme=no plain_direct=no
case "$access:$encrypt" in
    public-ip:*) needs_https=yes ;;
    direct:yes) needs_https=yes needs_acme=yes ;;
    direct:no) plain_direct=yes ;;
esac
# The plain HTTP port. Only "local" needs it open: the internet modes are reached
# through Cloudflare or over HTTPS, and Docker publishes a port past ufw, so an open
# one would also answer the internet on a cloud server.
bind=0.0.0.0
lan_default=no
[ "$access" != local ] || lan_default=yes
# Fresh installs use the mode's default; re-runs keep the current bind.
case "$current_bind" in 127.0.0.1) lan_default=no ;; 0.0.0.0) lan_default=yes ;; esac
lan=${lan_arg:-}
if [ "$plain_direct" = yes ]; then
    # Readers on the internet use the HTTP port: it cannot stay on this machine only.
    [ "$lan" != no ] || problem "--lan no does not fit --encrypt no: readers use the HTTP port."
    lan=yes
    warn "Without HTTPS, readers use HTTP port $port. People on the network path can read passwords and pages."
elif [ -z "$lan" ]; then
    ask "Let other devices on your home network connect over HTTP on port $port? (yes/no)" "$lan_default"
    lan=$REPLY
fi
case "$lan" in yes) ;; no) bind=127.0.0.1 ;; *) problem "--lan must be yes or no." ;; esac
compose_files=docker-compose.yml
[ "$needs_https" = no ] || compose_files=$compose_files:docker-compose.https.yml
[ "$needs_acme" = no ] || compose_files=$compose_files:docker-compose.acme.yml

# Ignore inherited Compose settings: this installer manages the stack beside it.
unset COMPOSE_FILE COMPOSE_PATH_SEPARATOR KOMIQ_PORT KOMIQ_BIND

# Existing ports published by this stack are safe on a re-run. Only accept a
# running server's actual host bindings, not arbitrary Docker listeners.
owned_bindings=
server_id=$(docker compose ps -q server 2>/dev/null) || server_id=
if [ -n "$server_id" ]; then
    owned_bindings=$(docker inspect --format '{{if .State.Running}}{{range .NetworkSettings.Ports}}{{range .}}{{println .HostIp .HostPort}}{{end}}{{end}}{{end}}' "$server_id" 2>/dev/null) || owned_bindings=
fi

listening_ports() {
    local output
    if command -v ss >/dev/null 2>&1 && output=$(ss -ltn 2>/dev/null); then
        printf '%s\n' "$output" | awk 'NR > 1 { n=split($4,a,":"); if (a[n] ~ /^[0-9]+$/) print a[n] }'
        return
    fi
    if command -v lsof >/dev/null 2>&1; then
        # lsof returns 1 when no sockets match; an empty list is normal.
        local status=0
        output=$(lsof -nP -iTCP -sTCP:LISTEN -Fn 2>/dev/null) || status=$?
        if [ "$status" -le 1 ]; then
            printf '%s\n' "$output" | awk '/^n/ { n=split($0,a,":"); if (a[n] ~ /^[0-9]+$/) print a[n] }'
            return
        fi
    fi
    if command -v netstat >/dev/null 2>&1 && output=$(netstat -an 2>/dev/null); then
        printf '%s\n' "$output" | awk '/LISTEN/ { n=split($4,a,/[:.]/); if (a[n] ~ /^[0-9]+$/) print a[n] }'
    else
        return 1
    fi
}
listeners=$(listening_ports) || problem "Could not check ports. Install ss (iproute2), lsof or netstat, then try again."
is_taken() { printf '%s\n' "$listeners" | awk -v port="$1" '$0 == port { found=1 } END { exit !found }'; }
check_port() {
    local candidate=$1 host_bind=$2 next
    if is_taken "$candidate"; then
        if printf '%s\n' "$owned_bindings" | awk -v port="$candidate" -v bind="$host_bind" '$1 == bind && $2 == port { found=1 } END { exit !found }'; then
            ok "Port $candidate is already used by this komiq server."
            return
        fi
        if [ "$candidate" = "$port" ]; then
            next=$((port + 1))
            while [ "$next" -le 65535 ]; do
                if ! is_taken "$next"; then
                    case "$needs_https:$needs_acme:$next" in
                        yes:*:443|*:yes:80) ;;
                        *) problem "Port $candidate is in use. Try --port $next." ;;
                    esac
                fi
                next=$((next + 1))
            done
        fi
        problem "Port $candidate is in use. Free it before continuing."
    fi
    ok "Port $candidate is free."
}
if [ "$needs_https" = yes ]; then
    [ "$port" -ne 443 ] || problem "HTTP port 443 conflicts with HTTPS. Choose another --port."
fi
if [ "$needs_acme" = yes ]; then
    [ "$port" -ne 80 ] || problem "HTTP port 80 conflicts with certificate checks. Choose another --port."
fi
check_port "$port" "$bind"
[ "$needs_https" = no ] || check_port 443 0.0.0.0
[ "$needs_acme" = no ] || check_port 80 0.0.0.0

# Back up once, then replace only the three installer keys. Keep TZ and all
# other settings and comments as written by the owner.
if [ -f .env ]; then
    [ -e .env.bak ] || cp -p .env .env.bak
else
    cp .env.example .env
fi
env_tmp=$(mktemp .env.install.XXXXXX)
trap 'rm -f "$env_tmp"' EXIT
cp -p .env "$env_tmp"
env_compose=
[ "$compose_files" = docker-compose.yml ] || env_compose=$compose_files
awk -v port="$port" -v bind="$bind" -v files="$env_compose" '
    /^[[:space:]]*(export[[:space:]]+)?KOMIQ_PORT[[:space:]]*=/ { print "KOMIQ_PORT=" port; p=1; next }
    /^[[:space:]]*(export[[:space:]]+)?KOMIQ_BIND[[:space:]]*=/ { print "KOMIQ_BIND=" bind; b=1; next }
    /^[[:space:]]*(export[[:space:]]+)?COMPOSE_FILE[[:space:]]*=/ { if (files != "") print "COMPOSE_FILE=" files; f=1; next }
    { print }
    END {
        if (!p) print "KOMIQ_PORT=" port
        if (!b) print "KOMIQ_BIND=" bind
        if (!f && files != "") print "COMPOSE_FILE=" files
    }
' .env > "$env_tmp"
mv "$env_tmp" .env
ok "Saved .env (port $port, bind $bind, access $access)."
export COMPOSE_FILE="$compose_files" COMPOSE_PATH_SEPARATOR=:
export KOMIQ_PORT="$port" KOMIQ_BIND="$bind"

firewall_ports=()
[ "$lan" != yes ] || firewall_ports+=("$port")
[ "$needs_https" = no ] || firewall_ports+=(443)
[ "$needs_acme" = no ] || firewall_ports+=(80)
if [ "${#firewall_ports[@]}" -gt 0 ]; then
    if [ "$(uname -s)" = Darwin ]; then
        printf '%s\n' 'Host firewall changes are skipped on macOS.'
    else
        firewall_tool=
        if command -v ufw >/dev/null 2>&1 && ufw_status=$(ufw status 2>/dev/null) && printf '%s\n' "$ufw_status" | awk '/^Status: active/ { active=1 } END { exit !active }'; then
            firewall_tool=ufw
        elif command -v firewall-cmd >/dev/null 2>&1 && [ "$(firewall-cmd --state 2>/dev/null || true)" = running ]; then
            firewall_tool=firewalld
        fi
        if [ -z "$firewall_tool" ]; then
            printf '%s\n' 'No active host firewall was found (ufw or firewalld).'
        else
            open=$firewall
            if [ "$open" = ask ]; then
                printf "Open port(s) %s in this machine's firewall? [Y/n] " "${firewall_ports[*]}"
                REPLY=Y
                if [ "$yes" = yes ]; then
                    printf 'Y\n'
                else
                    [ -t 0 ] || problem "No terminal. Supply --firewall yes|no or --yes."
                    IFS= read -r REPLY || problem "No answer received."
                    REPLY=${REPLY:-Y}
                fi
                case "$REPLY" in Y|y|yes|YES) open=yes ;; N|n|no|NO) open=no ;; *) problem "Answer yes or no, or use --firewall yes|no." ;; esac
            fi
            if [ "$open" = yes ]; then
                command -v sudo >/dev/null 2>&1 || problem "sudo is missing. Open the ports yourself or use --firewall no."
                for firewall_port in "${firewall_ports[@]}"; do
                    if [ "$firewall_tool" = ufw ]; then
                        sudo ufw allow "$firewall_port/tcp"
                    else
                        sudo firewall-cmd --permanent "--add-port=$firewall_port/tcp"
                    fi
                done
                [ "$firewall_tool" != firewalld ] || sudo firewall-cmd --reload
                ok "Opened host firewall ports ${firewall_ports[*]}."
            else
                printf 'Host firewall left as it is. Needed ports: %s.\n' "${firewall_ports[*]}"
            fi
        fi
    fi
else
    printf '%s\n' 'No inbound firewall ports are needed.'
fi
printf '%s\n' "If this is a cloud server, also open the same ports in your provider's firewall (security group / security list)."

printf '%s\n' 'Starting komiq. The first build takes 15 to 30 minutes.'
docker compose up -d --build || problem "Build or start failed. Check: docker compose logs server (from $PWD)."
printf '%s\n' 'Waiting for the server to be ready (up to 5 minutes).'
healthy=no
for ((attempt=0; attempt<60; attempt++)); do
    server_id=$(docker compose ps -q server) || server_id=
    health=
    if [ -n "$server_id" ]; then
        health=$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}missing{{end}}' "$server_id" 2>/dev/null) || health=
    fi
    if [ "$health" = healthy ]; then healthy=yes; break; fi
    sleep 5
done
[ "$healthy" = yes ] || problem "Server did not become healthy. Run: cd '$PWD' && docker compose logs server"
ok "Server is healthy."

# The choices made here, so setup does not ask them again (or get another answer).
setup_choice=(--access "$access")
[ "$access" != direct ] || setup_choice+=(--encrypt "$encrypt")
if [ "$no_setup" = no ]; then
    logs=$(docker compose logs --no-color server) || problem "Could not read the setup log. Run: docker compose logs server"
    code=$(printf '%s\n' "$logs" | awk '/setup code/ { last=$0 } END { print last }' | sed -n 's/.*setup code \([A-Z0-9]\{4\}-[A-Z0-9]\{4\}-[A-Z0-9]\{4\}\).*/\1/p')
    if [ -n "$code" ]; then
        if [ -t 0 ]; then
            docker compose exec server komika-server setup --setup-code "$code" "${setup_choice[@]}" || problem "Setup did not finish. Re-run setup from this folder when ready."
        else
            printf '%s\n' 'Finish setup from a terminal:'
            printf '  cd %q\n' "$PWD"
            printf '  docker compose exec server komika-server setup --setup-code %s %s\n' "$code" "${setup_choice[*]}"
        fi
    else
        printf '%s\n' 'The server is already set up.'
        # The ports follow this run. The server keeps its own access choice.
        [ "$access" = local ] || printf '%s\n' 'To change how people reach it, use Settings > Access in the console, or the setup command below.'
    fi
fi

lan_address=
if [ "$bind" = 127.0.0.1 ]; then
    lan_address=127.0.0.1
elif [ "$(uname -s)" = Darwin ]; then
    lan_address=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)
else
    lan_address=$(hostname -I 2>/dev/null | awk '{ print $1 }') || lan_address=
fi
lan_address=${lan_address:-<this-machine-address>}
case "$lan_address" in *:*) lan_address="[$lan_address]" ;; esac
printf '\nReader: http://%s:%s/\nConsole: http://%s:%s/admin\n' "$lan_address" "$port" "$lan_address" "$port"
case "$access:$encrypt" in
    tunnel:*|public-ip:*) printf '%s\n' 'Internet: use the HTTPS domain chosen in setup, and /admin for the console.' ;;
    direct:yes) printf '%s\n' 'Internet: use https://<public-IP>/ and https://<public-IP>/admin after setup.' ;;
    direct:no) printf 'Internet: use http://<public-IP>:%s/ and http://<public-IP>:%s/admin. This connection is not encrypted.\n' "$port" "$port" ;;
esac
printf 'Change a choice later: cd %q && docker compose exec server komika-server setup\n' "$PWD"
printf '%s\n' 'Update: git pull, then re-run this install.sh.'
printf '%s\n' 'Data stays in Docker volumes: komiq_server-data and komiq_engine-data.'
