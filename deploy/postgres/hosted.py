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
import threading
import time
import ssl
import urllib.error
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
SOCKET = "/var/run/postgresql"
# pgbackrest.sh appends one line per pgBackRest dropped-WAL warning, with
# sub-second UTC time (the last 10,000 lines).
DROPPED = "dropped-wal.log"
# Free space the data volume keeps: archive-push-queue-max (1 GiB) of WAL that
# waits for R2, max_wal_size (2 GB) that pg_wal holds between checkpoints, and
# 1 GiB for the spool, a checkpoint overrun and temporary files. Below it an
# R2 outage fills the disk before the queue limit drops WAL.
DISK_RESERVE = 4 << 30
ARCHIVE_FAILING = 15 * 60  # Also the archiver stall limit.
# M3 budget duty: readings on these dates, action at 7 GB (decimal, below 7 GiB).
BUDGET_ACTION = 7 * 10 ** 9
BUDGET_DATES = ("2026-10-20", "2026-10-27", "2026-11-03", "2026-11-10")
# Optional fenced standby (STANDBY.md). One standby, one physical slot, one
# replication role that logs in only with a client certificate.
NODE = re.compile(r"[a-z0-9][a-z0-9-]{0,62}")
SLOT = "komika_standby"
REPLICA_ROLE = "komika_replica"
# entrypoint.sh copies the mounted certificate directory here, postgres-owned 0600.
TLS = Path(os.environ.get("KOMIKA_PG_TLS", "/run/komika/tls"))
FENCE_KEYS = {"epoch", "primary_node", "timeline", "at"}
# The last node name a fence check passed with, on the data volume beside PGDATA:
# pg_basebackup and pgBackRest copy PGDATA, so a marker there would travel to the
# standby and into restores with another node's name.
NODE_MARKER = "node-id"
STANDBY_LAG_WARN = 300  # Seconds of replay lag before health reports it.
# The slot keeps WAL for an absent standby; postgresql.conf caps it at 2 GB
# (max_slot_wal_keep_size), and health reports it from 1 GiB on.
SLOT_RETAINED_WARN = 1 << 30


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


def fence(args):
    if safety.rehearsal_present():
        safety.repository_arguments(args)
        # Also fence implicit expiry during backups. configure() removes secrets
        # from the environment, so use the canonical prefix in its private file.
        config = configparser.ConfigParser(interpolation=None)
        config.read(CONFIG)
        safety.environment_fence(dict(os.environ, KOMIKA_R2_PREFIX=config["global"]["repo1-path"]))


def pgbr(*args, capture=False):
    fence(args)
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


def repository_settings():
    config = configparser.ConfigParser(interpolation=None)
    config.read(CONFIG)
    return config["global"]


def s3_request(method, key="", query=None, body=b"", extra=None, timeout=180):
    """One signed path-style S3 request to the configured repository bucket.

    Returns (status, headers, body). An HTTP error status is returned, never
    raised; a network or TLS failure raises. Extra headers are signed too.
    """
    settings = repository_settings()
    endpoint, bucket = settings["repo1-s3-endpoint"], settings["repo1-s3-bucket"]
    key_id, secret = settings["repo1-s3-key"], settings["repo1-s3-key-secret"]
    path = "/" + bucket + ("/" + urllib.parse.quote(key, safe="/~") if key else "")
    query = urllib.parse.urlencode(sorted((query or {}).items()), quote_via=urllib.parse.quote, safe="~")
    now = dt.datetime.now(dt.timezone.utc)
    date, stamp = now.strftime("%Y%m%d"), now.strftime("%Y%m%dT%H%M%SZ")
    digest = hashlib.sha256(body).hexdigest()
    headers = {"host": endpoint, "x-amz-content-sha256": digest, "x-amz-date": stamp}
    headers.update({name.lower(): value for name, value in (extra or {}).items()})
    signed = ";".join(sorted(headers))
    canonical = method + "\n" + path + "\n" + query + "\n" + "".join(
        k + ":" + headers[k] + "\n" for k in sorted(headers)) + "\n" + signed + "\n" + digest
    scope = date + "/auto/s3/aws4_request"
    to_sign = "AWS4-HMAC-SHA256\n" + stamp + "\n" + scope + "\n" + hashlib.sha256(canonical.encode()).hexdigest()
    signing = ("AWS4" + secret).encode()
    for part in (date, "auto", "s3", "aws4_request"):
        signing = hmac.new(signing, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
    headers["Authorization"] = f"AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed}, Signature={signature}"

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
    url = f"https://{endpoint}{path}" + (f"?{query}" if query else "")
    request = urllib.request.Request(url, data=body if method == "PUT" else None, headers=headers, method=method)
    try:
        with opener.open(request, timeout=timeout) as response:
            return response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read()


def repository_empty():
    """Separate authenticated ListObjectsV2 probe, like the frozen spike.

    pgBackRest can report a missing path instead of empty JSON. Only a valid
    signed HTTPS response proving zero objects authorizes new initialization.
    """
    settings = repository_settings()
    bucket = settings["repo1-s3-bucket"]
    prefix = settings["repo1-path"].lstrip("/") + "/"
    status, _, body = s3_request("GET", query={"list-type": "2", "prefix": prefix, "max-keys": "1"})
    if status != 200:
        raise ValueError("repository emptiness probe did not return HTTP 200")
    root = ET.fromstring(body)
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


def replica_cidrs():
    """Where the standby may connect from (KOMIKA_PG_REPLICA_CIDRS); empty = no standby.

    The standby can sit on another host, so public ranges are allowed here: the
    rule is `hostssl ... cert`, a client certificate signed by the standby CA.
    """
    networks = []
    for value in filter(None, (v.strip() for v in os.environ.get("KOMIKA_PG_REPLICA_CIDRS", "").split(","))):
        if "/" not in value:
            raise ValueError("replication addresses must use explicit CIDR notation")
        network = ipaddress.ip_network(value, strict=True)
        if network.prefixlen == 0:
            raise ValueError("a replication CIDR must not match every address")
        networks.append(str(network))
    return networks


def tls_files(*names):
    return all((TLS / name).is_file() for name in names)


def hba():
    rules = "".join(f"host komika komika {cidr} scram-sha-256\n" for cidr in allowed_cidrs())
    replicas = replica_cidrs()
    if replicas and not tls_files("server.crt", "server.key", "ca.crt"):
        raise ValueError("KOMIKA_PG_REPLICA_CIDRS needs the TLS files (server.crt, server.key, ca.crt)")
    rules += "".join(f"hostssl replication {REPLICA_ROLE} {cidr} cert\n" for cidr in replicas)
    atomic(DATA / "pg_hba.conf", """# Only the postgres OS user can administer via the shared socket.
local all postgres peer
""" + rules + """local all all reject
host all all 0.0.0.0/0 reject
host all all ::/0 reject
""")


def postgres_args(archive, fence=None):
    args = ["postgres", "-D", str(DATA), "-c", f"config_file={KIT}/postgresql.conf",
            "-c", f"hba_file={DATA}/pg_hba.conf", "-c", f"archive_mode={'on' if archive else 'off'}"]
    if tls_files("server.crt", "server.key", "ca.crt"):
        args += ["-c", "ssl=on", "-c", f"ssl_cert_file={TLS}/server.crt", "-c", f"ssl_key_file={TLS}/server.key",
                 "-c", f"ssl_ca_file={TLS}/ca.crt"]
    if fence is not None:
        # The server checks these at start when it has KOMIKA_NODE_ID (node_fence.rs).
        args += ["-c", f"komika.node_id={fence['primary_node']}", "-c", f"komika.fence_epoch={fence['epoch']}"]
    return args


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
-- /metrics sizes pg_wal and its archive backlog; nothing more (not pg_monitor).
GRANT EXECUTE ON FUNCTION pg_catalog.pg_ls_waldir() TO komika;
GRANT EXECUTE ON FUNCTION pg_catalog.pg_ls_archive_statusdir() TO komika;
""")
    finally:
        run(["pg_ctl", "-D", str(DATA), "-w", "-m", "fast", "stop"])


def start(archive):
    standby = (DATA / "standby.signal").is_file()
    fence = None
    if (DATA / "PG_VERSION").is_file():
        if (DATA / "PG_VERSION").read_text().strip() != "18":
            raise ValueError("PG_VERSION is not 18; refusing incompatible cluster")
        if standby:
            # A standby takes no writes; promote() moves the fence first.
            log("existing cluster with standby.signal; starting as a standby")
        else:
            check_node_marker()
            fence = fence_gate(archive, existing=True)
            log("existing cluster; starting normally")
    else:
        if DATA.exists() and (DATA.is_symlink() or any(DATA.iterdir())):
            raise ValueError("data directory is not empty and has no PG_VERSION")
        # A restore or a new cluster becomes a primary: the fence decides first.
        check_node_marker()
        fence = fence_gate(archive, existing=False)
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
    if fence is not None:
        write_node_marker(fence["primary_node"])
    hba()
    # PostgreSQL itself need not inherit backup keys; Docker env residual is
    # documented. Application password is only needed for bootstrap/health.
    os.environ.pop("KOMIKA_POSTGRES_PASSWORD", None)
    os.execvp("postgres", postgres_args(archive, fence))


def node_id():
    """This node's name (KOMIKA_NODE_ID). Unset means fencing is off, as before."""
    value = os.environ.get("KOMIKA_NODE_ID", "")
    if not value:
        return None
    if not NODE.fullmatch(value):
        raise ValueError("KOMIKA_NODE_ID must be 1-63 lower-case letters, digits or inner dashes")
    return value


def node_marker_problem(node):
    """Why this volume may not start as a primary under `node`, or None.

    A volume that once passed a fence check keeps that node's name. Without it, a
    release that dropped KOMIKA_NODE_ID would start an old primary unfenced."""
    path = STATE / NODE_MARKER
    if not path.is_file():
        return None
    marked = path.read_text().strip()
    if marked == node:
        return None
    return (f"this data volume was fenced as node {marked!r} ({path}), but KOMIKA_NODE_ID is "
            + ("unset" if node is None else repr(node))
            + f". Set KOMIKA_NODE_ID={marked} in deploy/.env (docker-compose.yml forwards it). Only if this "
            "volume really changes its node name or leaves the standby setup, remove that file first.")


def check_node_marker():
    problem = node_marker_problem(node_id())
    if problem:
        raise ValueError("refusing to start as primary: " + problem)


def write_node_marker(node):
    path = STATE / NODE_MARKER
    if not path.is_file() or path.read_text().strip() != node:
        atomic(path, node + "\n")


def fence_key():
    """Beside the pgBackRest path, never inside it: an empty-repository probe of
    the path must stay empty, and pgBackRest never lists or expires this key."""
    return repository_settings()["repo1-path"].strip("/") + ".fence.json"


def parse_fence(text):
    """fence.json, strictly: anything else is unreadable and never authorizes."""
    value = json.loads(text)
    if not isinstance(value, dict) or set(value) != FENCE_KEYS:
        raise ValueError("fence.json must hold exactly epoch, primary_node, timeline and at")
    if type(value["epoch"]) is not int or value["epoch"] < 1:
        raise ValueError("fence.json epoch must be a positive integer")
    if type(value["timeline"]) is not int or value["timeline"] < 1:
        raise ValueError("fence.json timeline must be a positive integer")
    if not isinstance(value["primary_node"], str) or not NODE.fullmatch(value["primary_node"]):
        raise ValueError("fence.json primary_node is not a node name")
    if not isinstance(value["at"], str) or not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", value["at"]):
        raise ValueError("fence.json at must be a UTC time like 2026-10-16T12:00:00Z")
    stamp(value["at"])
    return value


def read_fence():
    """(fence, etag); (None, None) only when the store answers 404."""
    status, headers, body = s3_request("GET", fence_key(), timeout=60)
    if status == 404:
        return None, None
    if status != 200:
        raise ValueError(f"fence.json read failed (HTTP {status})")
    etag = headers.get("ETag")
    if not etag:
        raise ValueError("fence.json read returned no ETag")
    return parse_fence(body.decode()), etag


def write_fence(fence, etag):
    """Write only if the object is still the one read (or still absent).

    True when written; False when another writer changed it first."""
    body = (json.dumps(fence, sort_keys=True) + "\n").encode()
    condition = {"If-None-Match": "*"} if etag is None else {"If-Match": etag}
    status, _, _ = s3_request("PUT", fence_key(), body=body, timeout=60,
                              extra=dict(condition, **{"Content-Type": "application/json"}))
    if status == 200:
        return True
    # 412: the condition failed. 409: S3 reports a concurrent conditional write.
    if status in (409, 412):
        return False
    raise ValueError(f"fence.json write failed (HTTP {status})")


def conditional_writes_honoured():
    """A store that ignores If-None-Match would let two promotions both win.
    Two create-only writes of a probe key: the second must be refused."""
    key = repository_settings()["repo1-path"].strip("/") + ".fence-probe"
    first, _, _ = s3_request("PUT", key, body=b"probe\n", extra={"If-None-Match": "*"}, timeout=60)
    second, _, _ = s3_request("PUT", key, body=b"probe\n", extra={"If-None-Match": "*"}, timeout=60)
    # The condition promotion uses: a stale ETag must be refused too.
    third, _, _ = s3_request("PUT", key, body=b"stale\n", extra={"If-Match": '"komika-stale-etag"'}, timeout=60)
    if any(status not in (200, 409, 412) for status in (first, second, third)):
        raise ValueError(f"fence probe write failed (HTTP {first}, {second}, {third})")
    return second in (409, 412) and third in (409, 412)


def fence_decision(fence, node, timeline, switched=False):
    """May this node run as the primary? (allowed, reason). Pure, for the matrix tests.

    switched: the data directory holds the fence timeline's history file, i.e.
    pg_promote() already chose that timeline here."""
    if node is None:
        return True, "fencing is off (no KOMIKA_NODE_ID)"
    if fence is None:
        return False, "fence.json is absent"
    if fence["primary_node"] != node:
        return False, (f"fence.json epoch {fence['epoch']} names {fence['primary_node']} as primary, "
                       f"not this node ({node}); this node may only be a standby")
    if timeline is not None and timeline == fence["timeline"] - 1 and switched:
        # Stopped between pg_promote() and its first checkpoint: pg_control still
        # shows the old timeline; crash recovery replays onto the new one.
        return True, (f"fence.json epoch {fence['epoch']} names this node ({node}); local timeline {timeline} "
                      f"switched to {fence['timeline']} before its first checkpoint")
    if timeline is not None and timeline < fence["timeline"]:
        return False, (f"local timeline {timeline} is older than fence timeline {fence['timeline']}: "
                       "this data directory is an old copy")
    return True, f"fence.json epoch {fence['epoch']} names this node ({node})"


def fence_gate(archive, existing):
    """Before PostgreSQL starts as a primary. Returns the fence (None when fencing
    is off) or raises. With no fence yet, the first primary claims epoch 1 with a
    create-only write; a lost claim is read again and decided like any fence."""
    node = node_id()
    if node is None:
        return None
    if not archive:
        raise ValueError("KOMIKA_NODE_ID needs the backup repository: fence.json lives there")
    timeline = local_timeline() if existing else None
    fence, etag = read_fence()
    if fence is None:
        # Two first starts at once: only a store that honours create-only writes
        # lets one of them lose.
        if not conditional_writes_honoured():
            raise ValueError("the repository store ignores conditional writes; fence.json cannot be claimed there")
        claim = {"epoch": 1, "primary_node": node, "timeline": timeline or 1, "at": iso(time.time())}
        if write_fence(claim, None):
            log(f"fence.json claimed: epoch 1, primary {node}")
            return claim
        fence, etag = read_fence()
    allowed, reason = fence_decision(fence, node, timeline,
                                     existing and fence is not None and switched_to(fence["timeline"]))
    if not allowed:
        raise ValueError("refusing to start as primary: " + reason)
    log(reason)
    return fence


def switched_to(timeline):
    """pg_promote() writes the new timeline's history file before it leaves recovery."""
    return (DATA / "pg_wal" / f"{timeline:08X}.history").is_file()


def local_timeline():
    """The data directory's checkpoint timeline, from pg_controldata."""
    output = subprocess.run(["pg_controldata", "-D", str(DATA)], check=True, text=True,
                            stdout=subprocess.PIPE, env=dict(os.environ, LC_ALL="C")).stdout
    found = re.search(r"^Latest checkpoint's TimeLineID:\s+(\d+)$", output, re.M)
    if not found:
        raise ValueError("pg_controldata shows no checkpoint timeline")
    return int(found[1])


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


def as_postgres(args):
    # Peer authentication on the shared socket needs the postgres OS user;
    # Docker health checks and `docker compose exec` run as root.
    return ["gosu", "postgres", *args] if os.geteuid() == 0 else list(args)


def collect(args, timeout, input=None):
    # Read-only: stdout only. Never print argv, stderr or configuration.
    return subprocess.run(as_postgres(args), check=True, text=True, input=input, timeout=timeout,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout


def query(sql, timeout=10):
    return collect(["psql", "-X", "-w", "-q", "-At", "-h", SOCKET, "-U", "postgres", "-d", "komika",
                    "-v", "ON_ERROR_STOP=1"], timeout, input=sql)


GRANT = """SELECT pg_is_in_recovery();
DO $$BEGIN
IF NOT pg_is_in_recovery() THEN
  GRANT EXECUTE ON FUNCTION pg_catalog.pg_ls_waldir() TO komika;
  GRANT EXECUTE ON FUNCTION pg_catalog.pg_ls_archive_statusdir() TO komika;
END IF;
END$$;
"""
# Only with a standby (KOMIKA_PG_REPLICA_CIDRS): the replication role, which
# logs in with a client certificate only (no password), and pg_read_all_stats
# so /metrics can read the WAL sender positions of that role.
REPLICA_GRANT = f"""DO $$BEGIN
IF NOT pg_is_in_recovery() THEN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{REPLICA_ROLE}') THEN
    CREATE ROLE {REPLICA_ROLE} LOGIN REPLICATION NOSUPERUSER NOCREATEDB NOCREATEROLE;
  END IF;
  GRANT pg_read_all_stats TO komika;
END IF;
END$$;
"""


def grant_sql():
    return GRANT + (REPLICA_GRANT if os.environ.get("KOMIKA_PG_REPLICA_CIDRS", "").strip() else "")


def monitoring_grant(deadline=1800, pause=10):
    """The init_cluster grant, idempotently, for clusters made before it.

    Retries quietly until PostgreSQL answers; one log line either way. Runs
    beside the scheduler and never delays or fails a backup.
    """
    end = time.monotonic() + deadline
    while True:
        try:
            recovering = query(grant_sql(), timeout=30).strip() == "t"
        except subprocess.CalledProcessError as error:
            # psql: 2 = no connection (yet), 3 = PostgreSQL refused the script.
            if error.returncode == 3 or time.monotonic() >= end:
                log("monitoring grant not applied; /metrics WAL gauges stay unavailable")
                return False
        except (subprocess.TimeoutExpired, OSError):
            if time.monotonic() >= end:
                log("monitoring grant not applied; /metrics WAL gauges stay unavailable")
                return False
        else:
            log("monitoring grant skipped: server is in recovery" if recovering else
                "monitoring grant present: komika may run pg_ls_waldir() and pg_ls_archive_statusdir()")
            return not recovering
        time.sleep(pause)


FACTS = """SELECT json_build_object(
 'in_recovery', pg_is_in_recovery(),
 'archived_count', archived_count, 'failed_count', failed_count,
 'last_archived_wal', last_archived_wal, 'last_archived_time', extract(epoch FROM last_archived_time),
 'last_failed_wal', last_failed_wal, 'last_failed_time', extract(epoch FROM last_failed_time),
 'ready', (SELECT count(*) FROM pg_ls_archive_statusdir() WHERE name LIKE '%.ready'),
 'oldest_ready', (SELECT extract(epoch FROM min(modification)) FROM pg_ls_archive_statusdir()
                  WHERE name LIKE '%.ready'))
FROM pg_stat_archiver;
"""
WAL = """SELECT json_build_object(
 'wal_bytes', wal_bytes::bigint, 'stats_reset', extract(epoch FROM stats_reset),
 'lsn_bytes', CASE WHEN pg_is_in_recovery() THEN NULL
                   ELSE pg_wal_lsn_diff(pg_current_wal_lsn(), '0/0')::bigint END)
FROM pg_stat_wal;
"""


# Both sides of a standby pair in one statement. LSNs are text ("0/3000148").
# Standby replay lag is 0 while everything received is replayed: an idle
# primary sends no commits, so the age of the last replayed commit alone
# would grow without any lag.
REPLICATION = """SELECT json_build_object(
 'in_recovery', pg_is_in_recovery(),
 'timeline', (SELECT timeline_id FROM pg_control_checkpoint()),
 'current_lsn', CASE WHEN pg_is_in_recovery() THEN NULL ELSE pg_current_wal_lsn() END,
 'senders', (SELECT coalesce(json_agg(json_build_object(
     'name', application_name, 'state', state, 'client', client_addr,
     'sent_lsn', sent_lsn, 'flush_lsn', flush_lsn, 'replay_lsn', replay_lsn,
     'replay_lag_seconds', CASE WHEN replay_lag IS NULL AND replay_lsn = sent_lsn THEN 0
                                ELSE extract(epoch FROM replay_lag) END,
     'replay_behind_bytes', pg_wal_lsn_diff(sent_lsn, replay_lsn)) ORDER BY application_name), '[]')
   FROM pg_stat_replication WHERE state <> 'backup'),
 'slots', (SELECT coalesce(json_agg(json_build_object(
     'name', slot_name, 'active', active, 'wal_status', wal_status,
     'retained_bytes', CASE WHEN pg_is_in_recovery() THEN NULL
                            ELSE pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn) END) ORDER BY slot_name), '[]')
   FROM pg_replication_slots WHERE slot_type = 'physical'),
 'receiver', (SELECT json_build_object('status', status, 'received_tli', received_tli,
     'flushed_lsn', flushed_lsn, 'sender_host', sender_host, 'slot', slot_name,
     'last_message', extract(epoch FROM last_msg_receipt_time)) FROM pg_stat_wal_receiver),
 'receive_lsn', pg_last_wal_receive_lsn(),
 'replay_lsn', pg_last_wal_replay_lsn(),
 'last_replayed_commit', extract(epoch FROM pg_last_xact_replay_timestamp()),
 'replay_lag_seconds', CASE WHEN NOT pg_is_in_recovery() THEN NULL
     WHEN pg_last_wal_receive_lsn() IS NOT NULL AND pg_last_wal_receive_lsn() = pg_last_wal_replay_lsn() THEN 0
     ELSE extract(epoch FROM now() - pg_last_xact_replay_timestamp()) END);
"""


def replication_facts(timeout=10):
    facts = json.loads(query(REPLICATION, timeout))
    if not isinstance(facts, dict):
        raise ValueError("unexpected replication facts")
    return facts


def replication_problems(facts):
    """Standby pair problems in short fixed words; empty without a standby."""
    problems = []
    if facts.get("in_recovery"):
        receiver = facts.get("receiver") or {}
        if receiver.get("status") != "streaming":
            problems.append("standby is not streaming from the primary")
        lag = facts.get("replay_lag_seconds")
        if lag is None or lag > STANDBY_LAG_WARN:
            problems.append(f"standby replay lag is over {STANDBY_LAG_WARN} s or unknown")
        return problems
    for slot in facts.get("slots") or []:
        name = slot.get("name")
        if slot.get("wal_status") in ("unreserved", "lost"):
            problems.append(f"slot {name} lost or about to lose WAL: run standby-init again")
        elif (slot.get("retained_bytes") or 0) > SLOT_RETAINED_WARN:
            problems.append(f"slot {name} holds over 1 GiB of WAL for its standby")
    for sender in facts.get("senders") or []:
        lag = sender.get("replay_lag_seconds")
        if lag is None or lag > STANDBY_LAG_WARN:
            problems.append(f"standby {sender.get('name')} replay lag is over {STANDBY_LAG_WARN} s or unknown")
    return problems


def archiver_facts(timeout=10):
    facts = json.loads(query(FACTS, timeout))
    if not isinstance(facts, dict):
        raise ValueError("unexpected archiver facts")
    return facts


def archiver_failing(facts, now):
    """A failure newer than the last success, and no success for 15 minutes."""
    failed, archived = facts.get("last_failed_time"), facts.get("last_archived_time")
    if failed is None or (archived is not None and archived >= failed):
        return False
    # Never archived but failed: how long is unknown, so it counts at once.
    return archived is None or now - archived > ARCHIVE_FAILING


def archiver_stalled(facts, now):
    """Segments wait (.ready) and neither the last success nor the oldest waiting
    segment is newer than 15 minutes: no failure recorded, no progress either.
    An idle stack has no .ready file; a segment that just became ready after
    hours of quiet is not a stall."""
    if not facts.get("ready"):
        return False
    times = [t for t in (facts.get("last_archived_time"), facts.get("oldest_ready")) if t is not None]
    return not times or now - max(times) > ARCHIVE_FAILING


def stamp(text):
    # dropped-wal.log has microseconds; older lines and pgBackRest have seconds.
    for layout in ("%Y-%m-%dT%H:%M:%S.%fZ", "%Y-%m-%dT%H:%M:%SZ"):
        try:
            return dt.datetime.strptime(text, layout).replace(tzinfo=dt.timezone.utc).timestamp()
        except ValueError:
            pass
    raise ValueError("not a UTC timestamp")


def iso(epoch):
    if epoch is None:
        return None
    return dt.datetime.fromtimestamp(epoch, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def dropped_events():
    """Times of recorded dropped-WAL warnings. No state directory is unreadable, not zero."""
    if not STATE.is_dir():
        raise OSError("backup state directory missing")
    path = STATE / DROPPED
    if not path.exists():
        return []
    events = []
    for line in path.read_text().splitlines():
        try:
            events.append(stamp(line.split()[0]))
        except (IndexError, ValueError):
            continue
    return events


def full_start(status):
    """pgBackRest's own start of the newest full, as perform() recorded it; 0
    (none recorded) keeps any recorded drop an open gap."""
    return status.get("full", {}).get("backup_start") or 0


def gap_open(events, start):
    """pgBackRest times have whole seconds: a drop in the same second as the
    full's start may have come after it, so it still counts as open."""
    return any(event >= start for event in events)


def free_bytes():
    usage = os.statvfs(DATA if DATA.exists() else STATE)
    return usage.f_bavail * usage.f_frsize


def health_problems(now=None):
    """Why postgres-backup is unhealthy, in short fixed words; empty when healthy.
    Local state and one socket query only; never a repository request here."""
    now = time.time() if now is None else now
    # No backup config is healthy only as an explicitly unprotected local stack.
    if not any(os.environ.get(k) for k in BACKUP_VARS) and not CONFIG.exists():
        return []
    try:
        replication = replication_facts(timeout=4)
    except (subprocess.SubprocessError, OSError, ValueError):
        replication = None
    if replication is not None and replication.get("in_recovery") and (DATA / "standby.signal").is_file():
        # A standby runs no backups: its health is streaming, replay and disk.
        problems = replication_problems(replication)
        try:
            if free_bytes() < DISK_RESERVE:
                problems.append("data volume has less than 4 GiB free")
        except OSError:
            problems.append("free space unreadable")
        return problems
    try:
        status = read_status()
    except (OSError, ValueError):
        return ["status.json unreadable"]
    problems = []
    for kind, age in (("incr", 26 * 3600), ("full", 8 * 86400), ("check", 26 * 3600)):
        success = status.get(kind, {}).get("success", 0)
        if not success or not 0 <= now - success <= age:
            problems.append(f"no {kind} success in the last {age // 3600} h (backup.sh {kind})")
    if status.get("last_error"):
        problems.append("last job error: " + str(status["last_error"])[:200])
    try:
        facts = archiver_facts(timeout=4)
        events = dropped_events()
        free = free_bytes()
    except (subprocess.SubprocessError, OSError, ValueError):
        return problems + ["archiver, dropped-WAL record or free space unreadable"]
    if gap_open(events, full_start(status)):
        problems.append("dropped WAL since the latest recorded full: PITR has a gap (backup.sh full)")
    if archiver_failing(facts, now):
        problems.append("WAL archiving has failed for over 15 minutes")
    if archiver_stalled(facts, now):
        problems.append("WAL waits to be archived with no progress for 15 minutes")
    if free < DISK_RESERVE:
        problems.append("data volume has less than 4 GiB free")
    if replication is not None:
        problems += replication_problems(replication)
    elif os.environ.get("KOMIKA_NODE_ID") or os.environ.get("KOMIKA_PG_REPLICA_CIDRS", "").strip():
        problems.append("replication state unreadable")
    return problems


def health(now=None):
    return not health_problems(now)


def repository_info(timeout=120):
    fence(("--output=json", "info"))
    entries = json.loads(collect(["pgbackrest", "--stanza=komika", "--output=json", "info"], timeout))
    if not isinstance(entries, list) or len(entries) != 1 or entries[0].get("name") != "komika":
        raise ValueError("unexpected repository info")
    return entries[0]


def tree_bytes(root):
    if not root.is_dir():
        raise OSError("directory missing")
    return sum((Path(top) / name).lstat().st_size for top, _, names in os.walk(root) for name in names)


def report_status(now=None, local=False):
    """Recovery facts as JSON. Unreadable means null plus a reason, never 0."""
    now = time.time() if now is None else now
    unavailable = []

    def lost(field, reason):
        unavailable.append(field + ": " + reason)
        return None

    def start(backup):
        return (backup.get("timestamp") or {}).get("start")

    def stop(backup):
        return (backup.get("timestamp") or {}).get("stop")

    def summary(backup):
        return {"label": backup.get("label"), "type": backup.get("type"),
                "start": iso(start(backup)), "stop": iso(stop(backup))}

    report = {"taken_at": iso(now)}
    try:
        status = read_status()
    except (OSError, ValueError):
        status = lost("jobs", "status.json unreadable")
    jobs = None
    if status is not None:
        jobs = {}
        for kind in ("full", "incr", "check"):
            state = status.get(kind, {})
            success = state.get("success")
            jobs[kind] = {"last_success": iso(success), "age_seconds": int(now - success) if success else None,
                          "error": state.get("error") or None}
            if not success:
                lost(f"jobs.{kind}", "no recorded success")
    report["jobs"] = jobs
    report["last_error"] = (status.get("last_error") or None) if status is not None else None

    entry = None
    if local:
        lost("backups", "not read (--local makes no repository request)")
    elif not CONFIG.exists():
        lost("backups", "no pgBackRest configuration in this container")
    else:
        try:
            entry = repository_info()
        except subprocess.CalledProcessError as error:
            lost("backups", f"pgbackrest info failed (exit {error.returncode})")
        except (subprocess.TimeoutExpired, OSError, ValueError, KeyError):
            lost("backups", "pgbackrest info failed or returned unexpected output")
    backups = sorted((b for b in entry.get("backup") or [] if type(stop(b)) is int),
                     key=stop) if entry is not None else None
    fulls = [b for b in backups or [] if b.get("type") == "full"]
    report["repository_status"] = (entry.get("status") or {}).get("message") if entry is not None else None
    report["latest_full_backup"] = summary(fulls[-1]) if fulls else None
    report["latest_backup"] = summary(backups[-1]) if backups else None
    if backups == []:
        lost("latest_backup", "repository holds no backup")
    archives = (entry.get("archive") or []) if entry is not None else []
    report["archive_min_wal"] = min((a["min"] for a in archives if a.get("min")), default=None)
    report["archive_max_wal"] = max((a["max"] for a in archives if a.get("max")), default=None)

    try:
        facts = archiver_facts()
    except (subprocess.SubprocessError, OSError, ValueError):
        facts = lost("archiver", "PostgreSQL did not answer on the local socket")
    if facts is not None:
        archived, failed = facts.get("last_archived_time"), facts.get("last_failed_time")
        oldest = facts.get("oldest_ready")
        report["archiver"] = {
            "archived_count": facts.get("archived_count"), "failed_count": facts.get("failed_count"),
            "last_archived_wal": facts.get("last_archived_wal"), "last_archived_time": iso(archived),
            "last_archived_age_seconds": None if archived is None else int(now - archived),
            "last_failed_wal": facts.get("last_failed_wal"), "last_failed_time": iso(failed),
            "last_failed_age_seconds": None if failed is None else int(now - failed),
            "failing": archiver_failing(facts, now), "stalled": archiver_stalled(facts, now),
            "oldest_ready_age_seconds": None if oldest is None else int(now - oldest),
            "in_recovery": facts.get("in_recovery")}
        report["ready_files"] = facts.get("ready")
    else:
        report["archiver"] = None
        try:
            report["ready_files"] = sum(1 for p in (DATA / "pg_wal" / "archive_status").iterdir()
                                        if p.name.endswith(".ready"))
        except OSError:
            report["ready_files"] = lost("ready_files", "neither SQL nor pg_wal/archive_status readable")
    try:
        report["spool_bytes"] = tree_bytes(STATE / "spool")
    except OSError:
        report["spool_bytes"] = lost("spool_bytes", "spool directory unreadable or archiving not configured")
    try:
        report["free_bytes"] = free_bytes()
    except OSError:
        report["free_bytes"] = lost("free_bytes", "data volume unreadable")
    report["free_bytes_reserve"] = DISK_RESERVE

    try:
        events = dropped_events()
    except OSError:
        events = lost("dropped_wal", "record unreadable")
    report["dropped_wal"] = None if events is None else {
        "events_recorded": len(events), "last": iso(max(events)) if events else None}
    last = max(events) if events else None
    if events is None:
        report["gap_open"] = lost("gap_open", "dropped-WAL record unreadable")
    elif not events:
        report["gap_open"] = False
    elif backups is not None:
        report["gap_open"] = not fulls or gap_open(events, start(fulls[-1]))
    elif status is not None:
        report["gap_open"] = gap_open(events, full_start(status))  # As recorded by perform().
    else:
        report["gap_open"] = lost("gap_open", "neither repository nor status.json readable")

    # The earliest point of the newest unbroken stretch; a full is the safe new base.
    if backups is None or events is None:
        recoverable = lost("recoverable_from", "needs the repository and the dropped-WAL record")
    elif events:
        after = [b for b in fulls if not gap_open(events, start(b))]
        recoverable = (stop(after[0]) if after else
                       lost("recoverable_from", "no full backup since the last dropped WAL; run backup.sh full"))
    elif backups:
        recoverable = stop(backups[0])
    else:
        recoverable = lost("recoverable_from", "repository holds no backup")
    report["recoverable_from"] = iso(recoverable)
    # When the archiver last finished a segment says nothing of the newest commit
    # inside it (old queued WAL can be archived late); only replay finds that.
    report["recoverable_until"] = lost(
        "recoverable_until", "not known without replaying WAL; archive_last_success is a completion "
        "time, not a recovery bound; latest_backup.stop is always reachable by restoring that backup")
    report["archive_last_success"] = iso(facts.get("last_archived_time")) if facts is not None else None
    report["archive_last_wal"] = facts.get("last_archived_wal") if facts is not None else None
    report["pitr"] = ("Point-in-time recovery only from recoverable_from, never across dropped WAL; how "
                      "far the archived WAL reaches is proven only by a restore. .done files and healthy "
                      "containers prove no continuity; a marker restore does.")
    try:
        report["node_id"] = node_id()
    except ValueError as error:
        report["node_id"] = lost("node_id", str(error))
    try:
        report["replication"] = replication_facts()
    except (subprocess.SubprocessError, OSError, ValueError):
        report["replication"] = lost("replication", "PostgreSQL did not answer on the local socket")
    if report["node_id"] is None:
        report["fence"] = None  # Fencing is off: no repository request for it.
    elif local:
        report["fence"] = lost("fence", "not read (--local makes no repository request)")
    elif not CONFIG.exists():
        report["fence"] = lost("fence", "no repository configuration in this container")
    else:
        try:
            report["fence"] = read_fence()[0]
            if report["fence"] is None:
                lost("fence", "fence.json is absent (no node has claimed it)")
        except (urllib.error.URLError, OSError, ValueError, KeyError):
            report["fence"] = lost("fence", "fence.json unreadable")
    report["health_problems"] = health_problems(now)
    report["unavailable"] = unavailable
    return report


def reading(now=None):
    """Budget reading for the dated M3 duty. Read-only; lists the prefix once."""
    now = time.time() if now is None else now
    unavailable = []
    report = {"taken_at": iso(now), "duty_dates": list(BUDGET_DATES), "action_bytes": BUDGET_ACTION}
    try:
        wal = json.loads(query(WAL))
        report["raw_wal"] = {"wal_bytes_since_reset": wal.get("wal_bytes"),
                             "stats_reset": iso(wal.get("stats_reset")), "lsn_bytes": wal.get("lsn_bytes")}
    except (subprocess.SubprocessError, OSError, ValueError, AttributeError):
        report["raw_wal"] = None
        unavailable.append("raw_wal: PostgreSQL did not answer on the local socket")
    report["repository"] = None
    if not CONFIG.exists():
        unavailable.append("repository: no pgBackRest configuration in this container")
    else:
        try:
            fence(("--recurse", "--output=json", "repo-ls"))
            listing = json.loads(collect(["pgbackrest", "--stanza=komika", "--recurse", "--output=json",
                                          "repo-ls"], 600))
            entries = listing.items() if isinstance(listing, dict) else [
                (e.get("name"), e) for e in listing if isinstance(e, dict)]
            files = {str(path): e["size"] for path, e in entries if isinstance(e, dict)
                     and e.get("type") == "file" and type(e.get("size")) is int}
            archive = sum(size for path, size in files.items() if path.startswith("archive/"))
            backup = sum(size for path, size in files.items() if path.startswith("backup/"))
            total = sum(files.values())
            report["repository"] = {"total_bytes": total, "archived_wal_bytes": archive, "backup_bytes": backup,
                                    "objects": len(files), "prefix_over_action_threshold": total >= BUDGET_ACTION,
                                    "prefix_below_action_bytes": BUDGET_ACTION - total}
        except subprocess.CalledProcessError as error:
            unavailable.append(f"repository: pgbackrest repo-ls failed (exit {error.returncode})")
        except (subprocess.TimeoutExpired, OSError, ValueError, AttributeError, TypeError):
            unavailable.append("repository: pgbackrest repo-ls failed or returned unexpected output")
    report["note"] = ("R2 account storage and Class A/B operation counts come from the Cloudflare "
                      "dashboard; this listing itself costs list (Class A) operations.")
    report["action_threshold_note"] = (
        "The 7 GB action threshold is account-wide: old Litestream data and other services share the "
        "allowance. prefix_over_action_threshold covers only this pgBackRest prefix; read the account "
        "total from the Cloudflare dashboard before deciding.")
    report["unavailable"] = unavailable
    return report


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
    if kind == "full":
        record_full(state)
    state["success"] = int(time.time())
    log(f"{kind} succeeded at {state['success']}")
    # Preserve other outstanding job errors until each failing type succeeds.
    state.pop("error", None)
    status["last_error"] = next((v["error"] for v in status.values()
                                if isinstance(v, dict) and v.get("error")), "")
    save_status(status)
    return True


def record_full(state):
    """pgBackRest's own start and stop of the newest full: the one gap boundary
    that health, status, watch and drill all use. If info cannot show it, the
    older record stays, so a recent drop keeps health red (never falsely green)."""
    try:
        fulls = [b for b in backup_info() or [] if b.get("type") == "full"
                 and type((b.get("timestamp") or {}).get("start")) is int
                 and type(b["timestamp"].get("stop")) is int]
    except (subprocess.CalledProcessError, ValueError, json.JSONDecodeError, OSError):
        fulls = []
    if not fulls:
        log("full succeeded but pgbackrest info did not show it; its start is not recorded")
        return
    newest = max(fulls, key=lambda b: b["timestamp"]["stop"])
    state.update(backup_label=newest.get("label"), backup_start=newest["timestamp"]["start"],
                 backup_stop=newest["timestamp"]["stop"])


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


def backup_gate():
    """Why this node must not run backup jobs now, or None. With fencing off
    nothing is checked (as before). A standby, or a primary the fence does not
    name, runs no jobs; an unreadable fence is a refusal too."""
    node = node_id()
    if node is None:
        return None
    try:
        if query("SELECT pg_is_in_recovery();", timeout=30).strip() == "t":
            return "this node is a standby"
        fence, _ = read_fence()
    except (subprocess.SubprocessError, urllib.error.URLError, OSError, ValueError, KeyError) as error:
        return f"fence or PostgreSQL unreadable ({type(error).__name__})"
    allowed, reason = fence_decision(fence, node, None)
    return None if allowed else reason


def fence_check():
    """`hosted.py fence-check`: may this node run as the primary? One JSON line;
    exit 0 yes, 1 no. Reads the fence and the data directory, changes nothing."""
    result = {"node_id": None, "fence": None, "local_timeline": None}
    try:
        node = result["node_id"] = node_id()
        if node is not None:
            if not CONFIG.exists():
                raise ValueError("no repository configuration in this container")
            if (DATA / "PG_VERSION").is_file():
                result["local_timeline"] = local_timeline()
            result["fence"] = read_fence()[0]
        fenced = result["fence"]
        allowed, reason = fence_decision(fenced, node, result["local_timeline"],
                                         fenced is not None and switched_to(fenced["timeline"]))
        problem = node_marker_problem(node)
        if allowed and problem:
            allowed, reason = False, problem
        if allowed and (DATA / "standby.signal").is_file():
            allowed, reason = False, "standby.signal is present: this node is a standby until promoted"
    except (subprocess.SubprocessError, urllib.error.URLError, OSError, ValueError, KeyError) as error:
        allowed, reason = False, f"fence check failed: {error}" if isinstance(error, ValueError) else \
            f"fence check failed ({type(error).__name__})"
    print(json.dumps(dict(result, allowed=allowed, reason=reason), sort_keys=True))
    return allowed


PROMOTE_USAGE = ("usage: hosted.py promote --old-primary-stopped OLD_NODE. Pass it only after you checked that "
                 "OLD_NODE, the primary that fence.json names now, cannot take writes: its host is powered off in "
                 "the provider console, or its PostgreSQL process is confirmed stopped. An unreachable port is not "
                 "proof: a network split can hide a primary that still takes writes. Nothing promotes automatically.")


def promote(argv):
    """Manual promotion of this standby: fence first, then pg_promote().

    1. Refuses without `--old-primary-stopped OLD_NODE`, and unless fence.json
       names OLD_NODE: the operator's word is about that node, so a second
       candidate that reads a fence a first promotion already moved is refused.
    2. Writes the next fence epoch naming this node, only if fence.json is
       unchanged since it was read, so two promotions from one fence cannot both win.
    3. pg_promote(), a checkpoint (the new timeline reaches pg_control, which
       fence checks read), and the server-facing komika.node_id /
       komika.fence_epoch settings. Starting the rest of the stack is
       promote.sh's job on the host.
    A rerun after a crash continues where it stopped: a fence that already names
    this node is not written again, and a node already out of recovery skips
    only pg_promote().
    """
    if len(argv) != 2 or argv[0] != "--old-primary-stopped" or not NODE.fullmatch(argv[1]):
        raise ValueError(PROMOTE_USAGE)
    old = argv[1]
    node = node_id()
    if node is None:
        raise ValueError("promotion needs KOMIKA_NODE_ID on this node")
    if old == node:
        raise ValueError("--old-primary-stopped names this node; name the old primary")
    if not CONFIG.exists():
        raise ValueError("promotion needs the backup repository configuration: fence.json lives there")
    before = replication_facts(timeout=30)
    # The timeline this copy is on: a restartpoint can lag the receiver.
    current = max(before.get("timeline") or 1, (before.get("receiver") or {}).get("received_tli") or 0)
    fence, etag = read_fence()
    if fence is not None and fence["primary_node"] == node:
        # A rerun: the fence holds the timeline this promotion predicted, one
        # above the copy it started from (or the new one, once promoted).
        if current < fence["timeline"] - (1 if before.get("in_recovery") else 0):
            raise ValueError(f"this copy is on timeline {current}, older than fence timeline {fence['timeline']}: "
                             "an old copy; rebuild it with standby-init")
        log(f"fence epoch {fence['epoch']} already names this node; finishing the promotion")
    else:
        if fence is None:
            raise ValueError("fence.json is absent: there is no fenced primary to take over from")
        if fence["primary_node"] != old:
            raise ValueError(f"fence.json epoch {fence['epoch']} names {fence['primary_node']}, not {old}: "
                             "another promotion may have moved it; check which node is the primary")
        if not before.get("in_recovery"):
            raise ValueError("this node is not a standby (not in recovery); refusing to move the fence")
        if current < fence["timeline"]:
            raise ValueError(f"this standby is on timeline {current}, older than fence timeline {fence['timeline']}: "
                             "it is an old copy and its promotion would reuse a timeline; rebuild it with standby-init")
        if not conditional_writes_honoured():
            raise ValueError("the repository store ignores conditional writes; a fence there cannot stop two "
                             "promotions, so this one is refused")
        fence = {"epoch": fence["epoch"] + 1, "primary_node": node,
                 # A lower bound: PostgreSQL picks a timeline above every one it knows.
                 "timeline": current + 1, "at": iso(time.time())}
        if not write_fence(fence, etag):
            raise ValueError("fence.json changed since it was read: another promotion won; this node stays a standby")
        log(f"fence.json epoch {fence['epoch']} now names this node ({node})")
    if before.get("in_recovery"):
        if query("SELECT pg_promote(true, 300);", timeout=330).strip() != "t":
            raise ValueError("pg_promote() did not finish within 300 s; run promote again")
    query("CHECKPOINT;", timeout=900)
    promoted = replication_facts(timeout=30)
    if promoted.get("in_recovery") or (promoted.get("timeline") or 0) < fence["timeline"]:
        raise ValueError(f"after pg_promote() the timeline is {promoted.get('timeline')}, below fence timeline "
                         f"{fence['timeline']}; the server settings were not set")
    # The standby settings go too: a standby later copied from here gets its own.
    query(f"ALTER SYSTEM SET komika.node_id = '{node}';\n"
          f"ALTER SYSTEM SET komika.fence_epoch = '{fence['epoch']}';\n"
          "ALTER SYSTEM RESET primary_conninfo;\nALTER SYSTEM RESET primary_slot_name;\n"
          "SELECT pg_reload_conf();\n", timeout=30)
    write_node_marker(node)
    print(json.dumps({"promoted": True, "node_id": node, "fence": fence, "timeline": promoted.get("timeline"),
                      "last_replayed_lsn": before.get("replay_lsn"),
                      "last_replayed_commit": iso(before.get("last_replayed_commit")),
                      "promoted_at": iso(time.time())}, sort_keys=True))
    return fence


def standby_init(argv):
    """`hosted.py standby-init --primary HOST:PORT` in a one-off container with an
    empty data volume: pg_basebackup over TLS with the replication slot, then
    standby.signal and primary_conninfo. Start the postgres service afterwards."""
    if len(argv) != 2 or argv[0] != "--primary":
        raise ValueError("usage: hosted.py standby-init --primary HOST:PORT")
    host, _, port = argv[1].rpartition(":")
    host = host.strip("[]")
    if not host or not port.isdigit() or not 0 < int(port) < 65536 or not re.fullmatch(r"[A-Za-z0-9.:-]+", host):
        raise ValueError("--primary must be HOST:PORT (a name or an address, and a port)")
    node = node_id()
    if node is None:
        raise ValueError("standby-init needs KOMIKA_NODE_ID on this node")
    if not tls_files("ca.crt", "client.crt", "client.key"):
        raise ValueError(f"standby-init needs ca.crt, client.crt and client.key in {TLS}")
    if DATA.exists() and (DATA.is_symlink() or any(DATA.iterdir())):
        raise ValueError("standby-init needs an empty data directory")
    fence, _ = read_fence()
    if fence is None:
        raise ValueError("fence.json is absent: start the primary with KOMIKA_NODE_ID first, so it claims the fence")
    if fence["primary_node"] == node:
        raise ValueError(f"fence.json names this node ({node}) as the primary; a standby here would leave no primary")
    conninfo = (f"host={host} port={port} user={REPLICA_ROLE} sslmode=verify-full sslrootcert={TLS}/ca.crt "
                f"sslcert={TLS}/client.crt sslkey={TLS}/client.key application_name={node}")
    def replication_command(command):
        return collect(["psql", "-X", "-w", "-At", conninfo + " replication=true", "-c", command], 60).strip()

    # slot_type|restart_lsn|restart_tli; a missing slot reads as an empty row.
    slot_type, _, rest = replication_command(f"READ_REPLICATION_SLOT {SLOT}").partition("|")
    if slot_type and not rest.partition("|")[0]:
        # Invalidated past max_slot_wal_keep_size: it holds no WAL and cannot be
        # used again. Dropping fails while a standby still uses it.
        replication_command(f"DROP_REPLICATION_SLOT {SLOT}")
        log(f"dropped the lost slot {SLOT} on the primary")
        slot_type = ""
    create = slot_type == ""
    DATA.mkdir(parents=True, exist_ok=True, mode=0o700)
    run(["pg_basebackup", "-d", conninfo, "-D", str(DATA), "-X", "stream", "--checkpoint=fast",
         f"--slot={SLOT}", *(["--create-slot"] if create else []), "--no-password"])
    auto = DATA / "postgresql.auto.conf"
    kept = [line for line in auto.read_text().splitlines(keepends=True)
            if not re.match(r"\s*primary_(conninfo|slot_name)\s*=", line)]
    atomic(auto, "".join(kept) + f"primary_conninfo = '{conninfo}'\nprimary_slot_name = '{SLOT}'\n")
    (DATA / "standby.signal").write_text("")
    write_node_marker(node)
    log(f"standby initialized from {host}:{port} with slot {SLOT}; start the postgres service")


def git_work_tree(path):
    """The work tree holding path (or the nearest existing parent), or None."""
    for parent in (path.resolve(), *path.resolve().parents):
        if (parent / ".git").exists():
            return parent
    return None


def standby_certs(argv):
    """`hosted.py standby-certs DIR NODE=HOST ...` on the operator's machine (needs
    openssl): one CA, and per node a server certificate for HOST plus a client
    certificate for the replication role. DIR/ca.key never leaves this machine;
    copy DIR/NODE/ to that node's TLS directory."""
    allow_git = "--inside-git-work-tree" in argv
    argv = [arg for arg in argv if arg != "--inside-git-work-tree"]
    if len(argv) < 2:
        raise ValueError("usage: hosted.py standby-certs [--inside-git-work-tree] DIR NODE=HOST [NODE=HOST ...]")
    out, pairs = Path(argv[0]), []
    tree = git_work_tree(out)
    if tree is not None and not allow_git:
        # ca.key signs every replication login; a `git add -A` would publish it.
        raise ValueError(f"{out} is inside the git work tree {tree}: private keys do not belong there. "
                         "Use a directory outside it (for example ~/komika-standby-certs), or pass "
                         "--inside-git-work-tree if it is ignored there on purpose")
    for pair in argv[1:]:
        node, _, host = pair.partition("=")
        if not NODE.fullmatch(node) or not host:
            raise ValueError("each node is NODE=HOST with a node name like the KOMIKA_NODE_ID")
        try:
            san = "IP:" + str(ipaddress.ip_address(host))
        except ValueError:
            if not re.fullmatch(r"(?=.{1,253}$)[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)*", host):
                raise ValueError(f"{host} is neither an address nor a host name")
            san = "DNS:" + host
        if (out / node).exists():
            raise ValueError(f"{out / node} exists; remove it to issue new certificates for {node}")
        pairs.append((node, host, san))
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    ca_key, ca_crt = out / "ca.key", out / "ca.crt"
    old = os.umask(0o077)
    try:
        if not ca_key.exists():
            run(["openssl", "req", "-x509", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                 "-nodes", "-keyout", str(ca_key), "-out", str(ca_crt), "-days", "3650",
                 "-subj", "/CN=komika standby CA", "-addext", "basicConstraints=critical,CA:TRUE",
                 "-addext", "keyUsage=critical,keyCertSign,cRLSign"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for node, host, san in pairs:
            directory = out / node
            directory.mkdir(mode=0o700)
            for name, subject, usage in (("server", host, f"subjectAltName={san}\nextendedKeyUsage=serverAuth\n"),
                                         ("client", REPLICA_ROLE, "extendedKeyUsage=clientAuth\n")):
                key, csr, crt = (directory / f"{name}.{ext}" for ext in ("key", "csr", "crt"))
                ext = directory / f"{name}.ext"
                ext.write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n" + usage)
                run(["openssl", "req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
                     "-keyout", str(key), "-out", str(csr), "-subj", f"/CN={subject}"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                run(["openssl", "x509", "-req", "-in", str(csr), "-CA", str(ca_crt), "-CAkey", str(ca_key),
                     "-set_serial", "0x" + os.urandom(16).hex(), "-days", "825", "-extfile", str(ext),
                     "-out", str(crt)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                csr.unlink()
                ext.unlink()
            (directory / "ca.crt").write_bytes(ca_crt.read_bytes())
            log(f"{directory}: server.crt for {host}, client.crt for {REPLICA_ROLE}, ca.crt")
    finally:
        os.umask(old)


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else "postgres"
    if command == "health":
        sys.exit(0 if health() else 1)
    if command == "standby-certs":
        standby_certs(sys.argv[2:])
        return
    if command in ("fence-check", "promote"):
        # Operator commands in a running container: they read the private config
        # that PostgreSQL's start wrote and never rewrite it.
        if command == "fence-check":
            if sys.argv[2:]:
                raise ValueError("usage: hosted.py fence-check")
            sys.exit(0 if fence_check() else 1)
        promote(sys.argv[2:])
        return
    if command in ("status", "reading"):
        # Collectors only read: the scheduler's private config, never a rewrite.
        local = sys.argv[2:] == ["--local"] and command == "status"
        if sys.argv[2:] and not local:
            raise ValueError("usage: hosted.py status [--local] | hosted.py reading")
        os.environ["PGBACKREST_CONFIG"] = str(CONFIG)
        print(json.dumps(report_status(local=local) if command == "status" else reading(),
                         indent=1, sort_keys=True))
        return
    archive = configure()
    if command == "postgres":
        start(archive)
    elif command == "scheduler":
        threading.Thread(target=monitoring_grant, daemon=True).start()
        if not archive:
            while True:
                time.sleep(86400)
        refused = None
        while True:
            busy = False
            gate = backup_gate()
            if gate != refused:
                log("backup jobs paused: " + gate if gate else "backup jobs run on this node")
                refused = gate
            try:
                if gate is None:
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
            # A paused node checks again each minute: after a promotion the due
            # slots run within a minute, not at the next 02:00 UTC.
            time.sleep(min(delay, 60) if busy or gate else delay)
    elif command == "job":
        if not archive:
            raise ValueError("backup job requires complete R2 configuration")
        kind = sys.argv[2]
        gate = backup_gate()
        if gate:
            raise ValueError("backup job refused: " + gate)
        with locked():
            status = read_status()
            if kind == "bootstrap-full":
                # Deliberate operator action before the first full chain.
                if not perform("full", slots(time.time())["full"], status, bootstrap=True):
                    sys.exit(1)
            elif not perform(kind, slots(time.time())[kind], status):
                sys.exit(1)
    elif command == "standby-init":
        if not archive:
            raise ValueError("standby-init needs the backup repository configuration: fence.json lives there")
        standby_init(sys.argv[2:])
    elif command == "exec":
        # Recovery/operator commands receive generated private config; no keys in argv.
        os.execvp(sys.argv[2], sys.argv[2:])
    else:
        raise ValueError("unknown hosted PostgreSQL command")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, subprocess.SubprocessError, OSError, json.JSONDecodeError,
            ET.ParseError, configparser.Error, KeyError) as exc:
        log(f"FATAL: {exc}" if isinstance(exc, ValueError) else
            f"FATAL: {type(exc).__name__}; startup/job refused (inspect preceding diagnostics)")
        sys.exit(1)
