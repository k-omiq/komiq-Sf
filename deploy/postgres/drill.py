#!/usr/bin/env python3
"""Restore into a caller-owned empty host directory; never mount the live volume.

Run directly (restore.sh, pitr-drill.sh) this file serves ONLY the hosted
recovery path and requires --hosted-recovery. A rehearsal reaches these
functions through rehearsal.sh alone, which sets REHEARSAL to a validated
safety.Rehearsal and passes already checked values, never raw arguments.
"""
import argparse
import datetime
import json
import math
import os
from pathlib import Path
import re
import subprocess
import signal
import time
import uuid
import safety

STATISTICS = []
HOSTED_RECOVERY = False
REHEARSAL = None  # Set only by rehearsal.py.
MARKER_DATABASE = "komika_pitr_drill"

KIT = Path(__file__).resolve().parent
RECOVERY_SETTINGS = {
    "max_connections": ("max_connections setting", 48),
    "max_worker_processes": ("max_worker_processes setting", 8),
    "max_wal_senders": ("max_wal_senders setting", 10),
    "max_prepared_transactions": ("max_prepared_xacts setting", 0),
    "max_locks_per_transaction": ("max_locks_per_xact setting", 64),
}


def compose_file():
    return Path(os.environ.get("KOMIKA_COMPOSE_FILE", KIT.parent / "docker-compose.yml")).resolve()


def hosted_only():
    """The direct entry points never select a rehearsal, whatever the environment."""
    if REHEARSAL is not None or safety.rehearsal_present() or compose_file() == safety.COMPOSE_FILE:
        raise ValueError("rehearsal mode is reachable only through rehearsal.sh")
    if not HOSTED_RECOVERY:
        raise ValueError("hosted marker drills/restores require explicit --hosted-recovery")


def compose():
    hosted_only()
    args = ["docker", "compose", "--env-file",
            os.environ.get("KOMIKA_COMPOSE_ENV_FILE", str(KIT.parent / ".env")),
            "-f", str(compose_file())]
    project = os.environ.get("KOMIKA_COMPOSE_PROJECT")
    if project:
        args.extend(["-p", project])
    return args


def guard(project, environment):
    if safety.rehearsal_present(environment):
        raise ValueError("rehearsal variables cannot select the hosted Compose file")
    hosted_only()


def child_environment():
    # A rehearsal talks only to the checked local socket with a minimal environment.
    return REHEARSAL.child_env if REHEARSAL is not None else None


def command(args, *, input=None, timeout=None, explain=False):
    # Do not print argv or stderr on errors: docker inspect carries environment.
    result = subprocess.run(args, input=input, text=True, capture_output=True, timeout=timeout,
                            env=child_environment())
    STATISTICS.extend(line for line in result.stderr.splitlines() if "statistics:" in line)
    if result.returncode:
        reason = ""
        if explain:
            # Only for a pgBackRest call: its own ERROR lines (it redacts keys;
            # they are the only record, the throwaway container is removed).
            errors = [line.strip()[:300] for line in result.stderr.splitlines()
                      if re.match(r"(?:P00 +)?ERROR: ", line.strip())]
            reason = "".join(f"\n  pgBackRest {line}" for line in errors[:5])
        raise RuntimeError(f"{args[0]} operation failed (exit {result.returncode}); inspect container logs{reason}")
    return result.stdout


def live_prefix():
    if REHEARSAL is not None:
        # Label and contract are rechecked before every exec; exec is by ID.
        return ["docker", "exec", "--user", "postgres", REHEARSAL.container("postgres", running=True)["Id"]]
    return [*compose(), "exec", "-T", "--user", "postgres", "postgres"]


def live(*args):
    return command([*live_prefix(), *args])


def psql(database):
    return ["psql", "-X", "-q", "-w", "-U", "postgres", "-d", database, "-v", "ON_ERROR_STOP=1", "-Atc"]


def sql(query, database="komika"):
    return live(*psql(database), query).strip()


def marker_database():
    """Markers live in their OWN database, never in the application database.

    The importer requires an empty target and the verifier fingerprints every
    schema of `komika`; one extra table there makes both refuse. Point-in-time
    recovery is cluster-wide, so a marker in another database proves the same
    boundary. Only the postgres OS user can reach this database (peer socket).
    """
    if sql(f"SELECT count(*) FROM pg_database WHERE datname = '{MARKER_DATABASE}'") != "1":
        sql(f"CREATE DATABASE {MARKER_DATABASE} TEMPLATE template0", database="postgres")
    sql("CREATE TABLE IF NOT EXISTS markers (drill text, step text, at timestamptz DEFAULT clock_timestamp(), PRIMARY KEY(drill,step)); "
        "CREATE TABLE IF NOT EXISTS operator_markers (label text PRIMARY KEY, at timestamptz NOT NULL DEFAULT clock_timestamp())",
        database=MARKER_DATABASE)


def empty_target(path, require_new=False):
    if not path.is_absolute() or path.is_symlink():
        raise ValueError("target must be an absolute, non-symlink directory")
    if path.exists() and (not path.is_dir() or any(path.iterdir())):
        raise ValueError("target must be empty; it will never be deleted by this script")
    if path.exists() and path.stat().st_uid != os.getuid():
        raise ValueError("existing target must be owned by the caller")
    path.mkdir(parents=False, exist_ok=not require_new, mode=0o700)
    return path.resolve()


def docker_inventory():
    docker_root = json.loads(command(["docker", "info", "--format", "{{json .DockerRootDir}}"]))
    ids = command(["docker", "ps", "-a", "-q"]).split()
    containers = json.loads(command(["docker", "inspect", *ids])) if ids else []
    volumes = command(["docker", "volume", "ls", "-q"]).split()
    volumes = json.loads(command(["docker", "volume", "inspect", *volumes])) if volumes else []
    return docker_root, containers, volumes


def fenced_target(path):
    """Inspect every container/volume, including stopped and orphaned resources."""
    if REHEARSAL is not None:
        # A new name directly under the marker-carrying, identity-fenced root.
        return REHEARSAL.new_path(str(path), "restore target")
    if not path.is_absolute() or path.exists() or path.is_symlink():
        raise ValueError("restore target must be a NEW absolute directory")
    target = path.resolve()
    if not target.parent.is_dir():
        raise ValueError("the parent of the restore target must exist")
    # Same path, inode and mount-table fence as a rehearsal, for the parent.
    safety.host_fence(target.parent, safety.protected_paths(*docker_inventory()), uid=os.getuid(),
                      require_owner=False, require_marker=False, plain_parent=True)
    return target


def wait_archive(segment):
    if len(segment) != 24 or any(c not in "0123456789ABCDEF" for c in segment):
        raise ValueError("unexpected WAL segment name")
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        args = [*live_prefix(), "sh", "-c", 'test -f "$PGDATA/pg_wal/archive_status/$1.done"',
                "sh", segment]
        result = subprocess.run(args, capture_output=True, timeout=max(0.01, deadline - time.monotonic()),
                                env=child_environment())
        if result.returncode == 0:
            # .done alone can also mean pgBackRest dropped WAL! Prove retrieval.
            live("/opt/komika-postgres/pgbackrest.sh", "--archive-async=n", "archive-get",
                 segment, f"/run/komika/drill-{segment}")
            live("rm", f"/run/komika/drill-{segment}")
            return
        time.sleep(1)
    raise RuntimeError("WAL archive was not acknowledged within 300 seconds")


def source(restore_only=False):
    if REHEARSAL is not None:
        # Endpoint, pinned file, effective topology and labels were all checked
        # when REHEARSAL was built; image and environment come from that contract.
        config = {"Config": {"Env": [f"{k}={v}" for k, v in REHEARSAL.environment.items() if v]},
                  "Image": safety.IMAGE, "Mounts": []}
        entry = REHEARSAL.container("postgres", running=not restore_only)
        if entry:
            config["Mounts"] = entry.get("Mounts") or []
        if not restore_only:
            config["RecoverySettings"] = {key: int(sql("SHOW " + key)) for key in RECOVERY_SETTINGS}
        return config
    # Host bind paths and retained bytes are meaningful only with a local daemon.
    # Respect Docker context precedence so an inherited context cannot bypass
    # the local-daemon fence with a misleading DOCKER_HOST.
    endpoint = None if os.environ.get("DOCKER_CONTEXT") else os.environ.get("DOCKER_HOST")
    if not endpoint:
        context = command(["docker", "context", "show"]).strip()
        endpoint = json.loads(command(["docker", "context", "inspect", context]))[0]["Endpoints"]["docker"]["Host"]
    if not endpoint.startswith("unix://"):
        raise ValueError("local Docker daemon required")
    # config is read via Compose; credentials are captured in memory, never
    # printed. Restore-only needs neither the source container nor its volume.
    selected = json.loads(command([*compose(), "config", "--format=json"]))
    service = selected["services"]["postgres"]
    environment = service["environment"]
    guard(selected["name"], environment)
    if restore_only:
        mounts = [{"Source": v["source"]} for v in service.get("volumes", []) if v["type"] == "bind"]
        identity = command([*compose(), "ps", "-a", "-q", "postgres"]).strip()
        if identity:
            if "\n" in identity:
                raise ValueError("multiple source postgres containers")
            mounts.extend(json.loads(command(["docker", "inspect", identity]))[0].get("Mounts", []))
        return {"Config": {"Env": [f"{k}={v}" for k, v in environment.items() if v is not None]},
                "Image": service["image"], "Mounts": mounts}
    identity = command([*compose(), "ps", "-q", "postgres"]).strip()
    if not identity or "\n" in identity:
        raise ValueError("exactly one running postgres service required")
    config = json.loads(command(["docker", "inspect", identity]))[0]
    if not config["State"]["Running"]:
        raise ValueError("postgres service is not running")
    guard(selected["name"], dict(item.split("=", 1) for item in config["Config"]["Env"] if "=" in item))
    config["RecoverySettings"] = {key: int(sql("SHOW " + key)) for key in RECOVERY_SETTINGS}
    return config


def restore_environment(config):
    environment = dict(item.split("=", 1) for item in config["Config"]["Env"] if "=" in item)
    for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET"):
        if not environment.get(key):
            raise ValueError("restore requires complete R2 configuration")
    if any("\n" in v or "\r" in v for v in environment.values()):
        raise ValueError("multiline Docker environment is unsupported")
    if safety.rehearsal_present(environment):
        safety.environment_fence(environment)
    return "\n".join(f"{key}={value}" for key, value in environment.items()
                     if key.startswith("KOMIKA_R2_") or key in ("KOMIKA_POSTGRES_PASSWORD",
                         "KOMIKA_REHEARSAL_PROJECT", "KOMIKA_PRODUCTION_R2_PREFIX")) + "\n"


def restore_limits():
    """Validated memory/CPU/PID limits; they become single docker arguments."""
    if REHEARSAL is not None:
        return {name: safety.limit(REHEARSAL.values, "KOMIKA_RESTORE_", name, safety.RESTORE_LIMITS)[0]
                for name in safety.RESTORE_LIMITS}
    limits = {}
    for name, default, pattern in (("MEMORY", "1g", r"[1-9][0-9]*[kmg]?"), ("CPUS", "1", r"[0-9]+(\.[0-9]+)?"),
                                   ("PIDS", "128", r"[1-9][0-9]*")):
        limits[name] = os.environ.get("KOMIKA_RESTORE_" + name, default)
        if not re.fullmatch(pattern, limits[name]):
            raise ValueError("invalid KOMIKA_RESTORE_" + name)
    return limits


def throwaway_arguments(config, *recipe, name=None):
    """A fixed recipe in a short-lived container that has repository access only.

    Fresh tmpfs storage only: never mount/recreate a source volume just to read
    repository metadata, and mount nothing from the host. A rehearsal reader
    joins only the rehearsal's own labelled bridge (never Docker's default
    bridge) and carries a name of the project's job pattern, so `status`,
    `stop-source` and `down` can find one that an interrupted client left
    behind. Hosted recovery uses the default bridge for R2 egress. The caller
    supplies restore_environment(config) on stdin.
    """
    environment = dict(item.split("=", 1) for item in config["Config"]["Env"] if "=" in item)
    project = environment.get("KOMIKA_REHEARSAL_PROJECT")
    identity = []
    network = "--network=bridge"
    if REHEARSAL is not None:
        if project != REHEARSAL.project or not name:
            raise ValueError("rehearsal metadata reader without its project identity")
        identity = ["--label", f"{safety.LABEL}={project}", "--name", name]
        network = "--network=" + REHEARSAL.repository_network()
    elif project:
        raise ValueError("rehearsal mode is reachable only through rehearsal.sh")
    limits = restore_limits()
    return ["docker", "run", *identity, "--rm", "--init", "--pull=never", network,
            "--memory=512m", "--memory-swap=512m",
            "--cpus=" + limits["CPUS"], "--pids-limit=" + limits["PIDS"], *safety.LOG_OPTIONS,
            "--env-file", "/dev/stdin", "--tmpfs", "/run/komika:mode=0700",
            "--tmpfs", "/var/lib/postgresql:mode=0700",
            config["Image"], "exec", *recipe]


def throwaway(config, *recipe, stream=False):
    """Run the reader; with stream=True its output goes to the terminal and the
    exit status is returned instead of the captured output."""
    name = REHEARSAL.transient_name("meta") if REHEARSAL is not None else None
    args = throwaway_arguments(config, *recipe, name=name)
    interrupted = True
    try:
        if stream:
            status = subprocess.run(args, input=restore_environment(config), text=True,
                                    env=child_environment()).returncode
            interrupted = False
            return status
        try:
            output = command(args, input=restore_environment(config))
        except RuntimeError:
            interrupted = False  # The reader ran and failed: nothing is on its way.
            raise
        interrupted = False
        return output
    finally:
        if name:
            REHEARSAL.remove_transient(name, settle=interrupted)


def repository(*args, config=None, restore_only=False):
    if restore_only:
        return throwaway(config, "/opt/komika-postgres/pgbackrest.sh", *args)
    return live("/opt/komika-postgres/pgbackrest.sh", *args)


def recovery_options(control, source_settings=None):
    options = []
    for key, (label, minimum) in RECOVERY_SETTINGS.items():
        match = re.search(r"^" + re.escape(label) + r":\s*(\d+)\s*$", control, re.MULTILINE)
        if not match:
            raise ValueError("pg_controldata omitted a recovery-relevant setting")
        value = max(minimum, int(match[1]), int((source_settings or {}).get(key, minimum)))
        options.extend(["-c", f"{key}={value}"])
    return " ".join(options)


TARGET_NOT_REACHED = "recovery ended before configured recovery target was reached"


def server_reason(log):
    """Why the restored server gave up, from its own log; never a guess."""
    fatal = [line.strip() for line in log.splitlines() if re.search(r"\b(?:FATAL|PANIC): ", line)]
    reason = fatal[-1][:300] if fatal else "its log has no FATAL line; read drill-postgres.log in the restored directory"
    if TARGET_NOT_REACHED in reason:
        reason += (". The target time is later than the last commit in the archived WAL: choose an earlier "
                   "--target-time, or omit it to restore the latest state")
    return reason


def wait_recovery(execute, query, timeout=1800, stopped=None):
    """Poll until recovery has ended. `stopped` returns the reason when the
    server is no longer running: it will never answer, so do not wait for it."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            answer = execute(*query, "SELECT NOT pg_is_in_recovery()",
                             timeout=remaining).strip()
            if time.monotonic() >= deadline:
                break
            if answer == "t":
                return
        except subprocess.TimeoutExpired:
            break
        except RuntimeError:
            # Readiness can precede recovery; retry temporary connection failures.
            # A server that stopped (found by the first wrong-time restore: the
            # target lay beyond the archived WAL) used to be polled for 1,800 s.
            reason = stopped() if stopped else None
            if reason:
                raise RuntimeError("the restored server stopped during recovery: " + reason)
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    raise RuntimeError("restore did not finish recovery/promote within the bounded wait")


def check_expectations(markers, expect):
    """The restored operator markers must match what the caller knows about T."""
    present, absent = expect
    missing = [label for label in present if label not in markers]
    unexpected = [label for label in absent if label in markers]
    if missing or unexpected:
        raise RuntimeError("marker expectation failed: restored operator_markers=" + (",".join(markers) or "(none)")
                           + (f"; expected but missing: {','.join(missing)}" if missing else "")
                           + (f"; must be absent but present: {','.join(unexpected)}" if unexpected else "")
                           + ". The restore did not stop at the intended point in time")
    if present or absent:
        print("marker expectations met: present=" + (",".join(present) or "-") + " absent=" + (",".join(absent) or "-"))


def target_instant(timestamp):
    # Fractions of 1..6 digits, on every Python version.
    text = re.sub(r"\.(\d{1,6})(?=[Z+-])", lambda m: "." + m[1].ljust(6, "0"), timestamp).replace("Z", "+00:00")
    parsed = datetime.datetime.fromisoformat(text)
    if parsed.tzinfo is None:
        raise ValueError("target time must include UTC offset")
    return parsed.astimezone(datetime.timezone.utc)


def recovery_target(timestamp):
    """The recovery_target_time that PostgreSQL accepts when it starts.

    PostgreSQL checks this setting before its time zone abbreviations are loaded,
    so `...Z` ends the start with `invalid value for parameter
    "recovery_target_time"` (found by the first drill against a repository). A
    numeric offset is always accepted: the same instant, written in UTC.
    """
    return target_instant(timestamp).strftime("%Y-%m-%d %H:%M:%S.%f+00")


def restore(config, target, backup, timestamp, keep, marker=None, expect=((), ())):
    name = "komika-pitr-" + uuid.uuid4().hex[:12]
    network = name + "-network"
    # Send credentials to Docker's env-file reader over stdin, NEVER in argv/file.
    subnet = safety.RESTORE_SUBNET
    env_text = restore_environment(config) + f"KOMIKA_PG_ALLOWED_CIDRS={subnet}\n"
    created = False
    network_created = False
    leave_running = False
    environment = dict(item.split("=", 1) for item in config["Config"]["Env"] if "=" in item)
    project = environment.get("KOMIKA_REHEARSAL_PROJECT")
    # The target label lets restore-cleanup and verify-restored find exactly this
    # container and network from the directory alone; no name is ever typed.
    labels = ["--label", f"{safety.LABEL}={project}", "--label", f"{safety.TARGET_LABEL}={target}"] if project else []
    if project:
        safety.environment_fence(environment)
    limits = restore_limits()
    markers = []
    if not re.fullmatch(safety.PATH_PATTERN, str(target)):
        raise ValueError("restore target path has characters that are unsafe in a mount specification")
    def execute(*args, timeout=None):
        return command(["docker", "exec", "--user", "postgres", name, *args], timeout=timeout)

    def stopped():
        """The server's own last FATAL line once it is no longer running, else None."""
        # pg_ctl status: 0 running, 3 not running; anything else proves nothing.
        status = subprocess.run(["docker", "exec", "--user", "postgres", name, "pg_ctl", "-D", "/restore", "status"],
                                capture_output=True, env=child_environment())
        if status.returncode != 3:
            return None
        return server_reason(execute("tail", "-n", "60", "/restore/drill-postgres.log"))
    try:
        # Private throwaway bridge needs outbound HTTPS for R2, never production
        # membership. Keep the exact network when --keep is requested.
        command(["docker", "network", "create", *labels, "--driver", "bridge", "--subnet", subnet, network])
        network_created = True
        memory = limits["MEMORY"]
        command(["docker", "create", *labels, "--name", name, "--init", "--pull=never",
                 f"--memory={memory}", f"--memory-swap={memory}",
                 "--cpus=" + limits["CPUS"], "--pids-limit=" + limits["PIDS"],
                 "--shm-size=256m", "--network", network, *safety.LOG_OPTIONS,
                 "--env-file", "/dev/stdin", "-e", "PGDATA=/restore",
                 "--mount", f"type=bind,src={target},dst=/restore",
                 "--tmpfs", "/run/komika:mode=0700", "--tmpfs", "/var/lib/postgresql:mode=0700",
                 config["Image"], "exec", "sleep", "infinity"], input=env_text)
        created = True
        command(["docker", "start", name])
        # Entrypoint owns only fresh anonymous storage and its tmpfs, not live PGDATA.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            ready = subprocess.run(["docker", "exec", name, "test", "-s", "/run/komika/pgbackrest.conf"],
                                   capture_output=True, env=child_environment())
            if ready.returncode == 0:
                break
            time.sleep(0.2)
        else:
            raise RuntimeError("throwaway configuration was not generated")
        command(["docker", "exec", "--user", "root", name, "chown", "postgres:postgres", "/restore"])
        command(["docker", "exec", "--user", "root", name, "chmod", "700", "/restore"])
        args = ["/opt/komika-postgres/pgbackrest.sh", "--log-level-stderr=detail", f"--set={backup}"]
        if timestamp:
            args.extend(["--type=time", f"--target={recovery_target(timestamp)}", "--target-action=promote"])
        before = time.monotonic()
        output = command(["docker", "exec", "--user", "postgres", name, *args, "restore"], explain=True)
        restore_seconds = time.monotonic() - before
        # Keep generated recovery settings; only disable uploads and bound resources.
        control = execute("env", "LC_ALL=C", "pg_controldata", "/restore")
        ceilings = recovery_options(control, config.get("RecoverySettings"))
        # Restore's HBA belongs to the source bridge; regenerate only HBA for
        # this isolated private subnet. Keep target/timeline/restore_command.
        execute("python3", "-c", "import sys; sys.path.insert(0, '/opt/komika-postgres'); import hosted; hosted.hba()")
        options = "-c config_file=/opt/komika-postgres/postgresql.conf -c hba_file=/restore/pg_hba.conf -c archive_mode=off -c shared_buffers=256MB " + ceilings
        before = time.monotonic()
        try:
            execute("pg_ctl", "-D", "/restore", "-l", "/restore/drill-postgres.log", "-w", "-t", "1800", "-o", options, "start")
        except RuntimeError:
            # The container is removed below: say now what the server itself said.
            raise RuntimeError("the restored server did not start: " + (stopped() or "pg_ctl start failed"))
        query = psql("komika")
        wait_recovery(execute, query, stopped=stopped)
        recovery_seconds = time.monotonic() - before
        authenticated = execute("sh", "-c", 'PGPASSWORD="$KOMIKA_POSTGRES_PASSWORD" psql -X -w -h "$HOSTNAME" -U komika -d komika -Atc "SELECT current_user"').strip()
        if authenticated != "komika":
            raise RuntimeError("restored application authentication failed")
        if marker:
            found = execute(*psql(MARKER_DATABASE), f"SELECT step FROM markers WHERE drill='{marker}' ORDER BY step").strip()
            if found != "first":
                raise RuntimeError("PITR marker verification failed: expected only first marker")
            print("markers: first present, second absent")
        if execute(*query, f"SELECT count(*) FROM pg_database WHERE datname = '{MARKER_DATABASE}'").strip() == "1":
            # Labels written by `rehearsal.sh sql-marker`; the caller knows which
            # ones were committed before and after the requested target time.
            markers = execute(*psql(MARKER_DATABASE), "SELECT label FROM operator_markers ORDER BY at, label").split()
            print("operator_markers=" + ",".join(markers))
        check_expectations(markers, expect)
        try:
            counts = execute(*query, "SELECT 'work=' || count(*) FROM work UNION ALL SELECT 'chapter=' || count(*) FROM chapter UNION ALL SELECT 'users=' || count(*) FROM users")
        except RuntimeError:
            # A backup taken before any import restores an empty application database.
            raise RuntimeError("the restored database `komika` has no application tables (work, chapter, users): "
                               "the backup was taken before the import; back up after `import` and restore that set")
        print(counts.strip())
        size = int(execute("du", "-sb", "/restore").split()[0])
        print(f"backup_set={backup} restore_seconds={restore_seconds:.3f} recovery_seconds={recovery_seconds:.3f} restored_bytes={size}")
        statistics = STATISTICS + [line for line in output.splitlines() if "statistics:" in line]
        print("\n".join(statistics) if statistics else "pgBackRest request counts: unavailable; use R2 account metrics")
        if keep:
            leave_running = True
            print(f"throwaway_container={name} throwaway_network={network} (no published port; archiving off)")
        else:
            execute("pg_ctl", "-D", "/restore", "-m", "fast", "-w", "stop")
    finally:
        if not leave_running:
            try:
                if created:
                    # Exact owned container only. The target bind directory is retained.
                    if project:
                        safety.require_label("container", name, project, command)
                    command(["docker", "rm", "-f", "-v", name])
            finally:
                if network_created:
                    if project:
                        safety.require_label("network", network, project, command)
                    command(["docker", "network", "rm", network])
    return {"restore_seconds": restore_seconds, "recovery_seconds": recovery_seconds,
            "restored_bytes": size, "restored_directory": str(target), "operator_markers": markers,
            "throwaway_container": name if keep else None, "throwaway_network": network if keep else None}


def validate_settings(settings):
    if not isinstance(settings, dict) or set(settings) != set(RECOVERY_SETTINGS):
        raise ValueError("source settings must contain exactly the five recovery ceilings")
    for key, value in settings.items():
        if type(value) is not int or not 0 <= value <= 2147483647 or (key != "max_prepared_transactions" and value == 0):
            raise ValueError("source settings must be valid nonnegative PostgreSQL integer ceilings")
    return settings


def load_settings(path):
    record = json.loads(path.read_text())
    settings = validate_settings(record["settings"])
    # Validate the literal control capture too; do not trust an incomplete record.
    options = recovery_options(record["pg_controldata"], settings)
    return {key: int(re.search(r"\b" + key + r"=(\d+)", options)[1]) for key in RECOVERY_SETTINGS}


# pgBackRest sends an S3 object larger than one part as a multipart upload. The
# kit does not set repo-storage-upload-chunk-size, so the part size is
# pgBackRest's S3 default of 5 MiB.
UPLOAD_CHUNK = 5 << 20
WAL_FILE = r"[0-9A-F]{24}-[0-9a-f]{40}(?:\.[a-z0-9]+)?"


def usage(listing):
    """What a `repo-ls --recurse --output=json` listing of the prefix holds now.

    Bytes and objects in total, below archive/ (WAL) and below backup/. An
    object count is not a request count: `write_requests_floor` is the least
    number of class-A writes that the objects present now have cost (one PUT
    for an object of at most one part, else one request per part plus the two
    that open and complete the multipart upload). Objects that were replaced
    or removed since, and every list request, come on top; the provider's
    account metrics are the only record of those.
    """
    if isinstance(listing, dict):
        entries = listing.items()
    elif isinstance(listing, list):
        entries = [(entry.get("name"), entry) for entry in listing if isinstance(entry, dict)]
    else:
        raise ValueError("the repository listing is not a JSON object")
    files = {str(path): entry["size"] for path, entry in entries
             if isinstance(entry, dict) and entry.get("type") == "file" and type(entry.get("size")) is int
             and entry["size"] >= 0}
    def below(top):
        return [size for path, size in files.items() if path.startswith(top + "/")]
    archive, backup = below("archive"), below("backup")
    return {
        "objects": len(files), "bytes": sum(files.values()),
        "archive_objects": len(archive), "archive_bytes": sum(archive),
        "backup_objects": len(backup), "backup_bytes": sum(backup),
        "wal_segments": sum(1 for path in files if path.startswith("archive/")
                            and re.fullmatch(WAL_FILE, path.rsplit("/", 1)[-1])),
        "multipart_objects": sum(1 for size in files.values() if size > UPLOAD_CHUNK),
        "write_requests_floor": sum(1 if size <= UPLOAD_CHUNK else math.ceil(size / UPLOAD_CHUNK) + 2
                                    for size in files.values()),
    }


def report(info, backup, timings, listing=None):
    # info describes stored BACKUPS, not all prefix objects/WAL. 2.59 can omit
    # repository.size; delta is the physical contribution of each chain member.
    sizes = [b.get("info", {}).get("repository", {}).get("delta") for b in info["backup"]]
    repository_size = sum(sizes) if all(type(size) is int for size in sizes) else None
    archives = info.get("archive", [])

    def number(value):
        return value if type(value) is int else None

    # The budget record: every backup of the chain with its logical (database)
    # and repository sizes, and what the whole prefix holds at this moment.
    backups = [{"label": b.get("label"), "type": b.get("type"), "prior": b.get("prior"),
                "timestamp_start": (b.get("timestamp") or {}).get("start"),
                "timestamp_stop": (b.get("timestamp") or {}).get("stop"),
                "wal_start": (b.get("archive") or {}).get("start"), "wal_stop": (b.get("archive") or {}).get("stop"),
                "database_size_bytes": number((b.get("info") or {}).get("size")),
                "database_delta_bytes": number((b.get("info") or {}).get("delta")),
                "repository_size_bytes": number(((b.get("info") or {}).get("repository") or {}).get("size")),
                "repository_delta_bytes": number(((b.get("info") or {}).get("repository") or {}).get("delta"))}
               for b in info["backup"]]
    return {"backup_label": backup["label"], "backup_type": backup["type"], **timings,
            "repository_size_bytes": repository_size,
            "wal_archive_min": min((a["min"] for a in archives if a.get("min")), default=None),
            "wal_archive_max": max((a["max"] for a in archives if a.get("max")), default=None),
            "backups": backups,
            # WAL archived between wal_archive_min and wal_archive_max, and the whole prefix.
            "repository_usage": usage(listing) if listing is not None else None}


def record_settings(out):
    """Retain the running source's recovery ceilings before its volume is removed."""
    if out.exists() or out.is_symlink():
        raise ValueError("settings output path must be new")
    config = source()
    control = live("sh", "-c", 'LC_ALL=C pg_controldata "$PGDATA"')
    settings = validate_settings(config["RecoverySettings"])
    recovery_options(control, settings)
    with out.open("x") as output:
        output.write(json.dumps({"settings": settings, "pg_controldata": control}, sort_keys=True) + "\n")


DROPPED_RECORD = "/var/lib/postgresql/komika-backup/dropped-wal.log"


def dropped_wal_events():
    """Times of the source's recorded dropped-WAL warnings (pgbackrest.sh), or
    None when the record cannot be read: its postgres container is not running."""
    script = 'test -d "${1%/*}" || exit 3; test -f "$1" || exit 0; cat -- "$1"'
    try:
        result = subprocess.run([*live_prefix(), "sh", "-c", script, "sh", DROPPED_RECORD],
                                capture_output=True, text=True, timeout=60, env=child_environment())
    except (OSError, subprocess.TimeoutExpired, ValueError, RuntimeError):
        return None
    if result.returncode:
        return None
    events = []
    for line in result.stdout.splitlines():
        # Microseconds since the fix; older lines have whole seconds.
        for layout in ("%Y-%m-%dT%H:%M:%S.%fZ", "%Y-%m-%dT%H:%M:%SZ"):
            try:
                events.append(datetime.datetime.strptime(line.split()[0], layout)
                              .replace(tzinfo=datetime.timezone.utc).timestamp())
                break
            except (IndexError, ValueError):
                continue
    return events


def utc(epoch):
    return datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def closes(backup, event):
    """A full whose start is later than the drop. pgBackRest starts have whole
    seconds, so a drop in that same second stays open (as in hosted.py)."""
    return backup.get("type") == "full" and backup["timestamp"]["start"] > event


def gap_windows(events, backups):
    """(start, end) per recorded drop: targets strictly between cannot be reached.

    The dropped segments were queued BEFORE the warning, so a gap may begin
    right after the last backup that completed before it. It closes at the
    stop of the first full backup that started after the warning.
    """
    windows = []
    for event in events:
        before = [b["timestamp"]["stop"] for b in backups if b["timestamp"]["stop"] <= event]
        after = [b["timestamp"]["stop"] for b in backups if closes(b, event)]
        windows.append((max(before, default=-math.inf), min(after, default=math.inf), event))
    return windows


def refuse_gap(instant, backups):
    import sys
    if REHEARSAL is not None:
        print("NOTE: a rehearsal does not read the dropped-WAL record", file=sys.stderr)
        return
    events = dropped_wal_events()
    if events is None:
        print("WARNING: the source's dropped-WAL record is unreachable (postgres not running); this restore "
              "cannot rule out an archive gap before the target time. See RESTORE.md, 'Recovery health'",
              file=sys.stderr)
        return
    for start, end, event in gap_windows(events, backups):
        if start < instant < end:
            raise ValueError(
                f"target time lies in a recorded archive gap: pgBackRest dropped WAL at {utc(event)}; "
                + (f"the gap may begin after {utc(start)} (the last backup before it) " if start > -math.inf
                   else "no retained backup precedes it ")
                + ("and no full backup has completed since" if end == math.inf
                   else f"and closes at {utc(end)} (the next full backup's stop)")
                + ". PITR never crosses missing WAL; choose a target outside the gap")


def refuse_open_gap(backups, accept=False):
    """Latest-state restore: with an open gap (no full started after the latest
    drop) recovery stops at the first missing segment and promotes that older
    state without an error. --accept-gap allows it: in a real disaster the
    pre-drop chain may be all there is."""
    import sys
    if REHEARSAL is not None:
        print("NOTE: a rehearsal does not read the dropped-WAL record", file=sys.stderr)
        return
    events = dropped_wal_events()
    if events is None:
        print("WARNING: the source's dropped-WAL record is unreachable (postgres not running); this latest-state "
              "restore cannot rule out an archive gap and may stop silently at one. See RESTORE.md, "
              "'Recovery health'", file=sys.stderr)
        return
    if not events or any(closes(b, max(events)) for b in backups):
        return
    gap = (f"pgBackRest dropped WAL at {utc(max(events))} and no full backup has started since, so a latest-state "
           "restore stops at the first missing segment and silently promotes an older state")
    if not accept:
        raise ValueError(f"an archive gap is open: {gap}. Take a new full backup first, or pass --accept-gap "
                         "to restore what the chain before the drop holds")
    print(f"WARNING: --accept-gap: {gap}", file=sys.stderr)


def run(target, restore_only=False, target_time=None, keep=False, json_out=None, source_settings=None,
        expect=((), ()), accept_gap=False):
    import sys
    if accept_gap and (not restore_only or target_time):
        raise ValueError("--accept-gap applies only to a latest-state restore (restore-only, no target-time)")
    if source_settings and not restore_only:
        raise ValueError("source-settings requires restore-only")
    if (target_time or keep) and not restore_only:
        raise ValueError("target-time and keep require restore-only; the PITR drill always stops")
    if json_out and (json_out.exists() or json_out.is_symlink()):
        raise ValueError("JSON output path must not exist")
    config = source(restore_only)
    # Refuse ANY overlap with live Docker mount sources before modifying a target.
    target = fenced_target(target)
    for mount in config.get("Mounts", []):
        mounted = Path(mount["Source"]).resolve()
        if target == mounted or target in mounted.parents or mounted in target.parents:
            raise ValueError("target overlaps a live mounted directory")
    if json_out and (target == json_out.resolve() or target in json_out.resolve().parents):
        raise ValueError("JSON output must be outside the restore directory")
    if restore_only:
        if source_settings:
            config["RecoverySettings"] = load_settings(source_settings)
        else:
            print("WARNING: no source-settings record; recovery uses backup control values and hosted defaults; later WAL ceiling increases may prevent recovery", file=sys.stderr)
    info = json.loads(repository("--output=json", "info", config=config, restore_only=restore_only))[0]
    if info.get("status", {}).get("code") != 0 or not info.get("backup"):
        raise ValueError("stanza and at least one valid backup required")
    if any(repo.get("status", {}).get("code", 0) != 0 for repo in info.get("repo", [])):
        raise ValueError("repository reports an error")
    backups = info["backup"]
    timestamp = target_time
    if timestamp:
        refuse_gap(target_instant(timestamp).timestamp(), backups)
        backups = [b for b in backups if b["timestamp"]["stop"] <= target_instant(timestamp).timestamp()]
    elif restore_only:
        refuse_open_gap(backups, accept_gap)
    if not backups:
        raise ValueError("no retained backup precedes target")
    backup = max(backups, key=lambda b: b["timestamp"]["stop"])
    label = backup["label"]
    if REHEARSAL is not None:
        # The restore writes the backup's database size plus recovered WAL below
        # the root. A marker drill does so while the source still fills the disk.
        size = backup.get("info", {}).get("size")
        if type(size) is not int or size < 0:
            raise ValueError("pgBackRest info has no database size for the selected backup; free space cannot be checked")
        advice = ("" if restore_only and not REHEARSAL.source_present() else
                  ". The source volumes still exist: the order that needs the least space is stop-source, "
                  "destroy-source-volumes --yes-delete-rehearsal-volumes, then restore --target-time")
        for fact in REHEARSAL.disk_gate("restore" if restore_only else "drill",
                                        root_bytes=math.ceil(safety.RESTORE_GROWTH * size), advice=advice):
            print(fact)
    target = empty_target(target, require_new=True)  # All fences passed before mutation.
    marker = None
    if not restore_only:
        marker = uuid.uuid4().hex
        marker_database()
        sql(f"INSERT INTO markers(drill,step) VALUES ('{marker}','first')", database=MARKER_DATABASE)
        timestamp = sql("SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")
        wait_archive(sql("SELECT pg_walfile_name(pg_switch_wal())"))
        time.sleep(1)
        sql(f"INSERT INTO markers(drill,step) VALUES ('{marker}','second')", database=MARKER_DATABASE)
        wait_archive(sql("SELECT pg_walfile_name(pg_switch_wal())"))
        print(f"database_time_T={timestamp}")
    # Read-only repository listing. Object count is NOT request count.
    objects = json.loads(repository("--recurse", "--output=json", "repo-ls", config=config, restore_only=restore_only))
    held = usage(objects)
    print(f"repository_bytes={held['bytes']} repository_objects={held['objects']} "
          f"wal_archive_bytes={held['archive_bytes']}")
    timings = restore(config, target, label, timestamp, keep, marker, expect)
    print(f"restored_directory={target} (retained for verification)")
    summary = json.dumps(report(info, backup, timings, objects), sort_keys=True)
    if json_out:
        with json_out.open("x") as output:
            output.write(summary + "\n")
    print(summary)  # One final machine-readable object, also written verbatim.


def main():
    global HOSTED_RECOVERY
    import sys
    if REHEARSAL is not None or safety.rehearsal_present() or compose_file() == safety.COMPOSE_FILE:
        raise ValueError("rehearsal mode is reachable only through rehearsal.sh")
    if sys.argv[1:2] == ["record-settings"]:
        parser = argparse.ArgumentParser(description="Retain running source recovery ceilings before removing its volume")
        parser.add_argument("--out", type=Path, required=True)
        parser.add_argument("--hosted-recovery", action="store_true")
        args = parser.parse_args(sys.argv[2:])
        HOSTED_RECOVERY = args.hosted_recovery
        hosted_only()
        return record_settings(args.out)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", type=Path)
    parser.add_argument("--restore-only", action="store_true")
    parser.add_argument("--target-time", help="UTC timestamp; restore-only mode")
    parser.add_argument("--keep", action="store_true", help="leave throwaway server running for the Phase 2 verifier")
    parser.add_argument("--json-out", type=Path, help="write the final JSON report to a new caller-owned file")
    parser.add_argument("--source-settings", type=Path, help="retained record-settings JSON for source-free recovery")
    parser.add_argument("--accept-gap", action="store_true",
                        help="latest-state restore although a recorded archive gap is open (stops at the gap)")
    parser.add_argument("--hosted-recovery", action="store_true", help="explicit real hosted recovery authorization (Phase 6+)")
    args = parser.parse_args()
    HOSTED_RECOVERY = args.hosted_recovery
    hosted_only()
    run(args.target, args.restore_only, args.target_time, args.keep, args.json_out, args.source_settings,
        accept_gap=args.accept_gap)


if __name__ == "__main__":
    def interrupted(signum, frame):
        raise SystemExit(128 + signum)
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (ValueError, RuntimeError, OSError, KeyError, json.JSONDecodeError) as error:
        # Never expose docker inspect data, command arguments, or credentials.
        print(f"restore/drill refused: {error}" if isinstance(error, (ValueError, RuntimeError)) else
              f"restore/drill failed: {safety.os_reason(error)}" if isinstance(error, OSError) else
              f"restore/drill failed: an unexpected internal error ({type(error).__name__})",
              file=__import__('sys').stderr)
        raise SystemExit(1)
