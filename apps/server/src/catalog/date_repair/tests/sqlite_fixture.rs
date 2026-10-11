//! Synthetic SQLite inputs for the retained pre-import date repair tests.
//! Application writer behavior is covered separately by native PostgreSQL tests.
pub use crate::catalog::{spine_key_and_label, ChapterInput, WorkInput};
use crate::db::SqliteDatabase as Database;
use anyhow::Result;
use chrono::Utc;
fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

pub async fn upsert_work_from_mangadex(
    pool: &Database,
    key: &str,
    input: &WorkInput,
) -> Result<String> {
    let id = new_id("w_");
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO work(id, primary_title, created_at, updated_at) VALUES($1,$2,$3,$4)")
        .bind(&id)
        .bind(&input.primary_title)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO work_external_id(work_id,provider,external_id) VALUES($1,'mangadex',$2)",
    )
    .bind(&id)
    .bind(key)
    .execute(pool)
    .await?;
    upsert_source_series(pool, &id, "mangadex", "mangadex", key, None, false).await?;
    Ok(id)
}
pub async fn upsert_source_series(
    pool: &Database,
    work: &str,
    ty: &str,
    source: &str,
    key: &str,
    url: Option<&str>,
    nsfw: bool,
) -> Result<String> {
    let id = new_id("ss_");
    sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,source_url,is_nsfw,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(&id).bind(work).bind(ty).bind(source).bind(key).bind(url).bind(nsfw as i64).bind(Utc::now().to_rfc3339()).execute(pool).await?;
    sqlx::query("INSERT INTO browse_catalogue(work_id,reader_id,title,created_at) VALUES($1,$2,'',$3) ON CONFLICT(work_id) DO UPDATE SET reader_id=excluded.reader_id")
        .bind(work).bind(key).bind(Utc::now().to_rfc3339()).execute(pool).await?;
    Ok(id)
}
pub async fn find_source_series_id(
    pool: &Database,
    ty: &str,
    source: &str,
    key: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM source_series WHERE source_type=$1 AND source_key=$3 AND source_id IS NOT DISTINCT FROM NULLIF($2,'')")
        .bind(ty).bind(source).bind(key).fetch_optional(pool).await?)
}
const CHAPTER_UPSERT_SQL: &str = "INSERT INTO chapter \
       (id, source_series_id, external_id, number, volume, lang, title, published_at, \
        readable_at, external_url, scanlator, chapter_key, label, created_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
     ON CONFLICT(source_series_id, external_id) DO UPDATE SET \
       number = excluded.number, volume = excluded.volume, lang = excluded.lang, \
       title = excluded.title, published_at = excluded.published_at, \
       readable_at = excluded.readable_at, external_url = excluded.external_url, \
       scanlator = excluded.scanlator, chapter_key = excluded.chapter_key, \
       label = excluded.label";

pub async fn upsert_chapter(
    pool: &Database,
    source_series_id: &str,
    ch: &ChapterInput,
) -> Result<()> {
    if ch.external_id.is_empty() {
        return Ok(());
    }
    let now = Utc::now().to_rfc3339();
    // Computed on the way in, never in SQL. `round(number * 100)` looks trivial enough to
    // inline into the statement, but the rule around it is not — the sanity clamp, the
    // name fallback and the `x:` namespace for unnumbered chapters all live in
    // `chapter_display`, and a SQL copy would be a fifth labelling rule that drifts.
    let (chapter_key, label) = spine_key_and_label(ch);
    sqlx::query(CHAPTER_UPSERT_SQL)
        .bind(new_id("ch_"))
        .bind(source_series_id)
        .bind(&ch.external_id)
        .bind(&ch.number)
        .bind(&ch.volume)
        .bind(&ch.lang)
        .bind(&ch.title)
        .bind(&ch.published_at)
        .bind(&ch.readable_at)
        .bind(&ch.external_url)
        .bind(&ch.scanlator)
        .bind(&chapter_key)
        .bind(&label)
        .bind(&now)
        .execute(pool)
        .await?;
    Ok(())
}

pub mod ledger {
    use super::*;
    const RELEASED_AT_SQL: &str = "COALESCE(c.readable_at, c.published_at)";
    const ANNOUNCE_AT_SQL: &str = "CASE      WHEN ss.created_at < COALESCE(c.readable_at, c.published_at)      THEN MAX(CAST(strftime('%s', COALESCE(c.readable_at, c.published_at)) AS INTEGER) * 1000, $1)      ELSE CAST(strftime('%s', COALESCE(c.readable_at, c.published_at)) AS INTEGER) * 1000      END";
    fn seedable_where(now_index: usize) -> String {
        let now_arg = crate::sql_params::placeholders(now_index, 1);
        format!(
            "c.chapter_key IS NOT NULL \
     AND {RELEASED_AT_SQL} IS NOT NULL \
     -- `strftime` returns NULL on anything it cannot parse. Dropping those rows is the
     -- only safe response: the column is NOT NULL, and the alternative to skipping an
     -- undatable chapter is inventing a date for it.
     AND strftime('%s', {RELEASED_AT_SQL}) IS NOT NULL \
     -- NOT YET RELEASED IS NOT FIRST-SEEN. MangaDex schedules chapters, and stamps
     -- external ones with a 2037 sentinel `publishAt` (236 rows today, every one of them a
     -- work missing from /updates entirely). Admitting them would put events in the
     -- ledger's future — the exact condition `assert_no_future_events` refuses to let a
     -- deploy finish with. They enter the ledger when they are actually readable: either
     -- the firehose re-offers them with a real `readableAt`, or the A1b backfill supplies
     -- one.
     AND CAST(strftime('%s', {RELEASED_AT_SQL}) AS INTEGER) * 1000 <= {now_arg}"
        )
    }
    pub(crate) async fn seed_work(pool: &Database, work_id: &str, now_ms: i64) -> Result<u64> {
        let n = sqlx::query(&format!(
            "INSERT OR IGNORE INTO release_event \
             (work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at) \
         SELECT ss.work_id, c.chapter_key, \
                MIN(CAST(strftime('%s', {RELEASED_AT_SQL}) AS INTEGER) * 1000), \
                c.source_series_id, \
                COALESCE(c.label, c.number, 'Oneshot'), \
                {ANNOUNCE_AT_SQL} \
           FROM source_series ss \
           JOIN chapter c ON c.source_series_id = ss.id \
          WHERE ss.work_id = $2 AND {} \
          GROUP BY c.chapter_key",
            seedable_where(3)
        ))
        // THREE BINDS, AND THE ORDER IS THE SQL TEXT ORDER, NOT THE ARGUMENT ORDER. The
        // `ANNOUNCE_AT_SQL` placeholder sits in the SELECT list, so it is the FIRST `?` in the
        // statement; `seedable_where(3)`'s bound is the last. Both are `now_ms`.
        .bind(now_ms)
        .bind(work_id)
        .bind(now_ms)
        .execute(pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    pub async fn seed_batch(pool: &Database) -> Result<u64> {
        let now = Utc::now().timestamp_millis();
        let works: Vec<String> = sqlx::query_scalar(&format!("SELECT DISTINCT ss.work_id FROM source_series ss JOIN chapter c ON c.source_series_id=ss.id WHERE {} AND NOT EXISTS (SELECT 1 FROM release_event re WHERE re.work_id=ss.work_id AND re.chapter_key=c.chapter_key)", seedable_where(1)))
            .bind(now).fetch_all(pool).await?;
        for work in &works {
            seed_work(pool, work, now).await?;
        }
        Ok(works.len() as u64)
    }
}

use super::super::suwayomi_upload_date_to_iso;
pub fn suwayomi_spine_input(
    id: i64,
    name: &str,
    chapter_number: f64,
    scanlator: Option<&str>,
    upload_date: Option<&str>,
) -> ChapterInput {
    let released = suwayomi_upload_date_to_iso(upload_date);
    ChapterInput {
        external_id: id.to_string(),
        number: Some(chapter_number.to_string()),
        volume: None,
        // `series_cache` is English-only by construction — `put_series` refuses to cache a
        // non-English series and `put_chapters` refuses chapters for an uncached one — so
        // every row that can reach here is English. The unified query filters on this.
        lang: Some("en".into()),
        title: Some(name.to_string()),
        published_at: released.clone(),
        readable_at: released,
        // Suwayomi serves pages for everything it lists; an off-site chapter is a MangaDex
        // concept (F1).
        external_url: None,
        scanlator: scanlator.map(str::to_string),
    }
}
