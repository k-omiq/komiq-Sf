#!/usr/bin/env python3
"""Stage the pinned PostgreSQL desktop payload. Python >=3.12, stdlib only.

--archive permits offline staging. Missing independent pins always fail closed.
The frozen spike supplies the audited extraction and conservative trim rules.
"""
import argparse
import hashlib
import importlib.util
import json
import re
import shutil
import sys
import tempfile
import urllib.request
from pathlib import Path

sys.dont_write_bytecode = True
RELEASE = "18.6.0"
# SHA-256 of the release assets, computed by the orchestrator from its own
# download of theseus-rs/postgresql-binaries 18.6.0 on 2026-10-11.
SHA256_X86_64_MACOS_REQUIRED = "f8918fbe747e0d79bda58ad8f8afcf23ac384b847ff3af18856f0825ba54b031"
SHA256_X86_64_LINUX_REQUIRED = "bb3d09f876b2383e25a8c9ce09e32d185a03656a23d074f5195542b1b1b3ca61"
SHA256_X86_64_WINDOWS_REQUIRED = "7da44c2dbcda3b49688ea08ce8cf99cfe677adf565f53a9145bf9002c74db7d5"
SHA256_AARCH64_LINUX_REQUIRED = "5d2b5e8be9e96bfc3d7f22090057a836ec36d5d092542d8d3b93cc145cf4ee13"
TARGETS = {
    "aarch64-apple-darwin": ("aarch64-macos", "a257bcdb8aa3301a13d6a5bcec48f8c9517045b7cbae71e50788b2615539e95b"),
    "x86_64-apple-darwin": ("x86_64-macos", SHA256_X86_64_MACOS_REQUIRED),
    "x86_64-unknown-linux-gnu": ("x86_64-linux", SHA256_X86_64_LINUX_REQUIRED),
    "aarch64-unknown-linux-gnu": ("aarch64-linux", SHA256_AARCH64_LINUX_REQUIRED),
    "x86_64-pc-windows-msvc": ("x86_64-windows", SHA256_X86_64_WINDOWS_REQUIRED),
}
PROGRAMS = ("postgres", "initdb", "pg_ctl", "pg_dump", "pg_restore", "psql")
REPO = Path(__file__).resolve().parent.parent


def spike_rules():
    spec = importlib.util.spec_from_file_location("postgres_spike_rules", Path(__file__).with_name("postgres-desktop-spike.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def stage(triple, archive, destination):
    slug, pin = TARGETS[triple]
    if pin is None or not re.fullmatch(r"[a-f0-9]{64}", pin):
        raise ValueError(f"Independent SHA-256 pin REQUIRED for {triple}; staging is disabled")
    rules = spike_rules()
    destination = destination.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    # Build privately. Existing runtime is replaced only after verification succeeds.
    with tempfile.TemporaryDirectory(prefix="postgres-stage-", dir=destination) as scratch:
        work = Path(scratch)
        if archive is None:
            archive = work / f"postgresql-{RELEASE}-{triple}.tar.gz"
            url = f"https://github.com/theseus-rs/postgresql-binaries/releases/download/{RELEASE}/{archive.name}"
            with urllib.request.urlopen(url, timeout=60) as source, archive.open("xb") as output:
                total = 0
                while block := source.read(1024 * 1024):
                    total += len(block)
                    if total > rules.ASSETS[triple][0]:
                        raise ValueError("Archive exceeds pinned size")
                    output.write(block)
        if archive.stat().st_size != rules.ASSETS[triple][0] or digest(archive) != pin:
            raise ValueError("Archive size or pinned SHA-256 mismatch")
        rules.extract_safe(archive, work / "extracted")
        root = rules.find_bundle(work / "extracted", triple)
        inventory = rules.inventory(root, triple)
        names = [n for n in inventory["runtime_files"] if Path(n).name not in ("pg_isready", "pg_isready.exe")]
        tree = work / "runtime"
        rules.copy_runtime(root, names, tree)
        # Include notices outside the PostgreSQL root too, retaining their provenance.
        for notice in rules.files(work / "extracted"):
            if not rules.contained(notice, root) and re.search(r"licen[cs]e|notice|copyright|copying", notice.name, re.I):
                target = tree / "licenses" / notice.relative_to(work / "extracted")
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(notice, target)
        exe = ".exe" if "windows" in triple else ""
        required = [f"bin/{name}{exe}" for name in PROGRAMS]
        required += [n for n in names if "pg_trgm" in Path(n).name]
        if not all(inventory["pg_trgm"].values()) or not all((tree / n).is_file() for n in required):
            raise ValueError("Missing runtime tool or pg_trgm")
        manifest = {"version": "18.6", "release": RELEASE, "target": slug, "triple": triple,
                    "archive_sha256": pin, "required": required,
                    "files": {p.relative_to(tree).as_posix(): digest(p) for p in rules.files(tree)}}
        (tree / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        final = destination / slug / "18"
        final.parent.mkdir(parents=True, exist_ok=True)
        bundle = destination / "bundle"
        if bundle.exists():
            for child in bundle.iterdir():
                marker = child / "18/manifest.json"
                if not marker.is_file() or child.name not in {value[0] for value in TARGETS.values()}:
                    raise ValueError("Refusing to replace an unmarked target-only bundle tree")
        if final.exists():
            previous_manifest = final / "manifest.json"
            if not previous_manifest.is_file():
                raise ValueError(f"Refusing to replace an unmarked runtime directory: {final}")
            previous = json.loads(previous_manifest.read_text(encoding="utf-8"))
            if previous.get("target") != slug or previous.get("version") != "18.6":
                raise ValueError("Previous staging manifest does not match this target/version")
        # Tauri's static resource mapping contains just this target, even if other
        # target trees have been staged in the same developer checkout.
        package = work / "bundle"
        shutil.copytree(tree, package / slug / "18", symlinks=True)
        if final.exists():
            shutil.rmtree(final)  # Only generated install resources; never app data.
        tree.rename(final)
        if bundle.exists():
            shutil.rmtree(bundle)
        package.rename(bundle)
        return final


def check_bundle(triple, destination):
    """Read-only validation for Tauri's bundle-only hook, including debug bundles."""
    slug, _ = TARGETS[triple]
    bundle = destination / "bundle"
    root = bundle / slug / "18"
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    if (manifest.get("version"), manifest.get("target"), manifest.get("triple")) != ("18.6", slug, triple):
        raise ValueError("Manifest target/version does not match build target")
    if any(p.name != slug for p in bundle.iterdir()):
        raise ValueError("Bundle contains another target or unowned entry")
    files = manifest.get("files", {})
    required = manifest.get("required", [])
    exe = ".exe" if slug.endswith("windows") else ""
    must = [f"bin/{p}{exe}" for p in PROGRAMS]
    must += [f"share/{p}" for p in ("postgres.bki", "postgresql.conf.sample", "pg_hba.conf.sample",
             "pg_ident.conf.sample", "information_schema.sql", "system_views.sql", "system_functions.sql",
             "system_constraints.sql", "sql_features.txt", "snowball_create.sql", "extension/pg_trgm.control")]
    if not isinstance(files, dict) or not isinstance(required, list) or not files or not required or not all(p in files for p in must):
        raise ValueError("Missing required PostgreSQL programs/share files")
    for prefix in ("share/extension/pg_trgm--", "share/timezonesets/", "share/tsearch_data/"):
        if not any(n.startswith(prefix) for n in files):
            raise ValueError(f"Missing {prefix}")
    if not any(n.startswith("lib/") and "pg_trgm." in n for n in files):
        raise ValueError("Missing pg_trgm library")
    if not all(isinstance(n, str) for n in required):
        raise ValueError("Invalid required path list")
    for name in set(files) | set(required):
        relative = Path(name)
        if not name or relative.is_absolute() or "\\" in name or ".." in relative.parts:
            raise ValueError(f"Unsafe manifest path: {name}")
        path = root / relative
        if not path.resolve().is_relative_to(root.resolve()) or not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"Missing, empty or escaping resource: {name}")
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--triple", choices=TARGETS, required=True)
    parser.add_argument("--check", action="store_true", help="validate target-only bundle offline; never fetch")
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--destination", type=Path, default=REPO / "apps/admin/src-tauri/postgres")
    args = parser.parse_args()
    if sys.version_info < (3, 12):
        parser.error("Python >=3.12 required")
    try:
        print(check_bundle(args.triple, args.destination) if args.check else stage(args.triple, args.archive, args.destination))
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"PostgreSQL staging/validation failed: {error}. Run scripts/postgres-desktop-bundle.py for the build target.\n")


if __name__ == "__main__":
    main()
