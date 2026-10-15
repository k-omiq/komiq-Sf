//! WAL drill for the no-op write paths (M4 item 6). Ignored; it runs only against a
//! scratch copy of a real database named by `KOMIKA_WAL_DRILL_URL`, never a live one.
//!
//! Each phase repeats one write path with data the database already holds, so a
//! correct no-op guard writes nothing. A phase runs its path twice: the first pass
//! settles whatever the reconstructed input differs in (reported, not compared), the
//! second pass is the measurement. Before each pass: `CHECKPOINT`, so every pass pays
//! its full-page images the same way. After each pass the phase's pool is closed, so
//! its backends exit and flush their statistics, and a fresh connection reads
//! `pg_stat_wal` and `pg_stat_user_tables`. The cluster must be otherwise idle (stop
//! the server); the connecting role needs `pg_checkpoint`.
//!
//! Environment: `KOMIKA_WAL_DRILL_URL` (required), `KOMIKA_WAL_DRILL_N` (rows per phase,
//! default 200), `KOMIKA_WAL_DRILL_PHASES` (comma list, default all),
//! `KOMIKA_WAL_DRILL_OUT` (JSON result path).

use crate::database::Database;
use serde_json::{json, Value};
use sqlx::{Connection, Row};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

const PHASES: &[&str] = &[
    "mangadex_upsert",
    "mangadex_details",
    "scan",
    "spine_chapters",
    "feed_projection",
    "work_stats",
    "work_stats_drift",
];

#[derive(Default, Clone)]
struct Snapshot {
    wal_bytes: i64,
    wal_fpi: i64,
    wal_records: i64,
    /// relname → (n_tup_upd, n_tup_hot_upd, n_tup_ins, n_tup_del)
    tables: BTreeMap<String, [i64; 4]>,
}

async fn snapshot(url: &str) -> Snapshot {
    let mut c = sqlx::PgConnection::connect(url).await.expect("drill connect");
    let wal = sqlx::query(
        "SELECT wal_bytes::bigint, wal_fpi::bigint, wal_records::bigint FROM pg_stat_wal",
    )
    .fetch_one(&mut c)
    .await
    .expect("pg_stat_wal");
    let mut tables = BTreeMap::new();
    for row in sqlx::query(
        "SELECT relname::text, n_tup_upd, n_tup_hot_upd, n_tup_ins, n_tup_del \
         FROM pg_stat_user_tables WHERE schemaname = 'public'",
    )
    .fetch_all(&mut c)
    .await
    .expect("pg_stat_user_tables")
    {
        tables.insert(
            row.get::<String, _>(0),
            [row.get(1), row.get(2), row.get(3), row.get(4)],
        );
    }
    c.close().await.ok();
    Snapshot {
        wal_bytes: wal.get(0),
        wal_fpi: wal.get(1),
        wal_records: wal.get(2),
        tables,
    }
}

async fn checkpoint(url: &str) {
    let mut c = sqlx::PgConnection::connect(url).await.expect("drill connect");
    sqlx::query("CHECKPOINT")
        .execute(&mut c)
        .await
        .expect("CHECKPOINT (grant pg_checkpoint to the drill role)");
    c.close().await.ok();
}

fn delta(a: &Snapshot, b: &Snapshot) -> Value {
    let mut tables = serde_json::Map::new();
    for (name, after) in &b.tables {
        let before = a.tables.get(name).copied().unwrap_or_default();
        let d: Vec<i64> = (0..4).map(|i| after[i] - before[i]).collect();
        if d.iter().any(|v| *v != 0) {
            tables.insert(
                name.clone(),
                json!({"upd": d[0], "hot_upd": d[1], "ins": d[2], "del": d[3]}),
            );
        }
    }
    json!({
        "wal_bytes": b.wal_bytes - a.wal_bytes,
        "wal_fpi": b.wal_fpi - a.wal_fpi,
        "wal_records": b.wal_records - a.wal_records,
        "tables": tables,
    })
}

/// One measured pass: checkpoint, snapshot, run, close the pool, snapshot.
async fn pass<F, Fut>(url: &str, run: F) -> Value
where
    F: FnOnce(Database) -> Fut,
    Fut: std::future::Future<Output = u64>,
{
    checkpoint(url).await;
    let before = snapshot(url).await;
    let pool = Database::connect_with(
        sqlx::postgres::PgConnectOptions::from_str(url).expect("drill url"),
        4,
    )
    .await
    .expect("drill pool");
    let started = std::time::Instant::now();
    let calls = run(pool.clone()).await;
    let seconds = started.elapsed().as_secs_f64();
    pool.close().await;
    // Backend exit flushes its pending statistics; give the exits a moment.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let after = snapshot(url).await;
    let mut out = delta(&before, &after);
    out["calls"] = json!(calls);
    out["seconds"] = json!(seconds);
    out
}

async fn ids(url: &str, sql: &str, n: i64) -> Vec<Vec<String>> {
    let mut c = sqlx::PgConnection::connect(url).await.expect("drill connect");
    let rows = sqlx::query(sql)
        .bind(n)
        .fetch_all(&mut c)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    c.close().await.ok();
    rows.iter()
        .map(|r| (0..r.len()).map(|i| r.get::<String, _>(i)).collect())
        .collect()
}

/// The MangaDex input a sweep would send for a work, rebuilt from what the database
/// holds (pinned fields from the pin's remembered source value).
async fn work_input(pool: &Database, work_id: &str) -> crate::catalog::WorkInput {
    use crate::catalog::{Alias, Cover, SourceTag, WorkInput};
    let w = sqlx::query(
        "SELECT primary_title, primary_lang, description, year, original_language, status, \
                demographic, content_rating, is_nsfw, author, artist, cover_phash, cover_file_name \
         FROM work WHERE id = $1",
    )
    .bind(work_id)
    .fetch_one(pool)
    .await
    .expect("work row");
    let pins: HashMap<String, Option<String>> =
        sqlx::query("SELECT field, source_value FROM work_field_pin WHERE work_id = $1")
            .bind(work_id)
            .fetch_all(pool)
            .await
            .expect("pins")
            .iter()
            .map(|r| (r.get::<String, _>(0), r.get::<Option<String>, _>(1)))
            .collect();
    let text = |col: &str| -> Option<String> {
        match pins.get(col) {
            Some(source) => source.clone(),
            None => w.get::<Option<String>, _>(col),
        }
    };
    let year = match pins.get("year") {
        Some(source) => source.as_deref().and_then(|v| v.parse().ok()),
        None => w.get::<Option<i64>, _>("year"),
    };
    let pairs = |sql: &'static str| async move {
        sqlx::query(sql)
            .bind(work_id)
            .fetch_all(pool)
            .await
            .expect(sql)
            .iter()
            .map(|r| (r.get::<String, _>(0), r.get::<String, _>(1)))
            .collect::<Vec<_>>()
    };
    let aliases = sqlx::query(
        "SELECT raw_title, lang FROM work_alias WHERE work_id = $1 ORDER BY raw_title, lang",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await
    .expect("aliases")
    .iter()
    .map(|r| Alias {
        raw: r.get(0),
        lang: r.get(1),
    })
    .collect();
    let tag_rows = sqlx::query(
        "SELECT tag, tag_id, tag_group FROM work_tag \
         WHERE work_id = $1 AND source = 'mangadex' ORDER BY ord, tag",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await
    .expect("tags");
    let covers = sqlx::query(
        "SELECT cover_file_name, lang, volume, is_primary FROM work_cover WHERE work_id = $1 \
         ORDER BY cover_file_name",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await
    .expect("covers")
    .iter()
    .map(|r| Cover {
        file_name: r.get(0),
        lang: r.get(1),
        volume: r.get(2),
        is_primary: r.get(3),
    })
    .collect();
    WorkInput {
        primary_title: w.get("primary_title"),
        primary_lang: w.get("primary_lang"),
        description: w.get("description"),
        year,
        original_language: text("original_language"),
        status: text("status"),
        demographic: text("demographic"),
        content_rating: text("content_rating"),
        is_nsfw: w.get("is_nsfw"),
        author: text("author"),
        artist: text("artist"),
        cover_phash: w.get("cover_phash"),
        cover_file_name: w.get("cover_file_name"),
        aliases,
        external_ids: pairs(
            "SELECT provider, external_id FROM work_tracker_link WHERE work_id = $1 ORDER BY provider",
        )
        .await,
        official_links: Vec::new(),
        descriptions: pairs(
            "SELECT lang, description FROM work_description WHERE work_id = $1 ORDER BY lang",
        )
        .await,
        credits: pairs(
            "SELECT role, name FROM work_credit WHERE work_id = $1 ORDER BY role, name",
        )
        .await,
        tags: tag_rows.iter().map(|r| r.get::<String, _>(0)).collect(),
        tag_details: tag_rows
            .iter()
            .filter_map(|r| {
                Some(SourceTag {
                    id: r.get(1),
                    name: r.get(0),
                    group: r.get::<Option<String>, _>(2)?,
                })
            })
            .collect(),
        covers,
    }
}

async fn run_phase(url: &str, phase: &str, n: i64) -> Value {
    match phase {
        "mangadex_upsert" | "mangadex_details" => {
            let works = ids(
                url,
                "SELECT ss.work_id, ss.source_key FROM source_series ss \
                 WHERE ss.source_type = 'mangadex' ORDER BY ss.work_id LIMIT $1",
                n,
            )
            .await;
            let details = phase == "mangadex_details";
            let mut passes = Vec::new();
            for _ in 0..2 {
                let works = works.clone();
                passes.push(
                    pass(url, |pool| async move {
                        for w in &works {
                            let input = work_input(&pool, &w[0]).await;
                            if details {
                                crate::catalog::refresh_mangadex_details(&pool, &w[0], &input)
                                    .await
                                    .expect("details");
                            } else {
                                crate::catalog::upsert_work_from_mangadex(&pool, &w[1], &input)
                                    .await
                                    .expect("upsert");
                            }
                        }
                        works.len() as u64
                    })
                    .await,
                );
            }
            json!({"settle": passes[0], "repeat": passes[1]})
        }
        "scan" | "spine_chapters" => {
            let series = ids(
                url,
                "SELECT s.id::text FROM suwayomi_series s \
                 WHERE EXISTS (SELECT 1 FROM suwayomi_chapter c WHERE c.manga_id = s.id) \
                   AND (s.lang IS NULL OR s.lang = 'en') ORDER BY s.id LIMIT $1",
                n,
            )
            .await;
            let series: Vec<i64> = series.iter().map(|r| r[0].parse().unwrap()).collect();
            let spine = phase == "spine_chapters";
            let mut passes = Vec::new();
            for i in 0..2 {
                let series = series.clone();
                passes.push(
                    pass(url, |pool| async move {
                        let mangas = crate::series_cache::get_series_fresh_batch(&pool, &series)
                            .await
                            .expect("series");
                        let chapters =
                            crate::series_cache::get_chapters_fresh_batch(&pool, &series)
                                .await
                                .expect("chapters");
                        let now = chrono::Utc::now() + chrono::Duration::minutes(i);
                        let mut calls = 0;
                        for id in &series {
                            let (Some((m, _)), Some((list, _))) = (mangas.get(id), chapters.get(id))
                            else {
                                continue;
                            };
                            calls += 1;
                            // A real scan fetches with `fetchManga: true`, and Suwayomi stamps
                            // `lastFetchedAt` to the fetch time (epoch seconds).
                            let mut m = m.clone();
                            m.last_fetched_at = Some(now.timestamp().to_string());
                            if spine {
                                let ssid: Option<String> = sqlx::query_scalar(
                                    "SELECT id FROM source_series \
                                     WHERE source_type = 'suwayomi' AND source_key = $1",
                                )
                                .bind(id.to_string())
                                .fetch_optional(&pool)
                                .await
                                .expect("source series");
                                let Some(ssid) = ssid else { continue };
                                let inputs: Vec<_> = list
                                    .iter()
                                    .map(crate::catalog::suwayomi_chapter_input)
                                    .collect();
                                crate::catalog::replace_source_chapters(&pool, &ssid, &inputs)
                                    .await
                                    .expect("spine");
                            } else {
                                crate::scanner::persist_scan_for_drill(&pool, &m, list, now)
                                    .await
                                    .expect("scan");
                            }
                        }
                        pool.flush_deferred().await;
                        calls
                    })
                    .await,
                );
            }
            json!({"settle": passes[0], "repeat": passes[1]})
        }
        "feed_projection" => {
            let works = ids(
                url,
                "SELECT work_id FROM feed_series_updates ORDER BY work_id LIMIT $1",
                n,
            )
            .await;
            let mut passes = Vec::new();
            for _ in 0..2 {
                let works = works.clone();
                passes.push(
                    pass(url, |pool| async move {
                        for w in &works {
                            let mut tx = pool.begin().await.expect("begin");
                            crate::catalog::fill_en_chapter_count(&mut tx, Some(&w[0]))
                                .await
                                .expect("count");
                            crate::catalog::project_feed_from_ledger(&mut tx, Some(&w[0]))
                                .await
                                .expect("ledger");
                            tx.commit().await.expect("commit");
                        }
                        works.len() as u64
                    })
                    .await,
                );
            }
            json!({"settle": passes[0], "repeat": passes[1]})
        }
        "work_stats" | "work_stats_drift" => {
            let rows = ids(
                url,
                "SELECT s.work_id, COALESCE(s.follows::text, ''), COALESCE(s.rating::text, ''), \
                        COALESCE(s.rating_avg::text, '') \
                 FROM work_stats s JOIN browse_catalogue b ON b.work_id = s.work_id \
                 WHERE s.rating IS NOT NULL ORDER BY s.work_id LIMIT $1",
                n,
            )
            .await;
            let drift = phase == "work_stats_drift";
            let mut passes = Vec::new();
            for i in 0..2 {
                let rows = rows.clone();
                passes.push(
                    pass(url, |pool| async move {
                        let mut pending = Vec::new();
                        let mut stats = HashMap::new();
                        for r in &rows {
                            let md = format!("drill-{}", r[0]);
                            let rating = r[2].parse::<f64>().ok().map(|v| {
                                // The second pass of the drift phase moves the rating in
                                // the ninth decimal: a value no reader can see.
                                if drift && i == 1 {
                                    v + 1e-9
                                } else {
                                    v
                                }
                            });
                            pending.push(crate::work_stats::PendingStats {
                                work_id: r[0].clone(),
                                mangadex_id: Some(md.clone()),
                            });
                            stats.insert(
                                md,
                                crate::work_stats::MdStats {
                                    follows: r[1].parse().ok(),
                                    rating,
                                    rating_avg: r[3].parse().ok(),
                                },
                            );
                        }
                        let now = (chrono::Utc::now() + chrono::Duration::minutes(i)).to_rfc3339();
                        for chunk in pending.chunks(100) {
                            crate::work_stats::write_work_stats(&pool, chunk, &stats, &now)
                                .await
                                .expect("stats");
                        }
                        rows.len() as u64
                    })
                    .await,
                );
            }
            json!({"settle": passes[0], "repeat": passes[1]})
        }
        other => panic!("unknown drill phase {other}"),
    }
}

#[tokio::test]
#[ignore = "drill: needs KOMIKA_WAL_DRILL_URL pointing at a scratch copy"]
async fn wal_drill() {
    let url = std::env::var("KOMIKA_WAL_DRILL_URL").expect("KOMIKA_WAL_DRILL_URL");
    let n: i64 = std::env::var("KOMIKA_WAL_DRILL_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let wanted = std::env::var("KOMIKA_WAL_DRILL_PHASES").unwrap_or_default();
    let mut out = serde_json::Map::new();
    for phase in PHASES {
        if !wanted.is_empty() && !wanted.split(',').any(|p| p == *phase) {
            continue;
        }
        let result = run_phase(&url, phase, n).await;
        println!("{phase}: {result}");
        out.insert(phase.to_string(), result);
    }
    if let Ok(path) = std::env::var("KOMIKA_WAL_DRILL_OUT") {
        std::fs::write(path, serde_json::to_string_pretty(&Value::Object(out)).unwrap())
            .expect("write drill result");
    }
}
