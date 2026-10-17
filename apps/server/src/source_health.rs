//! Per-source request health — WHY upstream requests fail (migration 0110).
//!
//! The scanner's own bookkeeping (`series_scan_state`, `catalog::source_scan_health`)
//! says that a series is failing and whether Suwayomi fell back to its cache. It does not
//! say why, and it never sees the requests a reader makes. This module is the missing
//! half: every upstream request is observed where it is made, its error text is sorted
//! into a [`Reason`], and the counts land in hourly buckets per source.
//!
//! Recording is an in-memory push (same shape as `views`): the request paths never wait
//! on SQLite's single writer, and a burst of page images collapses into a handful of
//! upserts per flush. The numbers are diagnostics, not accounting — a full buffer or a
//! lost flush drops counts rather than slowing a request.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::db::Database;
use anyhow::Result;

/// Retention for the hourly buckets — the widest window the admin surface asks for.
pub const BUCKET_RETENTION_HOURS: i64 = 24 * 7;

/// How often the background flusher drains the buffer. The admin panel is the only
/// reader, so half a minute of staleness costs nothing.
const FLUSH_INTERVAL_SECS: u64 = 30;

/// Cap on distinct buffered keys between flushes. Past it new keys are dropped (existing
/// ones keep counting), which bounds memory during a write outage.
const BUFFER_MAX_KEYS: usize = 8192;

/// Longest error text kept per bucket.
const MESSAGE_MAX_CHARS: usize = 240;

/// The `reason` stored for a successful request.
pub const REASON_OK: &str = "ok";

/// Which upstream request was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    /// A chapter-list fetch (the scanner, and the reader on a cache miss).
    Chapters,
    /// A reader opening a chapter: the page-list fetch.
    Pages,
    /// One page image proxied through the origin.
    Image,
    /// A source listing: discovery, source-sync, search, ingest.
    Browse,
}

impl Surface {
    fn as_str(self) -> &'static str {
        match self {
            Self::Chapters => "chapters",
            Self::Pages => "pages",
            Self::Image => "image",
            Self::Browse => "browse",
        }
    }
}

/// Who the request was for. Most request paths hold a Suwayomi manga id rather than a
/// source id; the manga → source lookup is deferred to flush time so recording stays a
/// pure in-memory push.
#[derive(Debug, Clone, Copy)]
pub enum Subject<'a> {
    Source(&'a str),
    Manga(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SubjectKey {
    Source(String),
    Manga(i64),
}

/// Why a request failed, sorted from the error text.
///
/// The split is by what the operator does next, not by HTTP status: `Blocked` means the
/// site refuses us, `Solver` means OUR FlareSolverr is the broken part, `NotFound` means
/// the site moved or the series is gone, `Extension` means the extension itself needs an
/// update, and `Engine` means Suwayomi — not the source — is the problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Blocked,
    RateLimited,
    Solver,
    NotFound,
    Timeout,
    Network,
    ServerError,
    Extension,
    Engine,
    Other,
}

impl Reason {
    /// Stable identifier for the DB column and the admin surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::RateLimited => "rate_limited",
            Self::Solver => "solver",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::ServerError => "server_error",
            Self::Extension => "extension",
            Self::Engine => "engine",
            Self::Other => "other",
        }
    }
}

/// The HTTP status a source answered with, read out of the error text. Extensions report
/// it as "HTTP error 403"; Suwayomi's own REST errors carry it as "error 500 …". Only a
/// three-digit number right after "error " counts, so a chapter number in a URL cannot be
/// mistaken for a status.
fn upstream_status(lower: &str) -> Option<u16> {
    let mut rest = lower;
    while let Some(at) = rest.find("http error ") {
        let tail = &rest[at + "http error ".len()..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() == 3 {
            return digits.parse().ok();
        }
        rest = tail;
    }
    None
}

/// Sort an upstream error into a [`Reason`].
///
/// Order matters. The source's own status ("HTTP error 403") is checked before the
/// generic words, because Suwayomi wraps a source failure in its own "error 500" and the
/// wrapper must not win. FlareSolverr is checked first of all: when the solver is down,
/// every message it produces also contains a network phrase, and "our solver is broken"
/// is the more useful answer.
pub fn classify(message: &str) -> Reason {
    let m = message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));

    if has(&["flaresolverr"]) {
        return Reason::Solver;
    }
    if has(&[
        "cloudflare",
        "captcha",
        "just a moment",
        "challenge",
        "access denied",
        "forbidden",
    ]) {
        return Reason::Blocked;
    }
    match upstream_status(&m) {
        Some(401 | 403 | 451) => return Reason::Blocked,
        Some(429) => return Reason::RateLimited,
        Some(404 | 410) => return Reason::NotFound,
        Some(500..=599) => return Reason::ServerError,
        _ => {}
    }
    if has(&["too many requests"]) {
        return Reason::RateLimited;
    }
    if has(&["timeout", "timed out"]) {
        return Reason::Timeout;
    }
    if has(&[
        "unable to resolve host",
        "unknownhostexception",
        "name resolution",
        "connection refused",
        "connection reset",
        "connectexception",
        "failed to connect",
        "sslhandshake",
        "sslexception",
        "unexpected end of stream",
    ]) {
        return Reason::Network;
    }
    // Suwayomi itself, not the source: our client's own errors and the engine's HTTP
    // failures. Checked before the extension words because "error decoding response
    // body" is ours, while a bare "Exception" is the extension's.
    if has(&[
        "error sending request",
        "error decoding response",
        "suwayomi returned no",
    ]) || m.starts_with("suwayomi error")
    {
        return Reason::Engine;
    }
    if has(&["source not found", "no source", "extension"]) {
        return Reason::Extension;
    }
    if has(&["not found"]) {
        return Reason::NotFound;
    }
    // A Java exception that is none of the network ones above is the extension failing
    // to read the page it got: the site changed its markup or its API.
    if has(&["exception", "unexpected", "failed to parse", "expected "]) {
        return Reason::Extension;
    }
    Reason::Other
}

#[derive(Debug, Clone)]
struct Entry {
    count: i64,
    last_message: Option<String>,
    last_at: String,
}

type Key = (SubjectKey, Surface, i64, &'static str);

struct Buffer {
    pending: Mutex<HashMap<Key, Entry>>,
}

impl Buffer {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Lock, tolerating poison — the map holds plain counts, always consistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Entry>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn push(
        &self,
        subject: Subject<'_>,
        surface: Surface,
        reason: &'static str,
        message: Option<&str>,
    ) {
        let now = chrono::Utc::now();
        let subject = match subject {
            Subject::Source(id) => SubjectKey::Source(id.to_string()),
            Subject::Manga(id) => SubjectKey::Manga(id),
        };
        let key = (subject, surface, now.timestamp() / 3600, reason);
        let message = message.map(|m| m.trim().chars().take(MESSAGE_MAX_CHARS).collect::<String>());
        let mut m = self.lock();
        if let Some(e) = m.get_mut(&key) {
            e.count += 1;
            e.last_at = now.to_rfc3339();
            if message.is_some() {
                e.last_message = message;
            }
            return;
        }
        if m.len() >= BUFFER_MAX_KEYS {
            return;
        }
        m.insert(
            key,
            Entry {
                count: 1,
                last_message: message,
                last_at: now.to_rfc3339(),
            },
        );
    }

    fn drain(&self) -> HashMap<Key, Entry> {
        std::mem::take(&mut *self.lock())
    }
}

/// The process-wide buffer behind [`ok`] / [`fail`] / [`observe`].
static GLOBAL_BUFFER: LazyLock<Buffer> = LazyLock::new(Buffer::new);

/// Record one successful upstream request.
pub fn ok(subject: Subject<'_>, surface: Surface) {
    GLOBAL_BUFFER.push(subject, surface, REASON_OK, None);
}

/// Record one failed upstream request, sorted by its error text.
pub fn fail(subject: Subject<'_>, surface: Surface, error: &dyn std::fmt::Display) {
    let message = error.to_string();
    GLOBAL_BUFFER.push(
        subject,
        surface,
        classify(&message).as_str(),
        Some(&message),
    );
}

/// Record the outcome of a request from its `Result`, whatever it carries.
pub fn observe<T, E: std::fmt::Display>(
    subject: Subject<'_>,
    surface: Surface,
    result: &std::result::Result<T, E>,
) {
    match result {
        Ok(_) => ok(subject, surface),
        Err(e) => fail(subject, surface, e),
    }
}

/// Drain the global buffer and persist it. Called by the flusher every tick and once
/// more on graceful shutdown.
pub async fn flush(pool: &Database) -> Result<usize> {
    flush_buffer(&GLOBAL_BUFFER, pool).await
}

async fn flush_buffer(buf: &Buffer, pool: &Database) -> Result<usize> {
    let drained = buf.drain();
    if drained.is_empty() {
        return Ok(0);
    }
    crate::db::retry_transaction(|| persist_buffer(pool, drained.clone())).await
}

async fn persist_buffer(pool: &Database, drained: HashMap<Key, Entry>) -> Result<usize> {
    // Manga → source, once per distinct manga (reads, outside the write transaction). A
    // manga with no `source_series` row is one we never enrolled (a browse preview); it
    // has no source to charge, so its counts are dropped.
    let mut source_of: HashMap<i64, Option<String>> = HashMap::new();
    for (subject, ..) in drained.keys() {
        if let SubjectKey::Manga(id) = subject {
            if !source_of.contains_key(id) {
                let source: Option<String> = crate::db_types::query_scalar(
                    "SELECT COALESCE(source_id, '') FROM source_series \
                      WHERE source_type = 'suwayomi' AND source_key = $1 LIMIT 1",
                )
                .bind(id.to_string())
                .fetch_optional(pool)
                .await?;
                source_of.insert(*id, source.filter(|s| !s.is_empty()));
            }
        }
    }
    // Fold to (source, surface, hour, reason): many mangas of one source become one row.
    let mut rows: HashMap<(String, Surface, i64, &'static str), Entry> = HashMap::new();
    for ((subject, surface, hour, reason), entry) in drained {
        let source = match subject {
            SubjectKey::Source(id) => id,
            SubjectKey::Manga(id) => match source_of.get(&id).cloned().flatten() {
                Some(s) => s,
                None => continue,
            },
        };
        match rows.get_mut(&(source.clone(), surface, hour, reason)) {
            Some(e) => {
                e.count += entry.count;
                if crate::db_types::date(&entry.last_at)? > crate::db_types::date(&e.last_at)? {
                    e.last_at = entry.last_at;
                    if entry.last_message.is_some() {
                        e.last_message = entry.last_message;
                    }
                }
            }
            None => {
                rows.insert((source, surface, hour, reason), entry);
            }
        }
    }
    let n = rows.len();
    let mut tx = pool.begin().await?;
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_by(|a, b| {
        (a.0 .0.as_str(), a.0 .1.as_str(), a.0 .2, a.0 .3).cmp(&(
            b.0 .0.as_str(),
            b.0 .1.as_str(),
            b.0 .2,
            b.0 .3,
        ))
    });
    for ((source, surface, hour, reason), e) in &rows {
        sqlx::query(
            "INSERT INTO source_health_bucket \
               (source_id, surface, hour_ts, reason, count, last_message, last_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT(source_id, surface, hour_ts, reason) DO UPDATE SET \
               count = source_health_bucket.count + excluded.count, \
               last_message = COALESCE(excluded.last_message, source_health_bucket.last_message), \
               last_at = GREATEST(source_health_bucket.last_at, excluded.last_at)",
        )
        .bind(source)
        .bind(surface.as_str())
        .bind(hour)
        .bind(reason)
        .bind(e.count)
        .bind(&e.last_message)
        .bind(crate::db_types::date(&e.last_at)?)
        .execute(&mut *tx)
        .await?;
        #[cfg(test)]
        crate::catalog::guards::held().await;
    }
    tx.commit().await?;
    Ok(n)
}

/// Spawn the background flusher: drains the buffer every [`FLUSH_INTERVAL_SECS`], and
/// once more when the shutdown watch flips. Panic-supervised like the other loops.
pub fn spawn_flusher(
    pool: Database,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(crate::task::supervise(
        "source-health-flush",
        Duration::from_secs(30),
        shutdown.clone(),
        move || {
            crate::db::Database::job_scope(
                crate::database::ParallelJob::SourceHealthFlush,
                run_flush_loop(pool.clone(), shutdown.clone()),
            )
        },
    ))
}

async fn run_flush_loop(pool: Database, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let stopping = tokio::select! {
            biased;
            _ = shutdown.changed() => true,
            _ = tick.tick() => *shutdown.borrow(),
        };
        // A failed flush loses that interval's counts on purpose: these are diagnostics,
        // and re-queueing them would grow the buffer for as long as the DB is unhappy.
        if let Err(e) = flush(&pool).await {
            let e = crate::db::observed_database_error(e);
            tracing::warn!(error = %e, "source health flush failed; dropped one interval");
        }
        if stopping {
            return;
        }
    }
}

/// Delete buckets past [`BUCKET_RETENTION_HOURS`]. Called by the GC sweep.
pub async fn prune_buckets(pool: &Database) -> Result<u64> {
    let cutoff = chrono::Utc::now().timestamp() / 3600 - BUCKET_RETENTION_HOURS;
    // Tests retain the statement's locks until the overlapping writer has started.
    #[cfg(test)]
    let mut tx = pool.begin().await?;
    let query = sqlx::query("DELETE FROM source_health_bucket WHERE hour_ts <= $1").bind(cutoff);
    #[cfg(not(test))]
    let r = query.execute(pool).await?;
    #[cfg(test)]
    let r = query.execute(&mut *tx).await?;
    #[cfg(test)]
    {
        crate::catalog::guards::held().await;
        tx.commit().await?;
    }
    Ok(r.rows_affected())
}

/// One (source, surface, reason) total over a window.
#[derive(Debug, Clone)]
pub struct Stat {
    pub source_id: String,
    pub surface: String,
    /// [`REASON_OK`] or a [`Reason`] identifier.
    pub reason: String,
    pub count: i64,
    /// The newest error text for this reason in the window; `None` for `ok`.
    pub last_message: Option<String>,
    pub last_at: String,
}

impl sqlx::FromRow<'_, sqlx::postgres::PgRow> for Stat {
    fn from_row(row: &sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use crate::db_types::Row;
        Ok(Self {
            source_id: row.try_get("source_id")?,
            surface: row.try_get("surface")?,
            reason: row.try_get("reason")?,
            count: row.try_get("count")?,
            last_message: row.try_get("last_message")?,
            last_at: row.try_get("last_at")?,
        })
    }
}

/// Totals for the last `hours` hours, one row per (source, surface, reason).
///
/// Select the newest bucket's message together with its timestamp. Equal clocks
/// use the earliest hourly bucket; totals include every bucket in the window.
pub async fn stats(pool: &Database, hours: i64) -> Result<Vec<Stat>> {
    let since = chrono::Utc::now().timestamp() / 3600 - hours.clamp(1, BUCKET_RETENTION_HOURS);
    Ok(sqlx::query_as::<_, Stat>(
        "SELECT source_id, surface, reason, count, last_message, last_at FROM ( \
            SELECT source_id, surface, reason, \
                   SUM(count) OVER (PARTITION BY source_id, surface, reason)::bigint AS count, \
                   last_message, last_at, \
                   ROW_NUMBER() OVER (PARTITION BY source_id, surface, reason \
                                      ORDER BY last_at DESC NULLS LAST, hour_ts ASC) AS rank \
              FROM source_health_bucket WHERE hour_ts > $1 \
         ) ranked WHERE rank = 1 ORDER BY source_id, surface, reason",
    )
    .bind(since)
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_sorts_real_upstream_messages() {
        for (message, want) in [
            ("HTTP error 403", Reason::Blocked),
            ("Failed to bypass Cloudflare", Reason::Blocked),
            ("HTTP error 429", Reason::RateLimited),
            ("HTTP error 404", Reason::NotFound),
            ("HTTP error 503", Reason::ServerError),
            (
                "flaresolverr: Temporary failure in name resolution",
                Reason::Solver,
            ),
            ("java.net.SocketTimeoutException: timeout", Reason::Timeout),
            ("Unable to resolve host \"example.org\"", Reason::Network),
            ("Suwayomi error 502 Bad Gateway", Reason::Engine),
            (
                "error sending request for url (http://127.0.0.1:4567/api/graphql)",
                Reason::Engine,
            ),
            ("java.lang.NullPointerException", Reason::Extension),
            ("Source not found", Reason::Extension),
            ("something else entirely", Reason::Other),
        ] {
            assert_eq!(classify(message), want, "{message}");
        }
    }

    #[test]
    fn the_source_status_wins_over_the_engine_wrapper_around_it() {
        // Suwayomi answers its REST image route with its own 500 and the source's error in
        // the body; the reason must be the source's.
        assert_eq!(
            classify("Suwayomi page error 500 Internal Server Error: HTTP error 403"),
            Reason::Blocked
        );
        // …and a chapter number is not a status.
        assert_eq!(upstream_status("http error at chapter 404"), None);
    }

    #[tokio::test]
    async fn source_health_flush_and_gc_prune_the_same_bucket_in_both_orders() {
        use crate::catalog::phase4_tests::{overlap, pool};

        for flush_first in [true, false] {
            let pool = pool().await;
            let recent = chrono::Utc::now().timestamp() / 3600;
            let old = recent - BUCKET_RETENTION_HOURS - 24;
            let before = "2020-01-01T00:00:00Z";
            let after = "2020-01-02T00:00:00Z";
            for (hour, count) in [(old, 5_i64), (recent, 2)] {
                sqlx::query("INSERT INTO source_health_bucket(source_id,surface,hour_ts,reason,count,last_message,last_at) VALUES ('src-a','pages',$1,'ok',$2,'before',$3)")
                    .bind(hour).bind(count).bind(crate::db_types::date(before).unwrap())
                    .execute(&pool).await.unwrap();
            }
            let flush = move |pool: Database| async move {
                let buffer = Buffer::new();
                for hour in [old, recent] {
                    buffer.lock().insert(
                        (
                            SubjectKey::Source("src-a".into()),
                            Surface::Pages,
                            hour,
                            REASON_OK,
                        ),
                        Entry {
                            count: 1,
                            last_message: Some("after".into()),
                            last_at: after.into(),
                        },
                    );
                }
                flush_buffer(&buffer, &pool).await.map(|_| ())
            };
            let prune = |pool: Database| async move { prune_buckets(&pool).await.map(|_| ()) };
            let (a, b) = if flush_first {
                overlap(&pool, flush, prune).await
            } else {
                overlap(&pool, prune, flush).await
            };
            a.unwrap();
            b.unwrap();
            type BucketRow = (
                String,
                String,
                i64,
                String,
                i64,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
            );
            let rows: Vec<BucketRow> = sqlx::query_as(
                "SELECT source_id,surface,hour_ts,reason,count,last_message,last_at FROM source_health_bucket ORDER BY hour_ts",
            ).fetch_all(&pool).await.unwrap();
            let expected = if flush_first {
                vec![(recent, 3)]
            } else {
                vec![(old, 1), (recent, 3)]
            };
            assert_eq!(rows.len(), expected.len());
            for (row, (hour, count)) in rows.into_iter().zip(expected) {
                assert_eq!(
                    (row.0.as_str(), row.1.as_str(), row.2, row.3.as_str(), row.4),
                    ("src-a", "pages", hour, "ok", count)
                );
                assert_eq!(row.5.as_deref(), Some("after"));
                assert_eq!(row.6, crate::db_types::date(after).unwrap());
            }
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn source_health_flush_retries_an_external_writer() {
        let pool = Database::connect("sqlite::memory:").await.unwrap();

        let external = Database::connect_with((*pool.connect_options()).clone(), 2)
            .await
            .unwrap();
        let mut held = external.begin().await.unwrap();
        sqlx::query("LOCK TABLE source_health_bucket IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *held)
            .await
            .unwrap();
        let buf = std::sync::Arc::new(Buffer::new());
        buf.push(Subject::Source("src-a"), Surface::Pages, REASON_OK, None);
        let writer = pool.clone();
        let worker_buf = buf.clone();
        let worker = tokio::spawn(async move { flush_buffer(&worker_buf, &writer).await });
        tokio::time::sleep(Duration::from_millis(350)).await;
        held.commit().await.unwrap();
        assert_eq!(worker.await.unwrap().unwrap(), 1);
        assert_eq!(stats(&pool, 24).await.unwrap()[0].count, 1);
        external.close().await;
        pool.close().await;
    }

    #[tokio::test]
    async fn flush_charges_a_manga_to_its_source_and_stats_sum_the_window() {
        let pool = Database::connect("sqlite::memory:").await.unwrap();

        sqlx::raw_sql("INSERT INTO work(id,primary_title,created_at,updated_at) VALUES ('w_health','Health','2020-01-01','2020-01-01'); INSERT INTO source_series(id,work_id,source_type,source_key,source_id,created_at) VALUES ('ss_health','w_health','suwayomi','7','src-a','2020-01-01')")
            .execute(&pool)
            .await
            .unwrap();

        let buf = Buffer::new();
        buf.push(Subject::Manga(7), Surface::Pages, REASON_OK, None);
        buf.push(
            Subject::Manga(7),
            Surface::Pages,
            "blocked",
            Some("HTTP error 403"),
        );
        buf.push(
            Subject::Source("src-a"),
            Surface::Pages,
            "blocked",
            Some("HTTP error 403"),
        );
        // Not enrolled: no source to charge, so it is dropped rather than stored under ''.
        buf.push(
            Subject::Manga(99),
            Surface::Pages,
            "blocked",
            Some("HTTP error 403"),
        );
        flush_buffer(&buf, &pool).await.unwrap();

        let mut got = stats(&pool, 24).await.unwrap();
        got.sort_by(|a, b| a.reason.cmp(&b.reason));
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].reason.as_str(), got[0].count), ("blocked", 2));
        assert_eq!(got[0].source_id, "src-a");
        assert_eq!(got[0].last_message.as_deref(), Some("HTTP error 403"));
        assert_eq!((got[1].reason.as_str(), got[1].count), ("ok", 1));
    }
}
