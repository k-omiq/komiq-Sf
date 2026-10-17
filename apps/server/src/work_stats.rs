//! The MangaDex statistics drain: follows + rating for every MangaDex-anchored work, into
//! `work_stats` (migration 0106), for Browse v2's MOST_FOLLOWS / HIGHEST_RATED sorts and its
//! `minFollows` / `minRating` filters.
//!
//! WHY A DRAIN. The `/manga` payloads the catalogue sweep already fetches carry no
//! statistics; MangaDex serves them from a separate endpoint, `GET /statistics/manga`, 100 ids
//! per call. ~115k anchored works is ~1,150 requests — too many to fire at once on an IP that
//! shares a ~5 req/s budget with the firehose, the catalogue sweep, the cover crawler and the
//! chapter drains, and too few to need anything cleverer than the drains already in
//! `mangadex` (A1b's `spawn_readable_backfill`, the scanlation-group backfill). This is
//! modelled on them exactly: a paced batch loop, an interruptible nap between batches, an
//! idle re-check instead of an exit, sparse progress logging.
//!
//! CURSORLESS AND RESUMABLE. The work-list is a query, not a position: a MangaDex-anchored
//! work with NO `work_stats` row (never fetched — first, so a fresh deploy fills the
//! catalogue before refreshing anything), then rows whose `fetched_at` is older than
//! [`STATS_STALE_AFTER`] (stalest first). Every pending work gets a row written with
//! `fetched_at = now` — answered or not — so every batch takes its works OFF the list, which
//! is the termination argument; and a restart simply resumes from whatever is still on it.
//!
//! FAILURE MODE. A failed request writes NOTHING: the batch stays on the work-list and the
//! loop backs off to the idle interval. Writing "fetched, no data" over a transient 503 would
//! take ~100 works off the list for a week.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::db::Database;
use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::mangadex::MangaDexClient;

/// Ids per `/statistics/manga` request — MangaDex's `manga[]` ceiling. One batch is one
/// request, as with the chapter drains (`CHAPTER_IDS_LIMIT`).
pub const STATS_IDS_LIMIT: usize = 100;

/// Gap between batches. 2 s apart is 0.5 req/s against the ~5 req/s per-IP ceiling — the
/// same budget the A1b and scanlation-group drains take, for the same reasons: it leaves
/// ~90% of the IP for the firehose and the sweep, and ~50 row writes/s on the single SQLite
/// writer. ~115k works ÷ 100 ≈ 1,150 requests ≈ 40-50 minutes for a full backfill (2 s gap
/// plus request latency), then the same again once a week as rows go stale.
const STATS_BACKFILL_GAP: Duration = Duration::from_secs(2);

/// Staggered behind every other boot-time drain (45 s catalogue backfill, 90 s spine, 120 s
/// A1b, 150 s groups) so a restart never starts two of them against one writer at once.
const STATS_BACKFILL_BOOT_DELAY: Duration = Duration::from_secs(180);

/// Re-check interval once the work-list is empty, and the back-off after a failed batch.
/// Nothing here is urgent — a new work simply joins the "never fetched" head of the list —
/// and the idle probe is two indexed LIMIT queries.
const STATS_BACKFILL_IDLE_RECHECK: Duration = Duration::from_secs(60 * 60);

/// How old a row may get before it is re-fetched. Follow counts move slowly and ratings
/// slower; weekly keeps a sort by follows honest without spending the IP budget on churn.
pub const STATS_STALE_AFTER: chrono::Duration = chrono::Duration::days(7);

/// Batches between progress lines: ~1,150 batches → ~6 lines per full pass.
const STATS_BACKFILL_PROGRESS_EVERY: u64 = 200;

/// One manga's statistics, as parsed from the response.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MdStats {
    pub follows: Option<i64>,
    /// `rating.bayesian` — what MangaDex sorts "rating" by. `None` for an unrated title
    /// (MangaDex reports a 0 bayesian there, which is "no data", not "rated zero").
    pub rating: Option<f64>,
    /// `rating.average` — the plain mean, `null` upstream for an unrated title.
    pub rating_avg: Option<f64>,
}

/// Parse a `/statistics/manga` response body into `uuid -> stats`.
///
/// Shape (MangaDex API 5.x):
/// `{"result":"ok","statistics":{"<uuid>":{"follows":123,"rating":{"average":7.9,
/// "bayesian":7.8,"distribution":{…}},"comments":…}}}`.
///
/// Strict where a wrong answer would be written as truth — `result` must be `"ok"` and
/// `statistics` must be present — and lenient per record, where one odd entry must not cost
/// the other 99: a missing/`null` field is `None`, an entry that is not an object is skipped
/// (its work is then "unanswered" and keeps its old values). An EMPTY `statistics` may arrive
/// as `[]` (a PHP-serialized empty map), which is read as "no entries", not as an error.
pub fn parse_statistics(body: &Value) -> Result<HashMap<String, MdStats>> {
    match body.get("result").and_then(Value::as_str) {
        Some("ok") => {}
        other => return Err(anyhow!("MangaDex /statistics/manga: result {other:?}")),
    }
    let stats = body
        .get("statistics")
        .ok_or_else(|| anyhow!("MangaDex /statistics/manga: no `statistics`"))?;
    let map = match stats {
        Value::Object(m) => m,
        Value::Array(a) if a.is_empty() => return Ok(HashMap::new()),
        _ => {
            return Err(anyhow!(
                "MangaDex /statistics/manga: `statistics` is not a map"
            ))
        }
    };
    let finite = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|x| x.is_finite());
    let mut out = HashMap::with_capacity(map.len());
    for (id, entry) in map {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let rating = entry.get("rating").and_then(Value::as_object);
        out.insert(
            id.to_ascii_lowercase(),
            MdStats {
                follows: entry
                    .get("follows")
                    .and_then(Value::as_i64)
                    .filter(|n| *n >= 0),
                rating: finite(rating.and_then(|r| r.get("bayesian"))).filter(|x| *x > 0.0),
                rating_avg: finite(rating.and_then(|r| r.get("average"))),
            },
        );
    }
    Ok(out)
}

/// One work on the statistics work-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStats {
    pub work_id: String,
    /// The MangaDex uuid to ask about, lowercased; `None` when the work has no (usable)
    /// MangaDex anchor any more — it is still written (fetched, unanswered) so it leaves the
    /// list.
    pub mangadex_id: Option<String>,
}

/// The never-fetched half of the work-list. ORDER BY `created_at` ALONE, deliberately: it is
/// exactly the index's second column, so the walk is the index order and LIMIT stops it.
/// Adding a tiebreak (`ss.id`) would turn it into a partial sort per equal `created_at` —
/// and the catalogue seed stamped ~100k anchors with near-identical times. A drain needs no
/// deterministic order among ties, only a terminating one. Pinned by
/// `the_stats_worklist_queries_are_index_walks`.
///
/// The PostgreSQL plan must remain a LIMIT-bounded index walk. The PostgreSQL
/// planner regression checks the real plan after ANALYZE on a populated fixture.
/// OFFSET 0 keeps the primary-key existence check correlated: otherwise PostgreSQL
/// can turn it into a hash anti-join and sort every anchor before applying LIMIT.
const NEVER_FETCHED_SQL: &str = "SELECT ss.work_id, ss.source_key \
       FROM source_series ss \
      WHERE ss.source_type = 'mangadex' \
        AND NOT EXISTS (SELECT 1 FROM work_stats s WHERE s.work_id = ss.work_id OFFSET 0) \
      ORDER BY ss.created_at NULLS FIRST LIMIT $1";

/// The stale half: `idx_work_stats_fetched` walked from the oldest row; the anchor per work is
/// a `source_series(work_id)` probe (oldest anchor, matching the never-fetched half).
const STALE_SQL: &str = "SELECT s.work_id, \
            (SELECT ss.source_key FROM source_series ss \
              WHERE ss.work_id = s.work_id AND ss.source_type = 'mangadex' \
              ORDER BY ss.created_at NULLS FIRST, ss.id LIMIT 1) \
       FROM work_stats s WHERE s.fetched_at < $1 \
      ORDER BY s.fetched_at NULLS FIRST LIMIT $2";

/// The next `limit` works to fetch: never-fetched first, then the stalest past `stale_before`.
///
/// PLANS. The never-fetched half drives from `source_series` through
/// `idx_source_series_type_created (source_type, created_at)` — oldest anchors first — with a
/// primary-key anti-probe into `work_stats` per row, and LIMIT short-circuits it. Its cost
/// grows with the number of works already fetched that the walk has to step over (a full
/// pass at the end of the backfill is ~115k index steps, a few ms), which is the price of
/// having no cursor. The stale half is an index walk of `idx_work_stats_fetched` from the
/// oldest row, also LIMIT-bounded.
///
/// A work with two MangaDex sources appears once (deduped here, keeping its oldest anchor).
pub async fn works_needing_stats(
    pool: &Database,
    limit: i64,
    stale_before: &str,
) -> Result<Vec<PendingStats>> {
    let mut out: Vec<PendingStats> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Asked for 2x: a duplicate-anchored work costs one extra row, and the dedupe below must
    // still be able to fill the batch.
    let never: Vec<(String, String)> = crate::db_types::query_tuple(NEVER_FETCHED_SQL)
        .bind(limit * 2)
        .fetch_all(pool)
        .await?;
    for (work_id, key) in never {
        if out.len() as i64 >= limit {
            break;
        }
        if seen.insert(work_id.clone()) {
            out.push(PendingStats {
                work_id,
                mangadex_id: mangadex_uuid(&key),
            });
        }
    }
    if (out.len() as i64) < limit {
        let stale: Vec<(String, Option<String>)> = crate::db_types::query_tuple(STALE_SQL)
            .bind(crate::db_types::date(stale_before)?)
            .bind(limit)
            .fetch_all(pool)
            .await?;
        for (work_id, key) in stale {
            if out.len() as i64 >= limit {
                break;
            }
            if seen.insert(work_id.clone()) {
                out.push(PendingStats {
                    work_id,
                    mangadex_id: key.as_deref().and_then(mangadex_uuid),
                });
            }
        }
    }
    Ok(out)
}

/// A MangaDex `source_key` as the uuid the statistics endpoint takes, or `None` if it is not
/// one (sending garbage would fail the WHOLE request — 400 — and so the whole batch).
fn mangadex_uuid(key: &str) -> Option<String> {
    uuid::Uuid::parse_str(key.trim())
        .ok()
        .map(|u| u.to_string())
}

/// What one batch moved. `fetched == 0` is the only completion signal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatsFill {
    /// Works taken off the work-list — the batch size, by construction.
    pub fetched: u64,
    /// …of which upstream answered.
    pub answered: u64,
    /// …of which upstream did not (or the work had no usable anchor). They keep their
    /// previous values and get a fresh `fetched_at`, so they are retried in a week, not in
    /// two seconds — a deleted manga must not pin the drain.
    pub unanswered: u64,
}

/// Write one batch, in one transaction. Client-free so ordering, termination and the
/// unanswered rule are testable without touching MangaDex.
///
/// Also copies the new values onto `browse_catalogue`, so a sort by follows reflects the
/// drain within one memo TTL rather than at the next twice-daily rebuild. That copy is the
/// rebuild's own rule (`browse_catalogue.follows/rating` = the `work_stats` row, see
/// `catalog::browse_catalogue_select_for`), so the two writers converge.
pub async fn write_work_stats(
    pool: &Database,
    pending: &[PendingStats],
    stats: &HashMap<String, MdStats>,
    now: &str,
) -> Result<StatsFill> {
    crate::db::retry_transaction(|| write_work_stats_once(pool, pending, stats, now)).await
}

async fn write_work_stats_once(
    pool: &Database,
    pending: &[PendingStats],
    stats: &HashMap<String, MdStats>,
    now: &str,
) -> Result<StatsFill> {
    let mut out = StatsFill::default();
    let mut tx = pool.begin().await?;
    let parents: Vec<_> = pending.iter().map(|p| &p.work_id).collect();
    let locked = crate::locks::lock_works(&mut tx, &parents).await?;
    #[cfg(test)]
    crate::catalog::guards::held().await;
    let mut pending: Vec<_> = pending.iter().collect();
    pending.sort_by(|a, b| a.work_id.cmp(&b.work_id));
    for p in pending {
        if !locked.locked.contains(&p.work_id) {
            out.fetched += 1;
            out.unanswered += 1;
            continue;
        }
        let answer = p.mangadex_id.as_ref().and_then(|id| stats.get(id));
        match answer {
            Some(s) => {
                out.answered += 1;
                sqlx::query(
                    "INSERT INTO work_stats (work_id, follows, rating, rating_avg, fetched_at) \
                     VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT(work_id) DO UPDATE SET follows = excluded.follows, \
                         rating = excluded.rating, rating_avg = excluded.rating_avg, \
                         fetched_at = excluded.fetched_at",
                )
                .bind(&p.work_id)
                .bind(s.follows)
                .bind(s.rating.map(stored_rating))
                .bind(s.rating_avg.map(stored_rating))
                .bind(crate::db_types::date(now)?)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                out.unanswered += 1;
                // Keep whatever we knew; only the clock moves.
                sqlx::query(
                    "INSERT INTO work_stats (work_id, fetched_at) VALUES ($1, $2) \
                     ON CONFLICT(work_id) DO UPDATE SET fetched_at = excluded.fetched_at",
                )
                .bind(&p.work_id)
                .bind(crate::db_types::date(now)?)
                .execute(&mut *tx)
                .await?;
            }
        }
        // `IS NOT`, not `<>`, for the refresh's reason: both columns are nullable.
        sqlx::query(
            "UPDATE browse_catalogue SET follows = s.follows, rating = s.rating \
               FROM work_stats s \
              WHERE s.work_id = browse_catalogue.work_id AND browse_catalogue.work_id = $1 \
                AND (browse_catalogue.follows IS DISTINCT FROM s.follows \
                     OR browse_catalogue.rating IS DISTINCT FROM s.rating)",
        )
        .bind(&p.work_id)
        .execute(&mut *tx)
        .await?;
        out.fetched += 1;
    }
    tx.commit().await?;
    Ok(out)
}

/// A rating as stored: four decimals (WAL item 6). The reader shows one, and the Highest
/// Rated sort keeps its order to within 0.0001. Without the rounding, a drift in the low
/// decimals would count as a change and rewrite the work's `browse_catalogue` row, which
/// carries 25 indexes.
fn stored_rating(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// `fetched_at` for "now", and the stale cutoff for it. Seconds-precision RFC 3339 with a
/// `Z`, so every value this module writes has the SAME shape and text order is time order.
fn stamps(now: chrono::DateTime<chrono::Utc>) -> (String, String) {
    let fmt =
        |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    (fmt(now), fmt(now - STATS_STALE_AFTER))
}

/// Works still on the list (never fetched, or stale), for the completion line.
async fn stats_remaining(pool: &Database, stale_before: &str) -> Result<i64> {
    Ok(crate::db_types::query_scalar(
        "SELECT (SELECT COUNT(DISTINCT ss.work_id) FROM source_series ss \
                  WHERE ss.source_type = 'mangadex' \
                    AND NOT EXISTS (SELECT 1 FROM work_stats s WHERE s.work_id = ss.work_id)) \
              + (SELECT COUNT(*) FROM work_stats WHERE fetched_at < $1)",
    )
    .bind(crate::db_types::date(stale_before)?)
    .fetch_one(pool)
    .await?)
}

/// One batch of the statistics drain. `fetched == 0` means nothing is due.
pub async fn backfill_work_stats(pool: &Database, client: &MangaDexClient) -> Result<StatsFill> {
    let (now, stale_before) = stamps(chrono::Utc::now());
    let pending = works_needing_stats(pool, STATS_IDS_LIMIT as i64, &stale_before).await?;
    if pending.is_empty() {
        return Ok(StatsFill::default());
    }
    let mut ids: Vec<String> = pending
        .iter()
        .filter_map(|p| p.mangadex_id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    // Nothing written on failure — see the module doc.
    let stats = client.get_statistics(&ids).await?;
    write_work_stats(pool, &pending, &stats, &now).await
}

/// Sleep unless shutdown fires first; `true` => stop. A dropped sender counts as shutdown —
/// treating `changed()`'s instant `Err` as "keep going" would collapse every gap to zero and
/// fire the drain at the token bucket's full rate (the mistake `spawn_readable_backfill`
/// documents; this is its local copy, as that function keeps its own).
async fn nap(d: Duration, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        r = shutdown.changed() => r.is_err() || *shutdown.borrow(),
    }
}

/// Run the statistics drain in the background forever: a full pass, then idle re-checks
/// that pick up new works and weekly-stale rows. Structured exactly like
/// `mangadex::spawn_group_backfill`. Waits on the MangaDex switch between batches (it
/// shares the in-process MangaDex limiter, so the one-replica rule applies to it verbatim).
pub fn spawn_stats_backfill(
    pool: Database,
    client: Arc<MangaDexClient>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let pass = async move {
        if nap(STATS_BACKFILL_BOOT_DELAY, &mut shutdown).await {
            return;
        }
        let mut total = StatsFill::default();
        let mut batches = 0u64;
        let mut announced = false;
        loop {
            if *shutdown.borrow() {
                tracing::info!(
                    batches,
                    fetched = total.fetched,
                    "stats backfill: shutting down mid-drain — the work-list resumes it"
                );
                return;
            }
            // Parked while MangaDex is off (`mangadex` module doc); resumes on the flip.
            if client.wait_enabled("stats backfill", &mut shutdown).await {
                return;
            }
            match backfill_work_stats(&pool, &client).await {
                Ok(fill) if fill.fetched == 0 => {
                    if !announced {
                        let (_, stale_before) = stamps(chrono::Utc::now());
                        match stats_remaining(&pool, &stale_before).await {
                            Ok(0) => tracing::info!(
                                batches,
                                fetched = total.fetched,
                                answered = total.answered,
                                unanswered = total.unanswered,
                                "stats backfill: drained — every MangaDex-anchored work has \
                                 fresh statistics"
                            ),
                            Ok(remaining) => tracing::warn!(
                                remaining,
                                batches,
                                "stats backfill: reported idle but works remain — they are \
                                 unreachable, not merely pending"
                            ),
                            Err(e) => {
                                tracing::warn!(error = %crate::db::observed_database_error(e), "stats backfill: remaining check failed")
                            }
                        }
                        announced = true;
                    }
                    if nap(STATS_BACKFILL_IDLE_RECHECK, &mut shutdown).await {
                        return;
                    }
                }
                Ok(fill) => {
                    total.fetched += fill.fetched;
                    total.answered += fill.answered;
                    total.unanswered += fill.unanswered;
                    batches += 1;
                    announced = false;
                    if batches.is_multiple_of(STATS_BACKFILL_PROGRESS_EVERY) {
                        tracing::info!(
                            batches,
                            fetched = total.fetched,
                            answered = total.answered,
                            unanswered = total.unanswered,
                            "stats backfill: progress"
                        );
                    }
                    if nap(STATS_BACKFILL_GAP, &mut shutdown).await {
                        return;
                    }
                }
                // Switched off mid-drain: nothing reached MangaDex, so wait on the
                // switch rather than back off for an hour.
                Err(e) if crate::mangadex::is_off(&e) => continue,
                Err(e) => {
                    // Nothing was written; the batch is simply re-offered. Back off to the
                    // idle interval — retrying an unhappy upstream every 2 s is how a
                    // rate-limit becomes a ban.
                    tracing::warn!(error = %crate::db::observed_database_error(e), batches, "stats backfill: batch failed — retrying later");
                    if nap(STATS_BACKFILL_IDLE_RECHECK, &mut shutdown).await {
                        return;
                    }
                }
            }
        }
    };
    tokio::spawn(Database::job_scope(
        crate::database::ParallelJob::StatsBackfill,
        pass,
    ));
}

/// The MangaDex rating to DISPLAY for a page of works, keyed by work id, in ONE `IN` query.
///
/// `rating_avg` (the plain mean — the column migration 0106 keeps "for display") first, the
/// bayesian `rating` only where the average is missing. A work with no `work_stats` row (not
/// MangaDex-anchored, or the drain has not reached it yet), or whose row carries no positive
/// value (MangaDex reports an unrated title as `average: null, bayesian: 0`), is simply absent:
/// absent means "MangaDex has no rating for it", never "rated zero".
///
/// Propagate failed reads so a memoized feed never stores a degraded rating map.
pub async fn display_rating_batch_checked(
    pool: &Database,
    work_ids: &[String],
) -> Result<HashMap<String, f64>> {
    if work_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ph = crate::sql_params::placeholders(1, work_ids.len());
    let sql = format!(
        "SELECT work_id, \
                COALESCE(CASE WHEN rating_avg > 0 THEN rating_avg END, \
                         CASE WHEN rating > 0 THEN rating END) \
           FROM work_stats WHERE work_id IN ({ph})"
    );
    let mut q = crate::db_types::query_tuple::<(String, Option<f64>)>(&sql);
    for id in work_ids {
        q = q.bind(id);
    }
    Ok(q.fetch_all(pool)
        .await?
        .into_iter()
        .filter_map(|(wid, r)| r.filter(|x| x.is_finite()).map(|x| (wid, x)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const U1: &str = "a96676e5-8ae2-425e-b549-7f15dd34a6d8";
    const U2: &str = "32d76d19-8a05-4db0-9fc2-e0b0648fe9d0";
    const U3: &str = "0aea9f43-d4a9-4bf7-bebc-550a512f9b95";

    #[test]
    fn statistics_json_parses_follows_bayesian_and_average() {
        let body = json!({
            "result": "ok",
            "statistics": {
                U1: {
                    "comments": {"threadId": 1, "repliesCount": 5},
                    "rating": {"average": 8.91, "bayesian": 8.79, "distribution": {"10": 5}},
                    "follows": 412345
                },
                // Unrated: average null, bayesian 0 -> both "no data".
                U2: {"comments": null, "rating": {"average": null, "bayesian": 0}, "follows": 3},
                // Missing fields entirely.
                U3: {},
                // Not an object: skipped, not fatal.
                "bogus": 7
            }
        });
        let m = parse_statistics(&body).unwrap();
        assert_eq!(
            m[U1],
            MdStats {
                follows: Some(412_345),
                rating: Some(8.79),
                rating_avg: Some(8.91)
            }
        );
        assert_eq!(
            m[U2],
            MdStats {
                follows: Some(3),
                rating: None,
                rating_avg: None
            }
        );
        assert_eq!(m[U3], MdStats::default());
        assert!(!m.contains_key("bogus"));
        // Upper-case keys are normalized to the lowercase uuids the work-list asks with.
        let upper = json!({"result": "ok", "statistics": {U1.to_uppercase(): {"follows": 1}}});
        assert_eq!(parse_statistics(&upper).unwrap()[U1].follows, Some(1));
    }

    #[test]
    fn statistics_json_rejects_errors_and_accepts_an_empty_array() {
        assert!(parse_statistics(&json!({"result": "error", "errors": []})).is_err());
        assert!(parse_statistics(&json!({"result": "ok"})).is_err());
        assert!(parse_statistics(&json!({"result": "ok", "statistics": 5})).is_err());
        assert!(parse_statistics(&json!({"statistics": {}})).is_err());
        assert!(parse_statistics(&json!({"result": "ok", "statistics": []}))
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn stats_publication_rechecks_a_work_deleted_by_a_concurrent_writer() {
        let pool = pool().await.foreground();
        sqlx::query(
            "INSERT INTO work(id,created_at,updated_at) VALUES ('w_deleted_stats',NOW(),NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut first = pool.begin().await.unwrap();
        crate::locks::lock_works_for_delete(&mut first, &["w_deleted_stats"])
            .await
            .unwrap();
        sqlx::query("DELETE FROM work WHERE id='w_deleted_stats'")
            .execute(&mut *first)
            .await
            .unwrap();
        let writer = pool.clone();
        let second = tokio::spawn(async move {
            write_work_stats(
                &writer,
                &[PendingStats {
                    work_id: "w_deleted_stats".into(),
                    mangadex_id: None,
                }],
                &HashMap::new(),
                "2026-01-01T00:00:00Z",
            )
            .await
        });
        crate::cover::wait_for_writer_lock(&pool).await;
        first.commit().await.unwrap();
        assert_eq!(
            second.await.unwrap().unwrap(),
            StatsFill {
                fetched: 1,
                answered: 0,
                unanswered: 1
            }
        );
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_stats WHERE work_id='w_deleted_stats'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0);
    }

    async fn pool() -> Database {
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    /// A work with a MangaDex anchor `key` created at `created`, plus its Browse row.
    async fn anchored(pool: &Database, work_id: &str, key: &str, created: &str) {
        sqlx::query(
            "INSERT INTO work (id, primary_title, created_at, updated_at) \
             VALUES ($1, $2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z') ON CONFLICT (id) DO NOTHING",
        )
        .bind(work_id)
        .bind(work_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ($1, $2, 'mangadex', 'mangadex', $3, $4)",
        )
        .bind(format!("ss-{work_id}-{key}"))
        .bind(work_id)
        .bind(key)
        .bind(crate::db_types::date(created).unwrap())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO browse_catalogue (work_id, reader_id, title, created_at) \
             VALUES ($1, $2, $3, '2026-01-01T00:00:00Z') ON CONFLICT (work_id) DO NOTHING",
        )
        .bind(work_id)
        .bind(work_id)
        .bind(work_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn stats_row(pool: &Database, work_id: &str, fetched_at: &str) {
        sqlx::query(
            "INSERT INTO work_stats (work_id, follows, rating, rating_avg, fetched_at) \
             VALUES ($1, 1, 1.0, 1.0, $2)",
        )
        .bind(work_id)
        .bind(crate::db_types::date(fetched_at).unwrap())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn parallel_stats(pool: &Database, work_id: &str) -> Result<StatsFill> {
        Database::job_scope(crate::database::ParallelJob::StatsBackfill, async {
            write_work_stats(
                pool,
                &[PendingStats {
                    work_id: work_id.into(),
                    mangadex_id: Some(U1.into()),
                }],
                &HashMap::from([(
                    U1.into(),
                    MdStats {
                        follows: Some(123),
                        rating: Some(8.25),
                        rating_avg: Some(8.5),
                    },
                )]),
                "2026-04-01T00:00:00Z",
            )
            .await
        })
        .await
    }

    async fn browse_row(pool: &Database, work_id: &str) -> String {
        sqlx::query_scalar("SELECT row_to_json(b)::text FROM browse_catalogue b WHERE work_id=$1")
            .bind(work_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn parallel_stats_and_browse_publication_converge_in_both_orders() {
        use crate::catalog::phase4_tests::{overlap, pool};
        for stats_first in [true, false] {
            let pool = pool().await;
            anchored(&pool, "w_stats_publication", U1, "2026-01-01T00:00:00Z").await;
            crate::catalog::refresh_browse_catalogue(&pool)
                .await
                .unwrap();
            sqlx::query(
                "UPDATE browse_catalogue SET title='Stale' WHERE work_id='w_stats_publication'",
            )
            .execute(&pool)
            .await
            .unwrap();
            if stats_first {
                let points = crate::locks::pause_points::PausePoints::default();
                let held = points.arm("guard-held");
                let writer = pool.clone();
                let stats = tokio::spawn(
                    crate::catalog::guards::TEST_PAUSE.scope(points, async move {
                        parallel_stats(&writer, "w_stats_publication").await
                    }),
                );
                tokio::time::timeout(Duration::from_secs(5), held.reached())
                    .await
                    .unwrap();
                // No wait: publication uses SKIP LOCKED and leaves this work for
                // the next rebuild while the statistics transaction stays open.
                let rebuilt = tokio::time::timeout(
                    Duration::from_secs(2),
                    crate::catalog::refresh_browse_catalogue(&pool),
                )
                .await;
                let title: String = sqlx::query_scalar(
                    "SELECT title FROM browse_catalogue WHERE work_id='w_stats_publication'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                held.release();
                rebuilt
                    .expect("Browse waited on a work it should skip")
                    .unwrap();
                assert_eq!(stats.await.unwrap().unwrap().answered, 1);
                assert_eq!(title, "Stale", "a busy work is a documented clean skip");
                crate::catalog::refresh_browse_catalogue(&pool)
                    .await
                    .unwrap();
            } else {
                let (publication, stats) = overlap(
                    &pool,
                    |pool| async move { crate::catalog::refresh_browse_catalogue(&pool).await },
                    |pool| async move { parallel_stats(&pool, "w_stats_publication").await },
                )
                .await;
                publication.unwrap();
                assert_eq!(stats.unwrap().answered, 1);
            }
            let values: (Option<i64>, Option<f64>, Option<i64>, Option<f64>) = sqlx::query_as(
                "SELECT s.follows,s.rating,b.follows,b.rating FROM work_stats s \
                 JOIN browse_catalogue b USING(work_id) WHERE work_id='w_stats_publication'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(values, (Some(123), Some(8.25), Some(123), Some(8.25)));
            let concurrent = browse_row(&pool, "w_stats_publication").await;
            sqlx::query("UPDATE browse_catalogue SET follows=NULL,rating=NULL WHERE work_id='w_stats_publication'")
                .execute(&pool).await.unwrap();
            crate::catalog::refresh_browse_catalogue(&pool)
                .await
                .unwrap();
            assert_eq!(browse_row(&pool, "w_stats_publication").await, concurrent);
        }
    }

    #[tokio::test]
    async fn parallel_stats_and_merge_drop_the_losers_stats_in_both_orders() {
        use crate::catalog::phase4_tests::{overlap, pool, work};
        for stats_first in [true, false] {
            let pool = pool().await;
            anchored(&pool, "w_stats_loser", U1, "2026-01-01T00:00:00Z").await;
            stats_row(&pool, "w_stats_loser", "2025-01-01T00:00:00Z").await;
            let winner = work(&pool, "Survivor").await;
            stats_row(&pool, &winner, "2026-01-01T00:00:00Z").await;
            let target = winner.clone();
            let stats =
                |pool: Database| async move { parallel_stats(&pool, "w_stats_loser").await };
            let merge = move |pool: Database| async move {
                crate::catalog::merge_works(&pool, "w_stats_loser", &target).await
            };
            let fill = if stats_first {
                let (stats, merge) = overlap(&pool, stats, merge).await;
                merge.unwrap();
                let fill = stats.unwrap();
                assert_eq!(fill.answered, 1);
                fill
            } else {
                let (merge, stats) = overlap(&pool, merge, stats).await;
                merge.unwrap();
                let fill = stats.unwrap();
                assert_eq!(
                    fill.unanswered, 1,
                    "a merged-away pending parent is skipped"
                );
                fill
            };
            assert_eq!(fill.fetched, 1);
            let retired: (i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM work WHERE id='w_stats_loser'), \
                        (SELECT COUNT(*) FROM work_stats WHERE work_id='w_stats_loser'), \
                        (SELECT COUNT(*) FROM browse_catalogue WHERE work_id='w_stats_loser')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(retired, (0, 0, 0));
            let owner: String =
                sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key=$1")
                    .bind(U1)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(owner, winner);
            let survivor: (Option<i64>, Option<f64>) =
                sqlx::query_as("SELECT follows,rating FROM work_stats WHERE work_id=$1")
                    .bind(&winner)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                survivor,
                (Some(1), Some(1.0)),
                "merge cascades the loser; it keeps the survivor's stats"
            );
        }
    }

    /// The work-list order: never-fetched first (oldest anchor first, a twice-anchored work
    /// once), then stale rows stalest-first; fresh rows and Suwayomi-only works never.
    #[tokio::test]
    async fn the_stats_drain_picks_never_fetched_first_then_stalest() {
        let pool = pool().await;
        anchored(&pool, "w_new_b", U2, "2026-02-02T00:00:00Z").await;
        anchored(&pool, "w_new_a", U1, "2026-02-01T00:00:00Z").await;
        // Twice anchored: appears once.
        anchored(&pool, "w_new_a", "not-a-uuid", "2026-02-03T00:00:00Z").await;
        anchored(&pool, "w_stale_old", U3, "2026-01-01T00:00:00Z").await;
        stats_row(&pool, "w_stale_old", "2026-03-01T00:00:00Z").await;
        anchored(
            &pool,
            "w_stale_new",
            "11111111-1111-1111-1111-111111111111",
            "2026-01-01T00:00:00Z",
        )
        .await;
        stats_row(&pool, "w_stale_new", "2026-03-05T00:00:00Z").await;
        anchored(
            &pool,
            "w_fresh",
            "22222222-2222-2222-2222-222222222222",
            "2026-01-01T00:00:00Z",
        )
        .await;
        stats_row(&pool, "w_fresh", "2026-03-20T00:00:00Z").await;
        // A Suwayomi-only work is never on the list.
        sqlx::query(
            "INSERT INTO work (id, primary_title, created_at, updated_at) \
             VALUES ('w_sw', 'S', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss-sw', 'w_sw', 'suwayomi', '9', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let stale_before = "2026-03-10T00:00:00Z";
        let list = works_needing_stats(&pool, 100, stale_before).await.unwrap();
        let order: Vec<&str> = list.iter().map(|p| p.work_id.as_str()).collect();
        assert_eq!(order, ["w_new_a", "w_new_b", "w_stale_old", "w_stale_new"]);
        assert_eq!(
            list[0].mangadex_id.as_deref(),
            Some(U1),
            "the oldest anchor"
        );
        // The batch limit is honoured across both halves.
        let two = works_needing_stats(&pool, 2, stale_before).await.unwrap();
        assert_eq!(
            two.iter().map(|p| p.work_id.as_str()).collect::<Vec<_>>(),
            ["w_new_a", "w_new_b"]
        );
        assert_eq!(
            works_needing_stats(&pool, 3, stale_before).await.unwrap()[2].work_id,
            "w_stale_old"
        );
        assert_eq!(stats_remaining(&pool, stale_before).await.unwrap(), 4);
    }

    /// Writing a batch: answered works get exact values (Browse's copy too), unanswered ones
    /// keep what they had and only move their clock — and either way the work leaves the
    /// list, so repeated batches TERMINATE even when upstream answers nothing.
    #[tokio::test]
    async fn the_stats_drain_writes_terminates_and_keeps_unanswered_values() {
        let pool = pool().await;
        anchored(&pool, "w_1", U1, "2026-02-01T00:00:00Z").await;
        anchored(&pool, "w_2", U2, "2026-02-02T00:00:00Z").await;
        anchored(&pool, "w_3", U3, "2026-02-03T00:00:00Z").await;
        // w_3 was fetched long ago and upstream will not answer for it now.
        sqlx::query(
            "INSERT INTO work_stats (work_id, follows, rating, rating_avg, fetched_at) \
             VALUES ('w_3', 77, 6.5, 6.6, '2025-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let now = "2026-04-01T00:00:00Z";
        let stale_before = "2026-03-25T00:00:00Z";
        let pending = works_needing_stats(&pool, 100, stale_before).await.unwrap();
        assert_eq!(pending.len(), 3);
        let body = json!({"result": "ok", "statistics": {
            U1: {"follows": 1000, "rating": {"average": 9.1, "bayesian": 9.0}},
            U2: {"follows": 10, "rating": {"average": null, "bayesian": 0}},
        }});
        let stats = parse_statistics(&body).unwrap();
        let fill = write_work_stats(&pool, &pending, &stats, now)
            .await
            .unwrap();
        assert_eq!(
            fill,
            StatsFill {
                fetched: 3,
                answered: 2,
                unanswered: 1
            }
        );
        let row = |id: &'static str| {
            let pool = pool.clone();
            async move {
                let row = crate::db_types::query_tuple::<(Option<i64>, Option<f64>, Option<f64>, chrono::DateTime<chrono::Utc>)>(
                    "SELECT follows, rating, rating_avg, fetched_at FROM work_stats WHERE work_id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
                (row.0, row.1, row.2, crate::db_types::format_date(row.3))
            }
        };
        assert_eq!(
            row("w_1").await,
            (
                Some(1000),
                Some(9.0),
                Some(9.1),
                crate::db_types::format_date(crate::db_types::date(now).unwrap())
            )
        );
        assert_eq!(
            row("w_2").await,
            (
                Some(10),
                None,
                None,
                crate::db_types::format_date(crate::db_types::date(now).unwrap())
            )
        );
        assert_eq!(
            row("w_3").await,
            (
                Some(77),
                Some(6.5),
                Some(6.6),
                crate::db_types::format_date(crate::db_types::date(now).unwrap())
            ),
            "unanswered keeps its values, only the clock moves"
        );
        let browse: (Option<i64>, Option<f64>) = crate::db_types::query_tuple(
            "SELECT follows, rating FROM browse_catalogue WHERE work_id = 'w_1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(browse, (Some(1000), Some(9.0)), "Browse's copy follows");
        // Terminated: nothing due, and an EMPTY answer for a fresh batch still drains it.
        assert!(works_needing_stats(&pool, 100, stale_before)
            .await
            .unwrap()
            .is_empty());
        anchored(
            &pool,
            "w_4",
            "33333333-3333-3333-3333-333333333333",
            "2026-02-04T00:00:00Z",
        )
        .await;
        let pending = works_needing_stats(&pool, 100, stale_before).await.unwrap();
        let fill = write_work_stats(&pool, &pending, &HashMap::new(), now)
            .await
            .unwrap();
        assert_eq!(fill.unanswered, 1);
        assert!(works_needing_stats(&pool, 100, stale_before)
            .await
            .unwrap()
            .is_empty());
        // And the Browse rebuild reaches the same values from `work_stats` (convergence).
        sqlx::query("UPDATE browse_catalogue SET follows = NULL, rating = NULL")
            .execute(&pool)
            .await
            .unwrap();
        crate::catalog::refresh_browse_catalogue(&pool)
            .await
            .unwrap();
        let rebuilt: (Option<i64>, Option<f64>) = crate::db_types::query_tuple(
            "SELECT follows, rating FROM browse_catalogue WHERE work_id = 'w_1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rebuilt, (Some(1000), Some(9.0)));
    }

    /// A rating is stored to four decimals, so a drift no reader can see does not rewrite
    /// the work's `browse_catalogue` row (WAL item 6).
    #[tokio::test]
    async fn a_rating_drift_below_four_decimals_leaves_browse_alone() {
        let pool = pool().await;
        anchored(&pool, "w_1", U1, "2026-02-01T00:00:00Z").await;
        let pending = works_needing_stats(&pool, 100, "2026-03-25T00:00:00Z")
            .await
            .unwrap();
        let answer = |rating: f64| {
            HashMap::from([(
                U1.to_string(),
                MdStats {
                    follows: Some(5),
                    rating: Some(rating),
                    rating_avg: Some(rating),
                },
            )])
        };
        write_work_stats(
            &pool,
            &pending,
            &answer(8.123456789),
            "2026-04-01T00:00:00Z",
        )
        .await
        .unwrap();
        let read = || async {
            crate::db_types::query_tuple::<(Option<f64>, Option<f64>, String)>(
                "SELECT s.rating, s.rating_avg, b.xmin::text FROM work_stats s \
                 JOIN browse_catalogue b ON b.work_id = s.work_id WHERE s.work_id = 'w_1'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let first = read().await;
        assert_eq!((first.0, first.1), (Some(8.1235), Some(8.1235)));
        write_work_stats(
            &pool,
            &pending,
            &answer(8.123456789 + 1e-9),
            "2026-04-08T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(
            read().await,
            first,
            "the drift writes no Browse row version"
        );
        write_work_stats(&pool, &pending, &answer(8.2), "2026-04-15T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(read().await.0, Some(8.2), "a real change still lands");
    }

    /// Both work-list halves must be LIMIT-bounded index walks — no temp B-tree over the
    /// ~115k anchors, no scan of `work_stats` — or every 2-second batch would re-sort the
    /// catalogue. The plans are printed so they can be quoted.
    #[tokio::test]
    async fn the_stats_worklist_queries_are_index_walks() {
        use sqlx::Row;
        let pool = pool().await;
        // PostgreSQL legitimately sorts a 300-row table rather than opening an
        // index. Use the audited catalogue scale to verify the production plan;
        // bulk fixtures keep this bounded test fast without planner overrides.
        sqlx::query("INSERT INTO work (id, primary_title, created_at, updated_at) \
            SELECT 'w_' || lpad(i::text, 6, '0'), 'Fixture', TIMESTAMPTZ '2026-01-01T00:00:00Z', TIMESTAMPTZ '2026-01-01T00:00:00Z' \
            FROM generate_series(0, 115566) i").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
            SELECT 'ss_' || lpad(i::text, 6, '0'), 'w_' || lpad(i::text, 6, '0'), 'mangadex', 'mangadex', \
            lpad(i::text, 8, '0') || '-0000-0000-0000-000000000000', TIMESTAMPTZ '2026-01-01T00:00:00Z' \
            FROM generate_series(0, 115566) i").execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO work_stats (work_id, follows, rating, rating_avg, fetched_at) \
            SELECT 'w_' || lpad(i::text, 6, '0'), 1, 1.0, 1.0, TIMESTAMPTZ '2026-01-01T00:00:00Z' \
            FROM generate_series(0, 115566) i WHERE i % 2 = 0",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE").execute(&pool).await.unwrap();
        for (label, sql, want) in [
            ("never", NEVER_FETCHED_SQL, "idx_source_series_type_created"),
            ("stale", STALE_SQL, "idx_work_stats_fetched"),
        ] {
            let text = format!("EXPLAIN (FORMAT JSON) {sql}");
            let rows = if label == "never" {
                sqlx::query(&text)
                    .bind(200i64)
                    .fetch_all(&pool)
                    .await
                    .unwrap()
            } else {
                sqlx::query(&text)
                    .bind(crate::db_types::date("2026-06-01T00:00:00Z").unwrap())
                    .bind(100i64)
                    .fetch_all(&pool)
                    .await
                    .unwrap()
            };
            let plan: serde_json::Value = rows[0].get(0);
            let root = &plan[0]["Plan"];
            assert_eq!(root["Node Type"], "Limit", "bounded worklist: {plan}");
            assert!(
                plan.to_string().contains(want),
                "{label}: expected index {want}: {plan}"
            );
            assert_ne!(
                root["Plans"][0]["Node Type"], "Sort",
                "unbounded top-level sort: {plan}"
            );
        }
    }

    #[test]
    fn stamps_are_uniform_and_the_cutoff_is_a_week_back() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-10-03T12:34:56.789+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (now, cutoff) = stamps(t);
        assert_eq!(now, "2026-10-03T12:34:56Z");
        assert_eq!(cutoff, "2026-09-26T12:34:56Z");
        assert!(cutoff < now, "text order is time order");
    }
}
