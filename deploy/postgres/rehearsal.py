#!/usr/bin/env python3
"""The only entry point of a database rehearsal.

Fixed subcommands only. Every Docker command line is built here from constants
and validated values: no caller-supplied Docker, Compose or pgBackRest option,
service name, container name, mount, user, entrypoint or repository path can
reach a command line. Settings and credentials come only from the env file.
"""
import datetime
import json
import math
import os
from pathlib import Path
import re
import signal
import sys
import time

KIT = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, KIT)  # rehearsal.sh runs Python isolated (-I): add only our kit.
import drill  # noqa: E402
import safety  # noqa: E402

USAGE = """usage: rehearsal.sh <subcommand> [options]   (full drill order; see RESTORE.md)

  preflight [--init-root]            checks without changes; --init-root creates the root of a NEW project
  up [--with-scheduler]              start postgres (and the backup scheduler); never restores
  status                             containers, jobs, unresolved jobs, kept restores, free space, backup status
  psql-check                         authenticated application query, locale and role facts
  import --sqlite FILE --server-bin FILE [--report FILE]
  verify --sqlite FILE --server-bin FILE [--report FILE]
  stanza-create
  backup full [--first] | backup incr
  check
  info [--json [--out FILE]]         backups in the repository; --out keeps the JSON for the budget record
  repo-usage [--out FILE]            read-only: object count and bytes under the rehearsal prefix
  sql-marker --label TEXT
  record-settings --out FILE
  archive-status
  drill --target NEWDIR --json-out FILE
  stop-source                        stop and remove the containers and jobs; volumes stay
  destroy-source-volumes --yes-delete-rehearsal-volumes
  restore --target NEWDIR --json-out FILE [--target-time UTC] [--source-settings FILE] [--keep]
          [--expect-marker LABEL]... [--expect-no-marker LABEL]...
  verify-restored --target DIR --sqlite FILE --server-bin FILE [--report FILE]
  restore-cleanup --target DIR
  repo-destroy --yes-delete-rehearsal-repo
  down                               containers, jobs and network; volumes stay
  logs [postgres|postgres-backup] [--tail N]

Selection comes from three exported variables only: KOMIKA_REHEARSAL_PROJECT,
KOMIKA_COMPOSE_ENV_FILE and KOMIKA_REHEARSAL_ROOT. FILE and NEWDIR outputs must
be new names directly under KOMIKA_REHEARSAL_ROOT. One operation runs at a time
on a host: every subcommand except status, logs, psql-check and archive-status
takes the host-wide operation lock and is refused at once while another holds it."""

# subcommand: (choices of each positional, optional trailing positional?, {flag: takes value}, required flags)
# A flag maps to False (no value), True (one value) or MANY (a value, repeatable).
MANY = "many"
JOB = {"--sqlite": True, "--server-bin": True, "--report": True}
SPEC = {
    "preflight": ((), False, {"--init-root": False}, ()),
    "up": ((), False, {"--with-scheduler": False}, ()),
    "status": ((), False, {}, ()),
    "psql-check": ((), False, {}, ()),
    "stanza-create": ((), False, {}, ()),
    "backup": ((("full", "incr"),), False, {"--first": False}, ()),
    "check": ((), False, {}, ()),
    "info": ((), False, {"--json": False, "--out": True}, ()),
    "repo-usage": ((), False, {"--out": True}, ()),
    "archive-status": ((), False, {}, ()),
    "import": ((), False, JOB, ("--sqlite", "--server-bin")),
    "verify": ((), False, JOB, ("--sqlite", "--server-bin")),
    "verify-restored": ((), False, dict(JOB, **{"--target": True}), ("--target", "--sqlite", "--server-bin")),
    "record-settings": ((), False, {"--out": True}, ("--out",)),
    "drill": ((), False, {"--target": True, "--json-out": True}, ("--target", "--json-out")),
    "restore": ((), False, {"--target": True, "--json-out": True, "--target-time": True,
                            "--source-settings": True, "--keep": False,
                            "--expect-marker": MANY, "--expect-no-marker": MANY}, ("--target", "--json-out")),
    "restore-cleanup": ((), False, {"--target": True}, ("--target",)),
    "sql-marker": ((), False, {"--label": True}, ("--label",)),
    "logs": (((safety.SERVICES),), True, {"--tail": True}, ()),
    "stop-source": ((), False, {}, ()),
    "destroy-source-volumes": ((), False, {"--yes-delete-rehearsal-volumes": False},
                               ("--yes-delete-rehearsal-volumes",)),
    "down": ((), False, {}, ()),
    "repo-destroy": ((), False, {"--yes-delete-rehearsal-repo": False}, ("--yes-delete-rehearsal-repo",)),
}
PSQL = ("psql", "-X", "-q", "-w", "-U", "postgres", "-d", "komika", "-v", "ON_ERROR_STOP=1", "-At", "-c")
PGBACKREST = "/opt/komika-postgres/pgbackrest.sh"
BACKUP = "/opt/komika-postgres/backup.sh"
FACTS = """SELECT 'server_version=' || current_setting('server_version')
UNION ALL SELECT 'database=' || datname || ' encoding=' || pg_encoding_to_char(encoding) || ' locale_provider=' || datlocprovider::text || ' builtin_locale=' || coalesce(datlocale, '') || ' collate=' || datcollate || ' ctype=' || datctype || ' owner=' || pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'komika'
UNION ALL SELECT 'role=' || rolname || ' login=' || rolcanlogin::text || ' superuser=' || rolsuper::text || ' createdb=' || rolcreatedb::text || ' createrole=' || rolcreaterole::text || ' replication=' || rolreplication::text FROM pg_roles WHERE rolname = 'komika'
UNION ALL SELECT 'extension=' || extname || ' version=' || extversion FROM pg_extension WHERE extname = 'pg_trgm'
UNION ALL SELECT 'data_checksums=' || current_setting('data_checksums') || ' password_encryption=' || current_setting('password_encryption') || ' archive_mode=' || current_setting('archive_mode') || ' in_recovery=' || pg_is_in_recovery()::text"""
APPLICATION = ('PGPASSWORD="$KOMIKA_POSTGRES_PASSWORD" PGOPTIONS="-c default_transaction_read_only=on" '
               'psql -X -q -w -h "$HOSTNAME" -U komika -d komika -v ON_ERROR_STOP=1 -At -c '
               '"SELECT \'application_role=\' || current_user || \' public_tables=\' || '
               '(SELECT count(*) FROM pg_tables WHERE schemaname = \'public\')"')
ARCHIVER = """SELECT 'archived_count=' || archived_count || ' failed_count=' || failed_count || ' last_archived_wal=' || coalesce(last_archived_wal, '') || ' last_archived_time=' || coalesce(last_archived_time::text, '') || ' last_failed_wal=' || coalesce(last_failed_wal, '') || ' last_failed_time=' || coalesce(last_failed_time::text, '') FROM pg_stat_archiver
UNION ALL SELECT 'current_wal=' || pg_walfile_name(pg_current_wal_lsn()) || ' database_time=' || to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')"""
WAL_FILES = ('printf "ready_segments=%s\\n" "$(find "$PGDATA/pg_wal/archive_status" -name "*.ready" | wc -l | tr -d " ")"; '
             'printf "pg_wal_bytes=%s\\n" "$(du -sb "$PGDATA/pg_wal" | cut -f1)"; '
             'printf "backup_status="; cat /var/lib/postgresql/komika-backup/status.json 2>/dev/null || echo "{}"')
STATUS_FILE = 'cat /var/lib/postgresql/komika-backup/status.json 2>/dev/null || echo "{}"'
# stanza-delete needs the stop file in this container's own lock path. Both
# commands pass the in-container project/prefix fence in pgbackrest.sh again.
DESTROY = f"{PGBACKREST} stop && {PGBACKREST} --force stanza-delete"
# Read-only: no container is created, started, stopped or removed and nothing is
# allocated, so these four run without the host-wide operation lock.
READ_ONLY = ("status", "logs", "psql-check", "archive-status")
# These reconcile what an interrupted step left; every other locked subcommand
# refuses while a job or metadata container of the project is unresolved.
CLEANERS = ("stop-source", "down", "restore-cleanup")


class Usage(Exception):
    pass


def parse(argv):
    """Strict: a known subcommand, known flags once each, no abbreviations or `=`."""
    if not argv or argv[0] not in SPEC:
        raise Usage("unknown or missing subcommand")
    name = argv[0]
    choices, optional, flags, required = SPEC[name]
    options, positional = {}, []
    index = 1
    while index < len(argv):
        argument = argv[index]
        if argument in flags:
            if argument in options and flags[argument] != MANY:
                raise Usage(f"{argument} given twice")
            if flags[argument]:
                index += 1
                if index >= len(argv):
                    raise Usage(f"{argument} needs a value")
                if flags[argument] == MANY:
                    options.setdefault(argument, []).append(argv[index])
                else:
                    options[argument] = argv[index]
            else:
                options[argument] = True
        elif argument.startswith("-") or len(positional) >= len(choices) or argument not in choices[len(positional)]:
            raise Usage(f"unknown argument for {name}")
        else:
            positional.append(argument)
        index += 1
    if len(positional) < len(choices) and not optional:
        raise Usage(f"{name} needs one of: {' | '.join(choices[len(positional)])}")
    missing = [flag for flag in required if flag not in options]
    if missing:
        raise Usage(f"{name} requires {missing[0]}")
    if name == "info" and "--out" in options and "--json" not in options:
        raise Usage("--out belongs to `info --json`")
    return name, positional, options


def say(text):
    try:
        print(text, flush=True)
    except BrokenPipeError:
        # `rehearsal.sh ... | head` must not abort an operation half way.
        sys.stdout = open(os.devnull, "w")


def need_repository(ctx):
    if ctx.no_repo:
        raise ValueError("KOMIKA_REHEARSAL_NO_REPO=1: this local proof stack has no repository; subcommand unavailable")


def need_linux(ctx):
    # Restores write a bind-mounted host directory as the container's root user.
    if not ctx.platform.startswith("linux"):
        raise ValueError("restore and drill need a Linux Docker host: the host mount-table fence is unavailable here")


def gate(ctx, step, **needs):
    for fact in ctx.disk_gate(step, **needs):
        say(fact)


def labelled(ctx):
    """Every container that carries this project's label, sorted by what it is."""
    found = {"service": [], "transient": [], "restore": [], "unknown": []}
    for identity in ctx.docker("ps", "-a", "-q", "--no-trunc", "--filter", f"label={safety.LABEL}={ctx.project}").split():
        try:
            entry = safety.require_label("container", identity, ctx.project, ctx.inspect)
        except RuntimeError:
            continue  # A `--rm` container that ended between the listing and the inspection.
        name = str(entry.get("Name")).lstrip("/")
        if name in ctx.containers.values():
            kind = "service"
        elif re.fullmatch(re.escape(ctx.project) + safety.TRANSIENT_PATTERN, name):
            kind = "transient"
        elif re.fullmatch(r"komika-pitr-[0-9a-f]{12}", name) and safety.TARGET_LABEL in entry["Config"]["Labels"]:
            kind = "restore"
        else:
            kind = "unknown"
        found[kind].append(entry)
    return found


def watch_free(ctx):
    """Free bytes and the floor of both filesystems; never a refusal."""
    try:
        floor = safety.floor_bytes(ctx.values)
        root = ctx.root or ctx.fence_root()
        places = [("docker_root", ctx.docker_root), ("rehearsal_root", str(root))]
    except ValueError as error:
        say(f"watch-free unavailable: {error}")
        return
    for what, path in places:
        try:
            free = safety.free_bytes(path)[0]
            say(f"watch-free {what}={path} free_bytes={free} floor_bytes={floor} "
                f"{'ok' if free >= floor else 'BELOW THE FLOOR'}")
        except (OSError, TypeError):
            say(f"watch-free {what}={path} free_bytes=unavailable (not visible on this host) floor_bytes={floor}")


def execute(ctx, *recipe):
    """A fixed recipe as postgres in the label- and contract-checked container."""
    entry = ctx.container("postgres", running=True)
    return ctx.stream("exec", "--user", "postgres", entry["Id"], *recipe)


def image_id(ctx):
    """The local image, after its kit files were compared with this kit's."""
    return ctx.image_identity()


def existing_file(value, what):
    path = safety.canonical_path(value, what)
    if not os.path.isfile(path) or os.path.islink(path):
        raise ValueError(f"{what} must be an existing regular file")
    return path


def usable(path, uid, gid, need, what):
    """The job has exactly this uid and gid and no capability: check the mode bits."""
    status = os.stat(path)
    shift = 6 if status.st_uid == uid else 3 if status.st_gid == gid else 0
    if (status.st_mode >> shift) & need != need:
        raise ValueError(f"{what} is not {'readable and executable' if need & 1 else 'readable'} by the job user {uid}:{gid}")


def utc_time(value):
    if not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{1,6})?(?:Z|\+00:00)", value):
        raise ValueError("--target-time must be ISO-8601 UTC, for example 2026-10-11T12:34:56.123456Z")
    drill.recovery_target(value)  # A real date and time, on every Python version.
    return value


# ---- subcommands ---------------------------------------------------------------

def up(ctx, positional, options):
    services = ["postgres"]
    if options.get("--with-scheduler"):
        need_repository(ctx)
        services.append("postgres-backup")
    image = image_id(ctx)
    refuse_implicit_restore(ctx)
    gate(ctx, "up")
    status = ctx.stream(*ctx.compose_base, "up", "-d", "--no-build", "--pull", "never", *services)
    if status:
        return status
    # Volumes that Compose has just created are judged like existing ones.
    ctx.storage()
    deadline = time.monotonic() + 600
    last = None
    while True:
        # The created container must equal the contract, not only the model.
        entry = ctx.container("postgres")
        if entry is None:
            raise ValueError("Compose reported success but the postgres container is absent")
        if entry.get("Image") != image:
            raise ValueError("the postgres container does not run the checked image build; "
                             "run `rehearsal.sh stop-source` and `rehearsal.sh up` again")
        state = entry["State"]
        health = (state.get("Health") or {}).get("Status")
        if (state.get("Status"), health) != last:
            last = (state.get("Status"), health)
            say(f"postgres state={last[0]} health={last[1]}")
        if health == "healthy":
            break
        if state.get("Status") in ("exited", "dead") or time.monotonic() > deadline:
            say("postgres did not become healthy; last log lines:")
            ctx.stream("logs", "--tail", "40", ctx.containers["postgres"])
            return 1
        time.sleep(3)
    if len(services) == 2:
        ctx.container("postgres-backup", running=True)
    return 0


IMPLICIT_RESTORE = ("a rehearsal `up` never restores. The source volume {volume} is absent and the repository under "
                    "the rehearsal prefix {prefix} already holds a backup, so starting PostgreSQL would restore it into "
                    "a new volume behind a disk check that knows nothing of its size. Restore explicitly, with the "
                    "space check for that backup: `rehearsal.sh restore --target NEWDIR --json-out FILE "
                    "[--target-time UTC]`. A new drill uses a new KOMIKA_R2_PREFIX, whose repository is empty")


def refuse_implicit_restore(ctx):
    """In a rehearsal `up` starts an existing cluster or initializes a new one; it never restores.

    The image refuses the same case by itself (hosted.start), whatever the
    host could see. Here the refusal comes before Compose creates anything,
    whenever the host can read the repository: the data volume is absent and
    the rehearsal network (which a reader needs) exists.
    """
    if ctx.no_repo:
        return
    data = f"{ctx.project}-postgres-data"
    if data in ctx.docker("volume", "ls", "--format", "{{.Name}}").split():
        return  # The image decides: it starts an existing cluster and refuses to restore into an empty volume.
    if ctx.network not in ctx.docker("network", "ls", "--format", "{{.Name}}").split():
        say("up: new source volumes; the image initializes a new cluster only on an empty repository "
            "(KOMIKA_PG_INIT_NEW=1) and refuses to restore an existing backup")
        return
    entries = None
    try:
        # A refusal of the reader itself (its network, its removal) is not swallowed here.
        text = drill.throwaway(drill.source(True), PGBACKREST, "--output=json", "info")
    except RuntimeError:
        text = None
    if text is not None:
        try:
            entries = json.loads(text)
        except ValueError:
            entries = None
    if entries is None:
        say("up: the repository could not be read from the host; the image decides at start and never restores "
            "in a rehearsal")
        return
    if any(isinstance(entry, dict) and entry.get("backup") for entry in (entries if isinstance(entries, list) else [])):
        raise ValueError(IMPLICIT_RESTORE.format(volume=data, prefix=safety.prefix_fence(ctx.environment)))


def unresolved_lines(ctx):
    """What the lock file and the daemon say about interrupted steps; read-only."""
    lines = []
    try:
        holder, entries = safety.read_ledger()
    except ValueError as error:
        return [f"operation lock: unreadable ({error})"]
    if holder:
        try:
            os.kill(holder["pid"], 0)
            alive = "running"
        except ProcessLookupError:
            alive = "NOT running (it ended without cleanup; the lock itself is free)"
        except OSError:
            alive = "running as another user"
        lines.append(f"operation recorded: subcommand={holder['subcommand']} pid={holder['pid']} since={holder['since']} "
                     f"process={alive}")
    else:
        lines.append("operation recorded: none")
    for entry in entries:
        mine = safety.unresolved_project(entry["name"]) == ctx.project
        lines.append(f"UNRESOLVED job container {entry['name']}: its creation was interrupted "
                     f"{max(0, int(time.time() - entry['noted']))} s ago and never confirmed; "
                     + ("`stop-source` or `down` reconciles it, every other step refuses until then" if mine else
                        "it belongs to another rehearsal project on this host"))
    return lines


def reconciled(ctx):
    """Under the operation lock a job or metadata container of this project can
    only be what an interrupted earlier step left: refuse until it is resolved."""
    pending = [entry["name"] for entry in (ctx.lock.unresolved(ctx.project) if ctx.lock is not None else [])]
    pattern = re.escape(ctx.project) + safety.TRANSIENT_PATTERN
    present = [name for name in ctx.docker("ps", "-a", "--filter", f"label={safety.LABEL}={ctx.project}",
                                           "--format", "{{.Names}}").split() if re.fullmatch(pattern, name)]
    names = sorted(set(pending) | set(present))
    if names:
        raise ValueError("unresolved job container(s) of an interrupted earlier step: "
                         + ", ".join(f"{name} ({'present' if name in present else 'creation never confirmed'})"
                                     for name in names)
                         + ". Run `rehearsal.sh status`, then `rehearsal.sh stop-source` (or `down`): they wait for a "
                           "late creation, remove the container and clear the record. No other step starts before that")


def status(ctx, positional, options):
    running = False
    for service in safety.SERVICES:
        entry = ctx.container(service)
        if entry is None:
            say(f"{service}: absent")
            continue
        state = entry["State"]
        running = running or (service == "postgres" and state.get("Running"))
        say(f"{service}: state={state.get('Status')} health={(state.get('Health') or {}).get('Status')} "
            f"started={state.get('StartedAt')} oom_killed={state.get('OOMKilled')} restarts={entry.get('RestartCount')}")
    for kind, names in (("volume", ctx.volumes), ("network", [ctx.network])):
        for name in names:
            say(f"{kind} {name}: {'present' if name in ctx.existing[kind] else 'absent'}")
    for entry in kept(ctx, None)[0]:
        say(f"kept restore {entry['Name'].lstrip('/')}: state={entry['State'].get('Status')} "
            f"target={entry['Config']['Labels'].get(safety.TARGET_LABEL)}")
    found = labelled(ctx)
    for entry in found["transient"]:
        say(f"job container {entry['Name'].lstrip('/')}: state={entry['State'].get('Status')} "
            f"started={entry['State'].get('StartedAt')} (removed by `stop-source` and `down`)")
    for entry in found["unknown"]:
        say(f"UNKNOWN container {entry['Name'].lstrip('/')}: carries this rehearsal's label but none of its "
            f"name patterns; state={entry['State'].get('Status')}; no subcommand touches it")
    for line in unresolved_lines(ctx):
        say(line)
    watch_free(ctx)
    if running:
        say("backup_status:")
        execute(ctx, "sh", "-c", STATUS_FILE)
        if not ctx.no_repo:
            return execute(ctx, PGBACKREST, "info")
        say("repository: none (KOMIKA_REHEARSAL_NO_REPO=1; NO backup and NO WAL archiving)")
    return 0


def psql_check(ctx, positional, options):
    status = execute(ctx, "/opt/komika-postgres/health.sh")
    say("application_health=" + ("ok" if status == 0 else "FAILED"))
    return status or execute(ctx, "sh", "-c", APPLICATION) or execute(ctx, *PSQL, FACTS)


def stanza_create(ctx, positional, options):
    need_repository(ctx)
    return execute(ctx, PGBACKREST, "stanza-create")


def backup(ctx, positional, options):
    need_repository(ctx)
    if options.get("--first") and positional[0] != "full":
        raise Usage("--first belongs to `backup full`")
    ctx.container("postgres", running=True)
    gate(ctx, "backup")
    return execute(ctx, BACKUP, "bootstrap-full" if options.get("--first") else positional[0])


def check(ctx, positional, options):
    need_repository(ctx)
    return execute(ctx, BACKUP, "check")


def info(ctx, positional, options):
    need_repository(ctx)
    if "--out" in options:
        out = ctx.new_path(options["--out"], "--out")
        # The budget record: per-backup database and repository sizes as pgBackRest reports them.
        text = repository_output(ctx, "--output=json", "info")
        try:
            listed = isinstance(json.loads(text), list)
        except ValueError:
            listed = False
        if not listed:
            raise ValueError("pgBackRest info did not print a JSON list; nothing was stored")
        write_new(out, text if text.endswith("\n") else text + "\n")
        say(text.strip())
        say(f"info_json={out}")
        return 0
    output = "--output=json" if options.get("--json") else "--output=text"
    entry = ctx.container("postgres")
    if entry and entry["State"].get("Running"):
        return execute(ctx, PGBACKREST, output, "info")
    # The source is gone: read metadata from a mount-free throwaway container.
    image_id(ctx)
    return drill.throwaway(drill.source(True), PGBACKREST, output, "info", stream=True)


def repository_output(ctx, *arguments):
    """The captured output of one read-only pgBackRest command of the allowlist:
    in the running source container, else in a mount-free reader."""
    entry = ctx.container("postgres")
    if entry and entry["State"].get("Running"):
        return drill.live(PGBACKREST, *arguments)
    image_id(ctx)
    return drill.throwaway(drill.source(True), PGBACKREST, *arguments)


def write_new(path, text):
    descriptor = os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
    with os.fdopen(descriptor, "w") as output:
        output.write(text)


def repo_usage(ctx, positional, options):
    """Object count and bytes under the rehearsal prefix; it can name no other prefix.

    The listing is `repo-ls --recurse --output=json` through the in-image
    wrapper, which fences project and prefix again. No argument reaches it.
    """
    need_repository(ctx)
    out = ctx.new_path(options["--out"], "--out") if "--out" in options else None
    prefix = safety.prefix_fence(ctx.environment)
    try:
        listing = json.loads(repository_output(ctx, "--recurse", "--output=json", "repo-ls"))
    except ValueError:
        raise ValueError("the repository listing is not JSON; nothing was counted")
    record = dict(prefix=prefix, measured_at=datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                  **drill.usage(listing))
    say(" ".join(f"{key}={value}" for key, value in record.items()))
    if out is not None:
        write_new(out, json.dumps(record, sort_keys=True) + "\n")
        say(f"repo_usage_json={out}")
    return 0


def archive_status(ctx, positional, options):
    return execute(ctx, *PSQL, ARCHIVER) or execute(ctx, "sh", "-c", WAL_FILES)


def sql_marker(ctx, positional, options):
    label = marker_label(options["--label"], "--label")
    # Own database: the application database must stay exactly what the importer
    # and the verifier expect. The label alphabet has no quote; the insert commits
    # before the database time is read.
    drill.marker_database()
    if drill.sql(f"INSERT INTO operator_markers(label) VALUES ('{label}') ON CONFLICT DO NOTHING RETURNING label",
                 database=drill.MARKER_DATABASE) != label:
        raise ValueError("that marker label already exists; choose a new label")
    say("marker=" + label)
    say(drill.sql("SELECT 'database_time=' || to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') "
                  "|| ' wal_lsn=' || pg_current_wal_lsn() || ' wal_file=' || pg_walfile_name(pg_current_wal_lsn())"))
    return 0


def marker_label(value, flag):
    if not re.fullmatch(safety.MARKER_LABEL, value):
        raise ValueError(f"{flag} must be 1..40 characters of letters, digits and _.-")
    return value


def record_settings(ctx, positional, options):
    ctx.fence_root()
    out = ctx.new_path(options["--out"], "--out")
    drill.record_settings(out)
    say(f"source_settings={out}")
    return 0


def marker_drill(ctx, positional, options):
    need_repository(ctx)
    need_linux(ctx)
    ctx.fence_root()
    report = ctx.new_path(options["--json-out"], "--json-out")
    target = ctx.new_path(options["--target"], "--target")
    image_id(ctx)
    with leftover(target):
        drill.run(target, json_out=report)
    return 0


class leftover:
    """A restore that fails after it began leaves bytes the caller cannot remove."""

    def __init__(self, target):
        self.target = target

    def __enter__(self):
        return self

    def __exit__(self, kind, error, traceback):
        if kind is not None and os.path.isdir(self.target):
            print(f"restore did not finish. The directory {self.target} stays and now belongs to the image's postgres "
                  f"user (uid 999), so only root can remove it: `sudo rm -rf -- {self.target}`. Remove it, or choose a "
                  "new --target name, before the next attempt; a target must not exist.", file=sys.stderr, flush=True)
        return False


def restore(ctx, positional, options):
    need_repository(ctx)
    need_linux(ctx)
    root = ctx.fence_root()
    report = ctx.new_path(options["--json-out"], "--json-out")
    target = ctx.new_path(options["--target"], "--target")
    settings = None
    if "--source-settings" in options:
        settings = Path(existing_file(options["--source-settings"], "--source-settings"))
        if settings.parent != root:
            raise ValueError("--source-settings must be a record directly under KOMIKA_REHEARSAL_ROOT")
    timestamp = utc_time(options["--target-time"]) if "--target-time" in options else None
    present = [marker_label(label, "--expect-marker") for label in options.get("--expect-marker", [])]
    absent = [marker_label(label, "--expect-no-marker") for label in options.get("--expect-no-marker", [])]
    if set(present) & set(absent):
        raise ValueError("a marker label cannot be expected both present and absent")
    image_id(ctx)
    with leftover(target):
        drill.run(target, True, timestamp, bool(options.get("--keep")), report, settings, (present, absent))
    return 0


def kept(ctx, target):
    """Kept throwaway restore containers/networks of THIS project (and target)."""
    filters = ["--filter", f"label={safety.LABEL}={ctx.project}"]
    filters += ["--filter", f"label={safety.TARGET_LABEL}" + (f"={target}" if target else "")]
    containers, networks = [], []
    for identity in ctx.docker("ps", "-a", "-q", "--no-trunc", *filters).split():
        entry = safety.require_label("container", identity, ctx.project, ctx.inspect)
        name = str(entry.get("Name")).lstrip("/")
        labels = entry["Config"]["Labels"]
        mounts = [(m.get("Type"), m.get("Source"), m.get("Destination")) for m in entry.get("Mounts") or []]
        if (not re.fullmatch(r"komika-pitr-[0-9a-f]{12}", name) or (target and labels.get(safety.TARGET_LABEL) != target)
                or mounts != [("bind", labels.get(safety.TARGET_LABEL), "/restore")]):
            raise ValueError("a labelled restore container does not match the throwaway restore contract")
        containers.append(entry)
    for identity in ctx.docker("network", "ls", "-q", "--no-trunc", *filters).split():
        entry = safety.require_label("network", identity, ctx.project, ctx.inspect)
        if (not re.fullmatch(r"komika-pitr-[0-9a-f]{12}-network", str(entry.get("Name")))
                or (target and (entry.get("Labels") or {}).get(safety.TARGET_LABEL) != target)):
            raise ValueError("a labelled restore network does not match the throwaway restore contract")
        networks.append(entry)
    return containers, networks


def kept_target(ctx, value):
    root = ctx.fence_root()
    target = safety.canonical_path(value, "--target")
    if Path(target).parent != root or not os.path.isdir(target):
        raise ValueError("--target must be an existing restore directory directly under KOMIKA_REHEARSAL_ROOT")
    return target


def restore_cleanup(ctx, positional, options):
    containers, networks = kept(ctx, kept_target(ctx, options["--target"]))
    for entry in containers:
        if entry["State"].get("Running"):
            # An interrupted restore may have no server to stop; that is fine.
            ctx.stream("exec", "--user", "postgres", entry["Id"], "pg_ctl", "-D", "/restore", "-w", "-m", "fast", "stop")
        safety.require_label("container", entry["Id"], ctx.project, ctx.inspect)
        ctx.docker("rm", "-f", "-v", entry["Id"])
    for entry in networks:
        safety.require_label("network", entry["Id"], ctx.project, ctx.inspect)
        ctx.docker("network", "rm", entry["Id"])
    say(f"removed_containers={len(containers)} removed_networks={len(networks)} (restored directory retained)")
    return 0


def job(ctx, command, options, network, host, growth=0.0):
    """Importer/verifier in a throwaway, non-root, read-only container."""
    root = ctx.fence_root()
    sqlite = existing_file(options["--sqlite"], "--sqlite")
    binary = existing_file(options["--server-bin"], "--server-bin")
    report = ctx.new_path(options["--report"], "--report") if "--report" in options else None
    # Only the main file is mounted. Pending pages in a write-ahead log or a hot
    # rollback journal would be silently left out, so refuse them.
    for suffix in ("-wal", "-journal"):
        if os.path.lexists(sqlite + suffix) and (os.path.islink(sqlite + suffix) or os.path.getsize(sqlite + suffix)):
            raise ValueError(f"--sqlite has a non-empty {suffix} file beside it; use a checkpointed standalone copy")
    image_id(ctx)
    name = ctx.transient_name("job")
    token = name[-8:]
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    kind = command.split("-", 1)[0]
    work = root / f"{kind}-{stamp}-{token}"
    # A root caller still runs the job as an unprivileged user.
    uid, gid = (ctx.uid, ctx.gid) if ctx.uid else (65534, 65534)
    usable(sqlite, uid, gid, 4, "--sqlite")
    usable(binary, uid, gid, 5, "--server-bin")
    if growth:
        # The data volume grows by the PostgreSQL base and by WAL not yet archived.
        gate(ctx, kind, docker_bytes=math.ceil(growth * os.path.getsize(sqlite)))
    for directory in (work, work / "tmp"):
        os.mkdir(directory, 0o700)
        if not ctx.uid:
            os.chown(directory, uid, gid)
    limits = {name: safety.limit(ctx.values, "KOMIKA_REHEARSAL_", "JOB_" + name)[0]
              for name in ("MEMORY", "CPUS", "PIDS")}
    # The URL carries the password: it travels on stdin, never in argv or a file.
    environment = (f"KOMIKA_IMPORT_POSTGRES_URL=postgres://komika:{ctx.values['KOMIKA_POSTGRES_PASSWORD']}@{host}:5432/komika\n"
                   "SQLITE_TMPDIR=/work/tmp\nTMPDIR=/work/tmp\nHOME=/work\n")
    say(f"work_directory={work}")
    say(f"job_container={name}")
    interrupted = True
    try:
        status = ctx.stream(
            "run", "--rm", "--init", "--pull=never", "--name", name,
            "--label", f"{safety.LABEL}={ctx.project}", "--network", network,
            "--user", f"{uid}:{gid}", "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges",
            f"--memory={limits['MEMORY']}", f"--memory-swap={limits['MEMORY']}",
            f"--cpus={limits['CPUS']}", f"--pids-limit={limits['PIDS']}", *safety.LOG_OPTIONS,
            "--env-file", "/dev/stdin", "--workdir", "/work",
            "--mount", f"type=bind,src={binary},dst=/opt/komika-server/komika-server,readonly",
            "--mount", f"type=bind,src={sqlite},dst=/source/komika.sqlite3,readonly",
            "--mount", f"type=bind,src={work},dst=/work",
            # SQLite may create its empty -wal/-shm beside the read-only file here.
            "--tmpfs", f"/source:rw,noexec,nosuid,nodev,size=64m,mode=0700,uid={uid},gid={gid}",
            "--tmpfs", "/var/lib/postgresql:mode=0700",
            "--entrypoint", "/opt/komika-server/komika-server",
            safety.IMAGE, command, "--sqlite", "/source/komika.sqlite3", "--json-out", "/work/report.json",
            input=environment)
        interrupted = False
    finally:
        # Ctrl-C, SIGTERM or a lost terminal kills the client, not the container;
        # the daemon may even create it after the client is gone.
        outcome = ctx.remove_transient(name, settle=interrupted)
        if outcome == "removed":
            say(f"job_container={name} was still present and has been removed")
        elif outcome == "unresolved":
            say(f"job_container={name} UNRESOLVED: its creation was interrupted and it did not appear; "
                "run `rehearsal.sh status`, then `rehearsal.sh stop-source` or `down`")
    produced = work / "report.json"
    if produced.is_file() and not produced.is_symlink():
        say(f"report={produced}")
        if report is not None:
            # The job wrote this file: never follow a link it may have planted.
            with os.fdopen(os.open(str(produced), os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)), "rb") as source:
                content = source.read(64 << 20)
            descriptor = os.open(str(report), os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
            with os.fdopen(descriptor, "wb") as copy:
                copy.write(content)
            say(f"report_copy={report}")
    say(f"{command} exit_status={status}")
    return status


def import_sqlite(ctx, positional, options):
    ctx.container("postgres", running=True)
    safety.require_label("network", ctx.network, ctx.project, ctx.inspect)
    return job(ctx, "import-sqlite-to-postgres", options, ctx.network, ctx.containers["postgres"],
               growth=safety.IMPORT_GROWTH)


def verify(ctx, positional, options):
    ctx.container("postgres", running=True)
    safety.require_label("network", ctx.network, ctx.project, ctx.inspect)
    return job(ctx, "verify-sqlite-postgres", options, ctx.network, ctx.containers["postgres"])


def verify_restored(ctx, positional, options):
    containers, networks = kept(ctx, kept_target(ctx, options["--target"]))
    if len(containers) != 1 or len(networks) != 1 or not containers[0]["State"].get("Running"):
        raise ValueError("no single running kept restore for this target; run `restore --keep` first")
    name = containers[0]["Name"].lstrip("/")
    if networks[0]["Name"] != name + "-network":
        raise ValueError("kept restore container and network do not belong together")
    return job(ctx, "verify-sqlite-postgres", options, networks[0]["Name"], name)


def logs(ctx, positional, options):
    tail = options.get("--tail", "200")
    if not re.fullmatch(r"[1-9][0-9]{0,4}", tail):
        raise ValueError("--tail must be a number from 1 to 99999")
    entry = ctx.container(positional[0] if positional else "postgres", contract=False)
    if entry is None:
        raise ValueError("that rehearsal container is absent")
    return ctx.stream("logs", "--tail", tail, entry["Id"])


def remove_containers(ctx):
    """Stop and remove the service containers and every job/metadata container.

    Each object is checked on its own (label, exact name or name pattern). What
    is refused is returned with its reason; everything else is still removed.
    """
    refused = [f"{kind} {name}: {reason}; never touched" for kind, name, reason in ctx.foreign]
    refused += [f"{kind} {name} is not rehearsal storage: {reason}; reported only, this subcommand never touches "
                "volumes (inspect it yourself)" for kind, name, reason in ctx.unsafe]
    pending = ctx.lock.unresolved(ctx.project) if ctx.lock is not None else []
    if pending:
        # A creation that was interrupted moments ago may still arrive: give it its time first.
        # Bounded by UNRESOLVED_AGE, whatever time the record claims.
        wait = safety.settle_wait(pending, time.time())
        if wait > 0:
            say(f"waiting {math.ceil(wait)} s: the creation of {', '.join(entry['name'] for entry in pending)} was "
                f"interrupted less than {safety.UNRESOLVED_AGE:g} s ago")
            time.sleep(wait)
    for service in reversed(safety.SERVICES):
        # Label alone authorizes removing a container with our exact name:
        # removal keeps both volumes and cannot touch any other resource.
        try:
            entry = ctx.container(service, contract=False)
            if entry is None:
                say(f"{service}: absent")
                continue
            if entry["State"].get("Running"):
                ctx.docker("stop", "-t", "240", entry["Id"])
            safety.require_label("container", entry["Id"], ctx.project, ctx.inspect)
            ctx.docker("rm", entry["Id"])
            say(f"{service}: stopped and removed (volumes kept)")
        except (ValueError, RuntimeError) as error:
            if not any(f"container {ctx.containers[service]}:" in line for line in refused):
                refused.append(f"container {ctx.containers[service]}: {error}; not removed")
    found = labelled(ctx)
    removed, failed = set(), set()
    for entry in found["transient"]:
        name = entry["Name"].lstrip("/")
        try:
            ctx.remove_transient(name)
            removed.add(name)
            say(f"job container {name}: removed")
        except (ValueError, RuntimeError) as error:
            failed.add(name)
            refused.append(f"container {name}: {error}; not removed")
    for entry in pending:
        name = entry["name"]
        if name not in failed:
            say(f"unresolved job container {name}: " + ("it had appeared and was removed" if name in removed else
                f"it never appeared in {max(0, int(time.time() - entry['noted']))} s") + "; record cleared")
    if pending:
        ctx.lock.resolve([entry["name"] for entry in pending if entry["name"] not in failed])
    for entry in found["unknown"]:
        refused.append(f"container {entry['Name'].lstrip('/')}: carries this rehearsal's label but none of its name "
                       "patterns; not removed (inspect it and remove it yourself)")
    return refused, found


def report(refused):
    for line in refused:
        say("REFUSED: " + line)
    return 1 if refused else 0


def stop_source(ctx, positional, options):
    return report(remove_containers(ctx)[0])


def destroy_source_volumes(ctx, positional, options):
    if any(ctx.container(service, contract=False) for service in safety.SERVICES):
        raise ValueError("rehearsal containers still exist; run `rehearsal.sh stop-source` first")
    for name in ctx.volumes:
        if name not in ctx.docker("volume", "ls", "--format", "{{.Name}}").split():
            say(f"volume {name}: absent")
            continue
        safety.require_label("volume", name, ctx.project, ctx.inspect)
        ctx.docker("volume", "rm", name)
        say(f"volume {name}: removed")
    return 0


def down(ctx, positional, options):
    refused, found = remove_containers(ctx)
    for entry in found["restore"]:
        target = entry["Config"]["Labels"].get(safety.TARGET_LABEL)
        refused.append(f"kept restore container {entry['Name'].lstrip('/')}: not removed by down; run "
                       f"`rehearsal.sh restore-cleanup --target {target}`")
    if ctx.network in ctx.docker("network", "ls", "--format", "{{.Name}}").split():
        try:
            entry = safety.require_label("network", ctx.network, ctx.project, ctx.inspect)
            attached = sorted(str(item.get("Name")) for item in (entry.get("Containers") or {}).values())
            if attached:
                raise ValueError("still used by container(s) " + ", ".join(attached)
                                 + " that this subcommand may not remove; disconnect or remove them first")
            ctx.docker("network", "rm", entry.get("Id") or ctx.network)
            say(f"network {ctx.network}: removed")
        except (ValueError, RuntimeError) as error:
            if not any(f"network {ctx.network}:" in line for line in refused):
                refused.append(f"network {ctx.network}: {error}; not removed")
    return report(refused)


def repo_destroy(ctx, positional, options):
    need_repository(ctx)
    for service in safety.SERVICES:
        entry = ctx.container(service, contract=False)
        if entry and entry["State"].get("Running"):
            raise ValueError("rehearsal containers are running; run `rehearsal.sh stop-source` first")
    image_id(ctx)
    config = drill.source(True)
    # The prefix is the one in the env file, already fenced here; the image
    # fences it again at start and before each pgBackRest command.
    say(f"deleting pgBackRest stanza under rehearsal prefix {safety.prefix_fence(ctx.environment)}")
    status = drill.throwaway(config, "sh", "-c", DESTROY, stream=True)
    if status:
        return status
    try:
        listing = json.loads(drill.throwaway(config, PGBACKREST, "--recurse", "--output=json", "repo-ls"))
        entries = listing.values() if isinstance(listing, dict) else listing
        say(f"repository_files_after={sum(1 for e in entries if isinstance(e, dict) and e.get('type') == 'file')}")
    except (RuntimeError, ValueError):
        say("repository_files_after=unknown (the listing of the now missing path failed)")
    return 0


def reason(error):
    """The text of a refusal. The kit's own refusals (ValueError, RuntimeError)
    are written to be read. An operating-system error says what the system
    said and which path it meant: a path is no secret. Every other error could
    carry docker inspect data or arguments and shows its class only."""
    if isinstance(error, (ValueError, RuntimeError)):
        return str(error)
    if isinstance(error, OSError):
        return safety.os_reason(error) + ". Check that the named path exists and that the calling user may use it"
    return (f"an unexpected internal error ({type(error).__name__}), most likely an answer of Docker that does not "
            "have the expected shape. The step stopped here; run `rehearsal.sh status` before anything else")


def preflight(options, lock=None):
    """Summary without secrets; every refusal is listed, none hidden.

    It changes nothing except what `--init-root` asks for, but its image check
    runs one short container: it holds the operation lock like every step.
    """
    refusals = []

    def step(label, function, *args):
        try:
            return (function(*args), True)
        except (ValueError, RuntimeError, OSError, KeyError, IndexError, TypeError, json.JSONDecodeError) as error:
            refusals.append(f"{label}: {reason(error)}")
            return (None, False)

    ctx = safety.Rehearsal(prepare=False, lock=lock)
    say(f"calling_uid={ctx.uid} calling_gid={ctx.gid} host_platform={ctx.platform}")
    say(f"operation_lock={safety.LOCK_PATH} (held by this preflight; one rehearsal operation at a time on this host)")
    if not ctx.platform.startswith("linux"):
        say("note: this host is not Linux; Docker runs elsewhere (a VM), the host mount-table fence is "
            "unavailable and restore/drill are refused here. Development proof only.")
    loaded = step("selection", ctx.load)[1]
    if loaded:
        mode = os.lstat(ctx.env_file)
        say(f"rehearsal_project={ctx.project}")
        say(f"env_file={ctx.env_file} owner={mode.st_uid} mode={mode.st_mode & 0o7777:04o}")
        if ctx.no_repo:
            say("repository=none (KOMIKA_REHEARSAL_NO_REPO=1: NO backup, NO WAL archiving; local proof only)")
        else:
            say(f"rehearsal_prefix={safety.canonical_prefix(ctx.environment['KOMIKA_R2_PREFIX'])}")
            say(f"production_prefix={safety.canonical_prefix(ctx.environment['KOMIKA_PRODUCTION_R2_PREFIX'])} "
                "(operator-supplied; confirm it against production's own backup configuration)")
    if step("docker endpoint", ctx.docker_endpoint)[1]:
        say(f"docker_context={ctx.context} docker_endpoint={ctx.endpoint}")
        facts = step("docker info", lambda: json.loads(ctx.docker("info", "--format", "{{json .}}")))[0] or {}
        say(f"docker_root_dir={facts.get('DockerRootDir')} server_version={facts.get('ServerVersion')} "
            f"os={facts.get('OperatingSystem')} cpus={facts.get('NCPU')} docker_memory_bytes={facts.get('MemTotal')}")
        say("compose_version=" + (step("compose version", lambda: ctx.docker("compose", "version", "--short").strip())[0] or "unknown"))
        if step("compose file pin", ctx.pin)[1]:
            say(f"compose_file={safety.COMPOSE_FILE} sha256={ctx.compose_digest}")
        if loaded:
            if step("effective topology", ctx.render)[1]:
                say("effective_topology=matches the contract (2 services, 2 labelled volumes, 1 bridge, no ports/binds/host modes)")
            step("resource labels", ctx.labels)
            # A label is no proof of storage: driver, options, mountpoint, real mount sources, overlaps.
            if step("source storage", ctx.storage)[1]:
                say("source_storage=existing source volumes and service containers are plain local rehearsal storage "
                    f"below {ctx.docker_root}/volumes (or absent)")
            step("unresolved jobs", reconciled, ctx)
        fresh = True
        if loaded and options.get("--init-root"):
            # Before the image check creates its short container: a NEW project has nothing yet.
            fresh = step("new project", ctx.require_absent)[1]
            if fresh:
                say(f"new_project=no container, volume or network of {ctx.project} exists")
        if loaded and not fresh:
            say(f"image={safety.IMAGE} not checked: --init-root creates nothing while the project is not new")
        elif loaded:
            # Also compares every kit file in the image with this kit (one container, no network).
            image = step("image", image_id, ctx)
            say(f"image={safety.IMAGE} id={image[0]}")
            if image[1]:
                say(f"image_kit_files=identical to this kit ({len(safety.IMAGE_FILES)} files: {' '.join(safety.IMAGE_FILES)}); "
                    "this proves these files only, not the base image, the installed packages or the runtime")
        if loaded:
            if options.get("--init-root") and fresh and step("init root", ctx.init_root)[1]:
                say("rehearsal_root_created=yes (marker written)")
            root = step("rehearsal root", ctx.fence_root)[0]
            if root:
                info = os.lstat(str(root))
                space = os.statvfs(str(root))
                mount = ctx.root_mount
                say(f"rehearsal_root={root} owner={info.st_uid} mode={info.st_mode & 0o7777:04o} "
                    f"free_bytes={space.f_bavail * space.f_frsize}")
                gate_facts = step("disk gate", ctx.disk_gate, "preflight")[0] or []
                for fact in gate_facts:
                    say(fact)
                say(f"disk_gate_rules=up,backup: floor; import: floor + {safety.IMPORT_GROWTH} x SQLite file size on "
                    f"Docker's data root; restore,drill: floor + {safety.RESTORE_GROWTH} x backup database size on the "
                    "rehearsal root (KOMIKA_REHEARSAL_MIN_FREE_GB sets the floor, 2..64, default 3). These are "
                    "estimates measured once, under the operation lock, before the step: watch free space yourself "
                    "while a step runs")
                say("rehearsal_root_mount=" + (f"{mount['point']} source={mount['source']} fstype={mount['fstype']} "
                                               f"fs_root={mount['root']}" if mount else "unavailable (no Linux mount table)"))
        docker_root = facts.get("DockerRootDir")
        try:
            space = os.statvfs(docker_root)
            say(f"docker_root_free_bytes={space.f_bavail * space.f_frsize}")
        except (OSError, TypeError):
            say("docker_root_free_bytes=unavailable (Docker's data root is not visible to this user/host)")
        try:
            memory = dict(line.split(":", 1) for line in Path("/proc/meminfo").read_text().splitlines())
            say(f"host_memory_total={memory['MemTotal'].strip()} host_memory_available={memory['MemAvailable'].strip()}")
        except (OSError, KeyError, ValueError):
            say("host_memory=unavailable (no /proc/meminfo); see docker_memory_bytes")
        inventory = step("docker inventory", ctx.inventory)[0]
        if inventory:
            for entry in inventory[1]:
                labels = (entry.get("Config") or {}).get("Labels") or {}
                say(f"container {str(entry.get('Name')).lstrip('/')} state={(entry.get('State') or {}).get('Status')} "
                    f"compose_project={labels.get('com.docker.compose.project')} rehearsal_label={labels.get(safety.LABEL)}")
            for entry in inventory[2]:
                say(f"volume {entry.get('Name')} labels={shown(entry.get('Labels'))}")
        networks = step("docker networks", lambda: [json.loads(ctx.docker("network", "inspect", identity))[0]
                        for identity in ctx.docker("network", "ls", "-q", "--no-trunc").split()])[0] or []
        for entry in networks:
            subnets = ",".join(item.get("Subnet", "") for item in (entry.get("IPAM") or {}).get("Config") or [])
            say(f"network {entry.get('Name')} subnets={subnets} labels={shown(entry.get('Labels'))}")
        if loaded:
            conflicts = safety.subnet_conflicts(networks, ctx.project)
            for fixed in (safety.SUBNET, safety.RESTORE_SUBNET):
                users = [(name, own) for subnet, name, own in conflicts if subnet == fixed]
                foreign = [name for name, own in users if not own]
                say(f"subnet {fixed}: " + ("free" if not users else "in use by " + ", ".join(name for name, _ in users)))
                if foreign:
                    refusals.append(f"subnet {fixed} overlaps network {foreign[0]} that is not this rehearsal's")
    for refusal in refusals:
        say("REFUSAL: " + refusal)
    say("PREFLIGHT OK" if not refusals else f"PREFLIGHT REFUSED ({len(refusals)})")
    return 1 if refusals else 0


def shown(labels):
    return "{" + ",".join(f"{key}={str(value)[:80]}" for key, value in sorted((labels or {}).items())) + "}"


HANDLERS = {
    "up": up, "status": status, "psql-check": psql_check, "stanza-create": stanza_create,
    "backup": backup, "check": check, "info": info, "archive-status": archive_status,
    "import": import_sqlite, "verify": verify, "verify-restored": verify_restored,
    "record-settings": record_settings, "drill": marker_drill, "restore": restore,
    "restore-cleanup": restore_cleanup, "sql-marker": sql_marker, "logs": logs,
    "stop-source": stop_source, "destroy-source-volumes": destroy_source_volumes,
    "down": down, "repo-destroy": repo_destroy, "repo-usage": repo_usage,
}


def main(argv=None):
    def interrupted(signum, frame):
        raise SystemExit(128 + signum)
    # A lost terminal (SIGHUP) and SIGTERM unwind like Ctrl-C, so every `finally`
    # that removes a job, metadata or restore container still runs. A signal the
    # caller chose to ignore (nohup for a long import) stays ignored.
    for number in (signal.SIGTERM, signal.SIGHUP):
        if signal.getsignal(number) is not signal.SIG_IGN:
            signal.signal(number, interrupted)
    lock = None
    try:
        name, positional, options = parse(sys.argv[1:] if argv is None else argv)
        if name not in READ_ONLY:
            # Before anything is validated or measured: one operation at a time on
            # this host. Held until the handler and its cleanup have ended.
            lock = safety.OperationLock().acquire(name)
        if name == "preflight":
            return preflight(options, lock)
        ctx = safety.Rehearsal(tolerant=name in ("stop-source", "down"), lock=lock)
        drill.REHEARSAL = ctx
        if lock is not None and name not in CLEANERS:
            reconciled(ctx)
        return HANDLERS[name](ctx, positional, options)
    except Usage as error:
        print(f"rehearsal usage error: {error}\n{USAGE}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130
    except (ValueError, RuntimeError, OSError, KeyError, IndexError, json.JSONDecodeError) as error:
        # Never expose docker inspect data, command arguments or credentials.
        print("rehearsal refused: " + reason(error), file=sys.stderr)
        return 1
    finally:
        if lock is not None:
            lock.release()


if __name__ == "__main__":
    sys.exit(main())
