#!/usr/bin/env python3
"""Bootstrap PostgreSQL 18 for disposable Rust tests and stop only their cluster."""
import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import urllib.request

RELEASE = "18.6.0"
PINNED = {"aarch64-apple-darwin": "a257bcdb8aa3301a13d6a5bcec48f8c9517045b7cbae71e50788b2615539e95b"}

def safe_extract(bundle, root):
    """Extract on Python 3.9+ without trusting archive paths or link traversal."""
    root = root.resolve()
    members = bundle.getmembers()
    names = {}
    for member in members:
        path = PurePosixPath(member.name)
        if path.is_absolute() or ".." in path.parts or not path.parts:
            raise RuntimeError("PostgreSQL archive has an unsafe member path")
        name = str(path)
        if name in names:
            raise RuntimeError("PostgreSQL archive has duplicate member paths")
        if not (member.isdir() or member.isfile() or member.issym() or member.islnk()):
            raise RuntimeError("PostgreSQL archive contains an unsupported special file")
        names[name] = member
    # Never write a regular member through an archive-provided link. Library
    # symlinks are created only after all regular files and directories exist.
    for name, member in names.items():
        path = PurePosixPath(name)
        for parent in path.parents:
            ancestor = names.get(str(parent))
            if ancestor and not ancestor.isdir():
                raise RuntimeError("PostgreSQL archive writes through a non-directory")
        target = root.joinpath(*path.parts)
        if member.issym() or member.islnk():
            link = PurePosixPath(member.linkname)
            if link.is_absolute():
                raise RuntimeError("PostgreSQL archive has an absolute link")
            base = target.parent if member.issym() else root
            try:
                (base / str(link)).resolve().relative_to(root)
            except (ValueError, RuntimeError):
                raise RuntimeError("PostgreSQL archive link escapes its directory")
    for name, member in names.items():
        target = root.joinpath(*PurePosixPath(name).parts)
        if member.isdir():
            target.mkdir(parents=True, exist_ok=True)
        elif member.isfile():
            target.parent.mkdir(parents=True, exist_ok=True)
            with bundle.extractfile(member) as source, target.open("xb") as destination:
                shutil.copyfileobj(source, destination)
            target.chmod(member.mode & 0o777)
    pending = [(name, member) for name, member in names.items() if member.islnk()]
    while pending:
        remaining = []
        for name, member in pending:
            source = root / member.linkname
            if not source.is_file():
                remaining.append((name, member))
                continue
            target = root.joinpath(*PurePosixPath(name).parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            os.link(source, target)
        if len(remaining) == len(pending):
            raise RuntimeError("PostgreSQL archive has an unresolved hard link")
        pending = remaining
    for name, member in names.items():
        if member.issym():
            target = root.joinpath(*PurePosixPath(name).parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.symlink_to(member.linkname)
    # Resolve the complete link graph too: links may refer to later members.
    for name, member in names.items():
        if member.issym():
            try:
                root.joinpath(*PurePosixPath(name).parts).resolve().relative_to(root)
            except (ValueError, RuntimeError):
                raise RuntimeError("PostgreSQL archive has an unsafe link graph")

def provision(root):
    machine = {"arm64": "aarch64", "AMD64": "x86_64"}.get(platform.machine(), platform.machine())
    system = {"Darwin": "apple-darwin", "Linux": "unknown-linux-gnu", "Windows": "pc-windows-msvc"}.get(platform.system())
    if system is None:
        raise RuntimeError("PostgreSQL test bootstrap does not support this host")
    triple = f"{machine}-{system}"
    # The Rust caller validates this runtime's executables and major version.
    # Its earlier audit recorded the archive checksum; reuse does not rehash it.
    artifacts = Path.home() / ".codex/artifacts/komika"
    if artifacts.is_dir():
        for candidate in artifacts.glob(f"*/runtime/postgresql-{RELEASE}-{triple}/bin"):
            if (candidate / "postgres").is_file():
                print(candidate)
                return
    root.mkdir(parents=True, exist_ok=True)
    asset = f"postgresql-{RELEASE}-{triple}.tar.gz"
    base = f"https://github.com/theseus-rs/postgresql-binaries/releases/download/{RELEASE}/"
    archive = root / asset
    with urllib.request.urlopen(base + asset, timeout=60) as response, archive.open("wb") as target:
        shutil.copyfileobj(response, target)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    if triple in PINNED:
        expected = PINNED[triple]
    else:
        with urllib.request.urlopen(base + asset + ".sha256", timeout=60) as response:
            matches = re.findall(r"\b[a-fA-F0-9]{64}\b", response.read(65536).decode("ascii"))
        if len(matches) != 1:
            raise RuntimeError("PostgreSQL archive checksum manifest is ambiguous")
        expected = matches[0].lower()
    if digest != expected:
        raise RuntimeError("PostgreSQL archive checksum does not match")
    with tarfile.open(archive) as bundle:
        safe_extract(bundle, root)
    archive.unlink()
    bins = list(root.glob("*/bin/postgres"))
    if len(bins) != 1:
        raise RuntimeError("PostgreSQL archive has unexpected layout")
    print(bins[0].parent)

def watch(root, binary):
    # The only authority to stop/remove comes from a private harness directory,
    # created in this process tree; never interpret a database URL or shared PID.
    # The Rust parent starts us in a separate process group. Also ignore direct
    # termination signals: ownership ends at pipe EOF, not an unrelated Ctrl-C.
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if not root.name.startswith("komika-pg-test-") or not (root / "data/PG_VERSION").is_file():
        raise RuntimeError("Refusing cleanup outside an owned test cluster")
    try:
        sys.stdin.buffer.read()
    finally:
        cleanup(root, binary)

def cleanup(root, binary):
    # Keep diagnostics beside (not inside) the directory that will be removed.
    # These lifecycle commands contain only the owned datadir, never a URL.
    diagnostic = root.with_name(root.name + ".cleanup.log")
    def record(message):
        try:
            descriptor = os.open(diagnostic, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
            with os.fdopen(descriptor, "w") as stream:
                stream.write(message + "\n")
        except OSError:
            print(message, file=sys.stderr, flush=True)
    def missing_ok(operation, path, exception):
        # Startup failure and EOF cleanup can race over the same owned files.
        # Python 3.9 does not yet ignore those disappearing descendants itself.
        if not isinstance(exception[1], FileNotFoundError):
            raise exception[1]
    try:
        if not root.exists():
            record("Owned PostgreSQL test directory was already removed")
            return
        result = subprocess.run([str(binary / "pg_ctl"), "-D", str(root / "data"), "stop", "-m", "immediate", "-w"], capture_output=True, text=True, timeout=30, check=False)
        record("Owned PostgreSQL stop exit={}\n{}{}".format(result.returncode, result.stdout, result.stderr).rstrip())
        if result.returncode:
            status = subprocess.run([str(binary / "pg_ctl"), "-D", str(root / "data"), "status"], capture_output=True, text=True, timeout=10, check=False)
            if status.returncode != 3 and root.exists():
                raise RuntimeError("Owned PostgreSQL shutdown could not be confirmed; data retained")
        shutil.rmtree(root, onerror=missing_ok)
        record("Owned PostgreSQL test directory removed")
    except Exception as error:
        record("Owned PostgreSQL cleanup failed: {}: {}".format(type(error).__name__, error))
        raise

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", type=Path)
    parser.add_argument("--watch", type=Path)
    parser.add_argument("--bin", type=Path)
    args = parser.parse_args()
    if args.runtime:
        provision(args.runtime)
    elif args.watch and args.bin:
        watch(args.watch, args.bin)
    else:
        parser.error("supply --runtime or --watch and --bin")
