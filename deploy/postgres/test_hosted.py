import datetime as dt
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import hosted as h

PATH = os.environ.get("PATH", "")


def epoch(value):
    return int(dt.datetime.fromisoformat(value).replace(tzinfo=dt.timezone.utc).timestamp())


class HostedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name, value in (("DATA", self.root / "data"), ("STATE", self.root / "state"),
                            ("CONFIG", self.root / "config")):
            p = patch.object(h, name, value)
            p.start()
            self.addCleanup(p.stop)
        p = patch.dict(os.environ, {}, clear=True)
        p.start()
        self.addCleanup(p.stop)
        self.exec = patch.object(h.os, "execvp")
        self.execute = self.exec.start()
        self.addCleanup(self.exec.stop)
        # Health reads the archiver over the socket and the volume's free space.
        self.facts = {"in_recovery": False, "archived_count": 9, "failed_count": 0, "last_archived_wal": "0000000100000000000000AA",
                      "last_archived_time": 0, "last_failed_wal": None, "last_failed_time": None, "ready": 0}
        # A primary without a standby, unless a test says otherwise.
        self.replication = {"in_recovery": False, "timeline": 1, "senders": [], "slots": [], "receiver": None}
        for name, value in (("archiver_facts", lambda timeout=10: self.facts), ("free_bytes", lambda: 100 << 30),
                            ("replication_facts", lambda timeout=10: self.replication)):
            p = patch.object(h, name, side_effect=value)
            p.start()
            self.addCleanup(p.stop)

    def configured(self):
        for key, value in zip(h.BACKUP_VARS, ("https://example.r2.cloudflarestorage.com", "komika-backup", "rehearsal/test", "testkey", "testsecret")):
            os.environ[key] = value
        return h.configure()

    def existing(self):
        h.DATA.mkdir()
        (h.DATA / "PG_VERSION").write_text("18")

    def info(self, code=0, backups=None, **extra):
        return json.dumps([dict(name="komika", status={"code": code}, backup=backups or [], **extra)])

    def test_existing_cluster_no_repository_probe(self):
        self.existing()
        with patch.object(h, "pgbr", side_effect=AssertionError("must not probe")):
            h.start(True)
        self.assertIn("archive_mode=on", self.execute.call_args.args[1])

    def test_incompatible_and_nonempty_cluster_refused(self):
        self.existing()
        (h.DATA / "PG_VERSION").write_text("17")
        with self.assertRaises(ValueError):
            h.start(True)
        (h.DATA / "PG_VERSION").unlink()
        (h.DATA / "unrelated").write_text("preserve")
        with self.assertRaises(ValueError):
            h.start(False)
        self.assertEqual((h.DATA / "unrelated").read_text(), "preserve")

    def test_local_no_backup_initializes_without_archiving(self):
        self.assertFalse(h.configure())
        with patch.object(h, "init_cluster") as init:
            h.start(False)
        init.assert_called_once()
        self.assertIn("archive_mode=off", self.execute.call_args.args[1])

    def test_partial_configuration_fails_even_existing(self):
        self.existing()
        os.environ["KOMIKA_R2_BUCKET"] = "test"
        with self.assertRaises(ValueError):
            h.configure()
        self.execute.assert_not_called()

    def test_config_private_and_credentials_removed(self):
        self.assertTrue(self.configured())
        self.assertEqual(h.CONFIG.stat().st_mode & 0o777, 0o600)
        self.assertIn("repo1-s3-key-secret=testsecret", h.CONFIG.read_text())
        self.assertIn("archive-push-queue-max=1GiB", h.CONFIG.read_text())
        self.assertFalse(any(k in os.environ for k in h.BACKUP_VARS))

    def test_config_injection_refused(self):
        self.configured()
        for name, value in (("KOMIKA_R2_SECRET", "secret\nnew-option=y"),
                            ("KOMIKA_R2_PREFIX", "../live"), ("KOMIKA_R2_ENDPOINT", "http://example.com")):
            with patch.dict(os.environ, dict(zip(h.BACKUP_VARS, ("example.com", "backup-bucket", "test", "key", "secret"))) | {name: value}):
                with self.assertRaises(ValueError):
                    h.configure()

    def test_valid_backup_restores_before_postgres(self):
        calls = []
        def pgbr(*args, **kwargs):
            calls.append(args)
            if "info" in args:
                return self.info(backups=[{"label": "valid"}])
            (h.DATA / "PG_VERSION").write_text("18")
        with patch.object(h, "pgbr", side_effect=pgbr), patch.object(h, "init_cluster") as init:
            h.start(True)
        self.assertEqual(calls[-1], ("restore",))
        init.assert_not_called()
        self.execute.assert_called_once()

    def test_a_rehearsal_never_restores_on_start_and_names_the_explicit_restore(self):
        """C3: an empty data directory and a rehearsal repository that already
        holds a backup. Hosted start restores (test above); a rehearsal refuses."""
        for init_new in ("0", "1", None):
            for production in ("komika", ""):  # Any KOMIKA_REHEARSAL_* name makes it a rehearsal.
                with self.subTest(init_new=init_new, production=production):
                    calls = []
                    def pgbr(*args, **kwargs):
                        calls.append(args)
                        return self.info(backups=[{"label": "20261012-000000F"}])
                    environment = {"KOMIKA_REHEARSAL_PROJECT": "komika-pg-rehearsal-test",
                                   "KOMIKA_PRODUCTION_R2_PREFIX": production}
                    if init_new is not None:
                        environment["KOMIKA_PG_INIT_NEW"] = init_new
                    with patch.dict(os.environ, environment), patch.object(h, "pgbr", side_effect=pgbr), \
                            patch.object(h, "init_cluster") as init:
                        with self.assertRaisesRegex(ValueError, "never restores implicitly.*"
                                                    r"`rehearsal.sh restore --target NEWDIR --json-out FILE"):
                            h.start(True)
                    self.assertEqual([call for call in calls if "restore" in call], [])
                    self.assertTrue(all("info" in call for call in calls), calls)
                    init.assert_not_called()
                    self.execute.assert_not_called()
                    self.assertEqual(list(h.DATA.iterdir()), [])  # Nothing was written into the volume.
        # A rehearsal still starts its existing cluster and still initializes on an empty repository.
        with patch.dict(os.environ, {"KOMIKA_REHEARSAL_PROJECT": "komika-pg-rehearsal-test", "KOMIKA_PG_INIT_NEW": "1"}), \
                patch.object(h, "pgbr", side_effect=[self.info(1), "{}"]), patch.object(h, "init_cluster") as init:
            h.start(True)
            init.assert_called_once()

    def test_restore_failure_never_initializes_or_executes(self):
        def pgbr(*args, **kwargs):
            if "info" in args:
                return self.info(backups=[{"label": "valid"}])
            (h.DATA / "partial").write_text("retain")
            raise subprocess.CalledProcessError(7, "fake restore")
        with patch.object(h, "pgbr", side_effect=pgbr), patch.object(h, "init_cluster") as init:
            with self.assertRaises(subprocess.CalledProcessError):
                h.start(True)
        init.assert_not_called()
        self.execute.assert_not_called()
        self.assertTrue((h.DATA / "partial").exists())

    def test_empty_reachable_repository_requires_explicit_flag(self):
        with patch.object(h, "pgbr", side_effect=[self.info(1), "{}"]), patch.object(h, "init_cluster") as init:
            with self.assertRaises(ValueError):
                h.start(True)
            init.assert_not_called()
        os.environ["KOMIKA_PG_INIT_NEW"] = "1"
        with patch.object(h, "pgbr", side_effect=[self.info(1), "{}"]), patch.object(h, "init_cluster") as init:
            h.start(True)
            init.assert_called_once()

    def test_auth_network_and_corrupt_repository_fail_closed(self):
        os.environ["KOMIKA_PG_INIT_NEW"] = "1"
        outcomes = [subprocess.CalledProcessError(39, "auth"), subprocess.CalledProcessError(50, "network"),
                    self.info(99), "not json", self.info(2), self.info(0),
                    self.info(backups=[{"label": "valid"}], repo=[{"status": {"code": 99}}])]
        for outcome in outcomes:
            with self.subTest(outcome=outcome), patch.object(h, "pgbr", side_effect=outcome if isinstance(outcome, Exception) else None, return_value=outcome), patch.object(h, "init_cluster") as init:
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    h.start(True)
                init.assert_not_called()
                self.execute.assert_not_called()

    def test_missing_stanza_with_objects_cannot_init(self):
        os.environ["KOMIKA_PG_INIT_NEW"] = "1"
        with patch.object(h, "pgbr", side_effect=[self.info(1), '{"backup/komika/file":{"size":1}}']):
            with self.assertRaises(ValueError):
                h.start(True)

    def test_missing_repository_errors_require_authenticated_empty_probe(self):
        os.environ["KOMIKA_PG_INIT_NEW"] = "1"
        for index, answers in enumerate(([self.info(1), subprocess.CalledProcessError(55, "repo-ls")],
                                         [subprocess.CalledProcessError(44, "info")])):
            with self.subTest(answers=answers), patch.object(h, "DATA", self.root / f"case-{index}"), \
                    patch.object(h, "pgbr", side_effect=answers), \
                    patch.object(h, "repository_empty", return_value=True) as probe, \
                    patch.object(h, "init_cluster") as init:
                h.start(True)
                probe.assert_called_once()
                init.assert_called_once()

    def test_missing_repository_nonempty_or_probe_failure_never_initializes(self):
        os.environ["KOMIKA_PG_INIT_NEW"] = "1"
        for result in (False, OSError("unreachable"), ValueError("bad XML")):
            with self.subTest(result=result), patch.object(h, "pgbr", side_effect=[self.info(1), subprocess.CalledProcessError(55, "repo-ls")]), \
                    patch.object(h, "repository_empty", side_effect=result if isinstance(result, Exception) else None, return_value=result) as probe, \
                    patch.object(h, "init_cluster") as init:
                with self.assertRaises((ValueError, OSError, subprocess.CalledProcessError)):
                    h.start(True)
                init.assert_not_called()
                self.execute.assert_not_called()
                probe.assert_called_once()  # Old fail-closed path never reached the authenticated probe.

    def test_nonmissing_repository_errors_do_not_probe_or_initialize(self):
        os.environ["KOMIKA_PG_INIT_NEW"] = "1"
        # Positive missing-path control distinguishes error classification from
        # blanket failure; then nonmissing auth/network errors must not probe.
        with patch.object(h, "pgbr", side_effect=subprocess.CalledProcessError(55, "info")), \
                patch.object(h, "repository_empty", return_value=True) as probe:
            self.assertIsNone(h.backup_info())
            probe.assert_called_once()
        for code in (39, 50, 99):
            with self.subTest(code=code), patch.object(h, "pgbr", side_effect=[self.info(1), subprocess.CalledProcessError(code, "repo-ls")]), \
                    patch.object(h, "repository_empty") as probe, patch.object(h, "init_cluster") as init:
                with self.assertRaises(subprocess.CalledProcessError):
                    h.start(True)
                probe.assert_not_called()
                init.assert_not_called()

    def test_authenticated_empty_probe_checks_response_and_signs_request(self):
        self.configured()
        empty = '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>komika-backup</Name><Prefix>rehearsal/test/</Prefix><KeyCount>0</KeyCount><IsTruncated>false</IsTruncated></ListBucketResult>'
        # Signature independently computed with OpenSSL HMAC from a literal
        # canonical request, not from hosted.py's signing implementation.
        fixed = dt.datetime(2026, 10, 11, 12, 0, tzinfo=dt.timezone.utc)
        with patch.object(h.urllib.request, "build_opener") as build, patch.object(h.dt, "datetime") as clock:
            clock.now.return_value = fixed
            response = build.return_value.open.return_value.__enter__.return_value
            response.status = 200
            response.read.return_value = empty.encode()
            self.assertTrue(h.repository_empty())
            request = build.return_value.open.call_args.args[0]
            self.assertIn("AWS4-HMAC-SHA256 Credential=testkey/", request.get_header("Authorization"))
            self.assertEqual(request.get_header("Authorization"),
                "AWS4-HMAC-SHA256 Credential=testkey/20261011/auto/s3/aws4_request, "
                "SignedHeaders=host;x-amz-content-sha256;x-amz-date, "
                "Signature=9f9cb4a46c4a6c7f29b10e1b1d8128d5cbc60528165529f2114ce5ab03a22cab")
            self.assertIn("prefix=rehearsal%2Ftest%2F", request.full_url)
            response.read.return_value = empty.replace('<KeyCount>0', '<KeyCount>1').encode()
            self.assertFalse(h.repository_empty())
            for invalid in (empty.replace('rehearsal/test/', 'production/'), '<Error/>', 'not XML'):
                response.read.return_value = invalid.encode()
                with self.assertRaises((ValueError, h.ET.ParseError)):
                    h.repository_empty()
            response.status = 403
            with self.assertRaises(ValueError):
                h.repository_empty()

    def test_root_only_listing_requires_authenticated_empty_probe(self):
        for empty in (True, False):
            with self.subTest(empty=empty), patch.object(h, "pgbr", side_effect=[self.info(1), '{".":{"type":"path"}}']), \
                    patch.object(h, "repository_empty", return_value=empty) as probe:
                if empty:
                    self.assertIsNone(h.backup_info())
                else:
                    with self.assertRaises(ValueError):
                        h.backup_info()
                probe.assert_called_once()

    def test_rehearsal_startup_guard_refuses_production_identity_and_prefix(self):
        complete = dict(zip(h.BACKUP_VARS, ("example.com", "test-bucket", "rehearsal/test", "testkey", "testsecret")))
        for project, production, prefix in (("komika", "production", "rehearsal/test"),
                ("komika-pg-rehearsal-test", "rehearsal/test", "rehearsal/test"),
                ("komika-pg-rehearsal-test", "/rehearsal//test/", "rehearsal/test"),
                ("komika-pg-rehearsal-test", "rehearsal", "rehearsal/test"),
                ("komika-pg-rehearsal-test", "rehearsal/test/child", "rehearsal/test"),
                ("komika-pg-rehearsal-test", "", "rehearsal/test")):
            environment = dict(complete, KOMIKA_REHEARSAL_PROJECT=project,
                               KOMIKA_PRODUCTION_R2_PREFIX=production, KOMIKA_R2_PREFIX=prefix)
            with self.subTest(environment=environment), patch.dict(os.environ, environment):
                with self.assertRaises(ValueError):
                    h.configure()
        self.assertFalse(h.CONFIG.exists())

    def test_rehearsal_commands_recheck_prefix_after_configuration(self):
        os.environ.update(KOMIKA_REHEARSAL_PROJECT="komika-pg-rehearsal-test", KOMIKA_PRODUCTION_R2_PREFIX="production")
        self.configured()
        self.assertNotIn("KOMIKA_R2_PREFIX", os.environ)
        # Every command line hosted.py itself sends in a rehearsal passes.
        sent = (("--output=json", "info"), ("--recurse", "--output=json", "repo-ls"), ("restore",),
                ("check",), ("--type=full", "backup"), ("--type=incr", "backup"))
        for args in sent:
            with self.subTest(args=args), patch.object(h, "run") as run:
                h.pgbr(*args)
                self.assertEqual(run.call_args.args[0], ["pgbackrest", "--stanza=komika", *args])
        # The same lines are refused once the prefixes are no longer disjoint.
        os.environ["KOMIKA_PRODUCTION_R2_PREFIX"] = "rehearsal/test"
        for args in sent + (("stanza-delete",), ("--force", "stanza-delete")):
            with patch.object(h, "run") as run, self.assertRaises(ValueError):
                h.pgbr(*args)
            run.assert_not_called()
        # Fix round 4 (F6): with a correct prefix only the allowlisted lines exist.
        os.environ["KOMIKA_PRODUCTION_R2_PREFIX"] = "production"
        for args in (("--repo1-path=/production", "check"), ("--config=/another.conf", "stanza-delete"),
                     ("--reset-repo1-path", "check"), ("--no-config", "info"), ("--reset-config", "info"),
                     ("--no-repo1-bundle", "--type=full", "backup"), ("--repo1-s3-bucket=other", "info"),
                     ("--repo1-s3-endpoint=example.com", "check"), ("--repo=2", "info"), ("--stanza=other", "info"),
                     ("--pg1-path=/x", "check"), ("--type=diff", "backup"), ("--force", "check"),
                     ("expire",), ("repo-rm", "/"), ("--recurse", "repo-rm", "/"), ("repo-put", "x"), ("repo-get", "x"),
                     ("annotate",), ("verify",), ("server",), ("start",), ("version",), ("help",), (), ("--output=json",),
                     ("info", "extra"), ("check", "--repo1-path=/production"), ("--output=json", "--output=json", "info"),
                     ("--delta", "restore"), ("--pg1-path=/x", "restore"), ("--set=../x", "restore"),
                     ("--target=2026-10-11 --repo1-path=/production", "--type=time", "restore"),
                     ("--recovery-option=restore_command=rm", "restore"), ("--db-include=x", "restore"),
                     ("--tablespace-map-all=/x", "restore"), ("--link-all", "restore")):
            with self.subTest(args=args), patch.object(h, "run") as run, self.assertRaises(ValueError):
                h.pgbr(*args)
            run.assert_not_called()
        # Outside a rehearsal nothing changes: hosted operators keep the full command.
        for key in ("KOMIKA_REHEARSAL_PROJECT", "KOMIKA_PRODUCTION_R2_PREFIX"):
            del os.environ[key]
        with patch.object(h, "run") as run:
            h.pgbr("expire")
            run.assert_called_once()

    def test_tls_verification_is_off_only_for_a_local_rehearsal_store(self):
        local = dict(zip(h.BACKUP_VARS, ("10.213.4.1", "test-bucket", "rehearsal/test", "testkey", "testsecret")),
                     KOMIKA_REHEARSAL_PROJECT="komika-pg-rehearsal-test-local", KOMIKA_PRODUCTION_R2_PREFIX="komika",
                     KOMIKA_R2_VERIFY_TLS="0")
        with patch.dict(os.environ, local):
            self.assertTrue(h.configure())
            self.assertIn("\nrepo1-storage-verify-tls=n\n", h.CONFIG.read_text())
            # The emptiness probe follows the same decision, and only after re-checking it.
            empty = ('<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>test-bucket</Name>'
                     '<Prefix>rehearsal/test/</Prefix><KeyCount>0</KeyCount><IsTruncated>false</IsTruncated></ListBucketResult>')
            with patch.object(h.urllib.request, "HTTPSHandler") as handler, patch.object(h.urllib.request, "build_opener") as build:
                response = build.return_value.open.return_value.__enter__.return_value
                response.status = 200
                response.read.return_value = empty.encode()
                self.assertTrue(h.repository_empty())
                context = handler.call_args.kwargs["context"]
                self.assertEqual(context.verify_mode, h.ssl.CERT_NONE)
                self.assertFalse(context.check_hostname)
            # A config line alone is not enough: outside the three conditions the probe refuses.
            for change in ({"KOMIKA_REHEARSAL_PROJECT": "komika-pg-rehearsal-test"}, {"KOMIKA_R2_VERIFY_TLS": "1"}):
                with self.subTest(change=change), patch.dict(os.environ, change), \
                        patch.object(h.urllib.request, "build_opener") as build, self.assertRaises(ValueError):
                    h.repository_empty()
                build.assert_not_called()
        h.CONFIG.unlink()
        # Each condition on its own is not enough; nothing is configured at all then.
        for change in ({"KOMIKA_REHEARSAL_PROJECT": "komika-pg-rehearsal-test"}, {"KOMIKA_R2_ENDPOINT": "example.com"},
                       {"KOMIKA_R2_ENDPOINT": "8.8.8.8"}, {"KOMIKA_R2_ENDPOINT": "127.0.0.1"},
                       {"KOMIKA_R2_VERIFY_TLS": "false"}, {"KOMIKA_R2_VERIFY_TLS": "no"}):
            with self.subTest(change=change), patch.dict(os.environ, dict(local, **change)):
                with self.assertRaises(ValueError):
                    h.configure()
                self.assertFalse(h.CONFIG.exists())
        # 1, empty or absent: verification stays on in a local project too.
        for value in ("1", "", None):
            environment = {k: v for k, v in dict(local, KOMIKA_R2_VERIFY_TLS=value).items() if v is not None}
            with self.subTest(value=value), patch.dict(os.environ, environment):
                self.assertTrue(h.configure())
                self.assertNotIn("verify-tls", h.CONFIG.read_text())
            h.CONFIG.unlink()

    def test_hosted_path_never_reads_the_tls_key(self):
        # No rehearsal variable: the key is not consulted, whatever it says.
        hosted = dict(zip(h.BACKUP_VARS, ("10.213.4.1", "komika-backup", "komika", "testkey", "testsecret")))
        for value in ("0", "false", "anything"):
            with self.subTest(value=value), patch.dict(os.environ, dict(hosted, KOMIKA_R2_VERIFY_TLS=value)), \
                    patch.object(h.safety, "local_s3", side_effect=AssertionError("hosted must not consult the key")):
                self.assertTrue(h.configure())
                self.assertNotIn("verify-tls", h.CONFIG.read_text())
                with patch.object(h.urllib.request, "HTTPSHandler") as handler, patch.object(h.urllib.request, "build_opener") as build:
                    build.return_value.open.return_value.__enter__.return_value.status = 500
                    with self.assertRaises(ValueError):
                        h.repository_empty()
                    context = handler.call_args.kwargs["context"]
                    self.assertEqual(context.verify_mode, h.ssl.CERT_REQUIRED)
                    self.assertTrue(context.check_hostname)
        # The hosted Compose file does not even pass the key to the container.
        compose = h.KIT.parent / "docker-compose.yml"
        if compose.is_file():  # Absent where only the rehearsal kit was copied, and in the image.
            self.assertNotIn("KOMIKA_R2_VERIFY_TLS", compose.read_text())

    def test_rehearsal_without_repository_refuses_every_repository_value(self):
        base = dict(KOMIKA_REHEARSAL_PROJECT="komika-pg-rehearsal-test", KOMIKA_REHEARSAL_NO_REPO="1")
        with patch.dict(os.environ, base), patch.object(h, "log") as log:
            self.assertFalse(h.configure())
            self.assertIn("WITHOUT backup or WAL archiving", log.call_args.args[0])
            with patch.object(h, "init_cluster"):
                h.start(False)
            self.assertIn("archive_mode=off", self.execute.call_args.args[1])
            # No private configuration exists, so no pgBackRest command can run.
            with patch.object(h, "run") as run, self.assertRaises(KeyError):
                h.pgbr("info")
            run.assert_not_called()
            with patch.object(h.sys, "argv", ["hosted.py", "job", "full"]), patch.object(h, "run") as run, \
                    self.assertRaisesRegex(ValueError, "requires complete R2"):
                h.main()
            run.assert_not_called()
        complete = dict(zip(h.BACKUP_VARS, ("example.com", "test-bucket", "rehearsal/test", "testkey", "testsecret")))
        for extra in ([dict([item]) for item in complete.items()]
                      + [complete, {"KOMIKA_PRODUCTION_R2_PREFIX": "production"}, {"KOMIKA_R2_VERIFY_TLS": "0"},
                         dict(complete, KOMIKA_PRODUCTION_R2_PREFIX="production")]):
            with self.subTest(extra=sorted(extra)), patch.dict(os.environ, dict(base, **extra)):
                with self.assertRaisesRegex(ValueError, "refuses every R2 variable"):
                    h.configure()
        for project in ("komika", "", "komika-pg-rehearsal-"):
            with self.subTest(project=project), patch.dict(os.environ, dict(base, KOMIKA_REHEARSAL_PROJECT=project)):
                with self.assertRaises(ValueError):
                    h.configure()
        # Any value other than exactly 1 keeps the full repository fence.
        for value in ("0", "true", "yes", " 1", "01"):
            with self.subTest(value=value), patch.dict(os.environ, dict(base, KOMIKA_REHEARSAL_NO_REPO=value)):
                with self.assertRaises(ValueError):
                    h.configure()
        self.assertFalse(h.CONFIG.exists())

    def test_private_cidrs_and_socket_application_rejection(self):
        h.DATA.mkdir()
        h.hba()
        default = (h.DATA / "pg_hba.conf").read_text()
        self.assertIn("local all postgres peer", default)
        self.assertNotIn("local komika", default)
        self.assertIn("local all all reject", default)
        with patch.dict(os.environ, {"KOMIKA_PG_ALLOWED_CIDRS": "172.30.21.0/24, fd00:30::/64"}):
            h.hba()
            changed = (h.DATA / "pg_hba.conf").read_text()
            self.assertIn("172.30.21.0/24 scram-sha-256", changed)
            self.assertNotIn("172.21.0.0/16", changed)
        for cidrs in ("0.0.0.0/0", "::/0", "8.8.8.0/24", "127.0.0.0/8", "169.254.0.0/16",
                      "2001:db8::/32", "172.30.21.1/24", "172.30.21.0", "10.0.0.0/7", "", "10.0.0.0/8,"):
            with self.subTest(cidrs=cidrs), patch.dict(os.environ, {"KOMIKA_PG_ALLOWED_CIDRS": cidrs}):
                with self.assertRaises(ValueError):
                    h.hba()

    def test_initdb_locale_scram_owner_and_no_password_argv(self):
        os.environ["KOMIKA_POSTGRES_PASSWORD"] = "a" * 32
        def fake_run(args, **kwargs):
            if args[0] == "initdb":
                h.DATA.mkdir()
        with patch.object(h, "run", side_effect=fake_run) as run:
            h.init_cluster()
        init = run.call_args_list[0].args[0]
        for flag in ("--locale-provider=builtin", "--builtin-locale=C.UTF-8", "--lc-collate=C", "--lc-ctype=C", "--encoding=UTF8", "--auth-host=scram-sha-256"):
            self.assertIn(flag, init)
        sql_call = run.call_args_list[2]
        self.assertIn("NOSUPERUSER", sql_call.kwargs["input"])
        self.assertIn("CREATE EXTENSION IF NOT EXISTS pg_trgm", sql_call.kwargs["input"])
        self.assertNotIn("a" * 32, str(sql_call.args))
        for granted in ("pg_ls_waldir()", "pg_ls_archive_statusdir()"):
            self.assertIn(f"GRANT EXECUTE ON FUNCTION pg_catalog.{granted} TO komika", sql_call.kwargs["input"])
        self.assertNotIn("GRANT pg_monitor", sql_call.kwargs["input"])
        self.assertLess(sql_call.kwargs["input"].index("\\connect komika"), sql_call.kwargs["input"].index("GRANT EXECUTE"))
        self.assertIn("172.21.0.0/16 scram", (h.DATA / "pg_hba.conf").read_text())

    def test_slots_sunday_full_daily_incremental_boundary(self):
        before = h.slots(epoch("2026-10-11T01:59:59"))
        at = h.slots(epoch("2026-10-11T02:00:00"))
        self.assertEqual(before["full"], epoch("2026-10-04T02:00:00"))
        self.assertEqual(at["full"], epoch("2026-10-11T02:00:00"))
        self.assertEqual(at["incr"], epoch("2026-10-10T03:00:00"))
        self.assertEqual(h.slots(epoch("2026-10-11T03:00:00"))["incr"], epoch("2026-10-11T03:00:00"))

    def test_scheduler_catchup_one_slot_and_no_double_after_restart(self):
        now = epoch("2026-10-23T10:00:00")
        with patch.object(h, "backup_info", return_value=[{"label": "valid"}]), patch.object(h, "pgbr") as pgbr, patch.object(h.time, "time", return_value=now):
            h.tick(now)
            self.assertEqual(pgbr.call_count, 3)
            self.assertEqual([c.args for c in pgbr.call_args_list], [("--type=full", "backup"), ("check",), ("--type=incr", "backup")])
            # Fresh status read simulates restart; old missed days are never replayed.
            h.tick(now)
            self.assertEqual(pgbr.call_count, 3)
        self.assertEqual(h.read_status()["incr"]["slot"], epoch("2026-10-23T03:00:00"))

    def test_scheduler_drains_slots_crossed_by_long_full_and_check(self):
        clock = [epoch("2026-10-11T02:00:00")]
        previous = h.slots(clock[0] - 1)
        h.save_status({kind: {"slot": slot} for kind, slot in previous.items()})
        calls = []
        def pgbr(*args, **kwargs):
            calls.append(args)
            if args == ("--type=full", "backup"):
                clock[0] = epoch("2026-10-11T03:10:00")
        with patch.object(h.time, "time", side_effect=lambda: clock[0]), \
                patch.object(h, "backup_info", return_value=[{}]), patch.object(h, "pgbr", side_effect=pgbr):
            h.tick()
            self.assertEqual(calls, [("--type=full", "backup"), ("check",), ("--type=incr", "backup")])
            self.assertEqual(h.read_status()["incr"]["slot"], epoch("2026-10-11T03:00:00"))
            h.tick()
            self.assertEqual(len(calls), 3)
        # A check crossing 03:00 must drain the incremental slot too.
        clock[0] = epoch("2026-10-12T02:00:00")
        def slow_check(*args, **kwargs):
            calls.append(args)
            clock[0] = epoch("2026-10-12T03:10:00")
        with patch.object(h.time, "time", side_effect=lambda: clock[0]), \
                patch.object(h, "backup_info", return_value=[{}]), patch.object(h, "pgbr", side_effect=slow_check):
            h.tick()
        self.assertEqual(calls[-2:], [("check",), ("--type=incr", "backup")])

    def test_interrupted_slot_persisted_before_job_and_not_repeated(self):
        now = epoch("2026-10-11T04:00:00")
        due = h.slots(now)
        def crash(*args, **kwargs):
            self.assertEqual(h.read_status()["full"]["slot"], due["full"])
            raise KeyboardInterrupt()
        with patch.object(h, "backup_info", return_value=[{}]), patch.object(h, "pgbr", side_effect=crash):
            with self.assertRaises(KeyboardInterrupt):
                h.tick(now)
        with patch.object(h, "backup_info", return_value=[{}]), patch.object(h, "pgbr") as pgbr:
            h.tick(now)
        self.assertNotIn(("--type=full", "backup"), [c.args for c in pgbr.call_args_list])
        self.assertIn("full interrupted", h.read_status()["last_error"])

    def test_scheduler_failure_remains_visible_and_never_bootstraps(self):
        self.configured()
        now = epoch("2026-10-11T04:00:00")
        with patch.object(h, "backup_info", return_value=None), patch.object(h, "pgbr") as pgbr:
            h.tick(now)
        self.assertEqual([c.args for c in pgbr.call_args_list], [("check",)])
        self.assertFalse(h.health(now))
        self.assertIn("failed", h.read_status()["last_error"])

    def test_health_ages_boundaries_errors_and_future_clock(self):
        self.configured()
        now = epoch("2026-10-11T04:00:00")
        status = {k: {"success": now} for k in ("full", "incr", "check")}
        h.save_status(status)
        self.assertTrue(h.health(now))
        for kind, age in (("incr", 26 * 3600), ("full", 8 * 86400), ("check", 26 * 3600)):
            status[kind]["success"] = now - age
            h.save_status(status)
            self.assertTrue(h.health(now))
            status[kind]["success"] -= 1
            h.save_status(status)
            self.assertFalse(h.health(now))
            status[kind]["success"] = now
        status["last_error"] = "failed"
        h.save_status(status)
        self.assertFalse(h.health(now))
        status["last_error"] = ""
        h.save_status(status)
        self.assertFalse(h.health(now - 1))

    def healthy(self, now):
        self.configured()
        h.save_status({k: {"success": now, "backup_start": now - 60} for k in ("full", "incr", "check")})
        self.facts["last_archived_time"] = now
        self.assertTrue(h.health(now))

    def test_health_dropped_wal_since_latest_full_start_is_a_gap(self):
        now = epoch("2026-10-11T04:00:00")
        self.healthy(now)
        record = h.STATE / h.DROPPED
        record.write_text("2026-10-11T03:58:59.999999Z 0000000100000000000000A0\n")  # Before the full started.
        self.assertTrue(h.health(now))
        # pgBackRest's start has whole seconds: a drop in that same second stays open.
        for line in ("2026-10-11T03:59:00.000000Z", "2026-10-11T03:59:00.500000Z", "2026-10-11T03:59:00Z",
                     "2026-10-11T03:59:30Z"):
            with self.subTest(line=line):
                record.write_text(line + " 0000000100000000000000A1\n")
                self.assertFalse(h.health(now))
                self.assertIn("dropped WAL since the latest recorded full: PITR has a gap (backup.sh full)",
                              h.health_problems(now))
        status = h.read_status()
        del status["full"]["backup_start"]  # No pgBackRest start recorded: any drop stays a gap.
        h.save_status(status)
        record.write_text("2026-10-01T00:00:00Z -\n")
        self.assertFalse(h.health(now))

    def test_health_archiver_failing_fifteen_minutes_and_disk_reserve(self):
        now = epoch("2026-10-11T04:00:00")
        self.healthy(now)
        self.facts.update(last_failed_time=now - 10, last_archived_time=now - 15 * 60)
        self.assertTrue(h.health(now))
        self.facts["last_archived_time"] = now - 15 * 60 - 1
        self.assertFalse(h.health(now))
        self.facts.update(last_archived_time=None)
        self.assertFalse(h.health(now))
        self.facts.update(last_archived_time=now, last_failed_time=None)
        with patch.object(h, "free_bytes", return_value=h.DISK_RESERVE - 1):
            self.assertFalse(h.health(now))
        with patch.object(h, "archiver_facts", side_effect=subprocess.CalledProcessError(2, "psql")):
            self.assertFalse(h.health(now))

    def test_health_stalled_archiver_backlog_without_a_failure(self):
        now = epoch("2026-10-11T04:00:00")
        self.healthy(now)
        # Idle: no .ready file, however old the last success.
        self.facts.update(last_archived_time=now - 86400, ready=0, oldest_ready=None)
        self.assertTrue(h.health(now))
        # A segment that just became ready after a quiet day is not a stall.
        self.facts.update(ready=1, oldest_ready=now - 5)
        self.assertTrue(h.health(now))
        self.facts.update(oldest_ready=now - 15 * 60 - 1)
        self.assertFalse(h.health(now))
        self.assertIn("WAL waits to be archived with no progress for 15 minutes", h.health_problems(now))
        # Progress within 15 minutes is lag, not a stall.
        self.facts.update(last_archived_time=now - 60)
        self.assertTrue(h.health(now))
        # Never archived: the oldest .ready file decides.
        self.facts.update(last_archived_time=None, oldest_ready=now - 60)
        self.assertTrue(h.health(now))
        self.facts.update(oldest_ready=now - 15 * 60 - 1)
        self.assertFalse(h.health(now))
        self.assertIn("oldest_ready", h.FACTS)

    def test_monitoring_grant_retries_quietly_skips_recovery_and_never_raises(self):
        refused = subprocess.CalledProcessError(2, "psql")
        with patch.object(h, "query", side_effect=[refused, refused, "f\n"]) as query, \
                patch.object(h.time, "sleep") as sleep, patch.object(h, "log") as log:
            self.assertTrue(h.monitoring_grant())
        self.assertEqual(sleep.call_count, 2)
        self.assertEqual(log.call_count, 1)
        sql = query.call_args.args[0]
        for granted in ("pg_ls_waldir()", "pg_ls_archive_statusdir()"):
            self.assertIn(f"GRANT EXECUTE ON FUNCTION pg_catalog.{granted} TO komika", sql)
        self.assertNotIn("pg_monitor", sql)
        self.assertIn("IF NOT pg_is_in_recovery()", sql)
        with patch.object(h, "query", return_value="t\n"), patch.object(h, "log") as log:
            self.assertFalse(h.monitoring_grant())
        self.assertIn("recovery", log.call_args.args[0])
        with patch.object(h, "query", side_effect=subprocess.CalledProcessError(3, "psql")), \
                patch.object(h.time, "sleep") as sleep, patch.object(h, "log") as log:
            self.assertFalse(h.monitoring_grant())
        sleep.assert_not_called()
        self.assertEqual(log.call_count, 1)
        with patch.object(h, "query", side_effect=refused), patch.object(h.time, "sleep"), \
                patch.object(h.time, "monotonic", side_effect=[0, 5, 2000]), patch.object(h, "log") as log:
            self.assertFalse(h.monitoring_grant(deadline=1800))
        self.assertEqual(log.call_count, 1)

    def test_status_reports_gap_recoverable_window_and_never_zero_for_unreadable(self):
        now = epoch("2026-10-14T12:00:00")
        full = {"label": "F1", "type": "full", "timestamp": {"start": epoch("2026-10-11T02:00:00"), "stop": epoch("2026-10-11T02:30:00")}}
        incr = {"label": "I1", "type": "incr", "timestamp": {"start": epoch("2026-10-13T03:00:00"), "stop": epoch("2026-10-13T03:05:00")}}
        full2 = {"label": "F2", "type": "full", "timestamp": {"start": epoch("2026-10-14T02:00:00"), "stop": epoch("2026-10-14T02:40:00")}}
        info = {"name": "komika", "status": {"code": 0, "message": "ok"}, "backup": [incr, full],
                "archive": [{"min": "000000010000000000000001", "max": "0000000100000000000000AA"}]}
        self.configured()
        h.save_status({k: {"success": now - 3600} for k in ("full", "incr", "check")})
        self.facts.update(last_archived_time=now - 30)
        with patch.object(h, "repository_info", return_value=info):
            report = h.report_status(now)
            self.assertIs(report["gap_open"], False)
            self.assertEqual(report["recoverable_from"], "2026-10-11T02:30:00Z")
            # Archive completion time is not a recovery bound: reported on its own.
            self.assertIsNone(report["recoverable_until"])
            self.assertTrue(any(u.startswith("recoverable_until: not known without replaying WAL")
                                for u in report["unavailable"]))
            self.assertEqual(report["archive_last_success"], "2026-10-14T11:59:30Z")
            self.assertEqual(report["archive_last_wal"], "0000000100000000000000AA")
            self.assertIs(report["archiver"]["stalled"], False)
            self.assertEqual(report["health_problems"], [])
            self.assertEqual(report["latest_backup"]["label"], "I1")
            self.assertEqual(report["latest_full_backup"]["label"], "F1")
            self.assertEqual(report["dropped_wal"], {"events_recorded": 0, "last": None})
            (h.STATE / h.DROPPED).write_text("2026-10-13T10:00:00Z 0000000100000000000000A0\n")
            report = h.report_status(now)
            self.assertIs(report["gap_open"], True)
            self.assertIsNone(report["recoverable_from"])
            self.assertIsNone(report["recoverable_until"])
            self.assertTrue(any(u.startswith("recoverable_from: no full backup") for u in report["unavailable"]))
            info["backup"].append(full2)
            report = h.report_status(now)
            self.assertIs(report["gap_open"], False)
            self.assertEqual(report["recoverable_from"], "2026-10-14T02:40:00Z")
            # A drop in the same second as that full's (whole-second) start stays open.
            (h.STATE / h.DROPPED).write_text("2026-10-14T02:00:00.250000Z 0000000100000000000000A0\n")
            report = h.report_status(now)
            self.assertIs(report["gap_open"], True)
            self.assertIsNone(report["recoverable_from"])
            (h.STATE / h.DROPPED).write_text("2026-10-14T01:59:59.999999Z 0000000100000000000000A0\n")
            self.assertIs(h.report_status(now)["gap_open"], False)
        self.assertIn("never across dropped WAL", report["pitr"])
        self.assertEqual(json.loads(json.dumps(report)), report)
        # Unreadable: null plus a reason, never 0.
        with patch.object(h, "archiver_facts", side_effect=subprocess.CalledProcessError(2, "psql")), \
                patch.object(h, "repository_info", side_effect=subprocess.CalledProcessError(82, "pgbackrest")):
            report = h.report_status(now)
        self.assertIsNone(report["archiver"])
        self.assertIsNone(report["latest_backup"])
        self.assertIsNone(report["ready_files"])
        self.assertEqual(report["spool_bytes"], 0)  # Present and empty: a real zero.
        reasons = " ".join(report["unavailable"])
        for field in ("archiver:", "backups: pgbackrest info failed (exit 82)", "ready_files:", "recoverable_from:"):
            self.assertIn(field, reasons)
        self.assertNotIn("testsecret", json.dumps(report))
        # --local never asks the repository; the gap then comes from the pgBackRest
        # start that perform() recorded in status.json, by the same rule.
        with patch.object(h, "repository_info", side_effect=AssertionError("no repository request")):
            report = h.report_status(now, local=True)
            self.assertIs(report["gap_open"], True)
            self.assertIn("dropped WAL since the latest recorded full: PITR has a gap (backup.sh full)",
                          report["health_problems"])
            status = h.read_status()
            status["full"]["backup_start"] = epoch("2026-10-14T02:00:00")
            h.save_status(status)
            self.assertIs(h.report_status(now, local=True)["gap_open"], False)
            (h.STATE / h.DROPPED).write_text("2026-10-14T02:00:00.250000Z 0000000100000000000000A0\n")
            report = h.report_status(now, local=True)
        self.assertIs(report["gap_open"], True)
        self.assertIn("backups: not read (--local makes no repository request)", report["unavailable"])

    def test_status_without_state_reports_unreadable_dropped_record(self):
        with patch.object(h, "archiver_facts", side_effect=OSError("no psql")):
            report = h.report_status(epoch("2026-10-14T12:00:00"))
        self.assertIsNone(report["dropped_wal"])
        self.assertIsNone(report["gap_open"])
        self.assertIn("backups: no pgBackRest configuration in this container", report["unavailable"])

    def test_reading_splits_archive_and_backups_against_seven_gigabytes(self):
        self.configured()
        listing = {"archive": {"type": "path"}, "archive/komika/18-1/a.zst": {"type": "file", "size": 3 * 10 ** 9},
                   "backup/komika/F/pg_data.zst": {"type": "file", "size": 4 * 10 ** 9},
                   "backup/komika/backup.info": {"type": "file", "size": 10}}
        wal = json.dumps({"wal_bytes": 123, "stats_reset": None, "lsn_bytes": 456})
        with patch.object(h, "query", return_value=wal), patch.object(h, "collect", return_value=json.dumps(listing)) as collect:
            report = h.reading(epoch("2026-10-20T09:00:00"))
        self.assertEqual(collect.call_args.args[0], ["pgbackrest", "--stanza=komika", "--recurse", "--output=json", "repo-ls"])
        self.assertEqual(report["repository"]["archived_wal_bytes"], 3 * 10 ** 9)
        self.assertEqual(report["repository"]["backup_bytes"], 4 * 10 ** 9 + 10)
        self.assertTrue(report["repository"]["prefix_over_action_threshold"])
        self.assertNotIn("action_needed", report["repository"])
        self.assertIn("account-wide", report["action_threshold_note"])
        self.assertEqual(report["raw_wal"]["lsn_bytes"], 456)
        self.assertEqual(report["duty_dates"], ["2026-10-20", "2026-10-27", "2026-11-03", "2026-11-10"])
        with patch.object(h, "query", side_effect=subprocess.CalledProcessError(2, "psql")), \
                patch.object(h, "collect", side_effect=subprocess.CalledProcessError(9, "pgbackrest")):
            report = h.reading()
        self.assertIsNone(report["raw_wal"])
        self.assertIsNone(report["repository"])
        self.assertEqual(len(report["unavailable"]), 2)

    def test_status_and_reading_never_rewrite_the_configuration(self):
        self.configured()
        before = h.CONFIG.read_text()
        for argv in (["hosted.py", "status", "--local"], ["hosted.py", "reading"]):
            with self.subTest(argv=argv), patch.object(h.sys, "argv", argv), patch.object(h, "configure") as configure, \
                    patch.object(h, "query", side_effect=OSError("no psql")), patch.object(h, "collect", side_effect=OSError("no pgbackrest")), \
                    patch("sys.stdout") as stdout:
                h.main()
            configure.assert_not_called()
            json.loads("".join(c.args[0] for c in stdout.write.call_args_list))
        self.assertEqual(h.CONFIG.read_text(), before)
        with patch.object(h.sys, "argv", ["hosted.py", "reading", "--local"]), self.assertRaises(ValueError):
            h.main()

    def test_full_success_records_pgbackrests_own_start_and_stop(self):
        self.configured()
        clock = [epoch("2026-10-11T02:00:00")]
        old = {"label": "F0", "type": "full", "timestamp": {"start": epoch("2026-10-04T02:00:05"), "stop": epoch("2026-10-04T02:09:00")}}
        new = {"label": "F1", "type": "full", "timestamp": {"start": epoch("2026-10-11T02:00:07"), "stop": epoch("2026-10-11T02:09:58")}}
        incr = {"label": "I1", "type": "incr", "timestamp": {"start": epoch("2026-10-11T02:05:00"), "stop": epoch("2026-10-11T02:06:00")}}
        listings = [[old], [incr, new, old]]
        def backup(*args, **kwargs):
            clock[0] += 600
        with patch.object(h, "backup_info", side_effect=lambda **kwargs: listings.pop(0)), \
                patch.object(h, "pgbr", side_effect=backup), patch.object(h.time, "time", side_effect=lambda: clock[0]):
            self.assertTrue(h.perform("full", 1, {}))
        full = h.read_status()["full"]
        self.assertEqual((full["backup_label"], full["backup_start"], full["backup_stop"]),
                         ("F1", epoch("2026-10-11T02:00:07"), epoch("2026-10-11T02:09:58")))
        self.assertEqual(full["success"], epoch("2026-10-11T02:10:00"))
        self.assertNotIn("started", full)
        # Info cannot show it afterwards: the job still succeeds, the older record stays.
        status = h.read_status()
        with patch.object(h, "backup_info", side_effect=[[new], ValueError("info")]), patch.object(h, "pgbr"), \
                patch.object(h, "log") as log:
            self.assertTrue(h.perform("full", 2, status))
        self.assertEqual(h.read_status()["full"]["backup_start"], epoch("2026-10-11T02:00:07"))
        self.assertIn("start is not recorded", log.call_args_list[0].args[0])

    def test_local_health_without_backup_is_explicitly_unprotected(self):
        self.assertTrue(h.health())

    def test_shared_lock_refuses_overlap_and_releases(self):
        with h.locked():
            with self.assertRaises(BlockingIOError):
                h.locked()
        with h.locked():
            pass

    def test_explicit_first_full_records_status_without_stanza_create(self):
        self.configured()
        now = epoch("2026-10-11T04:00:00")
        with patch.object(h, "backup_info", return_value=[]), patch.object(h, "pgbr") as pgbr, patch.object(h.time, "time", return_value=now):
            with h.locked():
                self.assertTrue(h.perform("full", h.slots(now)["full"], {}, bootstrap=True))
        self.assertEqual([c.args for c in pgbr.call_args_list], [("--type=full", "backup")])
        self.assertEqual(h.read_status()["full"]["success"], now)
        with patch.object(h, "backup_info", return_value=[{}]), patch.object(h, "pgbr") as pgbr:
            self.assertFalse(h.perform("full", h.slots(now)["full"], {}, bootstrap=True))
            pgbr.assert_not_called()

    def test_first_full_accepts_what_pgbackrest_reports_for_a_new_stanza(self):
        # Verbatim `pgbackrest --output=json info` of pgBackRest 2.59.3 after
        # stanza-create and one archived segment (local drill against MinIO):
        # the per-repository status is 2 as well, not only the total.
        fresh = ('[{"archive":[{"database":{"id":1,"repo-key":1},"id":"18-1","max":"000000010000000000000001",'
                 '"min":"000000010000000000000001"}],"backup":[],"cipher":"none","db":[{"id":1,"repo-key":1,'
                 '"system-id":7695421950619119644,"version":"18"}],"name":"komika","repo":[{"cipher":"none","key":1,'
                 '"status":{"code":2,"message":"no valid backups"}}],"status":{"code":2,"lock":{"backup":{"held":false},'
                 '"restore":{"held":false}},"message":"no valid backups"}}]')
        self.configured()
        now = epoch("2026-10-11T04:00:00")
        with patch.object(h, "pgbr", return_value=fresh):
            self.assertEqual(h.backup_info(allow_unbacked=True), [])
            # Start-up and every job except the explicit first full still refuse this state.
            with self.assertRaisesRegex(ValueError, "without a valid backup"):
                h.backup_info()
        calls = []

        def pgbr(*args, capture=False):
            calls.append(args)
            return fresh if "info" in args else None

        with patch.object(h, "pgbr", side_effect=pgbr), patch.object(h.time, "time", return_value=now), h.locked():
            self.assertTrue(h.perform("full", h.slots(now)["full"], {}, bootstrap=True))
            self.assertFalse(h.perform("incr", h.slots(now)["incr"], {}))
        # info before the full, info to record its pgBackRest start, info before the incremental.
        self.assertEqual(calls, [("--output=json", "info"), ("--type=full", "backup"), ("--output=json", "info"),
                                 ("--output=json", "info")])
        for code in (3, 4, 5, 6, 99):
            broken = json.loads(fresh)
            broken[0]["repo"][0]["status"]["code"] = code
            with self.subTest(code=code), patch.object(h, "pgbr", return_value=json.dumps(broken)):
                with self.assertRaisesRegex(ValueError, "repository reports an error"):
                    h.backup_info(allow_unbacked=True)

    def test_first_full_failure_records_last_error(self):
        self.configured()
        with patch.object(h, "backup_info", return_value=[]), patch.object(h, "pgbr", side_effect=subprocess.CalledProcessError(9, "fake")):
            self.assertFalse(h.perform("full", 1, {}, bootstrap=True))
        self.assertNotIn("success", h.read_status()["full"])
        self.assertIn("full failed", h.read_status()["last_error"])

    def test_backup_json_stdout_stays_clean(self):
        self.configured()
        self.assertIn("log-level-console=off", h.CONFIG.read_text())
        self.assertIn("log-level-stderr=info", h.CONFIG.read_text())

    def test_fake_pgbackrest_executable_failure_is_propagated(self):
        binary = self.root / "pgbackrest"
        binary.write_text('#!/bin/sh\nprintf "{broken}"\nexit 39\n')
        binary.chmod(0o700)
        os.environ["PATH"] = str(self.root)
        with self.assertRaises(subprocess.CalledProcessError) as error:
            h.backup_info()
        self.assertEqual(error.exception.returncode, 39)


class FakeStore:
    """An S3 bucket with ETags and conditional PUTs, as R2 and MinIO answer them."""

    def __init__(self, honour=True):
        # honour: True, False, or "create-only" (If-None-Match only, If-Match ignored).
        self.objects, self.honour, self.version, self.puts = {}, honour, 0, []
        self.before_put = None  # A rival writer that acts between a read and a write.

    def __call__(self, method, key="", query=None, body=b"", extra=None, timeout=180):
        extra = {k.lower(): v for k, v in (extra or {}).items()}
        if method == "GET":
            if key not in self.objects:
                return 404, {}, b"<Error/>"
            etag, data = self.objects[key]
            return 200, {"ETag": etag}, data
        if self.before_put:
            rival, self.before_put = self.before_put, None
            rival()
        current = self.objects.get(key)
        if self.honour:
            if extra.get("if-none-match") == "*" and current is not None:
                return 412, {}, b""
            if self.honour is True and "if-match" in extra and (current is None or current[0] != extra["if-match"]):
                return 412, {}, b""
        self.version += 1
        self.objects[key] = (f'"v{self.version}"', body)
        self.puts.append((key, extra))
        return 200, {"ETag": f'"v{self.version}"'}, b""

    def fence(self):
        return json.loads(self.objects["rehearsal/test.fence.json"][1])


class StandbyTests(unittest.TestCase):
    """Fence parsing, the refusal matrix, the claim, promote preconditions and a
    lost conditional write. The bucket is a fake; PostgreSQL is a fake query()."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name, value in (("DATA", self.root / "data"), ("STATE", self.root / "state"),
                            ("CONFIG", self.root / "config"), ("TLS", self.root / "tls")):
            p = patch.object(h, name, value)
            p.start()
            self.addCleanup(p.stop)
        p = patch.dict(os.environ, dict(zip(h.BACKUP_VARS, (
            "https://example.r2.cloudflarestorage.com", "komika-backup", "rehearsal/test", "testkey", "testsecret"))),
            clear=True)
        p.start()
        self.addCleanup(p.stop)
        self.assertTrue(h.configure())
        self.store = FakeStore()
        p = patch.object(h, "s3_request", side_effect=self.store)
        p.start()
        self.addCleanup(p.stop)
        self.exec = patch.object(h.os, "execvp")
        self.execute = self.exec.start()
        self.addCleanup(self.exec.stop)
        self.timeline = 1
        p = patch.object(h, "local_timeline", side_effect=lambda: self.timeline)
        p.start()
        self.addCleanup(p.stop)

    def existing(self, standby=False):
        h.DATA.mkdir()
        (h.DATA / "PG_VERSION").write_text("18")
        if standby:
            (h.DATA / "standby.signal").write_text("")

    def put_fence(self, **fields):
        fence = dict({"epoch": 4, "primary_node": "node-a", "timeline": 2, "at": "2026-10-16T12:00:00Z"}, **fields)
        self.store("PUT", "rehearsal/test.fence.json", body=json.dumps(fence).encode())
        return fence

    def test_fence_parsing_is_strict(self):
        good = {"epoch": 3, "primary_node": "node-a", "timeline": 2, "at": "2026-10-16T12:00:00Z"}
        self.assertEqual(h.parse_fence(json.dumps(good)), good)
        for change in ({"epoch": 0}, {"epoch": "3"}, {"epoch": True}, {"epoch": 1.0}, {"timeline": 0},
                       {"primary_node": "Node-A"}, {"primary_node": "-a"}, {"primary_node": ""}, {"primary_node": 7},
                       {"at": "2026-10-16 12:00:00"}, {"at": "2026-13-16T12:00:00Z"}, {"extra": 1}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                h.parse_fence(json.dumps(dict(good, **change)))
        for text in ("[]", "null", '{"epoch": 3}', "not json"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                h.parse_fence(text)
        # Beside the pgBackRest path, never inside it.
        self.assertEqual(h.fence_key(), "rehearsal/test.fence.json")

    def test_refusal_matrix(self):
        fence = {"epoch": 4, "primary_node": "node-a", "timeline": 3, "at": "2026-10-16T12:00:00Z"}
        cases = ((None, None, None, True), (fence, None, 1, True),  # Fencing off: no node name.
                 (None, "node-a", 3, False),                       # No fence: only a claim may start it.
                 (fence, "node-a", 3, True), (fence, "node-a", 4, True), (fence, "node-a", None, True),
                 (fence, "node-a", 2, False),                      # An old copy of the fenced node.
                 (fence, "node-b", 3, False), (fence, "node-b", 9, False))
        for value, node, timeline, allowed in cases:
            with self.subTest(node=node, timeline=timeline, fence=value is not None):
                self.assertEqual(h.fence_decision(value, node, timeline)[0], allowed)

    def test_node_name_is_validated(self):
        for value in ("Node", "a_b", "-a", "a" * 64, "a b"):
            with self.subTest(value=value), patch.dict(os.environ, {"KOMIKA_NODE_ID": value}), \
                    self.assertRaises(ValueError):
                h.node_id()
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": ""}):
            self.assertIsNone(h.node_id())

    def test_without_a_node_name_start_never_reads_the_fence(self):
        self.existing()
        self.put_fence(primary_node="node-b")
        with patch.object(h, "read_fence", side_effect=AssertionError("fencing is off")):
            h.start(True)
        args = self.execute.call_args.args[1]
        self.assertFalse(any(a.startswith("komika.") for a in args))

    def test_the_fenced_primary_starts_and_passes_its_epoch_to_the_server(self):
        self.existing()
        self.put_fence(primary_node="node-a", timeline=2)
        self.timeline = 2
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}):
            h.start(True)
        args = self.execute.call_args.args[1]
        self.assertIn("komika.node_id=node-a", args)
        self.assertIn("komika.fence_epoch=4", args)

    def test_old_primary_and_old_copies_refuse_to_start_as_primary(self):
        self.existing()
        for node, timeline in (("node-b", 2), ("node-a", 1)):
            self.put_fence(primary_node="node-a", timeline=2)
            self.timeline = timeline
            with self.subTest(node=node, timeline=timeline), patch.dict(os.environ, {"KOMIKA_NODE_ID": node}), \
                    self.assertRaises(ValueError) as refused:
                h.start(True)
            self.assertIn("refusing to start as primary", str(refused.exception))
        # Empty data directory: no restore and no initdb for a node the fence does not name.
        (h.DATA / "PG_VERSION").unlink()
        h.DATA.rmdir()
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), patch.object(h, "backup_info") as info, \
                patch.object(h, "init_cluster") as init, self.assertRaises(ValueError):
            h.start(True)
        info.assert_not_called()
        init.assert_not_called()
        self.execute.assert_not_called()

    def test_unreadable_fence_or_no_repository_refuses(self):
        self.existing()
        self.store.objects["rehearsal/test.fence.json"] = ('"x"', b"{broken")
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}), self.assertRaises(ValueError):
            h.start(True)
        with patch.object(h, "s3_request", return_value=(403, {}, b"")), \
                patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}), self.assertRaises(ValueError):
            h.start(True)
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}), self.assertRaises(ValueError):
            h.start(False)  # No repository: nowhere to keep a fence.
        self.execute.assert_not_called()

    def test_a_standby_starts_without_the_fence(self):
        self.existing(standby=True)
        self.put_fence(primary_node="node-a")
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), \
                patch.object(h, "read_fence", side_effect=AssertionError("a standby takes no writes")):
            h.start(True)
        self.assertFalse(any(a.startswith("komika.") for a in self.execute.call_args.args[1]))

    def test_first_primary_claims_epoch_one_and_a_lost_claim_is_decided_again(self):
        self.existing()
        self.timeline = 1
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}):
            h.start(True)
        self.assertEqual(self.store.fence()["epoch"], 1)
        self.assertEqual(self.store.fence()["primary_node"], "node-a")
        self.assertEqual(self.store.puts[-1][1].get("if-none-match"), "*")
        # Two first starts at once: node-b reads no fence, node-a claims first.
        del self.store.objects["rehearsal/test.fence.json"]
        self.store.before_put = lambda: self.put_fence(epoch=1, primary_node="node-a", timeline=1)
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), self.assertRaises(ValueError) as refused:
            h.start(True)
        self.assertIn("names node-a", str(refused.exception))

    def promote_with(self, replication, node="node-b", argv=("--old-primary-stopped", "node-a"), events=None):
        sql = []

        def query(text, timeout=10):
            sql.append(text)
            if events is not None and "pg_promote" in text:
                events.append("promote")
            return "t\n" if "pg_promote" in text else ""

        calls = []

        def facts(timeout=10):
            calls.append(timeout)
            # After pg_promote() and the checkpoint: out of recovery on the fence's timeline.
            if len(calls) > 1 and any("CHECKPOINT" in text for text in sql):
                current = h.read_fence()[0]
                return {"in_recovery": False, "timeline": current["timeline"] if current else 1}
            return replication(timeout)

        with patch.dict(os.environ, {"KOMIKA_NODE_ID": node}), patch.object(h, "query", side_effect=query), \
                patch.object(h, "replication_facts", side_effect=facts):
            result = h.promote(list(argv))
        return result, sql

    def standby_facts(self, timeline=2):
        return lambda timeout=10: {"in_recovery": True, "timeline": timeline, "receiver": None,
                                   "replay_lsn": "0/3000148", "last_replayed_commit": 1792000000}

    def test_promote_refuses_without_the_operators_word(self):
        self.put_fence()
        for argv in ((), ("--force",), ("--old-primary-stopped",), ("--old-primary-stopped", "node-a", "extra"),
                     ("--old-primary-unreachable", "node-a"), ("--old-primary-stopped", "Node-A"),
                     ("--old-primary-stopped=node-a",)):
            with self.subTest(argv=argv), self.assertRaises(ValueError) as refused:
                self.promote_with(self.standby_facts(), argv=argv)
            self.assertIn("unreachable port is not proof", str(refused.exception))
        self.assertEqual(self.store.fence()["epoch"], 4)

    def test_promote_preconditions(self):
        self.put_fence(primary_node="node-a")
        primary = lambda timeout=10: {"in_recovery": False, "timeline": 2}
        with self.assertRaises(ValueError):  # Not a standby, and the fence names another node.
            self.promote_with(primary)
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": ""}), self.assertRaises(ValueError):
            h.promote(["--old-primary-stopped", "node-a"])
        with self.assertRaises(ValueError):  # The operator named this node, not the old primary.
            self.promote_with(self.standby_facts(), argv=("--old-primary-stopped", "node-b"))
        h.CONFIG.unlink()
        with self.assertRaises(ValueError):  # No repository: nowhere to move the fence.
            self.promote_with(self.standby_facts())
        self.assertEqual(self.store.fence()["epoch"], 4)

    def test_promote_moves_the_fence_before_pg_promote(self):
        self.put_fence(primary_node="node-a", timeline=2)
        order = []
        real = h.write_fence
        with patch.object(h, "write_fence", side_effect=lambda *a: order.append("fence") or real(*a)):
            fence, sql = self.promote_with(self.standby_facts(timeline=2), events=order)
        self.assertEqual(order, ["fence", "promote"])
        self.assertEqual(self.store.fence(), fence)
        self.assertEqual((fence["epoch"], fence["primary_node"], fence["timeline"]), (5, "node-b", 3))
        self.assertEqual(self.store.puts[-1][1].get("if-match"), '"v1"')
        joined = "".join(sql)
        self.assertIn("CHECKPOINT", joined)
        self.assertIn("ALTER SYSTEM SET komika.node_id = 'node-b'", joined)
        self.assertIn("ALTER SYSTEM SET komika.fence_epoch = '5'", joined)
        self.assertIn("ALTER SYSTEM RESET primary_conninfo", joined)

    def test_two_promotions_cannot_both_win(self):
        self.put_fence(primary_node="node-a")
        # node-c moves the fence between this node's read and its write.
        writes = []
        real = h.write_fence

        def write(fence, etag):
            self.put_fence(epoch=5, primary_node="node-c", timeline=3)
            writes.append(fence)
            return real(fence, etag)

        with patch.object(h, "write_fence", side_effect=write), self.assertRaises(ValueError) as refused:
            self.promote_with(self.standby_facts())
        self.assertIn("another promotion won", str(refused.exception))
        self.assertEqual(self.store.fence()["primary_node"], "node-c")
        self.assertEqual(len(writes), 1)

    def test_a_later_candidate_never_takes_over_a_fence_already_moved(self):
        """node-b has moved the fence and not yet run pg_promote(); node-c, told
        that node-a stopped, reads node-b's fence and is refused."""
        self.put_fence(primary_node="node-b", epoch=5)
        with self.assertRaises(ValueError) as refused:
            self.promote_with(self.standby_facts(), node="node-c")
        self.assertIn("names node-b, not node-a", str(refused.exception))
        self.assertEqual(self.store.fence()["primary_node"], "node-b")

    def test_a_store_that_ignores_conditions_refuses_promotion(self):
        for honour in (False, "create-only"):
            self.store = FakeStore(honour)
            self.put_fence(primary_node="node-a")
            with self.subTest(honour=honour), patch.object(h, "s3_request", side_effect=self.store), \
                    self.assertRaises(ValueError) as refused:
                self.promote_with(self.standby_facts())
            self.assertIn("ignores conditional writes", str(refused.exception))
            self.assertEqual(self.store.fence()["primary_node"], "node-a")

    def test_an_old_standby_copy_is_never_promoted(self):
        self.put_fence(primary_node="node-a", timeline=3)
        with self.assertRaises(ValueError) as refused:  # On timeline 1 while the fence says 3.
            self.promote_with(self.standby_facts(timeline=1))
        self.assertIn("old copy", str(refused.exception))
        self.assertEqual(self.store.fence()["epoch"], 4)
        # A rerun on a stale copy: the fence already names it, predicted timeline 4.
        self.put_fence(primary_node="node-b", epoch=5, timeline=4)
        with self.assertRaises(ValueError):
            self.promote_with(self.standby_facts(timeline=1))
        # PostgreSQL ended below the fence's timeline: no server settings.
        self.put_fence(primary_node="node-a", timeline=2)
        sql = []
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), \
                patch.object(h, "query", side_effect=lambda text, timeout=10: sql.append(text) or "t\n"), \
                patch.object(h, "replication_facts", side_effect=[self.standby_facts(2)(), {"in_recovery": False, "timeline": 2}]), \
                self.assertRaises(ValueError):
            h.promote(["--old-primary-stopped", "node-a"])
        self.assertFalse(any("ALTER SYSTEM" in text for text in sql))

    def test_first_claim_needs_a_store_that_honours_conditions(self):
        self.existing()
        self.store.honour = False
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}), self.assertRaises(ValueError) as refused:
            h.start(True)
        self.assertIn("ignores conditional writes", str(refused.exception))
        self.assertNotIn("rehearsal/test.fence.json", self.store.objects)
        self.execute.assert_not_called()

    def test_promote_rerun_after_a_crash_does_not_write_again(self):
        fence = self.put_fence(primary_node="node-b", epoch=5)
        puts = len(self.store.puts)
        result, sql = self.promote_with(self.standby_facts())
        self.assertEqual(result, fence)
        self.assertEqual(len(self.store.puts), puts)
        self.assertTrue(any("pg_promote" in text for text in sql))
        # Promoted, then interrupted before the settings: the rerun finishes them
        # and does not promote again.
        result, sql = self.promote_with(lambda timeout=10: {"in_recovery": False, "timeline": 3})
        self.assertFalse(any("pg_promote" in text for text in sql))
        self.assertIn("ALTER SYSTEM SET komika.node_id = 'node-b'", "".join(sql))
        self.assertTrue(any("CHECKPOINT" in text for text in sql))

    def test_backup_jobs_run_only_on_the_fenced_primary(self):
        self.put_fence(primary_node="node-a")
        cases = (("", "f", None), ("node-a", "f", None), ("node-b", "f", "names node-a"),
                 ("node-a", "t", "standby"))
        for node, recovering, refusal in cases:
            with self.subTest(node=node, recovering=recovering), patch.dict(os.environ, {"KOMIKA_NODE_ID": node}), \
                    patch.object(h, "query", return_value=recovering + "\n"):
                gate = h.backup_gate()
                if refusal is None:
                    self.assertIsNone(gate)
                else:
                    self.assertIn(refusal, gate)
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}), \
                patch.object(h, "query", side_effect=subprocess.CalledProcessError(2, "psql")):
            self.assertIn("unreadable", h.backup_gate())

    def test_fence_check_command(self):
        self.existing()
        self.put_fence(primary_node="node-a", timeline=1)
        for node, standby, allowed in (("node-a", False, True), ("node-b", False, False), ("node-a", True, False)):
            if standby:
                (h.DATA / "standby.signal").write_text("")
            with self.subTest(node=node, standby=standby), patch.dict(os.environ, {"KOMIKA_NODE_ID": node}), \
                    patch("sys.stdout") as out:
                self.assertEqual(h.fence_check(), allowed)
            line = json.loads("".join(c.args[0] for c in out.write.call_args_list))
            self.assertEqual(line["allowed"], allowed)

    def test_replication_hba_needs_tls_and_explicit_cidrs(self):
        h.DATA.mkdir()
        with patch.dict(os.environ, {"KOMIKA_PG_REPLICA_CIDRS": "203.0.113.7/32"}), self.assertRaises(ValueError):
            h.hba()  # No TLS files.
        h.TLS.mkdir()
        for name in ("server.crt", "server.key", "ca.crt"):
            (h.TLS / name).write_text("x")
        with patch.dict(os.environ, {"KOMIKA_PG_REPLICA_CIDRS": "203.0.113.7/32, 10.1.0.0/16"}):
            h.hba()
        text = (h.DATA / "pg_hba.conf").read_text()
        self.assertIn("hostssl replication komika_replica 203.0.113.7/32 cert\n", text)
        self.assertLess(text.index("hostssl replication"), text.index("host all all 0.0.0.0/0 reject"))
        for value in ("0.0.0.0/0", "::/0", "203.0.113.7", "203.0.113.7/24"):
            with self.subTest(value=value), patch.dict(os.environ, {"KOMIKA_PG_REPLICA_CIDRS": value}), \
                    self.assertRaises(ValueError):
                h.hba()
        args = h.postgres_args(True)
        self.assertIn("ssl=on", args)
        self.assertIn(f"ssl_key_file={h.TLS}/server.key", args)
        self.assertNotIn("replication", h.GRANT)
        with patch.dict(os.environ, {"KOMIKA_PG_REPLICA_CIDRS": "203.0.113.7/32"}):
            self.assertIn("CREATE ROLE komika_replica LOGIN REPLICATION", h.grant_sql())
            self.assertIn("GRANT pg_read_all_stats TO komika", h.grant_sql())

    def test_standby_init_refusals_and_written_settings(self):
        with self.assertRaises(ValueError):
            h.standby_init(["--primary", "db.example"])
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), self.assertRaises(ValueError):
            h.standby_init(["--primary", "db.example:5432"])  # No TLS files.
        h.TLS.mkdir()
        for name in ("ca.crt", "client.crt", "client.key"):
            (h.TLS / name).write_text("x")
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), self.assertRaises(ValueError) as refused:
            h.standby_init(["--primary", "db.example:5432"])
        self.assertIn("fence.json is absent", str(refused.exception))
        self.put_fence(primary_node="node-b")
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), self.assertRaises(ValueError) as refused:
            h.standby_init(["--primary", "db.example:5432"])
        self.assertIn("names this node", str(refused.exception))
        self.put_fence(primary_node="node-a")
        calls = []

        def run(args, **kwargs):
            calls.append(args)
            (h.DATA / "PG_VERSION").write_text("18")
            (h.DATA / "postgresql.auto.conf").write_text(
                "komika.node_id = 'node-a'\nprimary_conninfo = 'host=old'\nprimary_slot_name = 'old'\n")

        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), patch.object(h, "collect", return_value="||\n"), \
                patch.object(h, "run", side_effect=run):
            h.standby_init(["--primary", "db.example:5432"])
        self.assertIn("--create-slot", calls[0])
        self.assertIn("--slot=komika_standby", calls[0])
        conninfo = calls[0][calls[0].index("-d") + 1]
        self.assertIn("sslmode=verify-full", conninfo)
        self.assertIn("application_name=node-b", conninfo)
        auto = (h.DATA / "postgresql.auto.conf").read_text()
        self.assertEqual(auto.count("primary_conninfo"), 1)
        self.assertIn(f"primary_conninfo = '{conninfo}'", auto)
        self.assertIn("primary_slot_name = 'komika_standby'", auto)
        self.assertTrue((h.DATA / "standby.signal").is_file())
        # A second standby-init needs an empty directory.
        with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), self.assertRaises(ValueError):
            h.standby_init(["--primary", "db.example:5432"])
        # An existing slot is reused; a lost one (no restart_lsn) is dropped and made again.
        for answer, dropped, created in (("physical|0/3000148|1\n", False, False), ("physical||\n", True, True)):
            shutil.rmtree(h.DATA)
            commands, calls[:] = [], []

            def collect(args, timeout, input=None):
                commands.append(args[-1])
                return answer if args[-1].startswith("READ_") else ""

            with self.subTest(answer=answer), patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-b"}), \
                    patch.object(h, "collect", side_effect=collect), patch.object(h, "run", side_effect=run):
                h.standby_init(["--primary", "db.example:5432"])
            self.assertEqual("DROP_REPLICATION_SLOT komika_standby" in commands, dropped)
            self.assertEqual("--create-slot" in calls[0], created)

    def test_without_a_node_name_status_and_health_ask_nothing_new(self):
        h.STATE.mkdir(exist_ok=True)
        with patch.object(h, "read_fence", side_effect=AssertionError("fencing is off")), \
                patch.object(h, "repository_info", side_effect=ValueError("no repository here")), \
                patch.object(h, "archiver_facts", return_value={"in_recovery": False, "ready": 0}), \
                patch.object(h, "free_bytes", return_value=100 << 30), \
                patch.object(h, "replication_facts", side_effect=subprocess.CalledProcessError(2, "psql")):
            report = h.report_status(local=False)
            self.assertIsNone(report["fence"])
            self.assertFalse(any("replication state" in p for p in h.health_problems()))
            with patch.dict(os.environ, {"KOMIKA_NODE_ID": "node-a"}):
                self.assertIn("replication state unreadable", h.health_problems())

    def test_health_of_a_standby_and_of_a_primary_with_a_slot(self):
        self.existing(standby=True)
        facts = {"in_recovery": True, "receiver": {"status": "streaming"}, "replay_lag_seconds": 0}
        with patch.object(h, "replication_facts", return_value=facts), patch.object(h, "free_bytes", return_value=100 << 30), \
                patch.object(h, "read_status", side_effect=AssertionError("a standby runs no backups")):
            self.assertEqual(h.health_problems(), [])
            facts.update(receiver=None, replay_lag_seconds=900)
            self.assertEqual(len(h.health_problems()), 2)
        self.assertEqual(len(h.replication_problems({"in_recovery": False, "slots": [],
                                                     "senders": [{"name": "node-b", "replay_lag_seconds": None}]})), 1)
        primary = {"in_recovery": False, "senders": [{"name": "node-b", "replay_lag_seconds": 400}],
                   "slots": [{"name": "komika_standby", "wal_status": "extended", "retained_bytes": 2 << 30},
                             {"name": "other", "wal_status": "lost", "retained_bytes": None}]}
        problems = h.replication_problems(primary)
        self.assertEqual(len(problems), 3)
        self.assertEqual(h.replication_problems({"in_recovery": False, "senders": [], "slots": []}), [])

    def test_standby_certs_verify_against_their_ca(self):
        if not shutil.which("openssl", path=PATH):
            self.skipTest("openssl not installed")
        os.environ["PATH"] = PATH
        out = self.root / "certs"
        h.standby_certs([str(out), "node-a=203.0.113.7", "node-b=db-b.example"])
        for node, san in (("node-a", "IP Address:203.0.113.7"), ("node-b", "DNS:db-b.example")):
            directory = out / node
            self.assertEqual(sorted(p.name for p in directory.iterdir()),
                             ["ca.crt", "client.crt", "client.key", "server.crt", "server.key"])
            for name in ("server.crt", "client.crt"):
                verify = subprocess.run(["openssl", "verify", "-CAfile", str(out / "ca.crt"), str(directory / name)],
                                        capture_output=True, text=True)
                self.assertEqual(verify.returncode, 0, verify.stdout + verify.stderr)
            text = subprocess.run(["openssl", "x509", "-noout", "-text", "-in", str(directory / "server.crt")],
                                  capture_output=True, text=True).stdout
            self.assertIn(san, text)
            client = subprocess.run(["openssl", "x509", "-noout", "-subject", "-in", str(directory / "client.crt")],
                                    capture_output=True, text=True).stdout
            self.assertIn("komika_replica", client)
            for key in ("server.key", "client.key"):
                self.assertEqual((directory / key).stat().st_mode & 0o077, 0)
        self.assertEqual((out / "ca.key").stat().st_mode & 0o077, 0)
        with self.assertRaises(ValueError):  # Never overwrites a node's certificates.
            h.standby_certs([str(out), "node-a=203.0.113.7"])
        for bad in ("Node=1.2.3.4", "node-c=", "node-c=bad host"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                h.standby_certs([str(out), bad])


if __name__ == "__main__":
    unittest.main()
