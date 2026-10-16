//! DB cache for Suwayomi source-series metadata + chapter lists (S1).
//!
//! The reader's Suwayomi paths (`series`, `chapters`, `updates`, `discovery`) used
//! to live-fetch from Suwayomi on every request (each of which fetches the upstream
//! source over the network — the latency the user hit). These helpers persist the
//! raw Suwayomi shapes into `suwayomi_series` / `suwayomi_chapter` on scan + ingest,
//! and read them back, so subsequent loads are served from SQLite. Only the cover
//! REFERENCE is stored (Worker-proxied) — never cover bytes (memory posture).

use crate::db::Database;
use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::suwayomi::{ChapterCount, SuwayomiChapter, SuwayomiManga, SuwayomiSourceLang};

/// TTL for cached series METADATA before a cache hit revalidates upstream. Series
/// metadata (title/status/description/cover) changes slowly, so a generous window.
pub const SERIES_TTL_SECS: i64 = 6 * 60 * 60; // 6h
/// TTL for a cached chapter list before a cache hit revalidates upstream. Chapters
/// arrive more often than metadata changes, so a tighter window.
pub const CHAPTERS_TTL_SECS: i64 = 90 * 60; // 90m

/// Whether a cached row's last-fetch timestamp is still within `ttl_secs` of now.
/// A missing or unparseable timestamp is treated as STALE (forces a revalidation),
/// never as fresh — the safe direction for a freshness gate.
pub fn is_fresh(fetched_at: Option<&str>, ttl_secs: i64) -> bool {
    let Some(ts) = fetched_at else { return false };
    let Ok(t) = DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    Utc::now()
        .signed_duration_since(t.with_timezone(&Utc))
        .num_seconds()
        < ttl_secs
}

/// Upsert one series' display metadata into the cache. `chapter_count` prefers the
/// manga's own count, else keeps whatever is already stored (so a detail fetch
/// without a fresh count doesn't zero it).
/// Returns `Ok(true)` when the series was written, `Ok(false)` when it was REFUSED by
/// the English-only gate below. Callers that report counts (e.g. `persistCatalogue`)
/// must distinguish the two: a refusal is a successful no-op, not a persist, and the id
/// must not be queued for a follow-up chapter fetch whose results `put_chapters` would
/// then discard.
pub async fn put_series(pool: &Database, m: &SuwayomiManga) -> Result<bool> {
    crate::engine_generation::wait(pool).await?;
    let result = put_series_inner(pool, m).await;
    crate::engine_generation::observe_write(pool, result)
}

async fn put_series_inner(pool: &Database, m: &SuwayomiManga) -> Result<bool> {
    let lang = m.source.as_ref().and_then(|s| s.lang.clone());
    // English-only backstop. Komika serves English only, but this cache is the
    // persistence chokepoint EVERY Suwayomi path funnels through — reader re-cache
    // (`resolve_series_cached`), cold-browse discovery, federated search, admin adds,
    // the scanner. Caching a non-English series here is exactly what let those paths
    // RESURRECT rows that `purge_foreign_language_suwayomi` had deleted (the purge
    // isn't durable without this gate). Refuse the write so a non-English series never
    // re-enters `suwayomi_series` (and thus never re-appears in Browse). A None/unknown
    // lang is allowed through — don't drop a legitimate English series whose source
    // omitted a lang — matching the purge's `lang IS NOT NULL AND lang <> 'en'`.
    if lang.as_deref().is_some_and(|l| l != "en") {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    crate::engine_generation::shared(
        &mut tx,
        crate::engine_keys::token(m.engine_generation.as_deref())?,
    )
    .await?;
    crate::locks::advisory(&mut tx, &series_publication_keys(&m.id.to_string())).await?;
    crate::engine_keys::capture_manga_on(&mut tx, m).await?;
    let now = Utc::now().to_rfc3339();
    let count = m.chapters.as_ref().map(|c| c.total_count);
    // `series_fetched_at` records ONLY a metadata fetch (this call) — distinct from
    // `updated_at`, which `put_chapters` also bumps — so the reader's series-metadata
    // TTL revalidates on the right event.
    sqlx::query(
        "INSERT INTO suwayomi_series \
           (id, title, thumbnail_url, author, artist, description, genre, status, \
            in_library, in_library_at, last_fetched_at, source_id, lang, chapter_count, \
            updated_at, created_at, series_fetched_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, COALESCE($14, 0), $15, $16, $17) \
         ON CONFLICT(id) DO UPDATE SET \
           title = excluded.title, thumbnail_url = excluded.thumbnail_url, \
           author = excluded.author, artist = excluded.artist, description = excluded.description, \
           genre = excluded.genre, status = excluded.status, in_library = excluded.in_library, \
           in_library_at = excluded.in_library_at, last_fetched_at = excluded.last_fetched_at, \
           source_id = excluded.source_id, lang = excluded.lang, \
           chapter_count = COALESCE($18, suwayomi_series.chapter_count), \
           updated_at = excluded.updated_at, \
           created_at = COALESCE(suwayomi_series.created_at, excluded.created_at), \
           series_fetched_at = excluded.series_fetched_at",
    )
    .bind(m.id)
    .bind(&m.title)
    .bind(&m.thumbnail_url)
    .bind(&m.author)
    .bind(&m.artist)
    .bind(&m.description)
    .bind(sqlx::types::Json(&m.genre))
    .bind(&m.status)
    .bind(m.in_library)
    .bind(&m.in_library_at)
    .bind(&m.last_fetched_at)
    .bind(&m.source_id)
    .bind(&lang)
    .bind(count)
    .bind(crate::db_types::date(&now)?)
    .bind(crate::db_types::date(&now)?)
    .bind(crate::db_types::date(&now)?)
    .bind(count)
    .execute(&mut *tx)
    .await?;
    #[cfg(test)]
    if let Ok(points) = SERIES_PUBLICATION_PAUSE.try_with(Clone::clone) {
        points.hit("series-cache-written").await;
    }
    crate::engine_generation::commit_ready(pool)?;
    tx.commit().await?;
    Ok(true)
}

/// The mutable projection of one cached chapter row — everything `put_chapters`
/// would write. Two lists that compare equal under this shape produce a
/// byte-identical `suwayomi_chapter` table, so the rewrite can be skipped.
#[derive(sqlx::FromRow, PartialEq)]
struct ChapterDigestRow {
    #[sqlx(try_from = "crate::db_types::DbI64")]
    id: i64,
    #[sqlx(try_from = "crate::db_types::DbString")]
    name: String,
    chapter_number: f64,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    scanlator: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    upload_date: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    is_read: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    is_bookmarked: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    is_downloaded: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    last_page_read: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    page_count: i64,
}

impl ChapterDigestRow {
    /// Project an incoming source chapter into the stored shape, so a fetched list
    /// can be compared field-for-field against what is already cached.
    fn of(c: &SuwayomiChapter) -> Self {
        Self {
            id: c.id,
            name: c.name.clone(),
            chapter_number: c.chapter_number,
            scanlator: c.scanlator.clone(),
            upload_date: c.upload_date.clone(),
            is_read: c.is_read as i64,
            is_bookmarked: c.is_bookmarked as i64,
            is_downloaded: c.is_downloaded as i64,
            last_page_read: c.last_page_read,
            page_count: c.page_count,
        }
    }
}

/// In-process "when did we last FETCH this manga's chapters" memo.
///
/// `put_chapters` used to be the thing that recorded a fetch, because it rewrote
/// every row's `updated_at` unconditionally — which is exactly the write we now skip
/// when nothing changed. Keeping the marker in the DB would reintroduce a write per
/// scan (measured: a one-row commit still costs ~17 KiB of WAL, barely less than the
/// full 48-row rewrite it replaces — WAL cost is per-COMMIT, not per-row).
///
/// So the marker lives here instead. It is pure cache-freshness bookkeeping: losing it
/// on restart just means the next reader request revalidates once, which is the same
/// safe direction `is_fresh` already takes for a missing timestamp. `chapters_last_fetched`
/// returns the max of this and the persisted `MAX(updated_at)`, so a cold process still
/// gets the durable "last CHANGED" floor.
static CHAPTER_FETCH_MEMO: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<i64, DateTime<Utc>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Above this many memoized series, entries older than the chapter TTL (which can no
/// longer make anything look fresh) are dropped. The cached catalogue is ~13.8k series,
/// so this only ever trips if the id space grows well past it.
const CHAPTER_FETCH_MEMO_CAP: usize = 32_768;

/// Rows per multi-row INSERT when a chapter list really changed. 12 binds per row, so
/// 100 rows is 1,200 parameters — an order of magnitude under SQLite's limit, while
/// covering the whole list in one statement for all but the longest series.
const CHAPTER_INSERT_CHUNK: usize = 100;

/// Record that `manga_id`'s chapter list was just fetched from the source.
fn note_chapters_fetched(manga_id: i64, now: DateTime<Utc>) {
    let Ok(mut memo) = CHAPTER_FETCH_MEMO.lock() else {
        return; // poisoned → freshness degrades to the DB floor, never to wrong data
    };
    let stats = &crate::telemetry::CACHE_CHAPTER_FETCH_MARKER;
    if !memo.contains_key(&manga_id) && memo.len() >= CHAPTER_FETCH_MEMO_CAP {
        let cutoff = now - chrono::Duration::seconds(CHAPTERS_TTL_SECS);
        let before = memo.len();
        memo.retain(|_, t| *t > cutoff);
        stats.expired((before - memo.len()) as u64);
        // Keep the hard bound without turning one new id into a catalogue-wide
        // freshness reset. Evict one oldest fetch marker; its next read falls back
        // to the durable MAX(updated_at) floor and safely revalidates if needed.
        if memo.len() >= CHAPTER_FETCH_MEMO_CAP {
            if let Some(oldest) = memo
                .iter()
                .min_by_key(|(id, fetched)| (**fetched, **id))
                .map(|(id, _)| *id)
            {
                memo.remove(&oldest);
                stats.evictions(1);
            }
        }
    }
    memo.insert(manga_id, now);
    stats.fills(1);
    stats.set_entries(memo.len());
}

/// The memoized fetch time for `manga_id`, if this process has fetched it.
fn memoized_chapters_fetched(manga_id: i64) -> Option<DateTime<Utc>> {
    let fetched = CHAPTER_FETCH_MEMO
        .lock()
        .ok()
        .and_then(|m| m.get(&manga_id).copied());
    count_marker_lookup(
        &crate::telemetry::CACHE_CHAPTER_FETCH_MARKER,
        fetched,
        Utc::now(),
    );
    fetched
}

/// Telemetry only: a marker counts as a hit only while it can still make a chapter
/// list look fresh. An older marker (still returned, as the newer of two stamps) or
/// no marker is a miss.
fn count_marker_lookup(
    stats: &crate::telemetry::CacheStats,
    fetched: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) {
    let fresh =
        fetched.is_some_and(|at| now.signed_duration_since(at).num_seconds() < CHAPTERS_TTL_SECS);
    if fresh {
        stats.hits(1);
    } else {
        stats.misses(1);
    }
}

/// The series' newest-chapter time derived from a chapter LIST: the numeric max of the
/// normalized `upload_date`s, as a millis-epoch string. `None` when the list holds no
/// datable chapter.
///
/// # Invariant
///
/// **`suwayomi_series.latest_chapter_at` never holds a value derived from our own
/// clock.** This function is the ONLY producer of that column's value (migration 0050's
/// backfill applies the identical derivation in SQL), and every input to it is an
/// upstream `upload_date`. When nothing is datable the answer is `None` → the column
/// stays NULL, which is honest: the reader renders `latestChapterAt || updatedAt`, so a
/// NULL degrades to the poll time *at the display layer*, where it is recoverable,
/// instead of being frozen into the DB as if it were a real release time.
///
/// This matters because the column drives the reader's "· 4h" label and every
/// "recently updated" ordering. A clock-derived value there reads as "Updated now" on a
/// series whose newest chapter is weeks old — and, being indistinguishable from a real
/// timestamp once stored, it would also pin that series to the top of every feed.
///
/// The source data is dirty: most `upload_date`s are 13-digit millis, ~2% are 10-digit
/// seconds (12,324 of 554,642 rows live), plus corrupt outliers (one live row reads
/// year 3800). So parse to i64, normalize, take a NUMERIC max, drop garbage — a raw
/// string `.max()` only sorted correctly by coincidence of the current epoch.
fn derive_latest_chapter_at(chapters: &[SuwayomiChapter]) -> Option<String> {
    chapters
        .iter()
        .filter_map(|c| c.upload_date.as_deref())
        .filter_map(normalize_epoch_millis)
        .max()
        // Zero-padded to 13 so the stored TEXT sorts identically to the number it
        // encodes. `updates` orders on this column AS TEXT — it has to, or
        // `idx_suwayomi_series_latest_chapter` (migration 0050) stops serving the ORDER
        // BY and the feed grows a temp B-tree — while `feed_series_updates.released_at`
        // stores `MAX(CAST(… AS BIGINT))` and orders numerically. The two agree only
        // while every value is the same width. `normalize_epoch_millis` accepts anything
        // in [1000, 4_102_444_800_000], so a pre-2001-09-09 chapter yields 4–12 digits,
        // and under BINARY collation any such string starting '2'..='9' sorts ABOVE every
        // 13-digit value — pinning an ancient chapter to the top of one feed while the
        // other correctly sorts it to the bottom. That is precisely the encoding-mismatch
        // class migration 0063 exists to eliminate. Zero live rows are affected today
        // (all 11,535 non-NULL values are 13 chars); this keeps it that way by
        // construction rather than by luck.
        .map(|ms| format!("{ms:013}"))
}

/// Fill in `m.latest_chapter_at` from the cache for a manga that came off the WIRE.
///
/// `latest_chapter_at` is not part of Suwayomi's manga shape (see [`SuwayomiManga`]), so
/// a live fetch always carries `None` — while `last_fetched_at` is stamped to NOW by the
/// very fetch we just made (we fetch with `fetchManga: true`). The display layer falls
/// back from one to the other, so a live-fetched series renders "Updated now" even
/// though we hold its real newest-chapter time one SELECT away. Call this on any
/// live-fetched manga before mapping it for the client.
///
/// A manga that already carries a value (i.e. one read out of the cache) is left alone,
/// and a series we have never chaptered stays `None` — never a clock value.
pub async fn hydrate_latest_chapter_at(pool: &Database, m: &mut SuwayomiManga) {
    if m.latest_chapter_at.is_some() {
        return;
    }
    m.latest_chapter_at = crate::db_types::query_scalar::<Option<i64>>(
        "SELECT latest_chapter_at FROM suwayomi_series WHERE id = $1",
    )
    .bind(m.id)
    .fetch_optional(pool)
    .await
    .inspect_err(|e| {
        crate::db::record_read_database_error(e);
        tracing::warn!(error = %e, id = m.id, "hydrate_latest_chapter_at failed");
    })
    .ok()
    .flatten()
    .flatten()
    .map(|ms| format!("{ms:013}"));
}

/// Replace the cached chapter list for a manga (delete + insert in one tx), and
/// sync the series' `chapter_count` to the stored count.
///
/// The rewrite is SKIPPED entirely when the fetched list is field-for-field identical
/// to what is already cached, and the series' derived columns (`chapter_count`,
/// `latest_chapter_at`) already match. `scanner::persist_scan` calls this on every
/// scan tick — ~24k/day, of which ≥95% find no new chapter — and each rewrite was a
/// DELETE plus one INSERT per chapter (48 on average) inside its own transaction.
///
/// That comparison runs FIRST, in a read-only snapshot on the reader lane
/// ([`chapters_cached_unchanged`]): indexed reads and zero writes. An unchanged scan
/// therefore never opens a writer transaction, takes no advisory key and no row lock,
/// and cannot make a foreground writer of the work wait. Only a list that differs goes
/// on to the guarded unit, which compares AGAIN under its locks: the lock-free answer
/// is a reason to skip, never a reason to write.
///
/// Skipping on the lock-free read is safe. The read saw a committed cache equal to what
/// this call would write, so skipping is the same outcome as writing it at that
/// instant; a writer that commits a different list later would have replaced ours too.
///
/// When the list HAS changed, the per-row INSERT loop is replaced by chunked multi-row
/// INSERTs (measured 24.0 ms → 2.6 ms per 48-chapter series). Cache, spine, release
/// ledger and source-native keys commit together (T194, T210): ledger rows are written through
/// `catalog::ledger::record_source_series_on` under the same work and source locks, so a
/// failed ledger write rolls the cache back too and the next scan, which then still
/// sees a different list, retries the whole publication.
#[cfg(test)]
pub async fn put_chapters(
    pool: &Database,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
) -> Result<()> {
    let generation = chapters
        .first()
        .map(|c| crate::engine_keys::token(c.engine_generation.as_deref()))
        .transpose()?
        .flatten();
    put_chapters_in_generation(pool, manga_id, chapters, generation).await
}

/// The explicit token also fences an empty authoritative chapter list.
pub async fn put_chapters_in_generation(
    pool: &Database,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
    generation: Option<uuid::Uuid>,
) -> Result<()> {
    crate::engine_generation::wait(pool).await?;
    let result = put_chapters_inner(pool, manga_id, chapters, generation).await;
    crate::engine_generation::observe_write(pool, result)
}

async fn put_chapters_inner(
    pool: &Database,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
    generation: Option<uuid::Uuid>,
) -> Result<()> {
    let wanted = WantedChapters::of(chapters);
    if chapters_cached_unchanged(pool, manga_id, chapters, &wanted, generation).await {
        // The unchanged path records a successful fetch without a database write.
        note_chapters_fetched(manga_id, Utc::now());
        return Ok(());
    }
    // Read before BEGIN: the unit below holds the leased background writer and its
    // locks, and must not wait for a reader connection while it does.
    let record_ledger = release_ledger_ready(pool, manga_id).await;
    let cached = crate::db::retry_transaction(|| {
        put_chapters_attempt(pool, manga_id, chapters, &wanted, record_ledger, generation)
    })
    .await?;
    if cached {
        // A failed write must not make an older chapter cache look freshly fetched.
        note_chapters_fetched(manga_id, Utc::now());
    }
    Ok(())
}

/// What one chapter replacement wants the cache to hold: the digest rows in id order
/// and the two derived series columns.
struct WantedChapters {
    rows: Vec<ChapterDigestRow>,
    count: i64,
    latest: Option<String>,
}

impl WantedChapters {
    fn of(chapters: &[SuwayomiChapter]) -> Self {
        let mut rows: Vec<ChapterDigestRow> = chapters.iter().map(ChapterDigestRow::of).collect();
        rows.sort_by_key(|r| r.id);
        Self {
            rows,
            count: crate::catalog::main_chapter_count(chapters.iter().map(|c| c.chapter_number)),
            latest: derive_latest_chapter_at(chapters),
        }
    }
}

/// The lock-free pre-check of [`put_chapters`]: is the committed cache already what
/// this replacement would write, including its keys? One read-only snapshot keeps
/// the display and key comparisons on the same committed version, without taking
/// a writer lease, advisory key or row lock. A failed read answers "not known",
/// which sends the caller to the guarded unit (it reads again
/// under its locks), so a busy reader pool can cost a transaction but never a result.
async fn chapters_cached_unchanged(
    pool: &Database,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
    wanted: &WantedChapters,
    generation: Option<uuid::Uuid>,
) -> bool {
    let read = async {
        let mut snapshot = pool
            .read()
            .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await?;
        // A list fetched under an earlier engine generation is not "unchanged" even when
        // it matches: the write path must refuse it at the generation fence instead of
        // reporting success.
        if let Some(generation) = generation {
            let current: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM engine_generation WHERE id = $1 AND state = 'active')",
            )
            .bind(generation)
            .fetch_one(&mut *snapshot)
            .await?;
            if !current {
                return Ok(false);
            }
        }
        let derived: Option<(i64, Option<i64>)> = crate::db_types::query_tuple(
            "SELECT chapter_count, latest_chapter_at FROM suwayomi_series WHERE id = $1",
        )
        .bind(manga_id)
        .fetch_optional(&mut *snapshot)
        .await?;
        let Some(derived) = derived else {
            return Ok(false);
        };
        if !chapter_derived_matches(&derived, wanted.count, &wanted.latest) {
            return Ok(false);
        }
        let existing = sqlx::query_as::<_, ChapterDigestRow>(CHAPTER_DIGEST_SELECT)
            .bind(manga_id)
            .fetch_all(&mut *snapshot)
            .await?;
        let unchanged = existing == wanted.rows
            && crate::engine_keys::chapters_unchanged_on(&mut snapshot, manga_id, chapters).await?;
        snapshot.rollback().await?;
        Ok::<_, anyhow::Error>(unchanged)
    };
    match read.await {
        Ok(unchanged) => unchanged,
        Err(e) => {
            let e = crate::db::observed_read_database_error(e);
            tracing::debug!(manga_id, error = %e, "series cache: unchanged pre-check failed");
            false
        }
    }
}

const CHAPTER_DIGEST_SELECT: &str = "SELECT id, name, chapter_number, scanlator, upload_date, \
            is_read, is_bookmarked, is_downloaded, last_page_read, page_count \
     FROM suwayomi_chapter WHERE manga_id = $1 ORDER BY id";

fn chapter_derived_matches(
    stored: &(i64, Option<i64>),
    count: i64,
    latest: &Option<String>,
) -> bool {
    stored.0 == count
        && (latest.is_none() || latest.as_deref().and_then(|v| v.parse::<i64>().ok()) == stored.1)
}

/// The advisory keys of a unit that publishes rows derived from the source mappings of
/// one Suwayomi series, for its single `locks::advisory` call. `Series` is the key of
/// the scan and admin writers of the series. The Suwayomi source identity key is the
/// one a writer holds while it creates a `source_series` row for this series
/// (`catalog::source_claim_keys`): a mapping that is absent when the unit looks for it
/// has no row to lock, so only this key keeps it from appearing while the unit runs.
pub(crate) fn series_publication_keys(series_id: &str) -> [crate::locks::AdvisoryKey; 2] {
    [
        crate::locks::AdvisoryKey::new(crate::locks::KeyClass::Series, &[series_id]),
        crate::locks::AdvisoryKey::source_identity("suwayomi", series_id),
    ]
}

#[cfg(test)]
tokio::task_local! { static CHAPTER_PUBLICATION_PAUSE: crate::locks::pause_points::PausePoints; }
#[cfg(test)]
tokio::task_local! { static SERIES_PUBLICATION_PAUSE: crate::locks::pause_points::PausePoints; }

async fn put_chapters_attempt(
    pool: &Database,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
    wanted: &WantedChapters,
    record_ledger: bool,
    generation: Option<uuid::Uuid>,
) -> Result<bool> {
    let (count, latest, incoming) = (wanted.count, &wanted.latest, &wanted.rows);
    let mut tx = pool.begin().await?;
    crate::engine_generation::shared(&mut tx, generation).await?;
    crate::locks::advisory(&mut tx, &series_publication_keys(&manga_id.to_string())).await?;
    let sources: Vec<String> = sqlx::query_scalar("SELECT id FROM source_series WHERE source_type = 'suwayomi' AND source_key = $1 ORDER BY id")
        .bind(manga_id.to_string()).fetch_all(&mut *tx).await?;
    crate::locks::lock_source_series(&mut tx, &sources).await?;
    let current: Vec<String> = sqlx::query_scalar("SELECT id FROM source_series WHERE source_type = 'suwayomi' AND source_key = $1 ORDER BY id").bind(manga_id.to_string()).fetch_all(&mut *tx).await?;
    if current != sources {
        return Err(crate::locks::RestartTransaction.into());
    }

    sqlx::query("SELECT id FROM suwayomi_series WHERE id = $1 FOR NO KEY UPDATE")
        .bind(manga_id)
        .execute(&mut *tx)
        .await?;
    #[cfg(test)]
    if let Ok(points) = CHAPTER_PUBLICATION_PAUSE.try_with(Clone::clone) {
        points.hit("chapters-locked").await;
    }
    let derived: Option<(i64, Option<i64>)> = crate::db_types::query_tuple(
        "SELECT chapter_count, latest_chapter_at FROM suwayomi_series WHERE id = $1",
    )
    .bind(manga_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(derived) = derived else {
        tx.rollback().await?;
        return Ok(false);
    };
    crate::engine_keys::capture_chapters_on(&mut tx, manga_id, chapters).await?;
    // The recheck under the locks. The lock-free pre-check only ever skips; whether to
    // WRITE is decided here, against rows nobody can change while the unit runs.
    let existing = sqlx::query_as::<_, ChapterDigestRow>(CHAPTER_DIGEST_SELECT)
        .bind(manga_id)
        .fetch_all(&mut *tx)
        .await?;
    let changed = &existing != incoming;
    if !changed && chapter_derived_matches(&derived, count, latest) {
        crate::engine_generation::commit_ready(pool)?;
        tx.commit().await?;
        return Ok(true);
    }

    let now = Utc::now().to_rfc3339();
    if changed {
        sqlx::query("DELETE FROM suwayomi_chapter WHERE manga_id = $1")
            .bind(manga_id)
            .execute(&mut *tx)
            .await?;
        // Twelve binds per row; keep well below SQLite's variable limit.
        for chunk in chapters.chunks(CHAPTER_INSERT_CHUNK) {
            let values = crate::sql_params::placeholders(1, chunk.len() * 12)
                .split(", ")
                .collect::<Vec<_>>()
                .chunks(12)
                .map(|row| format!("({})", row.join(", ")))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO suwayomi_chapter \
                   (id, manga_id, name, chapter_number, scanlator, upload_date, \
                    is_read, is_bookmarked, is_downloaded, last_page_read, page_count, updated_at) \
                 VALUES {values}"
            );
            let mut q = sqlx::query(&sql);
            for c in chunk {
                q = q
                    .bind(c.id)
                    .bind(c.manga_id)
                    .bind(&c.name)
                    .bind(c.chapter_number)
                    .bind(&c.scanlator)
                    .bind(&c.upload_date)
                    .bind(c.is_read)
                    .bind(c.is_bookmarked)
                    .bind(c.is_downloaded)
                    .bind(c.last_page_read)
                    .bind(c.page_count)
                    .bind(crate::db_types::date(&now)?);
            }
            q.execute(&mut *tx).await?;
        }
        // A spine error rolls back the chapter cache too. The identical upstream
        // list remains different from the old cache on the next scan and retries.
        let mapping = write_chapters_to_spine_on(&mut tx, manga_id, chapters).await?;
        // The release ledger in the same commit, under the locks taken above: `mapping`
        // is one of `sources`, so `lock_source_series` holds it and its work.
        if let (Some(ssid), true) = (mapping, record_ledger) {
            crate::catalog::ledger::record_source_series_on(&mut tx, &ssid).await?;
        }
    }
    // Preserve a previously good upstream time if this batch has no datable rows.
    sqlx::query(
        "UPDATE suwayomi_series \
         SET chapter_count = $1, latest_chapter_at = COALESCE($2, latest_chapter_at), \
             updated_at = $3 WHERE id = $4",
    )
    .bind(count)
    .bind(latest.as_deref().map(str::parse::<i64>).transpose()?)
    .bind(crate::db_types::date(&now)?)
    .bind(manga_id)
    .execute(&mut *tx)
    .await?;
    crate::engine_generation::commit_ready(pool)?;
    tx.commit().await?;
    Ok(true)
}

/// Mirror chapters on the caller's writer connection. An unmapped series is picked
/// up by the spine backfill once its source mapping exists.
async fn write_chapters_to_spine_on(
    conn: &mut sqlx::PgConnection,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
) -> Result<Option<String>> {
    // Keep the indexed source_key as text; casting that column would force a scan.
    let ssid: Option<String> = crate::db_types::query_scalar(
        "SELECT id FROM source_series \
         WHERE source_type = 'suwayomi' AND source_key = $1 \
         ORDER BY last_seen DESC NULLS LAST, id ASC LIMIT 1",
    )
    .bind(manga_id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(ssid) = ssid else {
        return Ok(None);
    };
    let inputs: Vec<crate::catalog::ChapterInput> = chapters
        .iter()
        .map(crate::catalog::suwayomi_chapter_input)
        .collect();
    crate::catalog::replace_source_chapters_on(conn, &ssid, &inputs).await?;
    Ok(Some(ssid))
}

/// Whether a chapter replacement may record release history: only once the spine
/// backfill has drained. During the backfill the ledger seed considers every source
/// together, which preserves first credit; recording one source early would not.
///
/// Best-effort and read before the unit begins. A failed read answers "no": the
/// replacement goes ahead without ledger rows, and the ledger seed or the next changed
/// scan records them.
async fn release_ledger_ready(pool: &Database, manga_id: i64) -> bool {
    match crate::catalog::spine::remaining(pool).await {
        Ok(remaining) => remaining == (0, 0),
        Err(e) => {
            let e = crate::db::observed_read_database_error(e);
            tracing::warn!(manga_id, error = %e, "series cache: spine readiness check failed");
            false
        }
    }
}

/// A cached-series row shape (reconstructed into a `SuwayomiManga`).
#[derive(sqlx::FromRow)]
struct SeriesRow {
    engine_generation: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    id: i64,
    #[sqlx(try_from = "crate::db_types::DbString")]
    title: String,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    thumbnail_url: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    author: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    artist: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    description: Option<String>,
    genre: Option<serde_json::Value>,
    #[sqlx(try_from = "crate::db_types::DbString")]
    status: String,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    in_library: i64,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    in_library_at: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    last_fetched_at: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalI64")]
    latest_chapter_at: Option<i64>,
    #[sqlx(try_from = "crate::db_types::DbString")]
    source_id: String,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    lang: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    chapter_count: i64,
}

impl From<SeriesRow> for SuwayomiManga {
    fn from(r: SeriesRow) -> Self {
        let genre: Vec<String> = r
            .genre
            .and_then(|g| serde_json::from_value(g).ok())
            .unwrap_or_default();
        SuwayomiManga {
            engine_generation: r.engine_generation,
            real_url: None,
            id: r.id,
            title: r.title,
            // Not persisted in the cache — only a live fetch carries the source URL.
            url: None,
            thumbnail_url: r.thumbnail_url,
            author: r.author,
            artist: r.artist,
            description: r.description,
            genre,
            status: r.status,
            in_library: r.in_library != 0,
            in_library_at: r.in_library_at,
            last_fetched_at: r.last_fetched_at,
            latest_chapter_at: r.latest_chapter_at.map(|ms| format!("{ms:013}")),
            source_id: r.source_id,
            source: Some(SuwayomiSourceLang { lang: r.lang }),
            chapters: Some(ChapterCount {
                total_count: r.chapter_count,
            }),
        }
    }
}

/// Parse a source `upload_date` string and normalize it to a millisecond epoch.
///
/// Suwayomi's `upload_date` is nominally millis, but the real data is mixed: mostly
/// 13-digit millis, ~2% 10-digit seconds, and a few corrupt values (negative, or a
/// year-2326 outlier). Returns `None` for anything non-numeric, non-positive, or
/// outside a sane range, so garbage never becomes a series' newest-chapter time.
/// Values below `10^12` (i.e. a plausible seconds epoch) are scaled to millis; values
/// already in millis pass through. The upper bound rejects the corrupt outliers while
/// still allowing dates well beyond the current year.
pub fn normalize_epoch_millis(s: &str) -> Option<i64> {
    let n: i64 = s.trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    // ~year 33658 in seconds; anything at/above is already millis.
    let ms = if n < 1_000_000_000_000 {
        n.checked_mul(1000)?
    } else {
        n
    };
    // Reject implausibly-far-future millis (> 2100-01-01) as corrupt. This MATTERS now
    // that the max is numeric rather than string: the real data has a year-2326 outlier
    // (11251332011000) that a numeric max WOULD pick — pinning that series to the top of
    // every "recently updated" feed forever. A string max dodged it by digit-length
    // luck; this bound handles it on purpose. Legit far-future MangaDex `publishAt`
    // dates (e.g. 2037) stay well under the bound.
    if ms > 4_102_444_800_000 {
        return None;
    }
    Some(ms)
}

const SERIES_SELECT: &str = "SELECT (SELECT id::text FROM engine_generation WHERE state='active') AS engine_generation, id, title, thumbnail_url, author, artist, description, genre, \
     status, in_library, in_library_at, last_fetched_at, latest_chapter_at, source_id, lang, \
     chapter_count \
     FROM suwayomi_series";

/// Load one cached series, or `None` on a cache miss.
pub async fn get_series(pool: &Database, id: i64) -> Result<Option<SuwayomiManga>> {
    let row = sqlx::query_as::<_, SeriesRow>(&format!("{SERIES_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(Into::into))
}

/// Load one cached series together with whether its METADATA is still fresh (fetched
/// within `SERIES_TTL_SECS`). `None` on a cache miss. The reader uses the flag to
/// decide whether to revalidate upstream on a cache hit.
pub async fn get_series_fresh(pool: &Database, id: i64) -> Result<Option<(SuwayomiManga, bool)>> {
    Ok(get_series_fresh_batch(pool, &[id]).await?.remove(&id))
}

/// Metadata and freshness for a page in bounded grouped queries, including misses.
pub async fn get_series_fresh_batch(
    pool: &Database,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, (SuwayomiManga, bool)>> {
    #[derive(sqlx::FromRow)]
    struct FreshRow {
        #[sqlx(flatten)]
        series: SeriesRow,
        #[sqlx(try_from = "crate::db_types::DbOptionalString")]
        series_fetched_at: Option<String>,
    }
    let mut out = std::collections::HashMap::new();
    for chunk in ids.chunks(256) {
        let ph = crate::sql_params::placeholders(1, chunk.len());
        let sql = format!(
            "{} WHERE id IN ({ph})",
            SERIES_SELECT.replace("chapter_count ", "chapter_count, series_fetched_at ")
        );
        let mut query = sqlx::query_as::<_, FreshRow>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        for row in query.fetch_all(pool).await? {
            let fresh = is_fresh(row.series_fetched_at.as_deref(), SERIES_TTL_SECS);
            out.insert(row.series.id, (row.series.into(), fresh));
        }
    }
    Ok(out)
}

/// Chapter lists and freshness for all sources of one page, without per-source reads.
pub async fn get_chapters_fresh_batch(
    pool: &Database,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, (Vec<SuwayomiChapter>, bool)>> {
    #[derive(sqlx::FromRow)]
    struct FreshRow {
        #[sqlx(flatten)]
        chapter: ChapterRow,
        #[sqlx(try_from = "crate::db_types::DbOptionalString")]
        last_fetched_at: Option<String>,
    }
    let mut out: std::collections::HashMap<i64, (Vec<SuwayomiChapter>, bool)> =
        std::collections::HashMap::new();
    for chunk in ids.chunks(256) {
        let ph = crate::sql_params::placeholders(1, chunk.len());
        let sql = format!(
            "SELECT (SELECT id::text FROM engine_generation WHERE state='active') AS engine_generation, id, manga_id, name, chapter_number, scanlator, upload_date, \
             is_read, is_bookmarked, is_downloaded, last_page_read, page_count, \
             MAX(updated_at) OVER (PARTITION BY manga_id) AS last_fetched_at \
             FROM suwayomi_chapter WHERE manga_id IN ({ph}) \
             ORDER BY manga_id, chapter_number DESC, id DESC"
        );
        let mut query = sqlx::query_as::<_, FreshRow>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        for row in query.fetch_all(pool).await? {
            let id = row.chapter.manga_id;
            let fresh = is_fresh(row.last_fetched_at.as_deref(), CHAPTERS_TTL_SECS)
                || memoized_chapters_fetched(id)
                    .is_some_and(|at| is_fresh(Some(&at.to_rfc3339()), CHAPTERS_TTL_SECS));
            let entry = out.entry(id).or_insert_with(|| (Vec::new(), fresh));
            entry.0.push(row.chapter.into());
        }
    }
    Ok(out)
}

/// When this manga's cached chapter list was last fetched, or `None` if it never was.
///
/// Two sources, whichever is newer:
///   - the persisted `MAX(suwayomi_chapter.updated_at)`, which `put_chapters` stamps
///     when it actually rewrites rows. Since that rewrite is now skipped for an
///     unchanged list, this is really "when the list last CHANGED" — a durable floor
///     that survives restarts but understates freshness.
///   - [`CHAPTER_FETCH_MEMO`], the in-process record of every `put_chapters` call
///     including the no-op ones, which is the true "last fetched".
///
/// `put_series` never touches either, so this still cleanly tracks chapter fetches
/// rather than metadata fetches.
pub async fn chapters_last_fetched(pool: &Database, manga_id: i64) -> Result<Option<String>> {
    let stored: Option<String> = crate::db_types::query_scalar::<Option<String>>(
        "SELECT MAX(updated_at) FROM suwayomi_chapter WHERE manga_id = $1",
    )
    .bind(manga_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    let Some(memo) = memoized_chapters_fetched(manga_id) else {
        return Ok(stored);
    };
    // Prefer the memo unless the persisted stamp is somehow newer (another process, or
    // a clock the memo predates) — max, never blind replacement.
    let stored_newer = stored
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|t| t.with_timezone(&Utc) > memo);
    Ok(if stored_newer {
        stored
    } else {
        Some(memo.to_rfc3339())
    })
}

/// Load the cached chapter list for a manga (source order: newest first by number).
pub async fn get_chapters(pool: &Database, manga_id: i64) -> Result<Vec<SuwayomiChapter>> {
    let rows = sqlx::query_as::<_, ChapterRow>(
        "SELECT (SELECT id::text FROM engine_generation WHERE state='active') AS engine_generation, id, manga_id, name, chapter_number, scanlator, upload_date, \
                is_read, is_bookmarked, is_downloaded, last_page_read, page_count \
         FROM suwayomi_chapter WHERE manga_id = $1 ORDER BY chapter_number DESC, id DESC",
    )
    .bind(manga_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

#[derive(sqlx::FromRow)]
struct ChapterRow {
    engine_generation: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    id: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    manga_id: i64,
    #[sqlx(try_from = "crate::db_types::DbString")]
    name: String,
    chapter_number: f64,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    scanlator: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    upload_date: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    is_read: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    is_bookmarked: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    is_downloaded: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    last_page_read: i64,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    page_count: i64,
}

impl From<ChapterRow> for SuwayomiChapter {
    fn from(r: ChapterRow) -> Self {
        SuwayomiChapter {
            engine_generation: r.engine_generation,
            manga: None,
            url: None,
            real_url: None,
            source_order: None,
            id: r.id,
            manga_id: r.manga_id,
            name: r.name,
            chapter_number: r.chapter_number,
            scanlator: r.scanlator,
            upload_date: r.upload_date,
            is_read: r.is_read != 0,
            is_bookmarked: r.is_bookmarked != 0,
            is_downloaded: r.is_downloaded != 0,
            last_page_read: r.last_page_read,
            page_count: r.page_count,
        }
    }
}

/// SQL predicate hiding NSFW-linked series from viewers who haven't opted in.
///
/// `suwayomi_series` carries no NSFW column of its own — the flag lives on the
/// canonical `work` it is linked to — so the gate is a NOT EXISTS against that
/// link. Takes one bind: `show_nsfw as i64`. Used verbatim by both remaining callers,
/// `library` and `recently_added`, which reference the table unaliased.
///
/// The EFFECTIVE flag is `COALESCE(is_nsfw_override, is_nsfw)`, not `is_nsfw`: both
/// admin "mark NSFW" mutations write ONLY the override column, so testing the raw
/// column let an admin override be ignored here. Production carries 3 works whose
/// override disagrees with the crawled flag (2 forced on, 1 forced off).
///
/// This gate no longer has anything to do with Browse. Browse moved onto
/// `browse_catalogue`, whose `is_nsfw` column is a MATERIALIZED copy of the same
/// COALESCE (migration 0069) kept live by `graphql::resync_feed_nsfw` — a different
/// mechanism with its own tests, precisely so this correlated anti-join is not on a paged
/// hot path. The `(? = 1 OR …)` shape survives here only because these two callers have a
/// small fixed `LIMIT` and no ORDER BY the OR can spoil; it is exactly the shape
/// `browse::build_where` documents as forbidden for a paged query.
const NSFW_GATE_SQL: &str = "($1 OR NOT EXISTS ( \
        SELECT 1 FROM source_series ss JOIN work w ON w.id = ss.work_id \
        WHERE ss.source_type = 'suwayomi' AND ss.source_key = CAST(suwayomi_series.id AS TEXT) \
          AND COALESCE(w.is_nsfw_override, w.is_nsfw)))";

/// Cached in-library series, most-recently-updated first — serves `discovery`
/// without a live source browse. `limit` caps the result.
///
/// The NSFW gate is applied IN SQL, before `LIMIT`. It used to be applied in Rust
/// after the query returned, which meant an anonymous viewer got 20 rows and then
/// had most of them stripped — `discovery` was observed live returning 0 POPULAR
/// items and 1 TRENDING item, i.e. the home page looked broken rather than
/// filtered. Filtering first means a full page is always returned.
pub async fn library(pool: &Database, limit: i64, show_nsfw: bool) -> Result<Vec<SuwayomiManga>> {
    // Ordered by REAL newest-chapter time (migration 0050), not `last_fetched_at`
    // (= poll time). NULLs (no datable chapter yet) sort last automatically under DESC,
    // then id for a stable total order — matching idx_suwayomi_series_latest_chapter so
    // this reads in order without a temp sort. This is the "recently updated" feed the
    // name implies.
    let rows = sqlx::query_as::<_, SeriesRow>(&format!(
        "{SERIES_SELECT} WHERE in_library = TRUE AND {NSFW_GATE_SQL} \
         ORDER BY latest_chapter_at DESC NULLS LAST, id DESC LIMIT $2"
    ))
    .bind(show_nsfw)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Cached catalogue series ordered by when they were FIRST added to our cache
/// (`created_at` desc) — powers the home "Latest Added" row. Distinct from
/// {@link library} (most-recently-updated) and the source "Latest" endpoint
/// (recently-updated upstream): this is "what the catalogue gained most recently".
/// `limit` caps the result. NSFW is gated in SQL before `LIMIT` — see {@link library}.
pub async fn recently_added(
    pool: &Database,
    limit: i64,
    show_nsfw: bool,
) -> Result<Vec<SuwayomiManga>> {
    // Keep the shared Phase-3 query unchanged; this path can use boolean SQL now.
    const RECENT_NSFW_GATE: &str = "($1 OR NOT EXISTS ( \
         SELECT 1 FROM source_series ss JOIN work w ON w.id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND ss.source_key = CAST(suwayomi_series.id AS TEXT) \
           AND COALESCE(w.is_nsfw_override, w.is_nsfw) = TRUE))";
    let rows = sqlx::query_as::<_, SeriesRow>(&format!(
        "{SERIES_SELECT} WHERE in_library = TRUE AND {RECENT_NSFW_GATE} \
         ORDER BY COALESCE(created_at, updated_at) DESC, id DESC LIMIT $2"
    ))
    .bind(show_nsfw)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// How many series are cached (any). Lets `discovery` decide DB-vs-live.
pub async fn count(pool: &Database) -> Result<i64> {
    Ok(
        crate::db_types::query_scalar("SELECT COUNT(*) FROM suwayomi_series")
            .fetch_one(pool)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::TestPoolOptions;

    async fn pool() -> Database {
        let pool = TestPoolOptions::new()
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

    fn manga(id: i64, title: &str) -> SuwayomiManga {
        SuwayomiManga {
            engine_generation: None,
            real_url: None,
            id,
            title: title.into(),
            url: None,
            thumbnail_url: Some(format!("/thumb/{id}")),
            author: Some("Author".into()),
            artist: Some("Artist".into()),
            description: Some("Desc".into()),
            genre: vec!["Action".into(), "Comedy".into()],
            status: "ONGOING".into(),
            in_library: true,
            in_library_at: Some("2026-01-01T00:00:00Z".into()),
            last_fetched_at: Some("2026-01-02T00:00:00Z".into()),
            latest_chapter_at: None,
            source_id: "src-1".into(),
            source: Some(SuwayomiSourceLang {
                lang: Some("en".into()),
            }),
            chapters: Some(ChapterCount { total_count: 3 }),
        }
    }

    fn chapter(id: i64, manga_id: i64, num: f64) -> SuwayomiChapter {
        SuwayomiChapter {
            engine_generation: None,
            manga: None,
            url: None,
            real_url: None,
            source_order: None,
            id,
            manga_id,
            name: format!("Ch {num}"),
            chapter_number: num,
            scanlator: Some("Scan".into()),
            upload_date: Some("1600000000000".into()),
            is_read: false,
            is_bookmarked: false,
            is_downloaded: false,
            last_page_read: 0,
            page_count: 20,
        }
    }

    #[tokio::test]
    async fn concurrent_metadata_writers_publish_cache_and_keys_in_one_transaction() {
        let pool = pool().await.foreground();
        // The source key stays: the write guard refuses a changed key as another series.
        // The title and web link are what one publication must change with its key row.
        let mut initial = manga(1, "Initial");
        initial.url = Some("/series".into());
        initial.real_url = Some("https://source.test/initial".into());
        put_series(&pool, &initial).await.unwrap();
        let mut older = initial.clone();
        older.title = "Older".into();
        older.real_url = Some("https://source.test/older".into());
        let mut newer = initial;
        newer.title = "Newer".into();
        newer.real_url = Some("https://source.test/newer".into());
        let points = crate::locks::pause_points::PausePoints::default();
        let held = points.arm("series-cache-written");
        let writer = pool.clone();
        let first = tokio::spawn(
            SERIES_PUBLICATION_PAUSE
                .scope(points, async move { put_series(&writer, &older).await }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(30), held.reached())
            .await
            .unwrap();
        let writer = pool.clone();
        let mut second = tokio::spawn(async move { put_series(&writer, &newer).await });
        tokio::select! {
            _ = crate::cover::wait_for_writer_lock(&pool) => {}
            done = &mut second => panic!("metadata publication did not wait for the series guard: {done:?}"),
        }
        let read = || {
            sqlx::query_as::<_, (String, String, String, String, String, String)>(
            "SELECT s.title, m.title, m.manga_key, m.web_url, s.xmin::text, m.xmin::text \
             FROM suwayomi_series s JOIN engine_manga_key m ON m.manga_id = s.id WHERE s.id = 1",
        )
        };
        let during = read().fetch_one(&pool).await.unwrap();
        assert_eq!(during.0, "Initial");
        assert_eq!(during.1, "Initial");
        assert_eq!(during.3, "https://source.test/initial");
        held.release();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        let after = read().fetch_one(&pool).await.unwrap();
        assert_eq!(after.0, "Newer");
        assert_eq!(after.1, "Newer");
        assert_eq!(after.2, "/series");
        assert_eq!(after.3, "https://source.test/newer");
        assert_eq!(
            after.4, after.5,
            "metadata and keys must share their publishing transaction"
        );
    }

    #[tokio::test]
    async fn two_foreground_chapter_replacements_leave_one_complete_cache_and_spine() {
        let pool = pool().await.foreground();
        put_series(&pool, &manga(1, "Series")).await.unwrap();
        sqlx::raw_sql("INSERT INTO work(id,created_at,updated_at) VALUES ('w_chapters',now(),now()); INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES ('ss_chapters','w_chapters','suwayomi','src-1','1',now())").execute(&pool).await.unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let armed = points.arm("chapters-locked");
        let writer = pool.clone();
        let first = tokio::spawn(CHAPTER_PUBLICATION_PAUSE.scope(points, async move {
            let mut c = chapter(11, 1, 1.0);
            c.url = Some("/chapter/11".into());
            c.source_order = Some(1);
            put_chapters(&writer, 1, &[c]).await
        }));
        tokio::time::timeout(std::time::Duration::from_secs(30), armed.reached())
            .await
            .unwrap();
        let writer = pool.clone();
        let second = tokio::spawn(async move {
            let mut chapters = [chapter(22, 1, 2.0), chapter(33, 1, 3.0)];
            for c in &mut chapters {
                c.url = Some(format!("/chapter/{}", c.id));
                c.source_order = Some(c.id as i32);
            }
            put_chapters(&writer, 1, &chapters).await
        });
        crate::cover::wait_for_writer_lock(&pool).await;
        armed.release();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        let cached: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM suwayomi_chapter WHERE manga_id=1 ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let spine: Vec<String> = sqlx::query_scalar("SELECT external_id FROM chapter WHERE source_series_id='ss_chapters' ORDER BY external_id").fetch_all(&pool).await.unwrap();
        assert_eq!(cached, vec![22, 33]);
        assert_eq!(spine, vec!["22", "33"]);
        let keys: Vec<(i64, String, i32, bool)> = sqlx::query_as(
            "SELECT c.id, k.chapter_key, k.source_order, c.xmin = k.xmin \
             FROM suwayomi_chapter c JOIN engine_chapter_key k ON k.chapter_id = c.id \
             WHERE c.manga_id = 1 ORDER BY c.id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            keys,
            vec![
                (22, "/chapter/22".into(), 22, true),
                (33, "/chapter/33".into(), 33, true)
            ],
            "the winning chapter cache and keys must come from the same transaction"
        );
        let count: i64 = sqlx::query_scalar("SELECT chapter_count FROM suwayomi_series WHERE id=1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);

        // The shared Series key alone holds a replacement back, before it reads any row
        // of the series. That key is all a writer of an absent admin row or of a new
        // source mapping can hold, so the row locks above cannot stand in for it.
        let mut holder = pool.begin().await.unwrap();
        crate::locks::advisory(
            &mut holder,
            &[crate::locks::AdvisoryKey::new(
                crate::locks::KeyClass::Series,
                &["1"],
            )],
        )
        .await
        .unwrap();
        let writer = pool.clone();
        let third =
            tokio::spawn(async move { put_chapters(&writer, 1, &[chapter(44, 1, 4.0)]).await });
        crate::cover::wait_for_writer_lock(&pool).await;
        let cached: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM suwayomi_chapter WHERE manga_id=1 ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            cached,
            vec![22, 33],
            "replaced while the Series key was held"
        );
        holder.commit().await.unwrap();
        third.await.unwrap().unwrap();
        let cached: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM suwayomi_chapter WHERE manga_id=1 ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let spine: Vec<String> = sqlx::query_scalar("SELECT external_id FROM chapter WHERE source_series_id='ss_chapters' ORDER BY external_id").fetch_all(&pool).await.unwrap();
        assert_eq!(cached, vec![44]);
        assert_eq!(spine, vec!["44"]);
    }

    /// A mapping of the series that does not exist yet has no row to lock. Its creator
    /// holds the Suwayomi source identity key, and the replacement must wait for that
    /// key: otherwise it finds no mapping, writes no spine and commits, and the new
    /// mapping keeps an empty spine until the next scan.
    #[tokio::test]
    async fn a_chapter_replacement_waits_for_a_source_mapping_that_is_being_created() {
        let pool = pool().await.foreground();
        put_series(&pool, &manga(1, "Series")).await.unwrap();
        sqlx::query("INSERT INTO work(id,created_at,updated_at) VALUES ('w_new',now(),now())")
            .execute(&pool)
            .await
            .unwrap();
        // The real catalogue writer, stopped inside its transaction after it took its
        // guards and before it inserted the mapping.
        let points = crate::locks::pause_points::PausePoints::default();
        let held = points.arm("guard-held");
        let creator_pool = pool.clone();
        let creator = tokio::spawn(
            crate::catalog::guards::TEST_PAUSE.scope(points, async move {
                crate::catalog::upsert_source_series(
                    &creator_pool,
                    "w_new",
                    "suwayomi",
                    "src-1",
                    "1",
                    None,
                    false,
                )
                .await
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(30), held.reached())
            .await
            .expect("the catalogue writer did not reach its guards");
        let writer = pool.clone();
        let mut replacement =
            tokio::spawn(async move { put_chapters(&writer, 1, &[chapter(11, 1, 1.0)]).await });
        tokio::select! {
            _ = crate::cover::wait_for_writer_lock(&pool) => {}
            done = &mut replacement => panic!("the replacement did not wait for the mapping creator: {done:?}"),
        }
        let cached: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM suwayomi_chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cached, 0, "replaced while the mapping was being created");
        held.release();
        let mapping = creator.await.unwrap().unwrap();
        replacement.await.unwrap().unwrap();
        let spine: Vec<String> = sqlx::query_scalar(
            "SELECT external_id FROM chapter WHERE source_series_id = $1 ORDER BY external_id",
        )
        .bind(&mapping)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(spine, vec!["11"]);
    }

    /// Whether another session can take the series row right now. A unit that waits
    /// for a parent (a work or a source mapping) must not hold this lower-tier row.
    async fn series_row_is_free(pool: &Database, manga_id: i64) -> bool {
        let mut probe = pool.begin().await.unwrap();
        let free =
            sqlx::query("SELECT id FROM suwayomi_series WHERE id = $1 FOR NO KEY UPDATE NOWAIT")
                .bind(manga_id)
                .execute(&mut *probe)
                .await
                .is_ok();
        probe.rollback().await.unwrap();
        free
    }

    async fn mapped_series(pool: &Database) {
        put_series(pool, &manga(1, "Series")).await.unwrap();
        sqlx::raw_sql("INSERT INTO work(id,created_at,updated_at) VALUES ('w_chapters',now(),now()); INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES ('ss_chapters','w_chapters','suwayomi','src-1','1',now())").execute(pool).await.unwrap();
    }

    /// An unchanged chapter list is decided on the reader lane, before any guard: it
    /// must return while another transaction holds the unit's advisory keys and the
    /// work and mapping rows. (A replacement that always opened its guarded unit
    /// would wait for the keys and time out.)
    #[tokio::test]
    async fn an_unchanged_chapter_list_takes_no_guard() {
        let pool = pool().await.foreground();
        mapped_series(&pool).await;
        let mut list = [chapter(11, 1, 1.0), chapter(22, 1, 2.0)];
        for c in &mut list {
            c.url = Some(format!("/chapter/{}", c.id));
            c.source_order = Some(c.id as i32);
        }
        put_chapters(&pool, 1, &list).await.unwrap();
        let mut holder = pool.begin().await.unwrap();
        crate::locks::advisory(&mut holder, &series_publication_keys("1"))
            .await
            .unwrap();
        crate::locks::lock_source_series(&mut holder, &["ss_chapters"])
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            put_chapters(&pool, 1, &list),
        )
        .await
        .expect("an unchanged list waited for the guards of the series")
        .expect("an unchanged list waited for the guards of the series");
        // A changed list still waits for them.
        let writer = pool.clone();
        let mut changed =
            tokio::spawn(async move { put_chapters(&writer, 1, &[chapter(33, 1, 3.0)]).await });
        tokio::select! {
            _ = crate::cover::wait_for_writer_lock(&pool) => {}
            done = &mut changed => panic!("a changed list did not wait for the guards: {done:?}"),
        }
        holder.rollback().await.unwrap();
        changed.await.unwrap().unwrap();
        let cached: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM suwayomi_chapter WHERE manga_id=1 ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(cached, vec![33]);
    }

    #[tokio::test]
    async fn a_key_only_change_waits_for_the_series_guard_without_rewriting_display_rows() {
        let pool = pool().await.foreground();
        put_series(&pool, &manga(1, "Series")).await.unwrap();
        let mut c = chapter(11, 1, 1.0);
        c.url = Some("/chapter/11".into());
        c.source_order = Some(1);
        put_chapters(&pool, 1, &[c.clone()]).await.unwrap();
        let before: String =
            sqlx::query_scalar("SELECT xmin::text FROM suwayomi_chapter WHERE id = 11")
                .fetch_one(&pool)
                .await
                .unwrap();
        let mut holder = pool.begin().await.unwrap();
        crate::locks::advisory(&mut holder, &series_publication_keys("1"))
            .await
            .unwrap();
        c.source_order = Some(2);
        let writer = pool.clone();
        let mut changed = tokio::spawn(async move { put_chapters(&writer, 1, &[c]).await });
        tokio::select! {
            _ = crate::cover::wait_for_writer_lock(&pool) => {}
            done = &mut changed => panic!("a key-only change bypassed the series guard: {done:?}"),
        }
        let order: i32 =
            sqlx::query_scalar("SELECT source_order FROM engine_chapter_key WHERE chapter_id = 11")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(order, 1);
        holder.rollback().await.unwrap();
        changed.await.unwrap().unwrap();
        let (after, order): (String, i32) = sqlx::query_as(
            "SELECT c.xmin::text, k.source_order FROM suwayomi_chapter c \
             JOIN engine_chapter_key k ON k.chapter_id = c.id WHERE c.id = 11",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(order, 2);
        assert_eq!(
            after, before,
            "key-only changes must leave the display row untouched"
        );
    }

    /// A changed list locks the work and the mapping of its series BEFORE the series
    /// row and before it writes anything. Another transaction holds only the work row,
    /// or only the mapping row: the replacement must wait, and while it waits the
    /// series row must still be free. (The spine write locks both parents as well, but
    /// only after the series row and the cache rows: the wrong order.)
    #[tokio::test]
    async fn a_chapter_replacement_takes_its_work_and_mapping_before_the_series_row() {
        for held in ["work", "mapping"] {
            let pool = pool().await.foreground();
            mapped_series(&pool).await;
            let mut holder = pool.begin().await.unwrap();
            if held == "work" {
                crate::locks::lock_works(&mut holder, &["w_chapters"])
                    .await
                    .unwrap();
            } else {
                sqlx::query(
                    "SELECT id FROM source_series WHERE id = 'ss_chapters' FOR NO KEY UPDATE",
                )
                .execute(&mut *holder)
                .await
                .unwrap();
            }
            let writer = pool.clone();
            let mut replacement =
                tokio::spawn(async move { put_chapters(&writer, 1, &[chapter(11, 1, 1.0)]).await });
            tokio::select! {
                _ = crate::cover::wait_for_writer_lock(&pool) => {}
                done = &mut replacement => panic!("{held}: the replacement did not wait: {done:?}"),
            }
            assert!(
                series_row_is_free(&pool, 1).await,
                "{held}: the replacement holds the series row while it waits for a parent"
            );
            holder.rollback().await.unwrap();
            replacement.await.unwrap().unwrap();
            let spine: Vec<String> = sqlx::query_scalar("SELECT external_id FROM chapter WHERE source_series_id='ss_chapters' ORDER BY external_id").fetch_all(&pool).await.unwrap();
            assert_eq!(spine, vec!["11"], "{held}");
        }
    }

    /// The mapping set is read, locked, and read again. A mapping that a writer
    /// outside the key protocol commits between the first read and the locks is not
    /// locked: the unit must restart and lock it (and its work) at the parent tier.
    /// Here the work of that late mapping is held by another transaction, so the
    /// restarted unit waits for it, holding nothing below. A unit that went on with
    /// its first read would reach that work from the spine write, with the series row
    /// and the rewritten cache already locked.
    #[tokio::test]
    async fn a_mapping_that_appears_during_discovery_restarts_the_replacement() {
        let pool = pool().await.foreground();
        put_series(&pool, &manga(1, "Series")).await.unwrap();
        sqlx::query("INSERT INTO work(id,created_at,updated_at) VALUES ('w_late',now(),now())")
            .execute(&pool)
            .await
            .unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let discovered = points.arm("source-discovered");
        let writer = pool.clone();
        let mut replacement = tokio::spawn(
            crate::locks::SOURCE_DISCOVERY_PAUSE.scope(points, async move {
                put_chapters(&writer, 1, &[chapter(11, 1, 1.0)]).await
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(30), discovered.reached())
            .await
            .expect("the replacement did not reach its mapping discovery");
        sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES ('ss_late','w_late','suwayomi','src-1','1',now())")
            .execute(&pool)
            .await
            .unwrap();
        let mut holder = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut holder, &["w_late"])
            .await
            .unwrap();
        discovered.release();
        tokio::select! {
            _ = crate::cover::wait_for_writer_lock(&pool) => {}
            done = &mut replacement => panic!("the replacement did not wait for the late mapping's work: {done:?}"),
        }
        assert!(
            series_row_is_free(&pool, 1).await,
            "the replacement went on with a stale mapping set and holds the series row"
        );
        holder.rollback().await.unwrap();
        replacement.await.unwrap().unwrap();
        let spine: Vec<String> = sqlx::query_scalar(
            "SELECT external_id FROM chapter WHERE source_series_id='ss_late' ORDER BY external_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(spine, vec!["11"]);
    }

    /// Cache, spine and release ledger are one commit. The ledger rows exist as soon
    /// as the replacement returns, and a failed ledger write rolls the cache and the
    /// spine back with it, so the next scan (which then still sees a different list)
    /// writes all three again.
    #[tokio::test]
    async fn a_chapter_replacement_commits_its_ledger_rows_with_the_cache() {
        let pool = pool().await.foreground();
        mapped_series(&pool).await;
        let counts = || async {
            sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT (SELECT count(*) FROM suwayomi_chapter WHERE manga_id = 1), \
                        (SELECT count(*) FROM chapter WHERE source_series_id = 'ss_chapters'), \
                        (SELECT count(*) FROM release_event WHERE work_id = 'w_chapters')",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        sqlx::raw_sql(
            "CREATE FUNCTION reject_release_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               RAISE EXCEPTION 'injected ledger failure'; END $$; \
             CREATE TRIGGER reject_release BEFORE INSERT ON release_event \
               FOR EACH ROW EXECUTE FUNCTION reject_release_fn()",
        )
        .execute(&pool)
        .await
        .unwrap();
        let error = put_chapters(&pool, 1, &[chapter(11, 1, 1.0)])
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected ledger failure"));
        assert_eq!(
            counts().await,
            (0, 0, 0),
            "the cache committed without its ledger rows"
        );

        sqlx::query("DROP TRIGGER reject_release ON release_event")
            .execute(&pool)
            .await
            .unwrap();
        put_chapters(&pool, 1, &[chapter(11, 1, 1.0)])
            .await
            .unwrap();
        assert_eq!(counts().await, (1, 1, 1));
    }

    #[tokio::test]
    async fn round_trips_series_and_chapters() {
        let pool = pool().await;
        put_series(&pool, &manga(1, "Solo Leveling")).await.unwrap();
        let got = get_series(&pool, 1).await.unwrap().expect("cached");
        assert_eq!(got.title, "Solo Leveling");
        assert_eq!(got.genre, vec!["Action", "Comedy"]);
        assert_eq!(got.source.unwrap().lang.as_deref(), Some("en"));
        assert!(got.in_library);
        assert_eq!(got.thumbnail_url.as_deref(), Some("/thumb/1"));

        // Chapters replace + sync count.
        put_chapters(
            &pool,
            1,
            &[
                chapter(10, 1, 1.0),
                chapter(11, 1, 2.0),
                chapter(12, 1, 3.0),
            ],
        )
        .await
        .unwrap();
        let chs = get_chapters(&pool, 1).await.unwrap();
        assert_eq!(chs.len(), 3);
        assert_eq!(chs[0].chapter_number, 3.0, "newest first");
        let cnt: i64 =
            crate::db_types::query_scalar("SELECT chapter_count FROM suwayomi_series WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cnt, 3, "count synced to stored chapters");

        // Re-put fewer chapters replaces (no accumulation).
        put_chapters(&pool, 1, &[chapter(10, 1, 1.0)])
            .await
            .unwrap();
        assert_eq!(get_chapters(&pool, 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn put_series_refuses_non_english_but_keeps_unknown_lang() {
        let pool = pool().await;
        // Non-English → not cached (the durable English-only backstop).
        let mut es = manga(1, "Título en Español");
        es.source = Some(SuwayomiSourceLang {
            lang: Some("es".into()),
        });
        // The RETURN VALUE is a contract `persistCatalogue` depends on: `false` means
        // refused, so the caller must not count it as persisted nor queue the id for a
        // chapter fetch. Assert it alongside the DB effect — asserting only the effect
        // would let the flag silently invert.
        assert!(
            !put_series(&pool, &es).await.unwrap(),
            "a non-English series must report Ok(false), not a write"
        );
        assert!(
            get_series(&pool, 1).await.unwrap().is_none(),
            "non-English series must not be cached"
        );

        // Unknown (None) lang → allowed (don't drop a legit English series w/o a lang).
        let mut unknown = manga(2, "No Lang");
        unknown.source = None;
        assert!(put_series(&pool, &unknown).await.unwrap());
        assert!(get_series(&pool, 2).await.unwrap().is_some());

        // English → cached as before.
        assert!(put_series(&pool, &manga(3, "English")).await.unwrap());
        assert!(get_series(&pool, 3).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn put_chapters_skips_when_series_not_cached() {
        let pool = pool().await;
        // No series row (e.g. a non-English series put_series refused) → chapters skipped,
        // so the chapter cache can't resurrect content for an un-cached series.
        put_chapters(&pool, 99, &[chapter(1, 99, 1.0)])
            .await
            .unwrap();
        assert!(
            get_chapters(&pool, 99).await.unwrap().is_empty(),
            "chapters for an un-cached series must not be stored"
        );

        // Once the (English) series is cached, chapters store normally.
        put_series(&pool, &manga(99, "Eng")).await.unwrap();
        put_chapters(&pool, 99, &[chapter(1, 99, 1.0)])
            .await
            .unwrap();
        assert_eq!(get_chapters(&pool, 99).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn recently_added_orders_by_first_add_and_preserves_it() {
        let pool = pool().await;
        for i in 1..=3 {
            put_series(&pool, &manga(i, &format!("S{i}")))
                .await
                .unwrap();
        }
        // A non-library series is excluded from the catalogue "Latest Added" row.
        let mut out = manga(4, "Excluded");
        out.in_library = false;
        put_series(&pool, &out).await.unwrap();

        // Pin distinct first-add times so ordering is deterministic (1 oldest, 3 newest).
        for (id, ts) in [
            (1, "2026-01-01T00:00:00Z"),
            (2, "2026-02-01T00:00:00Z"),
            (3, "2026-03-01T00:00:00Z"),
        ] {
            sqlx::query("UPDATE suwayomi_series SET created_at = $1 WHERE id = $2")
                .bind(crate::db_types::date(ts).unwrap())
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }

        // show_nsfw = true short-circuits the NSFW gate, preserving this test's
        // original intent (it asserts ordering, not filtering).
        let recent = recently_added(&pool, 10, true).await.unwrap();
        assert_eq!(
            recent.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![3, 2, 1],
            "newest-added first, in-library only"
        );

        // Re-upserting an existing series must NOT reset its first-add time.
        put_series(&pool, &manga(1, "S1 refreshed")).await.unwrap();
        let kept: Option<String> =
            crate::db_types::query_scalar("SELECT created_at FROM suwayomi_series WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            kept.as_deref(),
            Some("2026-01-01T00:00:00.000000+00:00"),
            "created_at preserved on upsert"
        );
    }

    #[tokio::test]
    async fn library_and_count() {
        let pool = pool().await;
        put_series(&pool, &manga(1, "A")).await.unwrap();
        let mut m2 = manga(2, "B");
        m2.in_library = false;
        put_series(&pool, &m2).await.unwrap();
        assert_eq!(count(&pool).await.unwrap(), 2);
        let lib = library(&pool, 10, true).await.unwrap();
        assert_eq!(lib.len(), 1, "only in-library series");
        assert_eq!(lib[0].id, 1);
    }

    /// The NSFW gate must run IN SQL, before LIMIT — an anonymous viewer asking for
    /// N rows should get N safe rows, not N rows with the unsafe ones removed
    /// afterwards. Regression test for `discovery` returning near-empty feeds.
    #[tokio::test]
    async fn feeds_gate_nsfw_in_sql_before_limit() {
        let pool = pool().await;
        // Three in-library series; the middle one is linked to an NSFW work.
        for id in [1_i64, 2, 3] {
            put_series(&pool, &manga(id, "T")).await.unwrap();
        }
        sqlx::query(
            r#"INSERT INTO work (id, primary_title, is_nsfw, created_at, updated_at) VALUES ('w_x', 'X', TRUE, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss_x', 'w_x', 'suwayomi', '2', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let anon = library(&pool, 10, false).await.unwrap();
        assert!(
            !anon.iter().any(|m| m.id == 2),
            "NSFW-linked series must not reach an anonymous viewer"
        );
        assert_eq!(anon.len(), 2, "the other two survive");

        // Asking for exactly 1 must yield a SAFE row, not an empty page — this is the
        // property that post-LIMIT filtering broke.
        let one = library(&pool, 1, false).await.unwrap();
        assert_eq!(one.len(), 1, "a full page of safe rows, not a trimmed one");
        assert_ne!(one[0].id, 2);

        let opted_in = library(&pool, 10, true).await.unwrap();
        assert_eq!(opted_in.len(), 3, "opted-in viewer still sees everything");
    }

    /// `put_chapters` records the real newest-chapter time (MAX upload_date), and the
    /// `library` feed orders by it — not by poll time. Regression for the -0.06
    /// correlation between `updated_at` and actual chapter recency.
    #[tokio::test]
    async fn library_orders_by_real_latest_chapter_not_poll_time() {
        let pool = pool().await;
        for id in [1_i64, 2, 3] {
            put_series(&pool, &manga(id, "S")).await.unwrap();
        }

        // Series 2 has the newest chapter; 1 is middle; 3 is oldest. `upload_date` is a
        // millis-epoch string, matching the source.
        let ch = |id: i64, mid: i64, up: &str| {
            let mut c = chapter(id, mid, 1.0);
            c.upload_date = Some(up.into());
            c
        };
        put_chapters(&pool, 1, &[ch(11, 1, "1600000000000")])
            .await
            .unwrap();
        put_chapters(&pool, 2, &[ch(21, 2, "1700000000000")])
            .await
            .unwrap();
        put_chapters(&pool, 3, &[ch(31, 3, "1500000000000")])
            .await
            .unwrap();

        let lib = library(&pool, 10, true).await.unwrap();
        assert_eq!(
            lib.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![2, 1, 3],
            "ordered by newest chapter, not by which was polled last"
        );
        assert_eq!(
            lib[0].latest_chapter_at.as_deref(),
            Some("1700000000000"),
            "the field carries the real normalized MAX(upload_date)"
        );

        // A 10-digit seconds value is normalized to millis, and still orders correctly
        // relative to a 13-digit millis value that is chronologically newer.
        put_chapters(&pool, 1, &[ch(11, 1, "1750000000")]) // seconds (~2025)
            .await
            .unwrap();
        let one = library(&pool, 10, true)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.id == 1)
            .unwrap();
        assert_eq!(
            one.latest_chapter_at.as_deref(),
            Some("1750000000000"),
            "seconds-epoch normalized to millis"
        );

        // A dateless re-scan must NOT wipe a good value (COALESCE guard).
        put_chapters(&pool, 2, &[ch(99, 2, "")]).await.unwrap();
        let two = library(&pool, 10, true)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.id == 2)
            .unwrap();
        assert_eq!(
            two.latest_chapter_at.as_deref(),
            Some("1700000000000"),
            "empty-batch scan preserves the prior newest-chapter time"
        );
    }

    /// The load-bearing invariant: `latest_chapter_at` only ever holds an upstream
    /// `upload_date`. A series we have never chaptered — or one whose chapters carry no
    /// usable date — must stay NULL rather than being stamped with our own clock.
    ///
    /// A clock value here is exactly the "the feed says 1 hour ago but the chapter is
    /// really days old" bug: it is indistinguishable from a real release time once
    /// stored, it renders as "Updated now", and it pins the series to the top of every
    /// feed (the feed ORDER BY is this column).
    #[tokio::test]
    async fn latest_chapter_at_is_never_derived_from_our_clock() {
        let pool = pool().await;
        let mid = 777_101_i64;
        let stored = |pool: Database| async move {
            crate::db_types::query_scalar::<Option<String>>(
                "SELECT latest_chapter_at FROM suwayomi_series WHERE id = $1",
            )
            .bind(mid)
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        // First touch: the series exists, no chapters known yet.
        put_series(&pool, &manga(mid, "Cold")).await.unwrap();
        assert_eq!(
            stored(pool.clone()).await,
            None,
            "a never-chaptered series must be NULL, not 'now'"
        );

        // Chapters arrive, but none of them is datable — still NULL.
        let dateless = |id: i64, up: Option<&str>| {
            let mut c = chapter(id, mid, id as f64);
            c.upload_date = up.map(Into::into);
            c
        };
        put_chapters(
            &pool,
            mid,
            &[
                dateless(1, None),
                dateless(2, Some("")),
                dateless(3, Some("not-a-number")),
                dateless(4, Some("0")),
                dateless(5, Some("-1700000000000")),
                // Corrupt far-future value (the live catalogue has one ~year 3800).
                dateless(6, Some("57766321270698")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            stored(pool.clone()).await,
            None,
            "undatable/garbage chapters must not fall back to a clock value"
        );

        // And the pure derivation agrees, independent of the DB.
        assert_eq!(derive_latest_chapter_at(&[]), None);
        assert_eq!(
            derive_latest_chapter_at(&[dateless(7, Some("1700000000000")), dateless(8, None)]),
            Some("1700000000000".to_string()),
        );
        assert_eq!(
            derive_latest_chapter_at(&[
                dateless(9, Some("1750000000")),      // seconds → millis
                dateless(10, Some("57766321270698")), // corrupt future → dropped
            ]),
            Some("1750000000000".to_string()),
            "the corrupt future value must not win the max",
        );
    }

    /// A LIVE-fetched manga carries no `latest_chapter_at` (it isn't in Suwayomi's wire
    /// shape) while its `last_fetched_at` is stamped to now by the very fetch we made —
    /// so the display layer's `latestChapterAt || updatedAt` fallback renders "Updated
    /// now" for a series whose real newest-chapter time we already hold. Hydrating from
    /// the cache closes that hole without inventing a timestamp.
    #[tokio::test]
    async fn hydrate_latest_chapter_at_fills_a_live_fetched_manga() {
        let pool = pool().await;
        let (chaptered, cold) = (777_102_i64, 777_103_i64);
        put_series(&pool, &manga(chaptered, "Known")).await.unwrap();
        put_series(&pool, &manga(cold, "Cold")).await.unwrap();
        let mut ch = chapter(1, chaptered, 1.0);
        ch.upload_date = Some("1700000000000".into());
        put_chapters(&pool, chaptered, &[ch]).await.unwrap();

        // The shape a live fetch produces: no latest_chapter_at, a fresh poll time.
        let mut live = manga(chaptered, "Known");
        live.latest_chapter_at = None;
        hydrate_latest_chapter_at(&pool, &mut live).await;
        assert_eq!(live.latest_chapter_at.as_deref(), Some("1700000000000"));

        // A series with no known chapter stays None — never a clock value.
        let mut live_cold = manga(cold, "Cold");
        hydrate_latest_chapter_at(&pool, &mut live_cold).await;
        assert_eq!(live_cold.latest_chapter_at, None);

        // An id we have never seen is a miss, not an error.
        let mut unknown = manga(777_104, "Unknown");
        hydrate_latest_chapter_at(&pool, &mut unknown).await;
        assert_eq!(unknown.latest_chapter_at, None);

        // An already-populated value (a cache read) is left alone.
        let mut cached = manga(chaptered, "Known");
        cached.latest_chapter_at = Some("1600000000000".into());
        hydrate_latest_chapter_at(&pool, &mut cached).await;
        assert_eq!(cached.latest_chapter_at.as_deref(), Some("1600000000000"));
    }

    /// Staleness: a `latest_chapter_at` that no longer matches the cached chapter set is
    /// repaired on the next scan — including on the skip-when-unchanged path, which is
    /// the path ≥95% of scans take. The repair must converge (one write, then zero), or
    /// it reintroduces the per-scan write that cost 1,786 MB/day of WAL.
    #[tokio::test]
    async fn put_chapters_repairs_a_stale_latest_chapter_at_without_rewriting_chapters() {
        let pool = pool().await;
        let mid = 777_105_i64;
        put_series(&pool, &manga(mid, "Stale")).await.unwrap();
        let mut ch = chapter(1, mid, 1.0);
        ch.upload_date = Some("1750000000000".into());
        let chs = [ch];
        put_chapters(&pool, mid, &chs).await.unwrap();

        let series_row = |pool: Database| async move {
            crate::db_types::query_tuple::<(Option<String>, String)>(
                "SELECT latest_chapter_at, updated_at FROM suwayomi_series WHERE id = $1",
            )
            .bind(mid)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let chapter_stamp = |pool: Database| async move {
            crate::db_types::query_scalar::<String>(
                "SELECT MAX(updated_at) FROM suwayomi_chapter WHERE manga_id = $1",
            )
            .bind(mid)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let ch_before = chapter_stamp(pool.clone()).await;

        // Simulate a row left behind by an older binary / migration 0050's backfill:
        // the chapter set has moved on, the derived column has not.
        sqlx::query("UPDATE suwayomi_series SET latest_chapter_at = $1 WHERE id = $2")
            .bind(1_600_000_000_000_i64)
            .bind(mid)
            .execute(&pool)
            .await
            .unwrap();

        // An ordinary scan finds the identical chapter list — and still heals the column.
        put_chapters(&pool, mid, &chs).await.unwrap();
        let (healed, updated_after_repair) = series_row(pool.clone()).await;
        assert_eq!(
            healed.as_deref(),
            Some("1750000000000"),
            "the column must be recomputed from the cached chapter set"
        );
        assert_eq!(
            chapter_stamp(pool.clone()).await,
            ch_before,
            "healing the series row must not rewrite the chapter rows"
        );

        // ...and the very next scan of the same unchanged list writes NOTHING.
        put_chapters(&pool, mid, &chs).await.unwrap();
        let (still, updated_after_noop) = series_row(pool.clone()).await;
        assert_eq!(still.as_deref(), Some("1750000000000"));
        assert_eq!(
            updated_after_repair, updated_after_noop,
            "the repair must converge — no write on a subsequent unchanged scan"
        );
    }

    /// P1-1: an unchanged chapter list must not be rewritten. `scanner::persist_scan`
    /// calls `put_chapters` on every tick (~24k/day) and ≥95% of those find no new
    /// chapter; each one used to DELETE and re-INSERT the whole list for a
    /// byte-identical result, which Litestream then shipped to R2.
    #[tokio::test]
    async fn put_chapters_skips_the_rewrite_when_nothing_changed() {
        let pool = pool().await;
        // A dedicated id: the fetch memo is a process-wide static keyed by manga id.
        let mid = 777_001_i64;
        let mut m = manga(mid, "Unchanged");
        m.chapters = Some(ChapterCount { total_count: 3 });
        put_series(&pool, &m).await.unwrap();
        let chs = [
            chapter(10, mid, 1.0),
            chapter(11, mid, 2.0),
            chapter(12, mid, 3.0),
        ];
        put_chapters(&pool, mid, &chs).await.unwrap();

        let stamp = |pool: Database| async move {
            let ch: Option<String> = crate::db_types::query_scalar(
                "SELECT MAX(updated_at) FROM suwayomi_chapter WHERE manga_id = $1",
            )
            .bind(mid)
            .fetch_one(&pool)
            .await
            .unwrap();
            let se: String = crate::db_types::query_scalar(
                "SELECT updated_at FROM suwayomi_series WHERE id = $1",
            )
            .bind(mid)
            .fetch_one(&pool)
            .await
            .unwrap();
            (ch.unwrap(), se)
        };
        let (ch_before, series_before) = stamp(pool.clone()).await;

        // Re-scan with the identical list → ZERO writes to either table.
        put_chapters(&pool, mid, &chs).await.unwrap();
        let (ch_after, series_after) = stamp(pool.clone()).await;
        assert_eq!(ch_before, ch_after, "chapter rows must not be rewritten");
        assert_eq!(
            series_before, series_after,
            "the series row must not be rewritten either"
        );

        // ...but the reader's chapter-freshness gate still sees a fresh FETCH, so a
        // skipped write can't turn the cache permanently stale.
        let last = chapters_last_fetched(&pool, mid).await.unwrap().unwrap();
        assert!(
            last > ch_before,
            "the fetch is recorded even when nothing was written ({last} vs {ch_before})"
        );
        assert!(is_fresh(Some(&last), CHAPTERS_TTL_SECS));

        // A drifted derived column IS repaired — `put_series` runs immediately before
        // this on every scan and overwrites chapter_count with the source's raw total.
        let mut inflated = manga(mid, "Unchanged");
        inflated.chapters = Some(ChapterCount { total_count: 99 });
        put_series(&pool, &inflated).await.unwrap();
        put_chapters(&pool, mid, &chs).await.unwrap();
        let cnt: i64 = crate::db_types::query_scalar(
            "SELECT chapter_count FROM suwayomi_series WHERE id = $1",
        )
        .bind(mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            cnt, 3,
            "deduped count re-asserted without touching chapters"
        );
        let (ch_still, _) = stamp(pool.clone()).await;
        assert_eq!(ch_still, ch_before, "chapter rows still untouched");
    }

    #[tokio::test]
    async fn spine_failure_rolls_back_cache_and_identical_fetch_retries() {
        let pool = pool().await;
        let mid = 777_106;
        put_series(&pool, &manga(mid, "Atomic chapters"))
            .await
            .unwrap();
        let work = crate::catalog::create_work(&pool, &crate::catalog::WorkInput::default())
            .await
            .unwrap();
        let ssid = crate::catalog::upsert_source_series(
            &pool,
            &work,
            "suwayomi",
            "src-1",
            &mid.to_string(),
            None,
            false,
        )
        .await
        .unwrap();
        put_chapters(&pool, mid, &[chapter(10, mid, 1.0)])
            .await
            .unwrap();
        let old_series: (i64, Option<i64>, String) = crate::db_types::query_tuple(
            "SELECT chapter_count, latest_chapter_at, updated_at FROM suwayomi_series WHERE id = $1",
        )
        .bind(mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        let old_spine: Vec<(String, String, String)> = crate::db_types::query_tuple(
            "SELECT external_id, id, created_at FROM chapter WHERE source_series_id = $1 ORDER BY external_id",
        )
        .bind(&ssid)
        .fetch_all(&pool)
        .await
        .unwrap();
        let old_fetch = chapters_last_fetched(&pool, mid).await.unwrap();
        sqlx::raw_sql(
            "CREATE FUNCTION fail_new_spine_chapter_fn() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN IF NEW.external_id = '11' THEN RAISE EXCEPTION 'forced spine failure'; END IF; \
             RETURN NEW; END $$; CREATE TRIGGER fail_new_spine_chapter BEFORE INSERT ON chapter \
             FOR EACH ROW EXECUTE FUNCTION fail_new_spine_chapter_fn()",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut newest = chapter(11, mid, 2.0);
        newest.upload_date = Some("1750000000000".into());
        let incoming = [chapter(10, mid, 1.0), newest];
        let error = put_chapters(&pool, mid, &incoming).await.unwrap_err();
        assert!(error.to_string().contains("forced spine failure"));
        let cached = get_chapters(&pool, mid).await.unwrap();
        assert_eq!(cached.iter().map(|ch| ch.id).collect::<Vec<_>>(), vec![10]);
        let series_after: (i64, Option<i64>, String) = crate::db_types::query_tuple(
            "SELECT chapter_count, latest_chapter_at, updated_at FROM suwayomi_series WHERE id = $1",
        )
        .bind(mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(series_after, old_series);
        let spine_after: Vec<(String, String, String)> = crate::db_types::query_tuple(
            "SELECT external_id, id, created_at FROM chapter WHERE source_series_id = $1 ORDER BY external_id",
        )
        .bind(&ssid)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(spine_after, old_spine);
        assert_eq!(chapters_last_fetched(&pool, mid).await.unwrap(), old_fetch);

        sqlx::query("DROP TRIGGER fail_new_spine_chapter ON chapter")
            .execute(&pool)
            .await
            .unwrap();
        // Exactly the same upstream list must heal both structures after the failure.
        put_chapters(&pool, mid, &incoming).await.unwrap();
        assert_eq!(get_chapters(&pool, mid).await.unwrap().len(), 2);
        let spine: Vec<(String, String, String)> = crate::db_types::query_tuple(
            "SELECT external_id, id, created_at FROM chapter WHERE source_series_id = $1 ORDER BY external_id",
        )
        .bind(&ssid)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(spine.len(), 2);
        assert_eq!(
            spine[0], old_spine[0],
            "the survivor retains its identity and first sighting"
        );
        assert_eq!(spine[1].0, "11");
        assert_eq!(
            get_series(&pool, mid)
                .await
                .unwrap()
                .unwrap()
                .chapters
                .unwrap()
                .total_count,
            2
        );
    }

    /// The other half of P1-1: a list that really changed is still replaced — now via
    /// a chunked multi-row INSERT rather than one statement per chapter.
    #[tokio::test]
    async fn put_chapters_rewrites_when_the_list_changed() {
        let pool = pool().await;
        let mid = 777_002_i64;
        put_series(&pool, &manga(mid, "Changing")).await.unwrap();
        put_chapters(&pool, mid, &[chapter(10, mid, 1.0), chapter(11, mid, 2.0)])
            .await
            .unwrap();

        // A new chapter appears.
        put_chapters(
            &pool,
            mid,
            &[
                chapter(10, mid, 1.0),
                chapter(11, mid, 2.0),
                chapter(12, mid, 3.0),
            ],
        )
        .await
        .unwrap();
        assert_eq!(get_chapters(&pool, mid).await.unwrap().len(), 3);

        // A mutable field changing on an otherwise-identical id set also rewrites.
        let mut renamed = chapter(12, mid, 3.0);
        renamed.name = "Ch 3 (v2)".into();
        renamed.page_count = 42;
        put_chapters(
            &pool,
            mid,
            &[chapter(10, mid, 1.0), chapter(11, mid, 2.0), renamed],
        )
        .await
        .unwrap();
        let stored = get_chapters(&pool, mid).await.unwrap();
        let ch3 = stored.iter().find(|c| c.id == 12).unwrap();
        assert_eq!(ch3.name, "Ch 3 (v2)");
        assert_eq!(ch3.page_count, 42);

        // A list longer than one INSERT chunk round-trips intact.
        let many: Vec<SuwayomiChapter> = (0..CHAPTER_INSERT_CHUNK as i64 + 37)
            .map(|i| chapter(1000 + i, mid, i as f64))
            .collect();
        put_chapters(&pool, mid, &many).await.unwrap();
        assert_eq!(get_chapters(&pool, mid).await.unwrap().len(), many.len());
    }

    /// P0-2: the effective NSFW flag is `COALESCE(is_nsfw_override, is_nsfw)` — both
    /// admin "mark NSFW" mutations write ONLY the override — so a filter testing the
    /// raw column silently ignored the admin's decision in both directions.
    #[tokio::test]
    async fn nsfw_filters_honour_the_admin_override() {
        let pool = pool().await;
        for id in [1_i64, 2, 3] {
            put_series(&pool, &manga(id, "T")).await.unwrap();
        }
        // Series 2: crawled SAFE, admin forced NSFW. Series 3: crawled NSFW, admin
        // cleared it.
        for (wid, sid, raw, over) in [("w_a", "2", 0, 1), ("w_b", "3", 1, 0)] {
            sqlx::query(
                "INSERT INTO work (id, primary_title, is_nsfw, is_nsfw_override, created_at, updated_at) \
                 VALUES ($1, 'X', $2, $3, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(wid)
            .bind(raw != 0)
            .bind(over != 0)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
                 VALUES ($1, $2, 'suwayomi', $3, '2026-01-01T00:00:00Z')",
            )
            .bind(format!("ss_{wid}"))
            .bind(wid)
            .bind(sid)
            .execute(&pool)
            .await
            .unwrap();
        }

        let anon: Vec<i64> = library(&pool, 10, false)
            .await
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert!(
            !anon.contains(&2),
            "override-on must hide a crawled-safe work"
        );
        assert!(
            anon.contains(&3),
            "override-off must reveal a crawled-NSFW work"
        );

        // `recently_added` shares the exact same gate constant, so it must agree — the two
        // are the only remaining callers, and a divergence between them would mean one home
        // row honoured the admin and the other did not.
        let added: Vec<i64> = recently_added(&pool, 10, false)
            .await
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert!(!added.contains(&2) && added.contains(&3));

        // Browse used to be the third caller, and its paginated `total` was the thing that
        // stayed wrong after the resolvers got their Rust-side backstop (a skewed count means
        // wrong page counts / `hasNext`). Browse now reads `browse_catalogue.is_nsfw`, a
        // MATERIALIZED copy of this same COALESCE that `graphql::resync_feed_nsfw` rewrites on
        // every admin mark — covered by `browse_total_and_page_follow_an_admin_nsfw_mark`, not
        // from here.
    }

    /// `CHAPTER_FETCH_MEMO`'s cap has to be a HARD bound. Expiring entries older than
    /// the chapter TTL is only advisory: if every entry is still live the retain frees
    /// nothing, and the map would grow without bound while paying an O(n) scan under
    /// its mutex on the scanner's hot path.
    #[test]
    fn chapter_fetch_memo_is_hard_bounded_when_every_entry_is_live() {
        // Isolate from other tests, which use small real manga ids.
        CHAPTER_FETCH_MEMO.lock().unwrap().clear();
        // Process-global counters: other tests move them too, so only lower bounds.
        let before = crate::telemetry::CACHE_CHAPTER_FETCH_MARKER.snapshot();
        let now = Utc::now();
        // Every stamp is `now`, i.e. inside CHAPTERS_TTL_SECS — the degenerate case.
        for id in 0..(CHAPTER_FETCH_MEMO_CAP as i64 + 64) {
            note_chapters_fetched(id, now);
        }
        let len = CHAPTER_FETCH_MEMO.lock().unwrap().len();
        assert!(
            len <= CHAPTER_FETCH_MEMO_CAP,
            "memo must never exceed its cap (len {len})"
        );
        assert_eq!(
            len, CHAPTER_FETCH_MEMO_CAP,
            "one new id must not flush live markers"
        );
        let retained = CHAPTER_FETCH_MEMO_CAP as i64 - 1;
        assert_eq!(memoized_chapters_fetched(retained), Some(now));
        note_chapters_fetched(retained, now);
        assert_eq!(
            CHAPTER_FETCH_MEMO.lock().unwrap().len(),
            len,
            "an existing id must not evict other markers"
        );
        // One eviction safely degrades to the durable MAX(updated_at) floor.
        let last = CHAPTER_FETCH_MEMO_CAP as i64 + 63;
        assert_eq!(memoized_chapters_fetched(last), Some(now));
        let after = crate::telemetry::CACHE_CHAPTER_FETCH_MARKER.snapshot();
        assert!(
            after.evictions >= before.evictions + 64,
            "one eviction per new id"
        );
        assert!(after.fills >= before.fills + CHAPTER_FETCH_MEMO_CAP as u64 + 65);
        assert!(after.hits >= before.hits + 2);
        CHAPTER_FETCH_MEMO.lock().unwrap().clear();
    }

    #[test]
    fn a_marker_past_the_chapter_ttl_is_a_miss_not_a_hit() {
        let stats = crate::telemetry::CacheStats::leak();
        let now = Utc::now();
        count_marker_lookup(stats, Some(now), now);
        count_marker_lookup(
            stats,
            Some(now - chrono::Duration::seconds(CHAPTERS_TTL_SECS + 1)),
            now,
        );
        count_marker_lookup(stats, None, now);
        let seen = stats.snapshot();
        assert_eq!((seen.hits, seen.misses), (1, 2));
    }

    #[test]
    fn normalize_epoch_millis_handles_dirty_source() {
        assert_eq!(
            normalize_epoch_millis("1700000000000"),
            Some(1_700_000_000_000)
        ); // 13-digit millis
        assert_eq!(
            normalize_epoch_millis("1750000000"),
            Some(1_750_000_000_000)
        ); // 10-digit seconds
        assert_eq!(
            normalize_epoch_millis(" 1750000000 "),
            Some(1_750_000_000_000)
        ); // trimmed
        assert_eq!(normalize_epoch_millis(""), None);
        assert_eq!(normalize_epoch_millis("abc"), None);
        assert_eq!(normalize_epoch_millis("0"), None);
        assert_eq!(normalize_epoch_millis("-126054423982000"), None); // corrupt negative
        assert_eq!(normalize_epoch_millis("11251332011000"), None); // year-2326 corrupt outlier
    }
}
