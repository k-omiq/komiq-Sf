#!/usr/bin/env python3
"""Hosted PostgreSQL lifecycle and serial UTC backups; standard library only."""
import datetime as dt
import configparser
import fcntl
import hashlib
import hmac
import ipaddress
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import ssl
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
import safety

KIT = Path(__file__).resolve().parent
DATA = Path(os.environ.get("PGDATA", "/var/lib/postgresql/18/docker"))
STATE = Path(os.environ.get("KOMIKA_BACKUP_STATE", "/var/lib/postgresql/komika-backup"))
CONFIG = Path(os.environ.get("PGBACKREST_CONFIG", "/run/komika/pgbackrest.conf"))
BACKUP_VARS = ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX",
               "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")
PRIVATE_RANGES = tuple(ipaddress.ip_network(cidr) for cidr in
                       ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"))


def allowed_cidrs():
    values = os.environ.get("KOMIKA_PG_ALLOWED_CIDRS", "172.21.0.0/16,fd00:21::/64").split(",")
    networks = []
    for value in values:
        if "/" not in value:
            raise ValueError("application addresses must use explicit CIDR notation")
        network = ipaddress.ip_network(value.strip(), strict=True)
        if not any(network.version == private.version and network.subnet_of(private)
                   for private in PRIVATE_RANGES):
            raise ValueError("application CIDRs must be private RFC1918 or IPv6 ULA ranges")
        networks.append(str(network))
    return networks


def log(message):
    print("postgres packaging: " + message, file=sys.stderr, flush=True)


def run(args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def pgbr(*args, capture=False):
    if safety.rehearsal_present():
        safety.repository_arguments(args)
        # Also fence implicit expiry during backups. configure() removes secrets
        # from the environment, so use the canonical prefix in its private file.
        config = configparser.ConfigParser(interpolation=None)
        config.read(CONFIG)
        safety.environment_fence(dict(os.environ, KOMIKA_R2_PREFIX=config["global"]["repo1-path"]))
    return run(["pgbackrest", "--stanza=komika", *args],
               stdout=subprocess.PIPE if capture else None).stdout


def atomic(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=".status-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as out:
            out.write(value)
            out.flush()
            os.fsync(out.fileno())
        os.replace(name, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def configure():
    allowed_cidrs()  # Refuse unsafe HBA even on an existing/local cluster.
    insecure = False
    if safety.rehearsal_present():
        if os.environ.get("KOMIKA_REHEARSAL_NO_REPO") == "1":
            # Local proof without any repository: no R2 value may be present, so
            # pgBackRest is never configured and no bucket can be addressed.
            safety.project_name(os.environ.get("KOMIKA_REHEARSAL_PROJECT"))
            if (any(os.environ.get(key) for key in BACKUP_VARS + ("KOMIKA_PRODUCTION_R2_PREFIX",))
                    or os.environ.get("KOMIKA_R2_VERIFY_TLS") not in (None, "", "1")):
                raise ValueError("KOMIKA_REHEARSAL_NO_REPO=1 refuses every R2 variable")
        else:
            _, prefix = safety.environment_fence(os.environ)
            os.environ["KOMIKA_R2_PREFIX"] = prefix
            # Rehearsal only, and only a `*-local` project with a private literal
            # endpoint: a throwaway self-signed S3 store. Hosted never gets here.
            insecure = safety.local_s3(os.environ)
    values = [os.environ.get(key, "") for key in BACKUP_VARS]
    if not any(values):
        log("WARNING: pgBackRest NOT configured; running WITHOUT backup or WAL archiving")
        return False
    if not all(values):
        raise ValueError("partial R2 configuration; all KOMIKA_R2_* variables are required")
    if any("\n" in v or "\r" in v or "\0" in v for v in values):
        raise ValueError("R2 configuration must use single-line values")
    endpoint, bucket, prefix, key, secret = values
    endpoint = endpoint.removeprefix("https://")
    if not re.fullmatch(r"[a-zA-Z0-9.-]+", endpoint) or "." not in endpoint:
        raise ValueError("R2 endpoint must be an HTTPS hostname without port or path")
    if not re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,62}", bucket):
        raise ValueError("invalid R2 bucket")
    prefix = safety.canonical_prefix(prefix)
    # Config values cannot inject another option; R2 credentials use this alphabet.
    if not all(re.fullmatch(r"[a-zA-Z0-9/+=_-]+", v) for v in (key, secret)):
        raise ValueError("invalid R2 credential encoding")
    STATE.mkdir(parents=True, exist_ok=True)
    for directory in ("spool", "lock"):
        (STATE / directory).mkdir(exist_ok=True)
    tls = "repo1-storage-verify-tls=n\n" if insecure else ""
    atomic(CONFIG, f"""[global]
repo1-type=s3
repo1-s3-uri-style=path
repo1-s3-region=auto
repo1-s3-endpoint={endpoint}
repo1-s3-bucket={bucket}
repo1-path=/{prefix}
repo1-s3-key={key}
repo1-s3-key-secret={secret}
{tls}repo1-bundle=y
repo1-block=y
compress-type=zst
archive-async=y
spool-path={STATE}/spool
lock-path={STATE}/lock
archive-push-queue-max=1GiB
start-fast=y
repo1-retention-full=2
process-max=2
archive-timeout=180
log-level-console=off
log-level-stderr=info
log-level-file=off

[komika]
pg1-path={DATA}
pg1-socket-path=/var/run/postgresql
pg1-user=postgres
""")
    # atomic() creates mode 0600; the config is in container-private tmpfs.
    for name in BACKUP_VARS:
        os.environ.pop(name, None)
    os.environ["PGBACKREST_CONFIG"] = str(CONFIG)
    return True


def repository_empty():
    """Separate authenticated ListObjectsV2 probe, like the frozen spike.

    pgBackRest can report a missing path instead of empty JSON. Only a valid
    signed HTTPS response proving zero objects authorizes new initialization.
    """
    config = configparser.ConfigParser(interpolation=None)
    config.read(CONFIG)
    settings = config["global"]
    endpoint, bucket = settings["repo1-s3-endpoint"], settings["repo1-s3-bucket"]
    prefix = settings["repo1-path"].lstrip("/") + "/"
    key, secret = settings["repo1-s3-key"], settings["repo1-s3-key-secret"]
    path = "/" + bucket
    query = urllib.parse.urlencode(sorted({"list-type": "2", "prefix": prefix,
                                           "max-keys": "1"}.items()),
                                   quote_via=urllib.parse.quote, safe="~")
    now = dt.datetime.now(dt.timezone.utc)
    date, stamp = now.strftime("%Y%m%d"), now.strftime("%Y%m%dT%H%M%SZ")
    digest = hashlib.sha256(b"").hexdigest()
    headers = {"host": endpoint, "x-amz-content-sha256": digest, "x-amz-date": stamp}
    signed = ";".join(sorted(headers))
    canonical = "GET\n" + path + "\n" + query + "\n" + "".join(
        k + ":" + headers[k] + "\n" for k in sorted(headers)) + "\n" + signed + "\n" + digest
    scope = date + "/auto/s3/aws4_request"
    to_sign = "AWS4-HMAC-SHA256\n" + stamp + "\n" + scope + "\n" + hashlib.sha256(canonical.encode()).hexdigest()
    signing = ("AWS4" + secret).encode()
    for part in (date, "auto", "s3", "aws4_request"):
        signing = hmac.new(signing, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
    headers["Authorization"] = f"AWS4-HMAC-SHA256 Credential={key}/{scope}, SignedHeaders={signed}, Signature={signature}"

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None

    context = ssl.create_default_context()
    if settings.get("repo1-storage-verify-tls") == "n":
        # configure() writes that line only for a local rehearsal store; check the
        # same three conditions again against the endpoint really used here.
        if not safety.local_s3(dict(os.environ, KOMIKA_R2_ENDPOINT=endpoint)):
            raise ValueError("TLS verification may be off only for a local rehearsal repository")
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect(),
                                        urllib.request.HTTPSHandler(context=context))
    request = urllib.request.Request(f"https://{endpoint}{path}?{query}", headers=headers, method="GET")
    with opener.open(request, timeout=180) as response:
        if response.status != 200:
            raise ValueError("repository emptiness probe did not return HTTP 200")
        root = ET.fromstring(response.read())
    def children(tag):
        return [n for n in root if n.tag.rsplit("}", 1)[-1] == tag]
    def value(tag):
        nodes = children(tag)
        if len(nodes) != 1:
            raise ValueError("invalid repository emptiness response")
        return nodes[0].text or ""
    if (root.tag.rsplit("}", 1)[-1] != "ListBucketResult" or value("Name") != bucket
            or value("Prefix") != prefix):
        raise ValueError("repository emptiness response escaped the selected prefix")
    return value("KeyCount") == "0" and value("IsTruncated") == "false" and not children("Contents")


def backup_info(allow_unbacked=False):
    try:
        entries = json.loads(pgbr("--output=json", "info", capture=True))
    except subprocess.CalledProcessError as error:
        # pgBackRest PathMissingError/FileMissingError. Other failures, including
        # authentication, network and malformed metadata, never authorize initdb.
        if error.returncode not in (44, 55) or not repository_empty():
            raise
        return None
    if not isinstance(entries, list) or len(entries) != 1 or entries[0].get("name") != "komika":
        raise ValueError("unexpected repository info")
    entry = entries[0]
    # Any per-repository error is fatal, even if info returned exit zero. A new
    # stanza reports 2 (no valid backups) per repository as well as in total;
    # the total below decides whether that state is acceptable to the caller.
    for repo in entry.get("repo", []):
        if repo.get("status", {}).get("code", 0) not in (0, 1, 2):
            raise ValueError("repository reports an error")
    code = entry.get("status", {}).get("code")
    if code == 0 and entry.get("backup"):
        return entry["backup"]
    if code == 2 and allow_unbacked and not entry.get("backup"):
        return []
    if code == 1:
        # info's 'missing stanza' alone does not prove absence: reachable listing
        # must show no objects. Partial/corrupt repositories MUST NOT initialize.
        try:
            objects = json.loads(pgbr("--recurse", "--output=json", "repo-ls", capture=True))
        except subprocess.CalledProcessError as error:
            # An error is NOT evidence of emptiness. Independently authenticate
            # and prove the prefix empty; any probe error propagates without initdb.
            if error.returncode in (44, 55) and repository_empty():
                return None
            raise
        if objects == {} or objects == []:
            return None
        if objects == {".": {"type": "path"}} and repository_empty():
            return None
        raise ValueError("missing stanza but repository is not empty")
    raise ValueError("stanza exists without a valid backup, or repository is corrupt")


def hba():
    rules = "".join(f"host komika komika {cidr} scram-sha-256\n" for cidr in allowed_cidrs())
    atomic(DATA / "pg_hba.conf", """# Only the postgres OS user can administer via the shared socket.
local all postgres peer
""" + rules + """local all all reject
host all all 0.0.0.0/0 reject
host all all ::/0 reject
""")


def postgres_args(archive):
    return ["postgres", "-D", str(DATA), "-c", f"config_file={KIT}/postgresql.conf",
            "-c", f"hba_file={DATA}/pg_hba.conf", "-c", f"archive_mode={'on' if archive else 'off'}"]


def init_cluster():
    password = os.environ.get("KOMIKA_POSTGRES_PASSWORD", "")
    if not re.fullmatch(r"[A-Za-z0-9_.~-]{24,}", password):
        raise ValueError("KOMIKA_POSTGRES_PASSWORD must be >=24 URI-safe characters; use random hex")
    run(["initdb", "-D", str(DATA), "-U", "postgres", "--encoding=UTF8",
         "--locale-provider=builtin", "--builtin-locale=C.UTF-8", "--lc-collate=C",
         "--lc-ctype=C", "--auth-local=peer", "--auth-host=scram-sha-256",
         "--data-checksums"])
    hba()
    # This temporary server has no TCP listener and no archiving; credentials go
    # via stdin SQL, never a psql argument or PostgreSQL's command line.
    options = f"-c config_file={KIT}/postgresql.conf -c listen_addresses='' -c archive_mode=off"
    run(["pg_ctl", "-D", str(DATA), "-w", "-o", options, "start"])
    try:
        run(["psql", "-X", "-w", "-h", "/var/run/postgresql", "-U", "postgres",
             "-d", "postgres", "-v", "ON_ERROR_STOP=1"], input=f"""
SET log_statement = 'none';
SET log_min_duration_statement = -1;
SET log_min_error_statement = 'panic';
CREATE ROLE komika LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD '{password}';
CREATE DATABASE komika OWNER komika ENCODING 'UTF8' LOCALE_PROVIDER builtin
 BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;
\\connect komika
CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public;
""")
    finally:
        run(["pg_ctl", "-D", str(DATA), "-w", "-m", "fast", "stop"])


def start(archive):
    if (DATA / "PG_VERSION").is_file():
        if (DATA / "PG_VERSION").read_text().strip() != "18":
            raise ValueError("PG_VERSION is not 18; refusing incompatible cluster")
        log("existing cluster; starting normally")
    else:
        if DATA.exists() and (DATA.is_symlink() or any(DATA.iterdir())):
            raise ValueError("data directory is not empty and has no PG_VERSION")
        DATA.mkdir(parents=True, exist_ok=True, mode=0o700)
        if archive:
            backups = backup_info()
            if backups and safety.rehearsal_present():
                # A rehearsal never restores on `up`: an implicit restore would fill
                # the data volume behind a disk check that knows nothing of its size.
                raise ValueError("empty data directory and the rehearsal repository already holds a backup: a "
                                 "rehearsal `up` never restores implicitly. Restore explicitly with "
                                 "`rehearsal.sh restore --target NEWDIR --json-out FILE [--target-time UTC]`; "
                                 "a new drill uses a new KOMIKA_R2_PREFIX, whose repository is empty")
            if backups:
                log("empty data directory; restoring latest backup before startup")
                # No option here: pgBackRest refuses archive-async on `restore`
                # (it belongs to archive-get/archive-push only).
                pgbr("restore")
                if not (DATA / "PG_VERSION").is_file():
                    raise ValueError("restore did not produce PG_VERSION")
            elif os.environ.get("KOMIKA_PG_INIT_NEW") == "1":
                log("reachable empty repository; explicit new-cluster initialization")
                init_cluster()
            else:
                raise ValueError("empty repository; initialization requires KOMIKA_PG_INIT_NEW=1")
        else:
            init_cluster()
    hba()
    # PostgreSQL itself need not inherit backup keys; Docker env residual is
    # documented. Application password is only needed for bootstrap/health.
    os.environ.pop("KOMIKA_POSTGRES_PASSWORD", None)
    os.execvp("postgres", postgres_args(archive))


def read_status():
    path = STATE / "status.json"
    return json.loads(path.read_text()) if path.exists() else {}


def save_status(status):
    atomic(STATE / "status.json", json.dumps(status, sort_keys=True) + "\n")


def slots(now):
    """Latest due slot of each type, UTC; at most ONE missed slot is caught up."""
    clock = dt.datetime.fromtimestamp(now, dt.timezone.utc)
    day = clock.replace(hour=2, minute=0, second=0, microsecond=0)
    if day > clock:
        day -= dt.timedelta(days=1)
    sunday = day - dt.timedelta(days=(day.weekday() + 1) % 7)
    incremental = clock.replace(hour=3, minute=0, second=0, microsecond=0)
    if incremental > clock:
        incremental -= dt.timedelta(days=1)
    return {"full": int(sunday.timestamp()), "check": int(day.timestamp()),
            "incr": int(incremental.timestamp())}


def health(now=None):
    status = read_status()
    now = time.time() if now is None else now
    # No backup config is healthy only as an explicitly unprotected local stack.
    if not any(os.environ.get(k) for k in BACKUP_VARS) and not CONFIG.exists():
        return True
    for kind, age in (("incr", 26 * 3600), ("full", 8 * 86400), ("check", 26 * 3600)):
        success = status.get(kind, {}).get("success", 0)
        if not success or not 0 <= now - success <= age:
            return False
    return not status.get("last_error")


def perform(kind, slot, status, bootstrap=False):
    if kind not in ("full", "incr", "check"):
        raise ValueError("job must be full, incr or check")
    # Persist BEFORE executing; a crash/restart cannot execute the same slot twice.
    state = status.setdefault(kind, {})
    state["slot"] = slot
    state["error"] = f"{kind} interrupted or still running"
    status["last_error"] = state["error"]
    save_status(status)
    try:
        if kind == "check":
            pgbr("check")
        else:
            backups = backup_info(allow_unbacked=bootstrap)
            if bootstrap and backups:
                raise ValueError("repository already has a backup; use job full")
            if not bootstrap and not backups:
                raise ValueError("first full backup is explicit; use backup.sh bootstrap-full")
            pgbr(f"--type={kind}", "backup")
    except (subprocess.CalledProcessError, ValueError, json.JSONDecodeError) as error:
        # Never print command arguments, environment or configuration on failure.
        status["last_error"] = f"{kind} failed ({type(error).__name__})"
        state["error"] = status["last_error"]
        log(status["last_error"])
        save_status(status)
        return False
    state["success"] = int(time.time())
    log(f"{kind} succeeded at {state['success']}")
    # Preserve other outstanding job errors until each failing type succeeds.
    state.pop("error", None)
    status["last_error"] = next((v["error"] for v in status.values()
                                if isinstance(v, dict) and v.get("error")), "")
    save_status(status)
    return True


def tick(now=None):
    status = read_status()
    # Recompute after EACH job: a long full/check can cross another due slot.
    # Explicit now is a fixed-clock test hook; production reads the live clock.
    while True:
        due = slots(time.time() if now is None else now)
        kind = next((kind for kind in ("full", "check", "incr")
                     if status.get(kind, {}).get("slot", -1) < due[kind]), None)
        if kind is None:
            return
        perform(kind, due[kind], status)


def locked():
    STATE.mkdir(parents=True, exist_ok=True)
    handle = (STATE / "scheduler.lock").open("a")
    # Kernel lock is released even after SIGKILL; shared by scheduler/manual jobs.
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BaseException:
        handle.close()
        raise
    return handle


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else "postgres"
    if command == "health":
        sys.exit(0 if health() else 1)
    archive = configure()
    if command == "postgres":
        start(archive)
    elif command == "scheduler":
        if not archive:
            while True:
                time.sleep(86400)
        while True:
            busy = False
            try:
                with locked():
                    tick()
            except BlockingIOError:
                busy = True
                log("another backup job owns the lock; waiting")
            # Wake at the next 02:00/03:00 UTC slot; lock contention retries sooner.
            clock = dt.datetime.now(dt.timezone.utc)
            next_day = clock.replace(hour=2, minute=0, second=0, microsecond=0)
            candidates = [next_day, next_day + dt.timedelta(hours=1), next_day + dt.timedelta(days=1)]
            delay = min((c - clock).total_seconds() for c in candidates if c > clock)
            time.sleep(min(delay, 60) if busy else delay)
    elif command == "job":
        if not archive:
            raise ValueError("backup job requires complete R2 configuration")
        kind = sys.argv[2]
        with locked():
            status = read_status()
            if kind == "bootstrap-full":
                # Deliberate operator action before the first full chain.
                if not perform("full", slots(time.time())["full"], status, bootstrap=True):
                    sys.exit(1)
            elif not perform(kind, slots(time.time())[kind], status):
                sys.exit(1)
    elif command == "exec":
        # Recovery/operator commands receive generated private config; no keys in argv.
        os.execvp(sys.argv[2], sys.argv[2:])
    else:
        raise ValueError("unknown hosted PostgreSQL command")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, subprocess.CalledProcessError, OSError, json.JSONDecodeError,
            ET.ParseError, configparser.Error, KeyError) as exc:
        log(f"FATAL: {exc}" if isinstance(exc, ValueError) else
            f"FATAL: {type(exc).__name__}; startup/job refused (inspect preceding diagnostics)")
        sys.exit(1)
