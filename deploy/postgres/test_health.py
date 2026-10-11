from pathlib import Path
import subprocess
import tempfile
import unittest
import safety


class HealthTests(unittest.TestCase):
    def test_authenticated_primary_required_recovery_never_healthy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "psql"
            # Model successful hot-standby queries: SELECT 1 still succeeds;
            # only the recovery predicate suppresses the result.
            binary.write_text('''#!/bin/sh
printf '%s\\n' "$@" > "$QUERY_TRACE"
test "$PGPASSWORD" = fake-password || exit 2
case "$*" in
  *"NOT pg_is_in_recovery()"*)
    test "$RECOVERING" = 1 || printf '1\\n' ;;
  *) printf '1\\n' ;;
esac
''')
            binary.chmod(0o700)
            environment = {"PATH": str(root), "HOSTNAME": "private-postgres", "KOMIKA_POSTGRES_PASSWORD": "fake-password",
                           "QUERY_TRACE": str(root / "trace")}
            for recovering, expected in (("1", 1), ("0", 0)):
                with self.subTest(recovering=recovering):
                    # safety.KIT is the kit under test: in the image its own /opt/komika-postgres.
                    result = subprocess.run(["/bin/sh", str(safety.KIT / "health.sh")],
                                            env=environment | {"RECOVERING": recovering}, capture_output=True)
                    self.assertEqual(result.returncode, expected)
            query = (root / "trace").read_text()
            self.assertIn("NOT pg_is_in_recovery()", query)
            self.assertIn("current_user = 'komika'", query)
            self.assertIn("rolsuper", query)
            self.assertIn("private-postgres", query)
            self.assertNotIn("fake-password", query)


if __name__ == "__main__":
    unittest.main()
