import os
import io
import json
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import patch
import drill
import safety


# Literal PostgreSQL 18.6 capture supplied by the orchestrator; independent of
# the parser table. Labels/spacing are deliberately not synthesized.
CONTROL = """max_connections setting:              48
max_worker_processes setting:         8
max_wal_senders setting:              10
max_prepared_xacts setting:           0
max_locks_per_xact setting:           64
"""
PROJECT = "komika-pg-rehearsal-test"
HOSTED = {"KOMIKA_COMPOSE_FILE": str(drill.KIT.parent / "docker-compose.yml"),
          "KOMIKA_COMPOSE_PROJECT": "komika", "KOMIKA_COMPOSE_ENV_FILE": "/synthetic/hosted.env"}
ENVIRONMENT = dict(KOMIKA_REHEARSAL_PROJECT=PROJECT, KOMIKA_R2_PREFIX="rehearsal/test",
                   KOMIKA_PRODUCTION_R2_PREFIX="production", KOMIKA_R2_ENDPOINT="example.com",
                   KOMIKA_R2_BUCKET="bucket", KOMIKA_R2_KEY="key", KOMIKA_R2_SECRET="secret",
                   KOMIKA_POSTGRES_PASSWORD="synthetic0synthetic0synthetic0")


class FakeRehearsal:
    """What rehearsal.py hands to drill.py: a selection that was already checked."""
    project = PROJECT
    child_env = {"PATH": "/synthetic/bin", "DOCKER_HOST": "unix:///synthetic.sock"}

    network = PROJECT + "-network"

    def __init__(self, root, entry=None, values=None, source=False):
        self.root, self.entry, self.values, self.environment = Path(root), entry, values or {}, dict(ENVIRONMENT)
        self.source, self.gates, self.removed, self.names, self.settled = source, [], [], [], []

    def source_present(self):
        return self.source

    def disk_gate(self, step, docker_bytes=0, root_bytes=0, advice=""):
        self.gates.append((step, docker_bytes, root_bytes, advice))
        return [f"disk_gate step={step}"]

    def repository_network(self):
        return self.network

    def transient_name(self, kind):
        self.names.append(f"{PROJECT}-{kind}-{len(self.names):08x}")
        return self.names[-1]

    def remove_transient(self, name, settle=False):
        self.removed.append(name)
        self.settled.append(settle)

    def container(self, service, running=None, contract=True):
        if running and self.entry is None:
            raise ValueError("rehearsal postgres container is not running")
        return self.entry

    def new_path(self, value, what):
        return safety.new_child(value, self.root, what)


class DrillTests(unittest.TestCase):
    def setUp(self):
        p = patch.dict(os.environ, {}, clear=True)
        p.start()
        self.addCleanup(p.stop)
        p = patch.object(drill, "HOSTED_RECOVERY", True)
        p.start()
        self.addCleanup(p.stop)

    def test_nonempty_symlink_and_relative_target_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "data").write_text("preserve")
            with self.assertRaises(ValueError):
                drill.empty_target(root)
            self.assertEqual((root / "data").read_text(), "preserve")
            (root / "link").symlink_to(root, target_is_directory=True)
            with self.assertRaises(ValueError):
                drill.empty_target(root / "link")
            with self.assertRaises(ValueError):
                drill.empty_target(Path("relative"))

    def test_remote_docker_refused(self):
        with patch.dict(os.environ, {"DOCKER_HOST": "tcp://remote:2375"}):
            with self.assertRaises(ValueError):
                drill.source()

    def test_archive_ack_requires_actual_retrieval(self):
        with patch.object(drill.subprocess, "run") as run, patch.object(drill, "live", return_value="") as live:
            run.return_value.returncode = 0
            drill.wait_archive("0" * 24)
            self.assertEqual(live.call_args_list[0].args[2], "archive-get")
        with patch.object(drill.subprocess, "run") as run, patch.object(drill, "live", side_effect=RuntimeError("missing WAL")):
            run.return_value.returncode = 0
            with self.assertRaises(RuntimeError):
                drill.wait_archive("0" * 24)

    def test_restore_failure_cleans_owned_container_and_keeps_directory(self):
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        config = {"Config": {"Env": environment}, "Image": "sha256:test", "NetworkSettings": {"Networks": {"komika_komika": {}}}}
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            calls = []
            def command(args, **kwargs):
                calls.append((args, kwargs))
                if "restore" in args:
                    (target / "partial").write_text("retained")
                    raise RuntimeError("restore failed")
                return ""
            with patch.object(drill, "command", side_effect=command), patch.object(drill.subprocess, "run") as run:
                run.return_value.returncode = 0
                with self.assertRaises(RuntimeError):
                    drill.restore(config, target, "backup", None, False)
            self.assertTrue((target / "partial").exists())
            self.assertEqual(calls[-2][0][:3], ["docker", "rm", "-f"])
            self.assertEqual(calls[-1][0][:3], ["docker", "network", "rm"])
            create, kwargs = next(c for c in calls if c[0][:2] == ["docker", "create"])
            self.assertIn("/dev/stdin", create)
            self.assertNotIn("fake", " ".join(create))
            self.assertIn("KOMIKA_R2_SECRET=fake", kwargs["input"])
            self.assertNotIn("--volumes-from", create)
            self.assertNotIn("--publish", create)
            self.assertIn("--cpus=1", create)
            self.assertIn("--pids-limit=128", create)
            self.assertNotIn("komika_komika", create)
            self.assertIn("KOMIKA_PG_ALLOWED_CIDRS=172.29.42.0/24", kwargs["input"])

    def test_success_checks_markers_auth_and_stops_without_uploads(self):
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        config = {"Config": {"Env": environment}, "Image": "sha256:test", "NetworkSettings": {"Networks": {"komika_komika": {}}}}
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "pg_controldata" in args:
                return CONTROL
            if "SELECT NOT pg_is_in_recovery()" in args:
                return "t\n"
            if any("SELECT step" in a for a in args):
                return "first\n"
            if "sh" in args:
                return "komika\n"
            if "du" in args:
                return "12345 /restore\n"
            return ""
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), patch.object(drill.subprocess, "run") as run:
            run.return_value.returncode = 0
            drill.restore(config, Path(directory), "backup", "2026-10-11T00:00:00Z", False, "marker")
        restore = next(c for c in calls if "restore" in c)
        self.assertIn("--target-action=promote", restore)
        self.assertNotIn("--archive-async=n", restore)  # pgBackRest refuses it on restore.
        start = next(c for c in calls if "start" in c and "pg_ctl" in c)
        self.assertIn("archive_mode=off", " ".join(start))
        for key, (_, minimum) in drill.RECOVERY_SETTINGS.items():
            self.assertIn(f"{key}={minimum}", " ".join(start))
        self.assertTrue(any("stop" in c and "pg_ctl" in c for c in calls))
        marker_check = next(c for c in calls if any("SELECT step" in a for a in c))
        self.assertEqual(marker_check[marker_check.index("-d") + 1], "komika_pitr_drill")
        self.assertEqual(calls[-2][:4], ["docker", "rm", "-f", "-v"])
        self.assertEqual(calls[-1][:3], ["docker", "network", "rm"])

    def test_recovery_settings_preserve_source_and_checkpoint_ceilings(self):
        options = drill.recovery_options(CONTROL)
        for key, (_, minimum) in drill.RECOVERY_SETTINGS.items():
            self.assertIn(f"{key}={minimum}", options)
        raised = CONTROL.replace("48", "64")
        options = drill.recovery_options(raised, {"max_worker_processes": 16, "max_wal_senders": 20,
                                                 "max_prepared_transactions": 10, "max_locks_per_transaction": 128})
        for setting in ("max_connections=64", "max_worker_processes=16", "max_wal_senders=20",
                        "max_prepared_transactions=10", "max_locks_per_transaction=128"):
            self.assertIn(setting, options)
        with self.assertRaises(ValueError):
            drill.recovery_options("")

    def test_delayed_recovery_measures_real_completion(self):
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        config = {"Config": {"Env": environment}, "Image": "sha256:test"}
        clock = [10.0]
        answers = iter(["f", "f", "f", "t"])
        polls = []
        def command(args, **kwargs):
            if "pg_controldata" in args:
                return CONTROL
            if "SELECT NOT pg_is_in_recovery()" in args:
                polls.append(clock[0])
                return next(answers)
            if "sh" in args:
                self.assertEqual(len(polls), 4)  # Authentication/verification AFTER promotion.
                return "komika"
            if "du" in args:
                return "12345 /restore"
            return ""
        def sleep(seconds):
            clock[0] += seconds
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), \
                patch.object(drill.subprocess, "run") as run, patch.object(drill.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(drill.time, "sleep", side_effect=sleep):
            run.return_value.returncode = 0
            timings = drill.restore(config, Path(directory), "backup", None, False)
        self.assertEqual(timings["recovery_seconds"], 3.0)
        self.assertEqual(timings["restored_bytes"], 12345)

    def test_recovery_timeout_is_bounded_and_connection_failure_retried(self):
        clock = [0]
        def sleep(seconds):
            clock[0] += seconds
        with patch.object(drill.time, "monotonic", side_effect=lambda: clock[0]), patch.object(drill.time, "sleep", side_effect=sleep):
            with self.assertRaises(RuntimeError):
                drill.wait_recovery(lambda *args, **kwargs: "f", [], timeout=3)
            self.assertEqual(clock[0], 3)
            answers = iter([RuntimeError("connection starting"), "t"])
            def execute(*args, **kwargs):
                answer = next(answers)
                if isinstance(answer, Exception):
                    raise answer
                return answer
            drill.wait_recovery(execute, [], timeout=3)
            self.assertEqual(clock[0], 4)

    def test_a_server_that_stopped_during_recovery_ends_the_wait_with_its_own_reason(self):
        # Found by the first wrong-time restore against a repository: the target
        # lay beyond the archived WAL, PostgreSQL shut down, and the kit polled a
        # dead server for 1,800 seconds before it said anything.
        log = ("2026-10-11 15:00:32.643 GMT [79] LOG:  last completed transaction was at log time 2026-10-11 14:49:46+00\n"
               "2026-10-11 15:00:32.643 GMT [79] FATAL:  recovery ended before configured recovery target was reached\n"
               "2026-10-11 15:00:32.651 GMT [73] LOG:  database system is shut down\n")
        reason = drill.server_reason(log)
        self.assertIn("FATAL:  recovery ended before configured recovery target was reached", reason)
        self.assertIn("choose an earlier --target-time", reason)
        self.assertNotIn("--target-time", drill.server_reason("2026-10-11 GMT [1] FATAL:  could not access file\n"))
        self.assertIn("no FATAL line", drill.server_reason("LOG:  starting\n"))
        clock = [0]
        def sleep(seconds):
            clock[0] += seconds
        def refused(*args, **kwargs):
            raise RuntimeError("connection refused")
        with patch.object(drill.time, "monotonic", side_effect=lambda: clock[0]), patch.object(drill.time, "sleep", side_effect=sleep):
            # Still starting: retried twice, then the server is gone.
            answers = iter([None, None, reason])
            with self.assertRaises(RuntimeError) as caught:
                drill.wait_recovery(refused, [], timeout=1800, stopped=lambda: next(answers))
            self.assertIn("the restored server stopped during recovery", str(caught.exception))
            self.assertIn("recovery ended before configured recovery target was reached", str(caught.exception))
            self.assertEqual(clock[0], 2, "ended at the third poll, not at the 1,800 second deadline")
        # Through restore(): pg_ctl status 3 is the only proof that the server is gone.
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        config = {"Config": {"Env": environment}, "Image": "sha256:test"}
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "pg_controldata" in args:
                return CONTROL
            if "SELECT NOT pg_is_in_recovery()" in args:
                raise RuntimeError("connection refused")
            if "tail" in args:
                return log
            return ""
        def run(args, **kwargs):
            return subprocess.CompletedProcess(args, 3 if args[-1] == "status" else 0, b"", b"")
        clock[0] = 0
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), \
                patch.object(drill.subprocess, "run", side_effect=run), \
                patch.object(drill.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(drill.time, "sleep", side_effect=sleep):
            with self.assertRaises(RuntimeError) as caught:
                drill.restore(config, Path(directory), "backup", "2026-10-11T15:00:00Z", True)
        self.assertIn("stopped during recovery", str(caught.exception))
        self.assertIn("choose an earlier --target-time", str(caught.exception))
        self.assertLess(clock[0], 60)
        self.assertEqual(sum("SELECT NOT pg_is_in_recovery()" in call for call in calls), 1)
        # Even with --keep nothing of a failed restore stays.
        self.assertEqual([call[:3] for call in calls[-2:]], [["docker", "rm", "-f"], ["docker", "network", "rm"]])
        # A start that fails reports the same way instead of "inspect container logs".
        def failing_start(args, **kwargs):
            if "start" in args and "pg_ctl" in args:
                raise RuntimeError("docker operation failed (exit 1); inspect container logs")
            return command(args, **kwargs)
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=failing_start), \
                patch.object(drill.subprocess, "run", side_effect=run):
            with self.assertRaises(RuntimeError) as caught:
                drill.restore(config, Path(directory), "backup", "2026-10-11T15:00:00Z", False)
        self.assertIn("the restored server did not start: ", str(caught.exception))
        self.assertIn("FATAL:  recovery ended", str(caught.exception))

    def test_promotion_query_hang_is_killed_and_late_success_rejected(self):
        import sys
        import time
        def hanging(*args, timeout=None):
            return drill.command([sys.executable, "-c", "import time; time.sleep(30); print('t')"], timeout=timeout)
        before = time.monotonic()
        with self.assertRaises(RuntimeError):
            drill.wait_recovery(hanging, [], timeout=0.15)
        self.assertLess(time.monotonic() - before, 2)
        clock = [0.0]
        def late(*args, timeout=None):
            self.assertEqual(timeout, 3)
            clock[0] = 100
            return "t"
        with patch.object(drill.time, "monotonic", side_effect=lambda: clock[0]), self.assertRaises(RuntimeError):
            drill.wait_recovery(late, [], timeout=3)

    def test_hosted_route_requires_explicit_flag_and_rehearsal_never_falls_back(self):
        with patch.object(drill, "HOSTED_RECOVERY", False), self.assertRaises(ValueError):
            drill.compose()
        with patch.dict(os.environ, {"KOMIKA_REHEARSAL_PROJECT": PROJECT}), self.assertRaises(ValueError):
            drill.compose()
        with patch.dict(os.environ, {"KOMIKA_REHEARSAL_ROOT": ""}), self.assertRaises(ValueError):
            drill.guard("komika", {})

    def test_record_settings_reads_show_and_control_and_validates_retained_record(self):
        settings = {"max_connections": 96, "max_worker_processes": 8, "max_wal_senders": 10,
                    "max_prepared_transactions": 0, "max_locks_per_transaction": 64}
        selected = {"name": "komika", "services": {"postgres": {"environment": {}}}}
        container = {"State": {"Running": True}, "Config": {"Env": []}}
        with patch.dict(os.environ, {"DOCKER_HOST": "unix:///test"}), \
                patch.object(drill, "command", side_effect=[json.dumps(selected), "source", json.dumps([container])]), \
                patch.object(drill, "sql", side_effect=lambda q: str(settings[q.removeprefix("SHOW ")])) as sql:
            self.assertEqual(drill.source()["RecoverySettings"], settings)
        self.assertEqual([c.args[0] for c in sql.call_args_list], ["SHOW " + key for key in settings])
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / "settings.json"
            with patch.object(drill, "source", return_value={"RecoverySettings": settings}), \
                    patch.object(drill, "live", return_value=CONTROL) as live, \
                    patch("sys.argv", ["drill.py", "record-settings", "--out", str(out), "--hosted-recovery"]):
                drill.main()
            self.assertIn('LC_ALL=C pg_controldata "$PGDATA"', live.call_args.args)
            retained = drill.load_settings(out)
            self.assertIn("max_connections=96", drill.recovery_options(CONTROL, retained))
            for invalid in ({}, dict(settings, max_connections=-1), dict(settings, max_connections="96"),
                            dict(settings, max_connections=True)):
                out.write_text(json.dumps({"pg_controldata": CONTROL, "settings": invalid}))
                with self.assertRaises(ValueError):
                    drill.load_settings(out)

    def test_source_free_restore_launch_keeps_post_backup_ceiling_increase(self):
        # Backup checkpoint has 48; archived WAL later requires source's 96.
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        config = {"Config": {"Env": environment}, "Image": "test", "RecoverySettings": {"max_connections": 96}}
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "pg_controldata" in args:
                self.assertIn("LC_ALL=C", args)
                return CONTROL
            if "SELECT NOT pg_is_in_recovery()" in args:
                self.assertGreater(kwargs["timeout"], 0)
                self.assertLessEqual(kwargs["timeout"], 1800)
                return "t"
            if "sh" in args:
                return "komika"
            if "du" in args:
                return "123 /restore"
            return ""
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), \
                patch.object(drill.subprocess, "run") as run:
            run.return_value.returncode = 0
            result = drill.restore(config, Path(directory), "backup", "2026-10-11T00:00:00Z", True)
        launch = next(c for c in calls if "pg_ctl" in c and "start" in c)
        self.assertIn("max_connections=96", " ".join(launch))
        self.assertNotIn("max_connections=48", " ".join(launch))
        self.assertEqual(result["restored_bytes"], 123)
        self.assertTrue(result["throwaway_container"].startswith("komika-pitr-"))
        self.assertEqual(result["throwaway_network"], result["throwaway_container"] + "-network")
        self.assertFalse(any(c[:2] == ["docker", "compose"] for c in calls))

    def test_rehearsal_restore_cleanup_refuses_unlabeled_resources(self):
        environment = dict(KOMIKA_REHEARSAL_PROJECT=PROJECT, KOMIKA_R2_PREFIX="rehearsal/test",
                           KOMIKA_PRODUCTION_R2_PREFIX="production", KOMIKA_R2_ENDPOINT="example.com",
                           KOMIKA_R2_BUCKET="bucket", KOMIKA_R2_KEY="key", KOMIKA_R2_SECRET="secret")
        config = {"Config": {"Env": [f"{k}={v}" for k, v in environment.items()]}, "Image": "test"}
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "restore" in args:
                raise RuntimeError("failed restore")
            if "inspect" in args:
                return '[{"Config":{"Labels":{}},"Labels":{}}]'
            return ""
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), \
                patch.object(drill.subprocess, "run") as run:
            run.return_value.returncode = 0
            with self.assertRaises(ValueError):
                drill.restore(config, Path(directory), "backup", None, False)
        self.assertFalse(any(c[:2] == ["docker", "rm"] or c[:3] == ["docker", "network", "rm"] for c in calls))
        for created in (c for c in calls if c[:2] == ["docker", "create"] or c[:3] == ["docker", "network", "create"]):
            self.assertIn("cc.komiq.rehearsal=" + PROJECT, created)
            # restore-cleanup and verify-restored find both by this directory label.
            self.assertTrue(any(a.startswith("cc.komiq.rehearsal.restore-target=") for a in created))

    def test_hosted_selection_and_no_rehearsal_through_direct_entry_points(self):
        with patch.dict(os.environ, HOSTED):
            selected = drill.compose()
            self.assertIn("/synthetic/hosted.env", selected)
            self.assertEqual(selected[-2:], ["-p", "komika"])
            drill.guard("komika", {})
            with self.assertRaises(ValueError):  # A container that carries rehearsal variables.
                drill.guard("komika", {"KOMIKA_REHEARSAL_PROJECT": PROJECT})
        # Neither a rehearsal variable nor the rehearsal file can select anything here.
        for selection in ({"KOMIKA_REHEARSAL_PROJECT": PROJECT}, {"KOMIKA_REHEARSAL_ROOT": ""},
                          {"KOMIKA_REHEARSAL_NO_REPO": "1"},
                          {"KOMIKA_COMPOSE_FILE": str(drill.KIT / "rehearsal-compose.yml")}):
            with self.subTest(selection=selection), patch.dict(os.environ, selection):
                with self.assertRaisesRegex(ValueError, "only through rehearsal.sh"):
                    drill.compose()
                with self.assertRaisesRegex(ValueError, "only through rehearsal.sh"):
                    drill.guard("komika", {})

    def test_direct_main_refuses_rehearsal_before_any_command(self):
        rehearsal_file = str(drill.KIT / "rehearsal-compose.yml")
        for selection in ({"KOMIKA_REHEARSAL_PROJECT": PROJECT, "KOMIKA_COMPOSE_FILE": rehearsal_file,
                           "KOMIKA_COMPOSE_ENV_FILE": "/synthetic/rehearsal.env"},
                          {"KOMIKA_COMPOSE_FILE": rehearsal_file}, {"KOMIKA_REHEARSAL_ROOT": "/synthetic"}):
            for argv in (["drill.py", "/synthetic/target", "--hosted-recovery"],
                         ["drill.py", "/synthetic/target", "--restore-only", "--keep", "--hosted-recovery"],
                         ["drill.py", "record-settings", "--out", "/synthetic/out.json", "--hosted-recovery"]):
                with self.subTest(selection=sorted(selection), argv=argv[1]), patch.dict(os.environ, selection), \
                        patch("sys.argv", argv), patch.object(drill, "command") as command, \
                        patch.object(drill.subprocess, "run") as run:
                    with self.assertRaisesRegex(ValueError, "only through rehearsal.sh"):
                        drill.main()
                    self.assertEqual(command.call_count + run.call_count, 0)
        # The hosted path still needs its explicit flag.
        with patch.dict(os.environ, HOSTED), patch("sys.argv", ["drill.py", "/synthetic/target"]), \
                patch.object(drill, "HOSTED_RECOVERY", False), patch.object(drill, "command") as command:
            with self.assertRaisesRegex(ValueError, "--hosted-recovery"):
                drill.main()
            self.assertEqual(command.call_count, 0)

    def test_restore_only_stopped_source_mounts_are_fenced(self):
        selected = {"name": "trial", "services": {"postgres": {"image": "test", "environment": {}, "volumes": []}}}
        stopped = {"Mounts": [{"Source": "/private/live-data"}], "State": {"Running": False}}
        with patch.dict(os.environ, {"DOCKER_HOST": "unix:///test", "DOCKER_CONTEXT": ""}), \
                patch.object(drill, "command", side_effect=[json.dumps(selected), "stopped-container", json.dumps([stopped])]) as command:
            config = drill.source(restore_only=True)
        self.assertEqual(config["Mounts"], stopped["Mounts"])
        self.assertIn("-a", command.call_args_list[1].args[0])

    def test_keep_retains_only_owned_network_and_lower_resource_limits(self):
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "pg_controldata" in args:
                return CONTROL
            if "SELECT NOT pg_is_in_recovery()" in args:
                return "t"
            if "sh" in args:
                return "komika"
            if "du" in args:
                return "12345 /restore"
            return ""
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), \
                patch.object(drill.subprocess, "run") as run, patch.dict(os.environ, {
                    "KOMIKA_RESTORE_MEMORY": "768m", "KOMIKA_RESTORE_CPUS": "0.5", "KOMIKA_RESTORE_PIDS": "96"}):
            run.return_value.returncode = 0
            drill.restore({"Image": "test", "Config": {"Env": environment}}, Path(directory), "backup", None, True)
        create = next(c for c in calls if c[:2] == ["docker", "create"])
        self.assertIn("--memory=768m", create)
        self.assertIn("--memory-swap=768m", create)
        self.assertIn("--cpus=0.5", create)
        self.assertIn("--pids-limit=96", create)
        # C8: explicit bounded logging on the restore container, before the image name.
        for option in ("--log-driver=json-file", "--log-opt=max-size=10m", "--log-opt=max-file=3"):
            self.assertEqual(create[:create.index("test")].count(option), 1, option)
        network = next(c for c in calls if c[:3] == ["docker", "network", "create"])[-1]
        self.assertEqual(create[create.index("--network") + 1], network)
        self.assertFalse(any(c[:2] == ["docker", "rm"] or c[:3] == ["docker", "network", "rm"] for c in calls))

    def test_restore_only_after_source_removed_writes_final_json_and_keeps(self):
        full = {"label": "full-0", "type": "full", "prior": None, "timestamp": {"start": -9, "stop": -5},
                "archive": {"start": "0001", "stop": "0001"},
                "info": {"size": 990, "delta": 990, "repository": {"size": 400, "delta": 400}}}
        backup = {"label": "pre-T", "type": "incr", "prior": "full-0", "timestamp": {"start": 0, "stop": 1},
                  "archive": {"start": "0002", "stop": "0003"},
                  "info": {"size": 1000, "delta": 30, "repository": {"delta": 321}}}
        info = {"status": {"code": 0}, "backup": [full, backup], "archive": [{"min": "0001", "max": "0003"}]}
        segment = "archive/komika/18-1/0000000100000000/000000010000000000000002-" + "ab" * 20 + ".zst"
        listing = {"archive": {"type": "path"}, "backup/komika/backup.info": {"type": "file", "size": 77},
                   "backup/komika/pre-T/pg_data/base/1/2.zst": {"type": "file", "size": (12 << 20) + 1},
                   segment: {"type": "file", "size": 5 << 20}, "archive/komika/archive.info": {"type": "file", "size": 10},
                   "backup/komika/pre-T": {"type": "path"}, "archive/komika/18-1/broken": {"type": "file"}}
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "info" in args:
                return json.dumps([info])
            if "repo-ls" in args:
                return json.dumps(listing)
            raise AssertionError("must not require a running source/container: " + str(args))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            target = root / "restore"
            out = root / "report.json"
            # Valid retained record: source ceiling raised AFTER the backup.
            settings = root / "settings.json"
            settings.write_text(json.dumps({"pg_controldata": CONTROL,
                "settings": {"max_connections": 96, "max_worker_processes": 8, "max_wal_senders": 10,
                             "max_prepared_transactions": 0, "max_locks_per_transaction": 64}}))
            timings = {"restore_seconds": 2.0, "recovery_seconds": 3.0, "restored_bytes": 12345}
            # The source container and its volume were removed: container() is None.
            fake = FakeRehearsal(root)
            with patch.object(drill, "REHEARSAL", fake), \
                    patch.object(drill, "command", side_effect=command), patch.object(drill, "restore", return_value=timings) as restore, \
                    patch("sys.stdout", new_callable=io.StringIO) as stdout:
                drill.run(target, True, "2026-10-11T00:00:00Z", True, out, settings, (["before"], ["after"]))
                final_line = stdout.getvalue().splitlines()[-1]
                summary = json.loads(final_line)
            self.assertEqual(out.read_text(), final_line + "\n")
            self.assertEqual(summary, json.loads(out.read_text()))
            # C7: the budget numbers stay in the record. Per backup the logical (database) and
            # repository sizes; from the listing the WAL archive bytes and the whole prefix.
            self.assertEqual(summary, dict(
                backup_label="pre-T", backup_type="incr", repository_size_bytes=721,
                wal_archive_min="0001", wal_archive_max="0003",
                backups=[dict(label="full-0", type="full", prior=None, timestamp_start=-9, timestamp_stop=-5,
                              wal_start="0001", wal_stop="0001", database_size_bytes=990, database_delta_bytes=990,
                              repository_size_bytes=400, repository_delta_bytes=400),
                         dict(label="pre-T", type="incr", prior="full-0", timestamp_start=0, timestamp_stop=1,
                              wal_start="0002", wal_stop="0003", database_size_bytes=1000, database_delta_bytes=30,
                              repository_size_bytes=None, repository_delta_bytes=321)],
                repository_usage=dict(objects=4, bytes=77 + (12 << 20) + 1 + (5 << 20) + 10,
                                      archive_objects=2, archive_bytes=(5 << 20) + 10,
                                      backup_objects=2, backup_bytes=77 + (12 << 20) + 1, wal_segments=1,
                                      # 12 MiB + 1 byte: three parts, plus the requests that open and complete the upload.
                                      multipart_objects=1, write_requests_floor=1 + 5 + 1 + 1),
                **timings))
            self.assertIn(f"repository_bytes={77 + (12 << 20) + 1 + (5 << 20) + 10} repository_objects=4 "
                          f"wal_archive_bytes={(5 << 20) + 10}", stdout.getvalue())
            self.assertEqual(restore.call_args.args[4], True)
            self.assertEqual(restore.call_args.args[6], (["before"], ["after"]))  # The marker expectations.
            # The disk gate saw 1.3 x the selected backup's database size, before the target was made.
            self.assertEqual(fake.gates, [("restore", 0, 1300, "")])
            self.assertEqual(restore.call_args.args[0]["Image"], safety.IMAGE)
            self.assertEqual(restore.call_args.args[0]["RecoverySettings"]["max_connections"], 96)
            self.assertEqual(restore.call_args.args[1], target)
            self.assertFalse(any("inspect" in c for c in calls))
            self.assertFalse(any("compose" in c for c in calls))
            readers = [c for c in calls if "run" in c]
            self.assertEqual(len(readers), 2)
            self.assertEqual(len(calls), 2)
            self.assertTrue(all("--mount" not in c and "--volume" not in c and "-v" not in c for c in readers))
            # Only the rehearsal's own bridge; each reader is named and removed in a finally.
            self.assertTrue(all("--network=" + PROJECT + "-network" in c for c in readers))
            self.assertFalse(any("--network=bridge" in c for c in readers))
            self.assertTrue(all("cc.komiq.rehearsal=" + PROJECT in c for c in readers))
            self.assertEqual([c[c.index("--name") + 1] for c in readers], fake.names)
            self.assertEqual(fake.removed, fake.names)
            # Targets outside the checked root never reach a repository reader.
            calls.clear()
            for bad in (root.parent / "elsewhere", root / "nested" / "deep", settings):
                with self.subTest(target=bad), patch.object(drill, "REHEARSAL", FakeRehearsal(root)), \
                        patch.object(drill, "command", side_effect=command), self.assertRaises(ValueError):
                    drill.run(bad, True, None, False, None, None)
            self.assertEqual(calls, [])

    def test_gate_refusal_and_missing_size_stop_before_the_target_exists(self):
        backup = {"label": "pre-T", "type": "full", "timestamp": {"stop": 1}, "info": {"size": 5000, "repository": {"delta": 1}}}
        info = {"status": {"code": 0}, "backup": [backup], "archive": []}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()

            class Full(FakeRehearsal):
                def disk_gate(self, step, docker_bytes=0, root_bytes=0, advice=""):
                    super().disk_gate(step, docker_bytes, root_bytes, advice)
                    raise ValueError("disk gate: refused" + advice)

            for source, expected in ((True, "stop-source, destroy-source-volumes"), (False, None)):
                fake = Full(root, source=source)
                with self.subTest(source=source), patch.object(drill, "REHEARSAL", fake), \
                        patch.object(drill, "command", return_value=json.dumps([info])), patch.object(drill, "restore") as restore:
                    with self.assertRaisesRegex(ValueError, "disk gate") as refusal:
                        drill.run(root / "target", True, None, False, None, None)
                    restore.assert_not_called()
                    self.assertFalse((root / "target").exists())
                    self.assertEqual(fake.gates[0][:3], ("restore", 0, 6500))
                    self.assertEqual(expected is not None and expected in str(refusal.exception), expected is not None)
                    self.assertEqual("stop-source" in str(refusal.exception), source)
            del backup["info"]["size"]
            with patch.object(drill, "REHEARSAL", FakeRehearsal(root)), patch.object(drill, "command", return_value=json.dumps([info])), \
                    patch.object(drill, "restore") as restore, self.assertRaisesRegex(ValueError, "no database size"):
                drill.run(root / "target", True, None, False, None, None)
            restore.assert_not_called()
            self.assertFalse((root / "target").exists())

    def test_marker_expectations_fail_the_restore_and_remove_its_container(self):
        drill.check_expectations(["before-pitr"], (["before-pitr"], ["after-pitr"]))
        drill.check_expectations([], ((), ()))
        for markers, expect, message in ((["before-pitr", "after-pitr"], (["before-pitr"], ["after-pitr"]), "must be absent but present: after-pitr"),
                                         ([], (["before-pitr"], []), "expected but missing: before-pitr"),
                                         (["a"], (["a", "b"], ["a"]), "expected but missing: b; must be absent but present: a")):
            with self.subTest(markers=markers), self.assertRaisesRegex(RuntimeError, message):
                drill.check_expectations(markers, expect)
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "pg_controldata" in args:
                return CONTROL
            if "SELECT NOT pg_is_in_recovery()" in args:
                return "t"
            if any("FROM pg_database" in a for a in args):
                return "1"
            if any("FROM operator_markers" in a for a in args):
                return "before-pitr\nafter-pitr\n"
            if "sh" in args:
                return "komika"
            if "du" in args:
                return "1 /restore"
            return ""
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), \
                patch.object(drill.subprocess, "run") as run:
            run.return_value.returncode = 0
            with self.assertRaisesRegex(RuntimeError, "marker expectation failed"):
                drill.restore({"Image": "test", "Config": {"Env": environment}}, Path(directory), "backup",
                              "2026-10-11T00:00:00Z", True, None, (["before-pitr"], ["after-pitr"]))
            # --keep never keeps a restore that failed its check.
            self.assertEqual(calls[-2][:4], ["docker", "rm", "-f", "-v"])
            self.assertEqual(calls[-1][:3], ["docker", "network", "rm"])
            calls.clear()
            result = drill.restore({"Image": "test", "Config": {"Env": environment}}, Path(directory), "backup",
                                   "2026-10-11T00:00:00Z", True, None, (["before-pitr", "after-pitr"], ["never"]))
            self.assertEqual(result["operator_markers"], ["before-pitr", "after-pitr"])

    def test_a_restore_without_application_tables_says_so_and_removes_its_container(self):
        environment = [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "pg_controldata" in args:
                return CONTROL
            if "SELECT NOT pg_is_in_recovery()" in args:
                return "t"
            if any("FROM pg_database" in a for a in args):
                return "0"
            if any("FROM work" in a for a in args):
                raise RuntimeError("docker operation failed (exit 1); inspect container logs")
            if "sh" in args:
                return "komika"
            return ""
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "command", side_effect=command), \
                patch.object(drill.subprocess, "run") as run:
            run.return_value.returncode = 0
            with self.assertRaisesRegex(RuntimeError, "has no application tables .*back up after `import`"):
                drill.restore({"Image": "test", "Config": {"Env": environment}}, Path(directory), "backup", None, True)
            self.assertEqual(calls[-2][:4], ["docker", "rm", "-f", "-v"])
            self.assertEqual(calls[-1][:3], ["docker", "network", "rm"])

    def test_hosted_readers_keep_the_default_bridge_and_no_rehearsal_identity(self):
        config = {"Config": {"Env": [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]}, "Image": "test"}
        args = drill.throwaway_arguments(config, "true")
        self.assertIn("--network=bridge", args)
        self.assertNotIn("--name", args)
        self.assertNotIn("--label", args)
        # A rehearsal environment can never ride the hosted reader.
        config["Config"]["Env"].append("KOMIKA_REHEARSAL_PROJECT=" + PROJECT)
        with self.assertRaisesRegex(ValueError, "only through rehearsal.sh"):
            drill.throwaway_arguments(config, "true")
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "REHEARSAL", FakeRehearsal(directory)):
            with self.assertRaises(ValueError):  # A rehearsal reader always has its project name.
                drill.throwaway_arguments(config, "true")
            args = drill.throwaway_arguments(config, "true", name=PROJECT + "-meta-00000000")
            self.assertIn("--network=" + PROJECT + "-network", args)
            self.assertNotIn("--network=bridge", args)
            # C8: a reader's log is bounded explicitly, whatever the daemon's default is.
            image = args.index("test")
            for option in ("--log-driver=json-file", "--log-opt=max-size=10m", "--log-opt=max-file=3",
                           "--memory=512m", "--memory-swap=512m", "--cpus=1", "--pids-limit=128"):
                self.assertEqual(args[:image].count(option), 1, option)

    def test_a_reader_is_removed_and_an_interrupted_one_is_waited_for(self):
        config = {"Config": {"Env": [f"{key}={value}" for key, value in ENVIRONMENT.items()]}, "Image": "test"}
        for outcome, settle in (("output", False), (RuntimeError("the reader failed"), False), (KeyboardInterrupt(), True)):
            for stream in (False, True):
                with self.subTest(outcome=outcome, stream=stream), tempfile.TemporaryDirectory() as directory:
                    fake = FakeRehearsal(directory)
                    effect = outcome if isinstance(outcome, BaseException) else None
                    with patch.object(drill, "REHEARSAL", fake), patch.object(drill, "command", side_effect=effect, return_value=outcome), \
                            patch.object(drill.subprocess, "run", side_effect=effect if not isinstance(effect, RuntimeError) else None) as run:
                        run.return_value.returncode = 0
                        try:
                            drill.throwaway(config, "true", stream=stream)
                        except (RuntimeError, KeyboardInterrupt):
                            pass
                    # Killed client: the create request may still reach the daemon.
                    expected = settle if not (stream and isinstance(outcome, RuntimeError)) else False
                    self.assertEqual((fake.removed, fake.settled), (fake.names, [expected]))

    def test_a_failed_restore_reports_what_pgbackrest_said(self):
        failed = subprocess.CompletedProcess([], 31, "", "P00  ERROR: [031]: option 'archive-async' not valid for command 'restore'\n"
                                             "INFO: restore command begin 2.59.3: --repo1-s3-key=<redacted>\n")
        with patch.object(drill.subprocess, "run", return_value=failed):
            with self.assertRaises(RuntimeError) as plain:
                drill.command(["docker", "inspect", "x"])
            with self.assertRaises(RuntimeError) as explained:
                drill.command(["docker", "exec", "x"], explain=True)
        self.assertEqual(str(plain.exception), "docker operation failed (exit 31); inspect container logs")
        self.assertEqual(str(explained.exception), "docker operation failed (exit 31); inspect container logs\n"
                         "  pgBackRest P00  ERROR: [031]: option 'archive-async' not valid for command 'restore'")

    def test_rehearsal_commands_use_the_checked_container_and_minimal_environment(self):
        entry = {"Id": "c" * 64, "Mounts": []}
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "REHEARSAL", FakeRehearsal(directory, entry)), \
                patch.object(drill.subprocess, "run") as run:
            run.return_value.returncode = 0
            run.return_value.stdout = "48\n"
            run.return_value.stderr = ""
            self.assertEqual(drill.sql("SHOW max_connections"), "48")
            argv = run.call_args.args[0]
            self.assertEqual(argv[:5], ["docker", "exec", "--user", "postgres", "c" * 64])
            self.assertEqual(run.call_args.kwargs["env"], FakeRehearsal.child_env)
            self.assertNotIn("compose", argv)
            drill.wait_archive("0" * 24)
            self.assertTrue(all(call.kwargs["env"] == FakeRehearsal.child_env for call in run.call_args_list))
        with tempfile.TemporaryDirectory() as directory, patch.object(drill, "REHEARSAL", FakeRehearsal(directory)), \
                patch.object(drill.subprocess, "run") as run:
            with self.assertRaises(ValueError):  # No running, checked container: no command at all.
                drill.sql("SELECT 1")
            self.assertEqual(run.call_count, 0)

    def test_markers_live_in_their_own_database_never_in_the_application_database(self):
        statements = []
        def live(*args):
            statements.append((args[args.index("-d") + 1], args[-1]))
            return "0" if "pg_database" in args[-1] else ""
        with patch.object(drill, "live", side_effect=live):
            drill.marker_database()
        self.assertEqual(statements[0][0], "komika")
        self.assertTrue(statements[0][1].startswith("SELECT count(*) FROM pg_database"))
        self.assertEqual(statements[1], ("postgres", "CREATE DATABASE komika_pitr_drill TEMPLATE template0"))
        self.assertEqual(statements[2][0], "komika_pitr_drill")
        self.assertIn("CREATE TABLE IF NOT EXISTS markers", statements[2][1])
        # Nothing but one catalog read ever runs in the application database.
        self.assertEqual([s for d, s in statements if d == "komika" and not s.startswith("SELECT")], [])
        statements.clear()
        with patch.object(drill, "live", side_effect=lambda *args: (statements.append(args[args.index("-d") + 1]), "1")[1]):
            drill.marker_database()
        self.assertNotIn("postgres", statements)  # Existing database is not created again.

    def test_restore_limits_are_validated_before_they_reach_a_command_line(self):
        for name, value in (("KOMIKA_RESTORE_CPUS", "1 --privileged"), ("KOMIKA_RESTORE_MEMORY", "1g --volume=/:/x"),
                            ("KOMIKA_RESTORE_PIDS", "-1"), ("KOMIKA_RESTORE_MEMORY", "")):
            with self.subTest(name=name, value=value), patch.dict(os.environ, {name: value}), self.assertRaises(ValueError):
                drill.restore_limits()
        self.assertEqual(drill.restore_limits(), {"MEMORY": "1g", "CPUS": "1", "PIDS": "128"})
        with tempfile.TemporaryDirectory() as directory:
            lowered = FakeRehearsal(directory, values={"KOMIKA_RESTORE_MEMORY": "768m", "KOMIKA_RESTORE_PIDS": "96"})
            with patch.object(drill, "REHEARSAL", lowered):
                self.assertEqual(drill.restore_limits(), {"MEMORY": "768m", "CPUS": "1", "PIDS": "96"})
            for values in ({"KOMIKA_RESTORE_MEMORY": "16g"}, {"KOMIKA_RESTORE_CPUS": "8"}, {"KOMIKA_RESTORE_PIDS": "4096"}):
                with self.subTest(values=values), patch.object(drill, "REHEARSAL", FakeRehearsal(directory, values=values)), \
                        self.assertRaises(ValueError):
                    drill.restore_limits()
        config = {"Config": {"Env": [f"{key}=fake" for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET")]}, "Image": "test"}
        with patch.object(drill, "command") as command, self.assertRaises(ValueError):
            drill.restore(config, Path("/synthetic/a,readonly=false"), "backup", None, False)
        self.assertEqual(command.call_count, 0)

    def test_hosted_target_fence_inspects_volumes_mounts_docker_root_and_aliases(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            output, production, docker_root = base / "outputs", base / "production-volume", base / "docker-data"
            for path in (output, production, docker_root):
                path.mkdir()
            containers = [{"Name": "/komika-server-1", "Config": {"Labels": {}}, "Mounts": [{"Source": str(production)}]}]
            volumes = [{"Name": "komika_server-data", "Labels": {}, "Mountpoint": str(production)}]
            calls = []
            def command(args, **kwargs):
                calls.append(args)
                if args[:2] == ["docker", "info"]:
                    return json.dumps(str(docker_root))
                if args[:2] == ["docker", "ps"]:
                    return "production-container" if containers else ""
                if args[:2] == ["docker", "inspect"]:
                    return json.dumps(containers)
                if args[:3] == ["docker", "volume", "ls"]:
                    return "production-volume" if volumes else ""
                if args[:3] == ["docker", "volume", "inspect"]:
                    return json.dumps(volumes)
                raise AssertionError(args)
            with patch.dict(os.environ, HOSTED), patch.object(drill, "command", side_effect=command):
                self.assertEqual(drill.fenced_target(output / "new"), output / "new")
                self.assertTrue(any(c[:3] == ["docker", "volume", "inspect"] for c in calls))
                for parent in (production, docker_root, docker_root / "volumes", base):
                    with self.subTest(parent=parent.name), self.assertRaises(ValueError):
                        drill.fenced_target(parent / "new")
                    self.assertFalse((parent / "new").exists())
                # A retained production volume still fences when ALL containers are gone,
                # and a container mount still fences when the volume list is empty.
                kept_volumes = list(volumes)
                volumes.clear()
                with self.assertRaises(ValueError):
                    drill.fenced_target(production / "new")
                volumes.extend(kept_volumes)
                containers.clear()
                with self.assertRaises(ValueError):
                    drill.fenced_target(production / "new")
                (output / "alias").symlink_to(production, target_is_directory=True)
                (output / "existing").mkdir()
                for target in (output / "alias" / "new", output / "nested" / "new", output / "existing", Path("relative")):
                    with self.subTest(target=target.name), self.assertRaises(ValueError):
                        drill.fenced_target(target)
                # A bind-mount alias of the production volume: same inode, another path.
                real = os.stat
                with patch.object(safety.os, "stat", side_effect=lambda p, *a, **k: real(str(production) if p == str(output) else p, *a, **k)), \
                        self.assertRaisesRegex(ValueError, "bind-mount alias"):
                    safety.host_fence(output, safety.protected_paths(str(docker_root), [], volumes), uid=os.getuid(),
                                      require_owner=False, require_marker=False, plain_parent=True,
                                      stat=safety.os.stat, platform="darwin")

    def test_report_missing_info_size_is_null_and_chain_deltas_sum(self):
        backup = {"label": "full", "type": "full"}
        self.assertIsNone(drill.report({"backup": [backup]}, backup, {})["repository_size_bytes"])
        self.assertIsNone(drill.report({"backup": [backup]}, backup, {})["wal_archive_min"])
        backup["info"] = {"repository": {"delta": 100}}
        incr = {"info": {"repository": {"delta": 20}}}
        self.assertEqual(drill.report({"backup": [backup, incr]}, backup, {})["repository_size_bytes"], 120)
        # Without a listing nothing is invented; numbers that are no integers are null.
        report = drill.report({"backup": [backup, {"info": {"size": "12", "repository": {"size": True}}}]}, backup, {})
        self.assertIsNone(report["repository_usage"])
        self.assertEqual([(b["database_size_bytes"], b["repository_size_bytes"], b["repository_delta_bytes"])
                          for b in report["backups"]], [(None, None, 100), (None, None, None)])

    def test_usage_counts_only_files_and_bounds_the_writes_from_below(self):
        chunk = drill.UPLOAD_CHUNK
        self.assertEqual(chunk, 5 << 20)
        self.assertEqual(drill.usage({}), dict(objects=0, bytes=0, archive_objects=0, archive_bytes=0, backup_objects=0,
                                               backup_bytes=0, wal_segments=0, multipart_objects=0, write_requests_floor=0))
        listing = {"backup/komika/a": {"type": "file", "size": chunk}, "backup/komika/b": {"type": "file", "size": chunk + 1},
                   "backup/komika/c": {"type": "file", "size": 3 * chunk}, "backup/komika": {"type": "path"},
                   "archive/komika/18-1/0000000100000000/000000010000000000000001-" + "0f" * 20: {"type": "file", "size": 0},
                   "archive/komika/18-1/0000000100000000/000000010000000000000001.partial": {"type": "file", "size": 1},
                   "archive/komika/18-1/00000002.history": {"type": "file", "size": 2},
                   "archivex/not-wal": {"type": "file", "size": 4}, "link": {"type": "link", "size": 9},
                   "negative": {"type": "file", "size": -1}, "text": {"type": "file", "size": "7"}}
        held = drill.usage(listing)
        self.assertEqual((held["objects"], held["bytes"]), (7, 5 * chunk + 1 + 0 + 1 + 2 + 4))
        self.assertEqual((held["archive_objects"], held["archive_bytes"], held["wal_segments"]), (3, 3, 1))
        self.assertEqual((held["backup_objects"], held["backup_bytes"]), (3, 5 * chunk + 1))
        # One PUT up to one part; else every part and the two requests around them.
        self.assertEqual((held["multipart_objects"], held["write_requests_floor"]), (2, 1 + (2 + 2) + (3 + 2) + 4))
        # The list form of the same listing, and a listing that is no listing.
        as_list = [dict(entry, name=name) for name, entry in listing.items()]
        self.assertEqual(drill.usage(as_list), held)
        for bad in ("text", 7, None):
            with self.subTest(listing=bad), self.assertRaises(ValueError):
                drill.usage(bad)


if __name__ == "__main__":
    unittest.main()
