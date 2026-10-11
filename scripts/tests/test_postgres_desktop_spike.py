"""Offline unit and orchestration checks; no download, sockets, or PostgreSQL.
Run: python3.12 -B -m unittest discover -s scripts/tests -p 'test_postgres_desktop_spike.py' -v
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import plistlib
from pathlib import Path
import signal
import tarfile
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("desktop_spike", Path(__file__).resolve().parents[1] / "postgres-desktop-spike.py")
spike = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(spike)
TRIPLE = "x86_64-unknown-linux-gnu"


def archive_at(path, entries):
    with tarfile.open(path, "w:gz") as tf:
        for name, value in entries:
            info = tarfile.TarInfo(name)
            info.mode = 0o755
            if isinstance(value, tuple):
                info.type, info.linkname = value
                tf.addfile(info)
            else:
                info.size = len(value)
                tf.addfile(info, io.BytesIO(value))


def fake_tree(root):
    for n in spike.PROGRAMS:
        p = root / "bin" / n
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(b"fake executable")
    for name, value in [("share/extension/pg_trgm.control", b"module_pathname = '$libdir/pg_trgm'"),
                        ("share/extension/pg_trgm--1.6.sql", b"-- SQL"),
                        ("lib/postgresql/pg_trgm.so", b"fake module"),
                        ("lib/libpq.so.5", b"fake dependency"),
                        ("include/libpq-fe.h", b"drop headers"),
                        ("bin/pgbench", b"drop extra tool"), ("COPYRIGHT", b"keep notice")]:
        p = root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(value)


class FakeSocket:
    def __enter__(self):
        return self

    def __exit__(self, *args):
        pass

    def bind(self, address):
        self.address = address

    def getsockname(self):
        return ("127.0.0.1", 45432)


class KitTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pg-spike-unit-", dir="/tmp" if os.name != "nt" else None)
        self.addCleanup(self.tmp.cleanup)
        self.work = Path(self.tmp.name)

    def make(self):
        return spike.Spike(self.work, TRIPLE, self.work / "result.json")

    def test_safe_archive_and_discovery(self):
        archive = self.work / "archive.tar.gz"
        archive_at(archive, [("pg/bin/postgres", b"fake"), ("pg/lib/libpq.so", b"fake")])
        spike.extract_safe(archive, self.work / "unpacked")
        self.assertEqual(spike.find_bundle(self.work / "unpacked", TRIPLE), self.work / "unpacked/pg")
        if os.name != "nt":
            self.assertTrue(os.access(self.work / "unpacked/pg/bin/postgres", os.X_OK))

    def test_reject_archive_traversal_special_and_links(self):
        bad = [("../escape", b"x"), ("/absolute", b"x"), ("C:/escape", b"x"),
               ("a\\..\\escape", b"x"), ("a:stream", b"x"),
               ("pg/link", (tarfile.SYMTYPE, "../../escape")),
               ("pg/link", (tarfile.LNKTYPE, "/etc/passwd")),
               ("pipe", (tarfile.FIFOTYPE, ""))]
        for i, item in enumerate(bad):
            with self.subTest(item=item):
                archive = self.work / f"bad-{i}.tar.gz"
                archive_at(archive, [item])
                with self.assertRaises((ValueError, tarfile.FilterError)):
                    spike.extract_safe(archive, self.work / f"out-{i}")

    def test_duplicate_members_rejected(self):
        archive = self.work / "dup.tar.gz"
        archive_at(archive, [("x", b"one"), ("x", b"two")])
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            spike.extract_safe(archive, self.work / "out")

    def test_inventory_and_trimmed_copy(self):
        root = self.work / "full"
        fake_tree(root)
        test_tool = root / "lib/pgxs/src/test/regress/pg_regress"
        test_tool.parent.mkdir(parents=True)
        test_tool.write_bytes(b"build-only")
        data = spike.inventory(root, TRIPLE)
        self.assertTrue(all(data["pg_trgm"].values()))
        self.assertLess(data["runtime_bytes"], data["full_bytes"])
        self.assertIn("include/libpq-fe.h", data["drop_files"])
        self.assertIn("bin/pgbench", data["drop_files"])
        self.assertIn("lib/pgxs/src/test/regress/pg_regress", data["drop_files"])
        self.assertEqual([e["name"] for e in data["extensions"]], ["pg_trgm"])
        spike.copy_runtime(root, data["runtime_files"], self.work / "copy")
        self.assertEqual(spike.tree_size(self.work / "copy"), data["runtime_bytes"])
        self.assertTrue((self.work / "copy/lib/libpq.so.5").exists())

    @unittest.skipIf(os.name == "nt", "Creating symlinks may need developer mode on Windows")
    def test_runtime_symlink_closure_and_escape(self):
        root = self.work / "full"
        fake_tree(root)
        (root / "lib/libpq.so").symlink_to("libpq.so.5")
        data = spike.inventory(root, TRIPLE)
        spike.copy_runtime(root, data["runtime_files"], self.work / "copy")
        self.assertTrue((self.work / "copy/lib/libpq.so").exists())
        (root / "lib/escape.so").symlink_to(self.work / "result.json")
        with self.assertRaisesRegex(RuntimeError, "escapes"):
            spike.inventory(root, TRIPLE)

    def test_download_checksum_with_in_memory_responses(self):
        archive = self.work / "fixture.tar.gz"
        archive_at(archive, [("pg/bin/postgres", b"fake")])
        raw = archive.read_bytes()
        digest = hashlib.sha256(raw).hexdigest()
        s = self.make()
        with patch.dict(spike.ASSETS, {TRIPLE: (len(raw), digest)}), patch.object(spike.urllib.request, "urlopen", side_effect=[io.BytesIO(raw), io.BytesIO((digest + "  archive.tar.gz\n").encode())]):
            details = {}
            s.download(details)
        self.assertEqual(details["sha256"], digest)
        self.assertEqual(s.bundle, self.work / "unpacked/pg")

    def test_checksum_mismatch_never_extracts(self):
        s = self.make()
        with patch.dict(spike.ASSETS, {TRIPLE: (4, None)}), patch.object(spike.urllib.request, "urlopen", side_effect=[io.BytesIO(b"fake"), io.BytesIO(b"0" * 64)]):
            with self.assertRaisesRegex(RuntimeError, "mismatch"):
                s.download({})
        self.assertIsNone(s.bundle)
        self.assertFalse((self.work / "unpacked").exists())

    def test_both_downloads_attempted_on_failure(self):
        s = self.make()
        with patch.object(spike.urllib.request, "urlopen", side_effect=OSError("offline")) as mock:
            with self.assertRaisesRegex(RuntimeError, "offline"):
                s.download({})
        self.assertEqual(mock.call_count, 2)

    def test_command_env_and_redaction_with_fake_process(self):
        with patch.dict(os.environ, {"PGHOST": "foreign", "PGPASSWORD": "foreign-password", "DYLD_LIBRARY_PATH": "foreign"}):
            s = self.make()
        if os.name != "nt":
            self.assertEqual((self.work / "empty.pgpass").stat().st_mode & 0o777, 0o600)
        captured = []
        class FakeProcess:
            def __init__(self, argv, **kw):
                captured.append(kw["env"])
                kw["stdout"].write(("value=" + s.password).encode())
            def wait(self, timeout=None):
                return 0
        with patch.object(spike.subprocess, "Popen", FakeProcess):
            s.run(["fake"], password=True)
            s.run(["fake"], password=False)
        self.assertEqual(captured[0]["PGPASSWORD"], s.password)
        self.assertNotIn("PGPASSWORD", captured[1])
        self.assertNotIn("PGHOST", captured[1])
        self.assertNotIn("DYLD_LIBRARY_PATH", captured[1])
        s.save()
        self.assertNotIn(s.password, (self.work / "result.json").read_text())
        for p in self.work.glob("command-*"):
            self.assertNotIn(s.password, p.read_text())

    def test_fake_full_orchestration_and_failure_continuation(self):
        s = self.make()
        root = self.work / "fixture"
        fake_tree(root)
        calls = []
        def download(details):
            s.bundle = root
        def init(c, details):
            calls.append("init")
            c["initialized"] = True
        def fn(name, fail=False):
            def call(c, details):
                calls.append(name)
                if fail:
                    raise RuntimeError("injected failure " + s.password)
            return call
        with patch.object(spike, "host_triple", return_value=TRIPLE), patch.object(s, "download", side_effect=download), patch.object(s, "linkage"), patch.object(s, "init", side_effect=init), patch.object(s, "start", side_effect=fn("start")), patch.object(s, "queries", side_effect=fn("queries", True)), patch.object(s, "cycle", side_effect=fn("cycle")), patch.object(s, "crash", side_effect=fn("crash")), patch.object(s, "backup", side_effect=fn("backup")), patch.object(s, "signed"), patch.object(s, "cleanup") as cleanup, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(s.execute(), 1)
        self.assertEqual(calls, ["init", "start", "queries", "cycle", "crash", "backup"])
        cleanup.assert_called_once()
        result = json.loads((self.work / "result.json").read_text())
        self.assertEqual([r["step"] for r in result["steps"]], list(range(1, 12)))
        self.assertEqual(result["steps"][5]["status"], "fail")
        self.assertEqual(result["steps"][8]["status"], "pass")
        self.assertNotIn(s.password, (self.work / "result.json").read_text())

    def test_ctrl_c_still_runs_cleanup(self):
        s = self.make()
        with patch.object(s, "download", side_effect=KeyboardInterrupt), patch.object(s, "cleanup") as cleanup, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(s.execute(), 130)
        cleanup.assert_called_once()
        self.assertTrue(s.result["interrupted"])
        self.assertEqual(s.result["steps"][-1]["step"], 11)

    def test_empty_dir_guard_output_guard_and_force(self):
        (self.work / "preserved.txt").write_text("keep")
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
            spike.main(["--work-dir", str(self.work)])
        self.assertEqual(raised.exception.code, 2)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            spike.main(["--work-dir", str(self.work), "--force", "--json-out", str(self.work.parent / "outside.json")])
        with patch.object(spike.Spike, "execute", return_value=0), patch.object(spike, "host_triple", return_value=TRIPLE):
            self.assertEqual(spike.main(["--work-dir", str(self.work), "--force", "--json-out", "reports/test.json"]), 0)
        self.assertEqual((self.work / "preserved.txt").read_text(), "keep")
        self.assertEqual(len(list(self.work.glob("run-*"))), 1)

    def runtime_scenario(self, crash_rows="1|just before kill one\n2|just before kill two", bad_second_start=False, search_fails=False, lost_rows=False, probe_timeout=False, probe_error=False):
        s = self.make()
        root = self.work / "runtime"
        fake_tree(root)
        state = {"running": False, "pid": 9000}
        calls = []
        def run(args, check=True, password=False, timeout=180, input_bytes=None, probe_cluster=None):
            args = [str(a) for a in args]
            calls.append(args)
            name = Path(args[0]).stem
            sql = input_bytes.decode("utf-8") if input_bytes is not None else ""
            text, error, rc = "", "", 0
            if name == "postgres" and "--version" in args:
                text = "postgres (PostgreSQL) 18.6\n"
            elif name == "postgres":
                rc, error = 1, 'FATAL: lock file "postmaster.pid" already exists'
            elif name == "initdb":
                pw = Path(next(a.split("=", 1)[1] for a in args if a.startswith("--pwfile=")))
                self.assertEqual(pw.read_text().strip(), s.password)
                data = Path(args[args.index("-D") + 1])
                data.mkdir()
                (data / "postgresql.conf").write_text("")
            elif name == "pg_ctl":
                data = Path(args[args.index("-D") + 1])
                if "start" in args:
                    if state["running"]:
                        if probe_error:
                            raise OSError("injected duplicate-start observation failure")
                        if probe_timeout:
                            raise spike.subprocess.TimeoutExpired(args, timeout)
                        rc, error = 1, "unrelated permission error" if bad_second_start else "another server might be running; lock file already exists"
                    else:
                        state["running"] = True
                        state["pid"] += 1
                        (data / "postmaster.pid").write_text(str(state["pid"]) + "\n" + str(data.resolve()) + "\n")
                elif "stop" in args:
                    state["running"] = False
                    (data / "postmaster.pid").unlink()
            elif name == "psql":
                if not password:
                    rc, error = 2, "fe_sendauth: no password supplied"
                elif "SELECT id, value FROM spike_persistence" in sql:
                    text = "1|before crash one\n" if lost_rows and state["pid"] >= 9003 and "spike_restored" not in args else "1|before crash one\n2|before crash two\n"
                elif "SELECT id, value FROM spike_crash_write" in sql:
                    text = crash_rows + "\n"
                elif search_fails and "CREATE EXTENSION" in sql:
                    rc, error = 1, "module load failure"
                elif "show_trgm" in sql:
                    text = "{fake}|t|t\n" if "Ōkami" in sql else "{fake}|t\n"
                elif "string_agg" in sql:
                    text = "B,Z,_,a,é\n"
                elif "SELECT" in sql:
                    text = "1\n"
            elif name == "pg_dump":
                Path(args[-1]).write_bytes(b"fake dump")
            rec = {"returncode": rc, "stdout": text, "stderr": error, "seconds": 0.01, "argv": args}
            if check and rc:
                raise RuntimeError(error)
            return rec
        def identity(pid):
            if not state["running"] or pid != state["pid"]:
                raise ProcessLookupError("not running")
            return (str((root / "bin/postgres").resolve()), str(pid))
        def kill(pid, sig):
            self.assertEqual(pid, state["pid"])
            state["running"] = False
        with patch.object(spike, "host_triple", return_value=TRIPLE), patch.object(s, "run", side_effect=run), patch.object(s, "identity", side_effect=identity), patch.object(s, "memory", return_value={"test": "fake"}), patch.object(spike.socket, "socket", return_value=FakeSocket()), patch.object(spike.os, "kill", side_effect=kill), patch.object(spike.time, "sleep"):
            c = s.new_cluster(root, "fake")
            s.init(c, {})
            started = {}
            s.start(c, started)
            self.assertEqual(started["unix_socket"]["status"], "pass")
            if search_fails:
                with self.assertRaisesRegex(RuntimeError, "module load failure"):
                    s.queries(c, {})
            else:
                s.queries(c, {})
            s.cycle(c, {})
            crashed = {}
            if crash_rows == "1|just before kill one" or lost_rows:
                with self.assertRaises(RuntimeError):
                    s.crash(c, crashed)
            else:
                s.crash(c, crashed)
            self.assertIsNotNone(crashed["stale_pid_before_restart"])
            dumped = {}
            s.backup(c, dumped)
            self.assertEqual(dumped["dump_bytes"], 9)
            s.cleanup({})
        self.assertFalse(state["running"])
        self.assertFalse(list(self.work.glob("*-password.tmp")))
        config = (c["data"] / "postgresql.conf").read_text()
        self.assertIn("listen_addresses = '127.0.0.1'", config)
        self.assertIn("shared_buffers = '64MB'", config)
        self.assertIn("max_connections = 20", config)
        self.assertTrue(all(Path(a[0]).is_absolute() for a in calls))
        return crashed

    def test_runtime_steps_with_fake_commands_and_socket(self):
        crashed = self.runtime_scenario()
        self.assertTrue(crashed["recovery_succeeded"])
        self.assertEqual(crashed["crash_write"]["outcome"], "fully-present")
        self.assertTrue(crashed["second_start_refused"])

    def test_crash_transaction_fully_absent_is_valid(self):
        crashed = self.runtime_scenario(crash_rows="")
        self.assertTrue(crashed["recovery_succeeded"])
        self.assertEqual(crashed["crash_write"]["outcome"], "fully-absent")

    def test_crash_partial_transaction_is_failure(self):
        crashed = self.runtime_scenario(crash_rows="1|just before kill one")
        self.assertFalse(crashed["recovery_succeeded"])
        self.assertEqual(crashed["crash_write"]["outcome"], "partial-or-corrupt")
        self.assertTrue(crashed["second_start_refused"])

    def test_crash_ready_without_all_pre_crash_rows_is_failure(self):
        crashed = self.runtime_scenario(lost_rows=True)
        self.assertTrue(crashed["recovery"]["tcp"]["server_ready"])
        self.assertFalse(crashed["recovery_succeeded"])
        self.assertIn("Preserved row contents differ", crashed["recovery_check"]["error"])

    def test_second_start_unrelated_error_does_not_claim_refusal_or_change_recovery(self):
        crashed = self.runtime_scenario(bad_second_start=True)
        self.assertTrue(crashed["recovery_succeeded"])
        self.assertFalse(crashed["second_start_refused"])

    def test_search_failure_does_not_block_lifecycle_or_dump(self):
        crashed = self.runtime_scenario(search_fails=True)
        self.assertTrue(crashed["recovery_succeeded"])

    def test_trigram_failure_does_not_skip_fulltext_or_authentication(self):
        s = self.make()
        def query(c, sql, **kwargs):
            if "pg_trgm" in sql or "gin_trgm_ops" in sql:
                raise RuntimeError("pg_trgm unavailable")
            if kwargs.get("password") is False:
                return {"returncode": 2, "stdout": "", "stderr": "no password supplied"}
            return {"returncode": 0, "stdout": "1\n", "stderr": ""}
        with patch.object(s, "psql", side_effect=query):
            details = {}
            with self.assertRaisesRegex(RuntimeError, "pg_trgm unavailable"):
                s.queries({}, details)
        self.assertEqual(details["similarity"]["status"], "fail")
        self.assertEqual(details["fulltext"]["status"], "pass")
        self.assertEqual(details["authentication"]["status"], "pass")

    def locale_scenario(self, modes, raise_check=False):
        s = self.make()
        active, calls = {}, []
        def query(c, sql, database="postgres", **kwargs):
            calls.append((database, sql))
            text, error, code = "", "", 0
            if sql.startswith("CREATE DATABASE"):
                name = sql.split()[2]
                locale = sql.split("LC_CTYPE '")[1][:-1]
                mode = modes.get(locale, "create-fail")
                if mode == "create-fail":
                    code, error = 1, "ERROR: invalid locale\nexact second line\n"
                else:
                    active[name] = mode
            elif sql.startswith("DROP DATABASE"):
                del active[sql.split()[2]]
            elif "u30eau30bcu30ed" in sql:
                text = "{ascii-key}"
            elif "tsvector_to_array" in sql:
                text = "t"
            elif "to_tsvector" in sql:
                text = "'ōkami':3 'から':2 'リゼロ':1"
            elif "show_trgm" in sql:
                if raise_check and database != "postgres":
                    raise RuntimeError("sample check raised")
                valid = active.get(database) == "working"
                text = ("{hash}|t" if valid else "{}|f") + ("|t" if valid else "|f") if "Ōkami" in sql else ("{hash}|t" if valid else "{}|f")
            elif "string_agg" in sql:
                text = "B,Z,_,a,é"
            elif "lower(" in sql:
                text = "äb ōkami"
            elif "~ '" in sql:
                text = "t"
            elif sql == "SELECT datctype FROM pg_database WHERE datname = current_database()":
                text = "C"
            elif "datlocprovider" in sql:
                text = "c||C|C"
            elif kwargs.get("password") is False:
                code, error = 2, "no password supplied"
            else:
                text = "1"
            return {"returncode": code, "stdout": text, "stderr": error, "seconds": 0}
        return s, active, calls, query

    def test_lc_ctype_create_failure_empty_and_first_working(self):
        s, active, calls, query = self.locale_scenario({"C.utf8": "empty", "en_US.UTF-8": "working"})
        with patch.object(s, "psql", side_effect=query), patch.object(s, "locale_initdb") as init:
            details = {}
            s.queries({}, details)
        probe = details["lc_ctype_probe"]
        self.assertEqual(probe["status"], "pass")
        data = probe["details"]
        self.assertEqual(data["first_working_lc_ctype"], "en_US.UTF-8")
        self.assertEqual([r["name"] for r in data["candidates"]], list(spike.LC_CTYPE_CANDIDATES))
        self.assertEqual(data["candidates"][0]["error"], "ERROR: invalid locale\nexact second line\n")
        self.assertFalse(data["candidates"][0]["created"])
        self.assertEqual(data["candidates"][1]["status"], "fail")
        self.assertFalse(data["candidates"][1]["samples"]["latin"]["non_ascii_letter_trigram"])
        self.assertEqual(data["pg_unicode_fast"]["status"], "pass")
        init.assert_called_once()
        self.assertFalse(active)
        created = [sql.split()[2] for db, sql in calls if sql.startswith("CREATE DATABASE")]
        self.assertEqual(len(created), len(set(created)))
        s.result["steps"] = [{"step": 6, "name": "Search", "details": details}]
        self.assertIn('"first_working_lc_ctype": "en_US.UTF-8"', "\n".join(s.diagnostic_lines()))

    def test_lc_ctype_all_fail_is_observation_search_passes(self):
        s, active, calls, query = self.locale_scenario({})
        with patch.object(s, "psql", side_effect=query), patch.object(s, "persist"), contextlib.redirect_stdout(io.StringIO()):
            rec = s.step(6, "Search", lambda d: s.queries({}, d))
        self.assertEqual(rec["status"], "pass")
        self.assertEqual(rec["details"]["lc_ctype_probe"]["status"], "pass")
        self.assertEqual(rec["details"]["fulltext"]["status"], "pass")
        self.assertIn("No measured LC_CTYPE", rec["details"]["lc_ctype_probe"]["details"]["finding"])
        self.assertFalse(any(sql.startswith("DROP DATABASE") for db, sql in calls))

    def test_lc_ctype_extra_initdb_flags_and_password_cleanup(self):
        s = self.make()
        root = self.work / "runtime"
        fake_tree(root)
        def run(args, **kwargs):
            return {"stdout": "postgres (PostgreSQL) 18.6\n", "stderr": "", "returncode": 0, "seconds": 0}
        with patch.object(spike, "host_triple", return_value=TRIPLE), patch.object(s, "run", side_effect=run) as commands:
            details = {}
            s.locale_initdb({"root": root}, "en_US.UTF-8", details)
        args = commands.call_args.args[0]
        self.assertIn("--locale-provider=builtin", args)
        self.assertIn("--builtin-locale=C.UTF-8", args)
        self.assertIn("--lc-collate=C", args)
        self.assertIn("--lc-ctype=en_US.UTF-8", args)
        self.assertNotIn("--locale=C", args)
        self.assertFalse(list(self.work.glob("*-password.tmp")))
        self.assertEqual(len(s.clusters), 1)
        self.assertFalse(s.clusters[0]["start_attempted"])

    def test_lc_ctype_windows_extra_initdb_budget_gate(self):
        s = self.make()
        s.triple = "x86_64-pc-windows-msvc"
        s.started_monotonic = 0
        for now, expected in ((900, "skipped"), (800, "pass")):
            with self.subTest(remaining=960 - now):
                s.result["steps"] = []
                data = {"first_working_lc_ctype": "C.UTF-8", "initdb": {"status": "pending"}}
                s.lc_ctype_probe = {"status": "pass", "details": data}
                def matrix(d):
                    d.update({"applicable": True, "outcome": "pass", "trials": []})
                with patch.object(s, "download"), patch.object(s, "inspect_inventory"), patch.object(s, "linkage"), patch.object(s, "windows_matrix", side_effect=matrix), patch.object(s, "signed"), patch.object(s, "cleanup"), patch.object(s, "persist"), patch.object(s, "locale_initdb") as init, patch.object(spike.time, "monotonic", return_value=now), contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(s.execute(), 0)
                self.assertEqual(data["initdb"]["status"], expected)
                self.assertEqual(init.call_count, int(expected == "pass"))
                self.assertIs(s.result["steps"][5]["details"]["lc_ctype_probe"], s.lc_ctype_probe)

    def test_lc_ctype_cleanup_after_check_raises_continues(self):
        s, active, calls, query = self.locale_scenario({"C.UTF-8": "working", "C.utf8": "working"}, raise_check=True)
        with patch.object(s, "psql", side_effect=query):
            details = {}
            s.locale_probe({}, details)
        self.assertFalse(active)
        self.assertEqual(len(details["candidates"]), 7)
        self.assertTrue(all(r["dropped"] for r in details["candidates"] if r["created"]))
        self.assertIn("sample check raised", details["candidates"][0]["error"])

    def test_json_write_failure_does_not_skip_cleanup(self):
        s = self.make()
        with patch.object(s, "download", side_effect=KeyboardInterrupt), patch.object(s, "save", side_effect=OSError("disk full")), patch.object(s, "cleanup") as cleanup, contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(s.execute(), 130)
        cleanup.assert_called_once()
        self.assertTrue(s.result["output_errors"])

    def test_cleanup_log_permission_error_is_diagnostic_and_other_clusters_stop(self):
        s = self.make()
        root = self.work / "runtime"
        fake_tree(root)
        with patch.object(spike, "host_triple", return_value=TRIPLE):
            a = s.new_cluster(root, "first")
            b = s.new_cluster(root, "second")
        for c in (a, b):
            c.update(start_attempted=True, pid=9000, identity=(str(root / "bin/postgres"), "creation"))
            c["log"].write_text("private log")
        original_read = Path.read_bytes
        def read(path):
            if path == b["log"]:
                raise PermissionError("unreadable log")
            return original_read(path)
        stopped = []
        def run(args, **kwargs):
            stopped.append(args[args.index("-D") + 1])
            return {"returncode": 0, "seconds": 0, "stdout": "", "stderr": ""}
        with patch.object(s, "run", side_effect=run), patch.object(s, "identity", side_effect=ProcessLookupError), patch.object(s, "wait_descendants"), patch.object(Path, "read_bytes", read):
            details = {}
            s.cleanup(details)
        self.assertEqual(stopped, [b["data"], a["data"]])
        self.assertTrue(all(c["stopped"] and c["status"] == "pass" for c in details["clusters"]))
        self.assertEqual(details["clusters"][0]["log_capture"]["status"], "fail")
        self.assertIn("PermissionError", details["clusters"][0]["log_capture"]["error"])

    def test_linux_linkage_records_actual_version_tags(self):
        s = self.make()
        root = self.work / "full"
        fake_tree(root)
        (root / "bin/postgres").write_bytes(b"\x7fELFfake")
        s.bundle = root
        def tool(name, *args, **kwargs):
            text = {"ldd": "libc.so.6 => /lib/libc.so.6\n", "readelf": ""}[name]
            if "--version-info" in args:
                text = "Name: GLIBC_2.17  Flags: none\nName: GLIBC_2.34 Flags: none\n"
            if "-d" in args:
                text = "(RUNPATH) Library runpath: [$ORIGIN/../lib]"
            return {"stdout": text, "stderr": "", "returncode": 0}
        with patch.object(spike.platform, "system", return_value="Linux"), patch.object(spike, "host_triple", return_value=TRIPLE), patch.object(s, "tool", side_effect=tool):
            details = {}
            s.linkage(details)
        self.assertEqual(details["highest_glibc_required"], "GLIBC_2.34")
        self.assertIn("RUNPATH", details["files"][0]["rpath_runpath_lines"][0])

    def test_windows_inventory_records_dll_presence_only(self):
        s = self.make()
        s.triple = "x86_64-pc-windows-msvc"
        root = self.work / "full"
        fake_tree(root)
        for name in spike.PROGRAMS:
            (root / "bin" / name).rename(root / "bin" / (name + ".exe"))
        (root / "bin/vcruntime140.dll").write_bytes(b"fake DLL")
        s.bundle = root
        with patch.object(spike.platform, "system", return_value="Windows"), patch.object(spike, "host_triple", return_value=s.triple):
            details = {}
            s.linkage(details)
        self.assertTrue(details["vc_runtime_included"])
        self.assertEqual(details["adjacent_to_executables"], ["bin/vcruntime140.dll"])
        self.assertIn("not prove", details["limitation"])
        self.assertIn("bin/vcruntime140.dll", spike.inventory(root, s.triple)["runtime_files"])

    def test_mac_linkage_separates_excluded_findings_and_runtime_failures(self):
        s = self.make()
        s.triple = "aarch64-apple-darwin"
        root = self.work / "full"
        fake_tree(root)
        test_tool = root / "lib/pgxs/src/test/regress/pg_regress"
        test_tool.parent.mkdir(parents=True)
        test_tool.write_bytes(b"\xcf\xfa\xed\xfeFAKE")
        (root / "bin/postgres").write_bytes(b"\xcf\xfa\xed\xfeFAKE")
        s.bundle = root
        s.runtime_files = spike.inventory(root, s.triple)["runtime_files"]
        bad_runtime = False
        def tool(name, *args, **kwargs):
            text = ""
            if name == "lipo":
                text = "arm64\n"
            elif name == "otool" and args[0] == "-L":
                dep = "/Users/runner/build/lib/libpq.5.dylib" if Path(args[-1]) == test_tool or bad_runtime else "/usr/lib/libSystem.B.dylib"
                text = f"{args[-1]}:\n\t{dep} (compatibility version 1.0.0)\n"
            return {"stdout": text, "stderr": "", "returncode": 0}
        with patch.object(spike.platform, "system", return_value="Darwin"), patch.object(spike, "host_triple", return_value=s.triple), patch.object(s, "tool", side_effect=tool):
            details = {}
            s.linkage(details)
            self.assertEqual(details["runtime_failures"], [])
            self.assertEqual(details["excluded_findings"][0]["file"], "lib/pgxs/src/test/regress/pg_regress")
            self.assertEqual(details["excluded_findings"][0]["finding"], "file must be dropped from the bundle")
            bad_runtime = True
            details = {}
            with self.assertRaisesRegex(RuntimeError, "External Mach-O dependencies"):
                s.linkage(details)
            self.assertEqual(details["runtime_failures"][0]["file"], "bin/postgres")
            self.assertEqual(len(details["excluded_findings"]), 1)

    def test_socket_states_never_fail_tcp_start(self):
        for kind in ("pass", "fail", "skipped-path-too-long", "setup-fail", "memory-fail"):
            with self.subTest(kind=kind):
                work = self.work / kind
                work.mkdir()
                s = spike.Spike(work, TRIPLE, work / "result.json")
                root = work / "runtime"
                fake_tree(root)
                with patch.object(spike, "host_triple", return_value=TRIPLE):
                    c = s.new_cluster(root, "x")
                c["initialized"] = True
                c["data"].mkdir(parents=True)
                if kind == "skipped-path-too-long":
                    c["socket"] = work / ("x" * 150)
                if kind == "setup-fail":
                    c["socket"].write_text("not a directory")
                def psql(c, sql, **kwargs):
                    if kind == "fail" and "host" in kwargs:
                        raise OSError("socket probe failed")
                    return {"stdout": "1\n", "stderr": "", "returncode": 0, "seconds": .01}
                memory = {"error": "ps unavailable"} if kind == "memory-fail" else {"postmaster_rss_bytes": 10}
                with patch.object(spike.platform, "system", return_value="Darwin"), patch.object(s, "owns", side_effect=[False, True]), patch.object(s, "capture", return_value=["ready"]), patch.object(s, "remember_descendants"), patch.object(s, "run", return_value={"seconds": .01}), patch.object(s, "psql", side_effect=psql), patch.object(s, "memory", return_value=memory), patch.object(spike.socket, "socket", return_value=FakeSocket()):
                    details = {}
                    s.start(c, details)
                expected = "fail" if kind == "setup-fail" else "pass" if kind == "memory-fail" else kind
                self.assertEqual(details["unix_socket"]["status"], expected)
                socket_details = details["unix_socket"].get("details", details["unix_socket"])
                self.assertEqual(socket_details["path_bytes"], len(os.fsencode(socket_details["path"])))
                self.assertEqual(socket_details["platform_limit_bytes"], 104)
                self.assertTrue(details["tcp"]["server_ready"])
                if kind == "setup-fail":
                    self.assertIn("unix_socket_directories = ''", (c["data"] / "postgresql.conf").read_text())
                if kind == "memory-fail":
                    self.assertEqual(details["idle_memory"]["status"], "fail")

    def test_old_python_prints_one_actionable_line_before_argparse(self):
        output = io.StringIO()
        with patch.object(spike.sys, "version_info", (3, 9, 6)), patch.object(spike.platform, "python_version", return_value="3.9.6"), contextlib.redirect_stderr(output):
            self.assertEqual(spike.main([]), 2)
        self.assertEqual(len(output.getvalue().splitlines()), 1)
        self.assertIn("Python 3.9.6 found", output.getvalue())
        self.assertIn("python3.12 or newer", output.getvalue())

    def test_summary_rows_metrics_escape_and_redaction(self):
        s = self.make()
        s.summary_md = self.work / "summary.md"
        s.result["steps"] = [{"step": 5, "name": "Start", "status": "pass", "seconds": .14,
                              "details": {"start_seconds": .14, "unix_socket": {"status": "skipped-path-too-long", "seconds": 0, "path_bytes": 140, "platform_limit_bytes": 104},
                                          "idle_memory": {"status": "pass", "seconds": .1, "postmaster_rss_bytes": 14843904},
                                          "tcp": {"status": "pass", "seconds": .1, "server_ready": True},
                                          "optional": {"status": "fail", "seconds": .01, "error": "x|y\n" + s.password}}}]
        s.persist()
        md = s.summary_md.read_text()
        self.assertEqual(len([line for line in md.splitlines() if line.startswith("| ")]), 7)
        self.assertIn("path_bytes=140", md)
        self.assertIn("postmaster_rss_bytes=14843904", md)
        self.assertIn("x\\|y [REDACTED]", md)
        self.assertNotIn(s.password, md)

    def test_summary_output_guards_and_sign_identity_forwarding(self):
        bad = ["result.json", "runtime/summary.md", "signed-a.md", "reference-tree-moved/summary.md", str(self.work.parent / "outside.md"), "reports"]
        for path in bad:
            with self.subTest(path=path), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                spike.main(["--work-dir", str(self.work), "--json-out", "reports/result.json" if path == "reports" else "result.json", "--summary-md", path])
        with patch.object(spike.Spike, "execute", return_value=0), patch.object(spike, "Spike", wraps=spike.Spike) as factory:
            self.assertEqual(spike.main(["--work-dir", str(self.work), "--summary-md", "summary.md", "--sign-identity", "TESTTEAM"]), 0)
        self.assertEqual(factory.call_args.kwargs["summary_md"], self.work.resolve() / "summary.md")
        self.assertEqual(factory.call_args.kwargs["sign_identity"], "TESTTEAM")

    def signed_scenario(self, identity="Developer ID Application: Spike", unavailable=False, bad_adhoc=False, bad_module=False, missing_team=False, sign_exception=False):
        s = self.make()
        s.sign_identity = identity
        s.triple = "aarch64-apple-darwin"
        root = self.work / "full"
        fake_tree(root)
        for name in ("bin/postgres", "lib/postgresql/pg_trgm.so"):
            (root / name).write_bytes(b"\xcf\xfa\xed\xfeFAKE")
        s.bundle = root
        s.runtime = root
        s.runtime_files = spike.inventory(root, s.triple)["runtime_files"]
        calls = []
        def tool(name, *args, **kwargs):
            calls.append((name, args))
            self.assertTrue(spike.contained(Path(args[-1]), s.work))
            if sign_exception and "--sign" in args and identity in args:
                raise TimeoutError("keychain authorization timed out")
            if unavailable and "--sign" in args and identity in args:
                return {"stdout": "", "stderr": "User denied keychain access", "returncode": 1}
            return {"stdout": "", "stderr": "TeamIdentifier=SPIKETEAM\n" if "c-team" in str(args[-1]) and not missing_team else "TeamIdentifier=not set\n", "returncode": 0}
        def init(c, d):
            if "a-adhoc " in str(c["root"]):
                raise RuntimeError("unexpected executable memory error" if bad_adhoc else "Library not loaded: @loader_path/../lib/libssl.3.dylib\ncode signature: mapping process and mapped file (non-platform) have different Team IDs")
            c["initialized"] = True
        def start(c, d):
            c["running"] = True
        def queries(c, d):
            d["pg_trgm_loaded"] = not bad_module
            if bad_module:
                raise RuntimeError("pg_trgm dlopen failed")
        def stop(c, d):
            c["running"] = False
        with patch.object(spike.platform, "system", return_value="Darwin"), patch.object(spike, "host_triple", return_value=s.triple), patch.object(s, "tool", side_effect=tool), patch.object(s, "init", side_effect=init), patch.object(s, "start", side_effect=start), patch.object(s, "owns", side_effect=lambda c: c.get("running", False)), patch.object(s, "queries", side_effect=queries), patch.object(s, "cycle"), patch.object(s, "crash"), patch.object(s, "stop", side_effect=stop):
            details = {}
            if bad_adhoc or bad_module or missing_team:
                with self.assertRaises(RuntimeError):
                    s.signed(details)
            else:
                s.signed(details)
        return details, calls

    def test_mac_signed_variants_inner_first_entitlements_and_crash_sequence(self):
        details, calls = self.signed_scenario()
        a, b, c = details["variants"]
        self.assertEqual([v["status"] for v in (a, b, c)], ["expected-failure", "pass", "pass"])
        self.assertIn("different Team IDs", a["error"])
        self.assertEqual(c["team_id"], "SPIKETEAM")
        self.assertTrue(b["pg_trgm_loaded"] and c["pg_trgm_loaded"])
        self.assertIsNone(a["pg_trgm_loaded"])
        self.assertEqual([r["name"] for r in b["repeated_steps"]], ["initdb", "start", "queries/auth", "stop/restart", "crash-recovery", "final-stop"])
        self.assertTrue(all(r["status"] == "skipped-prerequisite" for r in a["repeated_steps"][1:]))
        signcalls = [args for toolname, args in calls if "--force" in args]
        self.assertEqual(len(signcalls), 6)
        for v in (a, b, c):
            self.assertEqual([r["file"] for r in v["signatures"]], ["lib/postgresql/pg_trgm.so", "bin/postgres"])
            self.assertTrue(all("display" in r for r in v["signatures"]))
        entitled = [args for args in signcalls if "--entitlements" in args]
        self.assertEqual(len(entitled), 1)
        self.assertEqual(Path(entitled[0][-1]).name, "postgres")
        entitlements_path = Path(entitled[0][entitled[0].index("--entitlements") + 1])
        self.assertEqual(plistlib.loads(entitlements_path.read_bytes()), {"com.apple.security.cs.disable-library-validation": True})

    def test_mac_team_identity_optional(self):
        details, calls = self.signed_scenario(identity=None)
        self.assertEqual(details["variants"][2]["status"], "skipped-no-identity")
        self.assertEqual(len([a for _, a in calls if "--sign" in a]), 4)

    def test_mac_locked_identity_recorded_without_step_failure(self):
        details, _ = self.signed_scenario(unavailable=True)
        self.assertEqual(details["variants"][2]["status"], "skipped-signing-unavailable")
        self.assertIn("denied keychain", details["variants"][2]["error"])
        self.assertEqual(details["variants"][2]["repeated_steps"], [])

    def test_mac_team_variant_requires_real_team_id(self):
        details, _ = self.signed_scenario(missing_team=True)
        self.assertEqual(details["variants"][2]["status"], "fail")
        self.assertIn("same nonempty Team ID", details["variants"][2]["error"])

    def test_mac_identity_timeout_recorded_without_step_failure(self):
        details, _ = self.signed_scenario(sign_exception=True)
        self.assertEqual(details["variants"][2]["status"], "skipped-signing-unavailable")
        self.assertIn("keychain authorization timed out", details["variants"][2]["error"])

    def test_mac_unexpected_adhoc_failure_does_not_hide_other_variants(self):
        details, _ = self.signed_scenario(bad_adhoc=True)
        self.assertEqual([v["status"] for v in details["variants"]], ["fail", "pass", "pass"])

    def test_mac_extension_failure_does_not_skip_crash_or_stop(self):
        details, _ = self.signed_scenario(bad_module=True)
        b = details["variants"][1]
        self.assertFalse(b["pg_trgm_loaded"])
        self.assertEqual(b["repeated_steps"][-2]["status"], "pass")
        self.assertEqual(b["repeated_steps"][-1]["status"], "pass")

    def test_pid_identity_guard_no_foreign_kill(self):
        s = self.make()
        c = {"pid": 456, "identity": ("private", "old-start")}
        with patch.object(s, "identity", return_value=("foreign", "new-start")), patch.object(spike.os, "kill") as kill:
            with self.assertRaisesRegex(RuntimeError, "Refusing"):
                s.hard_kill(c)
        kill.assert_not_called()

    def test_interpreter_trim_every_target_and_missing_controls(self):
        for triple in spike.ASSETS:
            with self.subTest(triple=triple):
                root = self.work / triple
                fake_tree(root)
                if "windows" in triple:
                    for name in spike.PROGRAMS:
                        (root / "bin" / name).rename(root / "bin" / (name + ".exe"))
                suffix = ".dll" if "windows" in triple else ".dylib" if "darwin" in triple else ".so"
                for name in ("plpython3", "plpython3u", "plperl", "plperlu", "pltcl", "pltclu",
                             "hstore_plpython3", "jsonb_plpython3", "ltree_plpython3",
                             "hstore_plperl", "jsonb_plperl", "bool_plperl", "hstore_pltcl"):
                    (root / "lib" / (name + suffix)).write_bytes(b"module")
                    (root / "share/extension" / (name + ".control")).write_text("module_pathname = '$libdir/" + name + "'")
                    (root / "share/extension" / (name + "--1.0.sql")).write_text("-- SQL")
                (root / "share/extension/absent.control").write_text("module_pathname = '$libdir/absent'")
                (root / "share/extension/absent--1.0.sql").write_text("-- SQL")
                data = spike.inventory(root, triple)
                self.assertEqual(len(data["excluded_findings"]), 41)
                self.assertTrue(all(n in data["drop_files"] for n in [r["file"] for r in data["excluded_findings"]]))
                self.assertIn("lib/postgresql/pg_trgm.so", data["runtime_files"])
                self.assertIn("lib/libpq.so.5", data["runtime_files"])
                missing = next(r for r in data["control_module_checks"] if r["control"].endswith("absent.control"))
                self.assertEqual(missing["status"], "excluded-missing-module")
                self.assertEqual(missing["modules"], [])

    def test_linux_excluded_missing_interpreter_is_evidence_not_failure(self):
        s = self.make()
        root = self.work / "full"
        fake_tree(root)
        (root / "bin/postgres").write_bytes(b"\x7fELFfake")
        (root / "lib/hstore_plpython3.so").write_bytes(b"\x7fELFfake")
        s.bundle = root
        s.runtime_files = spike.inventory(root, TRIPLE)["runtime_files"]
        def tool(name, *args, **kwargs):
            missing = "\tlibpython3.11.so.1.0 => not found\n" if name == "ldd" and str(args[-1]).endswith("hstore_plpython3.so") else ""
            return {"stdout": missing, "stderr": "", "returncode": 0}
        with patch.object(spike.platform, "system", return_value="Linux"), patch.object(spike, "host_triple", return_value=TRIPLE), patch.object(s, "tool", side_effect=tool):
            details = {}
            s.linkage(details)
        self.assertEqual(details["runtime_failures"], [])
        finding = details["excluded_findings"][0]
        self.assertIn("interpreter", finding["reason"])
        self.assertIn("libpython3.11", finding["missing_libraries"][0])

    def test_controls_without_module_declaration_are_retained(self):
        root = self.work / "full"
        fake_tree(root)
        (root / "share/extension/sql_only.control").write_text("default_version = '1.0'")
        data = spike.inventory(root, TRIPLE)
        self.assertIn("share/extension/sql_only.control", data["runtime_files"])
        self.assertIn("no-module-declared", [r["status"] for r in data["control_module_checks"]])

    def test_unloadable_optional_control_is_dropped_required_module_fails(self):
        s = self.make()
        root = self.work / "runtime"
        fake_tree(root)
        (root / "lib/broken.so").write_bytes(b"fake")
        (root / "share/extension/broken.control").write_text("module_pathname = '$libdir/broken'")
        (root / "share/extension/broken--1.sql").write_text("--SQL")
        s.runtime = root
        selection = spike.inventory(root, TRIPLE)
        s.runtime_files, s.control_checks = selection["runtime_files"], selection["control_module_checks"]
        s.result["steps"] = [{"step": 2, "details": selection}, {"step": 3, "details": {}}]
        def psql(c, sql, **kwargs):
            if "broken" in sql:
                return {"returncode": 1, "stdout": "", "stderr": "could not load library: missing dependency"}
            return {"returncode": 0, "stdout": "", "stderr": ""}
        details = {}
        with patch.object(s, "psql", side_effect=psql):
            s.retained_module_loads({"root": root}, details)
        self.assertFalse((root / "share/extension/broken.control").exists())
        self.assertIn("lib/broken.so", s.runtime_files)  # No speculative library trim.
        self.assertIn("share/extension/broken.control", selection["drop_files"])
        self.assertEqual(len(details["excluded_findings"]), 2)
        with patch.object(s, "psql", side_effect=RuntimeError("pg_trgm cannot load")), self.assertRaisesRegex(RuntimeError, "pg_trgm cannot load"):
            s.retained_module_loads({}, {})
        self.assertTrue((root / "share/extension/pg_trgm.control").exists())

    def fake_kernel(self):
        class Fn:
            def __init__(self, result=1):
                self.result, self.calls = result, []
            def __call__(self, *args):
                self.calls.append(args)
                return self.result(*args) if callable(self.result) else self.result
        class Kernel:
            pass
        k = Kernel()
        for name in ("GetACP", "GetOEMCP", "GetConsoleCP", "GetConsoleOutputCP", "GetShortPathNameW",
                     "GetFileAttributesW", "RemoveDirectoryW", "OpenProcess", "QueryFullProcessImageNameW",
                     "GetProcessTimes", "GetExitCodeProcess", "CloseHandle", "GetCurrentProcess", "CreateDirectoryW",
                     "CreateFileW", "DeviceIoControl", "CreateToolhelp32Snapshot", "Process32FirstW", "Process32NextW",
                     "TerminateProcess", "WaitForSingleObject", "OpenProcessToken", "GetTokenInformation", "ImpersonateLoggedOnUser", "RevertToSelf"):
            setattr(k, name, Fn())
        return k

    def test_win32_signatures_and_last_error_loader(self):
        from ctypes import wintypes as w
        k = self.fake_kernel()
        with patch.object(spike.ctypes, "WinDLL", return_value=k, create=True) as loader:
            self.assertIs(spike.windows_kernel32(), k)
        self.assertEqual([c.args[0] for c in loader.call_args_list], ["kernel32", "advapi32"])
        self.assertTrue(all(c.kwargs == {"use_last_error": True} for c in loader.call_args_list))
        expected = {
            "GetACP": ([], w.UINT), "GetOEMCP": ([], w.UINT),
            "GetConsoleCP": ([], w.UINT), "GetConsoleOutputCP": ([], w.UINT),
            "GetShortPathNameW": ([w.LPCWSTR, w.LPWSTR, w.DWORD], w.DWORD),
            "GetFileAttributesW": ([w.LPCWSTR], w.DWORD), "RemoveDirectoryW": ([w.LPCWSTR], w.BOOL),
            "OpenProcess": ([w.DWORD, w.BOOL, w.DWORD], w.HANDLE),
            "CreateToolhelp32Snapshot": ([w.DWORD, w.DWORD], w.HANDLE),
            "Process32FirstW": ([w.HANDLE, spike.ctypes.POINTER(spike.WINDOWS_PROCESS_ENTRY)], w.BOOL),
            "Process32NextW": ([w.HANDLE, spike.ctypes.POINTER(spike.WINDOWS_PROCESS_ENTRY)], w.BOOL),
            "TerminateProcess": ([w.HANDLE, w.UINT], w.BOOL),
            "WaitForSingleObject": ([w.HANDLE, w.DWORD], w.DWORD),
            "QueryFullProcessImageNameW": ([w.HANDLE, w.DWORD, w.LPWSTR, spike.ctypes.POINTER(w.DWORD)], w.BOOL),
            "GetProcessTimes": ([w.HANDLE] + [spike.ctypes.POINTER(w.FILETIME)] * 4, w.BOOL),
            "GetExitCodeProcess": ([w.HANDLE, spike.ctypes.POINTER(w.DWORD)], w.BOOL),
            "GetCurrentProcess": ([], w.HANDLE),
            "CreateDirectoryW": ([w.LPCWSTR, w.LPVOID], w.BOOL),
            "CreateFileW": ([w.LPCWSTR, w.DWORD, w.DWORD, w.LPVOID, w.DWORD, w.DWORD, w.HANDLE], w.HANDLE),
            "DeviceIoControl": ([w.HANDLE, w.DWORD, w.LPVOID, w.DWORD, w.LPVOID, w.DWORD, spike.ctypes.POINTER(w.DWORD), w.LPVOID], w.BOOL),
            "OpenProcessToken": ([w.HANDLE, w.DWORD, spike.ctypes.POINTER(w.HANDLE)], w.BOOL),
            "GetTokenInformation": ([w.HANDLE, spike.ctypes.c_int, w.LPVOID, w.DWORD, spike.ctypes.POINTER(w.DWORD)], w.BOOL),
            "ImpersonateLoggedOnUser": ([w.HANDLE], w.BOOL),
            "RevertToSelf": ([], w.BOOL),
            "CloseHandle": ([w.HANDLE], w.BOOL),
        }
        for name, (args, result) in expected.items():
            self.assertEqual(getattr(k, name).argtypes, args, name)
            self.assertIs(getattr(k, name).restype, result, name)

    def test_win32_code_pages_utf8_and_no_console(self):
        k = self.fake_kernel()
        k.GetACP.result, k.GetOEMCP.result = 1252, 437
        k.GetConsoleCP.result, k.GetConsoleOutputCP.result = 0, 65001
        result = spike.windows_code_pages(k)
        self.assertFalse(result["process_code_page_is_utf8"])
        self.assertEqual(result["GetConsoleCP"], 0)
        k.GetACP.result = 65001
        self.assertTrue(spike.windows_code_pages(k)["process_code_page_is_utf8"])

    def test_short_path_success_unavailable_failure_and_resize(self):
        k = self.fake_kernel()
        def short(path, buf, size):
            if buf is None:
                return 256
            buf.value = "C:\\PGS\\UNICOD~1"
            return len(buf.value)
        k.GetShortPathNameW.result = short
        self.assertEqual(str(spike.windows_short_path("C:\\漢字", k)), "C:\\PGS\\UNICOD~1")
        def unchanged(path, buf, size):
            if buf is None:
                return 256
            buf.value = path
            return len(path)
        k.GetShortPathNameW.result = unchanged
        self.assertIsNone(spike.windows_short_path("C:\\漢字", k))
        k.GetShortPathNameW.result = 0
        with patch.object(spike, "windows_error", return_value=OSError("exact API failure")), self.assertRaisesRegex(OSError, "exact API failure"):
            spike.windows_short_path("C:\\漢字", k)
        k.GetShortPathNameW.result = 256
        with self.assertRaisesRegex(RuntimeError, "changed size"):
            spike.windows_short_path("C:\\漢字", k)

    def test_junction_creation_quotes_unicode_and_removal_never_recurses(self):
        import base64
        calls = []
        spike.windows_create_junction(Path("C:/ASCII link"), Path("C:/é漢字/O'Brien"), lambda *args: calls.append(args))
        script = base64.b64decode(calls[0][-1]).decode("utf-16le")
        self.assertIn("-ItemType Junction", script)
        self.assertIn("O''Brien", script)
        self.assertIn("é漢字", script)
        k = self.fake_kernel()
        k.GetFileAttributesW.result = 0x410
        spike.windows_remove_junction("C:/link", k)
        self.assertEqual(k.RemoveDirectoryW.calls, [("C:/link",)])
        k.GetFileAttributesW.result = 0x10
        with self.assertRaisesRegex(RuntimeError, "not a directory reparse point"):
            spike.windows_remove_junction("C:/target", k)
        self.assertEqual(len(k.RemoveDirectoryW.calls), 1)
        k.GetFileAttributesW.result = 0xffffffff
        with patch.object(spike, "windows_error", return_value=OSError("attributes failed")), self.assertRaises(OSError):
            spike.windows_remove_junction("C:/link", k)
        k.GetFileAttributesW.result, k.RemoveDirectoryW.result = 0x410, 0
        with patch.object(spike, "windows_error", return_value=OSError("remove failed")), self.assertRaisesRegex(OSError, "remove failed"):
            spike.windows_remove_junction("C:/link", k)

    def test_windows_identity_handle_width_and_close_on_failure(self):
        k = self.fake_kernel()
        k.OpenProcess.result = 0x123456789abcdef
        def image(handle, flags, buf, size):
            buf.value = str(self.work / "postgres.exe")
            return 1
        def times(handle, *values):
            values[0]._obj.dwHighDateTime, values[0]._obj.dwLowDateTime = 7, 9
            return 1
        k.QueryFullProcessImageNameW.result, k.GetProcessTimes.result = image, times
        self.assertEqual(spike.windows_identity(123, k), (str((self.work / "postgres.exe").resolve()), 7, 9))
        self.assertEqual(k.CloseHandle.calls, [(0x123456789abcdef,)])
        for call in ("QueryFullProcessImageNameW", "GetProcessTimes"):
            fn = getattr(k, call)
            previous = fn.result
            fn.result = 0
            with patch.object(spike, "windows_error", return_value=OSError(call)), self.assertRaisesRegex(OSError, call):
                spike.windows_identity(123, k)
            fn.result = previous
        self.assertEqual(len(k.CloseHandle.calls), 3)
        k.OpenProcess.result = 0
        with patch.object(spike, "windows_error", return_value=OSError("OpenProcess")), self.assertRaises(OSError):
            spike.windows_identity(123, k)
        self.assertEqual(len(k.CloseHandle.calls), 3)
        k.OpenProcess.result, k.CloseHandle.result = 123, 0
        with patch.object(spike, "windows_error", return_value=OSError("CloseHandle")), self.assertRaisesRegex(OSError, "CloseHandle"):
            spike.windows_identity(123, k)

    def test_windows_trial_path_contracts_and_helper_isolation(self):
        s = self.make()
        s.triple = "x86_64-pc-windows-msvc"
        s.runtime = self.work / "runtime"
        with patch.object(spike, "host_triple", return_value=s.triple):
            for i, strategy in enumerate(("direct", "relative", "ascii-helpers", "short", "junction"), 1):
                s.command_cwd = self.work
                trial = {"class": "C", "strategy": strategy, "status": "fail"}
                with patch.object(spike, "windows_short_path", return_value=self.work / "SHORT"), patch.object(spike, "windows_create_junction") as junction:
                    c = s.prepare_windows_trial(trial, i)
                self.assertIn("漢字", str(c["data"]))
                self.assertTrue(str(c["command_output_dir"]).isascii())
                self.assertEqual(s.command_output_dir, c["command_output_dir"])
                if strategy == "relative":
                    self.assertEqual(str(c["data_arg"]), "pg18")
                    self.assertEqual(s.command_cwd, c["data"].parent)
                    self.assertEqual(str(c["password_arg"]), "password.tmp")
                elif strategy == "ascii-helpers":
                    self.assertIn("漢字", str(c["data_arg"]))
                    for key in ("password", "pgpass", "log", "dump"):
                        self.assertTrue(str(c[key + "_arg"]).isascii())
                        self.assertNotEqual(c[key + "_arg"].parent, c["data"].parent)
                elif strategy == "junction":
                    junction.assert_called_once()
                    self.assertTrue(str(c["data_arg"]).isascii())
                elif strategy == "short":
                    self.assertIn("SHORT", str(c["data_arg"]))
                self.assertEqual(s.env["PGPASSFILE"], str(c["pgpass_arg"]))
                self.assertTrue(c["pgpass_file"].exists())

    def test_windows_missing_short_path_is_skipped(self):
        s = self.make()
        s.triple = "x86_64-pc-windows-msvc"
        s.runtime = self.work / "runtime"
        trial = {"class": "C", "strategy": "short", "status": "fail"}
        with patch.object(spike, "host_triple", return_value=s.triple), patch.object(spike, "windows_short_path", return_value=None):
            s.prepare_windows_trial(trial, 1)
        self.assertEqual(trial["status"], "skipped")
        self.assertIn("8.3", trial["reason"])

    def matrix_scenario(self, passes=None, setup_fail=False, cleanup_fail=False, budget=False):
        work = self.work / ("matrix-" + spike.secrets.token_hex(4))
        work.mkdir()
        s = spike.Spike(work, TRIPLE, work / "result.json")
        s.runtime = self.work / "runtime"
        passes = passes if passes is not None else {("A", "direct"), ("C", "junction")}
        env_before = s.env.copy()
        calls = []
        def prepare(trial, index):
            c = {"index": index}
            s.clusters.append(c)
            s.env["PGPASSFILE"] = "trial-specific"
            s.command_cwd = self.work / "trial"
            if setup_fail and index == 2:
                raise RuntimeError("setup exact error\nsecond line")
            return c
        def lifecycle(c, trial):
            calls.append(trial["name"])
            trial["stage_reached"] = "initdb"
            if (trial["class"], trial["strategy"]) not in passes:
                raise RuntimeError("initdb exact error\nsecond line")
            trial["stage_reached"] = "complete"
        def cleanup(details, final=True):
            self.assertFalse(final)
            if cleanup_fail and s.clusters[0]["index"] == 2:
                raise RuntimeError("cleanup exact error")
        details = {}
        with patch.object(spike, "windows_code_pages", return_value={"GetACP": 1252}), patch.object(s, "prepare_windows_trial", side_effect=prepare), patch.object(s, "windows_lifecycle", side_effect=lifecycle), patch.object(s, "cleanup", side_effect=cleanup), patch.object(s, "persist"):
            if budget:
                s.started_monotonic -= 950
            if not passes or ("A", "direct") not in passes or not any(c == "C" for c, st in passes) or budget:
                with self.assertRaisesRegex(RuntimeError, "requires a complete class A"):
                    s.windows_matrix(details)
            else:
                s.windows_matrix(details)
        self.assertEqual(s.env, env_before)
        self.assertEqual(s.command_cwd, work)
        self.assertIsNone(s.command_deadline)
        return s, details, calls

    def test_matrix_acceptance_independence_full_errors_cap_and_no_subst(self):
        s, details, calls = self.matrix_scenario(setup_fail=True)
        self.assertEqual(details["outcome"], "pass")
        self.assertEqual(len(details["trials"]), spike.WINDOWS_TRIAL_CAP)
        self.assertEqual(len(calls), 10)
        self.assertEqual(details["trials"][1]["stage_reached"], "setup")
        self.assertEqual(details["trials"][1]["error"], "RuntimeError: setup exact error\nsecond line")
        self.assertTrue(details["class_C_strategy_passed"])
        self.assertEqual(details["subst"]["status"], "skipped")
        self.assertEqual(s.clusters, [])

    def test_matrix_requires_both_ascii_and_cjk_success(self):
        self.matrix_scenario(passes={("A", "direct")})
        self.matrix_scenario(passes={("C", "junction")})
        self.matrix_scenario(passes={("B", "direct")})

    def test_matrix_budget_skips_never_passes(self):
        s, details, calls = self.matrix_scenario(budget=True)
        self.assertEqual(calls, [])
        self.assertTrue(all(t["status"] == "skipped" for t in details["trials"]))
        self.assertFalse(details["class_A_passed"])

    def test_matrix_cleanup_failure_is_isolated_and_tracked(self):
        s, details, calls = self.matrix_scenario(cleanup_fail=True)
        self.assertEqual(len(calls), 11)
        self.assertEqual(details["trials"][1]["cleanup_error"], "cleanup exact error")
        self.assertEqual(s.clusters, [{"index": 2}])

    def test_windows_lifecycle_stops_at_first_failed_stage(self):
        s = self.make()
        for failed in ("init", "queries", "stop", "crash", "backup"):
            calls = []
            def method(name):
                def fn(*args):
                    calls.append(name)
                    if name == failed:
                        raise RuntimeError("failed " + name)
                    if name == "backup":
                        args[1].update({"dump": {"status": "pass"}, "restore": {"status": "pass"}})
                    if name == "windows_observations":
                        args[1]["data_directory"] = str(self.work)
                return fn
            with contextlib.ExitStack() as stack:
                for name in ("init", "start", "windows_observations", "queries", "ensure_fixture", "stop", "crash", "backup"):
                    stack.enter_context(patch.object(s, name, side_effect=method(name)))
                stack.enter_context(patch.object(s, "check_rows", return_value="2"))
                stack.enter_context(patch.object(s, "persist"))
                trial = {"strategy": "direct"}
                with self.assertRaisesRegex(RuntimeError, "failed " + failed):
                    s.windows_lifecycle({}, trial)
            self.assertEqual(calls[-1], failed)
            self.assertEqual(sum(r["status"] == "fail" for r in trial["stages"].values()), 1)

    def test_relative_restart_uses_known_wide_path_and_restores_cwd(self):
        s = self.make()
        parent = self.work / "parent é"
        s.command_cwd = parent
        c = {"data": parent / "pg18", "log": parent / "postgres.log",
             "data_arg": Path("pg18"), "log_arg": Path("postgres.log")}
        reported = str(parent / "pg18").replace("é", "")
        trial = {"stages": {"path-observations": {"details": {"data_directory": reported}}}}
        def start(c, d):
            self.assertEqual(s.command_cwd, self.work)
            self.assertEqual(c["data_arg"], parent / "pg18")
            self.assertEqual(c["log_arg"], parent / "postgres.log")
        with patch.object(s, "stop"), patch.object(s, "start", side_effect=start), patch.object(s, "windows_observations"), patch.object(s, "check_rows", return_value="2"):
            details = {}
            s.windows_different_cwd_restart(c, trial, details)
        self.assertFalse(details["reported_path_matches_real"])
        self.assertEqual(details["reported_data_directory"], reported)
        self.assertIn("é", details["data_arg"])
        self.assertEqual(s.command_cwd, parent)
        self.assertEqual(str(c["data_arg"]), "pg18")
        with patch.object(s, "stop"), patch.object(s, "start", side_effect=RuntimeError("restart failed")), self.assertRaises(RuntimeError):
            s.windows_different_cwd_restart(c, trial, {})
        self.assertEqual(s.command_cwd, parent)
        self.assertEqual(str(c["log_arg"]), "postgres.log")

    def test_trial_command_timeout_and_cwd_are_enforced(self):
        s = self.make()
        captured = []
        class FakeProcess:
            def __init__(self, argv, **kw):
                captured.append(kw["cwd"])
            def wait(self, timeout):
                captured.append(timeout)
                return 0
        s.command_cwd = self.work / "parent 漢字"
        s.command_deadline = spike.time.monotonic() + 1
        with patch.object(spike.subprocess, "Popen", FakeProcess):
            s.run(["fake"], timeout=180)
        self.assertEqual(captured[0], s.command_cwd)
        self.assertGreater(captured[1], 0)
        self.assertLessEqual(captured[1], 1)
        s.command_deadline = 0
        with self.assertRaisesRegex(TimeoutError, "budget exhausted"):
            s.run(["fake"])

    def test_summary_matrix_and_unabridged_errors_even_when_step_passes(self):
        s = self.make()
        s.summary_md = self.work / "summary.md"
        long_error = "first line\n" + "X" * 1000 + "\nlast line " + s.password
        matrix = {"applicable": True, "code_pages": {"GetACP": 1252}, "subst": {"reason": "shared runner"},
                  "trials": [{"name": "C / direct", "class": "C", "strategy": "direct", "status": "fail",
                              "stage_reached": "initdb", "error": long_error}]}
        s.result["windows_path_matrix"] = matrix
        s.result["steps"] = [{"step": 4, "name": "Matrix", "status": "pass", "seconds": 1,
                              "details": {"windows_path_matrix": matrix}}]
        s.save_summary()
        md = s.summary_md.read_text()
        self.assertIn("C | direct | fail | initdb", md)
        self.assertIn("X" * 1000, md)
        self.assertIn("last line [REDACTED]", md)
        self.assertNotIn(s.password, md)


    def test_module_timeout_and_connection_failure_do_not_trim(self):
        s = self.make()
        root = self.work / "runtime"
        fake_tree(root)
        s.runtime = root
        selection = spike.inventory(root, TRIPLE)
        s.runtime_files, s.control_checks = selection["runtime_files"], selection["control_module_checks"]
        # Use an optional name so trimming would be possible for a real LOAD error.
        s.control_checks[0]["control"] = "share/extension/optional.control"
        for result in (TimeoutError("budget exhausted"), {"returncode": 2, "stdout": "", "stderr": "connection lost"}):
            with patch.object(s, "psql", **({"side_effect": result} if isinstance(result, Exception) else {"return_value": result})), self.assertRaises(RuntimeError):
                s.retained_module_loads({"root": root}, {})
        self.assertTrue((root / "share/extension/pg_trgm.control").exists())
        self.assertEqual(s.control_checks[0]["status"], "present")

    def test_windows_api_error_contains_code_operation_and_system_text(self):
        with patch.object(spike.ctypes, "get_last_error", return_value=5, create=True), patch.object(spike.ctypes, "FormatError", return_value="Access is denied.", create=True):
            error = spike.windows_error("OpenProcess")
        self.assertEqual(error.errno, 5)
        self.assertIn("OpenProcess: Access is denied.", str(error))

    def test_lifecycle_success_records_status_dump_restore_and_relative_restart(self):
        s = self.make()
        for strategy in ("direct", "relative"):
            calls = []
            def method(name):
                def fn(c, d=None):
                    calls.append(name)
                    if name == "backup":
                        d.update({"dump": {"status": "pass"}, "restore": {"status": "pass"}})
                    if name == "windows_observations":
                        d["data_directory"] = "/reported/data"
                return fn
            with contextlib.ExitStack() as stack:
                for name in ("init", "start", "windows_observations", "queries", "ensure_fixture", "stop", "crash", "backup"):
                    stack.enter_context(patch.object(s, name, side_effect=method(name)))
                different = stack.enter_context(patch.object(s, "windows_different_cwd_restart"))
                stack.enter_context(patch.object(s, "check_rows", return_value="2"))
                stack.enter_context(patch.object(s, "persist"))
                trial = {"strategy": strategy}
                s.windows_lifecycle({}, trial)
                self.assertEqual(different.call_count, 1 if strategy == "relative" else 0)
            self.assertEqual(trial["stage_reached"], "complete")
            self.assertEqual(trial["data_directory"], "/reported/data")
            self.assertTrue(trial["pg_ctl_status_works"] and trial["pg_dump_works"] and trial["pg_restore_works"])
            self.assertEqual(calls[-1], "stop")

    def test_windows_init_dump_restore_use_trial_argument_spelling(self):
        s = self.make()
        s.triple = "x86_64-pc-windows-msvc"
        root = self.work / "runtime"
        fake_tree(root)
        s.runtime = root
        trial = {"class": "C", "strategy": "relative", "status": "fail"}
        with patch.object(spike, "host_triple", return_value=s.triple):
            c = s.prepare_windows_trial(trial, 1)
        calls = []
        def run(args, **kwargs):
            args = [str(a) for a in args]
            calls.append(args)
            tool = Path(args[0]).stem
            if tool == "initdb":
                self.assertEqual(args[args.index("-D") + 1], "pg18")
                self.assertIn("--pwfile=password.tmp", args)
                self.assertTrue(c["password_file"].exists())
                c["data"].mkdir()
            elif tool == "pg_dump":
                self.assertEqual(args[-1], "nightly.dump")
                c["dump_file"].write_bytes(b"fake dump")
            elif tool == "pg_restore":
                self.assertEqual(args[-1], "nightly.dump")
            return {"stdout": "postgres (PostgreSQL) 18.6\n" if "--version" in args else "", "stderr": "", "returncode": 0, "seconds": .01}
        with patch.object(s, "run", side_effect=run), patch.object(s, "owns", return_value=True), patch.object(s, "ensure_fixture"), patch.object(s, "psql"), patch.object(s, "check_rows", return_value="2"):
            s.init(c, {})
            s.backup(c, {})
        self.assertFalse(c["password_file"].exists())
        self.assertEqual(s.command_cwd, c["data"].parent)
        self.assertEqual(len(calls), 4)

    def test_windows_execute_keeps_eleven_steps_and_prints_full_trial_errors(self):
        s = self.make()
        s.triple = "x86_64-pc-windows-msvc"
        def matrix(d):
            s.lc_ctype_probe = {"status": "pass", "details": {}}
            d.update({"applicable": True, "outcome": "pass", "subst": {"reason": "shared state"}, "trials": [
                {"name": "C / direct", "class": "C", "strategy": "direct", "status": "fail", "stage_reached": "initdb",
                 "error": "EXACT FIRST\n" + "Z" * 600 + "\nEXACT LAST"}]})
        output = io.StringIO()
        with patch.object(s, "download"), patch.object(s, "inspect_inventory"), patch.object(s, "linkage"), patch.object(s, "windows_matrix", side_effect=matrix), patch.object(s, "signed"), patch.object(s, "cleanup"), contextlib.redirect_stdout(output):
            self.assertEqual(s.execute(), 0)
        self.assertEqual([r["step"] for r in s.result["steps"]], list(range(1, 12)))
        self.assertIn("Z" * 600, output.getvalue())
        self.assertIn("EXACT FIRST\n", output.getvalue())
        self.assertIn("EXACT LAST", output.getvalue())
        self.assertTrue(all(r["status"] == "pass" for r in s.result["steps"]))

    @unittest.skipIf(os.name == "nt", "Creating fake symlinks may require Windows developer mode")
    def test_cleanup_restores_trial_cwd_and_removes_only_junction_link(self):
        s = self.make()
        with patch.object(spike, "host_triple", return_value=TRIPLE):
            c = s.new_cluster(self.work / "runtime", "trial")
        target = self.work / "real 漢字"
        target.mkdir()
        sentinel = target / "must-survive"
        sentinel.write_text("data")
        link = self.work / "ASCII-link"
        link.symlink_to(target, target_is_directory=True)
        c.update({"junction": link, "tool_cwd": target, "tool_env": dict(s.env, PGPASSFILE="trial.pgpass")})
        c.update(start_attempted=True, pid=9000, data_arg=link / "pg18")
        events = []
        def run(args, **kwargs):
            self.assertEqual(s.command_cwd, target)
            self.assertEqual(s.env["PGPASSFILE"], "trial.pgpass")
            self.assertEqual(args[args.index("-D") + 1], link / "pg18")
            events.append("stop")
            return {"returncode": 0, "stdout": "", "stderr": ""}
        def remove_link(p):
            self.assertEqual(events, ["stop", "verify"])
            p.unlink()
        def identity(pid):
            events.append("verify")
            raise ProcessLookupError
        fake_tree(c["root"])
        with patch.object(s, "run", side_effect=run), patch.object(s, "identity", side_effect=identity), patch.object(s, "wait_descendants"), patch.object(spike, "windows_remove_junction", side_effect=remove_link) as remove:
            s.cleanup({})
        remove.assert_called_once_with(link)
        self.assertTrue(sentinel.exists())
        self.assertFalse(link.exists())
        self.assertEqual(s.command_cwd, self.work)
        self.assertNotEqual(s.env["PGPASSFILE"], "trial.pgpass")


    def test_output_decode_order_cp1252_cp932_oem_and_replacement(self):
        cases = [
            ("é漢字\r\n".encode(), (1252, 932), "é漢字\n", "utf-8"),
            ("é\r\n".encode("cp1252"), (1252, 437), "é\n", "cp1252"),
            ("漢字\r\n".encode("cp932"), (932, 437), "漢字\n", "cp932"),
            (b"\x81\r\n", (1252, 437), "ü\n", "cp437"),
            (b"\xff\r\n", (), "�\n", "utf-8/replace"),
            (b"\xff", (999999,), "�", "utf-8/replace"),
        ]
        for raw, pages, expected, decoder in cases:
            with self.subTest(decoder=decoder):
                self.assertEqual(spike.decode_output(raw, pages), (expected, decoder))

    def test_decode_uses_active_windows_apis_and_survives_api_failure(self):
        k = self.fake_kernel()
        k.GetACP.result, k.GetOEMCP.result = 932, 437
        with patch.object(spike.os, "name", "nt"), patch.object(spike, "windows_kernel32", return_value=k):
            self.assertEqual(spike.decode_output("漢字".encode("cp932")), ("漢字", "cp932"))
        with patch.object(spike.os, "name", "nt"), patch.object(spike, "windows_kernel32", side_effect=OSError):
            self.assertEqual(spike.decode_output(b"\xff"), ("�", "utf-8/replace"))

    def test_psql_utf8_stdin_and_crlf_rows_with_byte_only_subprocess(self):
        s = self.make()
        root = self.work / "runtime"
        fake_tree(root)
        c = {"root": root, "port": 45432}
        calls = []
        class Process:
            def __init__(self, argv, **kw):
                calls.append((argv, kw, kw["stdin"].read()))
                kw["stdout"].write(b"1|before crash one\r\n2|before crash two\r\n")
            def wait(self, timeout=None):
                return 0
        with patch.object(s, "owns", return_value=True), patch.object(spike.subprocess, "Popen", Process):
            s.psql(c, "SELECT 'リゼロ Ōkami é'")
            self.assertEqual(s.check_rows(c), "2")
        argv, kw, stdin = calls[0]
        self.assertEqual(stdin, "SELECT 'リゼロ Ōkami é'\n".encode("utf-8"))
        self.assertNotIn("-c", argv)
        self.assertEqual(argv[-2:], ["-f", "-"])
        self.assertTrue(all("リゼロ" not in a for a in argv))
        self.assertEqual(kw["env"]["PGCLIENTENCODING"], "UTF8")
        self.assertNotIn("text", kw)
        self.assertNotIn("encoding", kw)

    def test_dump_restore_output_uses_decoder_and_utf8_environment(self):
        s = self.make()
        captured = []
        class Process:
            def __init__(self, argv, **kw):
                captured.append(kw)
                kw["stderr"].write(b"path \xe9\r\n")
            def wait(self, timeout=None):
                return 0
        original = spike.decode_output
        with patch.object(spike.subprocess, "Popen", Process), patch.object(spike, "decode_output", side_effect=lambda raw: original(raw, (1252, 437))):
            for tool in ("pg_dump", "pg_restore"):
                rec = s.run([tool], password=True)
                self.assertEqual(rec["stderr"], "path é\n")
                self.assertEqual(rec["stderr_decode_encoding"], "cp1252")
        self.assertTrue(all(k["env"]["PGCLIENTENCODING"] == "UTF8" and "text" not in k and "encoding" not in k for k in captured))

    def test_cleanup_identity_decode_error_cannot_prevent_stop(self):
        s = self.make()
        root = self.work / "runtime"
        fake_tree(root)
        with patch.object(spike, "host_triple", return_value=TRIPLE):
            c = s.new_cluster(root, "identity-error")
        c.update(start_attempted=True, pid=9000)
        events = []
        def run(args, **kwargs):
            events.append("stop")
            self.assertIn("fast", args)
            return {"returncode": 0, "stdout": "", "stderr": ""}
        def identity(pid):
            self.assertEqual(events[0], "stop")
            if len(events) == 1:
                events.append("decode-error")
                raise UnicodeDecodeError("utf-8", b"\xe9", 0, 1, "injected identity decode")
            raise ProcessLookupError
        s.command_deadline = 0
        with patch.object(s, "run", side_effect=run), patch.object(s, "identity", side_effect=identity), patch.object(s, "wait_descendants"), patch.object(spike.time, "sleep"):
            details = {}
            s.cleanup(details)
        self.assertEqual(details["clusters"][0]["status"], "pass")
        self.assertTrue(details["clusters"][0]["identity_check_errors"])
        self.assertEqual(s.command_deadline, 0)

    def test_cleanup_failed_stop_uses_identified_kill_and_refuses_reused_pid(self):
        for reused in (False, True):
            with self.subTest(reused=reused):
                work = self.work / str(reused)
                work.mkdir()
                s = spike.Spike(work, TRIPLE, work / "result.json")
                root = work / "runtime"
                fake_tree(root)
                with patch.object(spike, "host_triple", return_value=TRIPLE):
                    c = s.new_cluster(root, "fallback")
                ident = (str((root / "bin/postgres").resolve()), "creation")
                c.update(start_attempted=True, pid=9000, identity=ident)
                live = [True]
                def identity(pid):
                    if not live[0]:
                        raise ProcessLookupError
                    return ("other.exe", "new creation") if reused else ident
                def kill(cluster):
                    self.assertEqual(cluster["identity"], ident)
                    live[0] = False
                with patch.object(s, "run", return_value={"returncode": 1, "stdout": "", "stderr": "stop failed"}), patch.object(s, "identity", side_effect=identity), patch.object(s, "hard_kill", side_effect=kill) as killed, patch.object(s, "remember_descendants"), patch.object(s, "wait_descendants"):
                    details = {}
                    s.cleanup(details)
                self.assertEqual(killed.call_count, 0 if reused else 1)
                self.assertTrue(details["clusters"][0].get("pid_reused" if reused else "forced_stop"))

    def test_ascii_locale_observations_and_c_tsvector_are_recorded(self):
        s, active, calls, query = self.locale_scenario({"en_US.UTF-8": "empty", "C": "empty"})
        with patch.object(s, "psql", side_effect=query):
            details = {}
            s.locale_probe({}, details)
        self.assertTrue(details["ascii_key_identical_under_measured_lc_ctype"])
        self.assertEqual(details["cluster_default"]["observations"]["details"]["ascii_key_trigrams"], "{ascii-key}")
        self.assertTrue(details["lc_ctype_c"]["simple_keeps_non_ascii_tokens"])
        self.assertIn("リゼロ", details["lc_ctype_c"]["simple_tsvector"])
        candidate = next(r for r in details["candidates"] if r["name"] == "en_US.UTF-8")
        self.assertIn("kana/Hangul", candidate["finding"])
        self.assertFalse(active)

    def test_runtime_trim_ecpg_pgtypes_uuid_ossp_and_keep_required_linkage_gate(self):
        s = self.make()
        root = self.work / "full"
        fake_tree(root)
        dropped = ("bin/ecpg", "lib/libecpg.so.6.18", "lib/libecpg_compat.so.3.18", "lib/libpgtypes.so.3", "lib/ecpg.dll",
                   "include/ecpglib.h", "include/ecpgerrno.h", "include/pgtypes_date.h", "include/sqlca.h", "include/sqlda.h",
                   "lib/uuid-ossp.so", "share/extension/uuid-ossp.control", "share/extension/uuid-ossp--1.1.sql")
        for name in dropped:
            p = root / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(b"\x7fELFfake" if name.startswith("lib/") else b"fake")
        (root / "lib/libpq.so.5").write_bytes(b"\x7fELFfake")
        data = spike.inventory(root, TRIPLE)
        self.assertTrue(set(dropped).issubset(data["drop_files"]))
        self.assertTrue(set(dropped).issubset({r["file"] for r in data["excluded_findings"]}))
        s.bundle, s.runtime_files = root, data["runtime_files"]
        def tool(name, *args, **kwargs):
            return {"returncode": 0, "stderr": "", "stdout": "libmissing.so => not found" if name == "ldd" else ""}
        with patch.object(spike.platform, "system", return_value="Linux"), patch.object(spike, "host_triple", return_value=TRIPLE), patch.object(s, "tool", side_effect=tool):
            details = {}
            with self.assertRaisesRegex(RuntimeError, "libpq"):
                s.linkage(details)
        self.assertEqual([r["file"] for r in details["runtime_failures"]], ["lib/libpq.so.5"])
        self.assertTrue(any(r.get("missing_libraries") for r in details["excluded_findings"]))

    def test_windows_exit_verification_is_handle_based_and_closes_handles(self):
        k = self.fake_kernel()
        k.OpenProcess.result = 0x123456789abcdef
        def code(handle, result):
            result._obj.value = 259
            return 1
        k.GetExitCodeProcess.result = code
        self.assertTrue(spike.windows_process_alive(9000, k))
        k.GetExitCodeProcess.result = lambda h, p: setattr(p._obj, "value", 0) or 1
        self.assertFalse(spike.windows_process_alive(9000, k))
        self.assertEqual(k.CloseHandle.calls, [(0x123456789abcdef,)] * 2)
        k.OpenProcess.result = 0
        with patch.object(spike.ctypes, "get_last_error", return_value=87, create=True):
            self.assertFalse(spike.windows_process_alive(9000, k))
        with patch.object(spike.ctypes, "get_last_error", return_value=5, create=True), patch.object(spike, "windows_error", return_value=PermissionError("access denied")):
            with self.assertRaises(PermissionError):
                spike.windows_process_alive(9000, k)

    def test_standard_user_junction_token_selection_revert_and_failure(self):
        for elevated, failure in ((False, False), (True, False), (True, True)):
            with self.subTest(elevated=elevated, failure=failure):
                k = self.fake_kernel()
                k.GetFileAttributesW.result = 0x410
                k.OpenProcessToken.result = lambda h, a, p: setattr(p._obj, "value", 111) or 1
                def info(handle, cls, buf, size, used):
                    buf._obj.value = (222 if cls == 19 else (int(elevated) if handle.value == 111 else 0))
                    return 1
                k.GetTokenInformation.result = info
                details = {}
                with patch.object(spike, "windows_native_junction", side_effect=PermissionError("denied") if failure else None) as create, patch.object(Path, "samefile", return_value=True):
                    if failure:
                        with self.assertRaises(PermissionError):
                            spike.windows_standard_junction_probe(self.work / "link", self.work, details, k)
                    else:
                        spike.windows_standard_junction_probe(self.work / "link", self.work, details, k)
                self.assertTrue(details["standard_user_token"])
                self.assertEqual(details["token_source"], "linked UAC token" if elevated else "current token")
                self.assertEqual(k.ImpersonateLoggedOnUser.calls[0][0].value, 222 if elevated else 111)
                self.assertEqual(len(k.RevertToSelf.calls), 1)
                self.assertEqual([c[0].value for c in k.CloseHandle.calls], [222, 111] if elevated else [111])
                if not failure:
                    self.assertTrue(details["created"] and details["verified"] and details["removed"])
                create.assert_called_once()

    def test_native_junction_mount_point_buffer_and_failure_cleanup(self):
        k = self.fake_kernel()
        k.CreateFileW.result = 0x123456789abcdef
        details = []
        def ioctl(handle, code, buf, size, *args):
            raw = buf.raw[:size]
            header = spike.struct.unpack("<IHHHHHH", raw[:16])
            details.append(header)
            self.assertEqual(handle, 0x123456789abcdef)
            self.assertEqual(code, 0x000900a4)
            self.assertEqual(header[:4], (0xa0000003, len(raw) - 8, 0, 0))
            self.assertEqual(raw[16:16 + header[4]].decode("utf-16le"), "\\??\\" + str(self.work.resolve()))
            self.assertEqual(raw[16 + header[5]:16 + header[5] + header[6]].decode("utf-16le"), str(self.work.resolve()))
            return 1
        k.DeviceIoControl.result = ioctl
        spike.windows_native_junction(self.work / "link", self.work, k)
        self.assertEqual(k.CreateFileW.calls[0][1:6], (0x40000000, 0, None, 3, 0x02200000))
        self.assertEqual(k.CloseHandle.calls, [(0x123456789abcdef,)])
        k.DeviceIoControl.result = 0
        with patch.object(spike, "windows_error", return_value=OSError("set reparse failed")), self.assertRaisesRegex(OSError, "set reparse"):
            spike.windows_native_junction(self.work / "failed", self.work, k)
        self.assertEqual(k.RemoveDirectoryW.calls, [(str(self.work / "failed"),)])

    def test_junction_location_probes_only_localappdata_programdata_and_remove(self):
        s = self.make()
        local, shared = self.work / "local", self.work / "shared"
        local.mkdir()
        shared.mkdir()
        calls = []
        def probe(link, target, details):
            calls.append(link)
            self.assertIn(link.parent, (local, shared))
            self.assertTrue(target.is_dir())
            details.update(standard_user_token=True, created=True, verified=True, removed=True, token_source="linked UAC token")
        with patch.dict(os.environ, {"LOCALAPPDATA": str(local), "ProgramData": str(shared)}), patch.object(spike, "windows_standard_junction_probe", side_effect=probe):
            details = {}
            s.windows_junction_locations(details)
        self.assertEqual(len(calls), 2)
        self.assertTrue(all(r["standard_user_creation_demonstrated"] for r in details["locations"]))
        self.assertFalse(list(self.work.glob("junction-location-target-*")))

    def test_fresh_junction_restart_updates_every_tool_alias_and_cleanup_record(self):
        s = self.make()
        target, old = self.work / "real 漢字", self.work / "old-link"
        c = {"data": target / "pg18", "junction": old}
        for key, name in (("data", "pg18"), ("password", "password.tmp"), ("log", "postgres.log"), ("dump", "nightly.dump"), ("pgpass", "empty.pgpass")):
            c[key + "_arg"] = old / name
        events = []
        def start(cluster, details):
            events.append("start")
            self.assertNotEqual(cluster["junction"], old)
            self.assertTrue(all(cluster[key + "_arg"].parent == cluster["junction"] for key in ("data", "password", "log", "dump", "pgpass")))
            self.assertEqual(cluster["tool_env"]["PGPASSFILE"], str(cluster["pgpass_arg"]))
        with patch.object(s, "stop", side_effect=lambda c, d: events.append("stop")), patch.object(spike, "windows_remove_junction", side_effect=lambda p: events.append("remove")), patch.object(spike, "windows_create_junction", side_effect=lambda *a: events.append("create")), patch.object(s, "start", side_effect=start), patch.object(s, "windows_observations"), patch.object(s, "check_rows", return_value="2"):
            details = {}
            s.windows_relocated_junction_restart(c, details)
        self.assertEqual(events, ["stop", "remove", "create", "start"])
        self.assertEqual(details["preserved_rows"], "2")
        self.assertEqual(details["target"], str(target))

    def test_standard_user_missing_linked_token_is_unmeasured_and_closed(self):
        k = self.fake_kernel()
        k.OpenProcessToken.result = lambda h, a, p: setattr(p._obj, "value", 111) or 1
        def info(handle, cls, buf, size, used):
            if cls == 19:
                return 0
            buf._obj.value = 1
            return 1
        k.GetTokenInformation.result = info
        details = {"standard_user_token": False}
        with patch.object(spike, "windows_error", return_value=OSError("standard-user permission unmeasured")), patch.object(spike, "windows_native_junction") as create:
            with self.assertRaisesRegex(OSError, "unmeasured"):
                spike.windows_standard_junction_probe(self.work / "link", self.work, details, k)
        self.assertFalse(details["standard_user_token"])
        create.assert_not_called()
        self.assertEqual([c[0].value for c in k.CloseHandle.calls], [111])
        self.assertFalse(k.ImpersonateLoggedOnUser.calls)

    def test_unexpected_locale_probe_error_is_diagnostic_on_unix_and_windows(self):
        for windows in (False, True):
            with self.subTest(windows=windows):
                work = self.work / str(windows)
                work.mkdir()
                s = spike.Spike(work, "x86_64-pc-windows-msvc" if windows else TRIPLE, work / "result.json")
                s.lc_ctype_probe = {"status": "fail", "error": "injected observation failure", "details": {}}
                with patch.object(s, "psql", return_value={"returncode": 0, "stdout": "1", "stderr": ""}), patch.object(s, "retained_module_loads"):
                    # Authentication has its own gate; supply that expected refusal.
                    original = s.psql
                    def query(c, sql, **kwargs):
                        if kwargs.get("password") is False:
                            return {"returncode": 2, "stdout": "", "stderr": "no password supplied"}
                        return original(c, sql, **kwargs)
                    with patch.object(s, "psql", side_effect=query):
                        details = {}
                        s.queries({}, details)
                self.assertEqual(details["lc_ctype_probe"]["status"], "fail")
                if windows:
                    with patch.object(s, "download"), patch.object(s, "inspect_inventory"), patch.object(s, "linkage"), patch.object(s, "windows_matrix", side_effect=lambda d: d.update(outcome="pass")), patch.object(s, "signed"), patch.object(s, "cleanup"), patch.object(s, "persist"), contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(s.execute(), 0)

    def windows_os(self):
        # Change this module's view of the OS without changing pathlib's host OS.
        return patch.object(spike, "os", SimpleNamespace(**{**vars(os), "name": "nt"}))

    def test_every_start_restart_and_direct_probe_uses_files_not_pipes(self):
        s = self.make()
        s.command_output_dir = self.work / "ascii-helper"
        s.command_output_dir.mkdir()
        calls = []
        class FakeProcess:
            def __init__(self, args, **kw):
                # This fake would hang if the detached server inherited a PIPE.
                if kw["stdout"] == spike.subprocess.PIPE or kw["stderr"] == spike.subprocess.PIPE:
                    raise AssertionError("server retained a pipe")
                calls.append((args, kw))
                kw["stderr"].write(b"FATAL: lock file already exists\r\n")
            def wait(self, timeout=None):
                return 1
            def communicate(self, *args, **kwargs):
                raise AssertionError("must wait on the command, never pipe EOF")
        with patch.object(spike.subprocess, "Popen", FakeProcess):
            for tool, op in (("pg_ctl.exe", "start"), ("pg_ctl.exe", "restart"), ("postgres.exe", None)):
                args = [tool, "-D", "live"] + ([op] if op else [])
                rec = s.run(args, check=False)
                self.assertEqual(rec["stderr"], "FATAL: lock file already exists\n")
        for args, kw in calls:
            self.assertEqual(kw["stdin"], spike.subprocess.DEVNULL)
            for key in ("stdout", "stderr"):
                self.assertEqual(Path(kw[key].name).parent, s.command_output_dir)
                self.assertTrue(kw[key].name.isascii())

    def test_probe_timeout_records_finding_recovery_and_dump_continue(self):
        crashed = self.runtime_scenario(probe_timeout=True)
        self.assertTrue(crashed["recovery_succeeded"])
        self.assertEqual(crashed["second_start_refused"], "timeout")
        self.assertEqual(crashed["second_start_check"]["status"], "pass")
        direct = crashed["direct_postgres_start_check"]["details"]
        self.assertTrue(direct["refused"])
        self.assertEqual(direct["exit_code"], 1)
        self.assertIn("postmaster.pid", direct["message"])
        self.assertTrue(direct["original_server_unchanged"])

    def test_failed_duplicate_start_observation_still_runs_direct_probe_and_dump(self):
        crashed = self.runtime_scenario(probe_error=True)
        self.assertTrue(crashed["recovery_succeeded"])
        self.assertEqual(crashed["second_start_check"]["status"], "fail")
        self.assertFalse(crashed["second_start_refused"])
        self.assertTrue(crashed["direct_postgres_start_check"]["details"]["refused"])

    def test_pg_ctl_warning_alone_does_not_claim_lock_refusal(self):
        s = self.make()
        c = {"pid": 12, "identity": ("private", 1), "data": self.work / "pg18", "log": self.work / "postgres.log"}
        rec = {"returncode": 1, "stdout": "", "stderr": "another server might be running; trying to start server anyway", "seconds": .01}
        with patch.object(s, "owns", return_value=True), patch.object(s, "bin", return_value=self.work / "pg_ctl.exe"), patch.object(s, "run", return_value=rec):
            details = {}
            s.live_start_probe(c, details)
        self.assertFalse(details["refused"])

    def test_timeout_terminates_only_identified_probe_tree_inside_runtime(self):
        s = self.make()
        runtime = self.work / "runtime"
        c = {"root": runtime, "pid": 99, "identity": (str(runtime / "postgres.exe"), 1, 1)}
        identities = {10: (str(runtime / "pg_ctl.exe"), 1, 10),
                      11: (str(self.work / "system/cmd.exe"), 1, 11),
                      12: (str(runtime / "postgres.exe"), 1, 12),
                      13: (str(runtime / "postgres.exe"), 1, 9),
                      99: c["identity"], 20: (str(runtime / "postgres.exe"), 1, 20)}
        rows = [{"pid": pid, "parent": parent} for pid, parent in ((10, 1), (11, 10), (12, 11), (13, 10), (99, 10), (20, 1))]
        with patch.object(spike, "windows_identity", side_effect=lambda pid: identities[pid]), patch.object(spike, "windows_processes", return_value=rows), patch.object(spike, "windows_terminate_owned", return_value=True) as kill:
            details = {}
            s.stop_probe_tree(10, identities[10], c, details)
        self.assertEqual([call.args[0] for call in kill.call_args_list], [12, 10])
        with patch.object(spike, "windows_identity", return_value=(str(runtime / "pg_ctl.exe"), 2, 10)), patch.object(spike, "windows_processes") as snapshot, patch.object(spike, "windows_terminate_owned") as kill:
            s.stop_probe_tree(10, identities[10], c, {})
        kill.assert_not_called()
        snapshot.assert_not_called()

    def test_windows_run_timeout_keeps_output_and_invokes_probe_cleanup(self):
        s = self.make()
        events = []
        class FakeProcess:
            pid = 10
            def __init__(self, args, **kw):
                kw["stderr"].write(b"another server might be running\r\n")
            def wait(self, timeout=None):
                events.append(timeout)
                if len(events) == 1:
                    raise spike.subprocess.TimeoutExpired(["pg_ctl.exe"], timeout)
                return 1
            def kill(self):
                raise AssertionError("unverified kill")
        with self.windows_os(), patch.object(spike.subprocess, "Popen", FakeProcess), patch.object(spike, "windows_identity", return_value=("private", 1, 2)), patch.object(s, "stop_probe_tree") as cleanup:
            with self.assertRaises(spike.subprocess.TimeoutExpired):
                s.run(["pg_ctl.exe"], timeout=5, probe_cluster={"pid": 99})
        cleanup.assert_called_once()
        self.assertEqual(events, [5, 1])
        self.assertIn("another server", s.result["commands"][-1]["stderr"])
        self.assertEqual(s.result["commands"][-1]["probe_pid"], 10)

    def test_failed_initdb_cleanup_is_noop(self):
        s = self.make()
        with patch.object(spike, "host_triple", return_value=TRIPLE):
            c = s.new_cluster(self.work / "runtime", "failed-initdb")
        with self.windows_os(), patch.object(s, "run") as run, patch.object(spike, "windows_private_processes") as query:
            details = {}
            s.cleanup(details, final=False)
        run.assert_not_called()
        query.assert_not_called()
        self.assertFalse(c["initialized"])
        self.assertTrue(details["clusters"][0]["nothing_to_stop"])
        self.assertEqual(details["clusters"][0]["status"], "pass")

    def test_interrupted_start_no_pid_and_no_private_process_is_nothing_to_stop(self):
        s = self.make()
        with patch.object(spike, "host_triple", return_value=TRIPLE):
            c = s.new_cluster(self.work / "runtime", "interrupted")
        c["start_attempted"] = True
        with self.windows_os(), patch.object(s, "bin", return_value=self.work / "runtime/bin/pg_ctl.exe"), patch.object(s, "bounded_deadline", return_value=0), patch.object(s, "run", return_value={"returncode": 1, "stdout": "", "stderr": "PID file does not exist"}), patch.object(spike, "windows_private_processes", return_value={}) as query:
            details = {}
            s.cleanup(details, final=False)
        query.assert_called_once_with(c["root"])
        rec = details["clusters"][0]
        self.assertEqual(rec["status"], "pass")
        self.assertTrue(rec["nothing_to_stop"])
        self.assertEqual(rec["no_pid_evidence"]["live_private_postgres"], [])

    def test_private_process_filter_excludes_foreign_and_previous_runs(self):
        runtime = self.work / "runtime"
        rows = [{"pid": i, "parent": 1, "name": "postgres.exe"} for i in (1, 2, 3, 4)]
        rows.append({"pid": 5, "parent": 1, "name": "other.exe"})
        identities = {1: (str(runtime / "bin/postgres.exe"), 1, 1),
                      2: (str(self.work / "runtime-foreign/bin/postgres.exe"), 1, 2),
                      3: (str(self.work.parent / "previous/runtime/bin/postgres.exe"), 1, 3),
                      4: (str(runtime / "bin/postgres.exe"), 1, 4)}
        with patch.object(spike, "windows_processes", return_value=rows), patch.object(spike, "windows_identity", side_effect=lambda pid: identities[pid]), patch.object(spike, "windows_process_alive", side_effect=lambda pid: pid != 4):
            self.assertEqual(spike.windows_private_processes(runtime), {1: identities[1]})

    def test_final_sweep_runs_without_pid_and_ignores_other_runtimes(self):
        s = self.make()
        s.runtime = self.work / "runtime"
        ident = (str(s.runtime / "bin/postgres.exe"), 1, 2)
        foreign = (str(self.work.parent / "previous/runtime/bin/postgres.exe"), 1, 3)
        live = {12: ident, 13: foreign}
        rows = [{"pid": pid, "parent": 1, "name": "postgres.exe"} for pid in live]
        def terminate(pid, identity, runtime):
            self.assertEqual((pid, identity, runtime), (12, ident, s.runtime))
            live.pop(pid)
            return True
        with self.windows_os(), patch.object(spike, "windows_processes", return_value=rows), patch.object(spike, "windows_identity", side_effect=lambda pid: ident if pid == 12 else foreign), patch.object(spike, "windows_process_alive", side_effect=lambda pid: pid in live), patch.object(spike, "windows_terminate_owned", side_effect=terminate) as kill:
            details = {}
            s.cleanup(details)
        kill.assert_called_once_with(12, ident, s.runtime)
        self.assertEqual(live, {13: foreign})
        self.assertEqual(details["owned_process_sweep"]["status"], "pass")
        self.assertEqual(details["owned_process_sweep"]["remaining"], [])

    def test_final_sweep_resolves_interrupted_start_with_no_pid(self):
        s = self.make()
        s.runtime = self.work / "runtime"
        with patch.object(spike, "host_triple", return_value=TRIPLE):
            c = s.new_cluster(s.runtime, "interrupted-final")
        c["start_attempted"] = True
        ident = (str(s.runtime / "bin/postgres.exe"), 1, 2)
        with self.windows_os(), patch.object(s, "bin", return_value=s.runtime / "bin/pg_ctl.exe"), patch.object(s, "bounded_deadline", return_value=0), patch.object(s, "run", return_value={"returncode": 1, "stdout": "", "stderr": "PID file does not exist"}), patch.object(spike, "windows_private_processes", side_effect=[{12: ident}, {12: ident}, {}]), patch.object(spike, "windows_terminate_owned", return_value=True) as kill:
            details = {}
            s.cleanup(details)
        kill.assert_called_once_with(12, ident, s.runtime)
        self.assertTrue(details["clusters"][0]["resolved_by_final_sweep"])
        self.assertTrue(c["stop_verified"])
        self.assertEqual(details["clusters"][0]["status"], "pass")

    def test_toolhelp_wide_enumeration_handles_and_errors(self):
        k = self.fake_kernel()
        k.CreateToolhelp32Snapshot.result = 0x123456789abcdef
        def first(h, entry):
            self.assertEqual(entry._obj.dwSize, spike.ctypes.sizeof(spike.WINDOWS_PROCESS_ENTRY))
            entry._obj.th32ProcessID = 12
            entry._obj.th32ParentProcessID = 10
            entry._obj.szExeFile = "postgres.exe"
            return 1
        k.Process32FirstW.result = first
        k.Process32NextW.result = 0
        with patch.object(spike.ctypes, "get_last_error", return_value=18, create=True):
            rows = spike.windows_processes(k)
        self.assertEqual(rows, [{"pid": 12, "parent": 10, "name": "postgres.exe"}])
        self.assertEqual(k.CreateToolhelp32Snapshot.calls, [(2, 0)])
        self.assertEqual(k.CloseHandle.calls, [(0x123456789abcdef,)])
        with patch.object(spike.ctypes, "get_last_error", return_value=5, create=True), patch.object(spike, "windows_error", return_value=OSError("enumeration failed")):
            with self.assertRaisesRegex(OSError, "enumeration failed"):
                spike.windows_processes(k)

    def test_termination_uses_same_handle_creation_time_and_private_image(self):
        runtime = self.work / "runtime"
        ident = (str(runtime / "bin/postgres.exe"), 1, 2)
        k = self.fake_kernel()
        k.OpenProcess.result = 0x123456789abcdef
        k.WaitForSingleObject.result = 0
        with patch.object(spike, "windows_handle_identity", return_value=ident) as identity:
            self.assertTrue(spike.windows_terminate_owned(12, ident, runtime, k))
        identity.assert_called_once_with(0x123456789abcdef, k)
        self.assertEqual(k.OpenProcess.calls, [(0x101001, False, 12)])
        self.assertEqual(k.TerminateProcess.calls, [(0x123456789abcdef, 1)])
        self.assertEqual(k.CloseHandle.calls, [(0x123456789abcdef,)])
        k.TerminateProcess.calls.clear()
        with patch.object(spike, "windows_handle_identity", return_value=(ident[0], 1, 3)):
            self.assertFalse(spike.windows_terminate_owned(12, ident, runtime, k))
        self.assertFalse(k.TerminateProcess.calls)
        with self.assertRaisesRegex(RuntimeError, "outside this run"):
            spike.windows_terminate_owned(12, (str(self.work / "foreign/postgres.exe"), 1, 2), runtime, k)

    def test_junction_observation_failure_does_not_skip_restart_persistence(self):
        s = self.make()
        c = {"data": self.work / "real 漢字/pg18", "junction": self.work / "old-link"}
        for key, name in (("data", "pg18"), ("password", "password.tmp"), ("log", "postgres.log"), ("dump", "nightly.dump"), ("pgpass", "empty.pgpass")):
            c[key + "_arg"] = c["junction"] / name
        with patch.object(s, "stop"), patch.object(spike, "windows_remove_junction"), patch.object(spike, "windows_create_junction"), patch.object(s, "start"), patch.object(s, "windows_observations", side_effect=RuntimeError("observation only")), patch.object(s, "check_rows", return_value="2"):
            details = {}
            s.windows_relocated_junction_restart(c, details)
        self.assertEqual(details["path_observations"]["status"], "fail")
        self.assertEqual(details["preserved_rows"], "2")

    def test_path_observation_failure_does_not_abort_full_trial(self):
        s = self.make()
        with contextlib.ExitStack() as stack:
            for name in ("init", "start", "queries", "ensure_fixture", "stop", "crash", "persist"):
                stack.enter_context(patch.object(s, name))
            stack.enter_context(patch.object(s, "check_rows", return_value="2"))
            stack.enter_context(patch.object(s, "backup", side_effect=lambda c, d: d.update(dump={"status": "pass"}, restore={"status": "pass"})))
            stack.enter_context(patch.object(s, "windows_observations", side_effect=RuntimeError("status observation failed")))
            trial = {"strategy": "direct"}
            s.windows_lifecycle({}, trial)
        self.assertEqual(trial["stage_reached"], "complete")
        self.assertEqual(trial["stages"]["path-observations"]["status"], "fail")
        self.assertTrue(trial["pg_dump_works"] and trial["pg_restore_works"])
        self.assertFalse(trial["pg_ctl_status_works"])

if __name__ == "__main__":
    unittest.main()
