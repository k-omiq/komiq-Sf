#!/usr/bin/env python3
"""Reproduce pg_import/tests.rs fixture, including SQLx migration checksums."""
import hashlib
import json
import re
import sqlite3
import sys
from pathlib import Path


# The golden seed leaves 34 of the 63 application tables empty. One row for each of them,
# so that the conversion of every table the import plan copies is exercised with data.
# Values are inert: no row makes the server fetch anything or change another table
# (ended ingest job, rejected merge candidate, subscription disabled far in the future,
# session that does not expire). Text dates are RFC 3339; integer dates are milliseconds.
USER = "00000000-0000-4000-8000-000000000900"
WORK = "w_00000000000040008000000000000001"
WORK_B = "w_00000000000040008000000000000002"
SERIES = "ss_00000000000040008000000000000001"
COMMENT = "00000000-0000-4000-8000-000000001100"
MANGA = 710001
AT = "2021-02-03T04:05:06.789Z"
MS = 1612325106789
EXTRA_ROWS = {
    "all_mangadex_uuid": {"source_series_id": SERIES, "work_id": WORK, "suwayomi_key": "710001",
                          "mangadex_uuid": "00000000-0000-4000-8000-00000000a001", "resolved_at": AT},
    "browse_catalogue": {"work_id": WORK, "reader_id": WORK, "title": "Gate row é 漢字", "created_at": AT,
                         "released_at": MS, "rating": 7.25},
    "catalogue_sync_state": {"job": "desktop-gate-fixture", "last_synced_at": AT, "updated_at": AT, "seed_done": 1},
    "comment_media": {"id": "00000000-0000-4000-8000-00000000a002", "comment_id": COMMENT, "user_id": USER,
                      "webp": b"RIFF\x00\x00\x00\x00WEBP gate", "width": 1, "height": 1, "created_at": AT},
    "cover_source_backoff": {"source_id": "desktop-gate-source", "reason": "timeout", "first_seen": AT,
                             "last_seen": AT, "retry_after": AT},
    "extension_subscription": {"pkg_name": "desktop.gate.fixture", "created_at": AT,
                               "disabled_at": "2999-01-01T00:00:00.000Z"},
    "extension_update_log": {"id": 1, "at": AT, "pkg_name": "desktop.gate.fixture", "origin": "manual"},
    "feed_genre_facet": {"tag": "Gate genre", "safe_count": 1, "all_count": 1},
    "feed_reconcile_report": {"id": 1, "ran_at": AT, "duration_ms": 1, "rows_before": 0, "rows_after": 0,
                              "drifted": 0},
    "feed_series_updates": {"work_id": WORK, "reader_id": WORK, "title": "Gate row é 漢字", "released_at": MS},
    "feed_updates": {"work_id": WORK, "title": "Gate row é 漢字", "latest_at": AT},
    "instance_setting": {"key": "instance_name", "value": "Gate fixture é 漢字", "updated_at": AT},
    "maintenance_flag": {"key": "desktop_gate_fixture", "done_at": AT},
    "merge_candidate": {"id": "mc_desktop_gate", "source_series_id": SERIES, "candidate_work_id": WORK_B,
                        "score": 0.5, "method": "title", "status": "rejected", "created_at": AT, "resolved_at": AT},
    "release_event": {"work_id": WORK, "chapter_key": "gate-1", "first_seen_at": MS, "label": "Ch. 1"},
    "report": {"id": "00000000-0000-4000-8000-00000000a003", "kind": "other", "status": "resolved",
               "user_id": USER, "detail": "Gate row é 漢字", "resolved_at": AT, "resolved_by": USER, "created_at": AT},
    "series_view_bucket": {"series_key": WORK, "hour_ts": 447868, "views": 3},
    "sessions": {"token": "desktop-gate-fixture-session-token", "user_id": USER, "created_at": AT},
    "source_health_bucket": {"source_id": "desktop-gate-source", "surface": "scan", "hour_ts": 447868,
                             "reason": "timeout", "count": 1, "last_at": AT},
    "source_ingest_job": {"id": "ij_desktop_gate", "source_id": "desktop-gate-source", "state": "completed",
                          "started_at": AT, "finished_at": AT},
    "source_latest_snapshot": {"source_id": "desktop-gate-source", "ordered_ids": "[\"1\",\"2\"]", "captured_at": AT},
    "source_nsfw_pin": {"source_id": "desktop-gate-source", "created_at": AT},
    "source_rate_limit": {"source_id": "desktop-gate-source", "requests": 5, "window_secs": 1, "updated_at": AT},
    "source_scan_outage": {"source_id": "desktop-gate-source", "detected_at": AT, "last_alert_at": AT},
    "suwayomi_cover_issue": {"manga_id": MANGA, "reason": "timeout", "first_seen": AT, "last_seen": AT,
                             "retry_after": AT},
    "sync_state": {"id": 1, "last_full_pass_at": AT},
    "tag": {"id": "tag_desktop_gate", "name": "Gate tag é 漢字"},
    "user_avatars": {"user_id": USER, "webp": b"RIFF\x00\x00\x00\x00WEBP gate", "version": 1, "updated_at": AT},
    "work_alias_token": {"work_id": WORK, "token": "gate"},
    "work_cover_issue": {"work_id": WORK, "reason": "timeout", "first_seen": AT, "last_seen": AT},
    "work_field_pin": {"work_id": WORK, "field": "description", "value": "Gate row é 漢字", "pinned_at": AT},
    "work_redirect": {"old_id": "w_desktop_gate_old", "new_id": WORK, "created_at": AT},
    "work_split": {"work_a": WORK, "work_b": WORK_B, "split_at": AT},
    "work_tracker_link": {"work_id": WORK, "provider": "anilist", "external_id": "1"},
}


def fill_empty_tables(db):
    for table, row in EXTRA_ROWS.items():
        if db.execute('SELECT count(*) FROM "' + table + '"').fetchone()[0]:
            continue  # the seed fills it now
        columns = ",".join('"' + name + '"' for name in row)
        db.execute('INSERT INTO "' + table + '" (' + columns + ") VALUES (" + ",".join("?" * len(row)) + ")",
                   tuple(row.values()))


def build(path, repo):
    if path.exists():
        raise ValueError("Fixture destination already exists")
    path.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(path) as db:
        db.execute("PRAGMA foreign_keys=ON")
        db.execute("""CREATE TABLE _sqlx_migrations (
          version BIGINT PRIMARY KEY, description TEXT NOT NULL,
          installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
          success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)""")
        for migration in sorted((repo / "apps/server/migrations").glob("*.sql")):
            match = re.fullmatch(r"(\d+)_(.+)\.sql", migration.name)
            sql = migration.read_bytes()
            db.executescript(sql.decode("utf-8"))
            db.execute("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES (?,?,1,?,0)",
                       (int(match[1]), match[2].replace("_", " "), hashlib.sha384(sql).digest()))
            db.commit()
        for suffix, name in [(900, "owner"), (901, "reader"), (902, "cjk-reader")]:
            db.execute("INSERT INTO users(id,username,email,password_hash,created_at) VALUES (?,?,?,'fixture-secret','2020-01-01T00:00:00Z')",
                       (f"00000000-0000-4000-8000-{suffix:012d}", name, f"{name}@example.invalid"))
        db.commit()
        db.executescript((repo / "apps/server/tests/postgres_golden/seed_sqlite.sql").read_text(encoding="utf-8"))
        fill_empty_tables(db)
        db.commit()
        tables = [row[0] for row in db.execute("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'work_fts%' AND name <> '_sqlx_migrations' ORDER BY name")]
        counts = {name: db.execute('SELECT count(*) FROM "' + name.replace('"', '""') + '"').fetchone()[0] for name in tables}
    return counts


if __name__ == "__main__":
    # Arguments: the fixture file to make, the repository, the file for the row counts,
    # and optionally a file for the rows of `instance_setting` as the fixture holds them
    # (`[[key, value, updated_at], ...]`). The gate compares that table by content: the
    # running server adds rows of its own to it.
    destination, repo, counts, *settings = map(Path, sys.argv[1:])
    counts.write_text(json.dumps(build(destination, repo), sort_keys=True), encoding="utf-8")
    if settings:
        db = sqlite3.connect(destination)
        try:
            rows = db.execute("SELECT key, value, updated_at FROM instance_setting ORDER BY key").fetchall()
        finally:
            db.close()
        settings[0].write_text(json.dumps(rows, ensure_ascii=False), encoding="utf-8")
