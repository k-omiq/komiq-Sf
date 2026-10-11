import datetime as dt
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import hosted as h


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
        self.assertEqual(calls, [("--output=json", "info"), ("--type=full", "backup"), ("--output=json", "info")])
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


if __name__ == "__main__":
    unittest.main()
