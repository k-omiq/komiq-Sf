#!/bin/sh
# Shared by opt-in Docker runners. Never remove a resource merely by its name.
resource_init() {
    run_id=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    [ "${#run_id}" -eq 32 ] || { echo 'FAIL: random run identifier' >&2; return 1; }
    run_label=cc.komiq.test-run
    resource_root=$1
    mkdir -p "$resource_root"
    : > "$resource_root/container"
    : > "$resource_root/network"
    : > "$resource_root/volume"
}
resource_refuse_name() {
    if docker "$1" inspect "$2" >/dev/null 2>&1; then
        echo "FAIL: resource already exists: $1 $2" >&2
        return 1
    fi
}
resource_record() { printf '%s\n' "$2" >> "$resource_root/$1"; }
resource_verify() {
    if [ "$1" = container ]; then
        owner=$(docker container inspect --format '{{index .Config.Labels "cc.komiq.test-run"}}' "$2" 2>/dev/null) || return 1
    else
        owner=$(docker "$1" inspect --format '{{index .Labels "cc.komiq.test-run"}}' "$2" 2>/dev/null) || return 1
    fi
    [ "$owner" = "$run_id" ] || {
        echo "FAIL: resource ownership mismatch: $1 $2" >&2
        return 1
    }
}
resource_network() {
    resource_refuse_name network "$1" || return 1
    resource_id=$(docker network create --internal --label "$run_label=$run_id" "$1") || return 1
    resource_verify network "$resource_id" || return 1
    resource_record network "$resource_id"
}
resource_volume() {
    resource_refuse_name volume "$1" || return 1
    resource_id=$(docker volume create --label "$run_label=$run_id" "$1") || return 1
    resource_verify volume "$resource_id" || return 1
    resource_record volume "$resource_id"
}
resource_container() {
    # create returns an immutable ID before any application runs.
    resource_id=$(docker create --label "$run_label=$run_id" --pull never "$@") || return 1
    resource_verify container "$resource_id" || return 1
    resource_record container "$resource_id"
}
resource_cleanup() {
    for resource_kind in container network volume; do
        while IFS= read -r resource_id; do
            [ -n "$resource_id" ] || continue
            if [ "$resource_kind" = container ]; then
                owner=$(docker container inspect --format '{{index .Config.Labels "cc.komiq.test-run"}}' "$resource_id" 2>/dev/null) || continue
            else
                owner=$(docker "$resource_kind" inspect --format '{{index .Labels "cc.komiq.test-run"}}' "$resource_id" 2>/dev/null) || continue
            fi
            [ "$owner" = "$run_id" ] || continue
            if [ "$resource_kind" = container ]; then
                docker container rm -f "$resource_id" >/dev/null 2>&1 || true
            else
                docker "$resource_kind" rm "$resource_id" >/dev/null 2>&1 || true
            fi
        done < "$resource_root/$resource_kind"
    done
}
