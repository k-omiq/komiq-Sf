#!/usr/bin/env bash
# Opt-in: prebuild the self-host image and cache pinned postgres, then run
# bash deploy/selfhost/install.test.sh --real [komiq-selfhost:local].
# No pulls, builds, published ports, production mounts or old server binary.
set -euo pipefail
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
image=${1:-komiq-selfhost:local}
postgres_image=postgres:18.6-bookworm@sha256:afc7e2d441324c0388fa80c3d24f733b4194a4eb7f47dd8ee2b08eb1a24a647c
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
for scenario in new upgrade failure restore; do
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
helper() {
    resource_container --network none --tmpfs /var/lib/postgresql --entrypoint sh "$@"
    docker start -a "$resource_id"
    [ "$(docker inspect --format '{{.State.ExitCode}}' "$resource_id")" -eq 0 ]
}
# Verify actual Compose rejects a missing password rather than substituting a default.
: > "$task_root/empty-env"
if env -u KOMIQ_DB_PASSWORD docker compose --env-file "$task_root/empty-env" -f "$source_dir/docker-compose.yml" config --quiet > "$task_root/missing.log" 2>&1; then
    echo 'FAIL: Compose accepted a missing password' >&2; exit 1
fi
grep -q 'Run install.sh to generate KOMIQ_DB_PASSWORD' "$task_root/missing.log"
python3 "$source_dir/make-sqlite-fixture.py" "$task_root/komika.sqlite3"
# One source row in each of the two tables that the server also writes to, so that the
# upgrade check below compares real source rows there and not an empty set.
python3 - "$task_root/komika.sqlite3" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.execute("INSERT INTO instance_setting(key,value,updated_at) VALUES ('selfhost-test-kept-setting','kept value é','2020-01-01T00:00:00Z')")
    db.execute("INSERT INTO maintenance_flag(key,done_at) VALUES ('selfhost-test-kept-flag','2020-01-01T00:00:00Z')")
PY
cp "$task_root/komika.sqlite3" "$task_root/bad.sqlite3"
python3 - "$task_root/bad.sqlite3" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.execute("UPDATE users SET created_at='not-a-date'")
PY
for scenario in new upgrade failure; do
    project=$prefix-$scenario
    prepare_project
    compose up -d --no-recreate --no-build --pull never --wait --wait-timeout 300 postgres
    # Prove the app role is not privileged, and remote superuser access is rejected.
    role=$(compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" psql -h 127.0.0.1 -U komiq -d komiq -Atqc "SELECT rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication FROM pg_roles WHERE rolname=current_user"')
    [ "$role" = f ]
    if compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" psql -h 127.0.0.1 -U postgres -d postgres -Atqc "SELECT 1"' >/dev/null 2>&1; then echo 'FAIL: remote superuser accepted' >&2; exit 1; fi
    if [ "$scenario" != new ]; then
        fixture=komika.sqlite3
        [ "$scenario" != failure ] || fixture=bad.sqlite3
        helper \
            -v "${project}_server-data:/data" -v "$task_root:/fixture:ro" "$postgres_image" \
            -c "cp /fixture/$fixture /data/komika.sqlite3 && chown 10001:10001 /data /data/komika.sqlite3"
    fi
    compose up -d --no-recreate --no-deps --no-build --pull never server
    ready=no
    for ((attempt=0; attempt<600; attempt++)); do
        id=$(compose ps -aq server)
        state=$(docker inspect --format '{{.State.Status}}' "$id")
        if [ "$state" = exited ]; then break; fi
        if compose exec -T server curl -fsS http://127.0.0.1:8080/health/ready >/dev/null 2>&1; then ready=yes; break; fi
        sleep 1
    done
    if [ "$scenario" = failure ]; then
        [ "$state" = exited ] && [ "$ready" = no ]
        exit_code=$(docker inspect --format '{{.State.ExitCode}}' "$id")
        [ "$exit_code" -ne 0 ]
        compose logs --no-color server > "$task_root/failure.log"
        grep -Ei 'sqlite import:.*(fail|error)' "$task_root/failure.log"
        # Compare untouched source bytes in the volume, using a read-only helper.
        helper \
            -v "${project}_server-data:/data:ro" -v "$task_root:/fixture:ro" "$postgres_image" \
            -c 'cmp /fixture/bad.sqlite3 /data/komika.sqlite3'
    else
        [ "$ready" = yes ] || { compose logs server; exit 1; }
        if [ "$scenario" = upgrade ]; then
            helper \
                -v "${project}_server-data:/data:ro" -v "$task_root:/fixture:ro" "$postgres_image" \
                -c 'test ! -e /data/komika.sqlite3 && set -- /data/komika.sqlite3.imported-* && test "$#" = 1 && cmp /fixture/komika.sqlite3 "$1"'
            # The server verifies every imported table against the source before it serves.
            # To a file first: with pipefail, `grep -q` on a pipe ends the writer early and fails the line.
            compose logs --no-color server > "$task_root/upgrade-server.log" 2>&1
            grep -q 'sqlite import: verifier PASS: ' "$task_root/upgrade-server.log" \
                || { echo 'FAIL: no verifier PASS line' >&2; exit 1; }
            grep 'sqlite import: verifier PASS: ' "$task_root/upgrade-server.log" | cut -c1-200
            # Compare every imported source table's row count, including user data.
            # Two tables also get rows that are not in the source: the import's own repair
            # step and a serving server write there. For them every SOURCE key must be
            # present (for instance_setting with its value), and the keys that were added
            # are printed for the record.
            python3 - "$task_root/komika.sqlite3" "$task_root/extra.sql" > "$task_root/counts.sql" <<'PY'
import sqlite3, sys
SERVER_WRITTEN = {'instance_setting', 'maintenance_flag'}
def literal(value):
    return "'" + str(value).replace("'", "''") + "'"
extra = []
with sqlite3.connect(sys.argv[1]) as db:
    for (name,) in db.execute("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name"):
        if name.startswith(('sqlite_', 'work_fts')) or name == '_sqlx_migrations':
            continue
        count = db.execute(f'SELECT count(*) FROM "{name}"').fetchone()[0]
        if name in SERVER_WRITTEN:
            keys = [key for (key,) in db.execute(f'SELECT key FROM "{name}" ORDER BY key')]
            assert keys, f'the fixture has no source row in {name}'
            listed = ', '.join(literal(key) for key in keys) or "''"
            print(f"SELECT '{name}', (SELECT count(*) FROM \"{name}\" WHERE key IN ({listed})) = {count};")
            if name == 'instance_setting':
                pairs = ', '.join(f'({literal(key)}, {literal(value)})' for key, value in db.execute('SELECT key, value FROM instance_setting ORDER BY key'))
                print(f"SELECT 'instance_setting values', (SELECT count(*) FROM \"instance_setting\" WHERE (key, value) IN ({pairs})) = {count};")
            extra.append(f"SELECT '{name}', key FROM \"{name}\" WHERE key NOT IN ({listed}) ORDER BY key;")
        else:
            print(f"SELECT '{name}', count(*) = {count} FROM \"{name}\";")
with open(sys.argv[2], 'w') as out:
    out.write('\n'.join(extra) + '\n')
PY
            compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec psql -h 127.0.0.1 -U komiq -d komiq -At -v ON_ERROR_STOP=1' < "$task_root/counts.sql" > "$task_root/counts.out"
            if grep -v '|t$' "$task_root/counts.out"; then echo 'FAIL: imported row count' >&2; exit 1; fi
            echo 'rows that are not in the source (table|key):'
            compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec psql -h 127.0.0.1 -U komiq -d komiq -At -v ON_ERROR_STOP=1' < "$task_root/extra.sql"
            compose restart server
            compose up -d --no-recreate --no-deps --no-build --pull never --wait --wait-timeout 300 server
            compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec psql -h 127.0.0.1 -U komiq -d komiq -At -v ON_ERROR_STOP=1' < "$task_root/counts.sql" > "$task_root/counts.out"
            if grep -v '|t$' "$task_root/counts.out"; then echo 'FAIL: rerun changed counts' >&2; exit 1; fi
            compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec pg_dump -h 127.0.0.1 -U komiq -d komiq -Fc' > "$task_root/komiq.dump"
        fi
    fi
    echo "PASS: real $scenario"
done
# Restore the upgrade dump before starting a server in a fourth empty project.
project=$prefix-restore
prepare_project
compose up -d --no-recreate --no-build --pull never --wait --wait-timeout 300 postgres
compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec pg_restore -h 127.0.0.1 -U komiq -d komiq --no-owner --no-acl --exit-on-error' < "$task_root/komiq.dump"
compose up -d --no-recreate --no-deps --no-build --pull never --wait --wait-timeout 300 server
compose exec -T postgres sh -c 'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec psql -h 127.0.0.1 -U komiq -d komiq -At -v ON_ERROR_STOP=1' < "$task_root/counts.sql" > "$task_root/counts.out"
if grep -v '|t$' "$task_root/counts.out"; then echo 'FAIL: restored row count' >&2; exit 1; fi
echo 'PASS: real dump and fresh restore'
