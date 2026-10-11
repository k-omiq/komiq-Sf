#!/usr/bin/env python3
"""Build the same frozen source fixture as pg_import::tests::fixture, without a server."""
import hashlib
import pathlib
import sqlite3
import sys

repo = pathlib.Path(__file__).resolve().parents[2]
output = pathlib.Path(sys.argv[1])
if output.exists():
    raise SystemExit("Refusing to replace an existing fixture")
with sqlite3.connect(output) as db:
    db.execute("PRAGMA foreign_keys=ON")
    db.execute("""CREATE TABLE _sqlx_migrations (
        version BIGINT PRIMARY KEY, description TEXT NOT NULL,
        installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
        success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)""")
    for migration in sorted((repo / "apps/server/migrations").glob("*.sql")):
        version, description = migration.stem.split("_", 1)
        sql = migration.read_bytes()
        db.executescript(sql.decode())
        db.execute("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES (?,?,TRUE,?,0)",
                   (int(version), description.replace("_", " "), hashlib.sha384(sql).digest()))
        db.commit()
    for user_id, name in [("00000000-0000-4000-8000-000000000900", "owner"),
                          ("00000000-0000-4000-8000-000000000901", "reader"),
                          ("00000000-0000-4000-8000-000000000902", "cjk-reader")]:
        db.execute("INSERT INTO users(id,username,email,password_hash,created_at) VALUES (?,?,?,'fixture-secret','2020-01-01T00:00:00Z')",
                   (user_id, name, f"{name}@example.invalid"))
    db.executescript((repo / "apps/server/tests/postgres_golden/seed_sqlite.sql").read_text())
