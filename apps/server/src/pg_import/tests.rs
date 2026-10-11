use super::*;
use conversion::*;
use plan::{quote, tables, Column};
use sqlx::{ConnectOptions, Connection, Row, SqliteConnection};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

fn column(target: &str, source: &str) -> Column {
    Column {
        name: "value".into(),
        target: target.into(),
        source: source.into(),
        required: false,
        generated: false,
        identity: false,
        repair: false,
    }
}

#[test]
fn date_census_layouts_and_truncation() {
    let expected = date("2020-01-02T01:02:03Z").unwrap().0;
    for s in [
        "2020-01-02T01:02:03+00:00",
        "2020-01-02T03:02:03+02:00",
        "2020-01-02T01:02:03",
        "2020-01-02 01:02:03",
    ] {
        assert_eq!(date(s).unwrap(), (expected, false), "{s}");
    }
    for digits in 0..=9 {
        let suffix = if digits == 0 {
            String::new()
        } else {
            format!(".{}", &"123456789"[..digits])
        };
        let (us, cut) = date(&format!("2020-01-02T01:02:03{suffix}Z")).unwrap();
        let fraction = if digits == 0 {
            0
        } else {
            format!("{:<06}", &"123456789"[..digits.min(6)])
                .replace(' ', "0")
                .parse::<i64>()
                .unwrap()
        };
        assert_eq!(us, expected + fraction);
        assert_eq!(cut, digits > 6);
    }
    assert_eq!(
        date("2020-01-02T01:02:03.999999999Z").unwrap().0,
        expected + 999999
    );
    assert!(date("2020-01-02T01:02:03.123456000Z").unwrap().1);
    assert!(date("+20250-01-03T19:44:17.225+00:00").is_ok());
    let sentinel = date("2037-12-31T15:00:00+00:00").unwrap().0;
    assert_eq!(sentinel, milliseconds(2_145_884_400_000).unwrap());
}

#[test]
fn dates_have_hand_computed_utc_microseconds() {
    // 2020-01-01: (50*365 + 12 leap days)*86400 = 1_577_836_800 s.
    // UTC = local seconds - signed offset; multiply by 1_000_000 for micros.
    let jan1 = 1_577_836_800i64 * 1_000_000;
    for (value, expected, cut) in [
        (
            "2020-01-01T00:00:00-05:30",
            jan1 + (5 * 3600 + 30 * 60) * 1_000_000,
            false,
        ),
        (
            "2020-01-01T00:00:00+05:45",
            jan1 - (5 * 3600 + 45 * 60) * 1_000_000,
            false,
        ),
        (
            "2020-01-01T00:00:00-23:59",
            jan1 + (23 * 3600 + 59 * 60) * 1_000_000,
            false,
        ),
        (
            "2020-01-01T00:00:00+14:00",
            jan1 - 14 * 3600 * 1_000_000,
            false,
        ),
        // UTC crosses day, month and year into 2019-12-31 10:30.
        (
            "2020-01-01T00:30:00+14:00",
            jan1 - (13 * 3600 + 30 * 60) * 1_000_000,
            false,
        ),
        ("2020-01-01T00:00:00.999999999Z", jan1 + 999_999, true),
        // Jan has 31 days; Feb 29 is 28 days after Feb 1: 59*86400 s.
        (
            "2020-02-29T12:34:56Z",
            jan1 + (59 * 86400 + 12 * 3600 + 34 * 60 + 56) * 1_000_000,
            false,
        ),
        // 1970..20249: 18280*365 + (4570-183+46) leap days.
        // Jan 3 adds 2 days: 6_676_635*86400 + 19*3600 + 44*60 + 17 s,
        // plus 225_000 micros = 576_861_335_057_225_000.
        (
            "+20250-01-03T19:44:17.225+00:00",
            576_861_335_057_225_000,
            false,
        ),
    ] {
        assert_eq!(date(value).unwrap(), (expected, cut), "{value}");
    }
}

#[test]
fn milliseconds_have_hand_computed_utc_microseconds() {
    // 2026-01-01: (56*365 + 14)*86400 s, plus .225 s; ms*1000.
    assert_eq!(
        milliseconds(1_767_225_600_225).unwrap(),
        1_767_225_600_225_000
    );
    // 9999-12-31: 8029*365 + (2007-80+20) leap days + 364 = 2_932_896 days.
    // (days*86400 + 86399)*1_000_000 + 999_000 micros.
    assert_eq!(
        milliseconds(253_402_300_799_999).unwrap(),
        253_402_300_799_999_000
    );
    // -1001 ms = (-2 s + 999 ms), Euclidean remainder, hence -1_001_000 us.
    assert_eq!(milliseconds(-1001).unwrap(), -1_001_000);
}

#[test]
fn latest_chapter_at_requires_canonical_signed_decimal() {
    let c = column("bigint", "TEXT");
    for bad in ["007", "-0", "00", "-007", "+7", " 7", "7.0"] {
        assert!(convert(&c, "text", Some(bad.as_bytes())).is_err(), "{bad}");
    }
    for (text, expected) in [
        ("0", 0),
        ("7", 7),
        ("-7", -7),
        ("-9223372036854775808", i64::MIN),
    ] {
        assert_eq!(
            convert(&c, "text", Some(text.as_bytes())).unwrap().0,
            Cell::Integer(expected)
        );
    }
}

#[test]
fn date_refuses_unrecorded_or_invalid_values_without_panics() {
    for s in [
        "",
        "Z",
        "+",
        "+2-01-01",
        "2020-01-01",
        "2020-02-30T00:00:00Z",
        "2020-01-01t00:00:00z",
        "2020-01-01T24:00:00Z",
        "2020-01-01T00:00:60Z",
        "2020-01-01T00:00:00.Z",
        "2020-01-01T00:00:00.1234567890Z",
        "2020-01-01T00:00:00+24:00",
        "2020-01-01T00:00:00+01:60",
        "2020-01-01T00:00:00+0000",
        "2020-01-01 00:00:00Z",
        "0000-01-01T00:00:00Z",
        "2020-01-01T00:00:00Z secret",
        "💥",
    ] {
        assert!(date(s).is_err(), "{s}");
    }
}

#[test]
fn signed_epochs_and_range() {
    assert_eq!(milliseconds(-1).unwrap(), -1000);
    assert_eq!(
        milliseconds(-26_616_935_569_000).unwrap(),
        -26_616_935_569_000_000
    );
    assert_eq!(
        milliseconds(-37_028_731_301_000).unwrap(),
        -37_028_731_301_000_000
    );
    assert!(milliseconds(i64::MAX).is_err());
    assert!(milliseconds(i64::MIN).is_err());
    let mut c = column("timestamptz", "INTEGER");
    c.repair = true;
    for value in [0i64, 1, 1_767_225_600, 99_999_999_999] {
        assert!(convert(&c, "integer", Some(value.to_string().as_bytes())).is_err());
    }
    for value in [
        -1i64,
        -26_616_935_569_000,
        100_000_000_000,
        1_767_225_600_225,
    ] {
        assert!(convert(&c, "integer", Some(value.to_string().as_bytes())).is_ok());
    }
}

#[test]
fn uuid_and_flags() {
    assert_eq!(
        uuid("ABCDEFAB-1234-5678-ABCD-1234567890AB").unwrap(),
        "abcdefab-1234-5678-abcd-1234567890ab"
    );
    assert_eq!(
        uuid("ABCDEFAB12345678ABCD1234567890AB").unwrap(),
        "abcdefab-1234-5678-abcd-1234567890ab"
    );
    for s in [
        "",
        "ing_01234567890123456789012345678901",
        "{abcdefab-1234-5678-abcd-1234567890ab}",
        "abcdefab-1234-5678-abcd-1234567890ag",
    ] {
        assert!(uuid(s).is_err());
    }
    assert!(!boolean(0).unwrap());
    assert!(boolean(1).unwrap());
    for n in [-1, 2, i64::MAX] {
        assert!(boolean(n).is_err());
    }
}

#[test]
fn json_lossless_numbers_sorted_keys_duplicates_and_nul() {
    assert_eq!(
        json(r#"{"z":1.00,"a":{"b":2e3,"a":-0.0},"n":123456789012345678901234567890}"#).unwrap(),
        r#"{"a":{"a":0,"b":2e3},"n":12345678901234567890123456789e1,"z":1e0}"#
    );
    assert_eq!(json("[1,1.0,10e-1,0.100e1]").unwrap(), "[1e0,1e0,1e0,1e0]");
    for s in [
        r#"{"x":1,"x":2}"#,
        r#"{"a":1,"\u0061":2}"#,
        r#"[{"x":1,"x":2}]"#,
        r#""\u0000""#,
        r#"{"\u0000":1}"#,
        "1e131072",
        "1e-16384",
        "{} trailing",
        "",
    ] {
        assert!(json(s).is_err(), "{s}");
    }
    assert!(json(r#"{"escaped":"\\u0000","emoji":"\ud83d\ude00"}"#).is_ok());
    assert_ne!(
        json("9007199254740992").unwrap(),
        json("9007199254740993").unwrap()
    );
}

#[test]
fn integer_and_real_widths_and_storage_classes() {
    assert_eq!(integer(&i64::MIN.to_string(), "bigint").unwrap(), i64::MIN);
    assert!(integer("9223372036854775808", "bigint").is_err());
    assert!(integer("2147483648", "integer").is_err());
    assert_eq!(
        integer("-2147483648", "integer").unwrap(),
        i64::from(i32::MIN)
    );
    for s in ["1.0", "", " 1", "+1", "1e0"] {
        assert!(integer(s, "bigint").is_err());
    }
    for s in ["NaN", "inf", "-inf", "1e999"] {
        assert!(real(s).is_err());
    }
    assert_eq!(real("-0.0").unwrap().to_bits(), 0.0f64.to_bits());
    assert!(convert(&column("boolean", "INTEGER"), "real", Some(b"1.0")).is_err());
    assert_eq!(
        convert(&column("bigint", "TEXT"), "text", Some(b"1577926923123"))
            .unwrap()
            .0,
        Cell::Integer(1_577_926_923_123)
    );
}

#[test]
fn text_bytes_null_and_empty_remain_distinct() {
    let mut c = column("text", "TEXT");
    assert_eq!(
        convert(&c, "text", Some("café 漫画 😀\\\t\n\r".as_bytes()))
            .unwrap()
            .0,
        Cell::Text("café 漫画 😀\\\t\n\r".into())
    );
    assert_eq!(
        convert(&c, "text", Some(b"")).unwrap().0,
        Cell::Text(String::new())
    );
    assert_eq!(convert(&c, "null", None).unwrap().0, Cell::Null);
    assert!(convert(&c, "text", Some(&[255])).is_err());
    assert!(convert(&c, "text", Some(b"a\0b")).is_err());
    c.required = true;
    assert!(convert(&c, "null", None).is_err());
    assert_eq!(
        convert(&column("bytea", "BLOB"), "blob", Some(&[0, 255]))
            .unwrap()
            .0,
        Cell::Bytes(vec![0, 255])
    );
}

#[test]
fn copy_encoder_adversarial_fields() {
    let cases = [
        (Cell::Null, b"\\N".to_vec()),
        (Cell::Text(String::new()), vec![]),
        (Cell::Text("\\N".into()), b"\\\\N".to_vec()),
        (Cell::Text("\\.".into()), b"\\\\.".to_vec()),
        (
            Cell::Text("a\\b\tc\nd\re😀".into()),
            "a\\\\b\\tc\\nd\\re😀".as_bytes().to_vec(),
        ),
        (
            Cell::Bytes(vec![0, 9, 10, 13, 92, 255]),
            b"\\\\x00090a0d5cff".to_vec(),
        ),
    ];
    for (cell, expected) in cases {
        let mut out = Vec::new();
        copy::encode(&cell, &mut out).unwrap();
        assert_eq!(out, expected);
    }
    let t = date("+20250-01-03T19:44:17.225Z").unwrap().0;
    assert_eq!(time_text(t).unwrap(), "20250-01-03 19:44:17.225000+00:00");
    let mut out = Vec::new();
    copy::encode(&Cell::Time(t), &mut out).unwrap();
    assert!(!out.starts_with(b"+"));
}

#[test]
fn checksum_is_order_independent_and_counts_multiplicity() {
    assert_eq!(verifier::index_expression("id DESC NULLS LAST"), "id");
    assert_eq!(
        verifier::index_expression("translate(email, 'ASC', 'DESC') NULLS FIRST"),
        "translate(email, 'ASC', 'DESC')"
    );
    let mut a = verifier::Checksum::default();
    let mut b = verifier::Checksum::default();
    a.add("t", b"a");
    a.add("t", b"b");
    b.add("t", b"b");
    b.add("t", b"a");
    assert_eq!(a, b);
    let mut c = verifier::Checksum::default();
    c.add("t", b"a");
    c.add("t", b"a");
    assert_ne!(c.sum, [0; 32]);
    let mut d = verifier::Checksum::default();
    d.add("other", b"a");
    assert_ne!(d.sum, c.sum);
    let mut forms = BTreeSet::new();
    for v in [
        Cell::Null,
        Cell::Text(String::new()),
        Cell::Bytes(vec![]),
        Cell::Text("ab".into()),
    ] {
        let mut f = Vec::new();
        frame(&v, &mut f);
        assert!(forms.insert(f));
    }
}

pub(super) struct SourceFile(pub(super) PathBuf);
impl Drop for SourceFile {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

pub(super) async fn fixture() -> (SourceFile, SqliteConnection) {
    let path = std::env::temp_dir().join(format!(
        "komika-pg-import-{}.sqlite3",
        uuid::Uuid::new_v4().simple()
    ));
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true)
        .disable_statement_logging();
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    source::SQLITE.run(&mut db).await.unwrap();
    for (id, name) in [
        ("00000000-0000-4000-8000-000000000900", "owner"),
        ("00000000-0000-4000-8000-000000000901", "reader"),
        ("00000000-0000-4000-8000-000000000902", "cjk-reader"),
    ] {
        sqlx::query("INSERT INTO users (id,username,email,password_hash,created_at) VALUES ($1,$2,$3,'fixture-secret','2020-01-01T00:00:00Z')").bind(id).bind(name).bind(format!("{name}@example.invalid")).execute(&mut db).await.unwrap();
    }
    // Retain the exact pre-port SQLite source bytes, including nanoseconds.
    // The native PG/API golden seed deliberately applies the C10 precision cut.
    sqlx::raw_sql(include_str!("../../tests/postgres_golden/seed_sqlite.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    (SourceFile(path), db)
}

#[tokio::test]
async fn plan_matches_real_migrations_baseline_map_and_fk_order() {
    let mut db = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    source::SQLITE.run(&mut db).await.unwrap();
    let physical:Vec<String>=sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT IN ('work_fts','work_fts_data','work_fts_idx','work_fts_content','work_fts_docsize','work_fts_config') AND name <> '_sqlx_migrations' ORDER BY name").fetch_all(&mut db).await.unwrap();
    let plan = tables();
    let mut planned = plan.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    planned.sort();
    assert_eq!(physical, planned);
    let type_map: serde_json::Value = serde_json::from_str(include_str!("type_map.json")).unwrap();
    let baseline = include_str!("../../migrations_pg/0001_baseline.sql");
    let tombstones = include_str!("../../migrations_pg/0002_comment_tombstones.sql");
    let mut loaded = BTreeSet::new();
    let mut columns = 0;
    let mut identities = 0;
    for t in &plan {
        let actual = sqlx::query(&format!("PRAGMA table_xinfo({})", quote(&t.name)))
            .fetch_all(&mut db)
            .await
            .unwrap();
        assert_eq!(actual.len(), t.columns.len(), "{} columns", t.name);
        let body = baseline
            .split(&format!("CREATE TABLE {} (", quote(&t.name)))
            .nth(1)
            .unwrap()
            .split("\n);")
            .next()
            .unwrap();
        let mut pg_names = body
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('"'))
            .map(|line| line.split('"').nth(1).unwrap())
            .collect::<Vec<_>>();
        if t.name == "comments" {
            assert!(tombstones.contains("ADD COLUMN deleted_by TEXT NULL"));
            assert!(tombstones.contains("CHECK (deleted_by IN ('author', 'moderator'))"));
            pg_names.push("deleted_by");
        }
        assert_eq!(
            pg_names,
            t.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            "{} exact baseline columns",
            t.name
        );
        for (row, c) in actual.iter().zip(&t.columns) {
            assert_eq!(row.get::<String, _>("name"), c.name);
            assert_eq!(row.get::<String, _>("type"), c.source);
            assert_eq!(row.get::<i64, _>("hidden") != 0, c.generated);
            if t.name == "comments" && c.name == "deleted_by" {
                // The port's historical type map describes baseline columns only.
                assert_eq!(
                    (&*c.target, &*c.source, c.required),
                    ("text", "TEXT", false)
                );
                columns += 1;
                continue;
            }
            assert!(body.contains(&format!("{} {}", quote(&c.name), c.target)));
            if !c.generated {
                let mapped = type_map["columns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["table"] == t.name && v["column"] == c.name)
                    .unwrap();
                assert_eq!(mapped["postgres_type"], c.target);
                assert_eq!(mapped["sqlite_declared_type"], c.source);
            }
            if c.identity {
                identities += 1;
            }
            columns += 1;
        }
        let actual_fks = sqlx::query(&format!("PRAGMA foreign_key_list({})", quote(&t.name)))
            .fetch_all(&mut db)
            .await
            .unwrap();
        assert_eq!(actual_fks.len(), t.fks.len());
        for index in t.indexes.iter().filter(|i| !i.ascii) {
            let names = sqlx::query(&format!("PRAGMA index_info({})", quote(&index.name)))
                .fetch_all(&mut db)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect::<Vec<_>>();
            assert_eq!(names, index.columns, "{} index {}", t.name, index.name);
        }
        for fk in &t.fks {
            assert!(
                fk.parent == t.name || loaded.contains(&fk.parent),
                "{} before {}",
                t.name,
                fk.parent
            );
            assert!(actual_fks
                .iter()
                .any(|r| r.get::<String, _>("table") == fk.parent
                    && r.get::<String, _>("from") == fk.column
                    && r.get::<String, _>("to") == fk.parent_column));
        }
        loaded.insert(t.name.clone());
    }
    assert_eq!((plan.len(), columns, identities), (63, 443, 7));
}

#[tokio::test]
async fn comment_tombstone_sqlite_migration_preserves_rows_and_checks_values() {
    let mut db = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    // Exercise an upgrade of existing comments, not just an empty migrated DB.
    for migration in source::SQLITE.iter().filter(|m| m.version < 122) {
        sqlx::raw_sql(&migration.sql)
            .execute(&mut db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES('owner','owner','owner@example.test','hash','2020-01-01T00:00:00Z')")
        .execute(&mut db).await.unwrap();
    sqlx::query("INSERT INTO comments(id,target_type,target_id,user_id,body,created_at) VALUES('root','series','s1','owner','existing body','2020-01-01T00:00:00Z')")
        .execute(&mut db).await.unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0122_comment_tombstones.sql"))
        .execute(&mut db)
        .await
        .unwrap();
    let live: (String, Option<String>) =
        sqlx::query_as("SELECT body,deleted_by FROM comments WHERE id='root'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(live, ("existing body".into(), None));
    for value in [Some("author"), Some("moderator"), None] {
        sqlx::query("UPDATE comments SET deleted_by=$1 WHERE id='root'")
            .bind(value)
            .execute(&mut db)
            .await
            .unwrap();
    }
    assert!(
        sqlx::query("UPDATE comments SET deleted_by='unknown' WHERE id='root'")
            .execute(&mut db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn golden_source_prechecks_are_read_only_and_preserve_real_bits() {
    let (file, mut writer) = fixture().await;
    sqlx::query("UPDATE work_stats SET rating=$1")
        .bind(1.2345678901234567f64)
        .execute(&mut writer)
        .await
        .unwrap();
    writer.close().await.unwrap();
    let before = std::fs::read(&file.0).unwrap();
    let mut ro = source::open(&file.0).await.unwrap();
    assert!(sqlx::query("DELETE FROM work")
        .execute(&mut ro)
        .await
        .is_err());
    let pre = source::precheck(&mut ro).await.unwrap();
    assert!(pre.passed(), "{pre:#?}");
    assert!(!pre.truncated_fractions.is_empty());
    let stats = tables()
        .into_iter()
        .find(|t| t.name == "work_stats")
        .unwrap();
    let row = sqlx::query(&stats.sqlite_select())
        .fetch_one(&mut ro)
        .await
        .unwrap();
    let cells = source::row_cells(&stats, &row).unwrap().0;
    assert!(cells.contains(&Cell::Real(1.2345678901234567)));
    ro.close().await.unwrap();
    assert_eq!(before, std::fs::read(&file.0).unwrap());
}

#[tokio::test]
async fn prechecks_count_bad_values_and_redact_secrets() {
    let (file, mut writer) = fixture().await;
    sqlx::query("UPDATE work SET is_nsfw=7, created_at='password-token-private@example.invalid'")
        .execute(&mut writer)
        .await
        .unwrap();
    sqlx::query("UPDATE source_extension SET source_id=CAST(x'ff' AS TEXT) WHERE source_id=(SELECT min(source_id) FROM source_extension)")
        .execute(&mut writer)
        .await
        .unwrap();
    writer.close().await.unwrap();
    let mut ro = source::open(&file.0).await.unwrap();
    let pre = source::precheck(&mut ro).await.unwrap();
    assert!(!pre.passed());
    let json = serde_json::to_string(&pre).unwrap();
    assert!(!json.contains("password-token"));
    assert!(!json.contains("fixture-secret"));
    assert!(pre.rules.values().any(|r| r.failures == 15));
}

#[tokio::test]
async fn snapshot_is_consistent_while_a_live_writer_changes_wal() {
    let (file, mut writer) = fixture().await;
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&mut writer)
        .await
        .unwrap();
    let mut ro = source::open(&file.0).await.unwrap();
    let before = source::precheck(&mut ro).await.unwrap();
    sqlx::query("UPDATE work SET description='changed live'")
        .execute(&mut writer)
        .await
        .unwrap();
    let after = source::precheck(&mut ro).await.unwrap();
    assert_eq!(before.source_data_hash, after.source_data_hash);
    ro.close().await.unwrap();
    let mut new = source::open(&file.0).await.unwrap();
    let after = source::precheck(&mut new).await.unwrap();
    assert_ne!(before.source_data_hash, after.source_data_hash);
}

#[tokio::test]
async fn source_refuses_old_dirty_and_wrong_migration_history() {
    for sql in [
        "DELETE FROM _sqlx_migrations WHERE version=(SELECT max(version) FROM _sqlx_migrations)",
        "UPDATE _sqlx_migrations SET success=0 WHERE version=1",
        "UPDATE _sqlx_migrations SET checksum=x'00' WHERE version=1",
    ] {
        let (file, mut writer) = fixture().await;
        sqlx::query(sql).execute(&mut writer).await.unwrap();
        writer.close().await.unwrap();
        assert!(source::open(&file.0).await.is_err());
    }
}

#[tokio::test]
async fn source_inventory_allows_litestream_and_names_unknown_tables() {
    let (file, mut writer) = fixture().await;
    sqlx::raw_sql(
        "CREATE TABLE _litestream_lock (id INTEGER); CREATE TABLE _litestream_seq (seq INTEGER)",
    )
    .execute(&mut writer)
    .await
    .unwrap();
    source::open(&file.0).await.unwrap().close().await.unwrap();
    sqlx::query("CREATE TABLE unexpected_source_table (value TEXT)")
        .execute(&mut writer)
        .await
        .unwrap();
    writer.close().await.unwrap();
    let error = source::open(&file.0).await.err().unwrap().to_string();
    assert!(error.contains("unexpected_source_table"), "{error}");
}

#[tokio::test]
async fn source_inventory_uses_literal_sqlite_reserved_prefix() {
    for name in [
        "sqlitex_extra",
        "sqliteXextra",
        "SQLITEX_extra",
        "ordinary_extra",
    ] {
        let (file, mut writer) = fixture().await;
        source::open(&file.0).await.unwrap().close().await.unwrap();
        sqlx::raw_sql(&format!(
            "CREATE TABLE {} (value TEXT); INSERT INTO {} VALUES ('must retain')",
            quote(name),
            quote(name)
        ))
        .execute(&mut writer)
        .await
        .unwrap();
        writer.close().await.unwrap();
        let error = source::open(&file.0).await.err().unwrap().to_string();
        assert!(error.contains(name), "{error}");
    }
}

#[tokio::test]
async fn precheck_counts_noncanonical_latest_chapter_at() {
    for value in ["007", "-0"] {
        let (file, mut writer) = fixture().await;
        sqlx::query("UPDATE suwayomi_series SET latest_chapter_at=$1")
            .bind(value)
            .execute(&mut writer)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM suwayomi_series")
            .fetch_one(&mut writer)
            .await
            .unwrap();
        writer.close().await.unwrap();
        let mut source = source::open(&file.0).await.unwrap();
        let report = source::precheck(&mut source).await.unwrap();
        assert!(!report.passed());
        assert_eq!(
            report.rules["suwayomi_series.latest_chapter_at: non-canonical integer spelling"]
                .failures,
            count as u64
        );
    }
}

// All PG tests create/drop ONLY random komika_import_test_* databases. Cleanup
// runs even if an assertion panics. The private PostgreSQL 18 cluster is
// provisioned automatically; missing setup never silently skips a test.
mod postgres_gated {
    use super::*;
    use futures::FutureExt;
    use std::{future::Future, pin::Pin};
    type Case = fn(PathBuf, String) -> Pin<Box<dyn Future<Output = ()>>>;
    async fn with_database(case: Case) {
        let admin_url =
            crate::test_postgres::admin_url().expect("automatic disposable PG18 cluster");
        let mut admin = connect(&admin_url)
            .await
            .expect("test admin PostgreSQL connection (URL redacted)");
        let name = format!("komika_import_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'",quote(&name))).execute(&mut admin).await.expect("create owned PG18 test DB");
        let mut url = reqwest::Url::parse(&admin_url).expect("valid admin URL [redacted]");
        url.set_path(&format!("/{name}"));
        let (file, writer) = fixture().await;
        writer.close().await.unwrap();
        let result = std::panic::AssertUnwindSafe(case(file.0.clone(), url.to_string()))
            .catch_unwind()
            .await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", quote(&name)))
            .execute(&mut admin)
            .await
            .expect("drop owned test DB");
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
    fn boxed(f: impl Future<Output = ()> + 'static) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(f)
    }
    #[tokio::test]
    async fn comment_tombstone_import_round_trip_and_postgres_constraint() {
        with_database(|path, url| boxed(async move {
            mutate_sqlite(&path, "UPDATE comments SET body='',has_spoiler=0,deleted_by='author' WHERE id='00000000-0000-4000-8000-000000001100'; DELETE FROM comment_votes WHERE comment_id='00000000-0000-4000-8000-000000001100'; DELETE FROM notifications WHERE comment_id='00000000-0000-4000-8000-000000001100'").await;
            importer::run(&path, &url, None).await.unwrap();
            assert!(verifier::run(&path, &url).await.unwrap().complete);
            let mut pg = connect(&url).await.unwrap();
            let deleted: String = sqlx::query_scalar("SELECT deleted_by FROM comments WHERE id='00000000-0000-4000-8000-000000001100'")
                .fetch_one(&mut pg).await.unwrap();
            assert_eq!(deleted, "author");
            let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM comments WHERE parent_id='00000000-0000-4000-8000-000000001100' AND deleted_by IS NULL")
                .fetch_one(&mut pg).await.unwrap();
            assert!(replies > 0);
            let error = sqlx::query("UPDATE comments SET deleted_by='unknown' WHERE id='00000000-0000-4000-8000-000000001100'")
                .execute(&mut pg).await.unwrap_err();
            assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23514"));
        })).await;
    }
    async fn mutate_sqlite(path: &Path, sql: &str) {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(false)
            .disable_statement_logging();
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::raw_sql(sql).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
    }
    async fn user_table_count(url: &str) -> i64 {
        let mut pg = connect(url).await.unwrap();
        sqlx::query_scalar("SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind='r'").fetch_one(&mut pg).await.unwrap()
    }
    #[tokio::test]
    async fn golden_import_then_verify() {
        with_database(|path, url| {
            boxed(async move {
                let report = importer::run(&path, &url, None).await.unwrap();
                assert!(report.complete);
                assert_eq!(report.tables.len(), 63);
                assert!(verifier::run(&path, &url).await.unwrap().complete);
            })
        })
        .await;
    }

    async fn chapter_insert_trigger(pg: &mut sqlx::PgConnection) -> String {
        sqlx::query_scalar("SELECT tgname FROM pg_trigger WHERE tgrelid='public.chapter'::regclass AND tgisinternal AND (tgtype::integer & 4)<>0")
            .fetch_one(pg).await.unwrap()
    }

    #[tokio::test]
    async fn verifier_rejects_disabled_replica_and_always_internal_fk_triggers() {
        with_database(|path, url| boxed(async move {
            importer::run(&path, &url, None).await.unwrap();
            assert!(verifier::run(&path, &url).await.unwrap().complete);
            let mut pg = connect(&url).await.unwrap();
            let trigger = chapter_insert_trigger(&mut pg).await;
            let orphan = "INSERT INTO chapter(id,source_series_id,external_id,created_at) VALUES ('test-orphan','missing-source','test','2020-01-01T00:00:00Z')";
            let error = sqlx::query(orphan).execute(&mut pg).await.unwrap_err();
            assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23503"));
            for mode in ["DISABLE", "ENABLE REPLICA", "ENABLE ALWAYS"] {
                sqlx::query(&format!("ALTER TABLE chapter {mode} TRIGGER {}",quote(&trigger)))
                    .execute(&mut pg).await.unwrap();
                let error = verifier::run(&path, &url).await.err().unwrap().to_string();
                assert!(error.contains("foreign-key implementation triggers"), "{error}");
                if mode == "DISABLE" {
                    // Prove the mutation actually permits an orphan, without
                    // retaining wrong rows that could mask the trigger bug.
                    sqlx::query("BEGIN").execute(&mut pg).await.unwrap();
                    sqlx::query(orphan).execute(&mut pg).await.unwrap();
                    sqlx::query("ROLLBACK").execute(&mut pg).await.unwrap();
                }
                sqlx::query(&format!("ALTER TABLE chapter ENABLE TRIGGER {}",quote(&trigger)))
                    .execute(&mut pg).await.unwrap();
                assert!(verifier::run(&path, &url).await.unwrap().complete);
            }
        })).await;
    }

    #[tokio::test]
    async fn resume_rejects_internal_fk_trigger_mutation_before_loading_more_tables() {
        with_database(|path, url| {
            boxed(async move {
                assert!(importer::run(&path, &url, Some(32)).await.is_err());
                let mut pg = connect(&url).await.unwrap();
                let trigger = chapter_insert_trigger(&mut pg).await;
                sqlx::query(&format!(
                    "ALTER TABLE chapter DISABLE TRIGGER {}",
                    quote(&trigger)
                ))
                .execute(&mut pg)
                .await
                .unwrap();
                let error = importer::run(&path, &url, None)
                    .await
                    .err()
                    .unwrap()
                    .to_string();
                assert!(
                    error.contains("foreign-key implementation triggers"),
                    "{error}"
                );
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM komika_import_table")
                    .fetch_one(&mut pg)
                    .await
                    .unwrap();
                assert_eq!(count, 32);
                assert!(!import_complete(&mut pg).await.unwrap());
                sqlx::query(&format!(
                    "ALTER TABLE chapter ENABLE TRIGGER {}",
                    quote(&trigger)
                ))
                .execute(&mut pg)
                .await
                .unwrap();
                assert!(importer::run(&path, &url, None).await.unwrap().complete);
                assert!(verifier::run(&path, &url).await.unwrap().complete);
            })
        })
        .await;
    }

    #[tokio::test]
    async fn text_search_mapping_mutation_cannot_verify_wrong_documents() {
        with_database(|path, url| boxed(async move {
            mutate_sqlite(&path,"UPDATE work SET primary_title='Running Café リゼロ' WHERE id='w_00000000000040008000000000000001'").await;
            importer::run(&path, &url, None).await.unwrap();
            let mut pg = connect(&url).await.unwrap();
            let row: (String,String,String) = sqlx::query_as("SELECT normalized_title,trigram_key,doc::text FROM work_search_alias WHERE id='title:w_00000000000040008000000000000001'")
                .fetch_one(&mut pg).await.unwrap();
            // Literal expectations do not call importer/verifier helpers.
            assert_eq!(row, ("running cafe リゼロ".into(),"running cafe fca05ad7d889".into(),"'cafe':2 'running':1 'リゼロ':3".into()));
            assert!(verifier::run(&path, &url).await.unwrap().complete);
            sqlx::raw_sql("ALTER TEXT SEARCH CONFIGURATION pg_catalog.simple ALTER MAPPING FOR asciiword, word WITH pg_catalog.english_stem; UPDATE work_search_alias SET normalized_title=normalized_title")
                .execute(&mut pg).await.unwrap();
            let doc: String = sqlx::query_scalar("SELECT doc::text FROM work_search_alias WHERE id='title:w_00000000000040008000000000000001'")
                .fetch_one(&mut pg).await.unwrap();
            assert_eq!(doc, "'cafe':2 'run':1 'リゼロ':3");
            let error=verifier::run(&path, &url).await.err().unwrap().to_string();
            assert!(error.contains("text_search section"), "{error}");
            sqlx::raw_sql("ALTER TEXT SEARCH CONFIGURATION pg_catalog.simple ALTER MAPPING FOR asciiword, word WITH pg_catalog.simple; UPDATE work_search_alias SET normalized_title=normalized_title")
                .execute(&mut pg).await.unwrap();
            assert!(verifier::run(&path, &url).await.unwrap().complete);
        })).await;
    }

    #[tokio::test]
    async fn text_search_dictionary_options_are_fingerprinted() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(
                    path,
                    url,
                    "ALTER TEXT SEARCH DICTIONARY pg_catalog.simple (Accept=false)",
                    "text_search",
                )
                .await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn fresh_import_rejects_preexisting_search_mutations_without_ddl() {
        with_database(|path,url| boxed(async move {
            let mut pg=connect(&url).await.unwrap();
            for (mutation,restore) in [
                ("ALTER TEXT SEARCH CONFIGURATION pg_catalog.simple ALTER MAPPING FOR asciiword,word WITH pg_catalog.english_stem",
                 "ALTER TEXT SEARCH CONFIGURATION pg_catalog.simple ALTER MAPPING FOR asciiword,word WITH pg_catalog.simple"),
                ("ALTER TEXT SEARCH DICTIONARY pg_catalog.simple (Accept=false)",
                 "ALTER TEXT SEARCH DICTIONARY pg_catalog.simple (Accept)")
            ] {
                sqlx::query(mutation).execute(&mut pg).await.unwrap();
                let error=importer::run(&path,&url,None).await.err().unwrap().to_string();
                assert!(error.contains("simple text-search contract"), "{error}");
                assert_eq!(user_table_count(&url).await,0);
                sqlx::query(restore).execute(&mut pg).await.unwrap();
            }
            assert!(importer::run(&path,&url,None).await.unwrap().complete);
            assert!(verifier::run(&path,&url).await.unwrap().complete);
        })).await;
    }

    #[tokio::test]
    async fn resume_rejects_search_mapping_mutation_before_loading_more_tables() {
        with_database(|path,url| boxed(async move {
            assert!(importer::run(&path,&url,Some(32)).await.is_err());
            let mut pg=connect(&url).await.unwrap();
            sqlx::query("ALTER TEXT SEARCH CONFIGURATION pg_catalog.simple ALTER MAPPING FOR asciiword,word WITH pg_catalog.english_stem")
                .execute(&mut pg).await.unwrap();
            let error=importer::run(&path,&url,None).await.err().unwrap().to_string();
            assert!(error.contains("text_search section"),"{error}");
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM komika_import_table").fetch_one(&mut pg).await.unwrap();
            assert_eq!(count,32);
            assert!(!import_complete(&mut pg).await.unwrap());
            sqlx::query("ALTER TEXT SEARCH CONFIGURATION pg_catalog.simple ALTER MAPPING FOR asciiword,word WITH pg_catalog.simple")
                .execute(&mut pg).await.unwrap();
            assert!(importer::run(&path,&url,None).await.unwrap().complete);
            assert!(verifier::run(&path,&url).await.unwrap().complete);
        })).await;
    }

    #[tokio::test]
    async fn unknown_sqlitex_source_table_is_rejected_before_ddl_and_by_verifier() {
        with_database(|path,url| boxed(async move {
            mutate_sqlite(&path,"CREATE TABLE sqlitex_extra(value TEXT); INSERT INTO sqlitex_extra VALUES('must retain')").await;
            let error=importer::run(&path,&url,None).await.err().unwrap().to_string();
            assert!(error.contains("sqlitex_extra"),"{error}");
            assert_eq!(user_table_count(&url).await,0);
            mutate_sqlite(&path,"DROP TABLE sqlitex_extra").await;
            assert!(importer::run(&path,&url,None).await.unwrap().complete);
            assert!(verifier::run(&path,&url).await.unwrap().complete);
            mutate_sqlite(&path,"CREATE TABLE sqlitex_extra(value TEXT); INSERT INTO sqlitex_extra VALUES('must retain')").await;
            let error=verifier::run(&path,&url).await.err().unwrap().to_string();
            assert!(error.contains("sqlitex_extra"),"{error}");
        })).await;
    }
    async fn schema_mutation(path: PathBuf, url: String, sql: &str, section: &str) {
        importer::run(&path, &url, None).await.unwrap();
        let mut pg = connect(&url).await.unwrap();
        sqlx::raw_sql(sql).execute(&mut pg).await.unwrap();
        let error = verifier::run(&path, &url).await.err().unwrap().to_string();
        assert!(error.contains(&format!("{section} section")), "{error}");
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_dropped_trigger() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(path, url, "DROP TRIGGER work_pin_year ON work", "triggers").await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_changed_fk_action() {
        with_database(|path, url| boxed(async move {
            schema_mutation(path, url, "ALTER TABLE browse_catalogue DROP CONSTRAINT browse_catalogue_fk_0; ALTER TABLE browse_catalogue ADD CONSTRAINT browse_catalogue_fk_0 FOREIGN KEY (work_id) REFERENCES work(id) ON DELETE RESTRICT", "constraints").await;
        })).await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_dropped_check() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(
                    path,
                    url,
                    "ALTER TABLE source_rate_limit DROP CONSTRAINT source_rate_limit_ck_1",
                    "constraints",
                )
                .await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_added_default() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(
                    path,
                    url,
                    "ALTER TABLE work ALTER COLUMN description SET DEFAULT 'changed'",
                    "columns",
                )
                .await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_enabled_row_security() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(
                    path,
                    url,
                    "ALTER TABLE public.work ENABLE ROW LEVEL SECURITY",
                    "tables",
                )
                .await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_forced_row_security() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(
                    path,
                    url,
                    "ALTER TABLE public.work FORCE ROW LEVEL SECURITY",
                    "tables",
                )
                .await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_changed_column_collation() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(
                    path,
                    url,
                    "ALTER TABLE public.work ALTER COLUMN description TYPE text COLLATE \"C\" USING description::text",
                    "columns",
                )
                .await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_added_policy_with_row_security_disabled() {
        with_database(|path, url| {
            boxed(async move {
                schema_mutation(
                    path,
                    url,
                    "CREATE POLICY fingerprint_policy ON public.work USING (true)",
                    "policies",
                )
                .await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn fingerprint_detects_policy_definition_changes_with_flags_unchanged() {
        with_database(|path, url| {
            boxed(async move {
                importer::run(&path, &url, None).await.unwrap();
                let mut pg = connect(&url).await.unwrap();
                sqlx::query("CREATE POLICY fingerprint_policy ON public.work USING (true)")
                    .execute(&mut pg)
                    .await
                    .unwrap();
                let mut before = super::super::schema::fingerprint(&mut pg).await.unwrap();
                for sql in [
                    "ALTER POLICY fingerprint_policy ON public.work USING (false)",
                    "ALTER POLICY fingerprint_policy ON public.work WITH CHECK (true)",
                    "ALTER POLICY fingerprint_policy ON public.work TO CURRENT_USER",
                    "ALTER POLICY fingerprint_policy ON public.work RENAME TO renamed_policy",
                    "DROP POLICY renamed_policy ON public.work; CREATE POLICY renamed_policy ON public.work AS RESTRICTIVE TO CURRENT_USER USING (false) WITH CHECK (true)",
                    "DROP POLICY renamed_policy ON public.work; CREATE POLICY renamed_policy ON public.work AS RESTRICTIVE FOR UPDATE TO CURRENT_USER USING (false) WITH CHECK (true)",
                    "DROP POLICY renamed_policy ON public.work",
                ] {
                    sqlx::raw_sql(sql).execute(&mut pg).await.unwrap();
                    let after = super::super::schema::fingerprint(&mut pg).await.unwrap();
                    assert_eq!(before["tables"], after["tables"], "{sql}");
                    assert_ne!(before["policies"], after["policies"], "{sql}");
                    before = after;
                }
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_extra_table_with_data() {
        with_database(|path, url| boxed(async move {
            schema_mutation(path, url, "CREATE SCHEMA unrelated; CREATE TABLE unrelated.extra (value text); INSERT INTO unrelated.extra VALUES ('data')", "tables").await;
        })).await;
    }

    #[tokio::test]
    async fn verifier_fingerprint_detects_changed_search_function_after_rebuild() {
        with_database(|path, url| boxed(async move {
            schema_mutation(path, url, "CREATE OR REPLACE FUNCTION public.search_fold(value text) RETURNS text LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE SET search_path = pg_catalog, public AS $$ SELECT 'tampered'::text $$; SELECT public.rebuild_work_search_alias()", "functions").await;
        })).await;
    }

    #[tokio::test]
    async fn verifier_difference_examples_stop_at_five() {
        with_database(|path, url| {
            boxed(async move {
                importer::run(&path, &url, None).await.unwrap();
                let mut pg = connect(&url).await.unwrap();
                sqlx::query("UPDATE work SET description='changed diagnostic example'")
                    .execute(&mut pg)
                    .await
                    .unwrap();
                let error = verifier::run(&path, &url).await.err().unwrap().to_string();
                assert_eq!(error.matches(" columns description").count(), 5, "{error}");
                assert!(!error.contains("changed diagnostic example"));
            })
        })
        .await;
    }

    #[tokio::test]
    async fn resume_refuses_changed_baseline_fingerprint() {
        with_database(|path, url| {
            boxed(async move {
                assert!(importer::run(&path, &url, Some(2)).await.is_err());
                let mut pg = connect(&url).await.unwrap();
                let pinned: String =
                    sqlx::query_scalar("SELECT schema_hashes::text FROM komika_import_state")
                        .fetch_one(&mut pg)
                        .await
                        .unwrap();
                sqlx::query("ALTER TABLE work ALTER COLUMN description SET DEFAULT 'changed'")
                    .execute(&mut pg)
                    .await
                    .unwrap();
                let error = importer::run(&path, &url, None)
                    .await
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("columns section"), "{error}");
                let after: String =
                    sqlx::query_scalar("SELECT schema_hashes::text FROM komika_import_state")
                        .fetch_one(&mut pg)
                        .await
                        .unwrap();
                assert_eq!(pinned, after);
            })
        })
        .await;
    }

    #[tokio::test]
    async fn verifier_detects_single_changed_value() {
        with_database(|path,url|boxed(async move {
            importer::run(&path,&url,None).await.unwrap();let mut pg=connect(&url).await.unwrap();
            sqlx::query("UPDATE work SET description='one changed value' WHERE id=(SELECT min(id) FROM work)").execute(&mut pg).await.unwrap();
            let error=verifier::run(&path,&url).await.err().unwrap().to_string();assert!(error.contains("description"));assert!(!error.contains("one changed value"));
        })).await;
    }
    #[tokio::test]
    async fn verifier_detects_missing_row() {
        with_database(|path, url| {
            boxed(async move {
                importer::run(&path, &url, None).await.unwrap();
                let mut pg = connect(&url).await.unwrap();
                sqlx::query("DELETE FROM chapter WHERE id=(SELECT min(id) FROM chapter)")
                    .execute(&mut pg)
                    .await
                    .unwrap();
                assert!(verifier::run(&path, &url).await.is_err());
            })
        })
        .await;
    }
    #[tokio::test]
    async fn verifier_detects_extra_row() {
        with_database(|path, url| {
            boxed(async move {
                importer::run(&path, &url, None).await.unwrap();
                let mut pg = connect(&url).await.unwrap();
                sqlx::query(
                    "INSERT INTO instance_setting VALUES ('extra','value','2020-01-01T00:00:00Z')",
                )
                .execute(&mut pg)
                .await
                .unwrap();
                let error = verifier::run(&path, &url).await.err().unwrap().to_string();
                assert!(error.contains("extra in PostgreSQL"));
            })
        })
        .await;
    }
    #[tokio::test]
    async fn verifier_detects_timestamp_changed_by_one_microsecond() {
        with_database(|path,url|boxed(async move {
            importer::run(&path,&url,None).await.unwrap();let mut pg=connect(&url).await.unwrap();
            sqlx::query("UPDATE work SET updated_at=updated_at + interval '1 microsecond' WHERE id=(SELECT min(id) FROM work)").execute(&mut pg).await.unwrap();
            let error=verifier::run(&path,&url).await.err().unwrap().to_string();assert!(error.contains("updated_at"));
        })).await;
    }
    #[tokio::test]
    async fn interrupted_import_resumes_identically() {
        if std::env::var_os("KOMIKA_PG_TEST_CHILD").is_some() {
            let path =
                PathBuf::from(std::env::var_os("KOMIKA_PG_TEST_SOURCE").expect("child source"));
            let url = std::env::var("KOMIKA_PG_TEST_TARGET").expect("child target [redacted]");
            importer::run(&path, &url, Some(32))
                .await
                .expect("child must terminate at hook");
            panic!("child did not terminate at hook");
        }
        with_database(|path, url| {
            boxed(async move {
                let status=std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact","pg_import::tests::postgres_gated::interrupted_import_resumes_identically","--nocapture"])
                    .env("KOMIKA_PG_TEST_CHILD","1").env("KOMIKA_PG_TEST_SOURCE",&path).env("KOMIKA_PG_TEST_TARGET",&url)
                    .status().unwrap();
                assert_eq!(status.code(),Some(86),"child must exit abruptly between committed tables");
                let mut pg = connect(&url).await.unwrap();
                sqlx::raw_sql("SELECT pg_advisory_lock(192837465,180002); SELECT pg_advisory_unlock(192837465,180002)").execute(&mut pg).await.unwrap();
                assert!(!import_complete(&mut pg).await.unwrap());
                let pinned: String = sqlx::query_scalar("SELECT schema_hashes::text FROM komika_import_state")
                    .fetch_one(&mut pg).await.unwrap();
                super::super::schema::verify(&mut pg).await.unwrap(); // disabled triggers do not change definitions
                super::super::hang_up(pg).await;
                let report = importer::run(&path, &url, None).await.unwrap();
                assert_eq!(report.tables.iter().filter(|r| r.resumed).count(), 32);
                let mut pg = connect(&url).await.unwrap();
                let after: String = sqlx::query_scalar("SELECT schema_hashes::text FROM komika_import_state")
                    .fetch_one(&mut pg).await.unwrap();
                assert_eq!(pinned, after);
                assert!(verifier::run(&path, &url).await.unwrap().complete);
            })
        })
        .await;
    }
    #[tokio::test]
    async fn bad_copy_row_rolls_back_whole_table_and_trigger_change() {
        with_database(|path,url|boxed(async move {
            assert!(importer::run(&path,&url,Some(28)).await.is_err());
            let mut source=source::open(&path).await.unwrap();let mut pg=connect(&url).await.unwrap();
            // Inject target-only failure after successful source checks. COPY must
            // roll back even if preceding rows in this same table were accepted.
            sqlx::query("ALTER TABLE work_alias ADD CONSTRAINT test_bad_row CHECK (id <> 'a_00000000000040008000000000000013')").execute(&mut pg).await.unwrap();
            let table=tables().into_iter().find(|t|t.name=="work_alias").unwrap();assert!(copy::load(&mut source,&mut pg,&table).await.is_err());
            let n:i64=sqlx::query_scalar("SELECT count(*) FROM work_alias").fetch_one(&mut pg).await.unwrap();assert_eq!(n,0);
            let n:i64=sqlx::query_scalar("SELECT count(*) FROM komika_import_table WHERE table_name='work_alias'").fetch_one(&mut pg).await.unwrap();assert_eq!(n,0);
            let enabled:bool=sqlx::query_scalar("SELECT tgenabled='O' FROM pg_trigger WHERE tgrelid='public.work_alias'::regclass AND tgname='work_search_alias_sync'").fetch_one(&mut pg).await.unwrap();assert!(enabled);
        })).await;
    }
    #[tokio::test]
    async fn finished_target_refuses_second_import() {
        with_database(|path, url| {
            boxed(async move {
                importer::run(&path, &url, None).await.unwrap();
                // An invalid source would fail the full scan if it ran first.
                mutate_sqlite(&path, "UPDATE work SET created_at='invalid date'").await;
                assert!(importer::run(&path, &url, None)
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("already complete"));
            })
        })
        .await;
    }
    #[tokio::test]
    async fn different_resume_source_refused_even_if_header_unchanged() {
        with_database(|path, url| {
            boxed(async move {
                let options = sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
                let mut writer = SqliteConnection::connect_with(&options).await.unwrap();
                assert!(importer::run(&path, &url, Some(2)).await.is_err());
                let before = std::fs::read(&path).unwrap();
                sqlx::query("UPDATE work SET description='different snapshot'")
                    .execute(&mut writer)
                    .await
                    .unwrap();
                assert_eq!(
                    before,
                    std::fs::read(&path).unwrap(),
                    "change must be WAL-only"
                );
                assert!(importer::run(&path, &url, None)
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("source differs"));
            })
        })
        .await;
    }
    #[tokio::test]
    async fn precheck_failures_write_nothing() {
        with_database(|path,url|boxed(async move {
            mutate_sqlite(&path,"UPDATE work SET created_at='bad date',is_nsfw=2; UPDATE suwayomi_series SET genre='{\"dup\":1,\"dup\":2}'; UPDATE work_alias SET normalized_title='oversized'||printf('%02000d',1) WHERE id='a_00000000000040008000000000000013'; UPDATE users SET id='not-a-uuid' WHERE username='owner'").await;
            assert!(importer::run(&path,&url,None).await.is_err());assert_eq!(user_table_count(&url).await,0);
        })).await;
    }
    #[tokio::test]
    async fn arbitrary_target_refused_without_touching_it() {
        with_database(|path, url| {
            boxed(async move {
                let mut pg = connect(&url).await.unwrap();
                sqlx::raw_sql(
                    "CREATE TABLE unrelated (value text); INSERT INTO unrelated VALUES ('keep')",
                )
                .execute(&mut pg)
                .await
                .unwrap();
                mutate_sqlite(&path, "UPDATE work SET created_at='invalid date'").await;
                let error = importer::run(&path, &url, None)
                    .await
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("target is not empty"), "{error}");
                assert_eq!(user_table_count(&url).await, 1);
            })
        })
        .await;
    }
    #[tokio::test]
    async fn copy_oddities_pins_json_and_expanded_dates_survive() {
        with_database(|path,url|boxed(async move {
            mutate_sqlite(&path,"UPDATE work SET description=char(9)||char(10)||char(13)||'\\.\\N😀',created_at='+20250-01-03T19:44:17.225+00:00',artist='source'; INSERT INTO work_field_pin VALUES ('w_00000000000040008000000000000001','artist','pin','original source','2020-01-01T00:00:00Z'); UPDATE suwayomi_series SET genre='[123456789012345678901234567890,1.000,{\"z\":true,\"a\":null}]'; INSERT INTO user_avatars VALUES ('00000000-0000-4000-8000-000000000900',x'00090a0d5cff',1,'2020-01-01T00:00:00Z')").await;
            importer::run(&path,&url,None).await.unwrap();assert!(verifier::run(&path,&url).await.unwrap().complete);
        })).await;
    }
}

#[tokio::test]
async fn precheck_gates_null_keys_collisions_orphans_seconds_and_composite_bytes() {
    let (file, mut db) = fixture().await;
    sqlx::raw_sql("DROP TRIGGER users_username_nocase_update; DROP TRIGGER users_email_nocase_update; UPDATE users SET username='OWNER',email='OWNER@EXAMPLE.INVALID' WHERE username='reader'; INSERT INTO instance_setting VALUES (NULL,'private-secret','2020-01-01T00:00:00Z'); INSERT INTO release_event (work_id,chapter_key,first_seen_at,label) VALUES ('w_00000000000040008000000000000001','seconds',1767225600,'seconds'); UPDATE work_alias SET normalized_title=printf('%01000d',1), raw_title=printf('%01000d',2) WHERE id='a_00000000000040008000000000000013'").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&file.0)
        .foreign_keys(false);
    let mut db = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("UPDATE chapter SET source_series_id='missing-parent' WHERE id=(SELECT min(id) FROM chapter)").execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    let mut ro = source::open(&file.0).await.unwrap();
    let pre = source::precheck(&mut ro).await.unwrap();
    assert!(!pre.passed());
    let summary = serde_json::to_string(&pre).unwrap();
    for rule in [
        "NULL in required/primary-key",
        "users_username_ascii_unique",
        "users_email_ascii_unique",
        "FK orphan",
        "repair-upload-dates",
        "text-key budget",
    ] {
        assert!(summary.contains(rule), "{rule}");
    }
    for secret in ["private-secret", "OWNER@EXAMPLE.INVALID", "fixture-secret"] {
        assert!(!summary.contains(secret));
    }
    assert!(pre.rules.values().all(|r| r.examples.len() <= 5));
}

#[test]
fn json_copy_retains_input_number_text_and_frames_canonical_numbers() {
    let original = r#"{"z":1.000,"a":9007199254740993}"#;
    let cell = convert(&column("jsonb", "TEXT"), "text", Some(original.as_bytes()))
        .unwrap()
        .0;
    let mut encoded = Vec::new();
    copy::encode(&cell, &mut encoded).unwrap();
    assert_eq!(encoded, original.as_bytes());
    let equivalent = convert(
        &column("jsonb", "TEXT"),
        "text",
        Some(br#"{"a":9007199254740993,"z":1}"#),
    )
    .unwrap()
    .0;
    assert_eq!(cell, equivalent);
    let mut a = Vec::new();
    let mut b = Vec::new();
    frame(&cell, &mut a);
    frame(&equivalent, &mut b);
    assert_eq!(a, b);
}

#[test]
fn isolated_json_parser_matches_standard_grammar_and_preserves_private_keys() {
    let examples = [
        "null",
        "true",
        "false",
        "-0",
        "12.34e-5",
        r#""escaped \"quote\" \\ slash / \n \t""#,
        r#""\ud83d\ude00""#,
        r#"{"a":[null,true,1.2],"b":{}}"#,
        r#"["café",{},[]]"#,
    ];
    for valid in examples {
        assert!(json(valid).is_ok(), "{valid}");
        for end in 0..=valid.len() {
            if !valid.is_char_boundary(end) {
                continue;
            }
            let candidate = &valid[..end];
            assert_eq!(
                json(candidate).is_ok(),
                serde_json::from_str::<serde_json::Value>(candidate).is_ok(),
                "prefix {candidate:?}"
            );
        }
    }
    for invalid in [
        "[1,]",
        "{\"a\":1,}",
        "{1:2}",
        "{\"a\" 1}",
        "{\"a\":}",
        "[1 2]",
        "01",
        "-01",
        "1.",
        "1e",
        "1e+",
        "+1",
        ".5",
        "NaN",
        "Infinity",
        "tru",
        "nullnull",
        "{}\u{000b}",
        "{}\u{000c}",
        r#""\ud800""#,
        r#""\u000x""#,
        r#""\x""#,
    ] {
        assert!(json(invalid).is_err(), "{invalid:?}");
    }
    assert_eq!(
        json(r#"{"$serde_json::private::Number":"1"}"#).unwrap(),
        r#"{"$serde_json::private::Number":"1"}"#
    );
    let deep = format!("{}0{}", "[".repeat(128), "]".repeat(128));
    assert!(json(&deep).is_err());
}

#[test]
fn errors_redact_urls_credentials_and_driver_row_details() {
    let url = "postgresql://owner:private-password@secret-host/db";
    let app = anyhow::anyhow!("refused {url}");
    assert!(!cli::safe_error(&app, url).contains(url));
    let driver: anyhow::Error = sqlx::Error::Protocol(format!(
        "{url}; full row private-token private-email@example.invalid"
    ))
    .into();
    let safe = cli::safe_error(&driver, url);
    for secret in [
        "secret-host",
        "private-password",
        "private-token",
        "private-email",
    ] {
        assert!(!safe.contains(secret));
    }
}
