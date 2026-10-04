#!/usr/bin/env python3
"""File-backed SQLite regression and real-copy migration checks. No original DB writes."""
import argparse
import json
import pathlib
import shutil
import sqlite3
import subprocess
import time
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
ROOT = None
SCRATCH = None
EXTRA_MIGRATIONS = []


def find_repo():
    for path in (pathlib.Path.cwd(), *HERE.parents):
        if (path / 'apps/server/migrations').is_dir():
            return path
    raise SystemExit('Pass --repo with the repository root.')


def migrations():
    files = {int(p.name.split('_')[0]): p for p in (ROOT / 'apps/server/migrations').glob('*.sql')}
    for directory in EXTRA_MIGRATIONS:
        for p in directory.glob('*.sql'):
            number = int(p.name.split('_')[0])
            if number in files and files[number].read_bytes() != p.read_bytes():
                raise ValueError(f'migration {number} has conflicting contents: {files[number]} and {p}')
            files[number] = p
    return sorted(files.items())


def connect(path):
    conn = sqlite3.connect(path, cached_statements=0)
    conn.execute('PRAGMA foreign_keys=ON')
    assert conn.execute('PRAGMA foreign_keys').fetchone() == (1,)
    return conn

def apply(conn, path):
    start = time.perf_counter()
    try:
        conn.executescript('BEGIN IMMEDIATE;\n' + path.read_text() + '\nCOMMIT;')
    except BaseException:
        if conn.in_transaction:
            conn.rollback()
        raise
    return round(time.perf_counter() - start, 6)

def empty(path, through=120):
    path.unlink(missing_ok=True)
    conn = connect(path)
    timing = {}
    for number, file in migrations():
        if number <= through:
            timing[file.name] = apply(conn, file)
    return conn, timing

class Migrations(unittest.TestCase):
    def setUp(self):
        self.conn, self.before_timing = empty(SCRATCH / 'fixture.sqlite3', through=118)
        self.conn.execute("INSERT INTO users(id,username,email,password_hash,created_at) VALUES('u1','one','Alice@example.test','hash','2026-01-01'),('u2','two','alice@example.test','hash','2026-01-01')")
        self.conn.execute("INSERT INTO canonical_library(user_id,work_id,created_at) VALUES('u1','w_legacy','2026-01-01')")
        self.conn.execute("INSERT INTO series_scan_state(series_id,updated_at) VALUES('legacy_null','2026-01-01')")
        self.conn.execute("INSERT INTO suwayomi_series(id,title,status,source_id,updated_at,created_at) VALUES(7,'Seven','ONGOING','known','2026-10-01','1784280205'),(8,'Eight','ONGOING','known','2026-10-01','1784280205123'),(9,'Nine','ONGOING','known','2026-10-01','2026-07-18T13:13:26.100490+00:00')")
        self.conn.execute("INSERT INTO work(id,primary_title,created_at,updated_at) VALUES('w_old','Old rich title','2026-01-01','2026-01-01'),('w_keep','Seven','2026-02-01','2026-02-01'),('w_other','Other','2026-02-01','2026-02-01')")
        self.conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,is_nsfw,created_at) VALUES('ss_old','w_old','suwayomi','','7',1,'2026-01-01'),('ss_keep','w_keep','suwayomi','known','7',0,'2026-02-01'),('ss_other','w_old','suwayomi','known','17',0,'2026-02-01'),('ss_md','w_keep','mangadex','mangadex','md-seven',0,'2026-02-01'),('ss_unknown','w_other','suwayomi','','99',0,'2026-02-01')")
        self.conn.execute("INSERT INTO chapter(id,source_series_id,external_id,number,title,created_at,chapter_key,label) VALUES('ch_old_shared','ss_old','shared','1','old','2026-01-01','100','1'),('ch_keep_shared','ss_keep','shared','1','keeper','2026-02-01','100','1'),('ch_old_unique','ss_old','old-only','2','old unique','2026-01-01','200','2'),('ch_keep_unique','ss_keep','keep-only','3','keep unique','2026-02-01','300','3'),('ch_other','ss_other','other','1','other','2026-02-01','100','1')")
        self.conn.execute("INSERT INTO merge_candidate(id,source_series_id,candidate_work_id,score,method,created_at) VALUES('mc_old','ss_old','w_other',0.8,'title_exact','2026-01-01'),('mc_keep','ss_keep','w_other',0.9,'cover','2026-02-01')")
        self.conn.execute("INSERT INTO release_event(work_id,chapter_key,first_seen_at,first_source_series_id,label) VALUES('w_old','100',1,'ss_old','1')")
        self.conn.execute("INSERT INTO canonical_progress(user_id,chapter_id,work_id,last_page_read,updated_at) VALUES('u1','shared','w_old',15,'2026-01-01')")
        self.conn.commit()
        self.assertEqual(self.conn.execute('PRAGMA foreign_key_check').fetchall(), [])
        self.after_timing = {}
        for number, file in migrations():
            if number > 118:
                self.after_timing[file.name] = apply(self.conn, file)

    def tearDown(self):
        self.conn.close()

    def test_email_guards_preserve_legacy_collisions_and_block_new_ones(self):
        self.assertEqual(self.conn.execute('SELECT COUNT(*) FROM users').fetchone(), (2,))
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'users.email'):
            self.conn.execute("INSERT INTO users(id,username,email,password_hash,created_at) VALUES('u3','three','ALICE@example.test','hash','now')")
        self.conn.execute("UPDATE users SET email=email WHERE id='u1'")
        self.conn.execute("INSERT INTO users(id,username,email,password_hash,created_at) VALUES('u3','three','other@example.test','hash','now')")
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'users.email'):
            self.conn.execute("UPDATE users SET email='ALICE@example.test' WHERE id='u3'")

    def test_scan_nulls_backfill_and_future_insert_update_rejected(self):
        self.assertEqual(self.conn.execute("SELECT next_scan_at FROM series_scan_state WHERE series_id='legacy_null'").fetchone(), ('1970-01-01T00:00:00+00:00',))
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'next_scan_at'):
            self.conn.execute("INSERT INTO series_scan_state(series_id,updated_at) VALUES('new_null','now')")
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'next_scan_at'):
            self.conn.execute("UPDATE series_scan_state SET next_scan_at=NULL WHERE series_id='legacy_null'")
        self.conn.execute("INSERT INTO series_scan_state(series_id,next_scan_at,updated_at) VALUES('valid','1970-01-01T00:00:00+00:00','now')")

    def test_only_created_at_epoch_values_change(self):
        rows = self.conn.execute('SELECT id,created_at,updated_at FROM suwayomi_series ORDER BY id').fetchall()
        self.assertEqual(rows, [(7,'2026-07-17T09:23:25.000Z','2026-10-01'),(8,'2026-07-17T09:23:25.123Z','2026-10-01'),(9,'2026-07-18T13:13:26.100490+00:00','2026-10-01')])

    def test_retired_library_rows_preserved(self):
        self.assertEqual(self.conn.execute("SELECT series_id,created_at FROM user_library WHERE user_id='u1'").fetchall(), [('w_legacy','2026-01-01')])
        self.assertEqual(self.conn.execute("SELECT name FROM sqlite_schema WHERE name='canonical_library'").fetchall(), [])

    def test_duplicate_collapse_preserves_union_and_every_fk(self):
        self.assertEqual(self.conn.execute("SELECT id,work_id,source_id,is_nsfw,created_at FROM source_series WHERE source_type='suwayomi' AND source_key='7'").fetchall(), [('ss_keep','w_keep','known',1,'2026-01-01')])
        self.assertEqual(self.conn.execute("SELECT id,external_id FROM chapter WHERE source_series_id='ss_keep' ORDER BY external_id").fetchall(), [('ch_keep_unique','keep-only'),('ch_old_unique','old-only'),('ch_keep_shared','shared')])
        self.assertEqual(self.conn.execute('SELECT id,source_series_id FROM merge_candidate ORDER BY id').fetchall(), [('mc_keep','ss_keep'),('mc_old','ss_keep')])
        self.assertEqual(self.conn.execute("SELECT created_at FROM chapter WHERE id='ch_keep_shared'").fetchone(), ('2026-01-01',))
        self.assertEqual(self.conn.execute('PRAGMA foreign_key_check').fetchall(), [])
        self.assertEqual(self.conn.execute('SELECT COUNT(*) FROM work').fetchone(), (3,))
        self.assertEqual(self.conn.execute("SELECT work_id FROM source_series WHERE id='ss_other'").fetchone(), ('w_old',))
        self.assertEqual(self.conn.execute('SELECT chapter_id,work_id,last_page_read FROM canonical_progress').fetchone(), ('shared','w_old',15))
        self.assertEqual(self.conn.execute("SELECT work_id,first_source_series_id FROM release_event WHERE work_id='w_old'").fetchone(), ('w_old','ss_old'))

    def test_source_null_normalization_and_unique_global_manga_key(self):
        self.assertIsNone(self.conn.execute("SELECT source_id FROM source_series WHERE id='ss_unknown'").fetchone()[0])
        self.conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('new_unknown','w_other','suwayomi','','100','now')")
        self.assertIsNone(self.conn.execute("SELECT source_id FROM source_series WHERE id='new_unknown'").fetchone()[0])
        self.conn.execute("UPDATE source_series SET source_id='' WHERE id='ss_keep'")
        self.assertIsNone(self.conn.execute("SELECT source_id FROM source_series WHERE id='ss_keep'").fetchone()[0])
        with self.assertRaises(sqlite3.IntegrityError):
            self.conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('dup','w_other','suwayomi','different','7','now')")
        self.assertIn('idx_source_series_type_created', {r[1] for r in self.conn.execute("PRAGMA index_list('source_series')")})

    def test_generic_unknown_source_key_is_unique(self):
        self.conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('generic1','w_other','other',NULL,'unknown','now')")
        with self.assertRaises(sqlite3.IntegrityError):
            self.conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('generic2','w_other','other',NULL,'unknown','now')")
        with self.assertRaises(sqlite3.IntegrityError):
            self.conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('generic3','w_other','other','','unknown','now')")

    def test_reparented_chapters_keep_release_history_without_reannouncement(self):
        conn, _ = empty(SCRATCH / 'history.sqlite3', through=118)
        try:
            conn.execute("INSERT INTO work(id,created_at,updated_at) VALUES('history_old','2026-01-01','2026-01-01'),('history_keep','2026-08-01','2026-08-01')")
            conn.execute("INSERT INTO suwayomi_series(id,title,status,source_id,updated_at) VALUES(77,'History','ONGOING','known','now')")
            conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('history_old_source','history_old','suwayomi','','77','2026-01-01T00:00:00Z'),('history_keep_source','history_keep','suwayomi','known','77','2026-08-01T00:00:00Z'),('history_other_source','history_old','suwayomi','known','78','2026-01-01T00:00:00Z')")
            conn.execute("INSERT INTO chapter(id,source_series_id,external_id,chapter_key,number,published_at,created_at) VALUES('history_old_shared','history_old_source','shared','100','1','2026-07-01T00:00:00Z','2026-07-01'),('history_keep_shared','history_keep_source','shared','100','1','2026-07-02T00:00:00Z','2026-07-02'),('history_old_unique','history_old_source','old-only','200','2','2026-07-01T00:00:00Z','2026-07-01'),('history_other','history_other_source','unrelated','900','9','2026-07-01T00:00:00Z','2026-07-01'),('history_old_changed_key','history_old_source','changed-key','400','4','2026-07-01T00:00:00Z','2026-07-01'),('history_keep_changed_key','history_keep_source','changed-key','500','5','2026-07-01T00:00:00Z','2026-07-01')")
            conn.execute("INSERT INTO chapter(id,source_series_id,external_id,chapter_key,number,published_at,created_at) VALUES('history_old_later','history_old_source','later','600','6','2026-07-02T00:00:00Z','2026-07-02'),('history_keep_earlier','history_keep_source','later','600','6','2026-07-01T00:00:00Z','2026-07-01')")
            conn.execute("INSERT INTO release_event(work_id,chapter_key,first_seen_at,first_source_series_id,label,announce_at) VALUES('history_old','100',1782864000000,'history_old_source','old shared',NULL),('history_old','200',1782864000000,'history_old_source','old unique',1782867600000),('history_old','900',1782864000000,'history_other_source','unrelated',1782864000000),('history_old','400',1782864000000,'history_old_source','changed key',1782864000000),('history_keep','100',1782950400000,'history_keep_source','later shared',1783036800000)")
            conn.execute("INSERT INTO release_event(work_id,chapter_key,first_seen_at,first_source_series_id,label,announce_at) VALUES('history_old','600',1782950400000,'history_old_source','later',1783036800000),('history_keep','600',1782864000000,'history_keep_source','earlier',NULL)")
            old_events = conn.execute("SELECT * FROM release_event WHERE work_id='history_old' ORDER BY chapter_key").fetchall()
            conn.commit()
            for number, file in migrations():
                if number > 118:
                    apply(conn, file)
            self.assertEqual(conn.execute("SELECT * FROM release_event WHERE work_id='history_old' ORDER BY chapter_key").fetchall(), old_events)
            self.assertEqual(conn.execute("SELECT chapter_key,first_seen_at,first_source_series_id,label,announce_at FROM release_event WHERE work_id='history_keep' ORDER BY chapter_key").fetchall(), [('100',1782864000000,'history_old_source','old shared',1782864000000),('200',1782864000000,'history_old_source','old unique',1782867600000),('600',1782864000000,'history_keep_source','earlier',1782864000000)])
            # The ordinary seed's missing-history predicate must not select the moved
            # back catalogue: its now-based announcement gate would float it as new.
            self.assertEqual(conn.execute("SELECT ss.work_id,c.chapter_key FROM chapter c JOIN source_series ss ON ss.id=c.source_series_id WHERE c.id='history_old_unique'").fetchone(), ('history_keep','200'))
            self.assertEqual(conn.execute('''SELECT ss.work_id FROM source_series ss
                JOIN chapter c ON c.source_series_id=ss.id
                WHERE c.id='history_old_unique' AND c.chapter_key IS NOT NULL
                  AND COALESCE(c.readable_at,c.published_at) IS NOT NULL
                  AND strftime('%s',COALESCE(c.readable_at,c.published_at)) IS NOT NULL
                  AND CAST(strftime('%s',COALESCE(c.readable_at,c.published_at)) AS INTEGER) * 1000 <= ?
                  AND NOT EXISTS(SELECT 1 FROM release_event re
                      WHERE re.work_id=ss.work_id AND re.chapter_key=c.chapter_key)''', (1791456000000,)).fetchall(), [])
            self.assertEqual(conn.execute('PRAGMA foreign_key_check').fetchall(), [])
        finally:
            conn.close()

    def test_failed_parent_rebuild_rolls_back_all_children(self):
        conn, _ = empty(SCRATCH / 'rollback.sqlite3', through=118)
        conn.execute("INSERT INTO work(id,created_at,updated_at) VALUES('rollback_work','now','now')")
        conn.execute("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('rollback_source','rollback_work','suwayomi','known','1','now')")
        conn.execute("INSERT INTO chapter(id,source_series_id,external_id,number,created_at) VALUES('rollback_chapter','rollback_source','ch','1','now')")
        conn.execute("INSERT INTO merge_candidate(id,source_series_id,candidate_work_id,score,method,created_at) VALUES('rollback_candidate','rollback_source','rollback_work',1,'title_exact','now')")
        conn.commit()
        file = next(file for number, file in migrations() if number == 120)
        sql = file.read_text()
        marker = 'DROP TABLE source_series;'
        self.assertIn(marker, sql)
        # Keep BEGIN inside executescript, exactly as in apply().
        try:
            with self.assertRaises(sqlite3.OperationalError):
                conn.executescript('BEGIN IMMEDIATE;\n' + sql.replace(marker, marker + '\nSELECT * FROM deliberately_missing_table;', 1))
            conn.rollback()
            for table in ('source_series', 'chapter', 'merge_candidate'):
                self.assertEqual(conn.execute('SELECT COUNT(*) FROM ' + table).fetchone(), (1,))
            self.assertEqual(conn.execute('PRAGMA foreign_key_check').fetchall(), [])
        finally:
            conn.close()

    def test_index_removals_additions_and_hinted_query(self):
        present = {r[0] for r in self.conn.execute("SELECT name FROM sqlite_schema WHERE type='index'")}
        for absent in ('idx_fsu_order','idx_fsu_browse_order','idx_work_tag_work','idx_chapter_override_work','idx_comment_votes_comment','idx_reviews_series','idx_source_series_type_key','idx_suwayomi_series_created_at'):
            self.assertNotIn(absent,present)
        for added in ('idx_merge_candidate_work','idx_merge_candidate_source','idx_reviews_user','idx_notifications_comment','uq_source_series_suwayomi_key','uq_source_series_unknown_key'):
            self.assertIn(added,present)
        self.conn.execute("SELECT ss.work_id,ss.source_key FROM source_series ss INDEXED BY idx_source_series_type_created WHERE ss.source_type='mangadex' ORDER BY ss.created_at LIMIT 1").fetchall()

def copy_database(source, destination):
    # APFS/reflink copies are fast when available; ordinary copy is portable.
    for command in (['cp', '-c', str(source), str(destination)],
                    ['cp', '--reflink=auto', str(source), str(destination)]):
        result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode == 0:
            return
    shutil.copy2(source, destination)


def run_checks(args):
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(Migrations)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        raise SystemExit(1)
    c, timing = empty(SCRATCH / 'empty-all.sqlite3', through=float('inf'))
    assert not c.execute('PRAGMA foreign_key_check').fetchall()
    c.close()
    report = {'regression_tests': result.testsRun, 'empty_all_files': len(timing),
              'empty_migration_seconds': timing,
              'empty_total_seconds': round(sum(timing.values()), 6)}
    if args.real_copy:
        real_path = SCRATCH / 'real-copy.sqlite3'
        if args.real_copy.resolve() == real_path.resolve():
            raise ValueError('--real-copy must refer to an input backup, outside the generated destination')
        real_path.unlink(missing_ok=True)
        copy_database(args.real_copy, real_path)
        c = connect(real_path)
        before = {table: c.execute('SELECT COUNT(*) FROM ' + table).fetchone()[0]
                  for table in ('source_series', 'chapter', 'merge_candidate', 'work',
                                'release_event', 'canonical_progress', 'user_library')}
        c.execute('CREATE TEMP TABLE _schema_safety_original_release_event AS SELECT * FROM release_event')
        natural_union = c.execute("SELECT COUNT(*) FROM (SELECT CASE WHEN ss.source_type='suwayomi' THEN 'suwayomi:'||ss.source_key ELSE ss.id END source,c.external_id FROM chapter c JOIN source_series ss ON ss.id=c.source_series_id GROUP BY source,c.external_id)").fetchone()[0]
        real_timing = {}
        for number, file in migrations():
            if number > args.real_through:
                real_timing[file.name] = apply(c, file)
        after = {table: c.execute('SELECT COUNT(*) FROM ' + table).fetchone()[0] for table in before}
        assert not c.execute('PRAGMA foreign_key_check').fetchall()
        assert c.execute("SELECT source_key FROM source_series WHERE source_type='suwayomi' GROUP BY source_key HAVING COUNT(*)>1").fetchall() == []
        assert c.execute("SELECT COUNT(*) FROM source_series WHERE source_id=''").fetchone() == (0,)
        assert c.execute('SELECT COUNT(*) FROM chapter').fetchone() == (natural_union,)
        assert c.execute("SELECT COUNT(*) FROM suwayomi_series WHERE length(created_at) IN (10,13) AND created_at NOT GLOB '*[^0-9]*'").fetchone() == (0,)
        for table in ('work', 'canonical_progress'):
            assert before[table] == after[table]
        # Source deduplication may add keeper-work history or retain earlier clocks
        # there. Every original key survives, and neither clock moves forward.
        assert after['release_event'] >= before['release_event']
        assert c.execute('''SELECT COUNT(*) FROM _schema_safety_original_release_event old
            LEFT JOIN release_event current
              ON current.work_id=old.work_id AND current.chapter_key=old.chapter_key
            WHERE current.work_id IS NULL OR current.first_seen_at > old.first_seen_at
               OR COALESCE(current.announce_at,current.first_seen_at)
                    > COALESCE(old.announce_at,old.first_seen_at)''').fetchone() == (0,)
        report.update({'real_migration_seconds': real_timing,
                       'real_total_seconds': round(sum(real_timing.values()), 6),
                       'real_before_counts': before, 'real_after_counts': after,
                       'expected_unique_chapters': natural_union,
                       'real_foreign_keys_on': c.execute('PRAGMA foreign_keys').fetchone()[0]})
        c.close()
    if args.report:
        args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))


def main():
    global ROOT, SCRATCH, EXTRA_MIGRATIONS
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=pathlib.Path, help='repository root; otherwise discovered from cwd/script')
    parser.add_argument('--extra-migrations', type=pathlib.Path, action='append', default=[],
                        help='additional migration directory, for staged integration checks')
    parser.add_argument('--scratch-dir', type=pathlib.Path, help='generated databases; defaults to a temporary directory')
    parser.add_argument('--real-copy', type=pathlib.Path,
                        help='consistent, already-created input backup; copied before any writes')
    parser.add_argument('--real-through', type=int, default=116,
                        help='highest migration already applied to --real-copy (default:116)')
    parser.add_argument('--report', type=pathlib.Path, help='optional JSON output path')
    args = parser.parse_args()
    ROOT = (args.repo or find_repo()).resolve()
    EXTRA_MIGRATIONS = args.extra_migrations
    if (HERE / 'migrations').is_dir():
        EXTRA_MIGRATIONS = [HERE / 'migrations', *EXTRA_MIGRATIONS]
    if args.scratch_dir:
        SCRATCH = args.scratch_dir.resolve()
        SCRATCH.mkdir(parents=True, exist_ok=True)
        run_checks(args)
    else:
        with tempfile.TemporaryDirectory(prefix='komiq-schema-test-') as directory:
            SCRATCH = pathlib.Path(directory)
            run_checks(args)


if __name__ == '__main__':
    main()
