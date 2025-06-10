use std::str::FromStr;
use std::time::Duration;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

/// True if `e` is a transient SQLite lock/BUSY error worth retrying, rather than a
/// real failure.
///
/// This covers TWO distinct conditions that both surface with "database is locked":
///   - plain `SQLITE_BUSY` (code 5) — another writer holds the lock; `busy_timeout`
///     normally absorbs this, and seeing it means the timeout was exhausted.
///   - `SQLITE_BUSY_SNAPSHOT` (code 517) — a DEFERRED transaction read first, then
///     tried to upgrade to a write after another writer had already committed.
///     `busy_timeout` does not and structurally CANNOT retry this one: the
///     transaction's read snapshot is stale, so the only correct recovery is to
///     abort and re-run the whole transaction. Callers must retry, not just wait.
///
/// Matching on the message rather than a code keeps this working across the
/// sqlx/libsqlite error wrapping used by the different call sites.
pub fn is_locked_error(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("database is locked") || s.contains("database table is locked")
}

/// Open (creating if needed) the SQLite database and run migrations.
///
/// WAL journaling is required for Litestream continuous backup (it ships the WAL
/// to object storage) and lets the background scan scheduler write without
/// blocking readers; `synchronous = NORMAL` is the WAL-safe durability level, and
/// a busy timeout absorbs the brief writer contention from scanner ticks.
pub async fn init(database_url: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        // 15s (was 5s) absorbs longer write stalls when the catalogue sync, cover
        // drainer and scan scheduler contend for SQLite's single writer during a
        // full seed. All PRAGMAs below are per-connection and invisible to Litestream
        // (which only ships the WAL) — the memory-map + ~16 MB page cache serve the
        // read-heavy catalogue from RAM, and temp B-trees (sorts, GROUP BY) stay off
        // disk.
        //
        // mmap_size is 1.5 GiB, up from 256 MB: the main DB is ~1,375 MB, so a quarter
        // of it was mmap-able and the rest paid a read() + page-cache copy per page.
        // This is address space, not an allocation — only pages actually touched become
        // resident — and the host has 23.4 GB of RAM, so covering the whole file costs
        // nothing and removes the copy for the catalogue-wide scans (`search_catalogue`
        // count, ANALYZE, feed/FTS rebuilds) that read most of it.
        .busy_timeout(Duration::from_secs(15))
        .pragma("mmap_size", "1610612736")
        .pragma("cache_size", "-16000")
        .pragma("temp_store", "MEMORY");
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Open (creating if needed) the SEPARATE cover-cache DB and ensure its schema.
///
/// This DB holds only `work_cover_blob` (large, re-derivable-from-MangaDex cover
/// thumbnails). It is deliberately NOT the main DB: Litestream replicates only the
/// main DB to R2, so keeping covers here excludes them from backup while they're
/// still served from our own origin. Schema is a single table, created inline (no
/// migration directory) — there is no FK to `work` because that table lives in the
/// other DB; an orphaned blob (its work deleted) is harmless and re-derivable.
pub async fn init_covers(database_url: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        // Match the main pool's contention margin + per-connection cache tuning
        // (see `init`); the cover DB is point-lookup heavy but shares the writer
        // stalls during a cover-materialization burst.
        //
        // mmap_size stays at 256 MB here, deliberately unlike the main pool. This DB is
        // 20.5 GB of ~170 KB blobs read once and streamed to the client; mapping them
        // would push single-use blob pages through the page cache at the expense of
        // everything else. 256 MB comfortably holds the PK index (121k keys) and the
        // hot end of the table, which is all that benefits.
        .busy_timeout(Duration::from_secs(15))
        .pragma("mmap_size", "268435456")
        .pragma("cache_size", "-16000")
        .pragma("temp_store", "MEMORY");
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_cover_blob (\
             work_id    TEXT PRIMARY KEY,\
             webp       BLOB NOT NULL,\
             version    INTEGER NOT NULL,\
             updated_at TEXT NOT NULL\
         )",
    )
    .execute(&pool)
    .await?;
    // Resized, origin-cached Suwayomi SOURCE covers, keyed by numeric Suwayomi manga
    // id. The discovery/browse/search feeds hand back the raw `/api/v1/manga/{id}/
    // thumbnail` proxy path (full-resolution, up to several MB), so `serve_suwayomi_image`
    // downscales each cover to a bounded WebP on first request and caches it here.
    // Same rationale as `work_cover_blob`: large, re-derivable, and deliberately
    // excluded from Litestream backup.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS suwayomi_cover_blob (\
             manga_id   INTEGER PRIMARY KEY,\
             webp       BLOB NOT NULL,\
             updated_at TEXT NOT NULL\
         )",
    )
    .execute(&pool)
    .await?;
    // Extension icons Keiyoushi published AFTER the vendored snapshot in
    // `assets/ext-icons/` was taken (refreshed by `scripts/fetch-ext-icons.mjs`).
    // The baked-in set is authoritative and answers essentially every request;
    // this table only backfills extensions added upstream since, so the icons
    // don't go stale between snapshot refreshes. Same rationale as the cover
    // tables for living here: re-derivable from Keiyoushi, so deliberately
    // excluded from Litestream backup.
    //
    // An EMPTY `png` is a tombstone, not an icon: ~41 extensions publish no icon
    // in either source, and without a recorded miss every request for one would
    // re-fetch from Keiyoushi. `serve_ext_icon` serves 404 for a tombstone and
    // only re-checks upstream once it ages past its TTL.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS extension_icon_blob (\
             pkg_name   TEXT PRIMARY KEY,\
             png        BLOB NOT NULL,\
             updated_at TEXT NOT NULL\
         )",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

/// Tables whose `sqlite_stat1` rows the planner actually depends on, and whose row
/// counts / key distributions move enough to go stale.
///
/// `work_fts` is deliberately absent: `ANALYZE` skips virtual tables, so naming it is a
/// silent no-op.
const ANALYZE_TABLES: &[&str] = &[
    "feed_updates",
    "source_series",
    "suwayomi_series",
    "series_scan_state",
    "merge_candidate",
    "notifications",
    // Browse's own table (migration 0069) and the updates feed's (0064). Both are
    // wholesale-rebuilt on every sync cycle, so their stats go stale by construction.
    //
    // `browse_catalogue` matters most: it is the largest table the planner has to choose
    // an index for (115,567 rows across eight indices), and 0069's query plans were
    // verified on an ANALYZEd copy. Without stat rows the planner falls back to its
    // ~1M-row default and can miss the intended index for a filter combination —
    // reintroducing exactly the temp-B-tree sort over the whole table that those eight
    // indices exist to prevent. That is the regression this list's own doc comment
    // describes ("leaving the newest tables with no stat rows at all"), repeated on the
    // newest table again.
    "browse_catalogue",
    "feed_series_updates",
];

/// How often stats are refreshed. Well under the rate at which these tables change
/// shape, and cheap enough that the interval is set by "don't be stale", not by cost.
///
/// Measured on a copy of the production DB: 125 ms for the whole list with the pages
/// warm, 4.6 s with the page cache evicted (feed_updates 3.5 s + source_series 1.0 s
/// dominate). `ANALYZE` holds a write transaction on `sqlite_stat1` for the duration of
/// each table, so the worst case is a ~3.5 s write stall once every 6 h — comfortably
/// inside the pool's 15 s `busy_timeout`, which is why the scanner riding through it is
/// a wait rather than a failure. The 300 s boot offset below keeps even the cold pass
/// clear of the boot burst.
const ANALYZE_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// Spawn the recurring statistics refresh.
///
/// Migration 0058 runs a targeted `ANALYZE` once, but nothing refreshed it afterwards —
/// which is how production ended up with `sqlite_stat1` written when `_sqlx_migrations`
/// held only 48 rows, leaving the newest tables with no stat rows at all and the planner
/// assuming ~1M rows for each of them.
///
/// This runs exact per-table `ANALYZE`, NOT `PRAGMA optimize`. `PRAGMA optimize` on a DB
/// this size needs `PRAGMA analysis_limit` to be affordable (a bare `ANALYZE` scans
/// everything), and a limited analysis pins "average rows per key" at the sample limit:
/// measured, it reports `source_series.source_type` as selecting 401 of 123k rows rather
/// than the true 61,515. Those are exactly the selectivities 0058's new indexes are
/// chosen on, so a sampled optimize would overwrite good stats with wrong ones and could
/// de-select `idx_source_series_type_created`. Per-table `ANALYZE` has no such limit —
/// and confined to this list it costs ~70 ms, so there is nothing to buy with sampling.
pub fn spawn_analyze(pool: SqlitePool, shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(crate::task::supervise(
        "analyze",
        Duration::from_secs(60),
        shutdown.clone(),
        move || analyze_loop(pool.clone(), shutdown.clone()),
    ));
}

async fn analyze_loop(pool: SqlitePool, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    // 300s behind boot: the last slot in the background-loop stagger (scanner at 0,
    // feed/FTS rebuild at 20s, mangadex at 60s, GC at 150s), so refreshing stats never
    // lands inside the boot write burst.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(300),
        Duration::from_secs(ANALYZE_INTERVAL_SECS),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        interval_secs = ANALYZE_INTERVAL_SECS,
        "query-statistics refresh started"
    );
    loop {
        tokio::select! {
            _ = ticker.tick() => refresh_statistics(&pool).await,
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("query-statistics refresh stopping");
                    break;
                }
            }
        }
    }
}

/// One `ANALYZE` pass over [`ANALYZE_TABLES`]. Best-effort per table: a missing table or
/// a transient lock must not abort the rest of the pass or kill the loop.
async fn refresh_statistics(pool: &SqlitePool) {
    let started = std::time::Instant::now();
    let mut ok = 0usize;
    for table in ANALYZE_TABLES {
        // `table` is a compile-time constant from the list above, never user input.
        match sqlx::query(&format!("ANALYZE {table}")).execute(pool).await {
            Ok(_) => ok += 1,
            Err(e) => tracing::warn!(table, error = %e, "analyze: table failed"),
        }
    }
    tracing::info!(
        tables = ok,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "analyze: query statistics refreshed"
    );
}
