"""Shared fail-closed rehearsal fences; standard library only.

Inside the image only the project/prefix/argument fence is used (pgbackrest.sh
and hosted.py). Everything else here is host-side and is reachable only through
rehearsal.sh, which builds every Docker command line itself: there is no
pass-through of caller-supplied Docker, Compose or pgBackRest options.
"""
import datetime
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import posixpath
import re
import signal
import stat as stat_module
import subprocess
import sys
import time

KIT = Path(__file__).resolve().parent
LABEL = "cc.komiq.rehearsal"
TARGET_LABEL = "cc.komiq.rehearsal.restore-target"
IMAGE = "komika-postgres:18.6-pgbackrest-2.59"
COMPOSE_FILE = KIT / "rehearsal-compose.yml"
# SHA-256 of rehearsal-compose.yml. test_safety keeps this in step with the file.
COMPOSE_SHA256 = "1378c02cbd18a9f0d29f9d862892eb2ce36ec8a3b08bd01bc2ea69e36e4e3a34"
SUBNET = "172.30.21.0/24"
RESTORE_SUBNET = "172.29.42.0/24"
ROOT_MARKER = ".komika-rehearsal-root"
SERVICES = ("postgres", "postgres-backup")
VOLUMES = {"postgres-data": "/var/lib/postgresql", "postgres-socket": "/var/run/postgresql"}
R2_VARS = ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX",
           "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")
PATH_PATTERN = r"/[A-Za-z0-9_.@+-]+(?:/[A-Za-z0-9_.@+-]+)*"
NAME_PATTERN = r"[A-Za-z0-9_][A-Za-z0-9_.@+-]{0,127}"
# Every kit file the Dockerfile copies into the image. `up` and `preflight`
# compare each one with the image's copy; test_safety keeps this in step with
# the Dockerfile and .dockerignore.
IMAGE_DIRECTORY = "/opt/komika-postgres"
IMAGE_FILES = ("entrypoint.sh", "pgbackrest.sh", "health.sh", "backup.sh", "hosted.py", "safety.py",
               "postgresql.conf")
# Short-lived containers of a rehearsal: importer/verifier jobs and metadata readers.
TRANSIENT_PATTERN = r"-(?:job|meta)-[0-9a-f]{8}"
MARKER_LABEL = r"[A-Za-z0-9_.-]{1,40}"
# Injection points for tests; nothing in the environment or on a command line
# can change them.
PLATFORM = sys.platform
# The one host-wide operation lock (see OperationLock). A fixed path outside
# every rehearsal root; a test replaces this one line in a private kit copy.
LOCK_PATH = ("/run/lock" if PLATFORM.startswith("linux") else "/private/tmp") + "/komika-pg-rehearsal.lock"
# An interrupted client's create request may still be on its way to the daemon.
# The name is watched this long; after that the creation is UNRESOLVED (never
# "absent"), and stop-source/down clear it no earlier than UNRESOLVED_AGE later.
SETTLE_SECONDS = 2.0
UNRESOLVED_AGE = 30.0
# Bounded logs for every container the kit starts itself (Compose services
# carry the same values in the topology contract).
LOGGING = {"driver": "json-file", "options": {"max-file": "3", "max-size": "10m"}}
LOG_OPTIONS = ("--log-driver=json-file", "--log-opt=max-size=10m", "--log-opt=max-file=3")
# What the image itself puts into a container's environment (the pinned base
# image and the Dockerfile's ENV). Every other name of a live container must
# come from the validated model; test_image checks this list inside the image.
IMAGE_ENVIRONMENT = {
    "PATH": r"[A-Za-z0-9_.:/-]+", "LANG": r"[A-Za-z0-9_.@-]+", "GOSU_VERSION": r"[0-9][0-9.]*",
    "PG_MAJOR": r"18", "PG_VERSION": r"18\.6-[A-Za-z0-9.+~-]+",
    "PGBACKREST_CONFIG": r"/run/komika/pgbackrest\.conf",
}
# Docker's own default when a service sets no shm_size (the backup scheduler).
DEFAULT_SHM = 64 << 20


def read_mountinfo():
    return Path("/proc/self/mountinfo").read_text()


# ---- identity and repository prefix (also used inside the image) -------------

def rehearsal_present(environment=None):
    return any(k.startswith("KOMIKA_REHEARSAL_") for k in
               (os.environ if environment is None else environment))


def project_name(value):
    if not re.fullmatch(r"komika-pg-rehearsal-[a-z0-9](?:[a-z0-9-]{0,22}[a-z0-9])?", value or ""):
        raise ValueError("require KOMIKA_REHEARSAL_PROJECT=komika-pg-rehearsal-<1..24 lower-case letters, digits, inner dashes>")
    return value


def canonical_prefix(value):
    parts = [part for part in (value or "").split("/") if part]
    if not parts or any(not re.fullmatch(r"[A-Za-z0-9_-]+", part) for part in parts):
        raise ValueError("repository prefixes must be nonempty, without dot segments or unsafe characters")
    return "/".join(parts)


def prefix_fence(environment):
    rehearsal = canonical_prefix(environment.get("KOMIKA_R2_PREFIX"))
    production = canonical_prefix(environment.get("KOMIKA_PRODUCTION_R2_PREFIX"))
    if not rehearsal.startswith("rehearsal/"):
        raise ValueError("rehearsal R2 prefix must start rehearsal/")
    if (rehearsal == production or rehearsal.startswith(production + "/")
            or production.startswith(rehearsal + "/")):
        raise ValueError("rehearsal and production prefixes must be disjoint (no equality or ancestry)")
    return rehearsal


def environment_fence(environment):
    project = project_name(environment.get("KOMIKA_REHEARSAL_PROJECT"))
    prefix = prefix_fence(environment)
    return project, prefix


def local_s3(environment):
    """True when TLS verification is off for a throwaway S3 store on this host.

    KOMIKA_R2_VERIFY_TLS=0 exists for one case only: a local repository drill
    against a self-signed S3 store. It is accepted only with all three: a
    rehearsal, a project named `*-local`, and an endpoint that is a literal
    private IPv4 address. Every other use is a refusal, never a fallback. The
    hosted path does not call this function and never reads the key.
    """
    value = environment.get("KOMIKA_R2_VERIFY_TLS")
    if value in (None, "", "1"):
        return False
    if value != "0":
        raise ValueError("KOMIKA_R2_VERIFY_TLS accepts only 0 (omit the key to verify TLS)")
    if not rehearsal_present(environment):
        raise ValueError("KOMIKA_R2_VERIFY_TLS=0 is a rehearsal-only setting")
    if not project_name(environment.get("KOMIKA_REHEARSAL_PROJECT")).endswith("-local"):
        raise ValueError("KOMIKA_R2_VERIFY_TLS=0 requires a rehearsal project whose name ends with -local")
    match = re.fullmatch(r"(?:https://)?([0-9]{1,3}(?:\.[0-9]{1,3}){3})", environment.get("KOMIKA_R2_ENDPOINT") or "")
    try:
        address = ipaddress.IPv4Address(match[1]) if match else None
    except ValueError:
        address = None
    if address is None or not any(address in ipaddress.ip_network(cidr)
                                  for cidr in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")):
        raise ValueError("KOMIKA_R2_VERIFY_TLS=0 requires an endpoint that is a literal private IPv4 address "
                         "(10/8, 172.16/12 or 192.168/16)")
    return True


WAL_SEGMENT = r"[0-9A-F]{24}"
# The complete pgBackRest surface of a rehearsal: each command the kit sends,
# with the only options and parameters it sends them with. An option is either a
# literal or `name=` plus the pattern of its value.
PGBACKREST_ALLOWED = {
    "archive-push": ((), (r"pg_wal/[0-9A-Za-z.]{8,64}",)),
    "archive-get": (("--archive-async=n",), (WAL_SEGMENT, r"/run/komika/drill-" + WAL_SEGMENT)),
    "stanza-create": ((), ()),
    "check": ((), ()),
    "info": (("--output=text", "--output=json"), ()),
    "repo-ls": (("--recurse", "--output=json"), ()),
    "backup": (("--type=full", "--type=incr"), ()),
    "stop": ((), ()),
    "stanza-delete": (("--force",), ()),
    # No archive-async here: pgBackRest refuses that option on `restore`.
    "restore": (("--log-level-stderr=detail", "--type=time", "--target-action=promote",
                 ("--set=", r"[0-9]{8}-[0-9]{6}F(?:_[0-9]{8}-[0-9]{6}[DI])?"),
                 # The one form drill.recovery_target() writes: UTC with a numeric offset.
                 ("--target=", r"[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}\+00")), ()),
}


def repository_arguments(args, environment=None):
    """In a rehearsal only the kit's own pgBackRest command lines exist.

    Shape: options, one command, parameters. Anything that is not on the list is
    refused, so `--repo*`, `--config*`, `--reset-*`, `--no-*`, `--stanza`,
    `--pg1-path`, `expire`, `repo-rm` and every option invented later fail here.
    """
    environment = os.environ if environment is None else environment
    if not rehearsal_present(environment):
        return
    # An argv/env override must not redirect pgBackRest away from the checked
    # private config. Configuration and the fixed stanza are wrapper-owned.
    if any(key.startswith("PGBACKREST_") and key != "PGBACKREST_CONFIG" for key in environment):
        raise ValueError("pgBackRest environment overrides are forbidden in rehearsal")
    args = list(args)
    options = []
    while args and args[0].startswith("-"):
        options.append(args.pop(0))
    if not args or args[0] not in PGBACKREST_ALLOWED:
        raise ValueError("pgBackRest command is not on the rehearsal allowlist")
    allowed, parameters = PGBACKREST_ALLOWED[args[0]]
    for option in options:
        if not any(option == rule if isinstance(rule, str) else
                   option.startswith(rule[0]) and re.fullmatch(rule[1], option[len(rule[0]):])
                   for rule in allowed):
            raise ValueError("pgBackRest option is not on the rehearsal allowlist for " + args[0])
    if len(set(option.split("=", 1)[0] for option in options)) != len(options):
        raise ValueError("pgBackRest option is repeated: not on the rehearsal allowlist for " + args[0])
    if len(args) - 1 != len(parameters) or not all(
            isinstance(value, str) and re.fullmatch(pattern, value) for value, pattern in zip(args[1:], parameters)):
        raise ValueError("pgBackRest parameters are not on the rehearsal allowlist for " + args[0])


def invoke(args, **kwargs):
    result = subprocess.run(args, text=True, capture_output=True, **kwargs)
    if result.returncode:
        # Compose config and inspect may contain credentials. Never echo them.
        raise RuntimeError("Docker safety inspection failed; no action authorized")
    return result.stdout


def require_label(kind, name, project, run=invoke):
    args = ["docker", kind, "inspect", name]
    entries = json.loads(run(args))
    if len(entries) != 1:
        raise ValueError("resource identity is ambiguous")
    labels = entries[0].get("Config", {}).get("Labels", {}) if kind == "container" else entries[0].get("Labels", {})
    if (labels or {}).get(LABEL) != project:
        raise ValueError(f"{kind} {name} lacks the selected rehearsal label; refusing access/cleanup")
    return entries[0]


# ---- host paths --------------------------------------------------------------

def canonical_path(value, what):
    """An absolute path that is already resolved: no symlink, dot or odd byte."""
    if not isinstance(value, str) or not re.fullmatch(PATH_PATTERN, value):
        raise ValueError(f"{what} must be an absolute path of letters, digits and _.@+- segments")
    if os.path.normpath(value) != value or os.path.realpath(value) != value:
        raise ValueError(f"{what} must be already resolved (no symlinks or dot segments)")
    return value


def new_child(value, parent, what):
    """A path that does not exist yet, directly inside the already fenced parent."""
    if not isinstance(value, str) or not re.fullmatch(PATH_PATTERN, value):
        raise ValueError(f"{what} must be an absolute path of letters, digits and _.@+- segments")
    directory, name = posixpath.split(value)
    if directory != str(parent) or name.startswith(".") or not re.fullmatch(NAME_PATTERN, name):
        raise ValueError(f"{what} must be a new name directly under KOMIKA_REHEARSAL_ROOT")
    if os.path.lexists(value):
        raise ValueError(f"{what} must not exist yet")
    return Path(value)


# ---- rehearsal env file ------------------------------------------------------

SIZE = r"[1-9][0-9]{0,4}[mg]"
CPUS = r"(?:[1-9]|0\.[0-9]{1,2}|[1-9]\.[0-9]{1,2})"
PIDS = r"[1-9][0-9]{1,3}"
ENV_RULES = {
    "KOMIKA_REHEARSAL_PROJECT": r"[a-z0-9-]+",
    "KOMIKA_REHEARSAL_NO_REPO": r"[01]",
    "KOMIKA_POSTGRES_PASSWORD": r"[A-Za-z0-9_.~-]{24,128}",
    "KOMIKA_PG_INIT_NEW": r"[01]",
    "KOMIKA_R2_ENDPOINT": r"(?:https://)?[A-Za-z0-9.-]+",
    "KOMIKA_R2_BUCKET": r"[a-z0-9][a-z0-9.-]{1,62}",
    "KOMIKA_R2_PREFIX": r"[A-Za-z0-9_/-]+",
    "KOMIKA_PRODUCTION_R2_PREFIX": r"[A-Za-z0-9_/-]+",
    "KOMIKA_R2_KEY": r"[A-Za-z0-9/+=_-]+",
    "KOMIKA_R2_SECRET": r"[A-Za-z0-9/+=_-]+",
    # Only `0`, and only for a `*-local` project with a private literal endpoint.
    "KOMIKA_R2_VERIFY_TLS": r"0",
    "KOMIKA_REHEARSAL_MIN_FREE_GB": r"[1-9][0-9]?",
    "KOMIKA_REHEARSAL_PG_MEMORY": SIZE, "KOMIKA_REHEARSAL_PG_SHM": SIZE,
    "KOMIKA_REHEARSAL_PG_CPUS": CPUS, "KOMIKA_REHEARSAL_PG_PIDS": PIDS,
    "KOMIKA_REHEARSAL_BACKUP_MEMORY": SIZE, "KOMIKA_REHEARSAL_BACKUP_CPUS": CPUS,
    "KOMIKA_REHEARSAL_BACKUP_PIDS": PIDS,
    "KOMIKA_REHEARSAL_JOB_MEMORY": SIZE, "KOMIKA_REHEARSAL_JOB_CPUS": CPUS,
    "KOMIKA_REHEARSAL_JOB_PIDS": PIDS,
    "KOMIKA_RESTORE_MEMORY": SIZE, "KOMIKA_RESTORE_CPUS": CPUS, "KOMIKA_RESTORE_PIDS": PIDS,
}
# (default, ceiling). The env file may set a limit anywhere up to its ceiling.
LIMITS = {
    "PG_MEMORY": ("3g", 4 << 30), "PG_SHM": ("1g", 1 << 30), "PG_CPUS": ("1", 2.0), "PG_PIDS": ("256", 256),
    "BACKUP_MEMORY": ("512m", 512 << 20), "BACKUP_CPUS": ("1", 1.0), "BACKUP_PIDS": ("128", 128),
    "JOB_MEMORY": ("2g", 4 << 30), "JOB_CPUS": ("1", 2.0), "JOB_PIDS": ("256", 256),
}
RESTORE_LIMITS = {"MEMORY": ("3g", 4 << 30), "CPUS": ("1", 2.0), "PIDS": ("128", 256)}
# Free space that must remain on each filesystem a step writes to.
MIN_FREE_GB = ("3", 2, 64)
# Measured on the real database (2.76 GB SQLite file): 3.4 GB PostgreSQL base
# plus up to 4.0 GB of WAL before archiving; a restore writes the backup's
# database size plus recovered WAL.
IMPORT_GROWTH = 2.2
RESTORE_GROWTH = 1.3


def floor_bytes(values):
    raw = values.get("KOMIKA_REHEARSAL_MIN_FREE_GB") or MIN_FREE_GB[0]
    if not re.fullmatch(r"[1-9][0-9]?", raw) or not MIN_FREE_GB[1] <= int(raw) <= MIN_FREE_GB[2]:
        raise ValueError("KOMIKA_REHEARSAL_MIN_FREE_GB must be an integer from 2 to 64")
    return int(raw) << 30


def free_bytes(path):
    """(bytes an unprivileged writer may still use, device) of an existing path."""
    space = os.statvfs(path)
    return space.f_bavail * space.f_frsize, os.stat(path).st_dev


class uninterrupted:
    """Cleanup of our own containers must not be cut short by a second signal."""
    SIGNALS = tuple(getattr(signal, name) for name in ("SIGINT", "SIGTERM", "SIGHUP") if hasattr(signal, name))

    def __enter__(self):
        try:
            self.saved = [(number, signal.signal(number, signal.SIG_IGN)) for number in self.SIGNALS]
        except ValueError:  # Not the main thread: nothing to protect.
            self.saved = []

    def __exit__(self, *exception):
        for number, handler in self.saved:
            signal.signal(number, handler)


def os_reason(error):
    """An operating-system error as text: its class, what the system said and
    the path it meant. A path is no secret; nothing else of the error is shown."""
    paths = [path for path in (error.filename, error.filename2) if isinstance(path, str)]
    return (f"{type(error).__name__}: {error.strerror or 'the operating system refused the operation'}"
            + (f" ({' -> '.join(paths)})" if paths else ""))


# ---- the host-wide operation lock and its record of unresolved creations ------

TRANSIENT_NAME = r"(komika-pg-rehearsal-[a-z0-9](?:[a-z0-9-]{0,22}[a-z0-9])?)" + TRANSIENT_PATTERN
LOCK_DIRECTORY_HELP = (". The kit keeps its one lock file in a directory that is owned by root and sticky "
                       "(drwxrwxrwt, as /run/lock is on Debian and Ubuntu); it never creates or changes that "
                       "directory. Nothing was checked or started")
# What a lock record may claim: a process ID the kernel can have, and a time of
# day between 1970 and 2100. Anything else is not a record of this kit.
PID_MAX = 1 << 22
NOTED_MAX = 4102444800


def lock_directory(path, uid, lstat=os.lstat):
    """The fixed lock file lives in a real directory that nobody else can swap."""
    directory = posixpath.dirname(path)
    if not re.fullmatch(PATH_PATTERN, path) or os.path.realpath(directory) != directory:
        raise ValueError(f"the operation lock directory {directory} must be a real directory, not a link")
    try:
        info = lstat(directory)
    except OSError:
        raise ValueError(f"the operation lock directory {directory} does not exist on this host; "
                         "no rehearsal operation can be serialized here" + LOCK_DIRECTORY_HELP)
    mode = stat_module.S_IMODE(info.st_mode)
    if not stat_module.S_ISDIR(info.st_mode):
        raise ValueError(f"the operation lock directory {directory} is not a directory" + LOCK_DIRECTORY_HELP)
    if info.st_uid not in (0, uid):
        raise ValueError(f"the operation lock directory {directory} is owned by uid {info.st_uid}, not by root"
                         + LOCK_DIRECTORY_HELP)
    if mode & 0o022 and not mode & stat_module.S_ISVTX:
        raise ValueError(f"the operation lock directory {directory} (mode {mode:04o}) is writable by its group or by "
                         "others and is not sticky: another user could replace the lock file" + LOCK_DIRECTORY_HELP)
    return directory


def open_lock(path, uid, create):
    """The lock file's descriptor: never through a link, only the caller's own
    regular single-link file with mode 0600. None when absent and not created."""
    lock_directory(path, uid)
    flags = os.O_RDWR | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    refusal = (f"the operation lock {path} must be a regular file of the caller (uid {uid}) with mode 0600 and one "
               "name; it is something else. Every rehearsal command on this host runs as one user. If no "
               "rehearsal runs, its owner or root removes that file")
    descriptor = None
    if create:
        try:
            descriptor = os.open(path, flags | os.O_CREAT | os.O_EXCL, 0o600)
            os.fchmod(descriptor, 0o600)  # Whatever the caller's umask is.
        except FileExistsError:
            descriptor = None
        except PermissionError:
            directory = posixpath.dirname(path)
            info = os.lstat(directory)
            raise ValueError(f"the caller (uid {uid}) may not create the operation lock {path}: its directory "
                             f"{directory} (owner uid {info.st_uid}, mode {stat_module.S_IMODE(info.st_mode):04o}) "
                             "is not writable for the caller" + LOCK_DIRECTORY_HELP)
        except OSError:
            raise ValueError(refusal)
    if descriptor is None:
        try:
            descriptor = os.open(path, flags)
        except FileNotFoundError:
            if create:
                raise ValueError(refusal)
            return None
        except OSError:  # A link (ELOOP), another user's file (EACCES), a directory.
            raise ValueError(refusal)
    info = os.fstat(descriptor)
    if (not stat_module.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != uid
            or stat_module.S_IMODE(info.st_mode) != 0o600):
        os.close(descriptor)
        raise ValueError(refusal)
    return descriptor


def read_record(descriptor):
    """What the lock file says: the holder and the unresolved creations.

    The first JSON value counts; anything a crashed writer left behind it is
    ignored. Entries that do not have the exact shape are dropped.
    """
    text = os.pread(descriptor, 1 << 16, 0).decode("utf-8", "replace").lstrip()
    try:
        raw = json.JSONDecoder().raw_decode(text)[0] if text else {}
    except ValueError:
        raw = {}
    record = {"holder": None, "unresolved": []}
    if not isinstance(raw, dict):
        return record
    holder = raw.get("holder")
    if (isinstance(holder, dict) and type(holder.get("pid")) is int and 1 < holder["pid"] <= PID_MAX
            and re.fullmatch(r"[a-z-]{1,40}", str(holder.get("subcommand")))
            and re.fullmatch(r"[0-9TZ:-]{1,32}", str(holder.get("since")))):
        record["holder"] = {key: holder[key] for key in ("pid", "subcommand", "since")}
    for entry in raw.get("unresolved") if isinstance(raw.get("unresolved"), list) else []:
        if (isinstance(entry, dict) and isinstance(entry.get("name"), str)
                and re.fullmatch(TRANSIENT_NAME, entry["name"]) and type(entry.get("noted")) in (int, float)
                and 0 <= entry["noted"] <= NOTED_MAX):  # Also false for NaN and for an infinity.
            record["unresolved"].append({"name": entry["name"], "noted": entry["noted"]})
    return record


def settle_wait(entries, now):
    """Seconds that `stop-source` and `down` still give an interrupted creation.

    Never more than UNRESOLVED_AGE and never less than 0, whatever the record
    says: a time in the future (a forged record, a clock set back) cannot make
    a cleanup wait longer than the documented bound.
    """
    if not entries:
        return 0.0
    age = now - max(entry["noted"] for entry in entries)
    return float(min(UNRESOLVED_AGE, max(0.0, UNRESOLVED_AGE - age)))


def unresolved_project(name):
    return re.fullmatch(TRANSIENT_NAME, name)[1]


def read_ledger(path=None, uid=None):
    """(holder, unresolved) for the read-only `status`: never creates or locks."""
    descriptor = open_lock(LOCK_PATH if path is None else path, os.getuid() if uid is None else uid, create=False)
    if descriptor is None:
        return None, []
    try:
        record = read_record(descriptor)
    finally:
        os.close(descriptor)
    return record["holder"], record["unresolved"]


class OperationLock:
    """One rehearsal operation at a time on this host, whatever the project.

    Every subcommand that creates, starts, stops, removes or allocates takes
    this lock before it validates anything and holds it until it has cleaned
    up; free space is measured while it is held. A second invocation does not
    wait: it is refused at once. The kernel drops the lock when the process
    ends, however it ends. The file also records containers whose creation an
    interrupted step could not resolve; only a lock holder writes it.
    """

    def __init__(self, path=None, uid=None):
        self.path = LOCK_PATH if path is None else path
        self.uid = os.getuid() if uid is None else uid
        self.descriptor = None

    def acquire(self, subcommand):
        descriptor = open_lock(self.path, self.uid, create=True)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            holder = read_record(descriptor)["holder"] or {}
            os.close(descriptor)
            who = (f" (pid {holder['pid']}, subcommand {holder['subcommand']}, since {holder['since']})"
                   if holder else "")
            raise ValueError(f"another rehearsal.sh invocation holds the host-wide operation lock {self.path}{who}. "
                             "One operation at a time on this host, never two: nothing was checked or started. "
                             "Wait until it has ended (`rehearsal.sh status` is read-only and works meanwhile), "
                             "then run this subcommand again")
        try:
            # The name must still lead to the file that is locked.
            named = os.lstat(self.path)
            locked = os.fstat(descriptor)
            if (named.st_dev, named.st_ino) != (locked.st_dev, locked.st_ino):
                raise OSError("replaced")
        except OSError:
            os.close(descriptor)
            raise ValueError(f"the operation lock {self.path} was replaced while it was taken; nothing was started")
        self.descriptor = descriptor
        record = read_record(descriptor)
        record["holder"] = {"pid": os.getpid(), "subcommand": subcommand,
                            "since": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")}
        self.write(record)
        return self

    def write(self, record):
        data = (json.dumps(record, sort_keys=True) + "\n").encode()
        os.pwrite(self.descriptor, data, 0)
        os.ftruncate(self.descriptor, len(data))

    def release(self):
        if self.descriptor is None:
            return
        try:
            record = read_record(self.descriptor)
            record["holder"] = None
            self.write(record)
        finally:
            os.close(self.descriptor)  # Closing the description drops the lock.
            self.descriptor = None

    def unresolved(self, project=None):
        entries = read_record(self.descriptor)["unresolved"]
        return [entry for entry in entries if project is None or unresolved_project(entry["name"]) == project]

    def note(self, name):
        """Remember a container whose creation could not be resolved."""
        record = read_record(self.descriptor)
        if not any(entry["name"] == name for entry in record["unresolved"]):
            record["unresolved"].append({"name": name, "noted": time.time()})
            self.write(record)

    def resolve(self, names):
        record = read_record(self.descriptor)
        record["unresolved"] = [entry for entry in record["unresolved"] if entry["name"] not in names]
        self.write(record)


def size_bytes(value):
    if isinstance(value, bool):
        raise ValueError("invalid size")
    if isinstance(value, int):
        return value
    match = re.fullmatch(r"([0-9]+)\s*([kmg]?)(?:i?b)?", str(value).strip().lower())
    if not match:
        raise ValueError("invalid size")
    return int(match[1]) << {"": 0, "k": 10, "m": 20, "g": 30}[match[2]]


def seconds(value):
    if isinstance(value, bool):
        raise ValueError("invalid duration")
    if isinstance(value, (int, float)):
        return float(value)
    match = re.fullmatch(r"(?:([0-9]+)h)?(?:([0-9]+)m)?(?:([0-9]+(?:\.[0-9]+)?)s)?", str(value))
    if not match or not any(match.groups()):
        raise ValueError("invalid duration")
    return int(match[1] or 0) * 3600 + int(match[2] or 0) * 60 + float(match[3] or 0)


def limit(values, prefix, name, table=LIMITS):
    default, ceiling = table[name]
    raw = values.get(prefix + name) or default
    pattern = SIZE if name.endswith(("MEMORY", "SHM")) else CPUS if name.endswith("CPUS") else PIDS
    if not re.fullmatch(pattern, raw):
        raise ValueError(f"invalid value for {prefix}{name}")
    if name.endswith(("MEMORY", "SHM")):
        number = size_bytes(raw)
        if not 64 << 20 <= number <= ceiling:
            raise ValueError(f"{prefix}{name} must be between 64m and its documented ceiling")
    elif name.endswith("CPUS"):
        number = float(raw)
        if not 0.1 <= number <= ceiling:
            raise ValueError(f"{prefix}{name} must be between 0.1 and its documented ceiling")
    else:
        number = int(raw)
        if not 16 <= number <= ceiling:
            raise ValueError(f"{prefix}{name} must be between 16 and its documented ceiling")
    return raw, number


def parse_env_file(path, uid, lstat=os.lstat):
    """Strict KEY=VALUE allowlist, so Compose and this parser cannot disagree."""
    info = lstat(path)
    if not stat_module.S_ISREG(info.st_mode):
        raise ValueError("rehearsal env file must be a regular file, not a link")
    if info.st_uid != uid or info.st_mode & 0o077:
        raise ValueError("rehearsal env file must be owned by the caller with mode 0600")
    values = {}
    text = Path(path).read_bytes().decode("utf-8")
    if "\r" in text or "\0" in text:
        raise ValueError("rehearsal env file must use plain LF line endings")
    for number, line in enumerate(text.split("\n"), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        match = re.fullmatch(r"([A-Z0-9_]+)=(.*)", line)
        if not match:
            raise ValueError(f"env file line {number}: only unquoted KEY=VALUE lines are accepted")
        key, value = match.groups()
        if key not in ENV_RULES:
            raise ValueError(f"env file line {number}: unsupported variable {key}")
        if key in values:
            raise ValueError(f"env file line {number}: duplicate {key}")
        # Never echo a value: it may be a credential.
        if value and not re.fullmatch(ENV_RULES[key], value):
            raise ValueError(f"env file line {number}: invalid value for {key}")
        values[key] = value
    return values


def process_environment(environ):
    """Only three variables select a rehearsal; everything else is in the file."""
    allowed = {"KOMIKA_REHEARSAL_PROJECT", "KOMIKA_REHEARSAL_ROOT", "KOMIKA_COMPOSE_ENV_FILE"}
    for key in sorted(environ):
        if key.startswith(("DOCKER_", "COMPOSE_")):
            raise ValueError(f"{key} is set; rehearsals use only the unmodified default local Docker context")
        if key in allowed:
            continue
        if (key.startswith(("KOMIKA_REHEARSAL_", "KOMIKA_R2_", "KOMIKA_RESTORE_", "KOMIKA_COMPOSE_", "PGBACKREST_"))
                or key in ENV_RULES or key == "KOMIKA_PG_ALLOWED_CIDRS"):
            raise ValueError(f"{key} is set in the process environment; rehearsal settings come only from the env file")


# ---- exact effective Compose topology ---------------------------------------

def expected_model(project, values):
    """The one accepted effective configuration for this project and env file."""
    project_name(project)
    if values.get("KOMIKA_REHEARSAL_PROJECT") != project:
        raise ValueError("env file KOMIKA_REHEARSAL_PROJECT must equal the exported project")
    if not values.get("KOMIKA_POSTGRES_PASSWORD"):
        raise ValueError("env file requires KOMIKA_POSTGRES_PASSWORD")
    no_repo = values.get("KOMIKA_REHEARSAL_NO_REPO") == "1"
    repository = {key: values.get(key, "") for key in R2_VARS + ("KOMIKA_PRODUCTION_R2_PREFIX",)}
    if no_repo:
        if any(repository.values()):
            raise ValueError("KOMIKA_REHEARSAL_NO_REPO=1 refuses every R2 variable and the production prefix")
        if values.get("KOMIKA_R2_VERIFY_TLS"):
            raise ValueError("KOMIKA_REHEARSAL_NO_REPO=1 refuses every R2 variable and the production prefix")
        insecure = False
    else:
        if not all(repository.values()):
            raise ValueError("env file requires all five KOMIKA_R2_* values and KOMIKA_PRODUCTION_R2_PREFIX")
        prefix_fence(repository)
        insecure = local_s3(dict(values, KOMIKA_REHEARSAL_PROJECT=project))
    floor_bytes(values)
    environment = dict(repository, PGDATA="/var/lib/postgresql/18/docker",
                       KOMIKA_R2_VERIFY_TLS="0" if insecure else "1",
                       KOMIKA_POSTGRES_PASSWORD=values["KOMIKA_POSTGRES_PASSWORD"],
                       KOMIKA_PG_INIT_NEW=values.get("KOMIKA_PG_INIT_NEW") or "0",
                       KOMIKA_REHEARSAL_PROJECT=project,
                       KOMIKA_REHEARSAL_NO_REPO="1" if no_repo else "0",
                       KOMIKA_PG_ALLOWED_CIDRS=SUBNET)
    label = {LABEL: project}

    def service(name, prefix, command, test, interval, retries, start, depends):
        memory = limit(values, "KOMIKA_REHEARSAL_", prefix + "_MEMORY")[1]
        return {
            "image": IMAGE, "command": command, "entrypoint": None,
            "container_name": f"{project}-{name}", "environment": environment,
            "healthcheck": {"test": test, "interval": interval, "timeout": 5.0,
                            "retries": retries, "start_period": start},
            "init": True, "labels": label,
            "logging": {"driver": LOGGING["driver"], "options": dict(LOGGING["options"])},
            "mem_limit": memory, "memswap_limit": memory,
            "cpus": limit(values, "KOMIKA_REHEARSAL_", prefix + "_CPUS")[1],
            "pids_limit": limit(values, "KOMIKA_REHEARSAL_", prefix + "_PIDS")[1],
            "shm_size": limit(values, "KOMIKA_REHEARSAL_", "PG_SHM")[1] if name == "postgres" else None,
            "networks": ["rehearsal"], "restart": "no", "stop_grace_period": 240.0,
            "tmpfs": ["/run/komika:mode=0700"],
            "volumes": [["volume", source, target] for source, target in VOLUMES.items()],
            "depends_on": depends,
        }

    return {
        "name": project,
        "services": {
            "postgres": service("postgres", "PG", None, ["CMD", "/opt/komika-postgres/health.sh"],
                                20.0, 8, 120.0, {}),
            "postgres-backup": service("postgres-backup", "BACKUP", ["scheduler"],
                                       ["CMD", "python3", "/opt/komika-postgres/hosted.py", "health"],
                                       60.0, 3, 600.0, {"postgres": "service_healthy"}),
        },
        "volumes": {key: {"name": f"{project}-{key}", "labels": label, "driver": "local"} for key in VOLUMES},
        "networks": {"rehearsal": {"name": f"{project}-network", "labels": label,
                                   "driver": "bridge", "subnets": [SUBNET]}},
    }


def _only(mapping, allowed, where):
    if not isinstance(mapping, dict):
        raise ValueError(f"rehearsal topology: {where} has an unexpected shape")
    extra = sorted(set(mapping) - set(allowed))
    if extra:
        # Names of settings are not secret; values are never printed.
        raise ValueError(f"rehearsal topology: {where} has forbidden setting {extra[0]}")
    return mapping


def _service(name, rendered):
    allowed = ("image", "command", "entrypoint", "container_name", "environment", "healthcheck",
               "init", "labels", "logging", "mem_limit", "memswap_limit", "cpus", "pids_limit",
               "shm_size", "networks", "restart", "stop_grace_period", "tmpfs", "volumes", "depends_on")
    where = f"service {name}"
    # Anything else (ports, privileged, network_mode, cap_add, devices, pid, ipc,
    # userns_mode, volumes_from, user, build, env_file, security_opt, ...) is refused.
    _only(rendered, allowed, where)
    volumes = []
    for entry in rendered.get("volumes") or []:
        _only(entry, ("type", "source", "target", "volume"), where + " volume")
        if entry.get("volume") not in (None, {}):
            raise ValueError(f"rehearsal topology: {where} volume has forbidden options")
        volumes.append([entry.get("type"), entry.get("source"), entry.get("target")])
    networks = rendered.get("networks")
    if isinstance(networks, dict):
        if any(value not in (None, {}) for value in networks.values()):
            raise ValueError(f"rehearsal topology: {where} has forbidden network options")
        networks = sorted(networks)
    health = dict(_only(rendered.get("healthcheck") or {},
                        ("test", "interval", "timeout", "retries", "start_period"), where + " healthcheck"))
    for key in ("interval", "timeout", "start_period"):
        health[key] = seconds(health.get(key))
    depends = {}
    for key, value in (rendered.get("depends_on") or {}).items():
        _only(value, ("condition", "required", "restart"), where + " depends_on")
        if value.get("required", True) is not True or value.get("restart", False) is not False:
            raise ValueError(f"rehearsal topology: {where} has forbidden depends_on options")
        depends[key] = value.get("condition")
    tmpfs = rendered.get("tmpfs")
    shm = rendered.get("shm_size")
    return {
        "image": rendered.get("image"), "command": rendered.get("command"),
        "entrypoint": rendered.get("entrypoint"), "container_name": rendered.get("container_name"),
        "environment": rendered.get("environment"), "healthcheck": health,
        "init": rendered.get("init"), "labels": rendered.get("labels"), "logging": rendered.get("logging"),
        "mem_limit": size_bytes(rendered.get("mem_limit")),
        "memswap_limit": size_bytes(rendered.get("memswap_limit")),
        "cpus": float(rendered.get("cpus")) if not isinstance(rendered.get("cpus"), bool) else None,
        "pids_limit": rendered.get("pids_limit"),
        "shm_size": None if shm is None else size_bytes(shm),
        "networks": networks, "restart": rendered.get("restart"),
        "stop_grace_period": seconds(rendered.get("stop_grace_period")),
        "tmpfs": [tmpfs] if isinstance(tmpfs, str) else tmpfs,
        "volumes": volumes, "depends_on": depends,
    }


def normalize(rendered):
    """Reduce `docker compose config --format json` to the contract's shape.

    Unknown settings are refused, so a new way to reach the host can never pass
    by being absent from a deny list.
    """
    _only(rendered, ("name", "services", "volumes", "networks"), "project")
    services = rendered.get("services")
    _only(services, SERVICES, "services")
    volumes = {}
    for key, value in (rendered.get("volumes") or {}).items():
        _only(value, ("name", "labels", "driver"), f"volume {key}")  # no driver_opts, no external
        volumes[key] = {"name": value.get("name"), "labels": value.get("labels"), "driver": value.get("driver")}
    networks = {}
    for key, value in (rendered.get("networks") or {}).items():
        _only(value, ("name", "labels", "driver", "ipam"), f"network {key}")  # not external, no options
        ipam = _only(value.get("ipam") or {}, ("config",), f"network {key} ipam")
        subnets = []
        for item in ipam.get("config") or []:
            _only(item, ("subnet",), f"network {key} ipam")
            subnets.append(item.get("subnet"))
        networks[key] = {"name": value.get("name"), "labels": value.get("labels"),
                         "driver": value.get("driver"), "subnets": subnets}
    return {"name": rendered.get("name"),
            "services": {name: _service(name, services[name]) for name in services},
            "volumes": volumes, "networks": networks}


def difference(actual, expected, where="project"):
    """Path of the first difference; values are never returned (may be secret)."""
    if isinstance(expected, dict) and isinstance(actual, dict):
        for key in sorted(set(expected) | set(actual)):
            if key not in expected or key not in actual:
                return f"{where}.{key}"
            found = difference(actual[key], expected[key], f"{where}.{key}")
            if found:
                return found
        return None
    if type(actual) is not type(expected) and not (
            isinstance(actual, (int, float)) and isinstance(expected, (int, float))
            and not isinstance(actual, bool) and not isinstance(expected, bool)):
        return where
    return None if actual == expected else where


def check_topology(rendered, project, values):
    try:
        actual = normalize(rendered)
    except (TypeError, AttributeError, KeyError):
        raise ValueError("rehearsal topology: rendered configuration has an unexpected shape")
    except ValueError as error:
        raise ValueError(str(error) if str(error).startswith("rehearsal topology") else f"rehearsal topology: {error}")
    found = difference(actual, expected_model(project, values))
    if found:
        raise ValueError(f"rehearsal topology: effective Compose configuration differs from the contract at {found}")


def compose_pin(path=None, expected=None):
    digest = hashlib.sha256(Path(path or COMPOSE_FILE).read_bytes()).hexdigest()
    if digest != (expected or COMPOSE_SHA256):
        raise ValueError("rehearsal-compose.yml does not match the SHA-256 pinned in safety.py")
    return digest


def volume_path(docker_root, name):
    """Where a plain local volume of that name lives, and nowhere else."""
    return f"{docker_root}/volumes/{name}/_data"


def source_mounts(entry):
    """Every mount of a container as the daemon reports it, the source included."""
    return sorted((str(m.get("Type")), str(m.get("Name")), str(m.get("Source")), str(m.get("Destination")),
                   str(m.get("Driver"))) for m in entry.get("Mounts") or [])


def wanted_mounts(project, docker_root):
    """The two labelled local volumes, each backed by its own directory below Docker's data root."""
    return sorted(("volume", f"{project}-{key}", volume_path(docker_root, f"{project}-{key}"), target, "local")
                  for key, target in VOLUMES.items())


def environment_problems(actual_items, expected):
    """Names of a live container's environment that are outside the contract.

    Every name must be one the validated model sets (with the model's value) or
    one the image itself sets. That refuses each PG*, PGBACKREST_*, KOMIKA_*
    and loader variable that was added to a container. Returns
    (changed model values?, names outside the contract); never a value.
    """
    names, changed, outside = set(), False, set()
    for item in actual_items or []:
        key, separator, value = str(item).partition("=")
        if not separator or key in names:
            outside.add(key[:60])  # Malformed or given twice: which one counts is unclear.
            continue
        names.add(key)
        if key in expected:
            # INIT_NEW may be returned to 0 in the env file without a recreate.
            changed = changed or (value not in ("0", "1") if key == "KOMIKA_PG_INIT_NEW" else value != expected[key])
        elif key not in IMAGE_ENVIRONMENT or not re.fullmatch(IMAGE_ENVIRONMENT[key], value):
            outside.add(key[:60])
    changed = changed or any(key not in names for key in expected if key != "KOMIKA_PG_INIT_NEW")
    return changed, sorted(outside)


def cpu_limit(host):
    """The CPU ceiling a live container really has, in CPUs; 0 means none."""
    nano, quota, period = (host.get(key) or 0 for key in ("NanoCpus", "CpuQuota", "CpuPeriod"))
    if any(type(value) is not int or value < 0 for value in (nano, quota, period)):
        return 0
    if nano:
        return nano / 1e9 if not quota else 0  # Both at once is no state Docker creates.
    return quota / (period or 100000) if quota else 0


def check_container(entry, project, service, expected, docker_root):
    """A live rehearsal container must equal the contract before any exec/stop.

    `expected` is this service in the validated model (expected_model): the
    container that Compose really created must carry the same environment
    (repository prefix and identity included) and the same memory, swap, CPU,
    PID, shm and logging limits; a container whose limits were removed or
    raised afterwards is refused. Its mounts must be the two labelled volumes
    with their real source directories below `docker_root`.
    """
    name = f"{project}-{service}"
    host = entry.get("HostConfig") or {}
    config = entry.get("Config") or {}
    changed, outside = environment_problems(config.get("Env"), expected["environment"])
    ports = (entry.get("NetworkSettings") or {}).get("Ports") or {}
    log = host.get("LogConfig") or {}
    shm = DEFAULT_SHM if expected.get("shm_size") is None else expected["shm_size"]
    problems = {
        "name": entry.get("Name") != "/" + name,
        "label": (config.get("Labels") or {}).get(LABEL) != project,
        "environment (run `rehearsal.sh up` to recreate)": changed,
        "environment names outside the contract: " + " ".join(outside[:5]): bool(outside),
        "memory limit": type(host.get("Memory")) is not int or host["Memory"] != expected["mem_limit"],
        "swap limit": type(host.get("MemorySwap")) is not int or host["MemorySwap"] != expected["memswap_limit"],
        "CPU limit": abs(cpu_limit(host) - float(expected["cpus"])) > 1e-6,
        "PID limit": type(host.get("PidsLimit")) is not int or host["PidsLimit"] != expected["pids_limit"],
        "shm size": host.get("ShmSize") != shm,
        "logging": (log.get("Type"), log.get("Config") or {}) != (expected["logging"]["driver"], expected["logging"]["options"]),
        "restart policy": (host.get("RestartPolicy") or {}).get("Name") not in (None, "", "no"),
        "image": config.get("Image") != IMAGE,
        "user": bool(config.get("User")),
        "entrypoint": config.get("Entrypoint") != ["/opt/komika-postgres/entrypoint.sh"],
        "command": config.get("Cmd") != (["postgres"] if service == "postgres" else ["scheduler"]),
        "privileged": host.get("Privileged") is not False,
        "network mode": host.get("NetworkMode") != f"{project}-network",
        "published ports": bool(host.get("PortBindings")) or bool(host.get("PublishAllPorts")) or any(ports.values()),
        "capabilities": bool(host.get("CapAdd")),
        "devices": bool(host.get("Devices")) or bool(host.get("DeviceRequests")),
        "volumes-from": bool(host.get("VolumesFrom")),
        "security options": bool(host.get("SecurityOpt")),
        "host namespaces": bool(host.get("PidMode")) or bool(host.get("UTSMode")) or bool(host.get("UsernsMode"))
                           or host.get("IpcMode") not in ("", "private", "shareable", None),
        "tmpfs": sorted(host.get("Tmpfs") or {}) != ["/run/komika"],
        # Every mount, of any type, must be one of the two labelled volumes, and
        # its SOURCE must be that volume's own directory: a name and a
        # destination alone do not say which bytes are mounted.
        "mounts": source_mounts(entry) != wanted_mounts(project, docker_root),
        "networks": sorted((entry.get("NetworkSettings") or {}).get("Networks") or {}) != [f"{project}-network"],
    }
    wrong = [key for key, bad in problems.items() if bad]
    if wrong:
        raise ValueError(f"container {name} does not match the rehearsal contract ({', '.join(wrong)}); refusing to use it")
    return entry


def source_storage_problems(project, docker_root, containers, volumes, platform=None, mountinfo=None):
    """Why an existing source volume or service container is NOT rehearsal storage.

    A label says who created a resource, not what it is backed by. A volume of
    this project is accepted only when it is a plain `local` volume without
    driver options whose mountpoint is exactly its own directory below
    Docker's data root, when nothing else is mounted at or above that
    directory, and when no container other than the project's two services
    mounts it or any directory that overlaps it. The service containers' real
    mount sources must be those directories. Returns [(kind, name, reason)].
    """
    platform = PLATFORM if platform is None else platform
    if (not isinstance(docker_root, str) or not re.fullmatch(PATH_PATTERN, docker_root)
            or posixpath.normpath(docker_root) != docker_root or docker_root == "/"):
        raise ValueError("Docker did not report a plain absolute data root; source storage cannot be judged")
    names = [f"{project}-{key}" for key in VOLUMES]
    services = [f"{project}-{service}" for service in SERVICES]
    table = root_mount = None
    if platform.startswith("linux"):
        table = parse_mountinfo(read_mountinfo() if mountinfo is None else mountinfo)
        root_mount = backing(docker_root, table)[0]
    problems = []
    present = {}
    for volume in volumes:
        name = volume.get("Name")
        if name not in names:
            continue
        path = volume_path(docker_root, name)
        present[name] = path
        if (volume.get("Labels") or {}).get(LABEL) != project:
            problems.append(("volume", name, "it lacks the selected rehearsal label"))
        if volume.get("Driver") != "local":
            problems.append(("volume", name, "its driver is not `local`"))
        if volume.get("Options"):
            problems.append(("volume", name, "it has driver options (a `device`, `o` or `type` option can make "
                                             "any directory of the host its storage)"))
        if volume.get("Mountpoint") != path:
            problems.append(("volume", name, f"its mountpoint is not its own directory {path}"))
        elif table is not None and backing(path, table)[0] is not root_mount:
            problems.append(("volume", name, f"another filesystem or bind mount is mounted at "
                                             f"{backing(path, table)[0]['point']}, at or above its directory {path}"))
    foreign = [(f"volume {volume.get('Name')}", volume["Mountpoint"]) for volume in volumes
               if volume.get("Name") not in names and volume.get("Mountpoint")]
    wanted = wanted_mounts(project, docker_root)
    for container in containers:
        name = str(container.get("Name")).lstrip("/")
        labels = (container.get("Config") or {}).get("Labels") or {}
        if name in services and labels.get(LABEL) == project:
            if source_mounts(container) != wanted:
                problems.append(("container", name, "its mounts are not exactly the two rehearsal volumes with their "
                                                    f"own source directories below {docker_root}/volumes"))
            continue
        for mount in container.get("Mounts") or []:
            if mount.get("Name") in names:
                problems.append(("volume", mount["Name"], f"container {name}, which is not a service of this "
                                                          "rehearsal, mounts it"))
            if mount.get("Source"):
                foreign.append((f"container {name} mount", str(mount["Source"])))
    for name, path in present.items():
        mine = backing(path, table)[1] if table is not None else None
        for what, raw in foreign:
            resolved = os.path.realpath(raw)
            if overlap(path, resolved) or overlap(os.path.realpath(path), resolved):
                problems.append(("volume", name, f"its directory {path} overlaps {what} ({raw})"))
            elif mine is not None:
                other = backing(resolved, table)[1]
                if other[0] == mine[0] and overlap(mine[1], other[1]):
                    problems.append(("volume", name, f"its directory {path} is the same backing directory as "
                                                     f"{what} ({raw}) in the host mount table"))
    return problems


# ---- host mount identity fence ----------------------------------------------

def parse_mountinfo(text):
    def plain(value):
        return re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), value)
    mounts = []
    for line in text.splitlines():
        fields = line.split(" ")
        if "-" not in fields[6:] or len(fields) < 10:
            raise ValueError("unreadable host mount table")
        tail = fields.index("-", 6)
        mounts.append({"device": fields[2], "root": plain(fields[3]), "point": plain(fields[4]),
                       "fstype": fields[tail + 1], "source": plain(fields[tail + 2])})
    if not mounts:
        raise ValueError("empty host mount table")
    return mounts


def backing(path, mounts):
    """(mount, (device, path inside that filesystem)); needs no permission on path."""
    best = None
    for mount in mounts:  # Later entries shadow earlier ones on the same point.
        point = mount["point"]
        if point == "/" or path == point or path.startswith(point + "/"):
            if best is None or len(point) >= len(best["point"]):
                best = mount
    if best is None:
        raise ValueError("host mount table has no mount for a fenced path")
    relative = posixpath.relpath(path, best["point"])
    inner = best["root"] if relative == "." else posixpath.join(best["root"], relative)
    return best, (best["device"], posixpath.normpath(inner))


def overlap(first, second):
    return first == second or first == "/" or second == "/" or \
        first.startswith(second + "/") or second.startswith(first + "/")


def protected_paths(docker_root, containers, volumes, project=None, root=None):
    """Docker's data root, EVERY volume, and every mount source of other containers."""
    protected = [("Docker data root", docker_root)]
    for volume in volumes:
        if volume.get("Mountpoint"):
            protected.append((f"volume {volume.get('Name')}", volume["Mountpoint"]))
    for container in containers:
        labels = (container.get("Config") or {}).get("Labels") or {}
        for mount in container.get("Mounts") or []:
            source = mount.get("Source")
            if not source:
                continue
            # Only this rehearsal's own directories below its root are exempt; a
            # label alone never exempts a mount that lies anywhere else.
            if (project and root and labels.get(LABEL) == project
                    and os.path.realpath(source).startswith(str(root) + "/")):
                continue
            protected.append((f"container {str(container.get('Name')).lstrip('/')} mount", source))
    return protected


def host_fence(root, protected, *, uid, require_owner=True, require_marker=True, prospective=False,
               plain_parent=False, stat=os.stat, lstat=os.lstat, mountinfo=None, platform=None, homes=None):
    """Fence a rehearsal root (or a prospective one) by path, inode and mount table.

    Returns the mount line used, or None where the host has no Linux mount table.
    """
    platform = PLATFORM if platform is None else platform
    root = Path(canonical_path(str(root), "KOMIKA_REHEARSAL_ROOT"))
    if homes is None:
        import pwd
        homes = [entry.pw_dir for entry in pwd.getpwall()] + [os.environ.get("HOME", "")]
    # plain_parent: the hosted recovery path fences the parent of its target and
    # keeps its earlier freedom to restore beside other files.
    if not plain_parent and (str(root) == "/" or any(home and os.path.realpath(home) == str(root) for home in homes)):
        raise ValueError("rehearsal root must not be / or a home directory itself")
    if not prospective:
        info = lstat(str(root))
        if not stat_module.S_ISDIR(info.st_mode):
            raise ValueError("rehearsal root must be an existing directory")
        if require_owner and (info.st_uid != uid or stat_module.S_IMODE(info.st_mode) != 0o700):
            raise ValueError("rehearsal root must be owned by the caller with mode 0700")
        if require_marker:
            try:
                marker = lstat(str(root / ROOT_MARKER))
                content = (root / ROOT_MARKER).read_text() if stat_module.S_ISREG(marker.st_mode) else ""
            except OSError:
                marker, content = None, ""
            if marker is None or marker.st_uid != uid or content != marker_text(uid):
                raise ValueError("rehearsal root lacks its marker; create the root with `rehearsal.sh preflight --init-root`")
    chain = ([] if prospective else [root]) + list(root.parents)
    identities = {}
    for ancestor in chain:
        info = stat(str(ancestor))
        identities[(info.st_dev, info.st_ino)] = ancestor
    own = None if prospective else next(iter(identities))
    table = None
    if platform.startswith("linux"):
        table = parse_mountinfo(read_mountinfo() if mountinfo is None else mountinfo)
        mount, root_backing = backing(str(root), table)
    else:
        mount, root_backing = None, None
    for what, raw in protected:
        resolved = os.path.realpath(raw)
        refusal = f"rehearsal root overlaps {what} ({raw})"
        if overlap(str(root), resolved):
            raise ValueError(refusal)
        # Same inode through another path: a bind-mount alias, in either direction.
        visible = None
        for candidate in [Path(resolved)] + list(Path(resolved).parents):
            try:
                info = stat(str(candidate))
            except PermissionError:
                continue
            except OSError:
                if candidate == Path(resolved):
                    visible = False
                continue
            if candidate == Path(resolved):
                visible = True
                if (info.st_dev, info.st_ino) in identities:
                    raise ValueError(refusal + " by filesystem identity (bind-mount alias)")
            if own is not None and (info.st_dev, info.st_ino) == own:
                raise ValueError(refusal + " by filesystem identity (bind-mount alias)")
        if table is not None:
            if what == "Docker data root" and visible is False:
                raise ValueError("Docker's data root is not visible on this host; a local daemon is required")
            _, other = backing(resolved, table)
            if other[0] == root_backing[0] and overlap(root_backing[1], other[1]):
                raise ValueError(refusal + " in the host mount table (same backing directory)")
    return mount


def marker_text(uid):
    return f"komika-rehearsal-root v1 uid={uid}\n"


def subnet_conflicts(networks, project):
    """Other Docker networks that overlap one of the two fixed rehearsal subnets."""
    conflicts = []
    for network in networks:
        own = ((network.get("Labels") or {}).get(LABEL) == project)
        for item in ((network.get("IPAM") or {}).get("Config") or []):
            try:
                subnet = ipaddress.ip_network(item.get("Subnet", ""), strict=False)
            except ValueError:
                continue
            for fixed in (SUBNET, RESTORE_SUBNET):
                if subnet.version == 4 and subnet.overlaps(ipaddress.ip_network(fixed)):
                    conflicts.append((fixed, network.get("Name"), own))
    return conflicts


# ---- the validated selection --------------------------------------------------

class Rehearsal:
    """One validated rehearsal selection. Constructing it runs every check that
    must precede a subcommand and never changes Docker or host state."""

    def __init__(self, environ=None, run=invoke, prepare=True, tolerant=False, lock=None):
        self.environ = dict(os.environ if environ is None else environ)
        self.run = run
        self.uid, self.gid = os.getuid(), os.getgid()
        self.platform = PLATFORM
        # The held OperationLock, or None for the read-only subcommands.
        self.lock = lock
        # tolerant (stop-source, down): an object that lacks our label is listed
        # here and left alone, instead of stopping the cleanup of everything else.
        # `unsafe` lists source storage that is not rehearsal storage: these two
        # subcommands report it and still stop the containers that use it.
        self.tolerant, self.foreign, self.unsafe = tolerant, [], []
        self.child_env = {"PATH": self.environ.get("PATH", os.defpath),
                          "HOME": self.environ.get("HOME", "/"), "LC_ALL": "C"}
        self.context = self.endpoint = self.project = self.values = self.model = None
        self.root = self.root_mount = self.docker_root = None
        if prepare:
            for step in (self.load, self.docker_endpoint, self.pin, self.render, self.labels, self.storage):
                step()

    # -- steps (preflight runs them one by one to list every refusal) --
    def load(self):
        process_environment(self.environ)
        self.project = project_name(self.environ.get("KOMIKA_REHEARSAL_PROJECT"))
        self.env_file = canonical_path(self.environ.get("KOMIKA_COMPOSE_ENV_FILE"), "KOMIKA_COMPOSE_ENV_FILE")
        if not os.path.lexists(self.env_file):
            raise ValueError(f"the env file {self.env_file} does not exist: KOMIKA_COMPOSE_ENV_FILE must name this "
                             "rehearsal's private env file (a regular file of the caller, mode 0600)")
        self.values = parse_env_file(self.env_file, self.uid)
        self.model = expected_model(self.project, self.values)
        self.no_repo = self.values.get("KOMIKA_REHEARSAL_NO_REPO") == "1"
        self.containers = {service: f"{self.project}-{service}" for service in SERVICES}
        self.volumes = [f"{self.project}-{key}" for key in VOLUMES]
        self.network = f"{self.project}-network"
        self.environment = self.model["services"]["postgres"]["environment"]

    def docker_endpoint(self):
        name = self.docker("context", "show").strip()
        if not re.fullmatch(r"[A-Za-z0-9_.-]{1,64}", name):
            raise ValueError("unexpected Docker context name")
        endpoint = json.loads(self.docker("context", "inspect", name))[0]["Endpoints"]["docker"]["Host"]
        # On the Linux target only the default context is accepted. A developer
        # machine whose daemon lives in a VM may use its own local-socket context.
        if self.platform.startswith("linux") and name != "default":
            raise ValueError("only the default local Docker context is allowed")
        if not endpoint.startswith("unix:///") or not re.fullmatch(PATH_PATTERN, endpoint[7:]):
            raise ValueError("local Docker daemon (unix socket) required")
        if not stat_module.S_ISSOCK(os.stat(endpoint[7:]).st_mode):
            raise ValueError("Docker endpoint is not a local socket")
        self.context, self.endpoint = name, endpoint
        # Every later call is pinned to the checked socket.
        self.child_env["DOCKER_HOST"] = endpoint

    def pin(self):
        self.compose_digest = compose_pin()

    def render(self):
        self.compose_base = ["compose", "--env-file", self.env_file, "-f", str(COMPOSE_FILE), "-p", self.project]
        rendered = json.loads(self.docker(*self.compose_base, "config", "--format", "json"))
        check_topology(rendered, self.project, self.values)

    def labels(self):
        """Existing resources with our fixed names must already carry our label."""
        names = {
            "container": set(self.docker("ps", "-a", "--format", "{{.Names}}").split()),
            "volume": set(self.docker("volume", "ls", "--format", "{{.Name}}").split()),
            "network": set(self.docker("network", "ls", "--format", "{{.Name}}").split()),
        }
        def labelled(kind, name):
            try:
                require_label(kind, name, self.project, self.inspect)
            except ValueError as error:
                if not self.tolerant:
                    raise
                self.foreign.append((kind, name, str(error)))

        for kind, wanted in (("container", self.containers.values()), ("volume", self.volumes),
                             ("network", [self.network])):
            for name in wanted:
                if name in names[kind]:
                    labelled(kind, name)
        # Compose finds containers by its project label, not by container_name.
        for identity in self.docker("ps", "-a", "-q", "--filter",
                                    "label=com.docker.compose.project=" + self.project).split():
            labelled("container", identity)
        self.existing = names

    def storage(self):
        """Existing source volumes and service containers must BE rehearsal storage.

        Runs before Compose and before any use of a volume: a volume that
        carries our label but is bound to another directory, or whose
        directory is an alias of a foreign mount, is refused here.
        """
        self.docker_root, containers, volumes = self.inventory()
        for kind, name, reason in source_storage_problems(self.project, self.docker_root, containers, volumes,
                                                          self.platform):
            if not self.tolerant:
                raise ValueError(f"{kind} {name} is not rehearsal storage: {reason}. Its label does not prove what it "
                                 "is backed by; refusing every use. Inspect it yourself; no subcommand removes it")
            self.unsafe.append((kind, name, reason))

    def require_absent(self):
        """`preflight --init-root` starts a NEW project: none of its names may exist.

        Containers, volumes and networks with one of the project's names, its
        rehearsal label or its Compose project label all count.
        """
        label, compose = f"label={LABEL}={self.project}", f"label=com.docker.compose.project={self.project}"
        transient = re.escape(self.project) + TRANSIENT_PATTERN
        found = []
        for name in self.docker("ps", "-a", "--format", "{{.Names}}").split():
            if name in self.containers.values() or re.fullmatch(transient, name):
                found.append(f"container {name}")
        for filter_ in (label, compose):
            found += [f"container {identity[:12]} (label)" for identity in
                      self.docker("ps", "-a", "-q", "--filter", filter_).split()]
        found += [f"volume {name}" for name in self.docker("volume", "ls", "--format", "{{.Name}}").split()
                  if name in self.volumes]
        found += [f"volume {name} (label)" for name in
                  self.docker("volume", "ls", "--format", "{{.Name}}", "--filter", label).split()]
        found += [f"network {name}" for name in self.docker("network", "ls", "--format", "{{.Name}}").split()
                  if name == self.network]
        found += [f"network {name} (label)" for name in
                  self.docker("network", "ls", "--format", "{{.Name}}", "--filter", label).split()]
        if found:
            raise ValueError("--init-root starts a NEW rehearsal project, and resources of " + self.project
                             + " already exist: " + ", ".join(sorted(set(found))[:12])
                             + ". Choose a new project name (with a new prefix and root), or finish the old "
                               "rehearsal first (stop-source, destroy-source-volumes, restore-cleanup, down)")

    # -- Docker access: fixed binary, pinned socket, minimal environment --
    def docker(self, *args, **kwargs):
        return self.run(["docker", *args], env=self.child_env, **kwargs)

    def inspect(self, args, **kwargs):
        return self.run(args, env=self.child_env, **kwargs)

    def stream(self, *args, input=None):
        """Run with the caller's terminal attached; returns the exit status."""
        return subprocess.run(["docker", *args], env=self.child_env, input=input, text=True).returncode

    def container(self, service, running=None, contract=True):
        """The label- and contract-checked container entry, or None if absent."""
        name = self.containers[service]
        if name not in self.docker("ps", "-a", "--format", "{{.Names}}").split():
            if running:
                raise ValueError(f"rehearsal {service} container is not present; run `rehearsal.sh up`")
            return None
        entry = require_label("container", name, self.project, self.inspect)
        if contract:
            if self.docker_root is None:
                self.storage()
            check_container(entry, self.project, service, self.model["services"][service], self.docker_root)
        if running and not (entry.get("State") or {}).get("Running"):
            raise ValueError(f"rehearsal {service} container is not running; run `rehearsal.sh up`")
        return entry

    def inventory(self):
        docker_root = json.loads(self.docker("info", "--format", "{{json .DockerRootDir}}"))
        if not isinstance(docker_root, str) or not docker_root.startswith("/"):
            raise ValueError("Docker did not report its data root")
        docker_root = posixpath.normpath(docker_root)
        ids = self.docker("ps", "-a", "-q", "--no-trunc").split()
        containers = json.loads(self.docker("container", "inspect", *ids)) if ids else []
        names = self.docker("volume", "ls", "-q").split()
        volumes = json.loads(self.docker("volume", "inspect", *names)) if names else []
        return docker_root, containers, volumes

    def fence_root(self, prospective=False):
        value = self.environ.get("KOMIKA_REHEARSAL_ROOT")
        if not value:
            raise ValueError("require KOMIKA_REHEARSAL_ROOT (create it with `rehearsal.sh preflight --init-root`)")
        root = Path(canonical_path(value, "KOMIKA_REHEARSAL_ROOT")) if not prospective else Path(value)
        if str(root) == "/" or LOCK_PATH.startswith(str(root) + "/"):
            raise ValueError("rehearsal root must not contain the operation lock")
        if not prospective and not os.path.lexists(str(root)):
            raise ValueError(f"the rehearsal root {root} does not exist: run `rehearsal.sh preflight --init-root` "
                             "first (it creates the directory named by KOMIKA_REHEARSAL_ROOT and marks it)")
        docker_root, containers, volumes = self.inventory()
        self.docker_root = docker_root
        protected = protected_paths(docker_root, containers, volumes, self.project, root)
        self.root_mount = host_fence(root, protected, uid=self.uid, prospective=prospective,
                                     platform=self.platform)
        if not prospective:
            self.root = root
        return root

    def init_root(self):
        """Create a NEW root with its marker; the only way a root comes to exist."""
        value = self.environ.get("KOMIKA_REHEARSAL_ROOT") or ""
        if not re.fullmatch(PATH_PATTERN, value):
            raise ValueError("KOMIKA_REHEARSAL_ROOT must be an absolute path of letters, digits and _.@+- segments")
        parent, name = posixpath.split(value)
        canonical_path(parent, "the parent of KOMIKA_REHEARSAL_ROOT")
        if name.startswith(".") or os.path.lexists(value):
            raise ValueError("--init-root creates a NEW directory; the path exists or is hidden")
        self.fence_root(prospective=True)
        os.mkdir(value, 0o700)
        descriptor = os.open(posixpath.join(value, ROOT_MARKER),
                             os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600)
        with os.fdopen(descriptor, "w") as marker:
            marker.write(marker_text(self.uid))

    def new_path(self, value, what):
        return new_child(value, self.root or self.fence_root(), what)

    def disk_gate(self, step, docker_bytes=0, root_bytes=0, advice=""):
        """Refuse a step that would leave less than the floor on a filesystem it fills.

        docker_bytes and root_bytes are what the step is expected to write below
        Docker's data root (volumes) and below the rehearsal root (restore
        target, work directories). Returns one fact line per filesystem.
        """
        floor = floor_bytes(self.values)
        root = self.root or self.fence_root()
        places = [("the rehearsal root", str(root), root_bytes)]
        try:
            free_bytes(self.docker_root)
            places.insert(0, ("Docker's data root", self.docker_root, docker_bytes))
        except (OSError, TypeError):
            if self.platform.startswith("linux"):
                raise ValueError("disk gate: free space of Docker's data root cannot be measured; refusing " + step)
        filesystems = {}
        for what, path, extra in places:
            free, device = free_bytes(path)
            entry = filesystems.setdefault(device, {"what": [], "free": free, "need": floor})
            entry["what"].append(f"{what} ({path})")
            entry["need"] += int(extra)
            entry["free"] = min(entry["free"], free)
        facts = []
        for entry in filesystems.values():
            where = " and ".join(entry["what"])
            if entry["free"] < entry["need"]:
                raise ValueError(f"disk gate: {step} needs {entry['need']} free bytes on the filesystem of {where}, "
                                 f"available {entry['free']} (floor {floor}){advice}")
            facts.append(f"disk_gate step={step} needed_bytes={entry['need']} available_bytes={entry['free']} "
                         f"floor_bytes={floor} filesystem_of={where}")
        if len(places) == 1:
            facts.append("disk_gate note: Docker's data root is not visible on this host (Docker runs elsewhere); "
                         "only the rehearsal root was measured")
        return facts

    def source_present(self):
        """True while a source volume of this rehearsal still occupies the disk."""
        return bool(set(self.volumes) & set(self.docker("volume", "ls", "--format", "{{.Name}}").split()))

    def transient_name(self, kind):
        """A new name for one of this project's own short-lived containers."""
        if kind not in ("job", "meta"):
            raise ValueError("unknown transient container kind")
        return f"{self.project}-{kind}-{os.urandom(4).hex()}"

    def remove_transient(self, name, settle=False):
        """Remove one job/metadata container of this project if it still exists.

        Called in a `finally`: an interrupted client must not leave its container
        behind. Label and exact name pattern are checked before the removal.
        Returns "removed", "absent" or "unresolved".
        settle: the client was interrupted, so its create request may still be on
        the way to the daemon. The name is watched for SETTLE_SECONDS. A name
        that never appears in that time is NOT declared absent: nothing is
        known about a request that is still pending. It is recorded in the
        lock file and reported as unresolved; every later step refuses until
        `stop-source` or `down` has reconciled it.
        """
        if not re.fullmatch(re.escape(self.project) + TRANSIENT_PATTERN, name):
            raise ValueError("not a job or metadata container name of this rehearsal")
        def present():
            return name in self.docker("ps", "-a", "--format", "{{.Names}}").split()

        with uninterrupted():
            removed = False
            quiet = time.monotonic() + (SETTLE_SECONDS if settle else 0.0)
            for attempt in range(3):
                while not present():
                    if time.monotonic() >= quiet:
                        if settle and not removed:
                            return self.unresolved(name)
                        return "removed" if removed else "absent"
                    time.sleep(0.25)
                try:
                    entry = require_label("container", name, self.project, self.inspect)
                    self.docker("rm", "-f", "-v", entry["Id"])
                    removed = True
                except RuntimeError:
                    time.sleep(0.5)  # `--rm` is removing it at this moment.
            if present():
                raise ValueError(f"container {name} could not be removed; run `rehearsal.sh down`")
            return "removed" if removed else "absent"

    def unresolved(self, name):
        """Record and report a container whose creation was interrupted and not seen."""
        if self.lock is not None:
            self.lock.note(name)
        print(f"UNRESOLVED: the request that creates container {name} was interrupted, and the container did not "
              f"appear within {SETTLE_SECONDS:g} seconds. It is NOT known to be absent: the daemon may still create "
              "it. Run `rehearsal.sh status`, then `rehearsal.sh stop-source` (or `down`): they wait for a late "
              "creation, remove the container and clear this record. Every other step refuses until then.",
              file=sys.stderr, flush=True)
        return "unresolved"

    def repository_network(self):
        """The one network a metadata reader may join: this rehearsal's own bridge."""
        if self.network not in self.docker("network", "ls", "--format", "{{.Name}}").split():
            raise ValueError(f"rehearsal network {self.network} is absent; repository readers join only that network. "
                             "Run this step before `rehearsal.sh down`, or `rehearsal.sh up` first")
        require_label("network", self.network, self.project, self.inspect)
        return self.network

    def image_identity(self):
        """The image must carry exactly this kit's files; returns the image ID.

        One fixed command line: no network, no mount, nobody, read-only.
        """
        try:
            identity = self.docker("image", "inspect", "--format", "{{.Id}}", IMAGE).strip()
        except RuntimeError:
            raise ValueError(f"image {IMAGE} is not present locally; build it first (rehearsals never build or pull)")
        name = self.transient_name("meta")
        finished = False
        try:
            listing = self.docker(
                "run", "--rm", "--pull=never", "--name", name, "--label", f"{LABEL}={self.project}",
                "--network", "none", "--user", "65534:65534", "--read-only", "--cap-drop=ALL",
                "--security-opt=no-new-privileges", "--memory=64m", "--memory-swap=64m", "--cpus=0.5",
                "--pids-limit=32", *LOG_OPTIONS,
                "--entrypoint", "sha256sum", IMAGE, *(f"{IMAGE_DIRECTORY}/{item}" for item in IMAGE_FILES))
            finished = True
        except RuntimeError:
            finished = True
            raise ValueError(f"image {IMAGE} does not carry every kit file; rebuild it from this kit directory")
        finally:
            self.remove_transient(name, settle=not finished)
        inside = dict(reversed(line.split(None, 1)) for line in listing.splitlines() if line.strip())
        different = [item for item in IMAGE_FILES
                     if inside.get(f"{IMAGE_DIRECTORY}/{item}") != hashlib.sha256((KIT / item).read_bytes()).hexdigest()]
        if different:
            raise ValueError(f"image {IMAGE} was not built from this kit: {', '.join(different)} "
                             f"differ{'s' if len(different) == 1 else ''} from the copy in the image; rebuild the image "
                             f"from this kit on the build machine (docker build -t {IMAGE} <kit directory>), save it "
                             "and load it here; never build on a host that runs production")
        return identity


def main():
    """In-container fence only: called by pgbackrest.sh before every command."""
    try:
        if sys.argv[1:2] != ["--container"]:
            raise ValueError("safety.py has no host command; use rehearsal.sh")
        if rehearsal_present():
            # The command line first: it needs no configuration to be judged.
            repository_arguments(sys.argv[2:])
            import configparser
            config = configparser.ConfigParser(interpolation=None)
            config.read(os.environ.get("PGBACKREST_CONFIG", "/run/komika/pgbackrest.conf"))
            environment_fence(dict(os.environ, KOMIKA_R2_PREFIX=config["global"]["repo1-path"]))
    except (ValueError, RuntimeError, OSError, KeyError, json.JSONDecodeError) as error:
        print("rehearsal refused: " + (str(error) if isinstance(error, (ValueError, RuntimeError)) else type(error).__name__), file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
