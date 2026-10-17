from pathlib import Path
import subprocess
import tempfile
import unittest
import safety


class HealthTests(unittest.TestCase):
    def test_primary_must_not_recover_and_a_standby_must(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            data = root / "data"
            data.mkdir()
            binary = root / "psql"
            # Model successful hot-standby queries: SELECT 1 still succeeds;
            # only the recovery predicate suppresses the result.
            binary.write_text('''#!/bin/sh
printf '%s\\n' "$@" > "$QUERY_TRACE"
test "$PGPASSWORD" = fake-password || exit 2
case "$*" in
  *"pg_is_in_recovery() = true"*)
    test "$RECOVERING" = 1 && printf '1\\n' ;;
  *"pg_is_in_recovery() = false"*)
    test "$RECOVERING" = 1 || printf '1\\n' ;;
  *) exit 3 ;;
esac
exit 0
''')
            binary.chmod(0o700)
            environment = {"PATH": str(root), "HOSTNAME": "private-postgres", "KOMIKA_POSTGRES_PASSWORD": "fake-password",
                           "QUERY_TRACE": str(root / "trace"), "PGDATA": str(data)}
            for standby in (False, True):
                if standby:
                    (data / "standby.signal").write_text("")
                for recovering in ("1", "0"):
                    with self.subTest(standby=standby, recovering=recovering):
                        # safety.KIT is the kit under test: in the image its own /opt/komika-postgres.
                        result = subprocess.run(["/bin/sh", str(safety.KIT / "health.sh")],
                                                env=environment | {"RECOVERING": recovering}, capture_output=True)
                        healthy = (recovering == "1") == standby
                        self.assertEqual(result.returncode, 0 if healthy else 1)
            query = (root / "trace").read_text()
            self.assertIn("pg_is_in_recovery() = true", query)
            self.assertIn("current_user = 'komika'", query)
            self.assertIn("rolsuper", query)
            self.assertIn("private-postgres", query)
            self.assertNotIn("fake-password", query)


if __name__ == "__main__":
    unittest.main()
