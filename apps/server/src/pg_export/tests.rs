use super::{
    convert::{date_text, value, Value},
    export::{self, Arguments, Report},
    tables::{copied, is_copied, REBUILDABLE_POSTGRES_ONLY, TABLES, UNSUPPORTED_POSTGRES_ONLY},
    template,
};
use crate::pg_import::{
    conversion::{convert, Cell},
    plan::{self, Column},
    source::SQLITE,
};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection, SqliteConnection};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    path::{Path, PathBuf},
};

fn column(name: &str, target: &str, source: &str) -> Column {
    Column {
        name: name.into(),
        target: target.into(),
        source: source.into(),
        required: false,
        generated: false,
        identity: false,
        repair: false,
    }
}

/// The old value goes through the importer's rule and then through the
/// export's rule, as on the real path SQLite -> PostgreSQL -> SQLite.
fn round_trip(table: &str, c: &Column, storage: &str, old: &[u8]) -> (Cell, Value) {
    let cell = convert(c, storage, Some(old)).unwrap().0;
    let exported = value(table, c, &cell).unwrap();
    (cell, exported)
}

/// What the old build's readers make of an exported date: chrono's RFC 3339
/// parser (`DateTime::parse_from_rfc3339`), as microseconds.
fn old_build_reads(text: &str) -> i64 {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
        .timestamp_micros()
}

#[test]
fn dates_come_back_in_the_spelling_the_old_build_writes() {
    let c = column("created_at", "timestamptz", "TEXT");
    for (old, exported) in [
        // The old Linux build: 9 fraction digits. PostgreSQL keeps 6, cut, never rounded.
        (
            "2026-10-12T01:02:03.123456789+00:00",
            "2026-10-12T01:02:03.123456+00:00",
        ),
        (
            "2026-10-12T01:02:03.999999999+00:00",
            "2026-10-12T01:02:03.999999+00:00",
        ),
        // The old macOS build: 6 digits. Unchanged.
        (
            "2020-01-06T01:02:03.123456+00:00",
            "2020-01-06T01:02:03.123456+00:00",
        ),
        // Spellings the importer folds into one.
        ("2020-01-02T01:02:03Z", "2020-01-02T01:02:03+00:00"),
        ("2020-01-05T01:02:03.123Z", "2020-01-05T01:02:03.123+00:00"),
        (
            "2020-01-05T01:02:03.100000Z",
            "2020-01-05T01:02:03.100+00:00",
        ),
        ("2020-01-04T03:02:03+02:00", "2020-01-04T01:02:03+00:00"),
        (
            "2020-01-07T03:02:03.123456789+02:00",
            "2020-01-07T01:02:03.123456+00:00",
        ),
        // No zone means UTC (census: 7 values with 'T', and SQLite's datetime('now')).
        ("2020-01-08T01:02:03", "2020-01-08T01:02:03+00:00"),
        ("2020-01-01 01:02:03", "2020-01-01T01:02:03+00:00"),
        // Census oddities: the year 20250 and the 2037 marker.
        (
            "+20250-01-03T19:44:17.225+00:00",
            "+20250-01-03T19:44:17.225+00:00",
        ),
        ("2037-12-31T15:00:00+00:00", "2037-12-31T15:00:00+00:00"),
        // Before 1970.
        ("1126-07-18T04:07:11+00:00", "1126-07-18T04:07:11+00:00"),
    ] {
        let (cell, written) = round_trip("users", &c, "text", old.as_bytes());
        assert_eq!(written, Value::Text(exported.into()), "{old}");
        let Cell::Time(micros) = cell else {
            panic!("not a date")
        };
        // The importer reads the exported text as the instant PostgreSQL held.
        assert_eq!(
            convert(&c, "text", Some(exported.as_bytes())).unwrap(),
            (Cell::Time(micros), false),
            "{old}"
        );
        // And it is exactly what `to_rfc3339()` of the old build gives for it.
        assert_eq!(
            DateTime::from_timestamp_micros(micros)
                .unwrap()
                .to_rfc3339(),
            exported
        );
        if !exported.starts_with('+') {
            assert_eq!(old_build_reads(exported), micros, "{old}");
        }
    }
    // Text order is time order across the fraction widths the export writes,
    // as the old build's string comparisons need (`gc.rs`, pagination).
    let mut instants = [
        1_577_934_123_000_000i64,
        1_577_934_123_000_001,
        1_577_934_123_100_000,
        1_577_934_123_123_000,
        1_577_934_123_123_456,
        1_577_934_123_999_999,
        1_577_934_124_000_000,
    ]
    .map(|micros| (date_text("users", "created_at", micros).unwrap(), micros));
    instants.sort();
    assert!(instants.windows(2).all(|pair| pair[0].1 < pair[1].1));
}

#[test]
fn session_expiry_keeps_the_whole_second_z_spelling() {
    let c = column("expires_at", "timestamptz", "TEXT");
    // `auth::format_ts` of the old build: the bytes come back unchanged.
    for old in ["2999-01-01T00:00:00Z", "2020-02-01T00:00:00Z"] {
        assert_eq!(
            round_trip("sessions", &c, "text", old.as_bytes()).1,
            Value::Text(old.into())
        );
    }
    // The same column name in another table takes the common spelling.
    assert_eq!(
        round_trip("other", &c, "text", b"2999-01-01T00:00:00Z").1,
        Value::Text("2999-01-01T00:00:00+00:00".into())
    );
    // No build writes a fraction here. One that is there stays, exactly.
    let (cell, written) = round_trip("sessions", &c, "text", b"2026-11-11T08:09:10.5Z");
    assert_eq!(written, Value::Text("2026-11-11T08:09:10.500000Z".into()));
    assert_eq!(
        convert(&c, "text", Some(b"2026-11-11T08:09:10.500000Z"))
            .unwrap()
            .0,
        cell
    );
    // `session_user_for_token` compares strings: `expires_at > format_ts(now)`.
    let expires = date_text("sessions", "expires_at", 1_794_384_550_000_000).unwrap();
    assert_eq!(expires, "2026-11-11T08:09:10Z");
    assert!(expires.as_str() > "2026-11-11T08:09:09Z");
    assert!(expires.as_str() <= "2026-11-11T08:09:10Z");
}

#[test]
fn millisecond_epochs_come_back_as_the_same_integers() {
    let c = column("released_at", "timestamptz", "INTEGER");
    for ms in [
        1_767_225_600_225i64,
        2_145_884_400_000,
        100_000_000_000,
        0,
        -1,
        -1001,
        // Census: the years 1126 and 796.
        -26_616_935_569_000,
        -37_028_731_301_000,
    ] {
        assert_eq!(
            round_trip("release_event", &c, "integer", ms.to_string().as_bytes()).1,
            Value::Integer(ms),
            "{ms}"
        );
    }
    // PostgreSQL can hold a finer instant than the column can: refused, not rounded.
    assert!(value("release_event", &c, &Cell::Time(1_767_225_600_225_001)).is_err());
    assert!(value("release_event", &c, &Cell::Time(-1)).is_err());
}

#[test]
fn flags_uuids_numbers_bytes_text_and_json_come_back() {
    let flag = column("is_admin", "boolean", "INTEGER");
    assert_eq!(
        round_trip("users", &flag, "integer", b"0").1,
        Value::Integer(0)
    );
    assert_eq!(
        round_trip("users", &flag, "integer", b"1").1,
        Value::Integer(1)
    );

    // `Uuid::new_v4().to_string()`: lower case with hyphens. Other spellings fold into it.
    let id = column("id", "uuid", "TEXT");
    for old in [
        "abcdefab-1234-5678-abcd-1234567890ab",
        "ABCDEFAB-1234-5678-ABCD-1234567890AB",
        "ABCDEFAB12345678ABCD1234567890AB",
    ] {
        assert_eq!(
            round_trip("users", &id, "text", old.as_bytes()).1,
            Value::Text("abcdefab-1234-5678-abcd-1234567890ab".into())
        );
    }

    let wide = column("total", "bigint", "INTEGER");
    for n in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert_eq!(
            round_trip("series_views", &wide, "integer", n.to_string().as_bytes()).1,
            Value::Integer(n)
        );
    }
    let narrow = column("value", "integer", "INTEGER");
    assert_eq!(
        round_trip("comment_votes", &narrow, "integer", b"-1").1,
        Value::Integer(-1)
    );
    // `suwayomi_series.latest_chapter_at`: an integer the old build keeps as text.
    let text_integer = column("latest_chapter_at", "bigint", "TEXT");
    assert_eq!(
        round_trip("suwayomi_series", &text_integer, "text", b"1577926923123").1,
        Value::Text("1577926923123".into())
    );

    let real = column("override_interval_hours", "double precision", "REAL");
    for n in [18.5f64, 0.1, -2.5e-300, 1.7976931348623157e308] {
        let Value::Real(back) =
            round_trip("series_admin", &real, "real", n.to_string().as_bytes()).1
        else {
            panic!("not a real")
        };
        assert_eq!(back.to_bits(), n.to_bits());
    }

    let bytes = column("webp", "bytea", "BLOB");
    assert_eq!(
        round_trip("user_avatars", &bytes, "blob", &[0, 255, 10, 13]).1,
        Value::Blob(vec![0, 255, 10, 13])
    );

    let text = column("body", "text", "TEXT");
    let odd = "café 漫画 😀 \\ \t\n\r 'quote' \"double\"";
    assert_eq!(
        round_trip("comments", &text, "text", odd.as_bytes()).1,
        Value::Text(odd.into())
    );
    assert_eq!(
        round_trip("comments", &text, "text", b"").1,
        Value::Text(String::new())
    );
    assert_eq!(value("comments", &text, &Cell::Null).unwrap(), Value::Null);
    let mut required = column("body", "text", "TEXT");
    required.required = true;
    assert!(value("comments", &required, &Cell::Null).is_err());

    // jsonb comes back as PostgreSQL prints it. The old build parses it with
    // serde_json into the same value (`suwayomi_series.genre` is a string list).
    let json = column("genre", "jsonb", "TEXT");
    let printed = r#"["Action", "Slice of Life", "漫画"]"#;
    let cell = crate::pg_import::conversion::pg_cell(&json, Some(printed)).unwrap();
    let Value::Text(written) = value("suwayomi_series", &json, &cell).unwrap() else {
        panic!("not text")
    };
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&written).unwrap(),
        ["Action", "Slice of Life", "漫画"]
    );
    assert_eq!(
        convert(&json, "text", Some(written.as_bytes())).unwrap().0,
        convert(
            &json,
            "text",
            Some(r#"["Action","Slice of Life","漫画"]"#.as_bytes())
        )
        .unwrap()
        .0
    );
}

#[test]
fn classification_covers_the_import_plan() {
    let planned = plan::tables();
    assert_eq!(planned.len(), 63);
    assert_eq!(
        TABLES.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
        planned.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        "one line for each planned table, in plan order"
    );
    assert!(TABLES.iter().all(|(_, _, reason)| !reason.is_empty()));
    assert_eq!(
        UNSUPPORTED_POSTGRES_ONLY
            .iter()
            .chain(REBUILDABLE_POSTGRES_ONLY)
            .map(|(name, _)| *name)
            .collect::<BTreeSet<_>>(),
        plan::POSTGRES_ONLY
            .iter()
            .map(|table| table.name)
            .collect::<BTreeSet<_>>(),
        "every PostgreSQL-only table needs a rollback disposition"
    );
    assert!(UNSUPPORTED_POSTGRES_ONLY
        .iter()
        .chain(REBUILDABLE_POSTGRES_ONLY)
        .all(|(_, reason)| !reason.is_empty()));
    let unsupported: BTreeSet<_> = UNSUPPORTED_POSTGRES_ONLY.iter().map(|(n, _)| n).collect();
    let rebuildable: BTreeSet<_> = REBUILDABLE_POSTGRES_ONLY.iter().map(|(n, _)| n).collect();
    assert!(unsupported.is_disjoint(&rebuildable));
    assert_eq!(unsupported.len(), UNSUPPORTED_POSTGRES_ONLY.len());
    assert_eq!(rebuildable.len(), REBUILDABLE_POSTGRES_ONLY.len());
    assert_eq!(TABLES.iter().filter(|(_, copied, _)| *copied).count(), 24);
    // No table outside the copied set has a foreign key into it: replacing the
    // copied tables cannot break a row the export does not write.
    for table in planned.iter().filter(|t| !is_copied(&t.name)) {
        for fk in &table.fks {
            assert!(!is_copied(&fk.parent), "{} -> {}", table.name, fk.parent);
        }
    }
    // The only foreign key from a copied table to one that is not copied.
    let outward = planned
        .iter()
        .filter(|t| is_copied(&t.name))
        .flat_map(|t| {
            t.fks
                .iter()
                .filter(|fk| !is_copied(&fk.parent))
                .map(move |fk| (t.name.as_str(), fk.column.as_str(), fk.parent.as_str()))
        })
        .collect::<Vec<_>>();
    assert_eq!(outward, [("chapter_override", "work_id", "work")]);
    // No copied column uses the jsonb or the epoch rule today; the rules
    // exist for the day the list changes.
    for table in copied(122) {
        for c in table.copied() {
            assert!(c.target != "jsonb" && !(c.target == "timestamptz" && c.source == "INTEGER"));
        }
    }
    // A 0121 file has no `comments.deleted_by`.
    let has = |target: i64| {
        copied(target)
            .iter()
            .find(|t| t.name == "comments")
            .unwrap()
            .columns
            .iter()
            .any(|c| c.name == "deleted_by")
    };
    assert!(has(122) && !has(121));
}

/// A directory of this test's own, removed with everything in it.
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "komika-pg-export-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn migrator(level: i64) -> sqlx::migrate::Migrator {
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            SQLITE
                .iter()
                .filter(|m| m.version <= level)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
}

async fn sqlite(path: &Path, create: bool) -> SqliteConnection {
    use sqlx::ConnectOptions;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(create)
        .foreign_keys(true)
        .disable_statement_logging();
    SqliteConnection::connect_with(&options).await.unwrap()
}

const OWNER: &str = "00000000-0000-4000-8000-000000000900";
const READER: &str = "00000000-0000-4000-8000-000000000901";
const CJK: &str = "00000000-0000-4000-8000-000000000902";
const NEWCOMER: &str = "00000000-0000-4000-8000-000000000910";
const WORK: &str = "w_00000000000040008000000000000001";

/// A kept file as the old build leaves it: migrated to `level`, with the
/// golden seed (every date spelling of the census) and a row in every copied
/// table the seed does not fill.
async fn kept_file(scratch: &Scratch, level: i64) -> PathBuf {
    let path = scratch.file("kept.sqlite3");
    let mut db = sqlite(&path, true).await;
    migrator(level).run(&mut db).await.unwrap();
    for (id, name) in [(OWNER, "owner"), (READER, "reader"), (CJK, "cjk-reader")] {
        sqlx::query("INSERT INTO users (id,username,email,password_hash,created_at) VALUES ($1,$2,$3,'fixture-secret','2020-01-01T00:00:00Z')")
            .bind(id).bind(name).bind(format!("{name}@example.invalid"))
            .execute(&mut db).await.unwrap();
    }
    sqlx::raw_sql(include_str!("../../tests/postgres_golden/seed_sqlite.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql(&format!(
        "UPDATE users SET is_admin=1, avatar_url='/avatar/{OWNER}?v=1767225600225', display_name='Owner', bio='bio — text', created_at='2020-01-01T00:00:00Z' WHERE id='{OWNER}';
         UPDATE users SET created_at='2020-01-02T03:04:05.123456789+00:00' WHERE id='{READER}';
         UPDATE users SET created_at='2020-01-03 04:05:06' WHERE id='{CJK}';
         INSERT INTO sessions (token,user_id,created_at,expires_at) VALUES
           ('{live}','{OWNER}','2026-10-01T10:00:00.123456789+00:00','2999-01-01T00:00:00Z'),
           ('{dead}','{READER}','2020-01-01T00:00:00+00:00','2020-02-01T00:00:00Z');
         INSERT INTO user_avatars (user_id,webp,version,updated_at) VALUES ('{OWNER}',x'524946460089ff00',1767225600225,'2026-10-01T10:00:01.000000001+00:00');
         INSERT INTO comment_media (id,comment_id,user_id,webp,width,height,created_at) VALUES ('00000000-0000-4000-8000-000000001400','00000000-0000-4000-8000-000000001102','{CJK}',x'00ff10',640,480,'2026-10-01T10:00:02.5+00:00');
         INSERT INTO report (id,kind,status,user_id,subject_id,detail,admin_note,resolved_at,resolved_by,created_at) VALUES
           ('00000000-0000-4000-8000-000000001500','other','open','{READER}','{WORK}','Wrong cover',NULL,NULL,NULL,'2026-10-01T10:00:03.123456789+00:00'),
           ('00000000-0000-4000-8000-000000001501','other','resolved',NULL,NULL,'Anonymous','done','2026-10-02T10:00:03.987654321+00:00','{OWNER}','2026-10-01T11:00:00+00:00');
         INSERT INTO instance_setting (key,value,updated_at) VALUES ('mangadex_enabled','true','2026-10-01T10:00:04.123456789+00:00');
         INSERT INTO source_rate_limit (source_id,requests,window_secs,updated_at) VALUES ('source-710002',3,10,'2026-10-01T10:00:05.123456789+00:00');
         INSERT INTO source_nsfw_pin (source_id,created_at) VALUES ('source-710002','2026-10-01T10:00:06.123456789+00:00');
         INSERT INTO extension_subscription (pkg_name,created_at,last_synced_at,last_added,last_error,consecutive_failures,disabled_at) VALUES ('pkg.example','2026-09-01T00:00:00.123456789+00:00','2026-10-01T09:00:00.123456789+00:00',2,NULL,0,NULL);
         INSERT INTO series_view_bucket (series_key,hour_ts,views) VALUES ('710001',489000,5);",
        live = "a".repeat(64),
        dead = "b".repeat(64),
    ))
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    path
}

/// The cutover: PostgreSQL gets a copy of the kept file, brought to the
/// importer's level (0122) first when the kept file is at 0121. The kept file
/// itself is not touched. The real importer does the import.
async fn cut_over(scratch: &Scratch, kept: &Path, url: &str) {
    let copy = scratch.file("import-copy.sqlite3");
    std::fs::copy(kept, &copy).unwrap();
    let mut db = sqlite(&copy, false).await;
    SQLITE.run(&mut db).await.unwrap();
    db.close().await.unwrap();
    crate::pg_import::run_cli(&[
        "import-sqlite-to-postgres".into(),
        "--sqlite".into(),
        copy.to_string_lossy().into_owned(),
        "--postgres".into(),
        url.into(),
    ])
    .await
    .unwrap();
}

/// What users and the admin do on the PostgreSQL build after the cutover, in
/// the native types that build writes.
async fn user_writes(url: &str) {
    let mut pg = crate::pg_import::connect(url).await.unwrap();
    sqlx::raw_sql(&format!(
        "-- a new account, logged in
         INSERT INTO users (id,username,email,password_hash,avatar_url,is_admin,created_at,is_banned,show_nsfw,display_name,bio,must_change_password)
           VALUES ('{NEWCOMER}','Newcomer','newcomer@example.invalid','argon-fixture',NULL,FALSE,'2026-10-12 08:09:10.123456+00',FALSE,TRUE,NULL,NULL,FALSE);
         INSERT INTO sessions (token,user_id,created_at,expires_at) VALUES ('{token}','{NEWCOMER}','2026-10-12 08:09:10.123456+00','2026-11-11 08:09:10+00');
         -- library: one added, one removed, one added for a work made after the cutover
         INSERT INTO work (id,primary_title,is_nsfw,created_at,updated_at) VALUES ('w_made_after_cutover','New',FALSE,'2026-10-12 08:00:00+00','2026-10-12 08:00:00+00');
         INSERT INTO user_library (user_id,series_id,created_at,status,is_favorite) VALUES
           ('{NEWCOMER}','{WORK}','2026-10-12 08:10:00.5+00','reading',TRUE),
           ('{NEWCOMER}','w_made_after_cutover','2026-10-12 08:10:01+00',NULL,FALSE);
         DELETE FROM user_library WHERE user_id='{OWNER}' AND series_id='w_00000000000040008000000000000003';
         -- progress
         INSERT INTO canonical_progress (user_id,chapter_id,work_id,last_page_read,read,updated_at) VALUES ('{NEWCOMER}','00000000-0000-4000-8000-000000000101','{WORK}',3,FALSE,'2026-10-12 08:11:00.000001+00');
         UPDATE suwayomi_progress SET last_page_read=20, updated_at='2026-10-12 08:12:00+00' WHERE user_id='{OWNER}';
         -- a comment with a vote, an edit, a delete that keeps a reply, a delete that removes the row
         INSERT INTO comments (id,target_type,target_id,user_id,body,has_spoiler,created_at,parent_id) VALUES ('00000000-0000-4000-8000-000000001190','series','{WORK}','{NEWCOMER}','Written on PostgreSQL — 漫画',TRUE,'2026-10-12 08:13:00.654321+00',NULL);
         INSERT INTO comment_votes (comment_id,user_id,value,created_at) VALUES ('00000000-0000-4000-8000-000000001190','{OWNER}',1,'2026-10-12 08:13:30+00');
         UPDATE comments SET body='Another root, edited' WHERE id='00000000-0000-4000-8000-000000001102';
         DELETE FROM comment_votes WHERE comment_id='00000000-0000-4000-8000-000000001100';
         DELETE FROM notifications WHERE comment_id='00000000-0000-4000-8000-000000001100';
         UPDATE comments SET body='', has_spoiler=FALSE, deleted_by='author' WHERE id='00000000-0000-4000-8000-000000001100';
         DELETE FROM comments WHERE id='00000000-0000-4000-8000-000000001110';
         -- a review, a notification read
         INSERT INTO reviews (id,series_id,user_id,score,body,has_spoiler,created_at,updated_at) VALUES ('00000000-0000-4000-8000-000000001090','{WORK}','{NEWCOMER}',10,'Great',FALSE,'2026-10-12 08:14:00.1+00','2026-10-12 08:15:00+00');
         UPDATE notifications SET read_at='2026-10-12 09:00:00.5+00' WHERE id='00000000-0000-4000-8000-000000001301';
         -- a block rule made and removed again: the sequence moved on
         INSERT INTO content_block_rule (user_id,tags,ratings,created_at) VALUES ('{NEWCOMER}','x','','2026-10-12 08:16:00+00');
         DELETE FROM content_block_rule WHERE user_id='{NEWCOMER}';
         -- admin: one source pin removed and one made (the works of both sources are catalogue)
         DELETE FROM source_nsfw_pin WHERE source_id='source-710002';
         INSERT INTO source_nsfw_pin (source_id,created_at) VALUES ('source-710001','2026-10-12 08:19:00+00');
         -- admin: an alternative title added by hand, one removed, one on the work made after the cutover
         INSERT INTO work_alias (id,work_id,normalized_title,raw_title,lang) VALUES
           ('a_added_by_hand','{WORK}','hand made','Hand Made',NULL),
           ('a_of_the_new_work','w_made_after_cutover','new','New','en');
         DELETE FROM work_alias WHERE id='a_00000000000040008000000000000011';
         -- admin: a setting, and a chapter override on the work made after the cutover
         UPDATE instance_setting SET value='false', updated_at='2026-10-12 08:17:00+00' WHERE key='mangadex_enabled';
         INSERT INTO chapter_override (work_id,chapter_key,hidden,title_override,updated_at) VALUES ('w_made_after_cutover','100',TRUE,NULL,'2026-10-12 08:18:00+00');",
        token = "c".repeat(64),
    ))
    .execute(&mut pg)
    .await
    .unwrap();
    crate::pg_import::hang_up(pg).await;
}

fn arguments(scratch: &Scratch, kept: &Path, url: &str) -> Arguments {
    Arguments {
        template: kept.to_path_buf(),
        output: scratch.file("rolled-back.sqlite3"),
        postgres_url: url.into(),
    }
}

async fn text(db: &mut SqliteConnection, sql: &str) -> Vec<String> {
    sqlx::query_scalar(sql).fetch_all(db).await.unwrap()
}

/// No refusal leaves anything behind, and none touches the kept file.
fn assert_nothing_written(args: &Arguments, kept_sha256: &str) {
    assert!(!args.output.exists());
    assert!(!export::partial_path(&args.output).exists());
    assert_eq!(template::sha256(&args.template).unwrap(), kept_sha256);
}

async fn round_trip_at(level: i64) -> (Scratch, Report, Arguments) {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, level).await;
    let kept_sha256 = template::sha256(&kept).unwrap();
    cut_over(&scratch, &kept, &database.url).await;
    user_writes(&database.url).await;
    let args = arguments(&scratch, &kept, &database.url);
    let report = export::run(&args, None).await.unwrap();

    // The kept file is byte for byte what it was.
    assert_eq!(template::sha256(&kept).unwrap(), kept_sha256);
    assert_eq!(report.template.sha256, kept_sha256);
    assert!(report.complete);
    assert_eq!(report.target_sqlite_migration, level);
    assert!(!export::partial_path(&args.output).exists());
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!template::sibling(&args.output, suffix).exists());
    }
    // Accounts are in the file: no one but its owner reads it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&args.output)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let mut kept_db = sqlite(&kept, false).await;
    let mut out = sqlite(&args.output, false).await;

    // Local SQLite state.
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(&mut out)
        .await
        .unwrap();
    assert_eq!(integrity, ["ok"]);
    assert!(sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut out)
        .await
        .unwrap()
        .is_empty());
    const SCHEMA: &str = "SELECT type||' '||name||' '||tbl_name||' '||coalesce(sql,'') FROM sqlite_schema ORDER BY type,name";
    let schema = text(&mut out, SCHEMA).await;
    assert_eq!(schema, text(&mut kept_db, SCHEMA).await);
    assert_eq!(
        schema.iter().filter(|s| s.starts_with("trigger ")).count(),
        15
    );
    const HISTORY: &str = "SELECT version||' '||success||' '||hex(checksum)||' '||installed_on FROM _sqlx_migrations ORDER BY version";
    let history = text(&mut out, HISTORY).await;
    assert_eq!(history, text(&mut kept_db, HISTORY).await);
    assert!(history.last().unwrap().starts_with(&format!("{level} 1 ")));
    // The users triggers were dropped for the copy and are back at work.
    let clash = sqlx::query("INSERT INTO users (id,username,email,password_hash,created_at) VALUES ('x','NEWCOMER','x@example.invalid','x','2026-01-01T00:00:00+00:00')")
        .execute(&mut out).await.unwrap_err().to_string();
    assert!(
        clash.contains("UNIQUE constraint failed: users.username"),
        "{clash}"
    );
    // AUTOINCREMENT: ids 1 and 2 are in use, 3 was given out and deleted on PostgreSQL.
    assert_eq!(
        text(
            &mut out,
            "SELECT name||'='||seq FROM sqlite_sequence WHERE name='content_block_rule'"
        )
        .await,
        ["content_block_rule=3"]
    );
    assert_eq!(report.sequences["content_block_rule"], 3);

    // Exact stored values. `typeof` shows the storage class beside each value.
    assert_eq!(
        text(&mut out, "SELECT id||'|'||username||'|'||typeof(is_admin)||is_admin||'|'||typeof(show_nsfw)||show_nsfw||'|'||typeof(created_at)||'|'||created_at||'|'||coalesce(avatar_url,'NULL') FROM users ORDER BY id").await,
        [
            format!("{OWNER}|owner|integer1|integer0|text|2020-01-01T00:00:00+00:00|/avatar/{OWNER}?v=1767225600225"),
            format!("{READER}|reader|integer0|integer0|text|2020-01-02T03:04:05.123456+00:00|NULL"),
            format!("{CJK}|cjk-reader|integer0|integer1|text|2020-01-03T04:05:06+00:00|NULL"),
            format!("{NEWCOMER}|Newcomer|integer0|integer1|text|2026-10-12T08:09:10.123456+00:00|NULL"),
        ]
    );
    assert_eq!(
        text(&mut out, "SELECT substr(token,1,1)||'|'||user_id||'|'||created_at||'|'||expires_at FROM sessions ORDER BY token").await,
        [
            format!("a|{OWNER}|2026-10-01T10:00:00.123456+00:00|2999-01-01T00:00:00Z"),
            format!("b|{READER}|2020-01-01T00:00:00+00:00|2020-02-01T00:00:00Z"),
            format!("c|{NEWCOMER}|2026-10-12T08:09:10.123456+00:00|2026-11-11T08:09:10Z"),
        ]
    );
    assert_eq!(
        text(&mut out, &format!("SELECT series_id||'|'||created_at||'|'||coalesce(status,'NULL')||'|'||typeof(is_favorite)||is_favorite FROM user_library WHERE user_id IN ('{OWNER}','{NEWCOMER}') ORDER BY user_id,series_id")).await,
        [
            format!("{WORK}|2020-01-02T01:02:03+00:00|reading|integer1"),
            // The row for w_...03 was removed on PostgreSQL and is gone.
            "w_00000000000040008000000000000012|2020-01-05T01:02:03.123+00:00|NULL|integer0".to_owned(),
            "w_00000000000040008000000000000015|2020-01-08T01:02:03+00:00|NULL|integer0".to_owned(),
            format!("{WORK}|2026-10-12T08:10:00.500+00:00|reading|integer1"),
            // Kept, though the kept catalogue has no such work; the report names it.
            "w_made_after_cutover|2026-10-12T08:10:01+00:00|NULL|integer0".to_owned(),
        ]
    );
    assert_eq!(
        text(&mut out, "SELECT user_id||'|'||last_page_read||'|'||typeof(read)||read||'|'||updated_at FROM canonical_progress ORDER BY user_id").await,
        [
            format!("{OWNER}|8|integer0|2020-01-04T01:02:03+00:00"),
            format!("{READER}|9|integer1|2020-01-05T01:02:03.123+00:00"),
            format!("{CJK}|10|integer1|2020-01-06T01:02:03.123456+00:00"),
            format!("{NEWCOMER}|3|integer0|2026-10-12T08:11:00.000001+00:00"),
        ]
    );
    assert_eq!(
        text(&mut out, &format!("SELECT last_page_read||'|'||updated_at FROM suwayomi_progress WHERE user_id='{OWNER}'")).await,
        ["20|2026-10-12T08:12:00+00:00"]
    );
    let marker = if level >= 122 {
        "coalesce(deleted_by,'NULL')"
    } else {
        "'no column'"
    };
    let mark = |deleted: &str| {
        if level >= 122 {
            deleted.to_owned()
        } else {
            "no column".to_owned()
        }
    };
    assert_eq!(
        text(&mut out, &format!("SELECT substr(id,33)||'|'||body||'|'||typeof(has_spoiler)||has_spoiler||'|'||created_at||'|'||coalesce(parent_id,'NULL')||'|'||{marker} FROM comments ORDER BY id")).await,
        [
            // Deleted by its author on PostgreSQL; its reply keeps it as a tombstone.
            format!("1100||integer0|2020-01-01T01:02:03+00:00|NULL|{}", mark("author")),
            format!("1101|Reply — accents|integer1|2020-01-02T01:02:03+00:00|00000000-0000-4000-8000-000000001100|{}", mark("NULL")),
            format!("1102|Another root, edited|integer0|2020-01-03T01:02:03+00:00|NULL|{}", mark("NULL")),
            // 1110 was deleted outright.
            format!("1190|Written on PostgreSQL — 漫画|integer1|2026-10-12T08:13:00.654321+00:00|NULL|{}", mark("NULL")),
        ]
    );
    match &report.comment_tombstones_written_without_marker {
        Some(lost) => {
            assert_eq!(level, 121);
            assert_eq!(lost.count, 1);
            // Listed with the marker the file could not take.
            assert_eq!(
                lost.comments
                    .iter()
                    .map(|c| (c.comment_id.as_str(), c.deleted_by.as_str()))
                    .collect::<Vec<_>>(),
                [("00000000-0000-4000-8000-000000001100", "author")]
            );
        }
        None => assert_eq!(level, 122),
    }
    assert_eq!(
        text(&mut out, "SELECT substr(comment_id,33)||'|'||user_id||'|'||typeof(value)||value||'|'||created_at FROM comment_votes").await,
        [format!("1190|{OWNER}|integer1|2026-10-12T08:13:30+00:00")]
    );
    assert_eq!(
        text(&mut out, "SELECT substr(id,33)||'|'||hex(webp)||'|'||width||'x'||height||'|'||created_at FROM comment_media").await,
        ["1400|00FF10|640x480|2026-10-01T10:00:02.500+00:00"]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT typeof(webp)||'|'||hex(webp)||'|'||version||'|'||updated_at FROM user_avatars"
        )
        .await,
        ["blob|524946460089FF00|1767225600225|2026-10-01T10:00:01+00:00"]
    );
    assert_eq!(
        text(&mut out, "SELECT substr(id,33)||'|'||score||'|'||body||'|'||created_at||'|'||updated_at FROM reviews ORDER BY id").await,
        [
            "1000|6|Nice café|2020-01-01T01:02:03+00:00|2020-01-06T01:02:03.123456+00:00",
            "1001|7|Spoiler: 10.5|2020-01-02T01:02:03+00:00|2020-01-07T01:02:03.123456+00:00",
            "1002|8|漫画が好き|2020-01-03T01:02:03+00:00|2020-01-08T01:02:03+00:00",
            "1010|9|Legacy source review wins|2020-01-01T01:02:03+00:00|2020-02-01T00:00:00+00:00",
            "1090|10|Great|2026-10-12T08:14:00.100+00:00|2026-10-12T08:15:00+00:00",
        ]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT substr(id,33)||'|'||coalesce(read_at,'NULL') FROM notifications ORDER BY id"
        )
        .await,
        // 1300 pointed at the deleted comment and went with it.
        [
            "1301|2026-10-12T09:00:00.500+00:00",
            "1302|2020-01-08T01:02:03+00:00"
        ]
    );
    assert_eq!(
        text(&mut out, "SELECT substr(id,33)||'|'||status||'|'||coalesce(user_id,'NULL')||'|'||coalesce(resolved_at,'NULL')||'|'||coalesce(resolved_by,'NULL')||'|'||created_at FROM report ORDER BY id").await,
        [
            format!("1500|open|{READER}|NULL|NULL|2026-10-01T10:00:03.123456+00:00"),
            format!("1501|resolved|NULL|2026-10-02T10:00:03.987654+00:00|{OWNER}|2026-10-01T11:00:00+00:00"),
        ]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT key||'|'||value||'|'||updated_at FROM instance_setting"
        )
        .await,
        ["mangadex_enabled|false|2026-10-12T08:17:00+00:00"]
    );
    assert_eq!(
        text(&mut out, "SELECT series_id||'|'||typeof(override_interval_hours)||override_interval_hours||'|'||poll_every_minutes||'|'||typeof(paused_override)||paused_override||'|'||updated_at FROM series_admin").await,
        [format!("{WORK}|real18.5|45|integer0|2020-01-04T01:02:03+00:00")]
    );
    assert_eq!(
        text(&mut out, "SELECT pkg_name||'|'||created_at||'|'||last_synced_at||'|'||last_added||'|'||coalesce(disabled_at,'NULL') FROM extension_subscription").await,
        ["pkg.example|2026-09-01T00:00:00.123456+00:00|2026-10-01T09:00:00.123456+00:00|2|NULL"]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT series_key||'|'||hour_ts||'|'||views FROM series_view_bucket"
        )
        .await,
        ["710001|489000|5"]
    );
    assert_eq!(
        text(&mut out, "SELECT id||'|'||coalesce(user_id,'NULL')||'|'||tags||'|'||ratings||'|'||created_at FROM content_block_rule ORDER BY id").await,
        [
            format!("1|{READER}|gore||2020-01-01T01:02:03+00:00"),
            "2|NULL|romance|pornographic|2020-01-02T01:02:03+00:00".to_owned(),
        ]
    );
    assert_eq!(
        text(&mut out, "SELECT substr(id,33)||'|'||user_id||'|'||kind||'|'||target_type||'|'||target_id||'|'||created_at FROM user_activity ORDER BY id").await,
        [
            format!("1200|{OWNER}|review|series|{WORK}|2020-01-04T01:02:03+00:00"),
            format!("1201|{READER}|review|series|{WORK}|2020-01-05T01:02:03.123+00:00"),
            format!("1202|{CJK}|review|series|{WORK}|2020-01-06T01:02:03.123456+00:00"),
        ]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT series_key||'|'||typeof(total)||total||'|'||updated_at FROM series_views"
        )
        .await,
        ["710001|integer42|2020-01-06T01:02:03.123456+00:00"]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT name_key||'|'||name||'|'||ai_level||'|'||updated_at FROM scanlator_tag"
        )
        .await,
        ["alpha|Alpha|partial|2020-01-04T01:02:03+00:00"]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT source_id||'|'||ai_level||'|'||updated_at FROM source_ai_tag"
        )
        .await,
        ["source-710002|ai|2020-01-06T01:02:03.123456+00:00"]
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT source_id||'|'||created_at FROM source_nsfw_pin"
        )
        .await,
        ["source-710001|2026-10-12T08:19:00+00:00"]
    );
    // The pin rows are PostgreSQL's; the works they mark are the kept file's.
    // The report says which pins the admin applies again on the old build.
    assert_eq!(
        report.source_nsfw_pins_to_apply_again.pinned,
        ["source-710001"]
    );
    assert_eq!(
        report.source_nsfw_pins_to_apply_again.unpinned,
        ["source-710002"]
    );
    const WORKS: &str = "SELECT id||'|'||coalesce(is_nsfw_override,'NULL')||'|'||is_nsfw||'|'||updated_at FROM work ORDER BY id";
    assert_eq!(text(&mut out, WORKS).await, text(&mut kept_db, WORKS).await);
    // Alternative titles are the kept file's too. The report counts what an
    // admin, or the scanner, changed on PostgreSQL: one title added to a work
    // the kept file has, one removed. The title of the new work is not counted.
    const ALIASES: &str = "SELECT id||'|'||work_id||'|'||raw_title FROM work_alias ORDER BY id";
    assert_eq!(
        text(&mut out, ALIASES).await,
        text(&mut kept_db, ALIASES).await
    );
    assert_eq!(report.work_alias_changes_not_copied.added_on_kept_works, 1);
    assert_eq!(report.work_alias_changes_not_copied.removed, 1);
    assert_eq!(
        text(&mut out, "SELECT source_id||'|'||requests||'|'||window_secs||'|'||updated_at FROM source_rate_limit").await,
        ["source-710002|3|10|2026-10-01T10:00:05.123456+00:00"]
    );

    // References into the catalogue, which is not copied.
    assert_eq!(
        text(
            &mut out,
            "SELECT work_id||'|'||chapter_key FROM chapter_override"
        )
        .await,
        [format!("{WORK}|1050")]
    );
    let overrides = report
        .tables
        .iter()
        .find(|t| t.table == "chapter_override")
        .unwrap();
    assert_eq!((overrides.rows_read, overrides.rows_written), (2, 1));
    assert_eq!(overrides.left_out.len(), 1);
    assert_eq!(overrides.left_out[0].key["work_id"], "w_made_after_cutover");
    assert_eq!(overrides.left_out[0].key["chapter_key"], "100");
    assert_eq!(
        overrides.left_out[0].reason,
        "work_id is not in the kept file's work table"
    );
    assert!(overrides.left_out[0].kept_work_candidates.is_empty());
    assert_eq!(report.rows_left_out, 1);
    let library = report
        .unresolved_work_references
        .iter()
        .find(|r| r.table == "user_library")
        .unwrap();
    assert_eq!((library.rows, library.column.as_str()), (1, "series_id"));
    assert_eq!(library.missing_work_ids, ["w_made_after_cutover"]);
    // No merge leads from that work back to the kept file.
    assert!(library.kept_work_candidates.is_empty());
    assert!(report.work_references_rewritten.is_empty());
    // The catalogue is the kept file's: the work made on PostgreSQL is not there.
    assert_eq!(
        text(&mut out, "SELECT count(*)||'' FROM work").await,
        text(&mut kept_db, "SELECT count(*)||'' FROM work").await
    );
    assert_eq!(report.not_copied.len(), 63 - 24);
    assert_eq!(report.not_copied["work"], 15);

    // Every table of the report agrees with itself.
    assert_eq!(report.tables.len(), 24);
    for table in &report.tables {
        // The fixture has a row in every copied table.
        assert!(table.rows_written > 0, "{}", table.table);
        assert_eq!(table.postgres, table.sqlite, "{}", table.table);
        assert_eq!(table.rows_written, table.sqlite.rows, "{}", table.table);
        assert_eq!(
            table.rows_read,
            table.rows_written + table.left_out.len() as u64
        );
    }
    let users = report.tables.iter().find(|t| t.table == "users").unwrap();
    assert_eq!(
        (users.template_rows, users.rows_read, users.rows_written),
        (3, 4, 4)
    );
    // The report holds no stored secret.
    let printed = serde_json::to_string(&report).unwrap();
    for secret in [
        "fixture-secret",
        "argon-fixture",
        &"c".repeat(64),
        &database.url,
    ] {
        assert!(!printed.contains(secret));
    }
    out.close().await.unwrap();
    kept_db.close().await.unwrap();

    // The output is a source the real importer accepts in full: every value
    // in it passes the forward rules again.
    let again = crate::test_postgres::empty_database().unwrap();
    let second = Scratch::new();
    cut_over(&second, &args.output, &again.url).await;
    let mut first_pg = crate::pg_import::connect(&database.url).await.unwrap();
    let mut second_pg = crate::pg_import::connect(&again.url).await.unwrap();
    for table in copied(122) {
        let sql = format!(
            "SELECT coalesce(md5(string_agg(md5(t::text), '' ORDER BY md5(t::text))), '') FROM public.{} t{}",
            plan::quote(&table.name),
            // The one row the export left out.
            if table.name == "chapter_override" {
                " WHERE work_id <> 'w_made_after_cutover'"
            } else {
                ""
            }
        );
        let first: String = sqlx::query_scalar(&sql)
            .fetch_one(&mut first_pg)
            .await
            .unwrap();
        let second: String = sqlx::query_scalar(&sql)
            .fetch_one(&mut second_pg)
            .await
            .unwrap();
        if table.name == "comments" && level == 121 {
            assert_ne!(
                first, second,
                "the tombstone marker cannot survive a 0121 file"
            );
        } else {
            assert_eq!(first, second, "{} after a second import", table.name);
        }
    }
    crate::pg_import::hang_up(first_pg).await;
    crate::pg_import::hang_up(second_pg).await;
    (scratch, report, args)
}

#[tokio::test]
async fn round_trip_into_a_0121_file() {
    round_trip_at(121).await;
}

#[tokio::test]
async fn round_trip_into_a_0122_file() {
    round_trip_at(122).await;
}

/// The PostgreSQL build merges works the kept file has into works made after
/// the cutover, with its own merge (`catalog::merge_works_on`). The kept file
/// has only the merged works.
#[tokio::test]
async fn a_merge_on_postgres_leads_back_to_the_kept_work() {
    const W03: &str = "w_00000000000040008000000000000003";
    const W12: &str = "w_00000000000040008000000000000012";
    const W15: &str = "w_00000000000040008000000000000015";
    const OF_ONE: &str = "w_later_survivor_of_one";
    const OF_TWO: &str = "w_later_survivor_of_two";
    const WITH_CLASH: &str = "w_later_survivor_with_a_clash";
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    let kept_sha256 = template::sha256(&kept).unwrap();
    cut_over(&scratch, &kept, &database.url).await;
    let mut pg = crate::pg_import::connect(&database.url).await.unwrap();
    for id in [OF_ONE, OF_TWO, WITH_CLASH] {
        sqlx::query("INSERT INTO work (id,primary_title,is_nsfw,created_at,updated_at) VALUES ($1,'Later',FALSE,'2026-10-12 08:00:00+00','2026-10-12 08:00:00+00')")
            .bind(id)
            .execute(&mut pg)
            .await
            .unwrap();
    }
    for (merged, survivor) in [
        // One kept work into a new one: its rows can go back to it.
        (WORK, OF_ONE),
        // Two kept works into a new one: no one can tell which a row was for.
        (W12, OF_TWO),
        (W15, OF_TWO),
        (W03, WITH_CLASH),
    ] {
        let mut tx = pg.begin().await.unwrap();
        crate::catalog::merge_works_on(&mut tx, merged, survivor, None, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    sqlx::raw_sql(&format!(
        "-- a row under the merged id as well: going back would repeat (user, series)
         INSERT INTO user_library (user_id,series_id,created_at,status,is_favorite) VALUES ('{OWNER}','{W03}','2026-10-12 09:00:00+00',NULL,FALSE);
         -- admin: chapter overrides on two of the surviving works
         INSERT INTO chapter_override (work_id,chapter_key,hidden,title_override,updated_at) VALUES
           ('{OF_ONE}','7',TRUE,NULL,'2026-10-12 09:01:00+00'),
           ('{OF_TWO}','8',TRUE,NULL,'2026-10-12 09:02:00+00');"
    ))
    .execute(&mut pg)
    .await
    .unwrap();
    // The merge did what this test is about: no user row is left on the merged work.
    let moved: i64 = sqlx::query_scalar(&format!(
        "SELECT (SELECT count(*) FROM user_library WHERE series_id='{OF_ONE}') + (SELECT count(*) FROM reviews WHERE series_id='{WORK}')"
    ))
    .fetch_one(&mut pg)
    .await
    .unwrap();
    assert_eq!(moved, 3);
    crate::pg_import::hang_up(pg).await;

    let args = arguments(&scratch, &kept, &database.url);
    let report = export::run(&args, None).await.unwrap();
    assert!(report.complete);
    assert_eq!(template::sha256(&kept).unwrap(), kept_sha256);

    // One candidate: every row has the kept work's id again, and is listed.
    let rewritten = report
        .work_references_rewritten
        .iter()
        .map(|r| {
            assert_eq!(
                (r.postgres_work_id.as_str(), r.kept_work_id.as_str()),
                (OF_ONE, WORK)
            );
            (r.table.as_str(), r.column.as_str(), r.rows)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rewritten,
        [
            // The merge moved the kept override '1050' to the new work, and
            // the admin made '7' there.
            ("chapter_override", "work_id", 2),
            ("user_library", "series_id", 3),
            ("reviews", "series_id", 3),
            ("canonical_progress", "work_id", 3),
            ("comments", "target_id", 3),
            ("notifications", "target_id", 3),
            ("user_activity", "target_id", 3),
        ]
    );
    let mut out = sqlite(&args.output, false).await;
    let dead = format!(
        "SELECT ((SELECT count(*) FROM user_library WHERE series_id='{OF_ONE}') + (SELECT count(*) FROM reviews WHERE series_id='{OF_ONE}') \
         + (SELECT count(*) FROM canonical_progress WHERE work_id='{OF_ONE}') + (SELECT count(*) FROM comments WHERE target_id='{OF_ONE}') \
         + (SELECT count(*) FROM notifications WHERE target_id='{OF_ONE}') + (SELECT count(*) FROM user_activity WHERE target_id='{OF_ONE}')) || ''"
    );
    assert_eq!(text(&mut out, &dead).await, ["0"]);
    assert_eq!(
        text(&mut out, &format!("SELECT user_id||'|'||last_page_read FROM canonical_progress WHERE work_id='{WORK}' ORDER BY user_id")).await,
        [format!("{OWNER}|8"), format!("{READER}|9"), format!("{CJK}|10")]
    );
    assert_eq!(
        text(
            &mut out,
            &format!("SELECT substr(id,33) FROM reviews WHERE series_id='{WORK}' ORDER BY id")
        )
        .await,
        ["1000", "1001", "1002"]
    );

    // Two candidates, and one candidate whose id already has the row: the
    // rows keep the id PostgreSQL gave them, and the report names the kept works.
    assert_eq!(
        text(
            &mut out,
            &format!(
                "SELECT series_id FROM user_library WHERE user_id='{OWNER}' ORDER BY series_id"
            )
        )
        .await,
        [WORK, W03, OF_TWO, WITH_CLASH]
    );
    assert_eq!(report.unresolved_work_references.len(), 1);
    let library = &report.unresolved_work_references[0];
    assert_eq!(
        (
            library.table.as_str(),
            library.column.as_str(),
            library.rows
        ),
        ("user_library", "series_id", 6)
    );
    assert_eq!(library.missing_work_ids, [OF_TWO, WITH_CLASH]);
    assert_eq!(library.kept_work_candidates[OF_TWO], [W12, W15]);
    assert_eq!(library.kept_work_candidates[WITH_CLASH], [W03]);

    // The override on a missing work cannot be in the file. Its line in the
    // report names the kept works as well.
    let overrides = report
        .tables
        .iter()
        .find(|t| t.table == "chapter_override")
        .unwrap();
    let left_out = overrides
        .left_out
        .iter()
        .map(|left| {
            (
                left.key["chapter_key"].as_str(),
                left.kept_work_candidates.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(left_out, [("8", vec![W12.to_owned(), W15.to_owned()])]);
    assert_eq!(
        (overrides.rows_read, overrides.rows_written),
        (3, 2),
        "an override whose work has one kept candidate is written for that work"
    );
    assert_eq!(
        text(
            &mut out,
            "SELECT work_id||'|'||chapter_key||'|'||hidden FROM chapter_override ORDER BY chapter_key"
        )
        .await,
        [format!("{WORK}|1050|0"), format!("{WORK}|7|1")]
    );

    // The comparison with PostgreSQL was made before the rewrite, on every row.
    for table in &report.tables {
        assert_eq!(table.postgres, table.sqlite, "{}", table.table);
    }
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(&mut out)
        .await
        .unwrap();
    assert_eq!(integrity, ["ok"]);
    assert!(sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut out)
        .await
        .unwrap()
        .is_empty());
    out.close().await.unwrap();
}

/// PostgreSQL holds a chapter override for a kept work and one with the same
/// chapter key for the work it was merged into. Both would get the kept
/// work's key. The kept work's own row is written and the other is left out
/// and listed, in whichever order PostgreSQL returns the two.
#[tokio::test]
async fn a_kept_override_wins_its_key_in_either_read_order() {
    const SURVIVOR: &str = "w_later_survivor";
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    cut_over(&scratch, &kept, &database.url).await;
    let mut pg = crate::pg_import::connect(&database.url).await.unwrap();
    // The kept work is still a work on PostgreSQL, and a redirect leads from
    // it to a newer work: no merge leaves this behind, but the export must
    // describe it, not stop on it.
    sqlx::raw_sql(&format!(
        "INSERT INTO work (id,primary_title,is_nsfw,created_at,updated_at) VALUES ('{SURVIVOR}','Later',FALSE,'2026-10-12 08:00:00+00','2026-10-12 08:00:00+00');
         INSERT INTO work_redirect (old_id,new_id,created_at) VALUES ('{WORK}','{SURVIVOR}','2026-10-12 08:01:00+00');
         INSERT INTO chapter_override (work_id,chapter_key,hidden,title_override,updated_at) VALUES
           ('{SURVIVOR}','1050',TRUE,'the survivor''s row','2026-10-12 08:02:00+00'),
           ('{SURVIVOR}','7',TRUE,NULL,'2026-10-12 08:03:00+00');"
    ))
    .execute(&mut pg)
    .await
    .unwrap();
    for survivor_first in [false, true] {
        if survivor_first {
            // The kept work's row moves behind the survivor's rows.
            sqlx::raw_sql(&format!(
                "CREATE TEMP TABLE own AS SELECT * FROM chapter_override WHERE work_id='{WORK}';
                 DELETE FROM chapter_override WHERE work_id='{WORK}';
                 INSERT INTO chapter_override SELECT * FROM own;"
            ))
            .execute(&mut pg)
            .await
            .unwrap();
        }
        let order: Vec<String> =
            sqlx::query_scalar("SELECT work_id FROM chapter_override WHERE chapter_key='1050'")
                .fetch_all(&mut pg)
                .await
                .unwrap();
        assert_eq!(
            order,
            if survivor_first {
                [SURVIVOR, WORK]
            } else {
                [WORK, SURVIVOR]
            },
            "the order PostgreSQL returns the two rows in"
        );

        let mut args = arguments(&scratch, &kept, &database.url);
        args.output = scratch.file(&format!("rolled-back-{survivor_first}.sqlite3"));
        let report = export::run(&args, None).await.unwrap();
        assert!(report.complete);
        let mut out = sqlite(&args.output, false).await;
        assert_eq!(
            text(
                &mut out,
                "SELECT work_id||'|'||chapter_key||'|'||hidden||'|'||coalesce(title_override,'NULL') FROM chapter_override ORDER BY chapter_key"
            )
            .await,
            [
                // The kept work's own row, not the survivor's.
                format!("{WORK}|1050|0|Curated half chapter"),
                format!("{WORK}|7|1|NULL"),
            ]
        );
        out.close().await.unwrap();
        let overrides = report
            .tables
            .iter()
            .find(|t| t.table == "chapter_override")
            .unwrap();
        assert_eq!((overrides.rows_read, overrides.rows_written), (3, 2));
        assert_eq!(overrides.postgres, overrides.sqlite);
        assert_eq!(overrides.left_out.len(), 1);
        let left = &overrides.left_out[0];
        assert_eq!(
            (
                left.key["work_id"].as_str(),
                left.key["chapter_key"].as_str()
            ),
            (SURVIVOR, "1050")
        );
        assert_eq!(
            left.reason,
            "work_id is not in the kept file's work table, and the kept work it was merged from already has this row"
        );
        assert_eq!(left.kept_work_candidates, [WORK]);
        assert_eq!(report.rows_left_out, 1);
        let led_back = report
            .work_references_rewritten
            .iter()
            .map(|r| {
                (
                    r.table.as_str(),
                    r.postgres_work_id.as_str(),
                    r.kept_work_id.as_str(),
                    r.rows,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(led_back, [("chapter_override", SURVIVOR, WORK, 1)]);
    }
    crate::pg_import::hang_up(pg).await;
}

/// SQLite names the `-wal` and the old build names its directory lock after
/// the real file. A link must not hide either.
#[cfg(unix)]
#[tokio::test]
async fn a_symbolic_link_hides_nothing() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    cut_over(&scratch, &kept, &database.url).await;
    let elsewhere = scratch.file("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let real = std::fs::canonicalize(&scratch.0).unwrap();

    // A WAL-mode file that a connection has written: the -wal has content.
    let wal_kept = scratch.file("wal-kept.sqlite3");
    std::fs::copy(&kept, &wal_kept).unwrap();
    let mut writer = sqlite(&wal_kept, false).await;
    sqlx::raw_sql("PRAGMA journal_mode=WAL; UPDATE users SET bio='written into the wal'")
        .execute(&mut writer)
        .await
        .unwrap();
    let link = elsewhere.join("link-to-wal-kept.sqlite3");
    symlink(&wal_kept, &link).unwrap();
    let mut args = arguments(&scratch, &link, &database.url);
    let error = refusal(&args).await;
    assert!(error.contains("live") && error.contains("-wal"), "{error}");
    assert!(!args.output.exists() && !export::partial_path(&args.output).exists());
    writer.close().await.unwrap();

    // A server runs on the real file's directory.
    let link = elsewhere.join("link-to-kept.sqlite3");
    symlink(&kept, &link).unwrap();
    args.template = link.clone();
    let server = std::fs::File::create(scratch.file(".komiq-server.lock")).unwrap();
    server.try_lock().unwrap();
    let error = refusal(&args).await;
    assert!(error.contains("a server holds"), "{error}");
    drop(server);

    // An output that would get the name of the kept file's -wal, reached
    // through a linked directory; and a link that points nowhere.
    let linked_directory = elsewhere.join("data");
    symlink(&scratch.0, &linked_directory).unwrap();
    let mut overlap = arguments(&scratch, &kept, &database.url);
    overlap.output = linked_directory.join("kept.sqlite3-wal");
    let error = refusal(&overlap).await;
    assert!(error.contains("overlaps a side file"), "{error}");
    let dangling = scratch.file("dangling.sqlite3");
    symlink(scratch.file("nothing-here"), &dangling).unwrap();
    overlap.output = dangling.clone();
    let error = refusal(&overlap).await;
    assert!(error.contains("the output path exists"), "{error}");
    assert!(std::fs::symlink_metadata(&dangling)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!template::sibling(&kept, "-wal").exists());

    // A link to a clean file is a good template. The report names the real file.
    let kept_sha256 = template::sha256(&kept).unwrap();
    let report = export::run(&args, None).await.unwrap();
    assert!(report.complete);
    assert_eq!(report.template.path, real.join("kept.sqlite3"));
    assert_eq!(report.output.path, real.join("rolled-back.sqlite3"));
    assert_eq!(template::sha256(&kept).unwrap(), kept_sha256);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!template::sibling(&kept, suffix).exists());
        assert!(!template::sibling(&link, suffix).exists());
    }
}

/// The right schema is not enough: the rows must be the moved accounts.
#[tokio::test]
async fn refuses_a_postgres_that_is_not_the_cutover_database() {
    let scratch = Scratch::new();
    let kept = kept_file(&scratch, 121).await;
    let kept_sha256 = template::sha256(&kept).unwrap();

    // A new database of this release that no SQLite file was imported into.
    let fresh = crate::test_postgres::empty_database().unwrap();
    let mut pg = PgConnection::connect(&fresh.url).await.unwrap();
    crate::pg_import::importer::POSTGRES
        .run(&mut pg)
        .await
        .unwrap();
    pg.close().await.unwrap();
    let args = arguments(&scratch, &kept, &fresh.url);
    let error = refusal(&args).await;
    assert!(error.contains("no completed import from SQLite"), "{error}");
    assert_nothing_written(&args, &kept_sha256);

    // The imported database, with every account gone.
    let emptied = crate::test_postgres::empty_database().unwrap();
    cut_over(&scratch, &kept, &emptied.url).await;
    let mut pg = PgConnection::connect(&emptied.url).await.unwrap();
    sqlx::query("TRUNCATE users CASCADE")
        .execute(&mut pg)
        .await
        .unwrap();
    pg.close().await.unwrap();
    let args = arguments(&scratch, &kept, &emptied.url);
    let error = refusal(&args).await;
    assert!(
        error.contains("PostgreSQL has no account and the template has 3"),
        "{error}"
    );
    assert_nothing_written(&args, &kept_sha256);
}

async fn refusal(args: &Arguments) -> String {
    format!(
        "{:#}",
        export::run(args, None).await.err().expect("refused")
    )
}

#[tokio::test]
async fn refuses_postgres_only_user_rows_without_writing_or_deleting_anything() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    let kept_sha256 = template::sha256(&kept).unwrap();
    cut_over(&scratch, &kept, &database.url).await;
    let mut pg = PgConnection::connect(&database.url).await.unwrap();
    sqlx::query(
        "INSERT INTO scanlator_rating (scanlator_id,user_id,score,created_at,updated_at) \
         VALUES ('group:example',$1::uuid,8,now(),now())",
    )
    .bind(READER)
    .execute(&mut pg)
    .await
    .unwrap();
    let args = arguments(&scratch, &kept, &database.url);
    let error = refusal(&args).await;
    assert!(
        error.contains("scanlator_rating: 1 rows would be lost")
            && error.contains("nothing was exported")
            && error.contains("keep PostgreSQL and its backups"),
        "{error}"
    );
    assert_nothing_written(&args, &kept_sha256);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM scanlator_rating WHERE score=8")
        .fetch_one(&mut pg)
        .await
        .unwrap();
    assert_eq!(rows, 1, "refusal must preserve the unsupported user data");
    pg.close().await.unwrap();
}

#[tokio::test]
async fn empty_postgres_only_tables_are_reported_and_keep_both_sqlite_levels_supported() {
    for level in [121, 122] {
        let scratch = Scratch::new();
        let database = crate::test_postgres::empty_database().unwrap();
        let kept = kept_file(&scratch, level).await;
        cut_over(&scratch, &kept, &database.url).await;
        let args = arguments(&scratch, &kept, &database.url);
        let report = export::run(&args, None).await.unwrap();
        assert!(report.complete);
        assert_eq!(report.target_sqlite_migration, level);
        assert_eq!(
            report.postgres_only_not_copied.len(),
            plan::POSTGRES_ONLY.len()
        );
        for (name, _) in REBUILDABLE_POSTGRES_ONLY {
            assert_eq!(report.postgres_only_not_copied[*name], 0);
        }
        assert_eq!(report.postgres_only_not_copied["scanlator_rating"], 0);
        let report = serde_json::to_value(report).unwrap();
        assert_eq!(report["postgres_only_not_copied"]["scanlator_rating"], 0);
        let mut output = sqlite(&args.output, false).await;
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&mut output)
            .await
            .unwrap();
        assert_eq!(rows, 3);
        let has_ratings: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_schema WHERE name='scanlator_rating'")
                .fetch_one(&mut output)
                .await
                .unwrap();
        assert_eq!(has_ratings, 0);
        output.close().await.unwrap();
    }
}

#[tokio::test]
async fn nonempty_rebuildable_tables_are_omitted_and_reported() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    cut_over(&scratch, &kept, &database.url).await;
    let mut pg = PgConnection::connect(&database.url).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO engine_generation(id,first_seen,last_seen,state) VALUES ('00000000-0000-4000-8000-000000000001', now(), now(), 'active');
         INSERT INTO engine_manga_key VALUES ('00000000-0000-4000-8000-000000000001', 7, 'source', '/manga', NULL, 'Engine title', now());
         INSERT INTO engine_chapter_key VALUES
           ('00000000-0000-4000-8000-000000000001', 8, 7, 0, '/chapter/8', NULL, now()),
           ('00000000-0000-4000-8000-000000000001', 9, 7, 1, '/chapter/9', 'https://example.org/chapter/9', now());",
    ).execute(&mut pg).await.unwrap();
    let args = arguments(&scratch, &kept, &database.url);
    let report = export::run(&args, None).await.unwrap();
    assert!(report.complete);
    for (name, count) in [
        ("engine_generation", 1),
        ("engine_manga_key", 1),
        ("engine_chapter_key", 2),
    ] {
        assert_eq!(report.postgres_only_not_copied[name], count);
        let preserved: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {name}"))
            .fetch_one(&mut pg)
            .await
            .unwrap();
        assert_eq!(preserved, count);
    }
    let mut output = sqlite(&args.output, false).await;
    let tables: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_schema WHERE name LIKE 'engine_%'")
            .fetch_one(&mut output)
            .await
            .unwrap();
    assert_eq!(tables, 0);
    output.close().await.unwrap();
    pg.close().await.unwrap();
}

#[tokio::test]
async fn refuses_unclassified_postgres_tables_even_when_empty_or_outside_public() {
    for (schema, with_rows) in [("public", false), ("public", true), ("other", true)] {
        let scratch = Scratch::new();
        let database = crate::test_postgres::empty_database().unwrap();
        let kept = kept_file(&scratch, 121).await;
        let kept_sha256 = template::sha256(&kept).unwrap();
        cut_over(&scratch, &kept, &database.url).await;
        let mut pg = PgConnection::connect(&database.url).await.unwrap();
        if schema != "public" {
            sqlx::query("CREATE SCHEMA other")
                .execute(&mut pg)
                .await
                .unwrap();
        }
        sqlx::query(&format!(
            "CREATE TABLE {schema}.future_user_data (value bigint)"
        ))
        .execute(&mut pg)
        .await
        .unwrap();
        if with_rows {
            sqlx::query(&format!(
                "INSERT INTO {schema}.future_user_data VALUES (42)"
            ))
            .execute(&mut pg)
            .await
            .unwrap();
        }
        let args = arguments(&scratch, &kept, &database.url);
        let error = refusal(&args).await;
        assert!(
            error.contains("unclassified relations")
                && error.contains("future_user_data")
                && error.contains(schema)
                && error.contains("nothing was exported"),
            "{error}"
        );
        assert_nothing_written(&args, &kept_sha256);
        let rows: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {schema}.future_user_data"))
                .fetch_one(&mut pg)
                .await
                .unwrap();
        assert_eq!(rows, i64::from(with_rows));
        pg.close().await.unwrap();
    }
}

#[tokio::test]
async fn refuses_existing_output_and_leftover_partial() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    let kept_sha256 = template::sha256(&kept).unwrap();
    cut_over(&scratch, &kept, &database.url).await;
    let args = arguments(&scratch, &kept, &database.url);

    std::fs::write(&args.output, b"someone else's file").unwrap();
    assert!(refusal(&args).await.contains("the output path exists"));
    assert_eq!(std::fs::read(&args.output).unwrap(), b"someone else's file");
    std::fs::remove_file(&args.output).unwrap();

    let partial = export::partial_path(&args.output);
    std::fs::write(&partial, b"left by a killed run").unwrap();
    assert!(refusal(&args).await.contains("an earlier run was killed"));
    assert_eq!(std::fs::read(&partial).unwrap(), b"left by a killed run");
    std::fs::remove_file(&partial).unwrap();

    // Relative paths, and the template as its own output.
    let mut relative = arguments(&scratch, &kept, &database.url);
    relative.output = PathBuf::from("rolled-back.sqlite3");
    assert!(refusal(&relative).await.contains("absolute"));
    let mut same = arguments(&scratch, &kept, &database.url);
    same.output = kept.clone();
    assert!(refusal(&same).await.contains("the output path exists"));

    assert_nothing_written(&args, &kept_sha256);
    // Nothing above spoiled the inputs: the export still runs.
    assert!(export::run(&args, None).await.unwrap().complete);
}

#[tokio::test]
async fn refuses_a_busy_template() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    cut_over(&scratch, &kept, &database.url).await;
    let args = arguments(&scratch, &kept, &database.url);
    let kept_sha256 = template::sha256(&kept).unwrap();

    // Another connection holds the file's exclusive lock.
    let mut other = sqlite(&kept, false).await;
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut other)
        .await
        .unwrap();
    let error = refusal(&args).await;
    assert!(
        error.contains("another process has locked the template"),
        "{error}"
    );
    assert!(error.contains("stop every server"), "{error}");
    sqlx::query("ROLLBACK").execute(&mut other).await.unwrap();
    assert_nothing_written(&args, &kept_sha256);

    // A writer in the middle of a transaction: its journal has content.
    sqlx::raw_sql(
        "PRAGMA cache_size=1; BEGIN IMMEDIATE; UPDATE users SET bio=hex(randomblob(40000))",
    )
    .execute(&mut other)
    .await
    .unwrap();
    let journal = template::sibling(&kept, "-journal");
    assert!(std::fs::metadata(&journal).unwrap().len() > 0);
    let spilled = template::sha256(&kept).unwrap();
    let error = refusal(&args).await;
    assert!(
        error.contains("live") && error.contains("-journal"),
        "{error}"
    );
    assert_nothing_written(&args, &spilled);
    sqlx::query("ROLLBACK").execute(&mut other).await.unwrap();
    other.close().await.unwrap();
    // The writer's own rollback is free to lay the file out anew.
    let kept_sha256 = template::sha256(&kept).unwrap();

    // A server runs on the template's directory: it holds the directory lock.
    let lock_path = scratch.file(".komiq-server.lock");
    let server = std::fs::File::create(&lock_path).unwrap();
    server.try_lock().unwrap();
    let error = refusal(&args).await;
    assert!(error.contains("a server holds"), "{error}");
    drop(server);
    assert_nothing_written(&args, &kept_sha256);

    // A WAL-mode file with a connection that has written: the -wal has content.
    let wal_kept = scratch.file("wal-kept.sqlite3");
    std::fs::copy(&kept, &wal_kept).unwrap();
    let mut writer = sqlite(&wal_kept, false).await;
    sqlx::raw_sql("PRAGMA journal_mode=WAL; UPDATE users SET bio='written into the wal'")
        .execute(&mut writer)
        .await
        .unwrap();
    assert!(
        std::fs::metadata(template::sibling(&wal_kept, "-wal"))
            .unwrap()
            .len()
            > 0
    );
    let mut wal_args = arguments(&scratch, &wal_kept, &database.url);
    wal_args.output = scratch.file("from-wal.sqlite3");
    let error = refusal(&wal_args).await;
    assert!(error.contains("live") && error.contains("-wal"), "{error}");
    assert!(!wal_args.output.exists() && !export::partial_path(&wal_args.output).exists());
    // Closed cleanly, the same file is a good template, and it stays in WAL mode.
    writer.close().await.unwrap();
    let wal_sha256 = template::sha256(&wal_kept).unwrap();
    let report = export::run(&wal_args, None).await.unwrap();
    assert!(report.complete);
    assert_eq!(template::sha256(&wal_kept).unwrap(), wal_sha256);
    // Nothing was created beside the kept file either.
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!template::sibling(&wal_kept, suffix).exists());
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!template::sibling(&wal_args.output, suffix).exists());
    }
    assert_eq!(&std::fs::read(&wal_args.output).unwrap()[18..20], [2, 2]);
    let mut out = sqlite(&wal_args.output, false).await;
    assert_eq!(
        text(
            &mut out,
            &format!("SELECT bio FROM users WHERE id='{OWNER}'")
        )
        .await,
        // PostgreSQL's value, not the template's later one.
        ["bio — text"]
    );
    out.close().await.unwrap();
}

#[tokio::test]
async fn refuses_a_template_at_another_schema_version() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    cut_over(&scratch, &kept, &database.url).await;

    // One migration behind production.
    let older = scratch.file("older.sqlite3");
    let mut db = sqlite(&older, true).await;
    migrator(120).run(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut args = arguments(&scratch, &older, &database.url);
    let sha256 = template::sha256(&older).unwrap();
    let error = refusal(&args).await;
    assert!(
        error.contains("the template is at SQLite migration 120"),
        "{error}"
    );
    assert_nothing_written(&args, &sha256);

    // The right level with a changed migration checksum.
    let edited = scratch.file("edited.sqlite3");
    std::fs::copy(&kept, &edited).unwrap();
    let mut db = sqlite(&edited, false).await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum=x'00' WHERE version=121")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    args.template = edited.clone();
    let error = refusal(&args).await;
    assert!(error.contains("migration history differs"), "{error}");

    // A SQLite file that is not this server's, and a file that is not SQLite.
    let foreign = scratch.file("foreign.sqlite3");
    let mut db = sqlite(&foreign, true).await;
    sqlx::query("CREATE TABLE t (a)")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    args.template = foreign;
    assert!(refusal(&args).await.contains("no SQLx migration history"));
    let garbage = scratch.file("garbage.sqlite3");
    std::fs::write(&garbage, vec![7u8; 8192]).unwrap();
    args.template = garbage;
    refusal(&args).await;
    assert!(!args.output.exists() && !export::partial_path(&args.output).exists());
}

#[tokio::test]
async fn refuses_postgres_at_another_migration() {
    let scratch = Scratch::new();
    let kept = kept_file(&scratch, 121).await;
    let kept_sha256 = template::sha256(&kept).unwrap();

    // An empty database.
    let empty = crate::test_postgres::empty_database().unwrap();
    let args = arguments(&scratch, &kept, &empty.url);
    let error = refusal(&args).await;
    assert!(
        error.contains("PostgreSQL has 0 migrations applied"),
        "{error}"
    );
    assert_nothing_written(&args, &kept_sha256);

    // The database of the release before this one: every migration but the last.
    let older = crate::test_postgres::empty_database().unwrap();
    let mut pg = PgConnection::connect(&older.url).await.unwrap();
    let embedded = &crate::pg_import::importer::POSTGRES;
    let latest = embedded.iter().map(|m| m.version).max().unwrap();
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            embedded
                .iter()
                .filter(|m| m.version < latest)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&mut pg)
    .await
    .unwrap();
    pg.close().await.unwrap();
    let args = arguments(&scratch, &kept, &older.url);
    let error = refusal(&args).await;
    assert!(
        error.contains(&format!(
            "PostgreSQL has {} migrations applied and this server has {}",
            embedded.iter().count() - 1,
            embedded.iter().count()
        )),
        "{error}"
    );
    assert_nothing_written(&args, &kept_sha256);

    // No database at all: the error names no part of the connection URL.
    let mut nowhere = arguments(&scratch, &kept, &empty.url);
    nowhere.postgres_url = "postgres://user:secret-password@127.0.0.1:1/none".into();
    let error = super::cli::safe_error(
        &export::run(&nowhere, None).await.err().unwrap(),
        &nowhere.postgres_url,
    );
    assert!(
        !error.contains("secret-password") && !error.contains("127.0.0.1"),
        "{error}"
    );
}

#[tokio::test]
async fn a_difference_between_postgres_and_the_output_fails_the_run() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 122).await;
    cut_over(&scratch, &kept, &database.url).await;
    let mut pg = crate::pg_import::connect(&database.url).await.unwrap();
    let work = scratch.file("work.sqlite3");
    std::fs::copy(&kept, &work).unwrap();
    let mut out = sqlite(&work, false).await;
    // As in the export: no cascade while a table is replaced.
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut out)
        .await
        .unwrap();
    for (table, damage, column) in [
        // One microsecond, one flag, one deleted row, one extra row.
        ("reviews", "UPDATE reviews SET updated_at='2020-01-06T01:02:03.123457+00:00' WHERE id='00000000-0000-4000-8000-000000001000'", "updated_at"),
        ("user_library", "UPDATE user_library SET is_favorite=1-is_favorite WHERE rowid=(SELECT min(rowid) FROM user_library)", "is_favorite"),
        ("comments", "DELETE FROM comments WHERE id='00000000-0000-4000-8000-000000001110'", "extra in PostgreSQL"),
        ("instance_setting", "INSERT INTO instance_setting VALUES ('extra','x','2020-01-01T00:00:00+00:00')", "missing in PostgreSQL"),
    ] {
        let table = copied(122).into_iter().find(|t| t.name == table).unwrap();
        sqlx::query("BEGIN").execute(&mut out).await.unwrap();
        let mut report = export::load(&table, &mut pg, &mut out, &mut Vec::new())
            .await
            .unwrap();
        export::compare(&table, &mut report, &mut pg, &mut out)
            .await
            .unwrap();
        sqlx::query(damage).execute(&mut out).await.unwrap();
        report.sqlite = Default::default();
        let error = export::compare(&table, &mut report, &mut pg, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("{} differs between PostgreSQL", table.name))
                && error.contains(column),
            "{error}"
        );
        sqlx::query("ROLLBACK").execute(&mut out).await.unwrap();
    }
    out.close().await.unwrap();
    crate::pg_import::hang_up(pg).await;
}

#[tokio::test]
async fn a_kill_in_the_middle_leaves_no_output_file() {
    if std::env::var_os("KOMIKA_PG_EXPORT_TEST_CHILD").is_some() {
        let args = Arguments {
            template: PathBuf::from(std::env::var_os("KOMIKA_PG_EXPORT_TEST_TEMPLATE").unwrap()),
            output: PathBuf::from(std::env::var_os("KOMIKA_PG_EXPORT_TEST_OUTPUT").unwrap()),
            postgres_url: std::env::var("KOMIKA_PG_EXPORT_TEST_URL").expect("child URL [redacted]"),
        };
        let _ = export::run(&args, Some(9)).await;
        panic!("child did not terminate at hook");
    }
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    let kept_sha256 = template::sha256(&kept).unwrap();
    cut_over(&scratch, &kept, &database.url).await;
    user_writes(&database.url).await;
    let args = arguments(&scratch, &kept, &database.url);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "pg_export::tests::a_kill_in_the_middle_leaves_no_output_file",
            "--nocapture",
        ])
        .env("KOMIKA_PG_EXPORT_TEST_CHILD", "1")
        .env("KOMIKA_PG_EXPORT_TEST_TEMPLATE", &args.template)
        .env("KOMIKA_PG_EXPORT_TEST_OUTPUT", &args.output)
        .env("KOMIKA_PG_EXPORT_TEST_URL", &args.postgres_url)
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(86),
        "the child dies after nine tables, without unwinding"
    );
    // No file has the output's name. What the dead run wrote has the partial
    // name, and the kept file is as it was.
    assert!(!args.output.exists());
    let partial = export::partial_path(&args.output);
    assert!(partial.exists());
    assert_eq!(template::sha256(&kept).unwrap(), kept_sha256);
    // The next run does not build on the leftover and does not remove it.
    assert!(refusal(&args).await.contains("an earlier run was killed"));
    assert!(partial.exists() && !args.output.exists());
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(template::sibling(&partial, suffix));
    }
    let report = export::run(&args, None).await.unwrap();
    assert!(report.complete && args.output.exists() && !partial.exists());
    let tables = report
        .tables
        .iter()
        .map(|t| t.table.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(tables.len(), 24);
}

#[tokio::test]
async fn command_line_writes_the_report_and_fails_without_an_output() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 121).await;
    cut_over(&scratch, &kept, &database.url).await;
    let args = arguments(&scratch, &kept, &database.url);
    let command = |report: &Path| {
        [
            "export-postgres-to-sqlite",
            "--template",
            &kept.to_string_lossy(),
            "--output",
            &args.output.to_string_lossy(),
            "--postgres",
            &database.url,
            "--json-out",
            &report.to_string_lossy(),
        ]
        .map(str::to_owned)
    };
    // A report that cannot be written stops the run before the export: no
    // output file with an error status.
    let error = super::run_cli(&command(&scratch.file("no-such-directory/report.json")))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("the directory of"), "{error}");
    let error = super::run_cli(&command(&template::sibling(&kept, "-wal")))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("JSON output overlaps a database file"),
        "{error}"
    );
    assert!(!args.output.exists() && !export::partial_path(&args.output).exists());
    assert!(!template::sibling(&kept, "-wal").exists());

    let report = scratch.file("report.json");
    super::run_cli(&command(&report)).await.unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(value["complete"], true);
    assert_eq!(value["target_sqlite_migration"], 121);
    assert_eq!(value["tables"].as_array().unwrap().len(), 24);
    assert!(value["snapshot_time"].as_str().unwrap().ends_with('Z'));
    assert!(value["seconds"].as_f64().unwrap() > 0.0);
    assert!(!std::fs::read_to_string(&report)
        .unwrap()
        .contains(&database.url));

    // The second run finds the output of the first: an error, and a report that says so.
    let failed = scratch.file("failed.json");
    let error = super::run_cli(&command(&failed))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("the output path exists"), "{error}");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&failed).unwrap()).unwrap();
    assert_eq!(value["complete"], false);
    assert!(value["error"]
        .as_str()
        .unwrap()
        .contains("the output path exists"));
    // A report is never written over a file, a database least of all.
    let error = super::run_cli(&command(&kept))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("JSON output must be a new file"), "{error}");
    assert!(super::run_cli(&[
        "export-postgres-to-sqlite".into(),
        "--bogus".into(),
        "x".into()
    ])
    .await
    .is_err());
}

#[tokio::test]
async fn engine_generation_detached_progress_and_epoch_refuse_export() {
    for table in ["suwayomi_progress_detached", "engine_numeric_state"] {
        let scratch = Scratch::new();
        let database = crate::test_postgres::empty_database().unwrap();
        let kept = kept_file(&scratch, 121).await;
        let kept_sha256 = template::sha256(&kept).unwrap();
        cut_over(&scratch, &kept, &database.url).await;
        let mut pg = PgConnection::connect(&database.url).await.unwrap();
        if table == "engine_numeric_state" {
            sqlx::query("INSERT INTO engine_numeric_state VALUES(true,1,now())")
                .execute(&mut pg)
                .await
                .unwrap();
        } else {
            sqlx::query("INSERT INTO engine_generation(id,first_seen,last_seen,state) VALUES('00000000-0000-4000-8000-000000000001',now(),now(),'detached')").execute(&mut pg).await.unwrap();
            sqlx::query("INSERT INTO suwayomi_progress_detached VALUES('00000000-0000-4000-8000-000000000001',$1,'8','7',2,true,now())").bind(uuid::Uuid::parse_str(READER).unwrap()).execute(&mut pg).await.unwrap();
        }
        let args = arguments(&scratch, &kept, &database.url);
        let error = refusal(&args).await;
        assert!(
            error.contains(&format!("{table}: 1 rows would be lost")),
            "{error}"
        );
        assert_nothing_written(&args, &kept_sha256);
    }
}

#[tokio::test]
async fn engine_generation_detached_comment_targets_export_as_text_with_threads_intact() {
    let scratch = Scratch::new();
    let database = crate::test_postgres::empty_database().unwrap();
    let kept = kept_file(&scratch, 122).await;
    cut_over(&scratch, &kept, &database.url).await;
    let mut pg = PgConnection::connect(&database.url).await.unwrap();
    sqlx::query("INSERT INTO comments(id,target_type,target_id,user_id,body,created_at) VALUES('00000000-0000-4000-8000-000000001191','detached_chapter','old-generation:8',$1,'kept detached comment',now())").bind(uuid::Uuid::parse_str(READER).unwrap()).execute(&mut pg).await.unwrap();
    sqlx::query("INSERT INTO comments(id,target_type,target_id,user_id,body,created_at,parent_id) VALUES('00000000-0000-4000-8000-000000001192','detached_chapter','old-generation:8',$1,'kept reply',now(),'00000000-0000-4000-8000-000000001191')").bind(uuid::Uuid::parse_str(READER).unwrap()).execute(&mut pg).await.unwrap();
    let args = arguments(&scratch, &kept, &database.url);
    assert!(export::run(&args, None).await.unwrap().complete);
    let mut output = sqlite(&args.output, false).await;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM comments WHERE target_type='detached_chapter' AND target_id='old-generation:8'").fetch_one(&mut output).await.unwrap();
    assert_eq!(count, 2);
    let parent: Option<String> = sqlx::query_scalar(
        "SELECT parent_id FROM comments WHERE id='00000000-0000-4000-8000-000000001192'",
    )
    .fetch_one(&mut output)
    .await
    .unwrap();
    assert_eq!(
        parent.as_deref(),
        Some("00000000-0000-4000-8000-000000001191")
    );
    output.close().await.unwrap();
}
