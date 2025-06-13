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

use anyhow::Result;
use sqlx::SqlitePool;

/// Retention for hourly buckets — the widest query window (7 days). Older buckets are
/// swept by [`prune_buckets`]; the all-time `series_views` total is never pruned.
pub const BUCKET_RETENTION_HOURS: i64 = 24 * 7;

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
pub async fn view_key(pool: &SqlitePool, series_id: &str) -> String {
    if !series_id.is_empty() && series_id.bytes().all(|b| b.is_ascii_digit()) {
        return series_id.to_string();
    }
    let numeric: Option<String> = sqlx::query_scalar(
        "SELECT source_key FROM source_series \
         WHERE work_id = ? AND source_type = 'suwayomi' \
         ORDER BY CAST(source_key AS INTEGER) LIMIT 1",
    )
    .bind(series_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    numeric.unwrap_or_else(|| series_id.to_string())
}

/// Record one view for `series_id`. Idempotency is intentionally NOT enforced — every
/// chapter open counts (see module docs). Bumps the all-time total and the current
/// hourly bucket in one transaction.
pub async fn record(pool: &SqlitePool, series_id: &str) -> Result<()> {
    let key = view_key(pool, series_id).await;
    let now = chrono::Utc::now().to_rfc3339();
    let hour = current_hour();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO series_views (series_key, total, updated_at) VALUES (?, 1, ?) \
         ON CONFLICT(series_key) DO UPDATE SET \
           total = total + 1, updated_at = excluded.updated_at",
    )
    .bind(&key)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES (?, ?, 1) \
         ON CONFLICT(series_key, hour_ts) DO UPDATE SET views = views + 1",
    )
    .bind(&key)
    .bind(hour)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Load the all-time / 7-day / 24-hour view counts for one series (resolves its key).
///
/// One aggregate query folds the all-time `total` (from `series_views`) with both hourly
/// windows (from `series_view_bucket`): the 24h and 7d sums come from the same scan, gated
/// by per-row `CASE`s. Since 7d ⊇ 24h, the row filter bounds to the 7d cutoff and the 24h
/// `CASE` narrows within it. This is 2 awaits (`view_key` + this) instead of the former 4 —
/// every `Series.views` resolver in a feed pays the difference per item.
pub async fn counts_for(pool: &SqlitePool, series_id: &str) -> ViewCounts {
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
pub async fn trending_keys(pool: &SqlitePool, limit: i64) -> Result<Vec<(String, i64)>> {
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
pub async fn prune_buckets(pool: &SqlitePool) -> Result<u64> {
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

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn records_and_counts_across_windows() {
        let pool = pool().await;
        // Three views of a bare (un-catalogued) numeric series.
        for _ in 0..3 {
            record(&pool, "42").await.unwrap();
        }
        let c = counts_for(&pool, "42").await;
        assert_eq!(c.total, 3);
        assert_eq!(c.last24h, 3);
        assert_eq!(c.last7d, 3);

        // A different series with one view ranks below the first in trending.
        record(&pool, "7").await.unwrap();
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
        record(&pool, "42").await.unwrap();
        let h30 = current_hour() - 30;
        sqlx::query("UPDATE series_view_bucket SET hour_ts = ? WHERE series_key = '42'")
            .bind(h30)
            .execute(&pool)
            .await
            .unwrap();
        // A second, fresh (current-hour) view.
        record(&pool, "42").await.unwrap();

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
        record(&pool, "42").await.unwrap();
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
}
