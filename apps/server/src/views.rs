//! Per-series view (chapter-read) tracking — the popularity signal behind Trending
//! and the series-page "views" stat.
//!
//! A "view" is recorded once per chapter open (the reader fires `recordView` when a
//! chapter loads), for anonymous and ordinary signed-in readers alike — no auth is
//! REQUIRED — because popularity is a public engagement signal (the product decision is
//! to count all reads, not dedupe per user). That is why this is a dedicated no-auth
//! path and NOT piggybacked on `setProgress`, which requires a user and fires on every
//! page-position save. The one exception: an admin viewer's reads are silently skipped
//! (write-side gate in the `recordView` resolver) so an admin's own browsing can never
//! move Trending or the view counters — see `graphql/mod.rs::record_view`.
//!
//! Counts are keyed by a NORMALISED series key (`view_key`) so a series read under
//! either identity accrues to one total. Numeric Suwayomi ids are the primary display
//! identity (home / browse / library / the `series(id)` query all use them), so keys
//! normalise TOWARD the numeric id: a canonical `w_` work read via its work page folds
//! into its Suwayomi source id when it has one — that way Trending links to the same
//! page the rest of the app uses. A pure MangaDex work with no Suwayomi source keeps
//! its `w_` key.
//!
//! Storage (migration 0036):
//!   - `series_views`       — all-time total per key (one row per series).
//!   - `series_view_bucket` — hourly counts per key for the 24h / 7d windows and for
//!     ranking Trending; pruned beyond [`BUCKET_RETENTION_HOURS`] by the GC sweep.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::db::Database;
use anyhow::Result;

/// Retention for hourly buckets — the widest query window (7 days). Older buckets are
/// swept by [`prune_buckets`]; the all-time `series_views` total is never pruned.
pub const BUCKET_RETENTION_HOURS: i64 = 24 * 7;

/// How often the background flusher drains the view buffer. Trending is served from a
/// 30–60s in-process cache, so a few seconds of view staleness is invisible.
pub const FLUSH_INTERVAL_SECS: u64 = 3;

/// Cap on distinct `(series_id, hour)` entries held in memory between flushes. Each
/// entry is one small string + two i64s, so this bounds the buffer to a few hundred KB
/// in the worst case. When the cap is hit, [`record`] falls back to the old direct
/// write instead of dropping the view.
const BUFFER_MAX_KEYS: usize = 4096;

/// The three view windows for one series.
#[derive(Debug, Default, Clone, Copy)]
pub struct ViewCounts {
    pub total: i64,
    pub last7d: i64,
    pub last24h: i64,
}

/// The current Unix epoch hour (seconds / 3600) — the `hour_ts` bucket key.
fn current_hour() -> i64 {
    chrono::Utc::now().timestamp() / 3600
}

/// Resolve a reader-facing series id to its normalised view key (see module docs).
/// Numeric Suwayomi ids — the primary display identity — key on themselves; a canonical
/// `w_` work folds into its Suwayomi source id when it has one, else keeps its `w_` key.
///
/// The fold picks the smallest source id by NUMERIC order (source_key is the numeric
/// Suwayomi manga id) so it's stable and matches how the app numbers ids.
///
/// LIMITATION: for a work with MULTIPLE Suwayomi sources, views recorded via a
/// non-chosen source's numeric page key under that other id and won't merge into the
/// work's total — and a pure-MangaDex work that later GAINS a Suwayomi source splits
/// its pre/post views across the two keys. Fully unifying those would need a persistent
/// canonical-source-per-work concept; single-source works (the common case) unify
/// exactly.
pub async fn view_key(pool: &Database, series_id: &str) -> String {
    match pool.acquire().await {
        Ok(mut connection) => view_key_in(&mut connection, series_id)
            .await
            .unwrap_or_else(|_| series_id.to_string()),
        Err(_) => series_id.to_string(),
    }
}

async fn view_key_in(connection: &mut sqlx::SqliteConnection, series_id: &str) -> Result<String> {
    if !series_id.is_empty() && series_id.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(series_id.to_string());
    }
    let survivor: Option<String> =
        sqlx::query_scalar("SELECT new_id FROM work_redirect WHERE old_id = ?")
            .bind(series_id)
            .fetch_optional(&mut *connection)
            .await?;
    let work_id = survivor.as_deref().unwrap_or(series_id);
    let numeric: Option<String> = sqlx::query_scalar(
        "SELECT source_key FROM source_series \
         WHERE work_id = ? AND source_type = 'suwayomi' \
         ORDER BY CAST(source_key AS INTEGER) LIMIT 1",
    )
    .bind(work_id)
    .fetch_optional(&mut *connection)
    .await?;
    Ok(numeric.unwrap_or_else(|| work_id.to_string()))
}

/// In-memory view buffer: raw `series_id` views aggregated by the hour they were
/// recorded in, drained by the background flusher (see [`spawn_flusher`]).
///
/// Why buffer at all: `record` used to be 1 key-resolution read + a 2-upsert
/// transaction PER CHAPTER OPEN, serialized against every other write by SQLite's
/// single writer. Buffering coalesces a burst of opens into one batched transaction
/// every [`FLUSH_INTERVAL_SECS`], and de-duplicates the key-resolution read per
/// distinct series per flush.
///
/// The buffer stores the RAW reader-facing id, not the normalised view key — key
/// resolution (`view_key`) needs a DB read for `w_` ids, and doing it at flush time
/// keeps `record` a pure in-memory push. The hour is captured at record time so views
/// buffered across an hour boundary still land in the bucket they happened in.
struct ViewBuffer {
    /// `(series_id, hour_ts) -> count of views`.
    pending: Mutex<HashMap<(String, i64), i64>>,
}

impl ViewBuffer {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Lock, tolerating poison — the map holds plain counts, always consistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, i64), i64>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Buffer one view at `hour`. Returns `false` (buffering refused) only when the
    /// buffer is at [`BUFFER_MAX_KEYS`] distinct entries AND this view would add a new
    /// one — an existing entry can always keep counting.
    fn push_at(&self, series_id: &str, hour: i64) -> bool {
        let mut m = self.lock();
        if let Some(n) = m.get_mut(&(series_id.to_string(), hour)) {
            *n += 1;
            return true;
        }
        if m.len() >= BUFFER_MAX_KEYS {
            return false;
        }
        m.insert((series_id.to_string(), hour), 1);
        true
    }

    /// Take everything buffered, leaving the buffer empty.
    fn drain(&self) -> HashMap<(String, i64), i64> {
        std::mem::take(&mut *self.lock())
    }

    /// Return a failed flush's views to the buffer (summing with anything recorded
    /// since the drain) so one DB error doesn't lose them — the next tick retries.
    /// May transiently exceed [`BUFFER_MAX_KEYS`]; entries are aggregated counts, so
    /// one interval's overshoot is small. But a SUSTAINED write outage would grow the
    /// map by up to a cap of fresh keys per tick, so a hard valve at twice the cap
    /// drops the excess (with a count in the log) rather than let memory grow without
    /// bound — by then views are being lost to the direct-write fallback anyway.
    fn merge_back(&self, views: HashMap<(String, i64), i64>) {
        let mut m = self.lock();
        for (k, n) in views {
            *m.entry(k).or_insert(0) += n;
        }
        let hard_cap = BUFFER_MAX_KEYS * 2;
        if m.len() > hard_cap {
            let excess = m.len() - hard_cap;
            let dropped: i64 = {
                let keys: Vec<_> = m.keys().take(excess).cloned().collect();
                keys.iter().filter_map(|k| m.remove(k)).sum()
            };
            tracing::warn!(
                entries = excess,
                views = dropped,
                "view buffer over hard cap during flush failures; dropped oldest-iterated entries"
            );
        }
    }
}

/// The process-wide buffer behind [`record`] / [`flush`].
static GLOBAL_BUFFER: LazyLock<ViewBuffer> = LazyLock::new(ViewBuffer::new);

/// Record one view for `series_id`. Idempotency is intentionally NOT enforced — every
/// chapter open counts (see module docs).
///
/// This is now an in-memory push that returns immediately; the background flusher
/// ([`spawn_flusher`]) persists buffered views in one batched transaction every few
/// seconds. Only when the buffer is full (shouldn't happen at sane traffic — see
/// [`BUFFER_MAX_KEYS`]) does this fall back to the old direct write, so a view is
/// never silently dropped while the DB is up.
pub async fn record(pool: &Database, series_id: &str) -> Result<()> {
    // Under `cargo test`, write through synchronously: integration tests assert
    // counts right after the mutation, and the PROCESS-GLOBAL buffer would leak
    // views across parallel tests' separate pools. The buffered path is covered by
    // this module's own tests against private `ViewBuffer`s.
    #[cfg(test)]
    {
        let mut one = HashMap::new();
        one.insert((series_id.to_string(), current_hour()), 1i64);
        return write_batch(pool, one).await;
    }
    #[cfg(not(test))]
    {
        record_with(&GLOBAL_BUFFER, pool, series_id).await
    }
}

/// [`record`] against an explicit buffer (unit tests use a private one so parallel
/// tests don't cross-contaminate through the global).
async fn record_with(buf: &ViewBuffer, pool: &Database, series_id: &str) -> Result<()> {
    let hour = current_hour();
    if buf.push_at(series_id, hour) {
        return Ok(());
    }
    // Buffer full: write this one view directly rather than dropping it.
    let mut one = HashMap::new();
    one.insert((series_id.to_string(), hour), 1);
    write_batch(pool, one).await
}

/// Drain the global buffer and persist it in one transaction. Called by the background
/// flusher every tick and once more on graceful shutdown. On error the drained views
/// are merged back for the next tick's retry.
pub async fn flush(pool: &Database) -> Result<usize> {
    flush_buffer(&GLOBAL_BUFFER, pool).await
}

async fn flush_buffer(buf: &ViewBuffer, pool: &Database) -> Result<usize> {
    let drained = buf.drain();
    if drained.is_empty() {
        return Ok(0);
    }
    let n = drained.len();
    match crate::db::retry_transaction(|| write_batch(pool, drained.clone())).await {
        Ok(()) => Ok(n),
        Err(e) => {
            buf.merge_back(drained);
            Err(e)
        }
    }
}

/// Persist a batch of `(series_id, hour) -> count` views in ONE transaction.
///
/// Key resolution happens here, once per DISTINCT series id in the batch (memoized),
/// instead of once per view as the old per-view `record` did. Distinct raw ids that
/// normalise to the same view key (a `w_` work and its numeric Suwayomi source) are
/// summed before writing so the upsert count matches the old per-view arithmetic.
async fn write_batch(pool: &Database, views: HashMap<(String, i64), i64>) -> Result<()> {
    // Resolve redirects and source keys on the same writer snapshot as the upserts.
    // A merge committed before this writer is acquired must never recreate old keys.
    let mut tx = pool.begin().await?;
    let mut key_of: HashMap<String, String> = HashMap::new();
    for (series_id, _) in views.keys() {
        if !key_of.contains_key(series_id) {
            let key = view_key_in(&mut tx, series_id).await?;
            key_of.insert(series_id.clone(), key);
        }
    }
    // Fold to (view_key, hour) -> count.
    let mut by_key: HashMap<(String, i64), i64> = HashMap::new();
    for ((series_id, hour), n) in views {
        let key = key_of[&series_id].clone();
        *by_key.entry((key, hour)).or_insert(0) += n;
    }
    let now = chrono::Utc::now().to_rfc3339();
    for ((key, hour), n) in &by_key {
        sqlx::query(
            "INSERT INTO series_views (series_key, total, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(series_key) DO UPDATE SET \
               total = total + excluded.total, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(n)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES (?, ?, ?) \
             ON CONFLICT(series_key, hour_ts) DO UPDATE SET views = views + excluded.views",
        )
        .bind(key)
        .bind(hour)
        .bind(n)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Spawn the background view flusher: drains the buffer every [`FLUSH_INTERVAL_SECS`],
/// and once more when the shutdown watch flips so buffered views survive a graceful
/// shutdown. Panic-supervised like the other background loops. The returned handle
/// lets `main` await the final flush before the process exits.
pub fn spawn_flusher(
    pool: Database,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(crate::task::supervise(
        "view-flush",
        Duration::from_secs(30),
        shutdown.clone(),
        move || run_flush_loop(pool.clone(), shutdown.clone()),
    ))
}

async fn run_flush_loop(pool: Database, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Err(e) = flush(&pool).await {
                    // Views were merged back by `flush`; next tick retries.
                    tracing::warn!(error = %e, "view flush failed; retrying next tick");
                }
            }
            _ = shutdown.changed() => {
                if let Err(e) = flush(&pool).await {
                    tracing::warn!(error = %e, "final view flush on shutdown failed");
                }
                return;
            }
        }
    }
}

/// Load the all-time / 7-day / 24-hour view counts for one series (resolves its key).
///
/// One aggregate query folds the all-time `total` (from `series_views`) with both hourly
/// windows (from `series_view_bucket`): the 24h and 7d sums come from the same scan, gated
/// by per-row `CASE`s. Since 7d ⊇ 24h, the row filter bounds to the 7d cutoff and the 24h
/// `CASE` narrows within it. This is 2 awaits (`view_key` + this) instead of the former 4 —
/// every `Series.views` resolver in a feed pays the difference per item.
pub async fn counts_for(pool: &Database, series_id: &str) -> ViewCounts {
    let key = view_key(pool, series_id).await;
    let hour = current_hour();
    let since24h = hour - 24;
    let since7d = hour - BUCKET_RETENTION_HOURS;
    // An aggregate with no GROUP BY always yields exactly one row, even with zero buckets
    // (the SUMs come back NULL → 0, the scalar subquery → 0), so `fetch_one` is safe.
    let row = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT \
           (SELECT COALESCE(total, 0) FROM series_views WHERE series_key = ?), \
           COALESCE(SUM(CASE WHEN hour_ts > ? THEN views END), 0), \
           COALESCE(SUM(views), 0) \
         FROM series_view_bucket \
         WHERE series_key = ? AND hour_ts > ?",
    )
    .bind(&key)
    .bind(since24h)
    .bind(&key)
    .bind(since7d)
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0, 0));
    ViewCounts {
        total: row.0,
        last24h: row.1,
        last7d: row.2,
    }
}

/// The most-viewed series keys over the last 24 hours, most-viewed first — the ranking
/// behind Trending. Returns up to `limit` `(series_key, views)` pairs; empty until reads
/// accumulate (the caller pads a cold-start list with another feed).
pub async fn trending_keys(pool: &Database, limit: i64) -> Result<Vec<(String, i64)>> {
    let hour = current_hour();
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT series_key, SUM(views) AS v FROM series_view_bucket \
         WHERE hour_ts > ? GROUP BY series_key ORDER BY v DESC, series_key ASC LIMIT ?",
    )
    .bind(hour - 24)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Delete hourly buckets older than the retention window. Called by the GC sweep so the
/// bucket table stays bounded; the all-time totals in `series_views` are untouched. The
/// boundary uses `<=` to match the windows' strict `hour_ts > now - N` — the bucket at
/// exactly `now - RETENTION` is never summed by any window, so it's pruned rather than
/// kept forever.
pub async fn prune_buckets(pool: &Database) -> Result<u64> {
    let cutoff = current_hour() - BUCKET_RETENTION_HOURS;
    let r = sqlx::query("DELETE FROM series_view_bucket WHERE hour_ts <= ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    /// Record + immediately flush through a PRIVATE buffer. Tests never touch the
    /// global buffer — `cargo test` runs tests in parallel in one process, and views
    /// buffered by one test must not flush into another test's pool.
    async fn record_flushed(pool: &Database, series_id: &str) {
        let buf = ViewBuffer::new();
        record_with(&buf, pool, series_id).await.unwrap();
        flush_buffer(&buf, pool).await.unwrap();
    }

    #[tokio::test]
    async fn buffered_views_follow_a_merge_committed_by_another_connection() {
        let pool = pool().await;
        let external = Database::connect_with((*pool.connect_options()).clone(), 2)
            .await
            .unwrap();
        let buf = std::sync::Arc::new(ViewBuffer::new());
        buf.push_at("w_old", current_hour());
        let mut merge = external.begin().await.unwrap();
        sqlx::query("INSERT INTO work_redirect(old_id,new_id,created_at) VALUES ('w_old','w_survivor','2026-01-01')")
            .execute(&mut *merge).await.unwrap();
        let writer = pool.clone();
        let flush_buf = buf.clone();
        let mut flush = tokio::spawn(async move { flush_buffer(&flush_buf, &writer).await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut flush)
            .await
            .is_err());
        merge.commit().await.unwrap();
        flush.await.unwrap().unwrap();
        let rows: Vec<(String, i64)> = sqlx::query_as("SELECT series_key,total FROM series_views")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows, vec![("w_survivor".into(), 1)]);
        let keys: Vec<String> = sqlx::query_scalar("SELECT series_key FROM series_view_bucket")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(keys, vec!["w_survivor"]);
        external.close().await;
        pool.close().await;
    }

    #[tokio::test]
    async fn records_and_counts_across_windows() {
        let pool = pool().await;
        // Three views of a bare (un-catalogued) numeric series.
        for _ in 0..3 {
            record_flushed(&pool, "42").await;
        }
        let c = counts_for(&pool, "42").await;
        assert_eq!(c.total, 3);
        assert_eq!(c.last24h, 3);
        assert_eq!(c.last7d, 3);

        // A different series with one view ranks below the first in trending.
        record_flushed(&pool, "7").await;
        let top = trending_keys(&pool, 10).await.unwrap();
        assert_eq!(top[0], ("42".to_string(), 3));
        assert_eq!(top[1], ("7".to_string(), 1));
    }

    #[tokio::test]
    async fn windows_actually_bound_by_age() {
        // A view 30 hours ago must fall OUT of last24h but stay IN last7d and total —
        // proving the windows key on hour_ts, not just "any bucket". `record` writes the
        // current hour; backdate that bucket to hour-30.
        let pool = pool().await;
        record_flushed(&pool, "42").await;
        let h30 = current_hour() - 30;
        sqlx::query("UPDATE series_view_bucket SET hour_ts = ? WHERE series_key = '42'")
            .bind(h30)
            .execute(&pool)
            .await
            .unwrap();
        // A second, fresh (current-hour) view.
        record_flushed(&pool, "42").await;

        let c = counts_for(&pool, "42").await;
        assert_eq!(c.total, 2, "all-time counts both");
        assert_eq!(c.last24h, 1, "only the fresh view is within 24h");
        assert_eq!(c.last7d, 2, "both are within 7 days");
        // Trending's 24h ranking sees only the in-window view.
        let top = trending_keys(&pool, 10).await.unwrap();
        assert_eq!(top[0], ("42".to_string(), 1));
    }

    #[tokio::test]
    async fn old_buckets_prune_but_all_time_total_survives() {
        let pool = pool().await;
        record_flushed(&pool, "42").await;
        // Backdate the bucket beyond retention, leaving the all-time total intact.
        let old = current_hour() - (BUCKET_RETENTION_HOURS + 5);
        sqlx::query("UPDATE series_view_bucket SET hour_ts = ? WHERE series_key = '42'")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        let pruned = prune_buckets(&pool).await.unwrap();
        assert_eq!(pruned, 1);
        let c = counts_for(&pool, "42").await;
        assert_eq!(c.total, 1, "all-time total is never pruned");
        assert_eq!(c.last24h, 0, "the windowed bucket is gone");
    }

    #[tokio::test]
    async fn buffer_aggregates_and_flushes_in_one_batch() {
        let pool = pool().await;
        let buf = ViewBuffer::new();
        // A burst of opens: 3× series 42, 1× series 7 — nothing hits the DB yet.
        for _ in 0..3 {
            record_with(&buf, &pool, "42").await.unwrap();
        }
        record_with(&buf, &pool, "7").await.unwrap();
        assert_eq!(
            counts_for(&pool, "42").await.total,
            0,
            "buffered, not written"
        );

        // One flush lands everything; it reports the distinct entries drained.
        let n = flush_buffer(&buf, &pool).await.unwrap();
        assert_eq!(n, 2, "two distinct (series, hour) entries");
        assert_eq!(counts_for(&pool, "42").await.total, 3);
        assert_eq!(counts_for(&pool, "7").await.total, 1);

        // The buffer is empty after a successful flush — flushing again is a no-op.
        assert_eq!(flush_buffer(&buf, &pool).await.unwrap(), 0);
        assert_eq!(counts_for(&pool, "42").await.total, 3, "no double count");
    }

    #[tokio::test]
    async fn buffered_views_keep_their_recorded_hour() {
        // A view buffered 30 hours ago must flush into ITS bucket, not the flush-time
        // bucket — proving the hour is captured at record time.
        let pool = pool().await;
        let buf = ViewBuffer::new();
        assert!(buf.push_at("42", current_hour() - 30));
        assert!(buf.push_at("42", current_hour()));
        flush_buffer(&buf, &pool).await.unwrap();

        let c = counts_for(&pool, "42").await;
        assert_eq!(c.total, 2);
        assert_eq!(c.last24h, 1, "only the fresh view is within 24h");
        assert_eq!(c.last7d, 2);
    }

    #[tokio::test]
    async fn full_buffer_falls_back_to_direct_write() {
        let pool = pool().await;
        let buf = ViewBuffer::new();
        let hour = current_hour();
        // Fill the buffer to its cap with distinct series ids.
        for i in 0..BUFFER_MAX_KEYS {
            assert!(buf.push_at(&format!("id{i}"), hour));
        }
        // A NEW entry is refused… but an existing one can still keep counting.
        assert!(!buf.push_at("overflow", hour));
        assert!(buf.push_at("id0", hour));

        // `record` on a new series id while full writes DIRECTLY instead of dropping.
        record_with(&buf, &pool, "42").await.unwrap();
        assert_eq!(
            counts_for(&pool, "42").await.total,
            1,
            "fallback wrote through immediately"
        );
    }

    #[tokio::test]
    async fn failed_flush_keeps_views_for_retry() {
        // Flush into a pool with NO tables (migrations never ran): the write fails,
        // and the drained views must be merged back rather than lost.
        let broken = Database::connect("sqlite::memory:").await.unwrap();
        let buf = ViewBuffer::new();
        record_with(&buf, &broken, "42").await.unwrap();
        record_with(&buf, &broken, "42").await.unwrap();
        assert!(flush_buffer(&buf, &broken).await.is_err());

        // Views recorded between the drain and the retry are summed, not clobbered.
        record_with(&buf, &broken, "42").await.unwrap();

        // The retry (against a healthy pool) lands all three.
        let pool = pool().await;
        assert_eq!(flush_buffer(&buf, &pool).await.unwrap(), 1);
        assert_eq!(counts_for(&pool, "42").await.total, 3);
    }
}
