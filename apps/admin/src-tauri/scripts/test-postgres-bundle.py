#!/usr/bin/env python3
"""Offline staging checks: fake archives only; never downloads or starts PostgreSQL."""
import hashlib
import importlib.util
import io
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[4]
spec = importlib.util.spec_from_file_location("desktop_bundle", REPO / "scripts/postgres-desktop-bundle.py")
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)


class BundleTests(unittest.TestCase):
    def archive(self, root):
        path = root / "runtime.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            names = {f"runtime/bin/{name}": name for name in (*bundle.PROGRAMS, "pg_isready", "unneeded_tool")}
            names.update({
                "runtime/share/extension/pg_trgm.control": "module_pathname = '$libdir/pg_trgm'",
                "runtime/share/extension/pg_trgm--1.6.sql": "CREATE FUNCTION",
                "runtime/lib/pg_trgm.so": "module",
                "runtime/lib/pgxs/test_tool": "build tool",
                "runtime/lib/libecpg.so": "drop",
                "runtime/lib/libpgtypes.so": "drop",
                "runtime/lib/uuid-ossp.so": "drop",
                "runtime/lib/plpython3.so": "drop",
                "runtime/share/extension/plpython3.control": "drop",
                "runtime/COPYRIGHT": "PostgreSQL notice fixture",
                "NOTICE": "Distributor notice fixture",
            })
            for share in ("postgres.bki", "postgresql.conf.sample", "pg_hba.conf.sample", "pg_ident.conf.sample",
                          "information_schema.sql", "system_views.sql", "system_functions.sql", "system_constraints.sql",
                          "sql_features.txt", "snowball_create.sql", "timezonesets/Default", "tsearch_data/english.stop"):
                names[f"runtime/share/{share}"] = "fixture"
            for name, text in names.items():
                value = text.encode()
                member = tarfile.TarInfo(name)
                member.size = len(value)
                member.mode = 0o755 if "/bin/" in name else 0o644
                tar.addfile(member, io.BytesIO(value))
        return path

    def test_every_target_is_pinned_and_has_its_archive_size(self):
        # The gate targets and the one target that is only buildable (Intel macOS).
        self.assertEqual({triple: slug for triple, (slug, _) in bundle.TARGETS.items()}, {
            "aarch64-apple-darwin": "aarch64-macos",
            "x86_64-apple-darwin": "x86_64-macos",
            "x86_64-unknown-linux-gnu": "x86_64-linux",
            "aarch64-unknown-linux-gnu": "aarch64-linux",
            "x86_64-pc-windows-msvc": "x86_64-windows",
        })
        sizes = bundle.spike_rules().ASSETS
        for triple, (_, pin) in bundle.TARGETS.items():
            self.assertRegex(pin, r"^[a-f0-9]{64}$", triple)
            self.assertGreater(sizes[triple][0], 10_000_000, triple)
        self.assertEqual(bundle.TARGETS["aarch64-unknown-linux-gnu"][1],
                         "5d2b5e8be9e96bfc3d7f22090057a836ec36d5d092542d8d3b93cc145cf4ee13")
        self.assertEqual(sizes["aarch64-unknown-linux-gnu"][0], 11979851)

    def test_unpinned_targets_refuse_before_network_or_writes(self):
        unpinned = {"x86_64-apple-darwin": ("x86_64-macos", None)}
        with tempfile.TemporaryDirectory() as root, patch.dict(bundle.TARGETS, unpinned), patch.object(bundle.urllib.request, "urlopen", side_effect=AssertionError("network forbidden")):
            destination = Path(root) / "staged"
            with self.assertRaisesRegex(ValueError, "pin REQUIRED"):
                bundle.stage("x86_64-apple-darwin", None, destination)
            self.assertFalse(destination.exists())

    def test_verified_archive_trim_manifest_notices_and_target_only_mapping(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            archive = self.archive(root)
            triple = "x86_64-unknown-linux-gnu"
            pin = bundle.digest(archive)
            rules = bundle.spike_rules()
            rules.ASSETS[triple] = (archive.stat().st_size, pin)
            destination = root / "postgres"
            (destination / "other-target/18").mkdir(parents=True)
            with patch.dict(bundle.TARGETS, {triple: ("x86_64-linux", pin)}), patch.object(bundle, "spike_rules", return_value=rules):
                staged = bundle.stage(triple, archive, destination)
            manifest = json.loads((staged / "manifest.json").read_text())
            self.assertEqual(manifest["version"], "18.6")
            self.assertEqual(manifest["target"], "x86_64-linux")
            self.assertEqual(manifest["archive_sha256"], pin)
            for name, digest in manifest["files"].items():
                self.assertEqual(bundle.digest(staged / name), digest)
            self.assertTrue((staged / "lib/pg_trgm.so").exists())
            self.assertTrue((staged / "licenses/NOTICE").exists())
            for excluded in ["lib/pgxs/test_tool", "lib/libecpg.so", "lib/libpgtypes.so", "lib/uuid-ossp.so", "lib/plpython3.so", "bin/pg_isready", "bin/unneeded_tool"]:
                self.assertFalse((staged / excluded).exists(), excluded)
            self.assertEqual([p.name for p in (destination / "bundle").iterdir()], ["x86_64-linux"])
            self.assertTrue((destination / "other-target/18").exists())

    def test_bundle_only_check_missing_empty_wrong_and_right_target(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            destination = root / "postgres"
            triple = "x86_64-unknown-linux-gnu"
            with self.assertRaises(OSError):
                bundle.check_bundle(triple, destination)
            (destination / "bundle/x86_64-linux/18").mkdir(parents=True)
            (destination / "bundle/x86_64-linux/18/manifest.json").write_text("{}")
            with self.assertRaises(ValueError):
                bundle.check_bundle(triple, destination)
            archive = self.archive(root)
            pin = bundle.digest(archive)
            rules = bundle.spike_rules()
            rules.ASSETS[triple] = (archive.stat().st_size, pin)
            with patch.dict(bundle.TARGETS, {triple: ("x86_64-linux", pin)}), patch.object(bundle, "spike_rules", return_value=rules):
                bundle.stage(triple, archive, destination)
            staged = bundle.check_bundle(triple, destination)
            with self.assertRaises(OSError):
                bundle.check_bundle("x86_64-apple-darwin", destination)
            manifest_path = staged / "manifest.json"
            manifest = json.loads(manifest_path.read_text())
            manifest["target"] = "x86_64-macos"
            manifest_path.write_text(json.dumps(manifest))
            with self.assertRaises(ValueError):
                bundle.check_bundle(triple, destination)
            manifest["target"] = "x86_64-linux"
            manifest_path.write_text(json.dumps(manifest))
            (staged / "bin/postgres").write_bytes(b"")
            with self.assertRaisesRegex(ValueError, "empty"):
                bundle.check_bundle(triple, destination)

    def test_checksum_failure_preserves_previous_tree(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            archive = self.archive(root)
            triple = "x86_64-unknown-linux-gnu"
            rules = bundle.spike_rules()
            rules.ASSETS[triple] = (archive.stat().st_size, "0" * 64)
            destination = root / "postgres"
            previous = destination / "x86_64-linux/18/previous"
            previous.parent.mkdir(parents=True)
            previous.write_text("previous resource")
            with patch.dict(bundle.TARGETS, {triple: ("x86_64-linux", "0" * 64)}), patch.object(bundle, "spike_rules", return_value=rules):
                with self.assertRaisesRegex(ValueError, "mismatch"):
                    bundle.stage(triple, archive, destination)
            self.assertEqual(previous.read_text(), "previous resource")


if __name__ == "__main__":
    unittest.main()
