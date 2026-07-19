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

use anyhow::Result;
use sqlx::SqlitePool;

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
pub async fn flush(pool: &SqlitePool) -> Result<usize> {
    flush_buffer(&GLOBAL_BUFFER, pool).await
}

async fn flush_buffer(buf: &Buffer, pool: &SqlitePool) -> Result<usize> {
    let drained = buf.drain();
    if drained.is_empty() {
        return Ok(0);
    }
    // Manga → source, once per distinct manga (reads, outside the write transaction). A
    // manga with no `source_series` row is one we never enrolled (a browse preview); it
    // has no source to charge, so its counts are dropped.
    let mut source_of: HashMap<i64, Option<String>> = HashMap::new();
    for (subject, ..) in drained.keys() {
        if let SubjectKey::Manga(id) = subject {
            if !source_of.contains_key(id) {
                let source: Option<String> = sqlx::query_scalar(
                    "SELECT source_id FROM source_series \
                      WHERE source_type = 'suwayomi' AND source_key = ? LIMIT 1",
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
                if entry.last_at > e.last_at {
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
    for ((source, surface, hour, reason), e) in &rows {
        sqlx::query(
            "INSERT INTO source_health_bucket \
               (source_id, surface, hour_ts, reason, count, last_message, last_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(source_id, surface, hour_ts, reason) DO UPDATE SET \
               count = count + excluded.count, \
               last_message = COALESCE(excluded.last_message, last_message), \
               last_at = MAX(last_at, excluded.last_at)",
        )
        .bind(source)
        .bind(surface.as_str())
        .bind(hour)
        .bind(reason)
        .bind(e.count)
        .bind(&e.last_message)
        .bind(&e.last_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(n)
}

/// Spawn the background flusher: drains the buffer every [`FLUSH_INTERVAL_SECS`], and
/// once more when the shutdown watch flips. Panic-supervised like the other loops.
pub fn spawn_flusher(
    pool: SqlitePool,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(crate::task::supervise(
        "source-health-flush",
        Duration::from_secs(30),
        shutdown.clone(),
        move || run_flush_loop(pool.clone(), shutdown.clone()),
    ))
}

async fn run_flush_loop(pool: SqlitePool, mut shutdown: tokio::sync::watch::Receiver<bool>) {
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
            tracing::warn!(error = %e, "source health flush failed; dropped one interval");
        }
        if stopping {
            return;
        }
    }
}

/// Delete buckets past [`BUCKET_RETENTION_HOURS`]. Called by the GC sweep.
pub async fn prune_buckets(pool: &SqlitePool) -> Result<u64> {
    let cutoff = chrono::Utc::now().timestamp() / 3600 - BUCKET_RETENTION_HOURS;
    let r = sqlx::query("DELETE FROM source_health_bucket WHERE hour_ts <= ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// One (source, surface, reason) total over a window.
#[derive(Debug, Clone, sqlx::FromRow)]
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

/// Totals for the last `hours` hours, one row per (source, surface, reason).
///
/// `last_message` rides on the single `MAX(last_at)`: with exactly one min/max aggregate
/// SQLite takes the bare columns from the row that holds it, i.e. the newest bucket.
pub async fn stats(pool: &SqlitePool, hours: i64) -> Result<Vec<Stat>> {
    let since = chrono::Utc::now().timestamp() / 3600 - hours.clamp(1, BUCKET_RETENTION_HOURS);
    Ok(sqlx::query_as::<_, Stat>(
        "SELECT source_id, surface, reason, SUM(count) AS count, \
                last_message, MAX(last_at) AS last_at \
           FROM source_health_bucket \
          WHERE hour_ts > ? \
          GROUP BY source_id, surface, reason",
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
    async fn flush_charges_a_manga_to_its_source_and_stats_sum_the_window() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE source_series (source_type TEXT, source_key TEXT, source_id TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0110_source_health.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO source_series VALUES ('suwayomi', '7', 'src-a')")
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
