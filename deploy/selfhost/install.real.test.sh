#!/usr/bin/env bash
# Opt-in: prebuild the self-host image and cache pinned postgres, then run
# bash deploy/selfhost/install.test.sh --real [komiq-selfhost:local].
# No pulls, builds, published ports, production mounts or old server binary.
set -euo pipefail
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
image=${1:-komiq-selfhost:local}
task_root=$(mktemp -d)
source "$source_dir/test-resources.sh"
resource_init "$task_root/resources"
prefix=komiq-phase5b-$run_id
compose() { docker compose --project-name "$project" --env-file "$task_root/env" -f "$source_dir/docker-compose.yml" -f "$task_root/override.yml" -f "$task_root/resources.yml" "$@"; }
cleanup() {
    resource_cleanup
    rm -rf "$task_root"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
umask 077
password=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
printf 'KOMIQ_DB_PASSWORD=%s\nKOMIQ_IMAGE=%s\n' "$password" "$image" > "$task_root/env"
unset password
# Refuse every candidate name before creating any Docker resource.
for scenario in new restore; do
    candidate=$prefix-$scenario
    existing=$(docker ps -aq --filter "label=com.docker.compose.project=$candidate")
    [ -z "$existing" ] || { echo "FAIL: resource already exists: project $candidate" >&2; exit 1; }
    resource_refuse_name network "${candidate}_default"
    for volume in server-data postgres-data engine-data; do
        resource_refuse_name volume "${candidate}_$volume"
    done
    for service in server postgres suwayomi flaresolverr; do
        resource_refuse_name container "$candidate-$service-1"
    done
done
cat > "$task_root/override.yml" <<YAML
services:
  postgres:
    labels:
      cc.komiq.test-run: $run_id
  server:
    labels:
      cc.komiq.test-run: $run_id
    ports: !reset []
    depends_on: !reset {}
    restart: 'no'
    environment:
      CATALOGUE_SYNC: 'off'
      KOMIKA_MANGADEX: 'off'
      SUWAYOMI_URL: http://127.0.0.1:9
YAML
prepare_project() {
    resource_network "${project}_default"
    for volume in server-data postgres-data engine-data; do
        resource_volume "${project}_$volume"
    done
    cat > "$task_root/resources.yml" <<YAML
networks:
  default:
    external: true
    name: ${project}_default
volumes:
  server-data:
    external: true
    name: ${project}_server-data
  postgres-data:
    external: true
    name: ${project}_postgres-data
  engine-data:
    external: true
    name: ${project}_engine-data
YAML
    # Capture even partial Compose creation, before starting either container.
    create_status=0
    compose create --no-build --pull never postgres server || create_status=$?
    for service in postgres server; do
        id=$(compose ps -aq "$service")
        if [ -n "$id" ]; then
            resource_verify container "$id" || return 1
            resource_record container "$id"
        fi
    done
    [ "$create_status" = 0 ]
}
# Verify actual Compose rejects a missing password rather than substituting a default.
: > "$task_root/empty-env"
if env -u KOMIQ_DB_PASSWORD docker compose --env-file "$task_root/empty-env" -f "$source_dir/docker-compose.yml" config --quiet > "$task_root/missing.log" 2>&1; then
    echo 'FAIL: Compose accepted a missing password' >&2; exit 1
fi
grep -q 'Run install.sh to generate KOMIQ_DB_PASSWORD' "$task_root/missing.log"
for scenario in new; do
    project=$prefix-$scenario
    prepare_project
    compose up -d --no-recreate --no-build --pull never --wait --wait-timeout 300 postgres
    # Prove the app role is not privileged, and remote superuser access is rejected.
    role=$(compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" psql -h 127.0.0.1 -U komiq -d komiq -Atqc "SELECT rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication FROM pg_roles WHERE rolname=current_user"')
    [ "$role" = f ]
    if compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" psql -h 127.0.0.1 -U postgres -d postgres -Atqc "SELECT 1"' >/dev/null 2>&1; then echo 'FAIL: remote superuser accepted' >&2; exit 1; fi
    compose up -d --no-recreate --no-deps --no-build --pull never server
    ready=no
    for ((attempt=0; attempt<600; attempt++)); do
        id=$(compose ps -aq server)
        state=$(docker inspect --format '{{.State.Status}}' "$id")
        if [ "$state" = exited ]; then break; fi
        if compose exec -T server curl -fsS http://127.0.0.1:8080/health/ready >/dev/null 2>&1; then ready=yes; break; fi
        sleep 1
    done
    [ "$ready" = yes ] || { compose logs server; exit 1; }
    compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec psql -h 127.0.0.1 -U komiq -d komiq -v ON_ERROR_STOP=1' <<'SQL'
INSERT INTO instance_setting(key, value, updated_at)
VALUES ('selfhost-test-kept-setting', 'kept value é', '2020-01-01T00:00:00Z');
SQL
    cat > "$task_root/counts.sql" <<'SQL'
SELECT 'instance_setting', count(*) = 1 FROM instance_setting
WHERE key = 'selfhost-test-kept-setting' AND value = 'kept value é';
SQL
    compose restart server
    compose up -d --no-recreate --no-deps --no-build --pull never --wait --wait-timeout 300 server
    compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec psql -h 127.0.0.1 -U komiq -d komiq -At -v ON_ERROR_STOP=1' < "$task_root/counts.sql" > "$task_root/counts.out"
    if grep -v '|t$' "$task_root/counts.out"; then echo 'FAIL: restart changed settings' >&2; exit 1; fi
    compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec pg_dump -h 127.0.0.1 -U komiq -d komiq -Fc' > "$task_root/komiq.dump"
    echo "PASS: real $scenario"
done
# Restore the dump before starting a server in another empty project.
project=$prefix-restore
prepare_project
compose up -d --no-recreate --no-build --pull never --wait --wait-timeout 300 postgres
compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec pg_restore -h 127.0.0.1 -U komiq -d komiq --no-owner --no-acl --exit-on-error' < "$task_root/komiq.dump"
compose up -d --no-recreate --no-deps --no-build --pull never --wait --wait-timeout 300 server
compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec psql -h 127.0.0.1 -U komiq -d komiq -At -v ON_ERROR_STOP=1' < "$task_root/counts.sql" > "$task_root/counts.out"
if grep -v '|t$' "$task_root/counts.out"; then echo 'FAIL: restored row count' >&2; exit 1; fi
echo 'PASS: real dump and fresh restore'
