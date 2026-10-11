"""The in-image fence: pgbackrest.sh and `safety.py --container`.

Run by tests.sh against the kit and by image-test.sh inside the image against
the image's own copies in /opt/komika-postgres (KOMIKA_IMAGE_TEST=1).
"""
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import drill
import hosted
import safety

IN_IMAGE = os.environ.get("KOMIKA_IMAGE_TEST") == "1"
PROJECT = "komika-pg-rehearsal-test"
REHEARSAL = {"KOMIKA_REHEARSAL_PROJECT": PROJECT, "KOMIKA_PRODUCTION_R2_PREFIX": "komika"}
# Every pgBackRest command line the kit sends in a rehearsal (rehearsal.py,
# drill.py, hosted.py, postgresql.conf and the restore_command check).
SENT = (
    ["archive-push", "pg_wal/000000010000000000000001"],
    ["archive-push", "pg_wal/00000002.history"],
    ["archive-push", "pg_wal/000000010000000000000002.00000028.backup"],
    ["archive-push", "pg_wal/000000010000000000000003.partial"],
    ["--archive-async=n", "archive-get", "000000010000000000000001", "/run/komika/drill-000000010000000000000001"],
    ["stanza-create"], ["check"], ["info"], ["--output=text", "info"], ["--output=json", "info"],
    ["--recurse", "--output=json", "repo-ls"], ["--type=full", "backup"], ["--type=incr", "backup"],
    ["stop"], ["--force", "stanza-delete"], ["restore"],
    ["--log-level-stderr=detail", "--set=20261011-120000F", "restore"],
    ["--log-level-stderr=detail", "--set=20261011-120000F_20261012-030000I", "--type=time",
     "--target=2026-10-11 12:34:56.123456+00", "--target-action=promote", "restore"],
)
# pgBackRest reports a command line it cannot parse before it has read its log
# settings: on stdout, with its default `P00  ERROR:` prefix. Later failures (no
# cluster, no backup) come through the configured stderr log without that prefix.
UNPARSED = r"(?m)^P00 +ERROR|not valid for command|invalid option|unknown option|requires? (?:an? )?(?:argument|option|parameter)"
REFUSED = (
    ["--reset-repo1-path", "info"], ["--no-config", "info"], ["--reset-config", "info"], ["--config=/tmp/other.conf", "info"],
    ["--config-path=/tmp", "info"], ["--config-include-path=/tmp", "info"], ["--repo1-path=/komika", "info"],
    ["--repo1-path=/komika", "--force", "stanza-delete"], ["--repo2-path=/komika", "check"], ["--repo=2", "info"],
    ["--repo1-s3-bucket=production", "check"], ["--repo1-s3-endpoint=example.com", "check"],
    ["--repo1-s3-key=x", "check"], ["--repo1-type=posix", "--repo1-path=/var/lib/postgresql", "info"],
    ["--no-repo1-bundle", "--type=full", "backup"], ["--reset-repo1-retention-full", "--type=full", "backup"],
    ["--repo1-retention-full=1", "--type=full", "backup"], ["--stanza=other", "info"], ["--pg1-path=/x", "check"],
    ["--lock-path=/tmp", "stop"], ["--spool-path=/tmp", "archive-push", "pg_wal/000000010000000000000001"],
    ["expire"], ["--repo1-retention-full=1", "expire"], ["repo-rm", "/"], ["--recurse", "repo-rm", "/"],
    ["--recurse", "--repo1-path=/komika", "repo-rm", "/"], ["repo-put", "x"], ["repo-get", "backup/komika/backup.info"],
    ["annotate"], ["verify"], ["server"], ["server-ping"], ["start"], ["version"], ["help"], [], ["--output=json"],
    ["stanza-delete", "--force"], ["stanza-upgrade"], ["--type=diff", "backup"], ["--type=full", "--type=incr", "backup"],
    ["info", "--repo1-path=/komika"], ["check", "extra"], ["archive-push", "/etc/passwd"],
    ["archive-push", "pg_wal/../../etc/passwd"], ["archive-push", "pg_wal/000000010000000000000001", "extra"],
    ["--archive-async=n", "archive-get", "000000010000000000000001", "/var/lib/postgresql/18/docker/pg_wal/x"],
    ["archive-get", "../x", "/run/komika/drill-000000010000000000000001"],
    ["--delta", "restore"], ["--force", "restore"], ["--pg1-path=/var/lib/postgresql/18/docker", "restore"],
    ["--set=latest --repo1-path=/komika", "restore"], ["--target=2026-10-11 12:00:00+02", "--type=time", "restore"],
    ["--type=immediate", "restore"], ["--target-action=shutdown", "restore"], ["--target-timeline=2", "restore"],
    ["--recovery-option=restore_command=/bin/true", "restore"], ["--tablespace-map-all=/x", "restore"],
    ["--link-all", "restore"], ["--db-include=komika", "restore"], ["--archive-mode=off", "restore"],
    ["--archive-async=n", "restore"],
    # PostgreSQL refuses `Z` in recovery_target_time: only the numeric UTC form is sent.
    ["--target=2026-10-11T12:34:56.123456Z", "--type=time", "restore"], ["--target=2026-10-11 12:34:56+00", "--type=time", "restore"],
)


class ImageIdentityTests(unittest.TestCase):
    @unittest.skipUnless(IN_IMAGE, "runs inside the image only (image-test.sh)")
    def test_the_images_own_environment_is_what_the_live_container_check_allows(self):
        """Fix round 5 (C2): a live rehearsal container may carry the model's names and
        the names the image sets, nothing else. Here the image itself says which
        names and values it sets: every one must be allowed with its real value,
        and the image must set no other PG*, PGBACKREST_* or KOMIKA_* name."""
        for name, pattern in safety.IMAGE_ENVIRONMENT.items():
            with self.subTest(name=name):
                self.assertRegex(os.environ.get(name, ""), "^(?:" + pattern + ")$")
        stray = sorted(name for name in os.environ
                       if name.startswith(("PG", "KOMIKA_", "LD_", "PYTHON"))
                       and name not in safety.IMAGE_ENVIRONMENT
                       and name not in ("PGDATA", "KOMIKA_IMAGE_TEST", "PYTHONDONTWRITEBYTECODE", "PYTHONPATH"))
        self.assertEqual(stray, [])
        self.assertEqual(os.environ.get("PGDATA"), "/var/lib/postgresql/18/docker")  # The model sets the same value.

    @unittest.skipUnless(IN_IMAGE, "runs inside the image only (image-test.sh)")
    def test_code_under_test_is_the_images_own_copy(self):
        own = Path(safety.IMAGE_DIRECTORY)
        self.assertEqual(Path(hosted.__file__).resolve().parent, own)
        self.assertEqual(Path(safety.__file__).resolve().parent, own)
        self.assertEqual(safety.KIT, own)
        # The mounted kit and the image carry the same bytes: the identity check, from inside.
        mounted = Path(__file__).resolve().parent
        self.assertNotEqual(mounted, own)
        for name in safety.IMAGE_FILES:
            with self.subTest(file=name):
                self.assertEqual(hashlib.sha256((own / name).read_bytes()).hexdigest(),
                                 hashlib.sha256((mounted / name).read_bytes()).hexdigest(),
                                 "the image was not built from the mounted kit; rebuild it")

    def test_image_files_exist_and_scripts_are_executable(self):
        for name in safety.IMAGE_FILES:
            self.assertTrue((safety.KIT / name).is_file(), name)
            if name.endswith(".sh"):
                self.assertTrue(os.access(safety.KIT / name, os.X_OK), name)


class FenceTests(unittest.TestCase):
    """`safety.py --container` as pgbackrest.sh calls it, with a private config."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.config = self.base / "pgbackrest.conf"
        self.configure("rehearsal/test")

    def configure(self, prefix):
        self.config.write_text(f"[global]\nrepo1-path=/{prefix}\n\n[komika]\npg1-path=/x\n")

    def fence(self, arguments, **environment):
        base = {"PATH": os.environ.get("PATH", os.defpath), "PGBACKREST_CONFIG": str(self.config)}
        return subprocess.run([sys.executable, "-B", str(safety.KIT / "safety.py"), "--container", *arguments],
                              env=dict(base, **environment), capture_output=True, text=True)

    def test_every_command_line_the_kit_sends_passes(self):
        for arguments in SENT:
            with self.subTest(arguments=arguments):
                result = self.fence(arguments, **REHEARSAL)
                self.assertEqual((result.returncode, result.stderr), (0, ""))
                safety.repository_arguments(arguments, REHEARSAL)

    def test_everything_else_is_refused_in_a_rehearsal(self):
        for arguments in REFUSED:
            with self.subTest(arguments=arguments):
                result = self.fence(arguments, **REHEARSAL)
                self.assertEqual(result.returncode, 1, arguments)
                self.assertIn("rehearsal refused: pgBackRest", result.stderr)
                self.assertIn("allowlist", result.stderr)
                with self.assertRaises(ValueError):
                    safety.repository_arguments(arguments, REHEARSAL)

    def test_allowed_command_is_refused_against_any_other_prefix(self):
        for prefix in ("komika", "komika/child", "rehearsal", "production", "rehearsal/test/../../komika"):
            with self.subTest(prefix=prefix):
                self.configure(prefix)
                result = self.fence(["--force", "stanza-delete"], **REHEARSAL)
                self.assertEqual(result.returncode, 1)
                self.assertIn("rehearsal refused", result.stderr)
        self.configure("rehearsal/test")
        for change in ({"KOMIKA_PRODUCTION_R2_PREFIX": "rehearsal/test"}, {"KOMIKA_PRODUCTION_R2_PREFIX": "rehearsal"},
                       {"KOMIKA_PRODUCTION_R2_PREFIX": ""}, {"KOMIKA_REHEARSAL_PROJECT": "komika"},
                       {"PGBACKREST_REPO1_PATH": "/komika"}, {"PGBACKREST_STANZA": "other"}):
            with self.subTest(change=change):
                self.assertEqual(self.fence(["check"], **dict(REHEARSAL, **change)).returncode, 1)
        self.config.unlink()  # No configuration: nothing can be proven about the prefix.
        self.assertEqual(self.fence(["check"], **REHEARSAL).returncode, 1)

    def test_hosted_mode_is_untouched_and_host_use_is_refused(self):
        for arguments in (["expire"], ["--repo1-path=/komika", "info"], ["repo-ls"]):
            self.assertEqual(self.fence(arguments).returncode, 0)  # No rehearsal variable: no fence.
        result = subprocess.run([sys.executable, "-B", str(safety.KIT / "safety.py"), "up"], capture_output=True, text=True,
                                env={"PATH": os.environ.get("PATH", os.defpath)})
        self.assertEqual(result.returncode, 1)
        self.assertIn("no host command", result.stderr)


class WrapperTests(unittest.TestCase):
    """pgbackrest.sh must ask the fence before it runs the real binary."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        (self.base / "bin").mkdir()
        fake = self.base / "bin" / "pgbackrest"
        fake.write_text('#!/bin/sh\nprintf "%s\\n" "$PGBACKREST_CONFIG" "$@" > "$TRACE"\n')
        fake.chmod(0o755)
        self.trace = self.base / "trace"

    def wrapper(self, arguments, **environment):
        if self.trace.exists():
            self.trace.unlink()
        base = {"PATH": str(self.base / "bin") + os.pathsep + os.environ.get("PATH", os.defpath), "TRACE": str(self.trace)}
        return subprocess.run(["/bin/sh", str(safety.KIT / "pgbackrest.sh"), *arguments], env=dict(base, **environment),
                              capture_output=True, text=True)

    def test_wrapper_fixes_config_and_stanza_outside_a_rehearsal(self):
        result = self.wrapper(["--output=json", "info"], PGBACKREST_CONFIG="/tmp/caller-chosen.conf")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(),
                         ["/run/komika/pgbackrest.conf", "--stanza=komika", "--output=json", "info"])

    def test_wrapper_never_reaches_pgbackrest_when_the_fence_refuses(self):
        # Fails when pgbackrest.sh loses its safety.py call and when safety.main()
        # stops fencing: in both cases the fake binary below would run.
        for arguments in REFUSED:
            with self.subTest(arguments=arguments):
                result = self.wrapper(arguments, **REHEARSAL)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("rehearsal refused", result.stderr)
                self.assertFalse(self.trace.exists(), "pgbackrest ran although the fence must refuse")
        # An allowed command still needs the private configuration with the rehearsal prefix.
        if not Path("/run/komika/pgbackrest.conf").exists():
            result = self.wrapper(["check"], **REHEARSAL)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(self.trace.exists())

    @unittest.skipUnless(IN_IMAGE and os.getuid() == 0, "needs /run/komika: runs inside the image (image-test.sh)")
    def test_wrapper_passes_the_kits_commands_with_the_rehearsal_prefix(self):
        directory = Path("/run/komika")
        directory.mkdir(parents=True, exist_ok=True)
        config = directory / "pgbackrest.conf"
        self.addCleanup(lambda: config.exists() and config.unlink())
        config.write_text("[global]\nrepo1-path=/rehearsal/test\n")
        for arguments in SENT:
            with self.subTest(arguments=arguments):
                result = self.wrapper(arguments, **REHEARSAL)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.trace.read_text().splitlines(), ["/run/komika/pgbackrest.conf", "--stanza=komika", *arguments])
        config.write_text("[global]\nrepo1-path=/komika\n")
        for arguments in SENT:
            with self.subTest(production=arguments):
                self.assertNotEqual(self.wrapper(arguments, **REHEARSAL).returncode, 0)
                self.assertFalse(self.trace.exists())


class RealParserTests(unittest.TestCase):
    """The allowlist must list what pgBackRest itself accepts.

    Found by the first repository drill: `--archive-async=n restore` was on the
    list and in the code, and pgBackRest 2.59.3 answers `[031]: option
    'archive-async' not valid for command 'restore'`. No fake can know that.
    """

    def test_the_recovery_target_is_utc_with_a_numeric_offset(self):
        for given, sent in (("2026-10-11T12:34:56.123456Z", "2026-10-11 12:34:56.123456+00"),
                            ("2026-10-11T12:34:56Z", "2026-10-11 12:34:56.000000+00"),
                            ("2026-10-11T12:34:56.5+00:00", "2026-10-11 12:34:56.500000+00"),
                            ("2026-10-11T14:34:56.123+02:00", "2026-10-11 12:34:56.123000+00")):
            with self.subTest(given=given):
                self.assertEqual(drill.recovery_target(given), sent)
                safety.repository_arguments(["--set=20261011-120000F", "--type=time", f"--target={sent}",
                                             "--target-action=promote", "restore"], REHEARSAL)
        with self.assertRaises(ValueError):
            drill.recovery_target("2026-10-11T12:34:56")

    @unittest.skipUnless(IN_IMAGE, "needs the real postgres binary: runs inside the image (image-test.sh)")
    def test_the_real_postgres_accepts_the_recovery_target_the_kit_sends(self):
        # `postgres -C` runs the same check as a server start, without a cluster.
        with tempfile.TemporaryDirectory() as directory:
            identity = {}
            if os.getuid() == 0:
                import shutil
                identity = {"user": "postgres", "group": "postgres", "extra_groups": []}
                shutil.chown(directory, "postgres", "postgres")
            (Path(directory) / "postgresql.conf").write_text("")

            def check(value):
                return subprocess.run(["postgres", "-D", directory, "-C", "recovery_target_time", "-c",
                                       "recovery_target_time=" + value], capture_output=True, text=True, timeout=60, **identity)

            sent = drill.recovery_target("2026-10-11T12:34:56.123456Z")
            result = check(sent)
            self.assertEqual((result.returncode, result.stdout.strip()), (0, sent), result.stderr)
            # What the kit sent before the first drill: refused at start-up.
            result = check("2026-10-11T12:34:56.123456Z")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('invalid value for parameter "recovery_target_time"', result.stderr)

    @unittest.skipUnless(IN_IMAGE, "needs the real pgbackrest binary: runs inside the image (image-test.sh)")
    def test_the_real_pgbackrest_accepts_every_command_line_the_kit_sends(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            for name in ("repo", "pg", "spool", "lock"):
                (base / name).mkdir()
            # pgBackRest refuses to run as root: run it as the image's postgres user.
            identity = {}
            if os.getuid() == 0:
                import shutil
                identity = {"user": "postgres", "group": "postgres", "extra_groups": []}
                for path in (base, *base.iterdir()):
                    shutil.chown(path, "postgres", "postgres")
            config = base / "pgbackrest.conf"
            # The options of hosted.configure(), on a local empty repository.
            config.write_text(f"""[global]
repo1-type=posix
repo1-path={base}/repo
repo1-bundle=y
repo1-block=y
compress-type=zst
archive-async=y
spool-path={base}/spool
lock-path={base}/lock
archive-push-queue-max=1GiB
start-fast=y
repo1-retention-full=2
process-max=2
archive-timeout=180
log-level-console=off
log-level-stderr=info
log-level-file=off

[komika]
pg1-path={base}/pg
pg1-socket-path={base}
pg1-user=postgres
""")
            environment = {"PATH": os.environ.get("PATH", os.defpath), "PGBACKREST_CONFIG": str(config)}
            for arguments in SENT:
                with self.subTest(arguments=arguments):
                    # PostgreSQL runs archive_command in its data directory.
                    result = subprocess.run(["pgbackrest", "--stanza=komika", *arguments], env=environment,
                                            cwd=base / "pg", capture_output=True, text=True, timeout=120, **identity)
                    # Without a cluster and a backup most commands fail later, for
                    # another reason; none may fail as an invalid command line.
                    said = result.stdout + result.stderr
                    self.assertNotRegex(said, UNPARSED)
                    self.assertNotIn("root user", said)
                    self.assertNotIn(result.returncode, (23, 30, 31, 33, 34, 35, 36, 37, 43, 96), said[-400:])
            # The check itself can tell: the line the drill found is refused by pgBackRest.
            result = subprocess.run(["pgbackrest", "--stanza=komika", "--archive-async=n", "restore"], env=environment,
                                    cwd=base, capture_output=True, text=True, timeout=120, **identity)
            self.assertEqual(result.returncode, 31, result.stderr)
            self.assertRegex(result.stdout + result.stderr, UNPARSED)
            self.assertIn("option 'archive-async' not valid for command 'restore'", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
