//! Opt-in repair plan: preview a production BACKUP COPY, inspect unresolved counts,
//! then apply to that copy and preview again before considering production.
//! `komika-server repair-upload-dates sqlite:///absolute/copy.sqlite3 [--apply]`.
//! No startup hook, migration, network access or table rebuild. Like catalogue
//! refreshes, staging uses read-only main + private TEMP tables; publication uses
//! bounded, compare-and-swap writer turns. A crash is safe to rerun. The existing
//! maintenance_flag records completion only when every staged change was published.
//!
//! After source-based repair, orphan events with no usable source chapter get a
//! separate stored-value repair: seconds-like clocks use the application's shared
//! epoch normalizer, just as the fixed write path does. Rejected orphan clocks stay
//! unchanged and unresolved; first-source history is preserved. Their ledger-derived
//! feed/browse clocks are staged and published too. Rejected raw values clear chapter dates;
//! an invalid ledger event with no valid release is removed. A corrupt announce clock
//! falls back to the corrected release: historic availability cannot be reconstructed.

use std::{str::FromStr, time::Duration};

use anyhow::{ensure, Result};
use sqlx::{Connection, Row, SqliteConnection};

// SQLite repair must retain SQLite values after the main application engine flip.
#[derive(Debug, PartialEq)]
enum RefreshValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

fn refresh_values(
    row: &sqlx::sqlite::SqliteRow,
    start: usize,
    count: usize,
) -> Result<Vec<RefreshValue>> {
    use sqlx::{Row, TypeInfo, ValueRef};
    (start..start + count)
        .map(|i| {
            let value = row.try_get_raw(i)?;
            Ok(if value.is_null() {
                RefreshValue::Null
            } else {
                match value.type_info().name() {
                    "INTEGER" | "BOOLEAN" => RefreshValue::Integer(row.try_get(i)?),
                    "REAL" | "NUMERIC" => RefreshValue::Real(row.try_get(i)?),
                    "BLOB" => RefreshValue::Blob(row.try_get(i)?),
                    _ => RefreshValue::Text(row.try_get(i)?),
                }
            })
        })
        .collect()
}

fn bind_refresh_values<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    values: &'q [RefreshValue],
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    for value in values {
        query = match value {
            RefreshValue::Null => query.bind(Option::<String>::None),
            RefreshValue::Integer(v) => query.bind(*v),
            RefreshValue::Real(v) => query.bind(*v),
            RefreshValue::Text(v) => query.bind(v),
            RefreshValue::Blob(v) => query.bind(v),
        };
    }
    query
}

const FLAG: &str = "suwayomi-upload-date-units-v1";
const TABLES: &[(&str, &[&str])] = &[
    ("chapter", &["id"]),
    ("release_event", &["work_id", "chapter_key"]),
    ("feed_series_updates", &["work_id"]),
    ("browse_catalogue", &["work_id"]),
];
// Signed historic milliseconds (back to AD 1, matching the importer's range)
// are valid. Only recorded Suwayomi raw data can prove they need redating.
const BAD_PROJECTION_EPOCH: &str =
    "(released_at < -62135596800000 OR released_at BETWEEN 0 AND 99999999999 OR released_at > 4102444800000 \
    OR announced_at < -62135596800000 OR announced_at BETWEEN 0 AND 99999999999 OR announced_at > 4102444800000)";
const BAD_EPOCH: &str =
    "(first_seen_at < -62135596800000 OR first_seen_at BETWEEN 0 AND 99999999999 OR first_seen_at > 4102444800000 \
    OR announce_at < -62135596800000 OR announce_at BETWEEN 0 AND 99999999999 OR announce_at > 4102444800000)";
const BAD_SOURCE_EPOCH: &str = "(first_seen_at < 100000000000 OR first_seen_at > 4102444800000 \
    OR announce_at <= 0 OR announce_at BETWEEN 1 AND 99999999999 \
    OR announce_at > 4102444800000)";

pub async fn run_cli(args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 1 || (args.len() == 2 && args[1] == "--apply"),
        "usage: repair-upload-dates sqlite:///absolute/database-copy.sqlite3 [--apply]"
    );
    let apply = args.len() == 2;
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(&args[0])?
        .create_if_missing(false)
        .read_only(true)
        .foreign_keys(false)
        .pragma("query_only", "OFF");
    ensure!(
        options.get_filename().is_absolute(),
        "use an absolute database-copy path"
    );
    let mut snapshot = SqliteConnection::connect_with(&options).await?;
    stage(&mut snapshot).await?;
    let orphan_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orphan_event")
        .fetch_one(&mut snapshot)
        .await?;
    println!("orphan release_event: {orphan_count} rows would change (stored-value normalization)");
    let mut total = 0;
    for (table, keys) in TABLES {
        let count: i64 = sqlx::query_scalar(&diff_sql(table, keys, true))
            .fetch_one(&mut snapshot)
            .await?;
        if *table == "release_event" {
            println!(
                "{table}: {count} rows would change ({} source-based, {orphan_count} orphan)",
                count - orphan_count
            );
        } else {
            println!("{table}: {count} rows would change");
        }
        total += count;
    }
    // This table derives ONLY from MangaDex chapters, not Suwayomi upload_date.
    println!("feed_updates: 0 rows would change (MangaDex-only projection)");
    let unresolved_chapters: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM main.chapter c JOIN main.source_series ss \
         ON ss.id = c.source_series_id WHERE ss.source_type = 'suwayomi' \
         AND (COALESCE(c.readable_at, c.published_at) < '1973' \
              OR COALESCE(c.readable_at, c.published_at) > '2100-01-01T00:00:00+00:00' \
              OR strftime('%s', COALESCE(c.readable_at, c.published_at)) IS NULL \
                 AND COALESCE(c.readable_at, c.published_at) IS NOT NULL) \
         AND NOT EXISTS (SELECT 1 FROM normalized n WHERE n.id = c.id)",
    )
    .fetch_one(&mut snapshot)
    .await?;
    let unresolved_events: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM release_event WHERE {BAD_EPOCH}"
    ))
    .fetch_one(&mut snapshot)
    .await?;
    println!("unresolved (left unchanged): chapter={unresolved_chapters}, release_event={unresolved_events}");
    if apply {
        // Existing maintenance CLI connection: checks migration checksums, never migrates.
        let pool = crate::db::connect_existing_sqlite(&args[0]).await?;
        let mut changed = 0;
        for (table, keys) in TABLES {
            let expected: i64 = sqlx::query_scalar(&diff_sql(table, keys, true))
                .fetch_one(&mut snapshot)
                .await?;
            let n = publish(&pool, &mut snapshot, table, keys).await?;
            println!("{table}: {n} rows applied");
            changed += n;
            if n != expected {
                // Do not publish projections from a source/ledger snapshot that lost
                // a race. Earlier batches are durable and the next run resumes them.
                pool.close().await;
                anyhow::bail!("concurrent {table} changes skipped; rerun preview/apply");
            }
            if *table == "release_event" {
                println!("orphan release_event: {orphan_count} rows applied (stored-value normalization)");
            }
        }
        if changed == total && unresolved_chapters == 0 && unresolved_events == 0 {
            if !mark_complete(&pool).await? {
                println!("unresolved catalogue clocks remain; completion not recorded");
            }
        } else {
            println!(
                "unresolved or concurrent changes remain; rerun preview/apply before completion"
            );
        }
        pool.close().await;
    }
    snapshot.close().await?;
    Ok(())
}

async fn stage(conn: &mut SqliteConnection) -> Result<()> {
    // One consistent read snapshot; TEMP writes never take the main SQLite writer.
    sqlx::query("BEGIN").execute(&mut *conn).await?;
    for (table, keys) in TABLES {
        for prefix in ["previous_", ""] {
            sqlx::query(&format!(
                "CREATE TEMP TABLE {prefix}{table} AS SELECT * FROM main.{table}"
            ))
            .execute(&mut *conn)
            .await?;
            sqlx::query(&format!(
                "CREATE UNIQUE INDEX temp.{prefix}{table}_key ON {prefix}{table} ({})",
                keys.join(",")
            ))
            .execute(&mut *conn)
            .await?;
        }
    }
    sqlx::query(
        "CREATE TEMP TABLE normalized (id TEXT PRIMARY KEY, source_series_id TEXT, \
        chapter_key TEXT, work_id TEXT, iso TEXT, ms INTEGER)",
    )
    .execute(&mut *conn)
    .await?;
    let mut cursor = String::new();
    loop {
        let rows = sqlx::query(
            "SELECT c.id, c.source_series_id, c.chapter_key, ss.work_id, sc.upload_date \
             FROM main.chapter c JOIN main.source_series ss ON ss.id = c.source_series_id \
             JOIN main.suwayomi_chapter sc ON sc.id = CAST(c.external_id AS INTEGER) \
                  AND sc.manga_id = CAST(ss.source_key AS INTEGER) \
             WHERE ss.source_type = 'suwayomi' AND c.id > $1 ORDER BY c.id LIMIT 512",
        )
        .bind(&cursor)
        .fetch_all(&mut *conn)
        .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            cursor = row.try_get("id")?;
            let raw: Option<String> = row.try_get("upload_date")?;
            let ms = raw
                .as_deref()
                .and_then(crate::series_cache::normalize_epoch_millis);
            let iso = suwayomi_upload_date_to_iso(raw.as_deref());
            sqlx::query("INSERT INTO normalized VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(&cursor)
                .bind(row.try_get::<String, _>("source_series_id")?)
                .bind(row.try_get::<Option<String>, _>("chapter_key")?)
                .bind(row.try_get::<String, _>("work_id")?)
                .bind(iso)
                .bind(ms)
                .execute(&mut *conn)
                .await?;
        }
    }
    sqlx::query(
        "CREATE INDEX temp.normalized_source_key ON normalized(source_series_id, chapter_key)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE chapter SET published_at = n.iso, readable_at = n.iso \
        FROM normalized n WHERE chapter.id = n.id",
    )
    .execute(&mut *conn)
    .await?;
    // strftime in the ledger truncates subsecond precision; keep that contract.
    sqlx::query(
        "CREATE TEMP TABLE corrected_event AS \
        SELECT r.work_id, r.chapter_key, MIN(n.ms / 1000 * 1000) AS ms \
        FROM main.release_event r JOIN normalized n \
          ON n.source_series_id = r.first_source_series_id AND n.chapter_key = r.chapter_key \
        GROUP BY r.work_id, r.chapter_key",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX temp.corrected_event_key ON corrected_event(work_id, chapter_key)",
    )
    .execute(&mut *conn)
    .await?;
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(&format!(
        "UPDATE release_event SET \
         first_seen_at = CASE WHEN first_seen_at < 100000000000 OR first_seen_at > 4102444800000 \
                              THEN e.ms ELSE first_seen_at END, \
         announce_at = CASE WHEN announce_at <= 0 OR announce_at BETWEEN 1 AND 99999999999 \
                                OR announce_at > 4102444800000 THEN \
              CASE WHEN first_seen_at < 100000000000 OR first_seen_at > 4102444800000 \
                   THEN e.ms ELSE first_seen_at END ELSE announce_at END \
         FROM corrected_event e WHERE e.work_id = release_event.work_id \
         AND e.chapter_key = release_event.chapter_key AND e.ms <= $1 AND {BAD_SOURCE_EPOCH}"
    ))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    // Keep the event when a sibling source can honestly date the same key.
    // Deleting it would let the ordinary seed re-announce back-catalogue with now
    // via its availability gate. The single MIN chooses the source/label together.
    sqlx::query("CREATE TEMP TABLE fallback_event AS \
        SELECT r.work_id, r.chapter_key, \
          MIN(CAST(strftime('%s', COALESCE(c.readable_at, c.published_at)) AS INTEGER) * 1000) AS ms, \
          c.source_series_id, COALESCE(c.label, c.number, 'Oneshot') AS label \
        FROM release_event r LEFT JOIN corrected_event e USING(work_id, chapter_key) \
        JOIN main.source_series ss ON ss.work_id = r.work_id \
        JOIN chapter c ON c.source_series_id = ss.id AND c.chapter_key = r.chapter_key \
        WHERE (e.ms IS NULL OR e.ms > $1) \
          AND (e.work_id IS NOT NULL OR \
               CAST(strftime('%s', COALESCE(c.readable_at, c.published_at)) AS INTEGER) * 1000 >= 100000000000) \
          AND (e.work_id IS NOT NULL OR r.first_seen_at >= 0) \
          AND (r.first_seen_at < CASE WHEN e.work_id IS NULL THEN 100000000000 ELSE 1000000000000 END \
               OR r.first_seen_at > 4102444800000) \
          AND CAST(strftime('%s', COALESCE(c.readable_at, c.published_at)) AS INTEGER) * 1000 BETWEEN 1 AND $2 \
        GROUP BY r.work_id, r.chapter_key")
        .bind(now).bind(now).execute(&mut *conn).await?;
    sqlx::query(
        "CREATE UNIQUE INDEX temp.fallback_event_key ON fallback_event(work_id, chapter_key)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE release_event SET first_seen_at = f.ms, \
        first_source_series_id = f.source_series_id, label = f.label, \
        announce_at = CASE WHEN announce_at = first_seen_at OR announce_at <= 0 \
          OR announce_at BETWEEN 1 AND 99999999999 OR announce_at > 4102444800000 \
          THEN f.ms ELSE announce_at END \
        FROM fallback_event f WHERE f.work_id = release_event.work_id \
          AND f.chapter_key = release_event.chapter_key",
    )
    .execute(&mut *conn)
    .await?;
    // Rejected 12-digit inputs previously looked like 1973–2001 millis, so they
    // are above the census's 1e11 cutoff. Only widen the cutoff when the recorded
    // source still exists and its raw value is rejected by the shared normalizer.
    sqlx::query(
        "DELETE FROM release_event WHERE EXISTS \
         (SELECT 1 FROM corrected_event e WHERE e.work_id = release_event.work_id \
          AND e.chapter_key = release_event.chapter_key AND (e.ms IS NULL OR e.ms > $1) \
          AND (first_seen_at < 100000000000 OR first_seen_at > 4102444800000 \
               OR e.ms IS NULL AND first_seen_at < 1000000000000)) \
         AND NOT EXISTS (SELECT 1 FROM fallback_event f WHERE f.work_id = release_event.work_id \
                          AND f.chapter_key = release_event.chapter_key)",
    )
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE release_event SET announce_at = first_seen_at \
        WHERE first_seen_at BETWEEN 100000000000 AND 4102444800000 \
        AND (announce_at <= 0 OR announce_at BETWEEN 1 AND 99999999999 \
             OR announce_at > 4102444800000) AND EXISTS \
        (SELECT 1 FROM corrected_event e WHERE e.work_id = release_event.work_id \
         AND e.chapter_key = release_event.chapter_key)",
    )
    .execute(&mut *conn)
    .await?;
    // Only events for which neither the recorded raw source nor a dated sibling
    // could supply a value reach this step. Stored seconds are the original
    // upstream value, so apply exactly the normalizer used by the fixed writer.
    sqlx::query("CREATE TEMP TABLE orphan_event (work_id TEXT, chapter_key TEXT, first_seen_at INTEGER, announce_at INTEGER, PRIMARY KEY(work_id, chapter_key))")
        .execute(&mut *conn).await?;
    let mut cursor = 0i64;
    loop {
        let rows = sqlx::query(&format!(
            "SELECT rowid, work_id, chapter_key, first_seen_at, announce_at FROM release_event r \
             WHERE rowid > $1 AND {BAD_EPOCH} \
             AND NOT EXISTS (SELECT 1 FROM corrected_event e WHERE e.work_id=r.work_id AND e.chapter_key=r.chapter_key) \
             AND NOT EXISTS (SELECT 1 FROM fallback_event f WHERE f.work_id=r.work_id AND f.chapter_key=r.chapter_key) \
             ORDER BY rowid LIMIT 512"
        )).bind(cursor).fetch_all(&mut *conn).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            cursor = row.try_get("rowid")?;
            let first: i64 = row.try_get("first_seen_at")?;
            let announce: Option<i64> = row.try_get("announce_at")?;
            let repaired_first = orphan_clock(first);
            let repaired_announce = announce.map(orphan_clock);
            if first != repaired_first || announce != repaired_announce {
                sqlx::query("INSERT INTO orphan_event VALUES ($1,$2,$3,$4)")
                    .bind(row.try_get::<String, _>("work_id")?)
                    .bind(row.try_get::<String, _>("chapter_key")?)
                    .bind(repaired_first)
                    .bind(repaired_announce)
                    .execute(&mut *conn)
                    .await?;
            }
        }
    }
    sqlx::query("UPDATE release_event SET first_seen_at=o.first_seen_at, announce_at=o.announce_at FROM orphan_event o WHERE o.work_id=release_event.work_id AND o.chapter_key=release_event.chapter_key")
        .execute(&mut *conn).await?;
    sqlx::query("CREATE TEMP TABLE affected (work_id TEXT PRIMARY KEY)")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO affected SELECT n.work_id FROM normalized n \
        JOIN previous_chapter p ON p.id = n.id \
        WHERE p.published_at IS DISTINCT FROM n.iso OR p.readable_at IS DISTINCT FROM n.iso ON CONFLICT DO NOTHING",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO affected SELECT p.work_id FROM previous_release_event p \
        LEFT JOIN release_event r USING(work_id, chapter_key) WHERE r.work_id IS NULL \
        OR r.first_seen_at IS DISTINCT FROM p.first_seen_at OR r.announce_at IS DISTINCT FROM p.announce_at ON CONFLICT DO NOTHING",
    )
    .execute(&mut *conn)
    .await?;
    // Re-derive every candidate's projections, even when their old clocks are
    // plausible. Earlier chapter/ledger commits can survive an interruption.
    // Leave works with unresolved ledger clocks out of this recovery branch.
    sqlx::query(&format!(
        "INSERT OR IGNORE INTO affected SELECT a.work_id FROM \
        (SELECT work_id FROM normalized UNION SELECT work_id FROM release_event) a \
        WHERE NOT EXISTS (SELECT 1 FROM release_event WHERE work_id=a.work_id AND {BAD_EPOCH})"
    ))
    .execute(&mut *conn)
    .await?;
    // Exactly one MAX at this query level binds the label to its release winner.
    // Announcement aggregation must not choose the release label's source row.
    sqlx::query("CREATE TEMP TABLE clocks AS SELECT a.work_id, \
        MAX(r.first_seen_at) AS released, r.label AS latest_chapter, CAST(NULL AS INTEGER) AS announced \
        FROM affected a LEFT JOIN release_event r USING(work_id) GROUP BY a.work_id")
        .execute(&mut *conn).await?;
    sqlx::query(
        "UPDATE clocks SET announced=(SELECT MAX(COALESCE(r.announce_at,r.first_seen_at)) \
        FROM release_event r WHERE r.work_id=clocks.work_id)",
    )
    .execute(&mut *conn)
    .await?;
    // No ledger survivor: use actual remaining normalized source dates, or the
    // MangaDex-only projection. Never substitute the time the repair is run.
    sqlx::query(
        "UPDATE clocks SET (released,latest_chapter) = \
        (SELECT MAX(n.ms / 1000 * 1000),COALESCE(c.label,c.number,'Oneshot') \
         FROM normalized n JOIN chapter c ON c.id=n.id \
         WHERE n.work_id=clocks.work_id AND n.ms <= $1) WHERE released IS NULL",
    )
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE clocks SET (released,latest_chapter) = \
        (SELECT CAST(strftime('%s',f.latest_at) AS INTEGER)*1000,f.latest_chapter \
         FROM main.feed_updates f WHERE f.work_id=clocks.work_id) WHERE released IS NULL",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE clocks SET announced = released WHERE announced IS NULL")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE feed_series_updates SET released_at = k.released, announced_at = k.announced, latest_chapter = k.latest_chapter \
        FROM clocks k WHERE k.work_id = feed_series_updates.work_id AND k.released IS NOT NULL",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM feed_series_updates WHERE work_id IN \
        (SELECT work_id FROM clocks WHERE released IS NULL)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE browse_catalogue SET released_at = k.released, announced_at = k.announced, latest_chapter = k.latest_chapter \
        FROM clocks k WHERE k.work_id = browse_catalogue.work_id",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("COMMIT").execute(&mut *conn).await?;
    Ok(())
}

fn orphan_clock(value: i64) -> i64 {
    if (1..100_000_000_000).contains(&value) {
        crate::series_cache::normalize_epoch_millis(&value.to_string()).unwrap_or(value)
    } else {
        value
    }
}

async fn mark_complete(pool: &crate::db::SqliteDatabase) -> Result<bool> {
    // Check live projections and publish the marker in one writer statement.
    // No-ledger candidates use the already repaired chapter dates, then the
    // MangaDex-only feed, exactly as staging does. A concurrent ledger write or
    // plausible stale projection must not pass merely because it is in range.
    Ok(sqlx::query(&format!(
        "WITH candidates AS (SELECT work_id FROM release_event UNION \
          SELECT ss.work_id FROM chapter c JOIN source_series ss ON ss.id=c.source_series_id \
          JOIN suwayomi_chapter sc ON sc.id=CAST(c.external_id AS INTEGER) \
            AND sc.manga_id=CAST(ss.source_key AS INTEGER) WHERE ss.source_type='suwayomi'), \
         ledger AS (SELECT a.work_id, MAX(r.first_seen_at) AS released, r.label AS latest_chapter \
          FROM candidates a LEFT JOIN release_event r USING(work_id) GROUP BY a.work_id), \
         source_dates AS (SELECT ss.work_id, \
          MAX(CAST(strftime('%s',COALESCE(c.readable_at,c.published_at)) AS INTEGER)*1000) AS released, \
          COALESCE(c.label,c.number,'Oneshot') AS latest_chapter \
           FROM chapter c JOIN source_series ss ON ss.id=c.source_series_id \
           JOIN suwayomi_chapter sc ON sc.id=CAST(c.external_id AS INTEGER) \
             AND sc.manga_id=CAST(ss.source_key AS INTEGER) \
           WHERE ss.source_type='suwayomi' \
             AND CAST(strftime('%s',COALESCE(c.readable_at,c.published_at)) AS INTEGER)*1000 <= $3 \
           GROUP BY ss.work_id), \
         clocks AS (SELECT l.work_id, COALESCE(l.released,s.released, \
          CAST(strftime('%s',f.latest_at) AS INTEGER)*1000) AS released, \
          CASE WHEN l.released IS NOT NULL THEN l.latest_chapter \
               WHEN s.released IS NOT NULL THEN s.latest_chapter ELSE f.latest_chapter END AS latest_chapter, \
          (SELECT MAX(COALESCE(r.announce_at,r.first_seen_at)) FROM release_event r WHERE r.work_id=l.work_id) AS announced \
          FROM ledger l LEFT JOIN source_dates s USING(work_id) LEFT JOIN feed_updates f USING(work_id)) \
         INSERT INTO maintenance_flag (key, done_at) SELECT $1, $2 \
         WHERE NOT EXISTS (SELECT 1 FROM release_event WHERE {BAD_EPOCH}) \
         AND NOT EXISTS (SELECT 1 FROM feed_series_updates WHERE {BAD_PROJECTION_EPOCH}) \
         AND NOT EXISTS (SELECT 1 FROM browse_catalogue WHERE {BAD_PROJECTION_EPOCH}) \
         AND NOT EXISTS (SELECT 1 FROM clocks k JOIN feed_series_updates f USING(work_id) \
           WHERE k.released IS NULL OR f.released_at IS NOT k.released \
             OR f.announced_at IS NOT COALESCE(k.announced,k.released) \
             OR f.latest_chapter IS NOT k.latest_chapter) \
         AND NOT EXISTS (SELECT 1 FROM clocks k JOIN browse_catalogue b USING(work_id) \
           WHERE b.released_at IS NOT k.released \
             OR b.announced_at IS NOT COALESCE(k.announced,k.released) \
             OR b.latest_chapter IS NOT k.latest_chapter) \
         ON CONFLICT(key) DO UPDATE SET done_at = maintenance_flag.done_at"
    ))
    .bind(FLAG)
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(chrono::Utc::now().timestamp_millis())
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

fn diff_sql(table: &str, keys: &[&str], count: bool) -> String {
    let join = keys
        .iter()
        .map(|k| format!("p.{k} IS s.{k}"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let projection = if count {
        "COUNT(*)"
    } else {
        "p.rowid, p.*, s.*"
    };
    let dates: &[&str] = match table {
        "chapter" => &["published_at", "readable_at"],
        "release_event" => &[
            "first_seen_at",
            "announce_at",
            "first_source_series_id",
            "label",
        ],
        _ => &["released_at", "announced_at", "latest_chapter"],
    };
    let different = dates
        .iter()
        .map(|c| format!("p.{c} IS NOT s.{c}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    // All columns are selected deliberately: concurrent source/admin writes win.
    format!(
        "SELECT {projection} FROM previous_{table} p LEFT JOIN {table} s ON {join} \
        WHERE (s.{} IS NULL OR {different})",
        keys[0]
    )
}

async fn publish(
    pool: &crate::db::SqliteDatabase,
    snapshot: &mut SqliteConnection,
    table: &str,
    keys: &[&str],
) -> Result<i64> {
    let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&format!("PRAGMA temp.table_info({table})"))
            .fetch_all(&mut *snapshot)
            .await?;
    let names: Vec<&str> = columns.iter().map(|c| c.1.as_str()).collect();
    let same = |start| {
        let parameters = crate::sql_params::placeholders(start, names.len());
        names
            .iter()
            .zip(parameters.split(", "))
            .map(|(c, parameter)| format!("{c} IS {parameter}"))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let parameters = crate::sql_params::placeholders(1, names.len());
    let sets = names
        .iter()
        .zip(parameters.split(", "))
        .map(|(c, parameter)| format!("{c} = {parameter}"))
        .collect::<Vec<_>>()
        .join(", ");
    let key_index = names.iter().position(|n| *n == keys[0]).unwrap();
    let mut cursor = 0i64;
    let mut changed = 0;
    loop {
        let rows = sqlx::query(&format!(
            "{} AND p.rowid > $1 ORDER BY p.rowid LIMIT 100",
            diff_sql(table, keys, false)
        ))
        .bind(cursor)
        .fetch_all(&mut *snapshot)
        .await?;
        if rows.is_empty() {
            break;
        }
        let mut tx = pool.begin().await?;
        let started = std::time::Instant::now();
        for row in rows {
            if started.elapsed() >= Duration::from_millis(50) {
                break;
            }
            cursor = row.try_get(0)?;
            let old = refresh_values(&row, 1, names.len())?;
            let new = refresh_values(&row, 1 + names.len(), names.len())?;
            let deleted = new[key_index] == RefreshValue::Null;
            let sql = if deleted {
                {
                    let same = same(1);
                    format!("DELETE FROM main.{table} WHERE {same}")
                }
            } else {
                {
                    let same = same(names.len() + 1);
                    format!("UPDATE main.{table} SET {sets} WHERE {same}")
                }
            };
            let q = if deleted {
                sqlx::query(&sql)
            } else {
                bind_refresh_values(sqlx::query(&sql), &new)
            };
            changed += bind_refresh_values(q, &old)
                .execute(&mut *tx)
                .await?
                .rows_affected() as i64;
        }
        tx.commit().await?;
        // Give scanner/background writers a turn between bounded transactions.
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(changed)
}

// Retained SQLite source repair keeps its historical storage spelling.
// C10 applies at the application API edge, independently of this offline path.
pub(crate) fn suwayomi_upload_date_to_iso(upload_date: Option<&str>) -> Option<String> {
    let ms = crate::series_cache::normalize_epoch_millis(upload_date?)?;
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::WorkInput;
    use sqlite_fixture as catalog;
    #[path = "sqlite_fixture.rs"]
    mod sqlite_fixture;

    async fn fixture(raw: &str) -> (crate::db::SqliteDatabase, String, String) {
        let pool = crate::db::SqliteDatabase::connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        let work = catalog::upsert_work_from_mangadex(&pool, "date-repair", &WorkInput::default())
            .await
            .unwrap();
        let ss = catalog::upsert_source_series(&pool, &work, "suwayomi", "src", "777", None, false)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO suwayomi_chapter \
            (id, manga_id, name, chapter_number, upload_date, page_count, updated_at) \
            VALUES (9, 777, 'Chapter 9', 9, $1, 0, '2026-01-01')",
        )
        .bind(raw)
        .execute(&pool)
        .await
        .unwrap();
        let mut input = catalog::suwayomi_spine_input(9, "Chapter 9", 9.0, None, Some(raw));
        // Reproduce the old unvalidated millis conversion, including year 20250.
        let bad = chrono::DateTime::from_timestamp_millis(raw.parse().unwrap())
            .unwrap()
            .to_rfc3339();
        input.published_at = Some(bad.clone());
        input.readable_at = Some(bad);
        catalog::upsert_chapter(&pool, &ss, &input).await.unwrap();
        sqlx::query(
            "INSERT INTO release_event VALUES ($1, 'n:900', 1767225000, $2, 'Ch.9', 1767225000)",
        )
        .bind(&work)
        .bind(&ss)
        .execute(&pool)
        .await
        .unwrap();
        // Use the real grouping key rather than a guessed label spelling.
        sqlx::query("UPDATE release_event SET chapter_key = (SELECT chapter_key FROM chapter)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO feed_series_updates \
            (work_id, reader_id, title, released_at, announced_at) \
            VALUES ($1, '777', 'Repair', 1767225000, 1767225000)",
        )
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO browse_catalogue \
            (work_id, reader_id, title, released_at, announced_at, created_at) \
            VALUES ($1, '777', 'Repair', 1767225000, 1767225000, '2026-01-01') \
            ON CONFLICT(work_id) DO UPDATE SET released_at = excluded.released_at, \
                announced_at = excluded.announced_at",
        )
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
        (pool, work, ss)
    }

    async fn snapshot(pool: &crate::db::SqliteDatabase) -> SqliteConnection {
        let options = (*pool.connect_options())
            .clone()
            .read_only(true)
            .foreign_keys(false)
            .pragma("query_only", "OFF");
        let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
        stage(&mut conn).await.unwrap();
        conn
    }

    async fn apply(pool: &crate::db::SqliteDatabase, conn: &mut SqliteConnection) -> i64 {
        let mut changed = 0;
        for (table, keys) in TABLES {
            changed += publish(pool, conn, table, keys).await.unwrap();
        }
        changed
    }

    #[tokio::test]
    async fn preview_is_read_only_and_apply_is_idempotent_after_interruption() {
        let (pool, _, _) = fixture("1767225600").await;
        let url = format!(
            "sqlite://{}",
            pool.connect_options().get_filename().display()
        );
        run_cli(&[url]).await.unwrap(); // The CLI defaults to the read-only preview.
        let mut conn = snapshot(&pool).await;
        let main: i64 = sqlx::query_scalar("SELECT first_seen_at FROM main.release_event")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(main, 1_767_225_000);
        // The read-only main connection cannot mutate even if the preview has a bug.
        assert!(sqlx::query("DELETE FROM main.release_event")
            .execute(&mut conn)
            .await
            .is_err());
        for (table, keys) in TABLES {
            let count: i64 = sqlx::query_scalar(&diff_sql(table, keys, true))
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(count, 1, "{table}");
        }
        // Crash after repairing chapters/ledger but before the materialized feeds.
        assert_eq!(
            publish(&pool, &mut conn, "chapter", &["id"]).await.unwrap(),
            1
        );
        assert_eq!(
            publish(
                &pool,
                &mut conn,
                "release_event",
                &["work_id", "chapter_key"]
            )
            .await
            .unwrap(),
            1
        );
        conn.close().await.unwrap();
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 2);
        let dates: (String, String) =
            sqlx::query_as("SELECT published_at, readable_at FROM chapter")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            dates,
            (
                "2026-01-01T00:00:00+00:00".into(),
                "2026-01-01T00:00:00+00:00".into()
            )
        );
        for (table, column) in [
            ("release_event", "first_seen_at"),
            ("feed_series_updates", "released_at"),
            ("browse_catalogue", "announced_at"),
        ] {
            let value: i64 = sqlx::query_scalar(&format!("SELECT {column} FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(value, 1_767_225_600_000);
        }
        conn.close().await.unwrap();
        assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
    }

    #[tokio::test]
    async fn corrupt_source_clears_dates_without_inventing_a_release() {
        for raw in ["-26616935569000", "576861335057225", "999999999999"] {
            let (pool, _, _) = fixture(raw).await;
            if raw == "999999999999" {
                sqlx::query(
                    "UPDATE release_event SET first_seen_at = 999999999000, \
                    announce_at = 999999999000",
                )
                .execute(&pool)
                .await
                .unwrap();
            }
            let mut conn = snapshot(&pool).await;
            assert_eq!(apply(&pool, &mut conn).await, 4);
            let dates: (Option<String>, Option<String>) =
                sqlx::query_as("SELECT published_at, readable_at FROM chapter")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(dates, (None, None));
            for table in ["release_event", "feed_series_updates"] {
                let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(count, 0);
            }
            let date: Option<i64> = sqlx::query_scalar("SELECT released_at FROM browse_catalogue")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(date, None);
            conn.close().await.unwrap();
            assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
        }
    }

    #[tokio::test]
    async fn valid_millis_and_availability_clocks_are_preserved() {
        let (pool, _, _) = fixture("1767225600225").await;
        sqlx::query("UPDATE release_event SET announce_at = 1791028800000")
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 3);
        let published: String = sqlx::query_scalar("SELECT published_at FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(published, "2026-01-01T00:00:00.225+00:00");
        let clocks: (i64, i64) =
            sqlx::query_as("SELECT first_seen_at, announce_at FROM release_event")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(clocks, (1_767_225_600_000, 1_791_028_800_000));
        conn.close().await.unwrap();
        assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);

        let (pool, _, _) = fixture("2145884400000").await;
        let mut conn = snapshot(&pool).await;
        apply(&pool, &mut conn).await;
        let published: String = sqlx::query_scalar("SELECT published_at FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(published, "2037-12-31T15:00:00+00:00");
    }

    #[tokio::test]
    async fn rejected_winner_uses_a_dated_sibling_without_reannouncing_history() {
        let (pool, _, _) = fixture("999999999999").await;
        let md: String =
            sqlx::query_scalar("SELECT id FROM source_series WHERE source_type = 'mangadex'")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query("UPDATE source_series SET created_at = '2020-01-01T00:00:00Z' WHERE id = $1")
            .bind(&md)
            .execute(&pool)
            .await
            .unwrap();
        catalog::upsert_chapter(
            &pool,
            &md,
            &catalog::ChapterInput {
                external_id: "md-9".into(),
                number: Some("9".into()),
                lang: Some("en".into()),
                readable_at: Some("2025-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 4);
        let event: (i64, i64, String) = sqlx::query_as(
            "SELECT first_seen_at, announce_at, first_source_series_id FROM release_event",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(event, (1_735_689_600_000, 1_735_689_600_000, md));
        assert_eq!(catalog::ledger::seed_batch(&pool).await.unwrap(), 0);
        conn.close().await.unwrap();
        assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
    }

    async fn plausible_sibling_fixture(date: &str) -> (crate::db::SqliteDatabase, String) {
        let (pool, _, _) = fixture("999999999999").await;
        for table in ["release_event", "feed_series_updates", "browse_catalogue"] {
            let (first, announce) = if table == "release_event" {
                ("first_seen_at", "announce_at")
            } else {
                ("released_at", "announced_at")
            };
            sqlx::query(&format!(
                "UPDATE {table} SET {first}=999999999000, {announce}=999999999000"
            ))
            .execute(&pool)
            .await
            .unwrap();
        }
        let md: String =
            sqlx::query_scalar("SELECT id FROM source_series WHERE source_type='mangadex'")
                .fetch_one(&pool)
                .await
                .unwrap();
        catalog::upsert_chapter(
            &pool,
            &md,
            &catalog::ChapterInput {
                external_id: "md-9".into(),
                number: Some("9".into()),
                lang: Some("en".into()),
                readable_at: Some(date.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE chapter SET label='Sibling' WHERE source_series_id=$1")
            .bind(&md)
            .execute(&pool)
            .await
            .unwrap();
        (pool, md)
    }

    #[tokio::test]
    async fn plausible_projections_resume_after_chapter_and_ledger_publication() {
        for interrupted in [false, true] {
            let (pool, _) = plausible_sibling_fixture("2025-01-01T00:00:00Z").await;
            let mut conn = snapshot(&pool).await;
            if interrupted {
                assert_eq!(
                    publish(&pool, &mut conn, "chapter", &["id"]).await.unwrap(),
                    1
                );
                assert_eq!(
                    publish(
                        &pool,
                        &mut conn,
                        "release_event",
                        &["work_id", "chapter_key"]
                    )
                    .await
                    .unwrap(),
                    1
                );
                assert!(!mark_complete(&pool).await.unwrap());
                conn.close().await.unwrap();
                conn = snapshot(&pool).await;
            }
            assert_eq!(
                apply(&pool, &mut conn).await,
                if interrupted { 2 } else { 4 }
            );
            for table in ["feed_series_updates", "browse_catalogue"] {
                let clocks: (i64, i64) =
                    sqlx::query_as(&format!("SELECT released_at,announced_at FROM {table}"))
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                assert_eq!(clocks, (1_735_689_600_000, 1_735_689_600_000));
            }
            assert!(mark_complete(&pool).await.unwrap());
            conn.close().await.unwrap();
            assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
        }
    }

    #[tokio::test]
    async fn same_clock_fallback_publishes_source_and_label_with_full_row_cas() {
        for concurrent in [false, true] {
            let (pool, md) = plausible_sibling_fixture("2001-09-09T01:46:39Z").await;
            let mut conn = snapshot(&pool).await;
            let count: i64 = sqlx::query_scalar(&diff_sql(
                "release_event",
                &["work_id", "chapter_key"],
                true,
            ))
            .fetch_one(&mut conn)
            .await
            .unwrap();
            assert_eq!(count, 1);
            if concurrent {
                sqlx::query("UPDATE release_event SET label='Concurrent'")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            let changed = publish(
                &pool,
                &mut conn,
                "release_event",
                &["work_id", "chapter_key"],
            )
            .await
            .unwrap();
            assert_eq!(changed, i64::from(!concurrent));
            let event: (i64, i64, String, String) = sqlx::query_as(
                "SELECT first_seen_at,announce_at,first_source_series_id,label FROM release_event",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!((event.0, event.1), (999_999_999_000, 999_999_999_000));
            if concurrent {
                assert_ne!(event.2, md);
                assert_eq!(event.3, "Concurrent");
            } else {
                assert_eq!((event.2, event.3), (md, "Sibling".into()));
            }
        }
    }

    #[tokio::test]
    async fn completion_rejects_plausible_drift_and_checks_no_ledger_fallback() {
        for fallback in [false, true] {
            let (pool, work, _) = fixture("1767225600").await;
            if fallback {
                sqlx::query("DELETE FROM release_event")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            let mut conn = snapshot(&pool).await;
            apply(&pool, &mut conn).await;
            assert!(mark_complete(&pool).await.unwrap());
            for table in ["feed_series_updates", "browse_catalogue"] {
                sqlx::query(&format!(
                    "UPDATE {table} SET released_at=999999999000 WHERE work_id=$1"
                ))
                .bind(&work)
                .execute(&pool)
                .await
                .unwrap();
                assert!(!mark_complete(&pool).await.unwrap());
                sqlx::query(&format!(
                    "UPDATE {table} SET released_at=1767225600000 WHERE work_id=$1"
                ))
                .bind(&work)
                .execute(&pool)
                .await
                .unwrap();
            }
            assert!(mark_complete(&pool).await.unwrap());
        }
    }

    async fn orphan_fixture(missing_series: bool) -> crate::db::SqliteDatabase {
        let (pool, _, ss) = fixture("1767225600").await;
        if missing_series {
            sqlx::query("DELETE FROM source_series WHERE id=$1")
                .bind(ss)
                .execute(&pool)
                .await
                .unwrap();
        } else {
            sqlx::query("DELETE FROM chapter")
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    async fn assert_orphan_repaired(missing_series: bool) {
        let pool = orphan_fixture(missing_series).await;
        let mut conn = snapshot(&pool).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orphan_event")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(apply(&pool, &mut conn).await, 3);
        for (table, first, announce) in [
            ("release_event", "first_seen_at", "announce_at"),
            ("feed_series_updates", "released_at", "announced_at"),
            ("browse_catalogue", "released_at", "announced_at"),
        ] {
            let clocks: (i64, i64) =
                sqlx::query_as(&format!("SELECT {first}, {announce} FROM {table}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(clocks, (1_767_225_000_000, 1_767_225_000_000));
        }
        assert!(mark_complete(&pool).await.unwrap());
        conn.close().await.unwrap();
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 0);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orphan_event")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn orphan_missing_source_series_repairs_and_second_run_changes_zero() {
        assert_orphan_repaired(true).await;
    }

    #[tokio::test]
    async fn orphan_missing_chapter_repairs_and_second_run_changes_zero() {
        assert_orphan_repaired(false).await;
    }

    #[tokio::test]
    async fn orphan_prefers_a_dated_sibling_to_stored_value_normalization() {
        let pool = orphan_fixture(true).await;
        let md: String =
            sqlx::query_scalar("SELECT id FROM source_series WHERE source_type='mangadex'")
                .fetch_one(&pool)
                .await
                .unwrap();
        catalog::upsert_chapter(
            &pool,
            &md,
            &catalog::ChapterInput {
                external_id: "md-9".into(),
                number: Some("9".into()),
                lang: Some("en".into()),
                readable_at: Some("2025-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut conn = snapshot(&pool).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orphan_event")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(apply(&pool, &mut conn).await, 3);
        let event: (i64, String) =
            sqlx::query_as("SELECT first_seen_at, first_source_series_id FROM release_event")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(event, (1_735_689_600_000, md));
        conn.close().await.unwrap();
        assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
    }

    #[tokio::test]
    async fn orphan_announce_clock_uses_its_stored_value() {
        let pool = orphan_fixture(false).await;
        sqlx::query("UPDATE release_event SET first_seen_at=1767225000000, announce_at=1791258000")
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 3);
        let clocks: (i64, i64) =
            sqlx::query_as("SELECT first_seen_at, announce_at FROM release_event")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(clocks, (1_767_225_000_000, 1_791_258_000_000));
        conn.close().await.unwrap();
        assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
    }

    #[tokio::test]
    async fn rejected_orphan_clocks_stay_unresolved_without_completion() {
        for value in [-62_135_596_800_001i64, 0, 99_999_999_999, 4_102_444_800_001] {
            let pool = orphan_fixture(false).await;
            sqlx::query("UPDATE release_event SET first_seen_at=$1, announce_at=$1")
                .bind(value)
                .execute(&pool)
                .await
                .unwrap();
            let mut conn = snapshot(&pool).await;
            assert_eq!(apply(&pool, &mut conn).await, 0);
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM release_event WHERE {BAD_EPOCH}"
            ))
            .fetch_one(&mut conn)
            .await
            .unwrap();
            assert_eq!(count, 1);
            assert!(!mark_complete(&pool).await.unwrap());
            let actual: i64 = sqlx::query_scalar("SELECT first_seen_at FROM release_event")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(actual, value);
            conn.close().await.unwrap();
            assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
        }
        // A valid sibling event must not make a work with unresolved orphan
        // clocks eligible for the broader rerun projection recovery.
        let pool = orphan_fixture(false).await;
        sqlx::query(
            "UPDATE release_event SET first_seen_at=1767225000000,announce_at=1767225000000",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO release_event(work_id,chapter_key,first_seen_at,label,announce_at) \
            SELECT work_id,'unresolved',4102444800001,'Unresolved',4102444800001 FROM release_event")
            .execute(&pool).await.unwrap();
        for table in ["feed_series_updates", "browse_catalogue"] {
            sqlx::query(&format!(
                "UPDATE {table} SET released_at=1767225000000,announced_at=1767225000000"
            ))
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 0);
        assert!(!mark_complete(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn historic_mangadex_winner_and_announcement_survive_modern_siblings() {
        for sibling in [false, true] {
            let pool = crate::db::SqliteDatabase::connect("sqlite::memory:")
                .await
                .unwrap();
            sqlx::migrate!("./migrations")
                .run(pool.write())
                .await
                .unwrap();
            let work =
                catalog::upsert_work_from_mangadex(&pool, "historic-md", &WorkInput::default())
                    .await
                    .unwrap();
            let historic =
                catalog::find_source_series_id(&pool, "mangadex", "mangadex", "historic-md")
                    .await
                    .unwrap()
                    .unwrap();
            catalog::upsert_chapter(
                &pool,
                &historic,
                &catalog::ChapterInput {
                    external_id: "historic".into(),
                    number: Some("1".into()),
                    lang: Some("en".into()),
                    readable_at: Some("1126-01-01T00:00:00+00:00".into()),
                    published_at: Some("1126-01-01T00:00:00+00:00".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            if sibling {
                let modern = catalog::upsert_source_series(
                    &pool,
                    &work,
                    "mangadex",
                    "",
                    "modern-md",
                    None,
                    false,
                )
                .await
                .unwrap();
                catalog::upsert_chapter(
                    &pool,
                    &modern,
                    &catalog::ChapterInput {
                        external_id: "modern".into(),
                        number: Some("1".into()),
                        lang: Some("en".into()),
                        readable_at: Some("2026-01-01T00:00:00+00:00".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            catalog::ledger::seed_work(&pool, &work, 1_791_500_000_000)
                .await
                .unwrap();
            let before: (i64, String, Option<i64>) = sqlx::query_as(
                "SELECT first_seen_at,first_source_series_id,announce_at FROM release_event",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            // Independent Gregorian-day calculation: 1126-01-01 is 308265
            // days before 1970-01-01; 308265 * 86400000 = 26634096000000.
            assert_eq!(
                before,
                (-26_634_096_000_000, historic, Some(-26_634_096_000_000))
            );
            sqlx::query("INSERT INTO feed_series_updates(work_id,reader_id,title,released_at,announced_at,latest_chapter) \
                VALUES($1,'historic','Historic',-26634096000000,-26634096000000,'1')")
                .bind(&work).execute(&pool).await.unwrap();
            sqlx::query("UPDATE browse_catalogue SET released_at=-26634096000000,announced_at=-26634096000000,latest_chapter='1'")
                .execute(&pool).await.unwrap();
            let mut conn = snapshot(&pool).await;
            assert_eq!(apply(&pool, &mut conn).await, 0);
            let after: (i64, String, Option<i64>) = sqlx::query_as(
                "SELECT first_seen_at,first_source_series_id,announce_at FROM release_event",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(after, before);
            assert!(mark_complete(&pool).await.unwrap());
        }
    }

    #[tokio::test]
    async fn signed_orphan_history_is_preserved_without_source_evidence() {
        for value in [-26_634_096_000_000i64, -1] {
            let pool = orphan_fixture(true).await;
            sqlx::query("UPDATE release_event SET first_seen_at=$1,announce_at=$1")
                .bind(value)
                .execute(&pool)
                .await
                .unwrap();
            let mut conn = snapshot(&pool).await;
            assert_eq!(apply(&pool, &mut conn).await, 2);
            let actual: (i64, i64) =
                sqlx::query_as("SELECT first_seen_at,announce_at FROM release_event")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(actual, (value, value));
            assert!(mark_complete(&pool).await.unwrap());
            conn.close().await.unwrap();
            assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
        }
    }

    async fn removed_event_label_fixture() -> crate::db::SqliteDatabase {
        let (pool, work, ss) = fixture("576861335057225").await;
        sqlx::query(
            "UPDATE release_event SET first_seen_at=576861335057000,announce_at=576861335057000",
        )
        .execute(&pool)
        .await
        .unwrap();
        let input =
            catalog::suwayomi_spine_input(10, "Chapter 1", 1.0, None, Some("1767225600000"));
        let (key, label) = catalog::spine_key_and_label(&input);
        assert_eq!(label, "1");
        catalog::upsert_chapter(&pool, &ss, &input).await.unwrap();
        sqlx::query("INSERT INTO suwayomi_chapter(id,manga_id,name,chapter_number,upload_date,page_count,updated_at) \
            VALUES(10,777,'Chapter 1',1,'1767225600000',0,'2026-01-01')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO release_event(work_id,chapter_key,first_seen_at,first_source_series_id,label,announce_at) \
            VALUES($1,$2,1767225600000,$3,$4,1767225600000)")
            .bind(&work).bind(&key).bind(&ss).bind(&label).execute(&pool).await.unwrap();
        // A different row wins the announcement aggregate. The display label
        // must still come from the chapter with the latest release date.
        sqlx::query(
            "INSERT INTO release_event(work_id,chapter_key,first_seen_at,label,announce_at) \
            VALUES($1,'announcement',1735689600000,'Announcement',1791028800000)",
        )
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
        for table in ["feed_series_updates", "browse_catalogue"] {
            sqlx::query(&format!("UPDATE {table} SET latest_chapter='Ch.9',released_at=576861335057000,announced_at=576861335057000"))
                .execute(&pool).await.unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn repaired_projection_label_follows_release_winner_after_deletion_and_interruption() {
        for interrupted in [false, true] {
            let pool = removed_event_label_fixture().await;
            let mut conn = snapshot(&pool).await;
            if interrupted {
                publish(&pool, &mut conn, "chapter", &["id"]).await.unwrap();
                publish(
                    &pool,
                    &mut conn,
                    "release_event",
                    &["work_id", "chapter_key"],
                )
                .await
                .unwrap();
                assert!(!mark_complete(&pool).await.unwrap());
                conn.close().await.unwrap();
                conn = snapshot(&pool).await;
            }
            apply(&pool, &mut conn).await;
            for table in ["feed_series_updates", "browse_catalogue"] {
                let actual: (String, i64, i64) = sqlx::query_as(&format!(
                    "SELECT latest_chapter,released_at,announced_at FROM {table}"
                ))
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(actual, ("1".into(), 1_767_225_600_000, 1_791_028_800_000));
            }
            assert!(mark_complete(&pool).await.unwrap());
            conn.close().await.unwrap();
            assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
        }
    }

    #[tokio::test]
    async fn label_only_projection_drift_is_repaired_and_concurrent_label_writes_win() {
        let pool = removed_event_label_fixture().await;
        let mut conn = snapshot(&pool).await;
        apply(&pool, &mut conn).await;
        conn.close().await.unwrap();
        for table in ["feed_series_updates", "browse_catalogue"] {
            sqlx::query(&format!("UPDATE {table} SET latest_chapter='Removed'"))
                .execute(&pool)
                .await
                .unwrap();
        }
        assert!(!mark_complete(&pool).await.unwrap());
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 2);
        assert!(mark_complete(&pool).await.unwrap());
        conn.close().await.unwrap();
        sqlx::query("UPDATE feed_series_updates SET latest_chapter='Removed'")
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = snapshot(&pool).await;
        sqlx::query("UPDATE feed_series_updates SET latest_chapter='Concurrent'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            publish(&pool, &mut conn, "feed_series_updates", &["work_id"])
                .await
                .unwrap(),
            0
        );
        let label: String = sqlx::query_scalar("SELECT latest_chapter FROM feed_series_updates")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(label, "Concurrent");
        assert!(!mark_complete(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn empty_ledger_projection_label_uses_source_then_mangadex_then_null() {
        for mode in 0..3 {
            let raw = if mode == 0 {
                "1767225600"
            } else {
                "999999999999"
            };
            let (pool, work, _) = fixture(raw).await;
            sqlx::query("DELETE FROM release_event")
                .execute(&pool)
                .await
                .unwrap();
            if mode == 1 {
                sqlx::query(
                    "INSERT INTO feed_updates(work_id,title,latest_at,latest_chapter) \
                    VALUES($1,'MD','2024-03-01T00:00:00Z','MD 3')",
                )
                .bind(&work)
                .execute(&pool)
                .await
                .unwrap();
            }
            for table in ["feed_series_updates", "browse_catalogue"] {
                sqlx::query(&format!("UPDATE {table} SET latest_chapter='Removed'"))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            let mut conn = snapshot(&pool).await;
            apply(&pool, &mut conn).await;
            let actual: (Option<String>, Option<i64>) =
                sqlx::query_as("SELECT latest_chapter,released_at FROM browse_catalogue")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            let expected = match mode {
                0 => (Some("9".into()), Some(1_767_225_600_000)),
                1 => (Some("MD 3".into()), Some(1_709_251_200_000)),
                _ => (None, None),
            };
            assert_eq!(actual, expected);
            assert!(mark_complete(&pool).await.unwrap());
            conn.close().await.unwrap();
            assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
        }
    }

    #[tokio::test]
    async fn completion_marker_checks_current_seconds_like_clocks() {
        let pool = orphan_fixture(false).await;
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 3);
        conn.close().await.unwrap();
        for (table, column) in [
            ("release_event", "first_seen_at"),
            ("feed_series_updates", "released_at"),
            ("browse_catalogue", "announced_at"),
        ] {
            sqlx::query(&format!("UPDATE {table} SET {column}=1767225000"))
                .execute(&pool)
                .await
                .unwrap();
            assert!(!mark_complete(&pool).await.unwrap());
            sqlx::query(&format!("UPDATE {table} SET {column}=1767225000000"))
                .execute(&pool)
                .await
                .unwrap();
        }
        assert!(mark_complete(&pool).await.unwrap());
        assert!(mark_complete(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn orphan_projection_repair_resumes_after_ledger_publication() {
        let pool = orphan_fixture(true).await;
        let mut conn = snapshot(&pool).await;
        assert_eq!(
            publish(
                &pool,
                &mut conn,
                "release_event",
                &["work_id", "chapter_key"]
            )
            .await
            .unwrap(),
            1
        );
        conn.close().await.unwrap();
        let mut conn = snapshot(&pool).await;
        assert_eq!(apply(&pool, &mut conn).await, 2);
        conn.close().await.unwrap();
        assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 0);
    }

    #[tokio::test]
    async fn missing_raw_rows_and_concurrent_writes_are_preserved() {
        let (pool, _, _) = fixture("1767225600").await;
        sqlx::query("DELETE FROM suwayomi_chapter")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(apply(&pool, &mut snapshot(&pool).await).await, 3);
        let (pool, _, _) = fixture("1767225600").await;
        let mut conn = snapshot(&pool).await;
        sqlx::query("UPDATE chapter SET title = 'Concurrent scan'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            publish(&pool, &mut conn, "chapter", &["id"]).await.unwrap(),
            0
        );
        let title: String = sqlx::query_scalar("SELECT title FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title, "Concurrent scan");
    }
}
