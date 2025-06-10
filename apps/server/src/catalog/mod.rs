//! Canonical catalogue repository (CATALOGUE.md §3).
//!
//! The persistence layer over `work` / `work_alias` / `work_external_id` /
//! `source_series` / `chapter` / `merge_candidate`. Pure sqlx — no network. The
//! MangaDex sync (`crate::mangadex`) writes through `upsert_work_from_mangadex`;
//! the dedup matcher (`crate::dedup`) reads through the `find_*` / `load_match_data`
//! queries. Runtime queries only (matching the rest of the crate), so the build
//! needs no sqlx offline metadata.

pub mod normalize;
pub mod similarity;

use anyhow::Result;
use chrono::Utc;
use sqlx::SqlitePool;
use uuid::Uuid;

use normalize::normalize_title;

/// A single alt-title with its language tag (from MangaDex `altTitles`).
#[derive(Debug, Clone)]
pub struct Alias {
    pub raw: String,
    pub lang: Option<String>,
}

/// One cover of a work (F2). `file_name` is the `covers/{mangadex_id}/{leaf}`
/// path leaf; `is_primary` marks the work's main cover (the one mirrored on
/// `work.cover_file_name`).
#[derive(Debug, Clone)]
pub struct Cover {
    pub file_name: String,
    pub lang: Option<String>,
    pub volume: Option<String>,
    pub is_primary: bool,
}

/// Everything needed to upsert one canonical work. Built from a MangaDex manga, or
/// synthesized for a first-class non-MangaDex work added via a Tier-2 source.
#[derive(Debug, Clone, Default)]
pub struct WorkInput {
    pub primary_title: Option<String>,
    pub primary_lang: Option<String>,
    pub description: Option<String>,
    pub year: Option<i64>,
    pub original_language: Option<String>,
    pub status: Option<String>,
    pub demographic: Option<String>,
    pub content_rating: Option<String>,
    pub is_nsfw: bool,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub cover_phash: Option<String>,
    /// MangaDex cover fileName (e.g. `abc.jpg`); builds the cover URL for reader
    /// browse/read of canonical works (CATALOGUE.md §5).
    pub cover_file_name: Option<String>,
    pub aliases: Vec<Alias>,
    /// `(provider, external_id)` — e.g. `("al", "12345")`, `("mangadex", "<uuid>")`.
    pub external_ids: Vec<(String, String)>,
    /// Every localized description as `(lang, text)` (S2). The singular
    /// `description` above stays the English-preferred primary.
    pub descriptions: Vec<(String, String)>,
    /// Full credit list as `(role, name)` with role `author`/`artist` (S2). The
    /// singular `author`/`artist` above keep the first of each.
    pub credits: Vec<(String, String)>,
    /// Upstream genre/theme tags, already ordered (migration 0066). Written as the
    /// `source = 'mangadex'` half of `work_tag`; the admin-curated half is never
    /// touched. Empty for a Tier-2 work with no upstream tag list, which is a
    /// legitimate state — see `replace_source_tags`.
    pub tags: Vec<String>,
    /// Covers to store for this work (F2). Empty leaves any existing covers
    /// untouched; non-empty REPLACES the work's cover set (the sweep passes just
    /// the primary, the enrichment path passes the full `/cover` set).
    pub covers: Vec<Cover>,
}

/// One mirrored chapter to upsert under a `source_series`.
#[derive(Debug, Clone, Default)]
pub struct ChapterInput {
    pub external_id: String,
    pub number: Option<String>,
    pub volume: Option<String>,
    pub lang: Option<String>,
    pub title: Option<String>,
    pub published_at: Option<String>,
}

/// The fields the matcher needs to corroborate a candidate work. A complete DTO —
/// a few fields (id/title/language) are carried for callers/future signals even
/// though the current scorer reads only description/author/year/cover/aliases.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct WorkMatchData {
    pub work_id: String,
    pub primary_title: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub year: Option<i64>,
    pub original_language: Option<String>,
    pub cover_phash: Option<String>,
    /// Normalized alias keys for this work (includes the primary title).
    pub aliases_norm: Vec<String>,
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple())
}

/// Find the work already claiming `(provider, external_id)`, if any. The
/// highest-precision match key (CATALOGUE.md §4, step 1).
pub async fn find_work_by_external(
    pool: &SqlitePool,
    provider: &str,
    external_id: &str,
) -> Result<Option<String>> {
    let id = sqlx::query_scalar::<_, String>(
        "SELECT work_id FROM work_external_id WHERE provider = ? AND external_id = ?",
    )
    .bind(provider)
    .bind(external_id)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Distinct work ids whose alias index contains this exact normalized title
/// (step 2). More than one means an ambiguous title that step 4 must disambiguate.
pub async fn find_works_by_alias(pool: &SqlitePool, normalized: &str) -> Result<Vec<String>> {
    if normalized.is_empty() {
        return Ok(Vec::new());
    }
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT work_id FROM work_alias WHERE normalized_title = ?",
    )
    .bind(normalized)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Fuzzy blocking (step 3): distinct work ids whose alias index contains `token` as
/// a whole word, capped at `limit`. `token` is a whole word of the candidate's
/// normalized title (the fuzzy block keys on the top-N longest words), so the block
/// is selective.
///
/// H9: this is an index-usable EXACT-TOKEN lookup against the `work_alias_token`
/// inverted index (one row per work+word), replacing the old leading-wildcard
/// `normalized_title LIKE '%token%'` full-scan of `work_alias`. Word-level recall is
/// preserved because the block always feeds whole normalized words (never substrings)
/// and every alias word is indexed — so a distinctive mid/end-title word like "slime"
/// in "...as a Slime" still matches, now via the index instead of a scan.
pub async fn candidate_work_ids_by_token(
    pool: &SqlitePool,
    token: &str,
    limit: i64,
) -> Result<Vec<String>> {
    if token.len() < 2 {
        return Ok(Vec::new());
    }
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT work_id FROM work_alias_token WHERE token = ? LIMIT ?",
    )
    .bind(token)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Split a normalized title into its indexable word tokens (whitespace-separated,
/// byte-length ≥ 2 to mirror the lookup's `token.len() < 2` guard). The
/// `work_alias_token` inverted index is populated from these on every alias write.
fn alias_word_tokens(normalized_title: &str) -> Vec<&str> {
    normalized_title
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .collect()
}

/// Upsert the word tokens of one normalized alias into the `work_alias_token`
/// inverted index (H9). Idempotent via `INSERT OR IGNORE` on the UNIQUE(work_id,
/// token) key. Runs inside the caller's alias-write transaction.
async fn insert_alias_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    work_id: &str,
    normalized_title: &str,
) -> Result<()> {
    for token in alias_word_tokens(normalized_title) {
        sqlx::query("INSERT OR IGNORE INTO work_alias_token (work_id, token) VALUES (?, ?)")
            .bind(work_id)
            .bind(token)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Load the corroboration fields + normalized aliases for one work.
pub async fn load_match_data(pool: &SqlitePool, work_id: &str) -> Result<Option<WorkMatchData>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        primary_title: Option<String>,
        description: Option<String>,
        author: Option<String>,
        year: Option<i64>,
        original_language: Option<String>,
        cover_phash: Option<String>,
    }
    let row = sqlx::query_as::<_, Row>(
        "SELECT primary_title, description, author, year, original_language, cover_phash \
         FROM work WHERE id = ?",
    )
    .bind(work_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let aliases_norm = sqlx::query_scalar::<_, String>(
        "SELECT normalized_title FROM work_alias WHERE work_id = ?",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    Ok(Some(WorkMatchData {
        work_id: work_id.to_string(),
        primary_title: row.primary_title,
        description: row.description,
        author: row.author,
        year: row.year,
        original_language: row.original_language,
        cover_phash: row.cover_phash,
        aliases_norm,
    }))
}

/// Batch variant of [`load_match_data`]: loads the corroboration fields + normalized
/// aliases for many works in exactly two queries (one `work` scan + one `work_alias`
/// scan, both `IN (...)`) instead of `2·N` sequential round-trips. The dedup matcher
/// (`resolve_ex`) scores every blocked candidate, so on a full reconcile this collapses
/// up to ~300 round-trips per item into two. Returns a map keyed by `work_id`; ids with
/// no `work` row are simply absent (mirrors `load_match_data` returning `None`).
///
/// `work_ids` is bounded by the fuzzy-block limits (a few hundred), well under SQLite's
/// bound-parameter ceiling, so the `IN (...)` list is not chunked.
pub async fn load_match_data_batch(
    pool: &SqlitePool,
    work_ids: &[String],
) -> Result<std::collections::HashMap<String, WorkMatchData>> {
    let mut out: std::collections::HashMap<String, WorkMatchData> =
        std::collections::HashMap::new();
    if work_ids.is_empty() {
        return Ok(out);
    }
    // Dedup to keep the placeholder list tight (a HashSet of candidates may already be
    // distinct, but callers aren't required to guarantee it).
    let mut ids: Vec<&String> = work_ids.iter().collect();
    ids.sort();
    ids.dedup();
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");

    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        primary_title: Option<String>,
        description: Option<String>,
        author: Option<String>,
        year: Option<i64>,
        original_language: Option<String>,
        cover_phash: Option<String>,
    }
    let work_sql = format!(
        "SELECT id, primary_title, description, author, year, original_language, cover_phash \
         FROM work WHERE id IN ({placeholders})"
    );
    let mut q = sqlx::query_as::<_, Row>(&work_sql);
    for id in &ids {
        q = q.bind(*id);
    }
    for row in q.fetch_all(pool).await? {
        out.insert(
            row.id.clone(),
            WorkMatchData {
                work_id: row.id,
                primary_title: row.primary_title,
                description: row.description,
                author: row.author,
                year: row.year,
                original_language: row.original_language,
                cover_phash: row.cover_phash,
                aliases_norm: Vec::new(),
            },
        );
    }

    let alias_sql = format!(
        "SELECT work_id, normalized_title FROM work_alias WHERE work_id IN ({placeholders})"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&alias_sql);
    for id in &ids {
        q = q.bind(*id);
    }
    for (work_id, normalized_title) in q.fetch_all(pool).await? {
        // Only attach aliases to works that had a `work` row (mirror per-item semantics).
        if let Some(md) = out.get_mut(&work_id) {
            md.aliases_norm.push(normalized_title);
        }
    }
    Ok(out)
}

/// A canonical work resolved for reader browse/read (CATALOGUE.md §6). Carries the
/// MangaDex anchor id + cover fileName so the reader can build cover URLs and reach
/// pages via MangaDex@Home. `mangadex_id` is `None` for a first-class non-MangaDex
/// work (not reader-openable through this path yet).
#[derive(Debug, Clone, Default)]
pub struct CanonicalWork {
    pub work_id: String,
    pub mangadex_id: Option<String>,
    pub primary_title: Option<String>,
    pub description: Option<String>,
    /// Carried for completeness/future surfacing; not in the `Series` shape yet.
    #[allow(dead_code)]
    pub year: Option<i64>,
    pub original_language: Option<String>,
    pub status: Option<String>,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub is_nsfw: bool,
    /// Admin-pinned comic type (`content_type_override`); NULL => derive on read.
    pub content_type_override: Option<String>,
    /// Admin metadata overrides (NULL => use the derived/source value).
    pub title_override: Option<String>,
    pub description_override: Option<String>,
    pub is_nsfw_override: Option<bool>,
    pub cover_file_name: Option<String>,
    /// Version of the DB-cached cover blob (`work_cover_blob`), or NULL when no
    /// cover is cached — then the reader falls back to the Worker-proxied MangaDex
    /// URL. See `cover::work_cover_url`.
    pub cover_cached_version: Option<i64>,
    pub alt_titles: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// One mirrored chapter selected for the reader (already deduped to one row per
/// number). `external_id` is the MangaDex chapter uuid — the key used to fetch pages
/// via MangaDex@Home.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CanonicalChapter {
    pub external_id: String,
    pub number: Option<String>,
    /// Selected by the mirror query and carried for completeness; not surfaced yet.
    #[allow(dead_code)]
    pub volume: Option<String>,
    pub lang: Option<String>,
    pub title: Option<String>,
    pub published_at: Option<String>,
}

/// The effective genre/tag list for a canonical work, in strict precedence order:
///   1. the admin-curated `work_tag` half (`source = 'admin'`),
///   2. the upstream MangaDex half (`source = 'mangadex'`, migration 0066),
///   3. the distinct genres of its linked Suwayomi source series (parsed from the
///      cached JSON `genre` arrays).
///
/// Empty when none exists. Best-effort — any query/parse failure just contributes
/// nothing.
///
/// Tier 2 is what makes a catalogue-wide genre filter possible at all: tier 1 is empty
/// in production (curation is opt-in and rare) and tier 3 reaches only the ~13.8k works
/// with a Suwayomi link, so before MangaDex tags were ingested ~101k works had no genre.
/// The tiers do NOT merge: mixing a human's deliberate short list with ~6 upstream tags
/// would make curation unable to REMOVE a tag, which is most of what it is for.
///
/// A page of feed items should use [`work_effective_genres_batch`] instead: this issues
/// up to three round-trips per work.
pub async fn work_effective_genres(pool: &SqlitePool, work_id: &str) -> Vec<String> {
    let curated = sqlx::query_scalar::<_, String>(
        "SELECT tag FROM work_tag WHERE work_id = ? AND source = 'admin' ORDER BY ord, tag",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    if !curated.is_empty() {
        return curated;
    }
    // `ord` is the group ranking `mangadex::tag_names` assigned (genre, theme, format,
    // content), so this preserves "the axes a reader browses by, first".
    let upstream = sqlx::query_scalar::<_, String>(
        "SELECT tag FROM work_tag WHERE work_id = ? AND source = 'mangadex' ORDER BY ord, tag",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    if !upstream.is_empty() {
        return upstream;
    }
    // The cast is on `ss.source_key` (a TEXT column with no usable index for this
    // join), NOT on `sw.id`. Casting the INDEXED side — `CAST(sw.id AS TEXT)` — makes
    // the expression opaque to the planner, which then has no choice but `SCAN sw`
    // across all 13,802 rows of `suwayomi_series` for EVERY work looked up. Measured
    // on production: 14.98 ms -> 0.01 ms (EXPLAIN QUERY PLAN goes from `SCAN sw` to
    // `SEARCH sw USING INTEGER PRIMARY KEY (rowid=?)`). Same shape, ~1,500x.
    let jsons = sqlx::query_scalar::<_, String>(
        "SELECT sw.genre FROM source_series ss \
         JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER) \
         WHERE ss.work_id = ? AND ss.source_type = 'suwayomi' AND sw.genre IS NOT NULL",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for j in jsons {
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&j) {
            for g in list {
                let g = g.trim().to_string();
                if !g.is_empty() && seen.insert(g.clone()) {
                    out.push(g);
                }
            }
        }
    }
    out
}

/// Batched [`work_effective_genres`]: three grouped queries for a whole page instead of
/// three per work. Byte-identical per-work output (same tier precedence, same first-seen
/// ordering); works with no genre on any tier are simply absent from the map and the
/// caller defaults them to empty.
///
/// `map_series_batch` calls this once per feed page. Per-item it was ~15 ms × 25 items
/// = ~375 ms of pure genre lookup on a browse page — the single largest cost there,
/// despite a comment claiming the branch was "rare" (it is not: 13,789 of 13,802
/// Suwayomi series are catalogued, and the CURATED half of `work_tag` is empty in
/// production, so tier 1 never fires). Since migration 0066 most works are answered by
/// tier 2, an indexed (work_id, source) seek, and never reach the Suwayomi join at all.
pub async fn work_effective_genres_batch(
    pool: &SqlitePool,
    work_ids: &[String],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
    if work_ids.is_empty() {
        return out;
    }
    let ph = std::iter::repeat_n("?", work_ids.len())
        .collect::<Vec<_>>()
        .join(",");

    // 1) Admin-curated tags win outright wherever they exist.
    let curated_sql = format!(
        "SELECT work_id, tag FROM work_tag WHERE work_id IN ({ph}) AND source = 'admin' \
         ORDER BY work_id, ord, tag"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&curated_sql);
    for id in work_ids {
        q = q.bind(id);
    }
    for (wid, tag) in q.fetch_all(pool).await.unwrap_or_default() {
        out.entry(wid).or_default().push(tag);
    }

    // 2) Upstream MangaDex tags for everything curation didn't cover (migration 0066).
    //    Bound to the works still missing so a fully-curated page costs nothing here.
    let uncurated: Vec<&String> = work_ids.iter().filter(|w| !out.contains_key(*w)).collect();
    if !uncurated.is_empty() {
        let ph_up = std::iter::repeat_n("?", uncurated.len())
            .collect::<Vec<_>>()
            .join(",");
        let up_sql = format!(
            "SELECT work_id, tag FROM work_tag WHERE work_id IN ({ph_up}) AND source = 'mangadex' \
             ORDER BY work_id, ord, tag"
        );
        let mut qu = sqlx::query_as::<_, (String, String)>(&up_sql);
        for id in &uncurated {
            qu = qu.bind(*id);
        }
        for (wid, tag) in qu.fetch_all(pool).await.unwrap_or_default() {
            out.entry(wid).or_default().push(tag);
        }
    }

    // 3) Source genres for everything neither tag half covered. Same
    //    cast-the-non-indexed-side join as the single-work path above.
    let remaining: Vec<&String> = work_ids.iter().filter(|w| !out.contains_key(*w)).collect();
    if remaining.is_empty() {
        return out;
    }
    let ph2 = std::iter::repeat_n("?", remaining.len())
        .collect::<Vec<_>>()
        .join(",");
    let src_sql = format!(
        "SELECT ss.work_id, sw.genre FROM source_series ss \
         JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER) \
         WHERE ss.work_id IN ({ph2}) AND ss.source_type = 'suwayomi' AND sw.genre IS NOT NULL \
         ORDER BY ss.work_id, ss.id"
    );
    let mut q2 = sqlx::query_as::<_, (String, String)>(&src_sql);
    for id in &remaining {
        q2 = q2.bind(*id);
    }
    let mut seen: std::collections::HashMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    for (wid, json) in q2.fetch_all(pool).await.unwrap_or_default() {
        let Ok(list) = serde_json::from_str::<Vec<String>>(&json) else {
            continue;
        };
        for g in list {
            let g = g.trim().to_string();
            if g.is_empty() {
                continue;
            }
            if seen.entry(wid.clone()).or_default().insert(g.clone()) {
                out.entry(wid.clone()).or_default().push(g);
            }
        }
    }
    out
}

/// Load a canonical work with its MangaDex anchor + cover fileName + alt titles, for
/// the reader's canonical series path. `None` if the work id is unknown.
pub async fn load_canonical_work(
    pool: &SqlitePool,
    work_id: &str,
) -> Result<Option<CanonicalWork>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        primary_title: Option<String>,
        description: Option<String>,
        year: Option<i64>,
        original_language: Option<String>,
        status: Option<String>,
        author: Option<String>,
        artist: Option<String>,
        is_nsfw: i64,
        content_type_override: Option<String>,
        title_override: Option<String>,
        description_override: Option<String>,
        is_nsfw_override: Option<i64>,
        cover_file_name: Option<String>,
        cover_cached_version: Option<i64>,
        created_at: String,
        updated_at: String,
    }
    let row = sqlx::query_as::<_, Row>(
        "SELECT primary_title, description, year, original_language, status, author, artist, \
                is_nsfw, content_type_override, title_override, description_override, \
                is_nsfw_override, cover_file_name, cover_cached_version, created_at, updated_at \
         FROM work WHERE id = ?",
    )
    .bind(work_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };

    // The MangaDex source_series (its source_key is the MangaDex manga uuid).
    // A merge can fold two mangadex works together, leaving >1 mangadex source_series
    // on the target. Order deterministically (oldest first, id as final tiebreak) so
    // the cover/page anchor is stable across loads instead of whatever the planner
    // happens to return first.
    let mangadex_id = sqlx::query_scalar::<_, String>(
        "SELECT source_key FROM source_series \
         WHERE work_id = ? AND source_type = 'mangadex' \
         ORDER BY created_at ASC, id ASC LIMIT 1",
    )
    .bind(work_id)
    .fetch_optional(pool)
    .await?;

    let alt_titles = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT raw_title FROM work_alias WHERE work_id = ? ORDER BY raw_title",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;

    Ok(Some(CanonicalWork {
        work_id: work_id.to_string(),
        mangadex_id,
        primary_title: row.primary_title,
        description: row.description,
        year: row.year,
        original_language: row.original_language,
        status: row.status,
        author: row.author,
        artist: row.artist,
        is_nsfw: row.is_nsfw != 0,
        content_type_override: row.content_type_override,
        title_override: row.title_override,
        description_override: row.description_override,
        is_nsfw_override: row.is_nsfw_override.map(|v| v != 0),
        cover_file_name: row.cover_file_name,
        cover_cached_version: row.cover_cached_version,
        alt_titles,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// Load the reader-facing chapter list for a canonical work: the mirrored **English**
/// chapters of its MangaDex source_series, deduped to one row per chapter number and
/// ordered ascending by number. Komika serves only English chapters, so non-English
/// rows are excluded here (belt-and-suspenders alongside the English-only sync); a
/// number with several English scanlations is collapsed by `select_reader_chapters`.
pub async fn load_canonical_chapters(
    pool: &SqlitePool,
    work_id: &str,
) -> Result<Vec<CanonicalChapter>> {
    let rows = sqlx::query_as::<_, CanonicalChapter>(
        "SELECT c.external_id, c.number, c.volume, c.lang, c.title, c.published_at \
         FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
         WHERE ss.work_id = ? AND ss.source_type = 'mangadex' AND c.lang = 'en' \
         ORDER BY c.published_at DESC, c.external_id ASC",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    Ok(select_reader_chapters(rows))
}

/// The publish time of the work's most recent **English** MangaDex chapter — the
/// real "last updated" for a series' details page. `None` when the work has no English
/// chapters mirrored yet (caller falls back to the work-metadata timestamp).
///
/// This is deliberately NOT `work.updated_at`: that column is bumped on every routine
/// metadata re-sync (`Utc::now()` in `upsert_work`), so it reflects "last metadata
/// touch", not "last new chapter" — a work synced 4h ago but last updated 32 days ago
/// would wrongly read "4 hours ago". `published_at` falls back to `created_at` per row
/// only when a chapter has no publish date (mirrors `canonical_updates`).
pub async fn latest_english_chapter_at(pool: &SqlitePool, work_id: &str) -> Result<Option<String>> {
    let at = sqlx::query_scalar::<_, Option<String>>(
        "SELECT MAX(COALESCE(c.published_at, c.created_at)) \
         FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
         WHERE ss.work_id = ? AND ss.source_type = 'mangadex' AND c.lang = 'en'",
    )
    .bind(work_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(at)
}

/// Rebuild the materialized `feed_updates` table (migration 0051).
///
/// This is the exact grouping `canonical_updates` used to run per-request — one row
/// per work, its newest RELEASED English chapter — but executed ONCE here, in the
/// background, instead of on every page view. The resolver then reads ~20 indexed rows
/// instead of grouping 800k chapter rows through two temp B-trees (3.4s → sub-ms).
///
/// Rebuilt wholesale under one transaction: the table is small (one row per work with
/// a released chapter) and a full REPLACE is simpler and race-free versus incremental
/// upserts. `now` bounds out MangaDex's far-future scheduled `publishAt` values, so
/// unreleased chapters never enter the feed.
///
/// `is_nsfw` materializes the EFFECTIVE flag `COALESCE(is_nsfw_override, is_nsfw)`, not
/// the raw column: both admin "mark NSFW"/"mark SFW" mutations write only the override,
/// so copying `w.is_nsfw` made the feed the one surface that ignored every manual
/// reclassification until the next full sync.
///
/// ALSO refreshes [`refresh_feed_series_updates`] (the reader's merged Updates feed,
/// migration 0064), which is derived from this table plus the scanner's half — and that in
/// turn refreshes [`refresh_browse_catalogue`] (Browse's table, migration 0069), which is
/// derived from IT. Folded into one chain rather than wired separately so every existing
/// call site — boot and each post-sync pass — covers all three, and they can never drift
/// apart in freshness. The return value stays the `feed_updates` row count.
pub async fn refresh_feed_updates(pool: &SqlitePool) -> Result<u64> {
    let now = Utc::now().to_rfc3339();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM feed_updates")
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query(
        "INSERT INTO feed_updates \
             (work_id, mangadex_id, title, is_nsfw, cover_url, \
              latest_chapter, latest_chapter_title, latest_at) \
         SELECT ss.work_id, ss.source_key, w.primary_title, \
                COALESCE(w.is_nsfw_override, w.is_nsfw), \
                CASE WHEN w.cover_cached_version IS NOT NULL \
                     THEN '/covers/' || w.id || '.webp?v=' || w.cover_cached_version \
                     WHEN w.cover_file_name IS NOT NULL \
                     THEN '/covers/' || w.id || '.webp' \
                     ELSE NULL END, \
                c.number, c.title, \
                MAX(COALESCE(c.published_at, c.created_at)) \
         FROM chapter c \
         JOIN source_series ss ON ss.id = c.source_series_id \
         JOIN work w ON w.id = ss.work_id \
         WHERE ss.source_type = 'mangadex' AND c.lang = 'en' \
           AND COALESCE(c.published_at, c.created_at) <= ? \
         GROUP BY ss.work_id",
    )
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    // The reader's merged Updates feed is derived FROM the table we just rebuilt (plus
    // the scanner's half), so it is refreshed here rather than wired separately: every
    // existing caller of this function — boot (`main`) and each post-sync pass
    // (`mangadex`) — is exactly when both halves change. Best-effort: a failure here
    // must not make the canonical feed's refresh look like it failed.
    if let Err(e) = refresh_feed_series_updates(pool).await {
        tracing::warn!(error = %e, "feed_series_updates: refresh failed");
    }
    Ok(n)
}

/// Rebuild the materialized merged Updates feed, `feed_series_updates` (migration 0064).
///
/// This is the union of the reader's TWO update feeds, taken once and keyed by canonical
/// work: the MangaDex mirror half (already grouped for us into `feed_updates`) and the
/// scanner half (Suwayomi library series with a detected new chapter). The reader used to
/// fetch page 1 of each, merge, dedupe by lowercased title and cap at 60 — see the
/// migration for why that cannot be paginated.
///
/// Wholesale DELETE + INSERT under one IMMEDIATE transaction, exactly like
/// [`refresh_feed_updates`] and [`refresh_work_fts`]: the table is ~48k rows of small
/// scalars, and a full rebuild is simpler and race-free versus maintaining upserts
/// across every chapter-write and scan-write path.
///
/// ATOMICITY INCLUDES `comic_type`. The three INSERT/UPDATE phases below — mirror half,
/// scanner half, and the Rust-side [`fill_feed_series_updates_types`] pass — all run in
/// THIS ONE transaction, so no reader ever observes the new generation half-typed. It was
/// briefly split: the rebuild committed and the type fill ran after it, which left every
/// row with `comic_type IS NULL` for the duration of the fill — and `updatesFeed(type:)`
/// filters on a single equality, so the reader's format tabs returned an EMPTY feed
/// (`total: 0` included) on every rebuild. A partial visible state is worse than a slower
/// invisible one for a table whose whole purpose is being read consistently.
///
/// The cost is a longer write lock: the fill's three reads measure ~0.6 s warm on a copy
/// of production (48,409 rows), on top of the ~3 s the DELETE + two INSERTs already hold.
/// `resolve_comic_type` itself is a handful of `str::contains` and Unicode-script scans
/// per row and does not register. That is within the same envelope as the existing
/// writers this DB already schedules around — `refresh_feed_updates` is documented as a
/// ~3 s transaction and the periodic `ANALYZE` as a ~3.5 s one — and well inside the
/// pool's 15 s `busy_timeout`, so the scanner rides through it as a wait, not a failure.
/// This runs at boot and once per catalogue-sync cycle, not per request.
///
/// The fill being inside the transaction also means a failure there ROLLS BACK the whole
/// rebuild, leaving the previous generation intact, instead of committing an untyped one.
///
/// STALENESS. Both halves are as fresh as this call, i.e. boot + each catalogue sync.
/// That is the right cadence for the mirror half (canonical chapters only change then)
/// but NOT for the scanner half, which changes continuously: a series the scanner
/// detects a chapter for appears in `/updates` only after the next refresh. Closing that
/// gap means a one-row UPSERT in `scanner::persist_scan`, which already writes both
/// source columns — see the report/PR notes; it is deliberately not done here.
pub async fn refresh_feed_series_updates(pool: &SqlitePool) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM feed_series_updates")
        .execute(&mut *tx)
        .await?;

    // (1) The MangaDex mirror half. `feed_updates` has already done the per-work
    //     grouping, the far-future-`publishAt` bound and the effective-NSFW COALESCE, so
    //     this is a straight copy — except for two things:
    //
    //     * `released_at` is converted from ISO-8601 TEXT to EPOCH MILLISECONDS, because
    //       the scanner half's clock is 13-digit millis TEXT and the two encodings do not
    //       compare (see the migration). `strftime` returns NULL on anything it can't
    //       parse, and the guard drops those rows rather than inserting a NULL into a
    //       NOT NULL column and failing the whole refresh.
    //     * `title_override` is honoured. `refresh_feed_updates` copies bare
    //       `primary_title`, so an admin-retitled work reads under its old name on the
    //       canonical feed; this feed does not inherit that.
    let mirror = sqlx::query(&format!(
        "INSERT INTO feed_series_updates \
             (work_id, reader_id, title, cover_url, suwayomi_thumbnail, comic_type, \
              latest_chapter, latest_chapter_title, chapter_count, released_at, \
              detected_at, is_nsfw, status, content_rating) \
         SELECT fu.work_id, fu.work_id, COALESCE(w.title_override, fu.title), \
                fu.cover_url, NULL, NULL, \
                fu.latest_chapter, fu.latest_chapter_title, NULL, \
                CAST(strftime('%s', fu.latest_at) AS INTEGER) * 1000, \
                NULL, fu.is_nsfw, \
                {FSU_STATUS_SQL}, COALESCE(w.content_rating, 'safe') \
         FROM feed_updates fu \
         JOIN work w ON w.id = fu.work_id \
         WHERE strftime('%s', fu.latest_at) IS NOT NULL"
    ))
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // (2) The scanner half, folded onto the SAME work row. A Suwayomi series reaches a
    //     work through `source_series`, and a work can have several Suwayomi sources
    //     (production: 1,316 detected series collapse to 1,218 works) — the GROUP BY is
    //     the dedupe, and it is by identity rather than by title.
    //
    //     The bare `sy.*` / `w.*` / `sss.*` columns alongside the single `MAX()` are
    //     SQLite's documented "bare columns in an aggregate query" rule: with exactly one
    //     min/max aggregate, every bare column is taken from the row that produced it.
    //     So this row is the work's NEWEST-RELEASING Suwayomi source, and its id, title,
    //     cover and detection time all come from that same source — not smeared across
    //     sources by independent MAX()es.
    //
    //     `in_library = 1` matches `graphql::updates`' membership test (the feed is the
    //     reader's library), and both `IS NOT NULL` guards keep the NOT NULL sort key
    //     honest: an undated series is not an update. Neither excludes anything today —
    //     all 1,316 detected in-library series have a release time and a work mapping.
    let scanner = sqlx::query(
        &format!("INSERT INTO feed_series_updates \
             (work_id, reader_id, title, cover_url, suwayomi_thumbnail, comic_type, \
              latest_chapter, latest_chapter_title, chapter_count, released_at, \
              detected_at, is_nsfw, status, content_rating) \
         SELECT ss.work_id, \
                CASE WHEN EXISTS (SELECT 1 FROM source_series md \
                                   WHERE md.work_id = ss.work_id \
                                     AND md.source_type = 'mangadex') \
                     THEN ss.work_id ELSE CAST(sy.id AS TEXT) END, \
                COALESCE(w.title_override, w.primary_title, sy.title), \
                CASE WHEN w.cover_cached_version IS NOT NULL \
                     THEN '/covers/' || w.id || '.webp?v=' || w.cover_cached_version \
                     END, \
                sy.thumbnail_url, NULL, \
                NULL, NULL, sy.chapter_count, \
                MAX(CAST(sy.latest_chapter_at AS INTEGER)), \
                sss.last_new_chapter_at, \
                COALESCE(w.is_nsfw_override, w.is_nsfw), \
                {FSU_STATUS_SQL}, COALESCE(w.content_rating, 'safe') \
         FROM source_series ss \
         JOIN work w ON w.id = ss.work_id \
         JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
         JOIN series_scan_state sss ON sss.series_id = ss.source_key \
         WHERE ss.source_type = 'suwayomi' AND sy.in_library = 1 \
           AND sy.latest_chapter_at IS NOT NULL \
           AND sss.last_new_chapter_at IS NOT NULL \
         GROUP BY ss.work_id \
         ON CONFLICT(work_id) DO UPDATE SET \
             released_at = MAX(feed_series_updates.released_at, excluded.released_at), \
             detected_at = COALESCE(excluded.detected_at, feed_series_updates.detected_at), \
             chapter_count = COALESCE(feed_series_updates.chapter_count, excluded.chapter_count), \
             cover_url = COALESCE(NULLIF(feed_series_updates.cover_url, ''), excluded.cover_url), \
             suwayomi_thumbnail = COALESCE(feed_series_updates.suwayomi_thumbnail, excluded.suwayomi_thumbnail), \
             is_nsfw = MAX(feed_series_updates.is_nsfw, excluded.is_nsfw), \
             status = excluded.status, \
             content_rating = excluded.content_rating",
    ))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // The conflict clause max-merges the shared CLOCK and takes the scanner's
    // `detected_at`, but leaves every DISPLAY field — `reader_id`, `title`, `cover_url`,
    // `latest_chapter` — on the mirror half. Two reasons: `reader_id` must stay the
    // canonical `w_` id (it is the work's stable identity and the richer destination,
    // and the numeric id would send a mangadex-anchored work down the Suwayomi path),
    // and once the card navigates to the canonical page its label should describe that
    // page. Consequence to know: for a merged work whose Suwayomi source is ahead of the
    // mirror, the card's release time can be newer than the newest chapter the canonical
    // page lists, until the next MangaDex sync mirrors it.
    //
    // The scanner half's INSERT above now derives that same canonical `w_` id itself
    // (via the mangadex-anchor test), so a mangadex-anchored work whose MIRROR half never
    // fired — a licensing takedown leaves the spine with no dated chapter, so only the
    // Suwayomi half publishes a row — still navigates to its canonical page instead of the
    // Suwayomi one. Leaving `reader_id` off the conflict clause therefore no longer depends
    // on the mirror having won the insert race; both halves independently agree on the id.
    //
    // `status` / `content_rating` (migration 0068) are the exception to that mirror-wins
    // rule, and it costs nothing: both halves derive them from the SAME `work` row via the
    // same expression, so `excluded` and the existing value are always equal here. They are
    // assigned rather than omitted so the clause states the intent — these are properties
    // of the WORK, not of whichever half published the row — which is what keeps this
    // statement convergent with `scanner::upsert_feed_series_update`, whose pre-existing row
    // may be its own earlier write rather than a mirror row.

    // (3) `comic_type`, which cannot be done in SQL: `resolve_comic_type` is a
    //     five-step derivation ending in Unicode script tests on the title. Running the
    //     real function keeps this feed's format facet identical to every other surface's
    //     format badge, instead of a SQL approximation that disagrees with them.
    //
    //     INSIDE the transaction, deliberately — see the atomicity note in the doc
    //     comment. It reads the rows the two INSERTs above just wrote, which are visible
    //     to this connection and to nobody else until the commit below, so the new
    //     generation becomes visible fully typed or not at all.
    let typed = fill_comic_types(&mut tx, "feed_series_updates").await?;

    // (4) `en_chapter_count` (migration 0068) — Browse's CHAPTERS sort key and the number
    //     its cards print. Two statements, in this order, because the fallback must only
    //     ever RAISE a zero: the English mirror count is authoritative where it exists, and
    //     the Suwayomi count is the only thing available for a work whose MangaDex spine has
    //     no English chapter (the same precedence `map_canonical_series` applies between its
    //     English count and `aggregate_chapter_count`).
    //
    //     STILL INSIDE the transaction, for the reason the doc comment gives about
    //     `comic_type`: this is a SORT KEY, and a committed generation where every row reads
    //     0 would make `sort: CHAPTERS` degenerate to the `work_id DESC` tiebreaker — a
    //     silently wrong ordering rather than a visibly empty one, which is worse.
    let counted = sqlx::query(
        "UPDATE feed_series_updates SET en_chapter_count = en.n \
             FROM (SELECT ss.work_id AS work_id, COUNT(DISTINCT c.number) AS n \
                     FROM chapter c \
                     JOIN source_series ss ON ss.id = c.source_series_id \
                    WHERE ss.source_type = 'mangadex' AND c.lang = 'en' \
                    GROUP BY ss.work_id) AS en \
            WHERE en.work_id = feed_series_updates.work_id",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE feed_series_updates SET en_chapter_count = sw.n \
             FROM (SELECT ss.work_id AS work_id, MAX(sy.chapter_count) AS n \
                     FROM source_series ss \
                     JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
                    WHERE ss.source_type = 'suwayomi' \
                    GROUP BY ss.work_id) AS sw \
            WHERE sw.work_id = feed_series_updates.work_id \
              AND feed_series_updates.en_chapter_count = 0 \
              AND sw.n > 0",
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Browse's own table (migration 0069), which is DERIVED FROM the generation we just
    // committed — for a work with a feed row it copies that row's columns verbatim, so it has
    // to run after this commit, not before. Best-effort for the same reason
    // `refresh_feed_updates` treats this function as best-effort: a Browse rebuild failing
    // must not make the updates feed's rebuild look like it failed.
    //
    // A SEPARATE TRANSACTION, and that is a measured decision rather than a stylistic one.
    // 0064 folded the type fill into the rebuild's transaction so no reader could see a
    // half-typed generation, and the same argument holds WITHIN `refresh_browse_catalogue`
    // (which owns its own transaction for exactly that). What does not hold is sharing THIS
    // one: no query reads both tables (Browse reads only `browse_catalogue`, `updatesFeed`
    // only `feed_series_updates`), so cross-table atomicity buys nothing — and it costs
    // everything. Measured on a copy of production: the transaction above is already
    // ~12.0 s of SQL (`DELETE` 4.7 s + mirror INSERT 3.1 s + `en_chapter_count` 3.3 s + the
    // rest) plus ~0.6 s of type fill and a ~0.5 s commit, and Browse's rebuild adds ~6 s.
    // One shared transaction would therefore hold the write lock for ~19 s against the
    // pool's 15 s `busy_timeout` (db.rs) — past that ceiling the scanner's concurrent writes
    // stop being a wait and start being `SQLITE_BUSY` failures. Two transactions of ~13 s and
    // ~6 s each stay inside it.
    if let Err(e) = refresh_browse_catalogue(pool).await {
        tracing::warn!(error = %e, "browse_catalogue: refresh failed");
    }

    // Give the planner statistics for a table that did not exist when the periodic
    // `ANALYZE` list in `db.rs` was written. Without a `sqlite_stat1` row the planner
    // assumes ~1M rows for it (see the note on that list). Run AFTER the rebuild so the
    // stats describe a populated table, and outside the transaction so it never extends
    // the write lock. `feed_genre_facet` is ~100 rows and always read in full, so it needs
    // no statistics of its own.
    if let Err(e) = sqlx::query("ANALYZE feed_series_updates")
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "feed_series_updates: ANALYZE failed");
    }
    tracing::info!(
        mirror,
        scanner,
        typed,
        counted,
        "feed_series_updates: rebuilt merged updates feed"
    );
    Ok(mirror + scanner)
}

/// The SELECT behind `browse_catalogue` (migration 0069): one row per BROWSABLE work,
/// including the 67,000 that have no dated chapter and are therefore absent from
/// `feed_series_updates`.
///
/// A function rather than a `const` only because it interpolates [`FSU_STATUS_SQL`] — the
/// status normalization has to be the SAME expression the feed uses or one upstream word
/// would mean two different things on two Komika surfaces. Migration 0069's backfill mirrors
/// this text BY HAND, the same arrangement 0068's backfill has with the statements above.
/// Column order matches [`BROWSE_CATALOGUE_COLUMNS`].
///
/// EVERY column a work's `feed_series_updates` row already carries is COPIED VERBATIM from
/// it, NULLs included, rather than re-derived: the derivations there are already reviewed
/// (effective-NSFW, the epoch-millis clock, the mirror-wins merge of the two halves), and a
/// Browse card must not disagree with the same work's Updates card. Verified on a copy of
/// production — 0 of the 48,567 shared rows differ on any copied column. `comic_type` is the
/// one exception and is deliberately absent: [`fill_comic_types`] owns it (see below).
///
/// The two exclusions are the WHERE: a work with no `source_series` at all has nothing to
/// open on either path (2 works in production), and a work with no title would render a card
/// with no label and no href (0 works; the guard is also what makes `title NOT NULL` safe).
fn browse_catalogue_select() -> String {
    format!(
        "SELECT w.id, \
            -- The ANCHOR decides first: a MangaDex-anchored work navigates to its canonical
            -- `w_` page regardless of which feed half published its row, so a takedown work
            -- (mirror empty, feed row from the Suwayomi half) no longer inherits a numeric id
            -- that would send it to the Suwayomi page. The feed's id is only copied for a
            -- non-anchored work; the numeric Suwayomi id is the last resort.
            CASE WHEN md.work_id IS NOT NULL THEN w.id \
                 WHEN f.work_id IS NOT NULL THEN f.reader_id \
                 ELSE sw.source_key END, \
            CASE WHEN f.work_id IS NOT NULL THEN f.title \
                 ELSE COALESCE(w.title_override, w.primary_title, sw.title) END, \
            CASE WHEN f.work_id IS NOT NULL THEN f.cover_url \
                 WHEN w.cover_cached_version IS NOT NULL \
                      THEN '/covers/' || w.id || '.webp?v=' || w.cover_cached_version \
                 WHEN w.cover_file_name IS NOT NULL THEN '/covers/' || w.id || '.webp' END, \
            CASE WHEN f.work_id IS NOT NULL THEN f.suwayomi_thumbnail \
                 ELSE sw.thumbnail_url END, \
            CASE WHEN f.work_id IS NOT NULL THEN f.status ELSE {FSU_STATUS_SQL} END, \
            CASE WHEN f.work_id IS NOT NULL THEN f.content_rating \
                 ELSE COALESCE(w.content_rating, 'safe') END, \
            CASE WHEN f.work_id IS NOT NULL THEN f.is_nsfw \
                 ELSE COALESCE(w.is_nsfw_override, w.is_nsfw) END, \
            COALESCE(NULLIF(en.n, 0), NULLIF(swc.n, 0), 0), \
            f.released_at, \
            w.created_at \
       FROM work w \
       LEFT JOIN feed_series_updates f ON f.work_id = w.id \
       LEFT JOIN (SELECT work_id FROM source_series \
                   WHERE source_type = 'mangadex' GROUP BY work_id) md ON md.work_id = w.id \
       LEFT JOIN (SELECT ss.work_id AS work_id, ss.source_key AS source_key, \
                         sy.title AS title, sy.thumbnail_url AS thumbnail_url, MIN(ss.id) \
                    FROM source_series ss \
                    LEFT JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
                   WHERE ss.source_type = 'suwayomi' \
                   GROUP BY ss.work_id) sw ON sw.work_id = w.id \
       LEFT JOIN (SELECT ss.work_id AS work_id, MAX(sy.chapter_count) AS n \
                    FROM source_series ss \
                    JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
                   WHERE ss.source_type = 'suwayomi' \
                   GROUP BY ss.work_id) swc ON swc.work_id = w.id \
       LEFT JOIN (SELECT ss.work_id AS work_id, COUNT(DISTINCT c.number) AS n \
                    FROM chapter c \
                    JOIN source_series ss ON ss.id = c.source_series_id \
                   WHERE ss.source_type = 'mangadex' AND c.lang = 'en' \
                   GROUP BY ss.work_id) en ON en.work_id = w.id \
      WHERE (md.work_id IS NOT NULL OR sw.work_id IS NOT NULL) \
        AND COALESCE(f.title, w.title_override, w.primary_title, sw.title, '') <> ''"
    )
}

/// The `browse_catalogue` columns [`browse_catalogue_select`] produces, in its order.
/// `comic_type` is absent on purpose — see [`fill_comic_types`].
const BROWSE_CATALOGUE_COLUMNS: &str = "work_id, reader_id, title, cover_url, \
     suwayomi_thumbnail, status, content_rating, is_nsfw, en_chapter_count, released_at, \
     created_at";

/// The columns the UPSERT re-assigns on conflict, i.e. everything except the primary key and
/// `comic_type`.
const BROWSE_CATALOGUE_MUTABLE: &[&str] = &[
    "reader_id",
    "title",
    "cover_url",
    "suwayomi_thumbnail",
    "status",
    "content_rating",
    "is_nsfw",
    "en_chapter_count",
    "released_at",
    "created_at",
];

/// Rebuild `browse_catalogue` (migration 0069) — the table Browse pages.
///
/// Called from [`refresh_feed_series_updates`] right after its commit, because every column
/// this table shares with `feed_series_updates` is copied from it. So the cadence is the
/// existing one: boot (`main`) and once per catalogue-sync cycle (`mangadex`).
///
/// UPSERT + PRUNE, NOT `DELETE` + `INSERT`, which is where this diverges from every other
/// feed rebuild in this file — and the reason is measured. `DELETE FROM` + `INSERT` of
/// 115,567 rows across eight indices costs 14.0 s on a copy of production (the `INSERT` alone
/// is 4.5 s into an unindexed table; the other 9.5 s is index maintenance on random
/// `work_id`s). The upsert's `WHERE <any column differs>` makes a no-op cycle 4.9 s with
/// ZERO index writes, and a cycle that really changed 269 rows the same 5.0 s — the SELECT is
/// the floor and the writes are proportional to the change. It also never leaves the table
/// empty, which `DELETE` + `INSERT` does for the length of the transaction.
///
/// `IS NOT`, not `<>`, in that guard: three of the columns are nullable and `NULL <> NULL` is
/// NULL, so `<>` would treat "unchanged NULL" as "changed" and rewrite every such row's index
/// entries on every cycle.
///
/// THE PRUNE is what the missing `DELETE` costs us. Two disqualifications exist and they are
/// handled differently:
///
/// * The work row is GONE (a dedup merge deletes the loser). `work_id`'s foreign key is
///   `ON DELETE CASCADE` and `db.rs` enables `foreign_keys`, so that row is already gone —
///   nothing to do, and it is why 0 merge-retired works can linger here.
/// * The work LOST its last `source_series`. Nothing cascades, so it is deleted explicitly
///   (204 ms, an index seek per row). The full re-evaluation of the SELECT's WHERE as a
///   `NOT IN` measured 1,628 ms for the same 0 rows, and the extra 1.4 s only buys the
///   title-went-empty case — where the upsert already does the safe thing by leaving the row
///   at its last known title rather than writing a NULL into a `NOT NULL` column.
pub async fn refresh_browse_catalogue(pool: &SqlitePool) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let set = BROWSE_CATALOGUE_MUTABLE
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let changed = BROWSE_CATALOGUE_MUTABLE
        .iter()
        .map(|c| format!("browse_catalogue.{c} IS NOT excluded.{c}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let select = browse_catalogue_select();
    let upserted = sqlx::query(&format!(
        "INSERT INTO browse_catalogue ({BROWSE_CATALOGUE_COLUMNS}) {select} \
         ON CONFLICT(work_id) DO UPDATE SET {set} WHERE {changed}"
    ))
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let pruned = sqlx::query(
        "DELETE FROM browse_catalogue WHERE NOT EXISTS \
             (SELECT 1 FROM source_series ss WHERE ss.work_id = browse_catalogue.work_id)",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // `comic_type`, INSIDE this transaction for the reason 0064 gives about the feed's own
    // type fill: a committed generation whose rows have `comic_type IS NULL` is invisible to
    // Browse's format tabs (the filter is a single equality), and a NULL type is exactly what
    // the upsert leaves on a row it has just inserted. Running it here means the new
    // generation becomes visible fully typed or not at all, and a failure rolls back the
    // whole rebuild rather than publishing an untyped one.
    let typed = fill_comic_types(&mut tx, "browse_catalogue").await?;

    // The genre facet list Browse renders as chips (migration 0068), MOVED here from the feed
    // rebuild, because the chips must count the table the chip's filter actually queries.
    // That equality is the whole reason the facets stopped coming from `suwayomi_series`'
    // JSON blobs: a chip labelled "Action · 4,102" that returns a different number of results
    // is worse than no chip. Now that clicking a genre filters `browse_catalogue` (115,567
    // works), counting `feed_series_updates` (48,567) would re-introduce that gap at 2.4x.
    //
    // DELETE + INSERT in the same transaction as the table it counts, so a chip's number can
    // never describe a generation that no longer exists.
    //
    // NO `source` PREDICATE, matching `browse::build_where`'s genre clause exactly: the filter
    // matches ANY tag a work carries on any tier, so the count must too. (Tier PRECEDENCE is
    // `work_effective_genres`' rule for choosing what to DISPLAY, a different question.) The
    // two would drift the moment one grew the predicate and the other did not.
    sqlx::query("DELETE FROM feed_genre_facet")
        .execute(&mut *tx)
        .await?;
    let facets = sqlx::query(
        "INSERT INTO feed_genre_facet(tag, safe_count, all_count) \
         SELECT t.tag, SUM(CASE WHEN b.is_nsfw = 0 THEN 1 ELSE 0 END), COUNT(*) \
           FROM work_tag t JOIN browse_catalogue b ON b.work_id = t.work_id \
          GROUP BY t.tag",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    tx.commit().await?;

    // Browse's `total` is memoized for up to `browse::COUNT_TTL`, and we have just rewritten
    // the table those totals counted. A surviving entry would promise a page that no longer
    // exists — `hasNextPage: true` on the last page, or a pager rendering
    // "showing 115,531-115,560 of 115,530". One `HashMap::clear`, twice a day.
    crate::browse::clear_count_cache();

    // Statistics for a table `db.rs`' periodic `ANALYZE` list predates. Without a
    // `sqlite_stat1` row the planner assumes ~1M rows for it and can pick the wrong index of
    // the eight. AFTER the rebuild so the stats describe the current generation, and outside
    // the transaction so it never extends the write lock.
    if let Err(e) = sqlx::query("ANALYZE browse_catalogue").execute(pool).await {
        tracing::warn!(error = %e, "browse_catalogue: ANALYZE failed");
    }
    let total = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM browse_catalogue")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    tracing::info!(
        total,
        upserted,
        pruned,
        typed,
        facets,
        "browse_catalogue: rebuilt browse catalogue"
    );
    Ok(total as u64)
}

/// The normalized `feed_series_updates.status` expression, over a `work` row aliased `w`.
///
/// Shared by the rebuild's two INSERTs, by `scanner::upsert_feed_series_update`, and — by
/// hand, the way 0050's backfill mirrors `series_cache::derive_latest_chapter_at` — by
/// migration 0068's backfill. If they disagreed, a row's filterability would depend on which
/// writer created it.
///
/// The fold is `graphql::types::status_from`'s: `ON_HIATUS` -> `HIATUS`,
/// `PUBLISHING_FINISHED`/`LICENSED` -> `COMPLETED`. It exists because `komika_status` — the
/// only parser of this column — returns `None` for the long forms and the `SeriesStatus` enum
/// has no such members, so un-normalized those rows are unreachable by EVERY value a client
/// can send for `status:` (8 feed rows today; 201 across all works, growing as more of the
/// catalogue becomes readable).
pub const FSU_STATUS_SQL: &str = "CASE w.status \
        WHEN 'ON_HIATUS' THEN 'HIATUS' \
        WHEN 'PUBLISHING_FINISHED' THEN 'COMPLETED' \
        WHEN 'LICENSED' THEN 'COMPLETED' \
        ELSE COALESCE(w.status, 'UNKNOWN') END";

/// Materialize a feed table's `comic_type` by running the real
/// [`crate::graphql::types::resolve_comic_type`] over every one of its rows.
///
/// `table` is one of two hard-coded identifiers — `feed_series_updates` (migration 0064) or
/// `browse_catalogue` (0069) — never input. One function for both because the word WRITTEN
/// here is the word the `updatesFeed` AND Browse format filters look for; two copies of this
/// derivation would let a work's format differ between the two surfaces.
///
/// Batched, not per-row: four grouped reads (curated tags, upstream tags, source genres, base
/// fields) and then one `UPDATE … WHERE work_id IN (…)` per (type, chunk) — five distinct type
/// values, so ~100 statements for 48k rows and ~240 for 115k, rather than one per row. The
/// reads measure 568 ms over the 48,567-row feed and 823 ms over the 115,567-row browse table.
///
/// The stored word is COLLAPSED to the reader's three-way vocabulary
/// (`WEBTOON → MANHWA`, `COMIC → MANGA`), matching the reader's `toViewType`, so the
/// resolver's format filter can stay a single indexed equality. See the migration.
///
/// Takes a CONNECTION, not the pool, because it must run inside the caller's rebuild
/// transaction: it reads the rows that transaction has just written (uncommitted, so only this
/// connection can see them), and committing the rebuild without it would publish a generation
/// whose every row has `comic_type IS NULL` — an empty `updatesFeed(type:)` / Browse format
/// tab for the duration of the fill.
async fn fill_comic_types(conn: &mut sqlx::SqliteConnection, table: &'static str) -> Result<u64> {
    use std::collections::HashMap;

    // Curated tags win outright wherever they exist — the same rule
    // `work_effective_genres` applies. (Empty in production today, but an admin can
    // populate it, and this feed must not be the one surface that ignores that.)
    let mut genres: HashMap<String, Vec<String>> = HashMap::new();
    for (wid, tag) in sqlx::query_as::<_, (String, String)>(&format!(
        "SELECT wt.work_id, wt.tag FROM work_tag wt \
         JOIN {table} f ON f.work_id = wt.work_id \
         WHERE wt.source = 'admin' \
         ORDER BY wt.work_id, wt.ord, wt.tag"
    ))
    .fetch_all(&mut *conn)
    .await?
    {
        genres.entry(wid).or_default().push(tag);
    }
    // Tier 2: upstream MangaDex tags (migration 0066). This is what actually makes the
    // `comic_type` derivation below work for MangaDex-only works — `resolve_comic_type`
    // reads genre STRINGS to spot "Manhwa"/"Manhua"/"Long Strip", and before these tags
    // existed the ~101k works with no Suwayomi link reached it with an EMPTY genre slice
    // and fell all the way through to the title-script heuristic.
    let mut upstream_genres: HashMap<String, Vec<String>> = HashMap::new();
    for (wid, tag) in sqlx::query_as::<_, (String, String)>(&format!(
        "SELECT wt.work_id, wt.tag FROM work_tag wt \
         JOIN {table} f ON f.work_id = wt.work_id \
         WHERE wt.source = 'mangadex' \
         ORDER BY wt.work_id, wt.ord, wt.tag"
    ))
    .fetch_all(&mut *conn)
    .await?
    {
        upstream_genres.entry(wid).or_default().push(tag);
    }
    // Source genres, kept separate so the curated-wins rule stays a single lookup below
    // rather than a "did this key come from tags or from genres" question. The CAST is on
    // `ss.source_key` (unindexed TEXT), never on `sw.id` — casting the indexed side makes
    // the join opaque to the planner and forces a full `suwayomi_series` scan per row
    // (`work_effective_genres` measured 14.98 ms → 0.01 ms on exactly that change).
    let mut source_genres: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();
    for (wid, json) in sqlx::query_as::<_, (String, String)>(&format!(
        "SELECT ss.work_id, sw.genre FROM source_series ss \
         JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER) \
         JOIN {table} f ON f.work_id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND sw.genre IS NOT NULL \
         ORDER BY ss.work_id, ss.id"
    ))
    .fetch_all(&mut *conn)
    .await?
    {
        let Ok(list) = serde_json::from_str::<Vec<String>>(&json) else {
            continue;
        };
        for g in list {
            let g = g.trim().to_string();
            if !g.is_empty() && seen.entry(wid.clone()).or_default().insert(g.clone()) {
                source_genres.entry(wid.clone()).or_default().push(g);
            }
        }
    }

    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, String)>(&format!(
        "SELECT f.work_id, w.content_type_override, w.original_language, f.title \
         FROM {table} f JOIN work w ON w.id = f.work_id"
    ))
    .fetch_all(&mut *conn)
    .await?;

    let mut by_type: HashMap<&'static str, Vec<String>> = HashMap::new();
    for (work_id, override_word, original_language, title) in rows {
        let g = genres
            .get(&work_id)
            .or_else(|| upstream_genres.get(&work_id))
            .or_else(|| source_genres.get(&work_id))
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let t = crate::graphql::types::resolve_comic_type(
            override_word.as_deref(),
            original_language.as_deref(),
            g,
            &title,
        );
        // Collapse to the reader's three formats, exactly as its `toViewType` does. One
        // shared function, so the word WRITTEN here and the word the `updatesFeed` / Browse
        // filters look for cannot drift apart.
        let word = crate::graphql::types::collapsed_comic_type_word(t);
        by_type.entry(word).or_default().push(work_id);
    }

    // 500 ids per statement: well inside SQLite's 32,766 bound-parameter limit, and
    // small enough that one statement's prepared-plan cost stays trivial. The caller owns
    // the transaction — these UPDATEs commit with the rest of the rebuild, never on their
    // own.
    //
    // `AND comic_type IS NOT ?` makes an unchanged row a no-op rather than a rewrite, which
    // matters only for `browse_catalogue`: that table is UPSERTED rather than
    // DELETE+INSERTed, so on a steady-state cycle almost every row already holds the word we
    // just computed, and `comic_type` is a key column of four of its eight indices. Writing
    // it anyway would put 115k index rewrites into every cycle — the exact cost the upsert
    // exists to avoid. Measured: a 500-id chunk with 306 real changes takes 8 ms.
    // `IS NOT`, not `<>`, because the column is nullable and `NULL <> 'MANGA'` is NULL, which
    // would make a freshly-inserted (NULL-typed) row fail the guard and never get typed.
    // On `feed_series_updates` the guard is free: the rebuild DELETEs first, so every row is
    // NULL-typed and the counts are unchanged.
    const CHUNK: usize = 500;
    let mut n = 0u64;
    for (word, ids) in &by_type {
        for chunk in ids.chunks(CHUNK) {
            let ph = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "UPDATE {table} SET comic_type = ? \
                 WHERE work_id IN ({ph}) AND comic_type IS NOT ?"
            );
            let mut q = sqlx::query(&sql).bind(*word);
            for id in chunk {
                q = q.bind(id);
            }
            n += q.bind(*word).execute(&mut *conn).await?.rows_affected();
        }
    }
    Ok(n)
}

/// Rebuild the `work_fts` full-text index (migration 0052, AD-5).
///
/// Like `refresh_feed_updates`, this is a wholesale DELETE + INSERT under one
/// IMMEDIATE transaction: the corpus (one row per MangaDex-anchored, titled work)
/// is small text and a full rebuild is simpler and race-free versus maintaining
/// per-upsert triggers across `work` + `work_alias` + `source_series`. Called at
/// boot and after each MangaDex catalogue sync — exactly when titles/aliases change
/// — so search stays fresh within one sync interval (fine for a catalogue that
/// itself only syncs on that cadence).
pub async fn refresh_work_fts(pool: &SqlitePool) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM work_fts")
        .execute(&mut *tx)
        .await?;
    // Mirrors migration 0052's backfill exactly (chapter count grouped once via a
    // derived join, not a per-work correlated subquery).
    let n = sqlx::query(
        "INSERT INTO work_fts (work_id, chapters, title, aliases) \
         SELECT w.id, COALESCE(cc.n, 0), \
                COALESCE(w.title_override, w.primary_title, ''), \
                COALESCE((SELECT group_concat(a.raw_title, ' ') \
                          FROM work_alias a WHERE a.work_id = w.id), '') \
         FROM work w \
         LEFT JOIN (SELECT ss.work_id, COUNT(*) AS n FROM chapter ch \
                    JOIN source_series ss ON ss.id = ch.source_series_id \
                    GROUP BY ss.work_id) cc ON cc.work_id = w.id \
         WHERE COALESCE(w.title_override, w.primary_title, '') <> '' \
           AND EXISTS (SELECT 1 FROM source_series ss \
                       WHERE ss.work_id = w.id AND ss.source_type = 'mangadex')",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// Shortest ASCII query we will run through FTS. `q=a` prefix-matched 40k of the
/// 109k indexed works and `q=th` 30k — measured 0.5-2.5s of unauthenticated server
/// time per keystroke, for a result set no user can use. Non-Latin scripts are
/// exempt (see `fts_match_query`): a 1-2 character CJK query is a real word, and
/// measures at ~30-100 matches.
const MIN_FTS_QUERY_CHARS: usize = 3;

/// How many bm25-best matches the ranking re-sort considers. The exact/prefix title
/// tier costs two correlated `work_alias` lookups PER ROW, so evaluating it over
/// every match (tens of thousands on a short query) and then sorting the lot through
/// a temp B-tree was the search hot path. Ranking a bounded window of the bm25-best
/// candidates instead makes the tail cost flat, and `total` is capped to the same
/// window so pagination never promises a page the window can't serve.
const RANK_WINDOW: i64 = 500;

/// Build an FTS5 MATCH expression from raw user input: split into unicode
/// alphanumeric word tokens, quote each (so an FTS keyword like `and`/`or`/`near`
/// or a stray operator char is treated as a literal, never as syntax), and append
/// `*` to the LAST token for prefix matching so "solo lev" matches "Solo Leveling"
/// as you type. Tokens are implicitly AND-ed. Returns `None` when the query has no
/// usable token, or is too short to run (see `MIN_FTS_QUERY_CHARS`).
///
/// Only the last token is prefix-matched: while typing, the earlier tokens are
/// complete words and starring them only widens the postings list scanned ("one
/// piece" scanned every work containing a word starting with "one").
fn fts_match_query(raw: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            terms.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    let (last, head) = terms.split_last()?;
    // Too-short queries are refused rather than served slowly — EXCEPT when the query
    // carries a non-ASCII character, where a short token is a whole CJK word rather
    // than a one-letter prefix over the entire catalogue.
    let chars: usize = terms.iter().map(|t| t.chars().count()).sum();
    let has_non_ascii = terms.iter().any(|t| !t.is_ascii());
    if chars < MIN_FTS_QUERY_CHARS && !has_non_ascii {
        return None;
    }
    // Quote each token; a `"` can't appear (we kept only alphanumerics), so no
    // escaping is needed. `"tok"*` = prefix match on a quoted string literal.
    let mut out: Vec<String> = head.iter().map(|t| format!("\"{t}\"")).collect();
    out.push(format!("\"{last}\"*"));
    Some(out.join(" "))
}

/// Escape SQLite `LIKE` metacharacters (`\`, `%`, `_`) so a user query is matched
/// literally under `ESCAPE '\'` — a title containing `%` can't turn into a wildcard.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Full-text search over the canonical catalogue (AD-5). Returns `(total, page)`
/// where `page` is the `w_` work ids for the requested page. NSFW works are gated in
/// SQL (before LIMIT) unless `show_nsfw`, so `total` and pagination stay honest. An
/// empty / tokenless / too-short query yields `(0, [])` — the caller browses the
/// catalogue.
///
/// Ranking, in order: (1) an exact/prefix full-title TIER — a work whose title (or an
/// alias) equals the query ranks above one it merely prefixes, above a pure token
/// match; this is what makes "naruto" surface *Naruto* instead of a short doujinshi
/// that bm25's term-frequency bias would float up. (2) `chapters` DESC as a
/// notability proxy (a real series has hundreds of chapters, a spin-off one). (3)
/// bm25 as the final tie-break. The `chapters` proxy is interim — replace it with
/// MangaDex `follows` once `work_stats` is ingested (AD-6) for accurate ranking on
/// short/partial queries (e.g. a single distinctive word).
///
/// That tier is evaluated over a BOUNDED WINDOW of the `RANK_WINDOW` bm25-best matches
/// rather than over every match: its two correlated `work_alias` lookups per row cost
/// ~0.5-2.5s on a short query that matches tens of thousands of works, for rows no
/// page would ever show. `total` is capped to the same window so `has_next` can't
/// promise a page beyond it. The trade-off is that a match ranked worse than
/// `RANK_WINDOW` by bm25 can no longer be lifted into view by the title tier — bm25
/// favours short fields, so an exact title hit sits near the top of its own query's
/// window by construction.
///
/// The tier's case-folding is script-dependent: SQLite's built-in `lower()` folds
/// ASCII ONLY (`lower('ЖУРНАЛ')` = `'ЖУРНАЛ'`), so for a query carrying any non-ASCII
/// character the tier compares against `work_alias.normalized_title` — the key written
/// by Rust's unicode-aware `normalize_title`, which every work's titles are indexed
/// under. Latin queries keep the raw `lower()` comparison: `normalized_title` also
/// strips noise tails ("Naruto (Official Colored)" normalizes to "naruto"), which
/// would let a re-release tie with the work itself in the exact tier.
pub async fn search_works_fts(
    pool: &SqlitePool,
    query: &str,
    show_nsfw: bool,
    page: i64,
    page_size: i64,
) -> Result<(i64, Vec<String>)> {
    let Some(match_expr) = fts_match_query(query) else {
        return Ok((0, Vec::new()));
    };
    let offset = (page.max(1) - 1) * page_size;
    // NSFW gate mirrors map_canonical_series' effective flag (override wins). The FTS
    // table is referenced by NAME (`work_fts MATCH`), not the join alias — FTS5's
    // MATCH operator reads a bare alias as a column, not the table.
    let total = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM (SELECT 1 FROM work_fts JOIN work w ON w.id = work_fts.work_id \
          WHERE work_fts MATCH ? AND (? = 1 OR COALESCE(w.is_nsfw_override, w.is_nsfw) = 0) \
          LIMIT ?)",
    )
    .bind(&match_expr)
    .bind(show_nsfw as i64)
    .bind(RANK_WINDOW)
    .fetch_one(pool)
    .await?;
    if offset >= RANK_WINDOW {
        return Ok((total, Vec::new()));
    }
    // The title tier + its bind values, chosen by script (see the doc comment).
    let (tier, tier_params): (&str, Vec<String>) = if query.is_ascii() {
        let qx = query.trim().to_lowercase();
        let qp = format!("{}%", escape_like(&qx));
        // Tiers 2/3 repeat the exact/prefix test over `normalized_title` — punctuation
        // folded away, diacritics folded, noise tails stripped — BELOW the raw tiers,
        // never merged into them. Without them the tier compared the user's RAW text
        // against raw titles, so punctuation the user typed differently from the title
        // dropped EVERY row into one bottom tier, leaving `chapters DESC` to decide and
        // letting an unrelated spin-off outrank the work itself. Verified on production:
        // `dr stone` put "Dr. STONE reboot: Byakuya" above "Dr.STONE", and `spy family`
        // ranked "SPY×FAMILY" purely on chapter count; both are top-of-page correct with
        // these tiers, and `dr. stone` / `spy x family` still resolve at tier 0.
        //
        // They must stay BELOW the raw tiers because `normalize_title` ALSO strips noise
        // tails: "Naruto (Official Colored)" normalizes to plain "naruto", so promoting
        // normalized-exact into tier 0 would tie the re-release with *Naruto* itself.
        // Verified unchanged by this addition: naruto / one piece / bleach / dragon ball
        // / chainsaw man all keep the canonical work at rank 1.
        //
        // RESIDUAL (tested, see `search_ranks_the_exact_title_first_despite_punctuation_
        // differences`): because of that same tail-stripping, a punctuation-MISMATCHED
        // query still cannot separate a work from its own re-release — both land in tier
        // 2 and the re-release's larger chapter count wins. That is exactly what the
        // single-tier version did too, so this is a strict improvement rather than a new
        // regression; closing it needs a punctuation-folded but noise-PRESERVING key,
        // which no column stores today.
        (
            "CASE \
               WHEN lower(COALESCE(w.title_override, w.primary_title)) = ? \
                 OR EXISTS (SELECT 1 FROM work_alias a \
                            WHERE a.work_id = w.id AND lower(a.raw_title) = ?) THEN 0 \
               WHEN lower(COALESCE(w.title_override, w.primary_title)) LIKE ? ESCAPE '\\' \
                 OR EXISTS (SELECT 1 FROM work_alias a \
                            WHERE a.work_id = w.id AND lower(a.raw_title) LIKE ? ESCAPE '\\') THEN 1 \
               WHEN EXISTS (SELECT 1 FROM work_alias a \
                            WHERE a.work_id = w.id AND a.normalized_title = ?) THEN 2 \
               WHEN EXISTS (SELECT 1 FROM work_alias a \
                            WHERE a.work_id = w.id \
                              AND a.normalized_title LIKE ? ESCAPE '\\') THEN 3 \
               ELSE 4 END",
            {
                let nq = normalize_title(query);
                let np = format!("{}%", escape_like(&nq));
                vec![qx.clone(), qx, qp.clone(), qp, nq, np]
            },
        )
    } else {
        let nq = normalize_title(query);
        let np = format!("{}%", escape_like(&nq));
        (
            "CASE \
               WHEN EXISTS (SELECT 1 FROM work_alias a \
                            WHERE a.work_id = w.id AND a.normalized_title = ?) THEN 0 \
               WHEN EXISTS (SELECT 1 FROM work_alias a \
                            WHERE a.work_id = w.id \
                              AND a.normalized_title LIKE ? ESCAPE '\\') THEN 1 \
               ELSE 2 END",
            vec![nq, np],
        )
    };
    let sql = format!(
        "WITH cand AS ( \
           SELECT work_fts.work_id AS work_id, \
                  CAST(work_fts.chapters AS INTEGER) AS chapters, \
                  bm25(work_fts, 0.0, 0.0, 10.0, 1.0) AS rank \
             FROM work_fts JOIN work w ON w.id = work_fts.work_id \
            WHERE work_fts MATCH ? AND (? = 1 OR COALESCE(w.is_nsfw_override, w.is_nsfw) = 0) \
            ORDER BY bm25(work_fts, 0.0, 0.0, 10.0, 1.0) \
            LIMIT ?) \
         SELECT c.work_id FROM cand c JOIN work w ON w.id = c.work_id \
          ORDER BY {tier}, c.chapters DESC, c.rank, c.work_id \
          LIMIT ? OFFSET ?"
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql)
        .bind(&match_expr)
        .bind(show_nsfw as i64)
        .bind(RANK_WINDOW);
    for p in &tier_params {
        q = q.bind(p);
    }
    let ids = q
        .bind(page_size.min(RANK_WINDOW))
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok((total, ids))
}

/// NSFW flag of the work owning a mirrored MangaDex chapter (by chapter uuid), for
/// gating `canonicalPages`. `None` if the chapter isn't in the mirror.
pub async fn chapter_owner_is_nsfw(pool: &SqlitePool, external_id: &str) -> Result<Option<bool>> {
    let v = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(w.is_nsfw_override, w.is_nsfw) FROM chapter c \
         JOIN source_series ss ON ss.id = c.source_series_id \
         JOIN work w ON w.id = ss.work_id \
         WHERE c.external_id = ? AND ss.source_type = 'mangadex' LIMIT 1",
    )
    .bind(external_id)
    .fetch_optional(pool)
    .await?;
    Ok(v.map(|n| n != 0))
}

/// Parse a MangaDex chapter number string ("10", "10.5", or None) into a sort key.
/// Unparseable / missing numbers sort last.
fn chapter_sort_key(number: &Option<String>) -> f64 {
    number
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(f64::INFINITY)
}

/// True when `candidate` is a better reader representative than `existing` for the
/// same chapter number. Deterministic so a duplicated number always resolves to the
/// same `external_id` regardless of DB row order (CR2): English wins over non-English,
/// then latest `published_at`, then lowest `external_id`. Without this, two English
/// scanlations of one number would keep the arbitrary first-seen row and
/// `canonicalPages` could serve a different group's pages on reload.
fn prefer_reader_chapter(candidate: &CanonicalChapter, existing: &CanonicalChapter) -> bool {
    use std::cmp::Ordering;
    let cand_en = candidate.lang.as_deref() == Some("en");
    let exist_en = existing.lang.as_deref() == Some("en");
    if cand_en != exist_en {
        return cand_en;
    }
    // Same English-ness: prefer the latest publish (a present date beats `None`),
    // then the lowest `external_id` as a stable final tiebreak.
    match candidate.published_at.cmp(&existing.published_at) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => candidate.external_id < existing.external_id,
    }
}

/// Collapse the raw per-language chapter rows into one row per chapter number,
/// preferring an English translation, ordered ascending by number (number-less rows
/// last). Pure so it's unit-testable without a DB.
fn select_reader_chapters(rows: Vec<CanonicalChapter>) -> Vec<CanonicalChapter> {
    use std::collections::HashMap;
    // Key by number string ("" for number-less); keep the best row per key via a
    // deterministic tiebreak (see `prefer_reader_chapter`).
    let mut best: HashMap<String, CanonicalChapter> = HashMap::new();
    for row in rows {
        let key = row.number.clone().unwrap_or_default();
        match best.get(&key) {
            Some(existing) => {
                if prefer_reader_chapter(&row, existing) {
                    best.insert(key, row);
                }
            }
            None => {
                best.insert(key, row);
            }
        }
    }
    let mut out: Vec<CanonicalChapter> = best.into_values().collect();
    out.sort_by(|a, b| {
        chapter_sort_key(&a.number)
            .partial_cmp(&chapter_sort_key(&b.number))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.external_id.cmp(&b.external_id))
    });
    out
}

/// Insert or update a canonical work from a MangaDex manga (identified by its
/// MangaDex id). Reuses the existing work if the MangaDex external id already maps
/// to one; otherwise mints a new work. Aliases + external ids are added
/// idempotently, and a `mangadex` source_series is ensured. Returns the work id.
pub async fn upsert_work_from_mangadex(
    pool: &SqlitePool,
    mangadex_id: &str,
    input: &WorkInput,
) -> Result<String> {
    // IMMEDIATE, not DEFERRED: this transaction READS (the existing external-id
    // mapping) before it writes, and a DEFERRED read-then-write upgrade fails with
    // SQLITE_BUSY_SNAPSHOT (517) — which `busy_timeout` structurally cannot retry
    // (see `db::is_locked_error`). Taking the write lock up front turns that into
    // ordinary, retryable writer contention.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let existing = sqlx::query_scalar::<_, String>(
        "SELECT work_id FROM work_external_id WHERE provider = 'mangadex' AND external_id = ?",
    )
    .bind(mangadex_id)
    .fetch_optional(&mut *tx)
    .await?;
    let work_id = existing.unwrap_or_else(|| new_id("w_"));
    let now = Utc::now().to_rfc3339();

    // cover_phash uses COALESCE so a sync that hasn't computed the hash yet doesn't
    // wipe a previously-computed one.
    // `metadata_synced_at` is stamped on every MangaDex upsert (H1): it marks that
    // this work's all-language metadata was ATTEMPTED, so the backfill advances
    // past works whose upstream record carries no localized description.
    sqlx::query(
        "INSERT INTO work \
           (id, primary_title, primary_lang, description, year, original_language, status, \
            demographic, content_rating, is_nsfw, author, artist, cover_phash, cover_file_name, created_at, updated_at, metadata_synced_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
           primary_title = excluded.primary_title, primary_lang = excluded.primary_lang, \
           description = excluded.description, year = excluded.year, \
           original_language = excluded.original_language, status = excluded.status, \
           demographic = excluded.demographic, content_rating = excluded.content_rating, \
           is_nsfw = excluded.is_nsfw, author = excluded.author, artist = excluded.artist, \
           cover_phash = COALESCE(excluded.cover_phash, work.cover_phash), \
           cover_file_name = COALESCE(excluded.cover_file_name, work.cover_file_name), \
           updated_at = excluded.updated_at, metadata_synced_at = excluded.metadata_synced_at",
    )
    .bind(&work_id)
    .bind(&input.primary_title)
    .bind(&input.primary_lang)
    .bind(&input.description)
    .bind(input.year)
    .bind(&input.original_language)
    .bind(&input.status)
    .bind(&input.demographic)
    .bind(&input.content_rating)
    .bind(input.is_nsfw as i64)
    .bind(&input.author)
    .bind(&input.artist)
    .bind(&input.cover_phash)
    .bind(&input.cover_file_name)
    .bind(&now)
    .bind(&now)
    .bind(&now) // metadata_synced_at
    .execute(&mut *tx)
    .await?;

    // Ensure the MangaDex external id maps to this work, plus any cross-catalogue ids.
    sqlx::query(
        "INSERT OR IGNORE INTO work_external_id (work_id, provider, external_id) VALUES (?, 'mangadex', ?)",
    )
    .bind(&work_id)
    .bind(mangadex_id)
    .execute(&mut *tx)
    .await?;
    for (provider, ext) in &input.external_ids {
        if provider.is_empty() || ext.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO work_external_id (work_id, provider, external_id) VALUES (?, ?, ?)",
        )
        .bind(&work_id)
        .bind(provider)
        .bind(ext)
        .execute(&mut *tx)
        .await?;
    }

    insert_aliases(&mut tx, &work_id, &input.aliases).await?;
    insert_descriptions_and_credits(&mut tx, &work_id, input).await?;
    replace_source_tags(&mut tx, &work_id, &input.tags).await?;
    ensure_source_series(
        &mut tx,
        &work_id,
        "mangadex",
        "mangadex",
        mangadex_id,
        None,
        input.is_nsfw,
        &now,
    )
    .await?;

    tx.commit().await?;
    Ok(work_id)
}

/// Create a brand-new first-class canonical work (no MangaDex anchor) from an input.
/// Used by the Tier-2 add flow when the matcher decides "new work".
pub async fn create_work(pool: &SqlitePool, input: &WorkInput) -> Result<String> {
    // IMMEDIATE: a write-only transaction here today, but it shares the single writer
    // with the sync/scan/cover paths — taking the lock up front keeps it out of the
    // un-retryable SQLITE_BUSY_SNAPSHOT class if a read is ever added ahead of the
    // first INSERT (see `db::is_locked_error`).
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let work_id = new_id("w_");
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO work \
           (id, primary_title, primary_lang, description, year, original_language, status, \
            demographic, content_rating, is_nsfw, author, artist, cover_phash, cover_file_name, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&work_id)
    .bind(&input.primary_title)
    .bind(&input.primary_lang)
    .bind(&input.description)
    .bind(input.year)
    .bind(&input.original_language)
    .bind(&input.status)
    .bind(&input.demographic)
    .bind(&input.content_rating)
    .bind(input.is_nsfw as i64)
    .bind(&input.author)
    .bind(&input.artist)
    .bind(&input.cover_phash)
    .bind(&input.cover_file_name)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    for (provider, ext) in &input.external_ids {
        if provider.is_empty() || ext.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO work_external_id (work_id, provider, external_id) VALUES (?, ?, ?)",
        )
        .bind(&work_id)
        .bind(provider)
        .bind(ext)
        .execute(&mut *tx)
        .await?;
    }
    insert_aliases(&mut tx, &work_id, &input.aliases).await?;
    insert_descriptions_and_credits(&mut tx, &work_id, input).await?;
    tx.commit().await?;
    Ok(work_id)
}

/// Delete a work and its child rows. Intended to reclaim a freshly-minted work
/// that lost a concurrent dedup claim (H6) — such a work has no `source_series`,
/// `reviews`, or `user_library` rows yet, only the child metadata `create_work`
/// wrote. Explicit child deletes make this correct regardless of the connection's
/// `foreign_keys` pragma (mirrors the `merge_works` cleanup).
pub async fn delete_work_cascade(pool: &SqlitePool, work_id: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // `canonical_library` and `work_cover_issue` carry no enforced FK the app can rely
    // on (the pragma is per-connection), so leaving them out left rows pointing at a
    // work that no longer exists — a phantom library entry, and a cover-issue row that
    // permanently excludes a since-recycled id from the cover crawl. Kept in sync with
    // the same lists in `merge_works` and `purge_foreign_language_suwayomi`.
    for table in [
        "work_alias",
        "work_alias_token",
        "work_external_id",
        "work_description",
        "work_credit",
        "work_cover",
        "work_cover_issue",
        "work_tag",
        "chapter_override",
        "canonical_library",
        "merge_candidate",
    ] {
        let col = if table == "merge_candidate" {
            "candidate_work_id"
        } else {
            "work_id"
        };
        sqlx::query(&format!("DELETE FROM {table} WHERE {col} = ?"))
            .bind(work_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM work WHERE id = ?")
        .bind(work_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Upsert the localized descriptions + credit rows (S2). Idempotent: a
/// re-sync overwrites each `(work, lang)` description in place and `INSERT OR
/// IGNORE`s credits (names never mutate, they only accumulate).
async fn insert_descriptions_and_credits(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    work_id: &str,
    input: &WorkInput,
) -> Result<()> {
    for (lang, text) in &input.descriptions {
        if lang.is_empty() || text.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT INTO work_description (work_id, lang, description) VALUES (?, ?, ?) \
             ON CONFLICT(work_id, lang) DO UPDATE SET description = excluded.description",
        )
        .bind(work_id)
        .bind(lang)
        .bind(text)
        .execute(&mut **tx)
        .await?;
    }
    for (role, name) in &input.credits {
        if name.trim().is_empty() || !matches!(role.as_str(), "author" | "artist") {
            continue;
        }
        sqlx::query("INSERT OR IGNORE INTO work_credit (work_id, role, name) VALUES (?, ?, ?)")
            .bind(work_id)
            .bind(role)
            .bind(name.trim())
            .execute(&mut **tx)
            .await?;
    }
    // F2: covers ACCUMULATE (INSERT OR IGNORE — never delete), so the recurring
    // sweep (which only knows the primary) can't wipe an already-enriched full
    // cover set. The enrichment path adds the rest via the same call.
    for c in &input.covers {
        if c.file_name.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO work_cover (work_id, cover_file_name, lang, volume, is_primary) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(work_id)
        .bind(&c.file_name)
        .bind(&c.lang)
        .bind(&c.volume)
        .bind(c.is_primary as i64)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Replace the INGEST half of a work's `work_tag` rows (migration 0066).
///
/// REPLACE, not accumulate (unlike `work_cover` above): a tag removed upstream must
/// disappear here too, or the genre facet list slowly fills with tags no work actually
/// carries any more — and a facet that returns zero results is worse than a missing one.
///
/// Scoped to `source = 'mangadex'`, which is the entire reason that column exists: an
/// admin's curated rows survive every re-sync untouched, so "curated wins outright"
/// stays true (see `work_effective_genres`).
///
/// `INSERT OR IGNORE` rather than a plain INSERT because the primary key is
/// (work_id, tag): a tag the admin ALSO curated collides. Ignoring keeps the admin's row
/// — and crucially its `source = 'admin'` — so the work stays on the curated tier
/// instead of being quietly demoted to the upstream one.
async fn replace_source_tags(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    work_id: &str,
    tags: &[String],
) -> Result<()> {
    // Deletes UNCONDITIONALLY rather than early-returning on an empty list. An empty
    // upstream tag list is a legitimate state, and a work whose tags were all removed
    // upstream must end up with none here — an early return would keep them forever.
    sqlx::query("DELETE FROM work_tag WHERE work_id = ? AND source = 'mangadex'")
        .bind(work_id)
        .execute(&mut **tx)
        .await?;
    for (ord, tag) in tags.iter().enumerate() {
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO work_tag (work_id, tag, ord, source) \
             VALUES (?, ?, ?, 'mangadex')",
        )
        .bind(work_id)
        .bind(tag)
        .bind(ord as i64)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Rewrite just the upstream (`source = 'mangadex'`) half of an EXISTING work's tags
/// (migration 0066), in a transaction of its own.
///
/// The backfill's genre top-up path: a work already in the spine is skipped for the full
/// `upsert_work_from_mangadex`, but its genres may never have been written at all, and
/// re-running the whole upsert to get them would rewrite every column, every alias, every
/// credit and every cover for 113k works — vastly more write amplification (and
/// SQLITE_BUSY exposure) than the one thing that is actually missing.
///
/// IMMEDIATE for the same reason the other write paths take it: this shares the single
/// writer with the sync/scan/cover tasks, and grabbing the lock up front keeps a
/// contended run out of the un-retryable `SQLITE_BUSY_SNAPSHOT` class.
pub async fn refresh_source_tags(pool: &SqlitePool, work_id: &str, tags: &[String]) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    replace_source_tags(&mut tx, work_id, tags).await?;
    tx.commit().await?;
    Ok(())
}

/// Load a work's cover set (F2), primary first then by volume/file name.
pub async fn load_work_covers(pool: &SqlitePool, work_id: &str) -> Result<Vec<Cover>> {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, i64)>(
        "SELECT cover_file_name, lang, volume, is_primary FROM work_cover WHERE work_id = ? \
         ORDER BY is_primary DESC, \
                  CAST(COALESCE(NULLIF(volume, ''), '0') AS REAL), cover_file_name",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(file_name, lang, volume, is_primary)| Cover {
            file_name,
            lang,
            volume,
            is_primary: is_primary != 0,
        })
        .collect())
}

async fn insert_aliases(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    work_id: &str,
    aliases: &[Alias],
) -> Result<()> {
    for a in aliases {
        let norm = normalize_title(&a.raw);
        if norm.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO work_alias (id, work_id, normalized_title, raw_title, lang) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(new_id("al_"))
        .bind(work_id)
        .bind(&norm)
        .bind(&a.raw)
        .bind(&a.lang)
        .execute(&mut **tx)
        .await?;
        // H9: keep the token inverted index in lockstep with the alias write.
        insert_alias_tokens(tx, work_id, &norm).await?;
    }
    Ok(())
}

/// Admin: add one raw alt-title to a work (idempotent — the alias `UNIQUE` key drops a
/// repeat). Returns the normalized key it indexed, or `""` if the title had no
/// indexable content (nothing written). The caller uses the key to look for exact
/// matches on OTHER works to auto-merge.
pub async fn add_work_alias(pool: &SqlitePool, work_id: &str, raw_title: &str) -> Result<String> {
    let raw = raw_title.trim();
    let norm = normalize_title(raw);
    if norm.is_empty() {
        return Ok(String::new());
    }
    let mut tx = pool.begin().await?;
    insert_aliases(
        &mut tx,
        work_id,
        &[Alias {
            raw: raw.to_string(),
            lang: None,
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(norm)
}

/// Choose the best survivor among a set of works being merged together: a
/// MangaDex-anchored work first (it carries the richest metadata + the canonical
/// spine), then the one with the most sources, then the lowest id. Merging INTO the
/// survivor keeps its description/cover/credits, avoiding the metadata loss that
/// folding a rich work into a bare one would cause. Returns the sole id for a
/// singleton; errors only on a query failure.
pub async fn pick_survivor(pool: &SqlitePool, work_ids: &[String]) -> Result<String> {
    if work_ids.len() == 1 {
        return Ok(work_ids[0].clone());
    }
    let placeholders = std::iter::repeat_n("?", work_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT w.id FROM work w WHERE w.id IN ({placeholders}) \
         ORDER BY (SELECT COUNT(*) FROM source_series ss \
                   WHERE ss.work_id = w.id AND ss.source_type = 'mangadex') > 0 DESC, \
                  (SELECT COUNT(*) FROM source_series ss WHERE ss.work_id = w.id) DESC, \
                  w.id ASC LIMIT 1"
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for id in work_ids {
        q = q.bind(id);
    }
    Ok(q.fetch_one(pool).await?)
}

/// Admin: remove an alt-title from a work — matched by its normalized key OR its exact
/// raw text — then rebuild the work's token index (tokens can be shared across
/// aliases, so a per-token delete is unsafe; rebuild from what remains).
pub async fn remove_work_alias(pool: &SqlitePool, work_id: &str, raw_title: &str) -> Result<()> {
    let raw = raw_title.trim();
    let norm = normalize_title(raw);
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM work_alias WHERE work_id = ? AND (normalized_title = ? OR raw_title = ?)",
    )
    .bind(work_id)
    .bind(&norm)
    .bind(raw)
    .execute(&mut *tx)
    .await?;
    rebuild_work_alias_tokens(&mut tx, work_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Rebuild the `work_alias_token` inverted index for one work from its current alias
/// set. Used after an alias removal, where a shared token might still be carried by
/// another remaining alias — a blind per-token delete would corrupt the index.
async fn rebuild_work_alias_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    work_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM work_alias_token WHERE work_id = ?")
        .bind(work_id)
        .execute(&mut **tx)
        .await?;
    let norms: Vec<String> =
        sqlx::query_scalar("SELECT normalized_title FROM work_alias WHERE work_id = ?")
            .bind(work_id)
            .fetch_all(&mut **tx)
            .await?;
    for n in norms {
        insert_alias_tokens(tx, work_id, &n).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn ensure_source_series(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    work_id: &str,
    source_type: &str,
    source_id: &str,
    source_key: &str,
    source_url: Option<&str>,
    is_nsfw: bool,
    now: &str,
) -> Result<String> {
    sqlx::query(
        "INSERT INTO source_series \
           (id, work_id, source_type, source_id, source_key, source_url, is_nsfw, last_seen, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(source_type, source_id, source_key) DO UPDATE SET \
           last_seen = excluded.last_seen, is_nsfw = excluded.is_nsfw",
    )
    .bind(new_id("ss_"))
    .bind(work_id)
    .bind(source_type)
    .bind(source_id)
    .bind(source_key)
    .bind(source_url)
    .bind(is_nsfw as i64)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM source_series WHERE source_type = ? AND source_id = ? AND source_key = ?",
    )
    .bind(source_type)
    .bind(source_id)
    .bind(source_key)
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

/// Public, non-transactional variant of `ensure_source_series` for the add flow.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_source_series(
    pool: &SqlitePool,
    work_id: &str,
    source_type: &str,
    source_id: &str,
    source_key: &str,
    source_url: Option<&str>,
    is_nsfw: bool,
) -> Result<String> {
    let now = Utc::now().to_rfc3339();
    // IMMEDIATE: `ensure_source_series` upserts and then READS the row back, so a
    // DEFERRED start can land in the un-retryable SQLITE_BUSY_SNAPSHOT class.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let id = ensure_source_series(
        &mut tx,
        work_id,
        source_type,
        source_id,
        source_key,
        source_url,
        is_nsfw,
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// The install coordinates for one Suwayomi source's extension (§2.1). Written by
/// the scanner from the operator-side Suwayomi so a native device can install/pin the
/// exact extension a `source_series` came from. `version_code` is the version at
/// catalogue time — the device keeps its extension at or above it so `source_key`s
/// still resolve.
pub struct SourceExtensionInput {
    pub pkg_name: String,
    pub repo_url: String,
    pub apk_name: Option<String>,
    pub version_code: Option<i64>,
    pub lang: Option<String>,
    pub is_nsfw: bool,
}

/// Upsert one source's extension coordinates, keyed by its Suwayomi `source_id`
/// (the same value carried on `source_series.source_id`). Idempotent — a re-scan
/// overwrites all columns with the freshly-observed values and bumps `updated_at`.
pub async fn upsert_source_extension(
    pool: &SqlitePool,
    source_id: &str,
    input: &SourceExtensionInput,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO source_extension \
           (source_id, pkg_name, repo_url, apk_name, version_code, lang, is_nsfw, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(source_id) DO UPDATE SET \
           pkg_name = excluded.pkg_name, repo_url = excluded.repo_url, \
           apk_name = excluded.apk_name, version_code = excluded.version_code, \
           lang = excluded.lang, is_nsfw = excluded.is_nsfw, updated_at = excluded.updated_at",
    )
    .bind(source_id)
    .bind(&input.pkg_name)
    .bind(&input.repo_url)
    .bind(&input.apk_name)
    .bind(input.version_code)
    .bind(&input.lang)
    .bind(input.is_nsfw as i64)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark the works behind these MangaDex ids as metadata-attempted (H1), so the
/// backfill advances past ids MangaDex didn't return. Idempotent; a no-op for ids
/// with no `mangadex` source_series.
pub async fn mark_metadata_synced(pool: &SqlitePool, mangadex_ids: &[String]) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    for mid in mangadex_ids {
        sqlx::query(
            "UPDATE work SET metadata_synced_at = ? WHERE id IN \
             (SELECT work_id FROM source_series WHERE source_type = 'mangadex' AND source_key = ?)",
        )
        .bind(&now)
        .bind(mid)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Mark the works behind these MangaDex ids as full-cover-fetched (F2), so the
/// cover backfill advances past ids `/cover` returned nothing for. Idempotent.
pub async fn mark_covers_synced(pool: &SqlitePool, mangadex_ids: &[String]) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    for mid in mangadex_ids {
        sqlx::query(
            "UPDATE work SET covers_synced_at = ? WHERE id IN \
             (SELECT work_id FROM source_series WHERE source_type = 'mangadex' AND source_key = ?)",
        )
        .bind(&now)
        .bind(mid)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Escalate a work to NSFW from a source-level signal. Only ever sets the flag —
/// never clears it: "unknown = safe", but once any source signals NSFW the work stays
/// NSFW. Idempotent. The gating reads consult `work.is_nsfw` exclusively (never
/// `source_series.is_nsfw`), so a source-level signal must be OR'd in here to have any
/// effect (N4).
///
/// EXCEPT when MangaDex has authoritatively rated the work `safe`/`suggestive`: that
/// per-title rating wins over a source-level flag, which is unreliable (an aggregator
/// flagged NSFW at the source level taints every mainstream series it also carries —
/// this is what wrongly hid One Piece / Chainsaw Man from anonymous viewers). Matches
/// `genre_is_nsfw`'s own rule that "suggestive is kept SFW-visible". Migration 0053
/// backfilled the historical over-flags; this guard stops them recurring.
pub async fn mark_work_nsfw(pool: &SqlitePool, work_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE work SET is_nsfw = 1, updated_at = ? \
         WHERE id = ? AND is_nsfw = 0 \
           AND (content_rating IS NULL OR content_rating NOT IN ('safe', 'suggestive'))",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(work_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Outcome of folding one work into another (D1 admin merge).
#[derive(Debug, Clone, Default)]
pub struct MergeOutcome {
    pub moved_source_series: u64,
}

/// Fold `source_work` into `target_work` (D1): re-point every mapping + user-data
/// row from source to target, fold the source's identity signals (aliases, external
/// ids, descriptions, credits, covers, tags, chapter overrides, cover_phash,
/// cover_file_name and the NSFW flag) into target, record a `work_redirect` so the
/// source's id keeps resolving, then delete the now-empty source. Idempotent-ish: a
/// mapping the target already has is dropped rather than duplicated. Returns how many
/// `source_series` rows moved. Errors if either work is missing or they're equal.
///
/// LEAKS COVER BLOBS — prefer [`merge_works_ex`]. The cover blobs of the losing work
/// live in a SEPARATE database (see `db::init_covers`) that cannot join this
/// transaction, and this entry point passes no covers pool, so they are orphaned (a
/// live audit found 8,868 orphans / 1.53 GB accumulated exactly this way). Every
/// production call site now goes through `merge_works_ex` with `Some(&st.cover_pool)`;
/// this shim survives only for the tests that don't have a covers pool, and for
/// out-of-tree callers that haven't been converted yet.
#[allow(dead_code)] // no non-test caller left; kept as the covers-pool-free entry point
pub async fn merge_works(
    pool: &SqlitePool,
    source_work: &str,
    target_work: &str,
) -> Result<MergeOutcome> {
    merge_works_ex(pool, None, source_work, target_work).await
}

/// As [`merge_works`], but also reclaims the losing work's cached cover blob from the
/// un-replicated covers pool when one is supplied.
///
/// The blob table (`work_cover_blob`, keyed by `work_id`) is in a different database
/// with no FK to `work`, so nothing ever deleted the loser's row: a live audit found
/// 8,868 orphaned blobs totalling 1.53 GB accumulated from past merges and purges. The
/// delete runs AFTER the main transaction commits — a cross-DB transaction is
/// impossible, and this is the safe order (mirrors `cover::put_work_cover`): a crash in
/// between leaks one blob, exactly the harmless, re-derivable state we already tolerate,
/// whereas deleting first could strip a live cover if the merge then rolled back.
pub async fn merge_works_ex(
    pool: &SqlitePool,
    covers: Option<&SqlitePool>,
    source_work: &str,
    target_work: &str,
) -> Result<MergeOutcome> {
    merge_works_checked(pool, covers, source_work, target_work, None).await
}

/// The identity columns an automated merge decision is made from. Captured by the caller
/// when it evaluates its gate, then re-read and compared INSIDE the merge transaction so
/// the decision cannot be invalidated between the two.
#[derive(Debug, Clone, PartialEq, Eq, Default, sqlx::FromRow)]
pub struct WorkIdentity {
    pub primary_title: Option<String>,
    pub year: Option<i64>,
    pub author: Option<String>,
    pub cover_phash: Option<String>,
}

/// As [`merge_works_ex`], but with an optional OPTIMISTIC-CONCURRENCY precondition:
/// `(expected_source, expected_target)` is the identity snapshot the caller's gate was
/// evaluated against, re-read inside this transaction and required to still match.
///
/// WHY. `merge_works` physically DELETEs the losing work — it is irreversible. The
/// automated consolidation gate (`graphql::consolidate_gate`) reads `primary_title` /
/// `year` / `author` / `cover_phash` on a snapshot taken OUTSIDE any transaction, and
/// `RECONCILE_RUNNING` single-flights the sweep only against itself, never against admin
/// mutations. An `updateSeriesMetadata` landing in that window (retitling a work, or
/// setting the `year` that was the sole corroboration) makes the merge execute against
/// assumptions that no longer hold, and destroys a work on the strength of them. Failing
/// the precondition aborts with `merge precondition failed` and the caller simply
/// re-examines the pair on the next pass, when the gate sees the new values.
///
/// `None` keeps the unconditional behaviour, which is correct for a merge a human
/// explicitly asked for (`mergeWorks`, `resolveMergeCandidate`): the admin IS the
/// authority whose intent a precondition would be protecting.
pub async fn merge_works_checked(
    pool: &SqlitePool,
    covers: Option<&SqlitePool>,
    source_work: &str,
    target_work: &str,
    expected: Option<(&WorkIdentity, &WorkIdentity)>,
) -> Result<MergeOutcome> {
    if source_work == target_work {
        anyhow::bail!("cannot merge a work into itself");
    }
    // IMMEDIATE, not DEFERRED: this transaction opens with two `SELECT id FROM work`
    // reads and then writes across a dozen tables. A DEFERRED read-then-write upgrade
    // fails with SQLITE_BUSY_SNAPSHOT (517), which `busy_timeout` structurally cannot
    // retry (see `db::is_locked_error`) — and the dedup sweep propagates that error
    // with `?`, aborting the whole pass partway. Taking the write lock up front makes
    // the contention ordinary and absorbable.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    for id in [source_work, target_work] {
        let exists: Option<String> = sqlx::query_scalar("SELECT id FROM work WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            anyhow::bail!("no such work: {id}");
        }
    }
    // Precondition re-check, inside the write lock: from here to COMMIT nothing else can
    // change these columns, so a gate that still holds now holds for the whole merge.
    if let Some((exp_src, exp_tgt)) = expected {
        for (id, want) in [(source_work, exp_src), (target_work, exp_tgt)] {
            let now: WorkIdentity = sqlx::query_as(
                "SELECT primary_title, year, author, cover_phash FROM work WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if now != *want {
                anyhow::bail!("merge precondition failed: work {id} changed under the gate");
            }
        }
    }

    // Fold aliases (fresh ids; UNIQUE(work_id,normalized_title,lang) de-dupes).
    let src_aliases = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT normalized_title, raw_title, lang FROM work_alias WHERE work_id = ?",
    )
    .bind(source_work)
    .fetch_all(&mut *tx)
    .await?;
    for (norm, raw, lang) in src_aliases {
        sqlx::query(
            "INSERT OR IGNORE INTO work_alias (id, work_id, normalized_title, raw_title, lang) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(new_id("al_"))
        .bind(target_work)
        .bind(&norm)
        .bind(&raw)
        .bind(&lang)
        .execute(&mut *tx)
        .await?;
        // H9: fold the alias's word tokens into the target's inverted index too.
        insert_alias_tokens(&mut tx, target_work, &norm).await?;
    }
    // Fold external ids by RE-POINTING, not by insert-and-drop.
    //
    // `work_external_id`'s PRIMARY KEY is (provider, external_id) — it does NOT include
    // `work_id`, unlike every other folded child table. So the obvious
    // `INSERT OR IGNORE … SELECT ?, provider, external_id FROM … WHERE work_id = src`
    // collides with the SOURCE's own row on the very key it is copying, is silently
    // IGNORED, and the source rows are then deleted by the cleanup below — i.e. the fold
    // was a complete no-op and every merge DESTROYED the losing work's external ids.
    // Losing the loser's `mangadex` id is the worst case: `upsert_work_from_mangadex`
    // resolves that uuid through this exact table, so the next sync finds no mapping and
    // mints a BRAND-NEW work for it, silently undoing the merge and re-duplicating the
    // catalogue. An `UPDATE` cannot collide (the key is globally unique, so the target
    // can never already hold a key the source holds); `OR IGNORE` is belt-and-braces.
    sqlx::query("UPDATE OR IGNORE work_external_id SET work_id = ? WHERE work_id = ?")
        .bind(target_work)
        .bind(source_work)
        .execute(&mut *tx)
        .await?;
    // Fold the source's METADATA child rows into the target instead of dropping them.
    // Every one of these tables is keyed by (work_id, …), so `INSERT OR IGNORE …
    // SELECT` adds only what the target is missing and the target's own row always
    // wins a collision. Before this, the merge deleted all five outright: a merge that
    // picked the leaner survivor silently destroyed its localized descriptions, credit
    // list, cover set (109k rows catalogue-wide), curated tags and chapter overrides.
    sqlx::query(
        "INSERT OR IGNORE INTO work_description (work_id, lang, description) \
         SELECT ?, lang, description FROM work_description WHERE work_id = ?",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO work_credit (work_id, role, name) \
         SELECT ?, role, name FROM work_credit WHERE work_id = ?",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;
    // `is_primary` is forced to 0 for folded covers when the target already has a
    // primary — two primaries would make `load_work_covers`' ordering arbitrary.
    sqlx::query(
        "INSERT OR IGNORE INTO work_cover (work_id, cover_file_name, lang, volume, is_primary) \
         SELECT ?, cover_file_name, lang, volume, \
                CASE WHEN EXISTS (SELECT 1 FROM work_cover t \
                                  WHERE t.work_id = ? AND t.is_primary = 1) \
                     THEN 0 ELSE is_primary END \
         FROM work_cover WHERE work_id = ?",
    )
    .bind(target_work)
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;
    // Folded tags are appended AFTER the target's own (ord is the admin's ordering,
    // and the two works numbered theirs independently from 0).
    //
    // CURATED HALF ONLY (migration 0066). Folding the upstream half would be pointless
    // and harmful: pointless because `replace_source_tags` rebuilds the survivor's
    // `source = 'mangadex'` rows wholesale on its very next sync, and harmful because
    // until then the survivor's genre list would be the UNION of two works' upstream
    // tags — a blob no single work carries. Human curation is the only half worth
    // rescuing from a work that is about to stop existing.
    sqlx::query(
        "INSERT OR IGNORE INTO work_tag (work_id, tag, ord, source) \
         SELECT ?, tag, ord + (SELECT COALESCE(MAX(ord), -1) + 1 FROM work_tag WHERE work_id = ?), \
                'admin' \
         FROM work_tag WHERE work_id = ? AND source = 'admin'",
    )
    .bind(target_work)
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;
    // Chapter overrides key on the chapter NUMBER, which is shared across the merged
    // sources — so the source's hide/rename edits stay meaningful on the target.
    sqlx::query(
        "INSERT OR IGNORE INTO chapter_override \
             (work_id, chapter_key, hidden, title_override, updated_at) \
         SELECT ?, chapter_key, hidden, title_override, updated_at \
         FROM chapter_override WHERE work_id = ?",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;

    // Fold the work row's own carry-over columns in one statement:
    //   * cover_phash    — gives the target a hash if it has none (strengthens dedup).
    //   * cover_file_name — without this a merge into a cover-less survivor produced a
    //     cover-less merged work even though the loser had one. `cover_cached_version`
    //     is deliberately NOT copied: the blob is keyed by work_id and belongs to the
    //     loser, so the drainer re-materializes it under the target's id.
    //   * is_nsfw — OR'd in, or the merged work is served SFW to anonymous viewers when
    //     only the loser was flagged. Guarded exactly like `mark_work_nsfw`: an
    //     authoritative MangaDex `safe`/`suggestive` rating on the TARGET wins, so this
    //     can't re-create the over-flagging migration 0053 just cleaned up.
    //   * is_nsfw_override — folding only the BASE column leaked adult content: every
    //     gate reads the EFFECTIVE flag `COALESCE(is_nsfw_override, is_nsfw)`, and both
    //     admin "mark NSFW"/"mark SFW" mutations write ONLY the override. A work an admin
    //     had manually marked NSFW while its base flag stayed 0 (2 such works live today)
    //     therefore merged in as SFW and became visible to anonymous viewers. Carried
    //     only when the target has NO override of its own (an admin who ruled on the
    //     TARGET keeps that ruling) and only in the SET direction: dropping a loser's
    //     "mark SFW" leaves the work hidden, which is the safe failure; propagating it
    //     would un-hide the survivor.
    //     NOTE: no content_rating guard here — an override deliberately outranks the
    //     MangaDex rating at read time, so guarding it would silently void the admin's
    //     decision (unlike the base column, which 0053 exists to keep honest).
    sqlx::query(
        "UPDATE work SET \
           cover_phash = COALESCE(cover_phash, (SELECT cover_phash FROM work WHERE id = ?)), \
           cover_file_name = \
             COALESCE(cover_file_name, (SELECT cover_file_name FROM work WHERE id = ?)), \
           is_nsfw = CASE \
             WHEN is_nsfw = 1 THEN 1 \
             WHEN (SELECT is_nsfw FROM work WHERE id = ?) = 1 \
                  AND (content_rating IS NULL \
                       OR content_rating NOT IN ('safe', 'suggestive')) THEN 1 \
             ELSE is_nsfw END, \
           is_nsfw_override = CASE \
             WHEN is_nsfw_override IS NOT NULL THEN is_nsfw_override \
             WHEN (SELECT is_nsfw_override FROM work WHERE id = ?) = 1 THEN 1 \
             ELSE is_nsfw_override END \
         WHERE id = ?",
    )
    .bind(source_work)
    .bind(source_work)
    .bind(source_work)
    .bind(source_work)
    .bind(target_work)
    .execute(&mut *tx)
    .await?;

    // Re-point mappings + user data. `UPDATE OR IGNORE` skips a row that would
    // collide with one the target already has; the leftover source rows are then
    // removed (source_series explicitly, the rest by the final cascade delete).
    let moved = sqlx::query("UPDATE OR IGNORE source_series SET work_id = ? WHERE work_id = ?")
        .bind(target_work)
        .bind(source_work)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM source_series WHERE work_id = ?")
        .bind(source_work)
        .execute(&mut *tx)
        .await?;

    for (table, col) in [
        ("merge_candidate", "candidate_work_id"),
        // Per-user library membership for canonical works lives in `user_library`
        // (series_id = the `w_` work id); repoint it when folding works.
        ("user_library", "series_id"),
        ("reviews", "series_id"),
        // `canonical_library` is the superseded canonical-only library table
        // (migration 0024 folded it into `user_library`, but the table and its rows
        // remain). It was neither repointed nor deleted here, so any row in it would
        // dangle the moment the source `work` row went away.
        ("canonical_library", "work_id"),
    ] {
        sqlx::query(&format!(
            "UPDATE OR IGNORE {table} SET {col} = ? WHERE {col} = ?"
        ))
        .bind(target_work)
        .bind(source_work)
        .execute(&mut *tx)
        .await?;
    }
    // `reviews`/`user_library` have a unique key on (series_id, user_id) /
    // (user_id, series_id), so a user with a row on BOTH merged works had their
    // source row SKIPPED by the `UPDATE OR IGNORE` above (the target's row wins).
    // Delete those losing leftovers so the imminent `work` delete can't orphan a
    // row pointing at a nonexistent work (phantom library entry / stray review).
    // `canonical_library` (PK (user_id, work_id)) skips the same way.
    for (table, col) in [
        ("reviews", "series_id"),
        ("user_library", "series_id"),
        ("canonical_library", "work_id"),
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE {col} = ?"))
            .bind(source_work)
            .execute(&mut *tx)
            .await?;
    }
    // canonical_progress.work_id isn't part of any unique key → plain repoint.
    sqlx::query("UPDATE canonical_progress SET work_id = ? WHERE work_id = ?")
        .bind(target_work)
        .bind(source_work)
        .execute(&mut *tx)
        .await?;
    // Series comments (polymorphic) re-point to the target series id.
    sqlx::query(
        "UPDATE OR IGNORE comments SET target_id = ? WHERE target_type = 'series' AND target_id = ?",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;
    // The remaining polymorphic `w_`-keyed tables. None carries an FK to `work` (they
    // key series generically, by Suwayomi numeric id OR by `w_` work id), so nothing
    // cleaned them and a merge left them pointing at a deleted work:
    //   * user_activity — the profile activity feed deep-links `target_id`, producing
    //     exactly the "No such work" dead bookmark that migration 0056 exists to end;
    //   * notifications — same shape; its `comments` thread was already repointed just
    //     above, so leaving the notification behind broke the deep-link into it.
    // Neither has a unique key over the target column, so a plain UPDATE is enough.
    for table in ["user_activity", "notifications"] {
        sqlx::query(&format!(
            "UPDATE {table} SET target_id = ? WHERE target_type = 'series' AND target_id = ?"
        ))
        .bind(target_work)
        .bind(source_work)
        .execute(&mut *tx)
        .await?;
    }
    // View counters are KEYED by series, so they cannot be repointed blind: the target
    // usually has its own row and `UPDATE` would violate the PK (or, with OR IGNORE,
    // silently discard the loser's views). Sum them into the target instead, then drop
    // the source rows — otherwise `views::trending_keys` keeps ranking a deleted work
    // onto the Trending row, where it resolves to nothing.
    sqlx::query(
        "INSERT INTO series_views (series_key, total, updated_at) \
         SELECT ?, total, updated_at FROM series_views WHERE series_key = ? \
         ON CONFLICT(series_key) DO UPDATE SET \
           total = series_views.total + excluded.total, \
           updated_at = MAX(series_views.updated_at, excluded.updated_at)",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO series_view_bucket (series_key, hour_ts, views) \
         SELECT ?, hour_ts, views FROM series_view_bucket WHERE series_key = ? \
         ON CONFLICT(series_key, hour_ts) DO UPDATE SET \
           views = series_view_bucket.views + excluded.views",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;
    for table in ["series_views", "series_view_bucket"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE series_key = ?"))
            .bind(source_work)
            .execute(&mut *tx)
            .await?;
    }
    // DELIBERATELY NOT TOUCHED HERE — the two remaining tables that key on a `w_` id:
    //
    //   * `series_admin` — keyed by the same generic series id, but `setSeriesAdmin` /
    //     `setSeriesPaused` reject a `w_` id outright ("seriesId must be a numeric
    //     Suwayomi series id"), so a canonical work can never own a row. Verified on
    //     production: 0 `w_`-prefixed rows out of the whole table.
    //
    //   * `work_fts` — a DERIVED index (migration 0052), rebuilt wholesale by
    //     `refresh_work_fts` at boot and at the end of every MangaDex sync cycle — the
    //     same tick that runs the bulk consolidation sweep. Not maintained per-merge ON
    //     PURPOSE: `work_id` is an fts5 `UNINDEXED` column, so any `WHERE work_id = ?` is
    //     a full virtual-table scan (measured 39-41 ms WARM over production's 109,246
    //     rows), and paying that inside this IMMEDIATE transaction would hold the single
    //     writer lock ~40 ms PER MERGE across a sweep that folds thousands — for an index
    //     the next statement rebuilds anyway. A stale row is harmless to output too:
    //     `search_works_fts` inner-joins `work`, so a merged-away id is filtered out of
    //     both the page AND `total`. The residue is freshness only (the survivor is not
    //     findable under the aliases it just absorbed until the next rebuild), the same
    //     window every newly-added work already has.

    // Explicitly remove the source's remaining child rows before deleting it. Their
    // CONTENT has already been folded into the target above, so this only drops the
    // now-redundant source-scoped copies. Production has ON DELETE CASCADE, but being
    // explicit makes the merge correct regardless of the connection's `foreign_keys`
    // pragma (and testable in-memory). `work_cover_issue` is dropped rather than
    // repointed: it records that the LOSER's cover was unprocessable, and repointing it
    // would exclude the surviving work from the cover crawl forever.
    for table in [
        "work_alias",
        "work_alias_token",
        "work_external_id",
        "work_description",
        "work_credit",
        "work_cover",
        "work_cover_issue",
        "work_tag",
        "chapter_override",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE work_id = ?"))
            .bind(source_work)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM merge_candidate WHERE candidate_work_id = ?")
        .bind(source_work)
        .execute(&mut *tx)
        .await?;
    // Folding source_work's sources into target_work turns any candidate that pointed
    // at target_work — and whose source series we just moved there — self-referential
    // (source series and candidate work are now the same work). Those are dead no-ops
    // that would otherwise sit in the review queue forever; drop them here so the queue
    // stays clean without relying on the read-time filter alone.
    // Only PENDING self-refs are noise to purge; a just-`confirmed`/`rejected` row is
    // the audit record of a resolved merge (and the very candidate that drove THIS
    // merge is already `confirmed`) — keep those.
    sqlx::query(
        "DELETE FROM merge_candidate WHERE candidate_work_id = ? AND status = 'pending' \
         AND source_series_id IN (SELECT id FROM source_series WHERE work_id = ?)",
    )
    .bind(target_work)
    .bind(target_work)
    .execute(&mut *tx)
    .await?;
    // Leave a forwarding address (migration 0056) BEFORE the row disappears, so every
    // bookmark, cached reader URL, notification target and shared link minted against
    // the source id keeps resolving instead of 404-ing forever ("No such work").
    // Two statements, in this order:
    //   1. collapse chains — any redirect that pointed AT the work we're about to
    //      delete is rewritten to the survivor, so A->B followed by B->C leaves A->C
    //      and a resolver never has to walk more than one hop;
    //   2. record source -> target itself. `OR REPLACE` keeps it idempotent if the id
    //      is somehow redirected twice.
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE work_redirect SET new_id = ? WHERE new_id = ?")
        .bind(target_work)
        .bind(source_work)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT OR REPLACE INTO work_redirect (old_id, new_id, created_at) VALUES (?, ?, ?)",
    )
    .bind(source_work)
    .bind(target_work)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM work WHERE id = ?")
        .bind(source_work)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    // Cross-DB, so necessarily outside the transaction and AFTER it commits — which is
    // exactly why this must not use `?`. The merge is already durable at this point; a
    // failure here would report an error for work that DID happen, and the callers act
    // on that: `resolve_merge_candidate` reverts the review row to `pending` (leaving a
    // resolved merge stuck as unresolved), and `reconcile`/`consolidate` abort the whole
    // sweep. A leaked blob is re-derivable from MangaDex and is the same harmless state
    // a crash in this window already leaves behind, so log it and carry on.
    if let Some(covers) = covers {
        if let Err(e) = reclaim_cover_blob(covers, source_work).await {
            tracing::warn!(work_id = %source_work, error = %e, "merge: cover blob reclaim failed (leaked)");
        }
    }
    Ok(MergeOutcome {
        moved_source_series: moved,
    })
}

/// Follow a merged-away work id to its survivor (migration 0056), or `None` if the id
/// was never merged. `merge_works` collapses chains on write, so this is a SINGLE
/// lookup — no loop, and no risk of cycling.
///
/// Call sites: `graphql::canonical_series` and `graphql::reload_series_in_shape`, whose
/// not-found branches used to return "No such work" for every id that a merge retired.
/// Reading through the redirect there turns a permanently dead bookmark back into the
/// surviving series.
pub async fn redirect_work_id(pool: &SqlitePool, old_id: &str) -> Result<Option<String>> {
    let new_id =
        sqlx::query_scalar::<_, String>("SELECT new_id FROM work_redirect WHERE old_id = ?")
            .bind(old_id)
            .fetch_optional(pool)
            .await?;
    Ok(new_id)
}

/// Drop a dead work's cached cover blob from the un-replicated covers pool.
///
/// `work_cover_blob` lives in a different database from `work` (see `db::init_covers`)
/// and carries no FK, so a deleted work's blob is invisible to every cascade in the
/// main DB and simply accumulates — a live audit measured 8,868 orphans / 1.53 GB of a
/// 20.4 GB store. Blobs are re-derivable from MangaDex, so a failure here is logged by
/// the caller rather than being allowed to fail a merge that already committed.
pub async fn reclaim_cover_blob(covers: &SqlitePool, work_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM work_cover_blob WHERE work_id = ?")
        .bind(work_id)
        .execute(covers)
        .await?;
    Ok(())
}

/// One raw chapter row from any of a work's sources (S2 aggregation input).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkChapterRow {
    pub number: f64,
    pub title: Option<String>,
    pub source_type: String,
    pub source_id: String,
    pub suwayomi_manga_id: Option<String>,
    pub chapter_id: String,
    pub scanlator: Option<String>,
}

/// One reconciled Suwayomi mapping for a work (F1): the authoritative
/// `source_series` per `source_id`.
#[derive(Debug, Clone)]
pub struct AuthMapping {
    pub source_id: String,
    /// The authoritative Suwayomi manga id (= `source_series.source_key`), the
    /// mapping with the most cached chapters for its `source_id`.
    pub source_key: String,
}

/// The AUTHORITATIVE Suwayomi mapping per `source_id` for a work (F1). The dedup
/// matcher can consolidate several DISTINCT same-source manga onto one work (e.g.
/// two "Naruto" entries from the MangaDex extension, one with chapters and one
/// without), leaving redundant `source_series` rows for the same `source_id`. This
/// picks, per `source_id`, the mapping with the MOST cached chapters (tiebreak:
/// most-recent `last_seen`, then lowest `id`), so `aggregatedChapters`,
/// `workSources`/translators, and live `chapters()` all agree on ONE readable id.
pub async fn authoritative_suwayomi_mappings(
    pool: &SqlitePool,
    work_id: &str,
) -> Result<Vec<AuthMapping>> {
    let rows = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT ss.id, ss.source_id, ss.source_key, \
                (SELECT COUNT(*) FROM suwayomi_chapter sc \
                 WHERE sc.manga_id = CAST(ss.source_key AS INTEGER)) AS cached \
         FROM source_series ss \
         WHERE ss.work_id = ? AND ss.source_type = 'suwayomi' \
         ORDER BY ss.source_id ASC, cached DESC, ss.last_seen DESC, ss.id ASC",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    // The ORDER BY puts the winner per source_id first; keep the first per source_id.
    let mut out: Vec<AuthMapping> = Vec::new();
    for (_ss_id, source_id, source_key, _cached) in rows {
        if out.iter().any(|m| m.source_id == source_id) {
            continue; // already have this source's authoritative mapping
        }
        out.push(AuthMapping {
            source_id,
            source_key,
        });
    }
    Ok(out)
}

/// Every chapter across a work's sources (S2/F1): the AUTHORITATIVE Suwayomi source
/// caches (one mapping per source_id — see `authoritative_suwayomi_mappings`) UNION
/// the MangaDex mirror. Raw rows — the caller groups them by number. This is what
/// makes a work whose MangaDex spine has 0 chapters still show an installed source's
/// chapters, while never surfacing a redundant same-source mapping the reader can't
/// read from.
pub async fn work_source_chapters(pool: &SqlitePool, work_id: &str) -> Result<Vec<WorkChapterRow>> {
    let auth = authoritative_suwayomi_mappings(pool, work_id).await?;
    let mut rows: Vec<WorkChapterRow> = Vec::new();
    // Suwayomi chapters from the authoritative mapping of each source.
    for m in &auth {
        let Ok(manga_id) = m.source_key.parse::<i64>() else {
            continue;
        };
        let chapters = sqlx::query_as::<_, (f64, String, i64, Option<String>)>(
            "SELECT chapter_number, name, id, scanlator FROM suwayomi_chapter WHERE manga_id = ?",
        )
        .bind(manga_id)
        .fetch_all(pool)
        .await?;
        for (number, title, id, scanlator) in chapters {
            rows.push(WorkChapterRow {
                number,
                title: Some(title),
                source_type: "suwayomi".into(),
                source_id: m.source_id.clone(),
                suwayomi_manga_id: Some(m.source_key.clone()),
                chapter_id: id.to_string(),
                scanlator,
            });
        }
    }
    // MangaDex mirror (English).
    // `CAST(c.number AS REAL)` yields 0.0 for a non-numeric number string ("Extra",
    // "Oneshot", …), which would masquerade as a real chapter 0. Require at least one
    // digit so those non-numeric labels are excluded rather than folded onto 0.
    let md = sqlx::query_as::<_, WorkChapterRow>(
        "SELECT CAST(c.number AS REAL) AS number, c.title AS title, 'mangadex' AS source_type, \
                ss.source_id AS source_id, NULL AS suwayomi_manga_id, \
                c.external_id AS chapter_id, NULL AS scanlator \
         FROM source_series ss \
         JOIN chapter c ON c.source_series_id = ss.id \
         WHERE ss.work_id = ? AND ss.source_type = 'mangadex' AND c.lang = 'en' \
           AND c.number IS NOT NULL AND c.number GLOB '*[0-9]*'",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    rows.extend(md);
    Ok(rows)
}

/// The count of "main" chapters to display for a series: the number of DISTINCT
/// chapter numbers after grouping by their integer part (the floor). Grouping folds
/// three kinds of noise into one count that matches what external catalogues headline:
///   - per-scanlator / per-language duplicate rows of the same number collapse;
///   - a sparse ".5" bonus/omake (7.5, 14.5, …) folds into its base chapter (7, 14);
///   - split-part numbering (9.1, 9.2 or 10.1, 10.2 …) folds each real chapter into a
///     single count instead of one-per-part.
///
/// So Tsukimichi — 117 whole chapters carried as 151 scanlator-duplicated rows plus 4
/// ".5" extras — reports 117, while a split-numbered series like "Villainess Level 99"
/// (…9.1, 9.2, 10.1…) still reports its ~22 real chapters rather than the 12 whole
/// numbers that happen to appear. A real "Chapter 0 / Episode 0" first chapter IS
/// counted (common in webtoons/manhwa) by grouping on the floor and keeping group 0;
/// only NEGATIVE / non-finite sentinels are dropped. A source with no non-negative
/// numbers at all falls back to its raw row count so it never collapses to zero.
pub fn main_chapter_count<I: IntoIterator<Item = f64>>(numbers: I) -> i64 {
    use std::collections::HashSet;
    let mut groups: HashSet<i64> = HashSet::new();
    let mut total: i64 = 0;
    for n in numbers {
        total += 1;
        // Non-finite/sentinel and negative rows aren't chapters (but still count toward
        // the raw-row fallback below). A "chapter 0" is real content, so it's kept.
        if !n.is_finite() || n < 0.0 {
            continue;
        }
        groups.insert(n.floor() as i64);
    }
    if groups.is_empty() {
        total
    } else {
        groups.len() as i64
    }
}

/// `main_chapter_count` over reader chapters whose numbers are `Option<String>` (the
/// MangaDex mirror shape). Number-less rows only feed the raw-row fallback.
pub fn main_chapter_count_str(chapters: &[CanonicalChapter]) -> i64 {
    main_chapter_count(chapters.iter().map(|c| {
        c.number
            .as_deref()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(f64::NAN)
    }))
}

/// The aggregate chapter count of a work (S2): the number of main chapters across all
/// its sources (see `main_chapter_count`). This is the count the reader should see — a
/// work whose MangaDex spine has 0 chapters but whose asurascans source has 201
/// reports 201, not 0.
pub async fn aggregate_chapter_count(pool: &SqlitePool, work_id: &str) -> Result<i64> {
    let rows = work_source_chapters(pool, work_id).await?;
    Ok(main_chapter_count(rows.iter().map(|r| r.number)))
}

/// Resolve the id of an existing source_series by its natural key.
pub async fn find_source_series_id(
    pool: &SqlitePool,
    source_type: &str,
    source_id: &str,
    source_key: &str,
) -> Result<Option<String>> {
    let id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM source_series WHERE source_type = ? AND source_id = ? AND source_key = ?",
    )
    .bind(source_type)
    .bind(source_id)
    .bind(source_key)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Resolve the `(id, work_id)` of an existing source_series by its natural key —
/// for the idempotency pre-check in the Tier-2 add flow, which needs the linked
/// work to report back without re-running the matcher (DD2).
pub async fn find_source_series(
    pool: &SqlitePool,
    source_type: &str,
    source_id: &str,
    source_key: &str,
) -> Result<Option<(String, String)>> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT id, work_id FROM source_series \
         WHERE source_type = ? AND source_id = ? AND source_key = ?",
    )
    .bind(source_type)
    .bind(source_id)
    .bind(source_key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Resolve `(id, work_id)` of a linked source_series by `(source_type, source_key)`
/// alone — no `source_id`. A Suwayomi manga id (the `source_key`) is a global
/// autoincrement, unique across sources, so this is unambiguous for `suwayomi`. Lets
/// the enrol path (OPT-6) pre-check linkage BEFORE fetching the manga (which is the
/// only way it would otherwise learn the `source_id`), so a re-enrol of an
/// already-linked series issues no upstream call.
pub async fn find_source_series_by_key(
    pool: &SqlitePool,
    source_type: &str,
    source_key: &str,
) -> Result<Option<(String, String)>> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT id, work_id FROM source_series WHERE source_type = ? AND source_key = ?",
    )
    .bind(source_type)
    .bind(source_key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Upsert one mirrored chapter under a source_series (idempotent on external id).
pub async fn upsert_chapter(
    pool: &SqlitePool,
    source_series_id: &str,
    ch: &ChapterInput,
) -> Result<()> {
    if ch.external_id.is_empty() {
        return Ok(());
    }
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO chapter \
           (id, source_series_id, external_id, number, volume, lang, title, published_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(source_series_id, external_id) DO UPDATE SET \
           number = excluded.number, volume = excluded.volume, lang = excluded.lang, \
           title = excluded.title, published_at = excluded.published_at",
    )
    .bind(new_id("ch_"))
    .bind(source_series_id)
    .bind(&ch.external_id)
    .bind(&ch.number)
    .bind(&ch.volume)
    .bind(&ch.lang)
    .bind(&ch.title)
    .bind(&ch.published_at)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Enqueue a mid-confidence match for manual admin review. `Ok(Some(id))` is a freshly
/// inserted row; `Ok(None)` means the pair was SUPPRESSED because a candidate for the
/// very same `(source_series_id, candidate_work_id)` already exists — in ANY status.
///
/// AN ADMIN'S DECISION IS FINAL. This used to be a plain `INSERT`, so every writer that
/// re-derived the same pair appended another row. The dedup scanner re-derives them
/// constantly: `RECONCILE_PENDING_WHERE` only excludes a work with a *pending*
/// candidate, so the moment an admin REJECTS a pair the work becomes selectable again,
/// the same fuzzy match is recomputed, and a brand-new `pending` row silently reverses
/// the human "no". Verified on live data (2026-07-26): 5 duplicate pairs, 4 of them
/// `rejected` → re-proposed as `pending`; 12 distinct rejected pairs were still
/// re-proposable, and the consolidation gate now routes ~10.4k refusals through this
/// same queue. Suppression is on the PAIR, not the work: a work whose match against
/// work A was rejected can still be proposed against work B.
///
/// Re-proposal is refused unconditionally, including "the evidence changed" (a new
/// pHash, a newly-learned author). The score is advisory metadata — an admin rejects a
/// PAIR ("these are different series"), a judgement a better similarity number does not
/// invalidate, and there is no UI that would show the admin "this is back because the
/// cover hash changed". A genuinely new decision is available through the admin merge
/// mutation, which is explicit.
///
/// Idempotent and race-free without a UNIQUE index: the existence test and the insert
/// are ONE statement (`INSERT … SELECT … WHERE NOT EXISTS`), so under SQLite's single
/// writer a concurrent enqueue of the same pair loses the race and inserts nothing,
/// rather than erroring out of the ingest path a plain `INSERT` + unique index would.
pub async fn insert_merge_candidate(
    pool: &SqlitePool,
    source_series_id: &str,
    candidate_work_id: &str,
    score: f64,
    method: &str,
) -> Result<Option<String>> {
    let id = new_id("mc_");
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "INSERT INTO merge_candidate \
           (id, source_series_id, candidate_work_id, score, method, status, created_at) \
         SELECT ?, ?, ?, ?, ?, 'pending', ? \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM merge_candidate \
             WHERE source_series_id = ? AND candidate_work_id = ?)",
    )
    .bind(&id)
    .bind(source_series_id)
    .bind(candidate_work_id)
    .bind(score)
    .bind(method)
    .bind(&now)
    .bind(source_series_id)
    .bind(candidate_work_id)
    .execute(pool)
    .await?;
    Ok((res.rows_affected() > 0).then_some(id))
}

/// A sync job's persisted state.
#[derive(Debug, Clone)]
pub struct SyncState {
    /// While `seed_done` is false this is a provisional `createdAt` resume point;
    /// afterwards it's the incremental `updatedAtSince` cursor.
    pub cursor: String,
    /// Whether the initial full `createdAt` seed has completed at least once.
    pub seed_done: bool,
}

/// Read a job's full sync state (cursor + whether its initial seed has finished).
/// `None` means the job has never run → do a fresh full `createdAt` seed.
pub async fn get_sync_state(pool: &SqlitePool, job: &str) -> Result<Option<SyncState>> {
    let row = sqlx::query_as::<_, (String, i64)>(
        "SELECT last_synced_at, seed_done FROM catalogue_sync_state WHERE job = ?",
    )
    .bind(job)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(cursor, seed_done)| SyncState {
        cursor,
        seed_done: seed_done != 0,
    }))
}

/// Persist provisional seed progress (leaving `seed_done = 0`) so an interrupted
/// full seed resumes from `since` instead of restarting at `createdAt`=0 (M6).
pub async fn set_seed_progress(pool: &SqlitePool, job: &str, since: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO catalogue_sync_state (job, last_synced_at, updated_at, seed_done) \
         VALUES (?, ?, ?, 0) \
         ON CONFLICT(job) DO UPDATE SET last_synced_at = excluded.last_synced_at, \
           updated_at = excluded.updated_at",
    )
    .bind(job)
    .bind(since)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a job's initial full seed complete and set its incremental cursor to
/// `since` (the wall-clock at the completed cycle's start).
pub async fn mark_seed_done(pool: &SqlitePool, job: &str, since: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO catalogue_sync_state (job, last_synced_at, updated_at, seed_done) \
         VALUES (?, ?, ?, 1) \
         ON CONFLICT(job) DO UPDATE SET last_synced_at = excluded.last_synced_at, \
           updated_at = excluded.updated_at, seed_done = 1",
    )
    .bind(job)
    .bind(since)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a one-time maintenance pass (migration 0055) has already completed.
pub async fn maintenance_flag_present(pool: &SqlitePool, key: &str) -> Result<bool> {
    let n: Option<i64> = sqlx::query_scalar("SELECT 1 FROM maintenance_flag WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(n.is_some())
}

/// Record a one-time maintenance pass as complete (idempotent).
pub async fn set_maintenance_flag(pool: &SqlitePool, key: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO maintenance_flag (key, done_at) VALUES (?, ?)")
        .bind(key)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
    Ok(())
}

/// Persist the sync cursor for a job. `since` is the MangaDex `since` timestamp to use
/// as `updatedAtSince` on the next incremental cycle (typically the wall-clock at the
/// start of the cycle just completed, so anything updated during it is caught next time).
pub async fn set_sync_cursor(pool: &SqlitePool, job: &str, since: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO catalogue_sync_state (job, last_synced_at, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(job) DO UPDATE SET last_synced_at = excluded.last_synced_at, \
           updated_at = excluded.updated_at",
    )
    .bind(job)
    .bind(since)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Clear a sync job's row entirely so the next cycle treats it as a fresh seed
/// (`get_sync_state` → `None` → full createdAt sweep from scratch). Used by the admin
/// `resyncCatalogue` mutation, since `seed_done` can't be reset via raw SQL (the DB is
/// container-owned and there's no sqlite3 in the image).
pub async fn reset_sync_state(pool: &SqlitePool, job: &str) -> Result<()> {
    sqlx::query("DELETE FROM catalogue_sync_state WHERE job = ?")
        .bind(job)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Extension-level sync subscriptions (source-sync §3) ──────────────────────────

/// Enable/disable an extension's sync subscription. Enabling is an idempotent upsert
/// (a re-enable keeps the original `created_at` and prior sync stats); disabling drops
/// the row so it's no longer walked by the background source-sync job.
pub async fn set_extension_subscription(
    pool: &SqlitePool,
    pkg_name: &str,
    subscribed: bool,
) -> Result<()> {
    if subscribed {
        sqlx::query(
            "INSERT INTO extension_subscription (pkg_name, created_at) VALUES (?, ?) \
             ON CONFLICT(pkg_name) DO NOTHING",
        )
        .bind(pkg_name)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
    } else {
        sqlx::query("DELETE FROM extension_subscription WHERE pkg_name = ?")
            .bind(pkg_name)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Every subscribed extension's package id — the work-list for one source-sync pass.
pub async fn subscribed_extensions(pool: &SqlitePool) -> Result<Vec<String>> {
    // Breaker-disabled subscriptions are skipped — see SUBSCRIPTION_FAILURE_LIMIT.
    // They stay in the table (so the admin surface can show why they stopped and
    // offer a re-enable) but are not walked.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT pkg_name FROM extension_subscription WHERE disabled_at IS NULL ORDER BY pkg_name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// The subscribed package ids as a set, for badging the admin `extensions` listing
/// with `subscribed` in one query instead of a per-row lookup.
pub async fn subscribed_extension_set(
    pool: &SqlitePool,
) -> Result<std::collections::HashSet<String>> {
    Ok(subscribed_extensions(pool).await?.into_iter().collect())
}

/// How many consecutive failing passes before a subscription is auto-disabled.
/// Generous enough to ride out a transient upstream outage or a flaresolverr restart,
/// small enough that a genuinely-dead source stops being retried daily forever.
pub const SUBSCRIPTION_FAILURE_LIMIT: i64 = 5;

/// Record the outcome of one sync pass for an extension. `error` is `None` on a clean
/// pass.
///
/// A clean pass resets `consecutive_failures`; a failing one increments it and, at
/// `SUBSCRIPTION_FAILURE_LIMIT`, trips the breaker by stamping `disabled_at`. Returns
/// `true` if this call disabled the subscription, so the caller can log it once.
pub async fn mark_subscription_synced(
    pool: &SqlitePool,
    pkg_name: &str,
    added: i64,
    error: Option<&str>,
) -> Result<bool> {
    // Done in one statement so a concurrent pass can't interleave a read-modify-write
    // and lose a strike.
    let now = Utc::now().to_rfc3339();
    if error.is_none() {
        sqlx::query(
            "UPDATE extension_subscription \
             SET last_synced_at = ?, last_added = ?, last_error = NULL, \
                 consecutive_failures = 0 \
             WHERE pkg_name = ?",
        )
        .bind(&now)
        .bind(added)
        .bind(pkg_name)
        .execute(pool)
        .await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE extension_subscription \
         SET last_synced_at = ?, last_added = ?, last_error = ?, \
             consecutive_failures = consecutive_failures + 1, \
             disabled_at = CASE \
                 WHEN disabled_at IS NOT NULL THEN disabled_at \
                 WHEN consecutive_failures + 1 >= ? THEN ? \
                 ELSE NULL END \
         WHERE pkg_name = ?",
    )
    .bind(&now)
    .bind(added)
    .bind(error)
    .bind(SUBSCRIPTION_FAILURE_LIMIT)
    .bind(&now)
    .bind(pkg_name)
    .execute(pool)
    .await?;
    let tripped: Option<(String,)> = sqlx::query_as(
        "SELECT disabled_at FROM extension_subscription \
         WHERE pkg_name = ? AND disabled_at = ?",
    )
    .bind(pkg_name)
    .bind(&now)
    .fetch_optional(pool)
    .await?;
    Ok(tripped.is_some())
}

/// Re-enable a subscription the breaker disabled, clearing its failure state ENTIRELY —
/// the source starts again from zero strikes and needs a fresh
/// `SUBSCRIPTION_FAILURE_LIMIT` consecutive failures to re-trip.
///
/// That full reset is right for the ADMIN "I've fixed it, try again" signal
/// (`setExtensionSubscription`, `graphql/mod.rs`), where the human is asserting the
/// source is healthy and the prior strikes are stale. It is WRONG for the automatic
/// timed re-arm — see [`rearm_subscription_breaker_probe`].
pub async fn reset_subscription_breaker(pool: &SqlitePool, pkg_name: &str) -> Result<()> {
    sqlx::query(
        "UPDATE extension_subscription \
         SET disabled_at = NULL, consecutive_failures = 0, last_error = NULL \
         WHERE pkg_name = ?",
    )
    .bind(pkg_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Re-arm a breaker-disabled subscription for a single PROBE pass: clear `disabled_at`
/// but leave the strike count one short of `SUBSCRIPTION_FAILURE_LIMIT`, so one more
/// failure re-trips immediately while one success resets it to zero
/// (`mark_subscription_synced` zeroes on success).
///
/// This is what `sync::rearm_stale_breakers` means by a probe, and the arithmetic is the
/// whole point. `BREAKER_REARM_HOURS` documents the cost of a still-dead source as "one
/// wasted walk a week"; with a FULL reset it is five — the source must fail
/// `SUBSCRIPTION_FAILURE_LIMIT` (5) consecutive daily passes before the breaker trips
/// again, i.e. 5 wasted walks per 5-day-probe + 7-day-disabled cycle, five times the
/// documented cost and, with 12 subscriptions, five times the pointless upstream load.
/// Arming at `LIMIT - 1` makes the code match the documented intent: 1 wasted walk, then
/// disabled again for the week.
///
/// Deliberately NOT folded into `reset_subscription_breaker` by mutating it in place: the
/// admin re-subscribe path must keep the full reset (a source an admin just fixed should
/// not be one blip from being disabled again for a week). The two callers want genuinely
/// different semantics, so they get two functions.
///
/// HANDOFF: currently unused in the binary. `sync::rearm_stale_breakers` (`sync.rs:438`)
/// still calls `reset_subscription_breaker`; that one call must become
/// `rearm_subscription_breaker_probe` for the fix to take effect. `sync.rs` belongs to
/// another agent, so the swap is not made here — see the fix report.
#[allow(dead_code)]
pub async fn rearm_subscription_breaker_probe(pool: &SqlitePool, pkg_name: &str) -> Result<()> {
    sqlx::query(
        "UPDATE extension_subscription \
         SET disabled_at = NULL, consecutive_failures = MAX(? - 1, 0) \
         WHERE pkg_name = ?",
    )
    .bind(SUBSCRIPTION_FAILURE_LIMIT)
    .bind(pkg_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Every enrolled Suwayomi manga id (the `source_key`), for the library-reconcile pass
/// that re-asserts `inLibrary=true` upstream so drifted/single-added series keep being
/// scanned. Manga ids are globally unique across sources, so the source_id is not needed.
pub async fn suwayomi_source_keys(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT source_key FROM source_series WHERE source_type = 'suwayomi'")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(k,)| k).collect())
}

/// Purge every enrolled Suwayomi series whose source language is not English, and
/// return their Suwayomi manga ids so the caller can best-effort `inLibrary=false`
/// them upstream. Komika serves English only, but the multi-language `all.mangadex`
/// extension had leaked ~59 languages of series (with native-language titles) and
/// their chapters into the library before the English-only enrolment filter landed.
/// This removes the ones already mirrored: their `source_series` link, scan state,
/// cached chapters, and the `suwayomi_series` row. A work left with no remaining
/// source_series is cascade-deleted; a work still anchored to the MangaDex catalogue
/// (the common case — non-English titles dedup onto multi-language canonical works)
/// keeps its row and merely loses the Suwayomi link. Idempotent and cheap once
/// drained (the target set is then empty), so it is safe to run every reconcile pass.
/// `lang IS NULL` rows are deliberately NOT touched: an unknown language must not be
/// mistaken for non-English and delete a legitimate series.
pub async fn purge_foreign_language_suwayomi(pool: &SqlitePool) -> Result<Vec<i64>> {
    // The non-English target set, as manga ids (INTEGER) and as TEXT (the form every
    // satellite table keys the Suwayomi id in).
    const NON_EN: &str = "SELECT id FROM suwayomi_series WHERE lang IS NOT NULL AND lang <> 'en'";
    let non_en_text = format!("(SELECT CAST(id AS TEXT) FROM ({NON_EN}) t)");

    // Cheap pre-check OUTSIDE the write lock: the steady state is "nothing to purge"
    // (this runs every reconcile pass), and opening a write transaction just to discover
    // that would contend with ingest for the single SQLite writer for no reason.
    let any: Option<i64> = sqlx::query_scalar(&format!("{NON_EN} LIMIT 1"))
        .fetch_optional(pool)
        .await?;
    if any.is_none() {
        return Ok(Vec::new());
    }

    // Everything in ONE write transaction so recovery is atomic and idempotent: a crash
    // before commit rolls back wholesale and the next reconcile re-runs cleanly; a
    // commit finishes the job (the previous version cascaded orphans AFTER the commit,
    // so an ill-timed restart — which a server rebuild causes — permanently leaked the
    // orphaned works). Delete every row that references `suwayomi_series` via subquery
    // BEFORE deleting `suwayomi_series` itself, so the subqueries still resolve.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    // BOTH reads happen INSIDE the write lock, and this is load-bearing, not tidiness.
    //
    // `orphan_works` is a snapshot of "works whose every source_series is about to be
    // deleted", and it is applied as an unconditional `DELETE FROM work WHERE id IN (…)`.
    // Read outside the transaction, a concurrent ingest linking a NEW source_series to
    // one of those works in the gap — the federated/admin add path does exactly this —
    // leaves the work no longer orphaned, but it is cascade-deleted anyway, taking a
    // live series with it. `BEGIN IMMEDIATE` holds the writer for the whole span, so the
    // set cannot go stale between being computed and being applied.
    //
    // `series_ids` is read here for the same reason: it is RETURNED so the caller can
    // best-effort `inLibrary=false` those ids upstream, while the deletes below are
    // driven by the `NON_EN` subquery re-evaluated inside the transaction. Read outside,
    // a series arriving in the gap would be deleted but not reported, so Suwayomi would
    // keep it enrolled and re-mirror it on the next scan.
    let series_ids: Vec<i64> = sqlx::query_scalar(NON_EN).fetch_all(&mut *tx).await?;
    if series_ids.is_empty() {
        tx.rollback().await?;
        return Ok(Vec::new());
    }

    // Works that will be ORPHANED by the purge: every one of their source_series is a
    // to-be-purged Suwayomi link. Computed BEFORE the deletes (it needs the links
    // intact). A work still anchored to the MangaDex catalogue or an English source is
    // excluded (the non-purged branch count is > 0), so consolidated works survive and
    // merely lose the foreign-language translator mapping.
    let orphan_works: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT ss.work_id FROM source_series ss GROUP BY ss.work_id \
         HAVING SUM(CASE WHEN ss.source_type = 'suwayomi' AND ss.source_key IN {non_en_text} \
                         THEN 0 ELSE 1 END) = 0"
    ))
    .fetch_all(&mut *tx)
    .await?;

    // Satellite rows keyed by the numeric Suwayomi id (as TEXT): per-user library +
    // reading progress, admin overrides, and view counters. Left uncleaned these
    // dangle forever (e.g. a purged series stuck in a user's library, re-rendered live
    // on every view) since none carry an enforced FK to the refreshable cache.
    for stmt in [
        format!("DELETE FROM user_library      WHERE series_id  IN {non_en_text}"),
        format!("DELETE FROM suwayomi_progress  WHERE series_id  IN {non_en_text}"),
        format!("DELETE FROM series_admin       WHERE series_id  IN {non_en_text}"),
        format!("DELETE FROM series_views       WHERE series_key IN {non_en_text}"),
        format!("DELETE FROM series_view_bucket WHERE series_key IN {non_en_text}"),
        format!("DELETE FROM series_scan_state  WHERE series_id  IN {non_en_text}"),
        format!(
            "DELETE FROM source_series WHERE source_type = 'suwayomi' AND source_key IN {non_en_text}"
        ),
        format!("DELETE FROM suwayomi_chapter WHERE manga_id IN ({NON_EN})"),
    ] {
        sqlx::query(&stmt).execute(&mut *tx).await?;
    }

    // Cascade the orphaned works + their child rows, in the same transaction. Mirrors
    // `delete_work_cascade`'s table set (kept in sync with it), but set-based so a
    // first-run purge of thousands doesn't fan out into per-work round-trips.
    if !orphan_works.is_empty() {
        let ph = std::iter::repeat_n("?", orphan_works.len())
            .collect::<Vec<_>>()
            .join(",");
        for (table, col) in [
            ("work_alias", "work_id"),
            ("work_alias_token", "work_id"),
            ("work_external_id", "work_id"),
            ("work_description", "work_id"),
            ("work_credit", "work_id"),
            ("work_cover", "work_id"),
            ("work_cover_issue", "work_id"),
            ("work_tag", "work_id"),
            ("chapter_override", "work_id"),
            ("canonical_library", "work_id"),
            ("merge_candidate", "candidate_work_id"),
            ("work", "id"),
        ] {
            let sql = format!("DELETE FROM {table} WHERE {col} IN ({ph})");
            let mut q = sqlx::query(&sql);
            for w in &orphan_works {
                q = q.bind(w);
            }
            q.execute(&mut *tx).await?;
        }
    }

    // Drop review candidates whose source_series was just removed (dangling by
    // source_series_id — the candidate_work_id ones are handled by the cascade above).
    sqlx::query(
        "DELETE FROM merge_candidate WHERE source_series_id NOT IN (SELECT id FROM source_series)",
    )
    .execute(&mut *tx)
    .await?;

    // suwayomi_series LAST — every subquery above resolves against it.
    sqlx::query(&format!(
        "DELETE FROM suwayomi_series WHERE id IN ({NON_EN})"
    ))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(series_ids)
}

/// Backfill a "due now" scan-state row for every enrolled Suwayomi series that lacks one,
/// so the DB-driven scanner (which selects work from `series_scan_state`) picks up series
/// enrolled by paths that don't scan-on-enrol (federated search) or predate scan-state.
/// One set-based INSERT…SELECT (no per-row round-trips); returns how many rows it added.
pub async fn backfill_pending_scan_states(pool: &SqlitePool) -> Result<u64> {
    // `ON CONFLICT DO NOTHING` in addition to the `NOT EXISTS` guard: a concurrent
    // `ensure_pending` (federated ingest) can insert the same series_id between this
    // statement's NOT-EXISTS check and its insert; without the conflict clause that race
    // raises a UNIQUE violation that rolls back the ENTIRE multi-row backfill (audit LOW).
    // `next_scan_at = DUE_NOW_SENTINEL` (not NULL): the due-query is a bounded `<= ?` range,
    // so a NULL row would never be selected and the series would silently never scan.
    let res = sqlx::query(
        "INSERT INTO series_scan_state (series_id, next_scan_at, updated_at) \
         SELECT ss.source_key, ?, ? FROM source_series ss \
         WHERE ss.source_type = 'suwayomi' \
           AND NOT EXISTS (SELECT 1 FROM series_scan_state sst WHERE sst.series_id = ss.source_key) \
         ON CONFLICT(series_id) DO NOTHING",
    )
    .bind(crate::scanner::DUE_NOW_SENTINEL)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Which of `keys` are already enrolled for `source_type` — one set-based query instead
/// of a per-key round-trip (audit #10, the source-sync LATEST walk). Returns the subset
/// of `keys` that exist; unknown keys are the caller's "new" set.
pub async fn existing_source_keys(
    pool: &SqlitePool,
    source_type: &str,
    keys: &[String],
) -> Result<std::collections::HashSet<String>> {
    // Chunk so the `IN (?, ?, …)` bind count can't approach SQLite's
    // SQLITE_MAX_VARIABLE_NUMBER, even if a caller ever passes a very large key set (the
    // LATEST-walk caller only passes one browse page, but keep the helper generally safe).
    const CHUNK: usize = 500;
    let mut found = std::collections::HashSet::new();
    for chunk in keys.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT source_key FROM source_series \
             WHERE source_type = ? AND source_key IN ({placeholders})"
        );
        let mut q = sqlx::query_scalar::<_, String>(&sql).bind(source_type);
        for k in chunk {
            q = q.bind(k);
        }
        found.extend(q.fetch_all(pool).await?);
    }
    Ok(found)
}

/// Record that a full source-sync pass just completed, for restart-throttling
/// (`source_sync_due`). Singleton row keyed on id=1.
pub async fn mark_source_sync_pass(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "INSERT INTO sync_state (id, last_full_pass_at) VALUES (1, ?) \
         ON CONFLICT(id) DO UPDATE SET last_full_pass_at = excluded.last_full_pass_at",
    )
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a full source-sync pass is due — true when none has ever run, or the last one
/// completed at least ~90% of `interval_seconds` ago. Lets the scheduler skip the redundant
/// immediate pass that `tokio::time::interval` fires on every restart (audit #3).
///
/// The 90% threshold (not a full interval) is deliberate: a scheduled tick fires exactly
/// one interval after the previous tick *start*, but the pass is only stamped when it
/// *completes* — a duration `T` later. Comparing against a full interval would make every
/// scheduled tick see `interval - T < interval` and skip, silently halving the cadence to
/// ~2 intervals. The 10% slack absorbs any pass shorter than `0.1 * interval` (e.g. ~2.4h
/// at the 1-day default) so scheduled ticks run on time while restart ticks are still
/// skipped when a pass ran recently.
pub async fn source_sync_due(pool: &SqlitePool, interval_seconds: u64) -> bool {
    let last: Option<String> =
        sqlx::query_scalar("SELECT last_full_pass_at FROM sync_state WHERE id = 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let threshold = (interval_seconds as i64) * 9 / 10;
    match last.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok()) {
        None => true,
        Some(t) => (Utc::now() - t.with_timezone(&Utc)).num_seconds() >= threshold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[test]
    fn fts_match_query_stars_only_the_last_token_and_refuses_tiny_queries() {
        // Only the trailing (still-being-typed) token is prefix-matched; starring the
        // complete leading tokens only widened the postings list scanned.
        assert_eq!(
            fts_match_query("solo lev").as_deref(),
            Some("\"solo\" \"lev\"*")
        );
        assert_eq!(fts_match_query("naruto").as_deref(), Some("\"naruto\"*"));
        // Operator/keyword chars stay literal, quoted.
        assert_eq!(
            fts_match_query("re:zero AND").as_deref(),
            Some("\"re\" \"zero\" \"and\"*")
        );
        // Too short to run: `a`/`th` prefix-matched 30-40k of 109k indexed works and
        // cost seconds of unauthenticated server time per keystroke.
        assert_eq!(fts_match_query("a"), None);
        assert_eq!(fts_match_query("th"), None);
        assert_eq!(fts_match_query("  !! "), None);
        // …but a short NON-ASCII query is a whole word, not a one-letter prefix.
        assert_eq!(fts_match_query("鬼滅").as_deref(), Some("\"鬼滅\"*"));
    }

    fn slime_input() -> WorkInput {
        WorkInput {
            primary_title: Some("That Time I Got Reincarnated as a Slime".into()),
            description: Some("Mikami Satoru is reincarnated as a slime.".into()),
            year: Some(2015),
            is_nsfw: false,
            aliases: vec![
                Alias {
                    raw: "That Time I Got Reincarnated as a Slime".into(),
                    lang: Some("en".into()),
                },
                Alias {
                    raw: "Tensei Shitara Slime Datta Ken".into(),
                    lang: Some("ja-ro".into()),
                },
            ],
            external_ids: vec![("al".into(), "101517".into())],
            ..Default::default()
        }
    }

    async fn insert_suwayomi_series(pool: &SqlitePool, id: i64, lang: Option<&str>) {
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, lang, in_library, updated_at) \
             VALUES (?, ?, 'ONGOING', 'src', ?, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(format!("Series {id}"))
        .bind(lang)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn purge_removes_non_english_and_keeps_consolidated_and_english() {
        let pool = pool().await;

        // English series (id 100) — must fully survive.
        let w_en = create_work(&pool, &slime_input()).await.unwrap();
        insert_suwayomi_series(&pool, 100, Some("en")).await;
        upsert_source_series(&pool, &w_en, "suwayomi", "src", "100", None, false)
            .await
            .unwrap();

        // Non-English series (id 200) CONSOLIDATED onto a MangaDex-anchored work — the
        // work must survive (still has its mangadex link); only the Suwayomi link goes.
        let w_md = create_work(&pool, &slime_input()).await.unwrap();
        upsert_source_series(
            &pool, &w_md, "mangadex", "mangadex", "uuid-abc", None, false,
        )
        .await
        .unwrap();
        insert_suwayomi_series(&pool, 200, Some("es")).await;
        upsert_source_series(&pool, &w_md, "suwayomi", "src", "200", None, false)
            .await
            .unwrap();

        // Non-English series (id 201) STANDALONE — its work must be cascade-deleted.
        let w_orphan = create_work(&pool, &slime_input()).await.unwrap();
        insert_suwayomi_series(&pool, 201, Some("es")).await;
        upsert_source_series(&pool, &w_orphan, "suwayomi", "src", "201", None, false)
            .await
            .unwrap();

        // Satellite rows keyed by the numeric id for the non-English series (id 201),
        // plus an English chapter (manga 100) that must survive.
        sqlx::query("INSERT INTO suwayomi_chapter (id, manga_id, name, chapter_number, updated_at) VALUES (1, 201, 'c', 1.0, 'now'), (2, 100, 'c', 1.0, 'now')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('u1','u1','u1@x','h','now')")
            .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO user_library (user_id, series_id, created_at) VALUES ('u1', '201', 'now')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO series_views (series_key, total, updated_at) VALUES ('201', 3, 'now')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO series_scan_state (series_id, updated_at) VALUES ('200','now'),('201','now'),('100','now')")
            .execute(&pool).await.unwrap();

        // --- purge ---
        let mut purged = purge_foreign_language_suwayomi(&pool).await.unwrap();
        purged.sort();
        assert_eq!(purged, vec![200, 201], "returns the non-English manga ids");

        let count = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        // English survives fully.
        assert_eq!(
            count("SELECT COUNT(*) FROM suwayomi_series WHERE id=100").await,
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM suwayomi_chapter WHERE manga_id=100").await,
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM series_scan_state WHERE series_id='100'").await,
            1
        );
        // Non-English rows gone everywhere.
        assert_eq!(
            count("SELECT COUNT(*) FROM suwayomi_series WHERE lang<>'en'").await,
            0
        );
        assert_eq!(count("SELECT COUNT(*) FROM source_series WHERE source_type='suwayomi' AND source_key IN ('200','201')").await, 0);
        assert_eq!(
            count("SELECT COUNT(*) FROM suwayomi_chapter WHERE manga_id=201").await,
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM user_library WHERE series_id='201'").await,
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM series_views WHERE series_key='201'").await,
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM series_scan_state WHERE series_id IN ('200','201')").await,
            0
        );
        // Consolidated work survives (kept its mangadex link); standalone work deleted.
        assert_eq!(
            count(Box::leak(
                format!("SELECT COUNT(*) FROM work WHERE id='{w_md}'").into_boxed_str()
            ))
            .await,
            1,
            "consolidated work kept"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM source_series WHERE source_type='mangadex'").await,
            1,
            "mangadex link intact"
        );
        assert_eq!(
            count(Box::leak(
                format!("SELECT COUNT(*) FROM work WHERE id='{w_orphan}'").into_boxed_str()
            ))
            .await,
            0,
            "orphan work cascade-deleted"
        );
        assert_eq!(
            count(Box::leak(
                format!("SELECT COUNT(*) FROM work WHERE id='{w_en}'").into_boxed_str()
            ))
            .await,
            1
        );

        // Idempotent: a second pass is a no-op returning nothing. Also exercises the
        // nothing-to-do fast path, which now short-circuits BEFORE taking the write lock.
        assert!(purge_foreign_language_suwayomi(&pool)
            .await
            .unwrap()
            .is_empty());
    }

    /// REGRESSION (BUG 1). Both merge-candidate writers used a plain `INSERT`, so every
    /// re-derivation of a pair appended another row. `RECONCILE_PENDING_WHERE` excludes a
    /// work only while its candidate is *pending*, so the instant an admin REJECTED a
    /// pair the dedup scanner picked the work up again, recomputed the identical fuzzy
    /// match, and wrote a fresh `pending` row — silently reversing the human decision.
    /// Live data (2026-07-26) had 5 duplicate pairs, 4 sitting `pending` on top of a
    /// `rejected` audit row.
    #[tokio::test]
    async fn insert_merge_candidate_never_re_proposes_a_pair_an_admin_resolved() {
        let pool = pool().await;
        let loser = create_work(&pool, &slime_input()).await.unwrap();
        let survivor = create_work(&pool, &slime_input()).await.unwrap();
        let other = create_work(&pool, &slime_input()).await.unwrap();
        insert_suwayomi_series(&pool, 300, Some("en")).await;
        let ssid = upsert_source_series(&pool, &loser, "suwayomi", "src", "300", None, false)
            .await
            .unwrap();

        let first = insert_merge_candidate(&pool, &ssid, &survivor, 0.62, "fuzzy")
            .await
            .unwrap();
        assert!(first.is_some(), "the first enqueue of a pair inserts");

        // Idempotent while still PENDING — a second sweep must not double the queue.
        assert!(
            insert_merge_candidate(&pool, &ssid, &survivor, 0.62, "fuzzy")
                .await
                .unwrap()
                .is_none(),
            "an already-pending pair is suppressed, not duplicated"
        );

        for decision in ["rejected", "confirmed"] {
            sqlx::query("UPDATE merge_candidate SET status = ?, resolved_at = '2026-01-01'")
                .bind(decision)
                .execute(&pool)
                .await
                .unwrap();
            // Three sweeps, and a "better" score — a rejection is about the PAIR, and no
            // similarity number re-opens it.
            for _ in 0..3 {
                assert!(
                    insert_merge_candidate(&pool, &ssid, &survivor, 0.99, "phash")
                        .await
                        .unwrap()
                        .is_none(),
                    "a {decision} pair must never be re-proposed"
                );
            }
            let rows: (i64, String) =
                sqlx::query_as("SELECT COUNT(*), MAX(status) FROM merge_candidate")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                rows,
                (1, decision.to_string()),
                "re-running neither duplicates the row nor reverts it to pending"
            );
        }

        // Suppression is per-PAIR, not per-work: the same source series matched against a
        // DIFFERENT work is a new question and must still reach the admin.
        assert!(
            insert_merge_candidate(&pool, &ssid, &other, 0.62, "fuzzy")
                .await
                .unwrap()
                .is_some(),
            "a rejected pair must not blacklist the source series against every other work"
        );
    }

    /// REGRESSION (TOCTOU). The consolidation gate reads a work's identity OUTSIDE any
    /// transaction and `merge_works` then physically DELETEs the loser. An admin
    /// `updateSeriesMetadata` landing in that window — here, clearing the `year` that was
    /// the pair's sole corroboration — must abort the merge, not destroy a work on
    /// grounds that no longer exist.
    #[tokio::test]
    async fn merge_works_checked_aborts_when_the_snapshot_went_stale() {
        let pool = pool().await;
        let loser = create_work(&pool, &slime_input()).await.unwrap();
        let survivor = create_work(&pool, &slime_input()).await.unwrap();
        // What a gate reading these two works right now would have captured (both are
        // built from `slime_input`, so their identity columns are identical).
        let snapshot = WorkIdentity {
            primary_title: Some("That Time I Got Reincarnated as a Slime".into()),
            year: Some(2015),
            author: None,
            cover_phash: None,
        };
        let expect_loser = snapshot.clone();
        let expect_survivor = snapshot;

        // The admin edit lands between the gate's read and the merge's write lock.
        sqlx::query("UPDATE work SET year = NULL WHERE id = ?")
            .bind(&loser)
            .execute(&pool)
            .await
            .unwrap();

        let err = merge_works_checked(
            &pool,
            None,
            &loser,
            &survivor,
            Some((&expect_loser, &expect_survivor)),
        )
        .await
        .expect_err("a stale snapshot must abort the merge");
        assert!(
            err.to_string().starts_with("merge precondition failed"),
            "the caller distinguishes a stale gate from a real failure by this prefix, got: {err}"
        );
        let alive: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(alive, 2, "an aborted merge must not delete anything");

        // Re-reading the identity (what the next sweep does) lets the merge proceed.
        let fresh_loser = WorkIdentity {
            year: None,
            ..expect_loser.clone()
        };
        merge_works_checked(
            &pool,
            None,
            &loser,
            &survivor,
            Some((&fresh_loser, &expect_survivor)),
        )
        .await
        .expect("a matching snapshot merges normally");
        let alive: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(alive, 1);
    }

    /// `merge_works_ex` (no precondition) is the human-initiated path and must stay
    /// unconditional — an admin who clicked Merge is the authority a precondition would
    /// be protecting.
    #[tokio::test]
    async fn merge_works_ex_still_merges_without_a_precondition() {
        let pool = pool().await;
        let loser = create_work(&pool, &slime_input()).await.unwrap();
        let survivor = create_work(&pool, &slime_input()).await.unwrap();
        sqlx::query("UPDATE work SET primary_title = 'renamed under the caller' WHERE id = ?")
            .bind(&loser)
            .execute(&pool)
            .await
            .unwrap();
        merge_works_ex(&pool, None, &loser, &survivor)
            .await
            .unwrap();
        let alive: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(alive, 1);
    }

    /// BUG 3. The timed re-arm is documented as costing "one wasted walk a week"; a full
    /// `reset_subscription_breaker` makes it cost `SUBSCRIPTION_FAILURE_LIMIT` (5) daily
    /// walks before the breaker can trip again. The probe arm leaves exactly one strike
    /// left, so a still-dead source re-trips on its very next failure — while the ADMIN
    /// re-subscribe path keeps the full reset.
    #[tokio::test]
    async fn breaker_probe_rearm_leaves_one_strike_while_admin_reset_clears_all() {
        let pool = pool().await;
        set_extension_subscription(&pool, "pkg", true)
            .await
            .unwrap();
        for _ in 0..SUBSCRIPTION_FAILURE_LIMIT {
            mark_subscription_synced(&pool, "pkg", 0, Some("502"))
                .await
                .unwrap();
        }
        let disabled: Option<String> = sqlx::query_scalar(
            "SELECT disabled_at FROM extension_subscription WHERE pkg_name='pkg'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(disabled.is_some(), "breaker tripped at the limit");

        // Timed probe re-arm: enabled again, but one failure away from re-tripping.
        rearm_subscription_breaker_probe(&pool, "pkg")
            .await
            .unwrap();
        let (disabled, fails): (Option<String>, i64) = sqlx::query_as(
            "SELECT disabled_at, consecutive_failures FROM extension_subscription WHERE pkg_name='pkg'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            disabled.is_none(),
            "the probe pass re-enables the subscription"
        );
        assert_eq!(fails, SUBSCRIPTION_FAILURE_LIMIT - 1);
        assert!(
            mark_subscription_synced(&pool, "pkg", 0, Some("502"))
                .await
                .unwrap(),
            "ONE more failing walk re-trips the breaker — not {SUBSCRIPTION_FAILURE_LIMIT}"
        );

        // The admin "I've fixed it" reset is the opposite: a clean slate.
        reset_subscription_breaker(&pool, "pkg").await.unwrap();
        let (disabled, fails): (Option<String>, i64) = sqlx::query_as(
            "SELECT disabled_at, consecutive_failures FROM extension_subscription WHERE pkg_name='pkg'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(disabled.is_none());
        assert_eq!(
            fails, 0,
            "an admin re-subscribe must not leave the source one blip from disabled"
        );
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_indexes_aliases_and_externals() {
        let pool = pool().await;
        let a = upsert_work_from_mangadex(&pool, "md-uuid-1", &slime_input())
            .await
            .unwrap();
        // Second upsert of the same MangaDex id reuses the same work.
        let b = upsert_work_from_mangadex(&pool, "md-uuid-1", &slime_input())
            .await
            .unwrap();
        assert_eq!(a, b);

        // External-id lookups resolve (both the mangadex id and the AniList id).
        assert_eq!(
            find_work_by_external(&pool, "mangadex", "md-uuid-1")
                .await
                .unwrap(),
            Some(a.clone())
        );
        assert_eq!(
            find_work_by_external(&pool, "al", "101517").await.unwrap(),
            Some(a.clone())
        );

        // Alias index resolves the romaji alt-title.
        let norm = normalize_title("Tensei Shitara Slime Datta Ken");
        assert_eq!(
            find_works_by_alias(&pool, &norm).await.unwrap(),
            vec![a.clone()]
        );

        // A mangadex source_series was ensured and chapters attach to it.
        let ssid = find_source_series_id(&pool, "mangadex", "mangadex", "md-uuid-1")
            .await
            .unwrap()
            .expect("source_series exists");
        upsert_chapter(
            &pool,
            &ssid,
            &ChapterInput {
                external_id: "ch-1".into(),
                number: Some("1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        upsert_chapter(
            &pool,
            &ssid,
            &ChapterInput {
                external_id: "ch-1".into(),
                number: Some("1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chapter WHERE source_series_id = ?")
                .bind(&ssid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "chapter upsert is idempotent on external id");
    }

    #[tokio::test]
    async fn sync_state_seed_progress_and_completion() {
        let pool = pool().await;
        // Never run → no state → a fresh createdAt seed.
        assert!(get_sync_state(&pool, "catalogue").await.unwrap().is_none());

        // A provisional seed checkpoint keeps seed_done = false so the next cycle
        // resumes the createdAt seed from this cursor (M6).
        set_seed_progress(&pool, "catalogue", "2026-07-11T00:00:00")
            .await
            .unwrap();
        let s = get_sync_state(&pool, "catalogue")
            .await
            .unwrap()
            .expect("state present");
        assert_eq!(s.cursor, "2026-07-11T00:00:00");
        assert!(!s.seed_done, "still seeding after a provisional checkpoint");

        // A later checkpoint advances the cursor, still seeding.
        set_seed_progress(&pool, "catalogue", "2026-07-11T06:00:00")
            .await
            .unwrap();
        assert!(
            !get_sync_state(&pool, "catalogue")
                .await
                .unwrap()
                .unwrap()
                .seed_done
        );

        // Completing the seed flips seed_done and sets the incremental cursor.
        mark_seed_done(&pool, "catalogue", "2026-07-12T00:00:00")
            .await
            .unwrap();
        let s = get_sync_state(&pool, "catalogue").await.unwrap().unwrap();
        assert_eq!(s.cursor, "2026-07-12T00:00:00");
        assert!(s.seed_done);

        // Incremental cursor advances without clearing seed_done.
        set_sync_cursor(&pool, "catalogue", "2026-07-13T00:00:00")
            .await
            .unwrap();
        let s = get_sync_state(&pool, "catalogue").await.unwrap().unwrap();
        assert_eq!(s.cursor, "2026-07-13T00:00:00");
        assert!(s.seed_done, "incremental cursor keeps seed_done");

        // Jobs are independent.
        assert!(get_sync_state(&pool, "chapters").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reset_sync_state_forces_fresh_seed() {
        let pool = pool().await;
        // A completed seed: seed_done latched true (the truncated-seed situation).
        mark_seed_done(&pool, "catalogue", "2026-07-12T00:00:00")
            .await
            .unwrap();
        assert!(
            get_sync_state(&pool, "catalogue")
                .await
                .unwrap()
                .unwrap()
                .seed_done
        );

        // resyncCatalogue clears the row entirely → next cycle sees None → fresh seed.
        reset_sync_state(&pool, "catalogue").await.unwrap();
        assert!(
            get_sync_state(&pool, "catalogue").await.unwrap().is_none(),
            "reset clears the row so the next cycle re-seeds from createdAt=0"
        );
        // Idempotent: resetting an already-absent job is a no-op, not an error.
        reset_sync_state(&pool, "catalogue").await.unwrap();
    }

    fn ch(external_id: &str, number: Option<&str>, lang: Option<&str>) -> CanonicalChapter {
        CanonicalChapter {
            external_id: external_id.into(),
            number: number.map(Into::into),
            volume: None,
            lang: lang.map(Into::into),
            title: None,
            published_at: None,
        }
    }

    fn ch_pub(
        external_id: &str,
        number: Option<&str>,
        lang: Option<&str>,
        published_at: Option<&str>,
    ) -> CanonicalChapter {
        CanonicalChapter {
            published_at: published_at.map(Into::into),
            ..ch(external_id, number, lang)
        }
    }

    #[test]
    fn reader_chapters_english_tiebreak_deterministic() {
        // Two English scanlations of the same number (CR2). The kept representative
        // must be the same regardless of input order: latest `published_at` wins.
        let older = ch_pub("md-a", Some("5"), Some("en"), Some("2020-01-01T00:00:00Z"));
        let newer = ch_pub("md-z", Some("5"), Some("en"), Some("2023-01-01T00:00:00Z"));

        for rows in [
            vec![older.clone(), newer.clone()],
            vec![newer.clone(), older.clone()],
        ] {
            let out = select_reader_chapters(rows);
            assert_eq!(out.len(), 1);
            assert_eq!(
                out[0].external_id, "md-z",
                "latest published_at kept regardless of input order"
            );
        }

        // Equal `published_at` → lowest `external_id` breaks the tie, both orders.
        let a = ch_pub("md-a", Some("5"), Some("en"), Some("2021-01-01T00:00:00Z"));
        let b = ch_pub("md-b", Some("5"), Some("en"), Some("2021-01-01T00:00:00Z"));
        for rows in [vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]] {
            let out = select_reader_chapters(rows);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].external_id, "md-a");
        }
    }

    #[test]
    fn reader_chapters_dedupe_prefer_english_and_order() {
        let rows = vec![
            ch("es-2", Some("2"), Some("es")),
            ch("en-1", Some("1"), Some("en")),
            ch("es-1", Some("1"), Some("es")),
            ch("en-10", Some("10"), Some("en")),
            ch("en-2", Some("2"), Some("en")),
            ch("oneshot", None, Some("en")),
        ];
        let out = select_reader_chapters(rows);
        // One row per number, ascending numerically (10 after 2, not lexically), the
        // number-less oneshot last.
        let ids: Vec<&str> = out.iter().map(|c| c.external_id.as_str()).collect();
        assert_eq!(ids, vec!["en-1", "en-2", "en-10", "oneshot"]);
        // Number 1 resolved to the English row even though a Spanish one was seen first.
        assert_eq!(out[0].lang.as_deref(), Some("en"));
    }

    #[tokio::test]
    async fn canonical_work_and_chapters_round_trip() {
        let pool = pool().await;
        let w = upsert_work_from_mangadex(
            &pool,
            "md-uuid-1",
            &WorkInput {
                cover_file_name: Some("cover.jpg".into()),
                ..slime_input()
            },
        )
        .await
        .unwrap();
        let cw = load_canonical_work(&pool, &w).await.unwrap().unwrap();
        assert_eq!(cw.mangadex_id.as_deref(), Some("md-uuid-1"));
        assert_eq!(cw.cover_file_name.as_deref(), Some("cover.jpg"));
        assert!(cw.alt_titles.iter().any(|t| t.contains("Slime")));

        let ssid = find_source_series_id(&pool, "mangadex", "mangadex", "md-uuid-1")
            .await
            .unwrap()
            .unwrap();
        for (ext, num, lang) in [
            ("c2", "2", "en"),
            ("c1", "1", "en"),
            ("c1es", "1", "es"), // duplicate number in another language → excluded
            ("c3es", "3", "es"), // English-absent number → excluded entirely
        ] {
            upsert_chapter(
                &pool,
                &ssid,
                &ChapterInput {
                    external_id: ext.into(),
                    number: Some(num.into()),
                    lang: Some(lang.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let chs = load_canonical_chapters(&pool, &w).await.unwrap();
        let ids: Vec<&str> = chs.iter().map(|c| c.external_id.as_str()).collect();
        // English-only: the Spanish rows (including the English-absent "3") are dropped.
        assert_eq!(
            ids,
            vec!["c1", "c2"],
            "English-only, deduped by number, ordered"
        );
        // NSFW-owner lookup resolves through the chapter uuid.
        assert_eq!(
            chapter_owner_is_nsfw(&pool, "c1").await.unwrap(),
            Some(false)
        );
        assert_eq!(chapter_owner_is_nsfw(&pool, "nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn latest_english_chapter_at_ignores_non_english_and_missing() {
        let pool = pool().await;
        let w = upsert_work_from_mangadex(&pool, "md-latest", &slime_input())
            .await
            .unwrap();
        // No English chapter yet → None (caller falls back to work metadata time).
        assert_eq!(latest_english_chapter_at(&pool, &w).await.unwrap(), None);

        let ssid = find_source_series_id(&pool, "mangadex", "mangadex", "md-latest")
            .await
            .unwrap()
            .unwrap();
        for (ext, num, lang, pub_at) in [
            ("en1", "1", "en", Some("2026-01-01T00:00:00Z")),
            ("en2", "2", "en", Some("2026-02-01T00:00:00Z")), // newest English
            ("es3", "3", "es", Some("2026-06-01T00:00:00Z")), // newer, but Spanish → ignored
        ] {
            upsert_chapter(
                &pool,
                &ssid,
                &ChapterInput {
                    external_id: ext.into(),
                    number: Some(num.into()),
                    lang: Some(lang.into()),
                    published_at: pub_at.map(Into::into),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        // The Spanish 2026-06 row is ignored; the latest English publish wins.
        assert_eq!(
            latest_english_chapter_at(&pool, &w)
                .await
                .unwrap()
                .as_deref(),
            Some("2026-02-01T00:00:00Z"),
        );
    }

    #[tokio::test]
    async fn source_extension_upsert_is_idempotent_on_source_id() {
        // §2.1: the source_id is the PK, so a re-scan updates the same row in place
        // (e.g. a bumped version_code) rather than inserting a duplicate.
        let pool = pool().await;
        let sid = "1024";
        upsert_source_extension(
            &pool,
            sid,
            &SourceExtensionInput {
                pkg_name: "eu.kanade.tachiyomi.extension.en.mangadex".into(),
                repo_url: "https://example.test/index.min.json".into(),
                apk_name: Some("tachiyomi-en.mangadex-v1.4.60.apk".into()),
                version_code: Some(60),
                lang: Some("en".into()),
                is_nsfw: false,
            },
        )
        .await
        .unwrap();

        // Re-observe the same source with a higher version_code.
        upsert_source_extension(
            &pool,
            sid,
            &SourceExtensionInput {
                pkg_name: "eu.kanade.tachiyomi.extension.en.mangadex".into(),
                repo_url: "https://example.test/index.min.json".into(),
                apk_name: Some("tachiyomi-en.mangadex-v1.4.61.apk".into()),
                version_code: Some(61),
                lang: Some("en".into()),
                is_nsfw: false,
            },
        )
        .await
        .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_extension")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "upsert is idempotent on the source_id PK");
        let version: i64 =
            sqlx::query_scalar("SELECT version_code FROM source_extension WHERE source_id = ?")
                .bind(sid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(version, 61, "the second upsert updates the row in place");
    }

    #[tokio::test]
    async fn metadata_sync_marker_advances_past_descriptionless_works() {
        // H1: a work whose upstream record has NO description still gets
        // `metadata_synced_at` set on upsert (and via mark_metadata_synced for ids
        // MangaDex never returns), so the backfill selector (metadata_synced_at IS
        // NULL) drains instead of looping on it forever.
        let pool = pool().await;

        // A description-less MangaDex work: upsert must still stamp the marker.
        let no_desc = WorkInput {
            primary_title: Some("Description-less Title".into()),
            aliases: vec![Alias {
                raw: "Description-less Title".into(),
                lang: None,
            }],
            ..Default::default()
        };
        let w = upsert_work_from_mangadex(&pool, "md-nodesc", &no_desc)
            .await
            .unwrap();
        // No work_description row (nothing to insert)...
        let descs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_description WHERE work_id = ?")
                .bind(&w)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(descs, 0, "no localized description upstream");
        // ...but the marker IS set, so the backfill selector won't re-pick it.
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM source_series ss JOIN work w ON w.id = ss.work_id \
             WHERE ss.source_type = 'mangadex' AND w.metadata_synced_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pending, 0, "the upsert advanced the backfill cursor");

        // A pre-S2 work with a NULL marker is selectable, then mark_metadata_synced
        // clears it even without MangaDex returning the record.
        sqlx::query("UPDATE work SET metadata_synced_at = NULL WHERE id = ?")
            .bind(&w)
            .execute(&pool)
            .await
            .unwrap();
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM source_series ss JOIN work w ON w.id = ss.work_id \
             WHERE ss.source_type = 'mangadex' AND w.metadata_synced_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pending, 1, "reset marker makes it selectable again");
        mark_metadata_synced(&pool, &["md-nodesc".to_string()])
            .await
            .unwrap();
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM source_series ss JOIN work w ON w.id = ss.work_id \
             WHERE ss.source_type = 'mangadex' AND w.metadata_synced_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            pending, 0,
            "mark_metadata_synced advances a not-returned id"
        );
    }

    #[tokio::test]
    async fn authoritative_mapping_dedupes_same_source_and_aggregation_agrees() {
        // F1: a work with TWO mappings for the same source (the dedup matcher
        // consolidated two distinct manga) — key "366" (0 cached) and key "377"
        // (42 cached) — plus MangaPlus "357" (3). The authoritative mapping per
        // source is the one with the most cached chapters, and aggregation +
        // provenance must reference only those (not the empty 366).
        let pool = pool().await;
        let work = upsert_work_from_mangadex(
            &pool,
            "md-naruto",
            &WorkInput {
                primary_title: Some("Naruto".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // MangaDex-ext source "2499": two mappings (366 empty, 377 full).
        upsert_source_series(&pool, &work, "suwayomi", "2499", "366", None, false)
            .await
            .unwrap();
        upsert_source_series(&pool, &work, "suwayomi", "2499", "377", None, false)
            .await
            .unwrap();
        // MangaPlus source "1998": one mapping (357, 3 chapters).
        upsert_source_series(&pool, &work, "suwayomi", "1998", "357", None, false)
            .await
            .unwrap();
        let now = "2026-01-01T00:00:00Z";
        // Cache 42 chapters under 377, 3 under 357, 0 under 366.
        for (manga, count) in [(377, 42), (357, 3)] {
            for n in 1..=count {
                sqlx::query(
                    "INSERT INTO suwayomi_chapter (id, manga_id, name, chapter_number, page_count, updated_at) \
                     VALUES (?, ?, ?, ?, 0, ?)",
                )
                .bind(manga * 1000 + n)
                .bind(manga)
                .bind(format!("Ch {n}"))
                .bind(n as f64)
                .bind(now)
                .execute(&pool)
                .await
                .unwrap();
            }
        }

        let auth = authoritative_suwayomi_mappings(&pool, &work).await.unwrap();
        // One authoritative per source_id: 377 for 2499, 357 for 1998.
        assert_eq!(auth.len(), 2);
        let by_src: std::collections::HashMap<&str, &str> = auth
            .iter()
            .map(|m| (m.source_id.as_str(), m.source_key.as_str()))
            .collect();
        assert_eq!(
            by_src["2499"], "377",
            "the 42-chapter mapping wins over 366"
        );
        assert_eq!(by_src["1998"], "357");

        // Aggregation only references authoritative ids (never 366).
        let rows = work_source_chapters(&pool, &work).await.unwrap();
        let ids: std::collections::HashSet<&str> = rows
            .iter()
            .filter_map(|r| r.suwayomi_manga_id.as_deref())
            .collect();
        assert!(ids.contains("377") && ids.contains("357"));
        assert!(!ids.contains("366"), "redundant empty mapping is excluded");
        // Aggregate distinct-number count = union(377's 1..42, 357's 1..3) = 42.
        assert_eq!(aggregate_chapter_count(&pool, &work).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn aggregate_chapters_unions_across_sources() {
        // S2: a work whose MangaDex spine has 0 chapters but whose Suwayomi source
        // (asurascans, manga id 333) has chapters must report the aggregate.
        let pool = pool().await;
        let work = upsert_work_from_mangadex(
            &pool,
            "md-solo",
            &WorkInput {
                primary_title: Some("Solo Leveling".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // A Suwayomi source mapping (source_key = the Suwayomi manga id "333").
        upsert_source_series(&pool, &work, "suwayomi", "asura-src", "333", None, false)
            .await
            .unwrap();
        // Cache three asura chapters (as the scanner would), plus a duplicate number
        // from a second (hypothetical) source to prove numbers dedupe.
        upsert_source_series(&pool, &work, "suwayomi", "other-src", "999", None, false)
            .await
            .unwrap();
        let now = "2026-01-01T00:00:00Z";
        for (id, manga, num) in [(1, 333, 1.0), (2, 333, 2.0), (3, 333, 3.0), (4, 999, 1.0)] {
            sqlx::query(
                "INSERT INTO suwayomi_chapter (id, manga_id, name, chapter_number, page_count, updated_at) \
                 VALUES (?, ?, ?, ?, 0, ?)",
            )
            .bind(id)
            .bind(manga)
            .bind(format!("Ch {num}"))
            .bind(num)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Aggregate count = 3 distinct numbers (1,2,3), not 4 rows.
        assert_eq!(aggregate_chapter_count(&pool, &work).await.unwrap(), 3);
        // Raw rows carry per-source availability (number 1 from BOTH sources).
        let rows = work_source_chapters(&pool, &work).await.unwrap();
        let n1: Vec<&str> = rows
            .iter()
            .filter(|r| r.number == 1.0)
            .map(|r| r.suwayomi_manga_id.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(n1.len(), 2, "chapter 1 available from two sources");
        assert!(n1.contains(&"333") && n1.contains(&"999"));
    }

    #[test]
    fn main_chapter_count_groups_by_chapter_number() {
        // The Tsukimichi shape: 117 whole chapters (1..=117), 4 ".5" bonus chapters
        // (which fold into their base chapter), and 30 numbers each duplicated by a
        // second scanlator → 151 source rows. The displayed count must be 117, NOT 121
        // (counting the .5s) and NOT 151 (raw rows).
        let mut nums: Vec<f64> = (1..=117).map(|n| n as f64).collect();
        nums.extend([7.5, 14.5, 21.5, 28.5]);
        for n in 1..=30 {
            nums.push(n as f64); // a duplicate row for chapters 1..30 (second scanlator)
        }
        assert_eq!(nums.len(), 151, "the raw row count is the inflated 151");
        assert_eq!(main_chapter_count(nums), 117);

        // Split-part numbering (chapter 9 = 9.1 + 9.2, chapter 10 = 10.1 + 10.2 …): each
        // real chapter must count once, NOT collapse to the few whole numbers present.
        // Here chapters 1,2, then 3.1/3.2, 4.1/4.2 → 4 chapters (1,2,3,4), not 2.
        assert_eq!(main_chapter_count([1.0, 2.0, 3.1, 3.2, 4.1, 4.2]), 4);

        // A real "Chapter 0" first chapter is counted (webtoon/manhwa episode 0): chapters
        // 0,1,2 → 3, and a 0.5 teaser folds into group 0 (still one chapter there).
        assert_eq!(main_chapter_count([0.0, 0.5, 1.0, 2.0, f64::NAN]), 3);
        assert_eq!(main_chapter_count([0.5, 0.9]), 1);

        // Negatives / non-finite are sentinels, never chapters.
        assert_eq!(main_chapter_count([-1.0, -5.0, 1.0, 2.0]), 2);

        // Fallback: no non-negative numbers at all → raw row count (never zero-out a
        // series that genuinely has chapters).
        assert_eq!(main_chapter_count([f64::NAN, f64::NAN, f64::NAN]), 3);
        assert_eq!(main_chapter_count([-1.0, -2.0]), 2);
    }

    #[tokio::test]
    async fn merge_works_folds_source_into_target() {
        // D1: fold a federation-only duplicate into the MangaDex-anchored work.
        let pool = pool().await;
        // Target: MangaDex-anchored, enriched.
        let target = upsert_work_from_mangadex(
            &pool,
            "md-naruto",
            &WorkInput {
                primary_title: Some("Naruto".into()),
                author: Some("Kishimoto Masashi".into()),
                aliases: vec![Alias {
                    raw: "Naruto".into(),
                    lang: Some("en".into()),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Source: a federation-only work with its own Suwayomi mapping + an alias
        // the target lacks.
        let source = create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Naruto".into()),
                aliases: vec![Alias {
                    raw: "ナルト".into(),
                    lang: Some("ja".into()),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        upsert_source_series(&pool, &source, "suwayomi", "998", "357", None, false)
            .await
            .unwrap();

        let out = merge_works(&pool, &source, &target).await.unwrap();
        assert_eq!(out.moved_source_series, 1, "the Suwayomi mapping moved");

        // Source work is gone; the Suwayomi mapping now points at the target.
        let src_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work WHERE id = ?")
            .bind(&source)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(src_exists, 0, "source work deleted");
        let mapped: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = '357'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mapped, target);
        // The source's ja alias was folded into the target.
        let ja: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_alias WHERE work_id = ? AND raw_title = 'ナルト'",
        )
        .bind(&target)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ja, 1, "source alias folded into target");
        // No orphaned source_series/aliases remain for the deleted work.
        let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_alias WHERE work_id = ?")
            .bind(&source)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(orphans, 0);

        // Merging a work into itself is rejected.
        assert!(merge_works(&pool, &target, &target).await.is_err());
        // A missing work is rejected.
        assert!(merge_works(&pool, "w_nope", &target).await.is_err());
    }

    #[tokio::test]
    async fn merge_works_folds_metadata_instead_of_dropping_it() {
        // The merge used to DELETE the source's descriptions/credits/covers/tags/
        // chapter overrides, and never carried its cover_file_name or NSFW flag — so
        // folding a rich work into a bare survivor destroyed metadata and could
        // un-hide an adult work. Everything below must survive the fold.
        let pool = pool().await;
        let target = create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Survivor".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let source = create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Loser".into()),
                is_nsfw: true,
                cover_file_name: Some("abc.jpg".into()),
                cover_phash: Some("ffff0000ffff0000".into()),
                descriptions: vec![("de".into(), "Beschreibung".into())],
                credits: vec![("author".into(), "Some Author".into())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for (sql, binds) in [
            (
                "INSERT INTO work_cover (work_id, cover_file_name, lang, volume, is_primary) \
                 VALUES (?, 'abc.jpg', 'ja', '1', 1)",
                vec![source.clone()],
            ),
            (
                "INSERT INTO work_tag (work_id, tag, ord) VALUES (?, 'isekai', 0)",
                vec![source.clone()],
            ),
            (
                "INSERT INTO chapter_override (work_id, chapter_key, hidden, updated_at) \
                 VALUES (?, '100', 1, '2026-01-01T00:00:00Z')",
                vec![source.clone()],
            ),
            (
                "INSERT INTO work_cover_issue (work_id, reason, first_seen, last_seen) \
                 VALUES (?, 'too_large', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                vec![source.clone()],
            ),
        ] {
            let mut q = sqlx::query(sql);
            for b in &binds {
                q = q.bind(b);
            }
            q.execute(&pool).await.unwrap();
        }

        merge_works(&pool, &source, &target).await.unwrap();

        async fn count(pool: &SqlitePool, sql: &str, wid: &str) -> i64 {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(wid)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        for (what, sql) in [
            (
                "localized description",
                "SELECT COUNT(*) FROM work_description WHERE work_id = ? AND lang = 'de'",
            ),
            (
                "credit",
                "SELECT COUNT(*) FROM work_credit WHERE work_id = ?",
            ),
            (
                "cover row",
                "SELECT COUNT(*) FROM work_cover WHERE work_id = ?",
            ),
            ("tag", "SELECT COUNT(*) FROM work_tag WHERE work_id = ?"),
            (
                "chapter override",
                "SELECT COUNT(*) FROM chapter_override WHERE work_id = ?",
            ),
        ] {
            assert_eq!(count(&pool, sql, &target).await, 1, "{what} folded");
        }
        // The loser's cover-ISSUE marker is dropped, not repointed — it would exclude
        // the survivor from the cover crawl forever.
        let issues: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_issue")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(issues, 0, "cover issue dropped with the losing work");

        let row: (Option<String>, i64, Option<String>) =
            sqlx::query_as("SELECT cover_file_name, is_nsfw, cover_phash FROM work WHERE id = ?")
                .bind(&target)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0.as_deref(), Some("abc.jpg"), "cover_file_name folded");
        assert_eq!(row.1, 1, "NSFW flag OR'd into the survivor");
        assert_eq!(row.2.as_deref(), Some("ffff0000ffff0000"));

        // The merged-away id keeps resolving.
        let redirect: Option<String> =
            sqlx::query_scalar("SELECT new_id FROM work_redirect WHERE old_id = ?")
                .bind(&source)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(redirect.as_deref(), Some(target.as_str()));
    }

    #[tokio::test]
    async fn merge_works_collapses_redirect_chains_and_respects_the_nsfw_rating_guard() {
        let pool = pool().await;
        // a -> b, then b -> c must leave BOTH a and b pointing straight at c.
        let mut ids = Vec::new();
        for t in ["A", "B", "C"] {
            ids.push(
                create_work(
                    &pool,
                    &WorkInput {
                        primary_title: Some(t.into()),
                        // C is authoritatively rated `safe` by MangaDex and must NOT be
                        // re-flagged NSFW by folding a source-flagged work into it
                        // (the over-flagging migration 0053 cleaned up).
                        content_rating: if t == "C" { Some("safe".into()) } else { None },
                        is_nsfw: t == "B",
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            );
        }
        merge_works(&pool, &ids[0], &ids[1]).await.unwrap();
        merge_works(&pool, &ids[1], &ids[2]).await.unwrap();
        let hops: Vec<(String, String)> =
            sqlx::query_as("SELECT old_id, new_id FROM work_redirect ORDER BY old_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(hops.len(), 2);
        for (_, new_id) in &hops {
            assert_eq!(new_id, &ids[2], "chain collapsed to a single hop");
        }
        let nsfw: i64 = sqlx::query_scalar("SELECT is_nsfw FROM work WHERE id = ?")
            .bind(&ids[2])
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            nsfw, 0,
            "a `safe` content_rating wins over a folded NSFW flag"
        );
    }

    #[tokio::test]
    async fn merge_works_repoints_external_ids_instead_of_destroying_them() {
        // REGRESSION: `work_external_id`'s PRIMARY KEY is (provider, external_id) and
        // does NOT include work_id, so the `INSERT OR IGNORE … SELECT` fold collided with
        // the SOURCE's own row, was silently ignored, and the cleanup delete then wiped
        // the loser's external ids outright. Losing the loser's `mangadex` id is the
        // severe case: `upsert_work_from_mangadex` resolves that uuid through this table,
        // so the next sync would find nothing and mint a fresh duplicate work, undoing
        // the merge.
        let pool = pool().await;
        let target = create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Survivor".into()),
                external_ids: vec![("al".into(), "111".into())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let source = create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Loser".into()),
                external_ids: vec![
                    ("mangadex".into(), "uuid-loser".into()),
                    ("mal".into(), "222".into()),
                ],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        merge_works(&pool, &source, &target).await.unwrap();

        let mut owned: Vec<(String, String)> = sqlx::query_as(
            "SELECT provider, external_id FROM work_external_id WHERE work_id = ? \
             ORDER BY provider",
        )
        .bind(&target)
        .fetch_all(&pool)
        .await
        .unwrap();
        owned.sort();
        assert_eq!(
            owned,
            vec![
                ("al".to_string(), "111".to_string()),
                ("mal".to_string(), "222".to_string()),
                ("mangadex".to_string(), "uuid-loser".to_string()),
            ],
            "the loser's external ids moved to the survivor, keeping the target's own"
        );
        // Nothing was orphaned or dropped on the floor.
        let stray: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_external_id WHERE work_id = ?")
                .bind(&source)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stray, 0);
        // The decisive property: the retired uuid still resolves to the SURVIVOR, so the
        // next MangaDex sync updates it instead of creating a second canonical work.
        let resolved = find_work_by_external(&pool, "mangadex", "uuid-loser")
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some(target.as_str()));
    }

    #[tokio::test]
    async fn merge_works_carries_an_admin_nsfw_override_to_an_unruled_survivor() {
        // REGRESSION: every gate reads the EFFECTIVE flag COALESCE(is_nsfw_override,
        // is_nsfw) and both admin mutations write ONLY the override, so folding just the
        // base column silently un-hid a work an admin had manually marked NSFW while its
        // base flag stayed 0 (two such works exist in production today). Note the target
        // here is rated `safe`, which correctly blocks the BASE fold — the override must
        // still carry, because an override deliberately outranks the MangaDex rating.
        let pool = pool().await;
        let mk = |title: &'static str, rating: Option<&'static str>| {
            let pool = pool.clone();
            async move {
                create_work(
                    &pool,
                    &WorkInput {
                        primary_title: Some(title.into()),
                        content_rating: rating.map(Into::into),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
            }
        };
        let target = mk("Survivor", Some("safe")).await;
        let source = mk("Loser", None).await;
        sqlx::query("UPDATE work SET is_nsfw = 0, is_nsfw_override = 1 WHERE id = ?")
            .bind(&source)
            .execute(&pool)
            .await
            .unwrap();

        merge_works(&pool, &source, &target).await.unwrap();

        let (base, over): (i64, Option<i64>) =
            sqlx::query_as("SELECT is_nsfw, is_nsfw_override FROM work WHERE id = ?")
                .bind(&target)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(base, 0, "the `safe` rating still blocks the BASE fold");
        assert_eq!(
            over,
            Some(1),
            "the admin's manual NSFW mark survives the merge"
        );

        // …but a survivor the admin has ALREADY ruled on keeps its own decision, and a
        // loser's "mark SFW" is never propagated (dropping it leaves the work hidden —
        // the safe failure; propagating it would un-hide the survivor).
        let target2 = mk("Survivor2", None).await;
        let source2 = mk("Loser2", None).await;
        sqlx::query("UPDATE work SET is_nsfw = 1, is_nsfw_override = 0 WHERE id = ?")
            .bind(&source2)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE work SET is_nsfw_override = 1 WHERE id = ?")
            .bind(&target2)
            .execute(&pool)
            .await
            .unwrap();
        merge_works(&pool, &source2, &target2).await.unwrap();
        let over2: Option<i64> =
            sqlx::query_scalar("SELECT is_nsfw_override FROM work WHERE id = ?")
                .bind(&target2)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            over2,
            Some(1),
            "the target's own admin ruling is not undone"
        );
    }

    #[tokio::test]
    async fn merge_works_repoints_activity_and_folds_view_counters() {
        // These tables key series generically (a Suwayomi numeric id OR a `w_` work id)
        // and carry no FK, so nothing cleaned them: the activity feed deep-linked a
        // deleted work (the exact "No such work" dead bookmark migration 0056 exists to
        // end) and `views::trending_keys` kept ranking it onto the Trending row.
        let pool = pool().await;
        let mk = |t: &'static str| {
            let pool = pool.clone();
            async move {
                create_work(
                    &pool,
                    &WorkInput {
                        primary_title: Some(t.into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
            }
        };
        let target = mk("Survivor").await;
        let source = mk("Loser").await;
        sqlx::query("INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('u1','u1','u1@x','h','now')")
            .execute(&pool).await.unwrap();
        for (sql, id) in [
            (
                "INSERT INTO user_activity (id, user_id, kind, target_type, target_id, created_at) \
                 VALUES ('a1','u1','library_add','series',?, 'now')",
                &source,
            ),
            (
                "INSERT INTO notifications (id, user_id, kind, target_type, target_id, created_at) \
                 VALUES ('n1','u1','reply','series',?, 'now')",
                &source,
            ),
        ] {
            sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
        }
        // Both works already have view counters, so the loser's must be SUMMED into the
        // survivor's — a blind repoint would hit the PK and either fail or drop them.
        for (key, total) in [(&source, 10i64), (&target, 4)] {
            sqlx::query(
                "INSERT INTO series_views (series_key, total, updated_at) VALUES (?, ?, 'now')",
            )
            .bind(key)
            .bind(total)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES (?, 100, ?)",
            )
            .bind(key)
            .bind(total)
            .execute(&pool)
            .await
            .unwrap();
        }
        // A bucket the survivor does NOT share must move across intact.
        sqlx::query(
            "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES (?, 101, 7)",
        )
        .bind(&source)
        .execute(&pool)
        .await
        .unwrap();
        // `feed_updates` is the ONE work_id table the merge disposes of through a real
        // `ON DELETE CASCADE` instead of an explicit statement, so its cleanup depends on
        // the connection's `foreign_keys` pragma (set by `db::init`, and on by default in
        // sqlx). Pinned here: if that ever regresses, the updates feed starts rendering a
        // deleted work.
        sqlx::query(
            "INSERT INTO feed_updates (work_id, title, latest_at) VALUES (?, 'Loser', 'now')",
        )
        .bind(&source)
        .execute(&pool)
        .await
        .unwrap();

        merge_works(&pool, &source, &target).await.unwrap();

        let one = |sql: &'static str, id: String| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            one(
                "SELECT COUNT(*) FROM user_activity WHERE target_id = ?",
                target.clone()
            )
            .await,
            1,
            "activity repointed to the survivor"
        );
        assert_eq!(
            one(
                "SELECT COUNT(*) FROM notifications WHERE target_id = ?",
                target.clone()
            )
            .await,
            1,
            "notification repointed to the survivor"
        );
        assert_eq!(
            one(
                "SELECT total FROM series_views WHERE series_key = ?",
                target.clone()
            )
            .await,
            14,
            "all-time view totals summed, not lost or duplicated"
        );
        assert_eq!(
            one(
                "SELECT SUM(views) FROM series_view_bucket WHERE series_key = ?",
                target.clone()
            )
            .await,
            21,
            "shared bucket summed (4+10) and the unshared one (7) moved across"
        );
        // Nothing left pointing at the deleted work.
        for (sql, what) in [
            (
                "SELECT COUNT(*) FROM user_activity WHERE target_id = ?",
                "activity",
            ),
            (
                "SELECT COUNT(*) FROM notifications WHERE target_id = ?",
                "notification",
            ),
            (
                "SELECT COUNT(*) FROM series_views WHERE series_key = ?",
                "views",
            ),
            (
                "SELECT COUNT(*) FROM series_view_bucket WHERE series_key = ?",
                "buckets",
            ),
            (
                "SELECT COUNT(*) FROM feed_updates WHERE work_id = ?",
                "feed row",
            ),
        ] {
            assert_eq!(one(sql, source.clone()).await, 0, "{what} orphaned");
        }
    }

    #[tokio::test]
    async fn merge_works_collapses_a_fan_in_redirect_graph_without_cycles() {
        // Chain collapse must survive MULTIPLE parents, not just a single A->B->C chain:
        // fold A and C into B, then B into D, and all three retired ids must resolve to D
        // in ONE hop (`redirect_work_id` does a single lookup and never walks).
        let pool = pool().await;
        let mut id = std::collections::HashMap::new();
        for t in ["A", "B", "C", "D"] {
            id.insert(
                t,
                create_work(
                    &pool,
                    &WorkInput {
                        primary_title: Some(t.into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            );
        }
        merge_works(&pool, &id["A"], &id["B"]).await.unwrap();
        merge_works(&pool, &id["C"], &id["B"]).await.unwrap();
        merge_works(&pool, &id["B"], &id["D"]).await.unwrap();

        for t in ["A", "B", "C"] {
            assert_eq!(
                redirect_work_id(&pool, &id[t]).await.unwrap().as_deref(),
                Some(id["D"].as_str()),
                "{t} resolves to D in one hop"
            );
        }
        // The invariants the resolver relies on: no id is both a source and a target
        // (that would need a second hop), and no row points at itself (which would hand
        // the resolver back the dead id — the CHECK in migration 0056 makes it
        // impossible, this asserts the merge never even tries).
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT old_id, new_id FROM work_redirect")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 3);
        let olds: std::collections::HashSet<&str> = rows.iter().map(|(o, _)| o.as_str()).collect();
        for (old, new) in &rows {
            assert_ne!(old, new, "no self-redirect");
            assert!(
                !olds.contains(new.as_str()),
                "no target is itself redirected"
            );
        }
        // And a surviving work is never given a redirect of its own.
        assert!(redirect_work_id(&pool, &id["D"]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn search_ranks_the_exact_title_first_despite_punctuation_differences() {
        // The tier compared the user's RAW text to raw titles, so typing punctuation the
        // title spells differently ("dr stone" vs "Dr.STONE") dropped EVERY row to the
        // bottom tier and let a spin-off with an equal chapter count outrank the work
        // itself. Reproduced on production before the fix; the normalized tiers sit BELOW
        // the raw ones so a noise-tail re-release still can't tie with the work itself.
        let pool = pool().await;
        let mk = |title: &'static str, uuid: &'static str| {
            let pool = pool.clone();
            async move {
                let w = create_work(
                    &pool,
                    &WorkInput {
                        primary_title: Some(title.into()),
                        aliases: vec![Alias {
                            raw: title.into(),
                            lang: None,
                        }],
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                // `work_fts` only indexes MangaDex-anchored works.
                upsert_source_series(&pool, &w, "mangadex", "mangadex", uuid, None, false)
                    .await
                    .unwrap();
                w
            }
        };
        let main = mk("Dr.STONE", "u-main").await;
        let spinoff = mk("Dr. STONE reboot: Byakuya", "u-spin").await;
        let colored = mk("Dr.STONE (Official Colored)", "u-col").await;
        // Give the two impostors a HIGHER chapter count than the work itself. `chapters
        // DESC` is the tie-break immediately under the title tier, so without a tier that
        // separates them these outrank the real work — which is exactly the production
        // failure, and what makes this test discriminate rather than pass on bm25 luck.
        for (w, uuid, n) in [(&spinoff, "u-spin", 40), (&colored, "u-col", 30)] {
            let ssid: String = sqlx::query_scalar(
                "SELECT id FROM source_series WHERE work_id = ? AND source_key = ?",
            )
            .bind(w)
            .bind(uuid)
            .fetch_one(&pool)
            .await
            .unwrap();
            for i in 0..n {
                sqlx::query(
                    "INSERT INTO chapter (id, source_series_id, external_id, number, lang, created_at) \
                     VALUES (?, ?, ?, ?, 'en', '2026-01-01T00:00:00Z')",
                )
                .bind(format!("ch_{uuid}_{i}"))
                .bind(&ssid)
                .bind(format!("{uuid}-{i}"))
                .bind(i.to_string())
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        refresh_work_fts(&pool).await.unwrap();

        let pos = |ids: &[String], w: &str| ids.iter().position(|i| i == w);
        let (_, ids) = search_works_fts(&pool, "dr stone", false, 1, 20)
            .await
            .unwrap();
        assert!(
            pos(&ids, &main) < pos(&ids, &spinoff),
            "a DIFFERENT work must not outrank the real one on chapter count alone; \
             before the normalized tiers all three sat in one tier and the 40-chapter \
             spin-off won. got {ids:?}"
        );

        // KNOWN RESIDUAL, asserted so it can't drift silently: a punctuation-mismatched
        // query cannot separate a work from its own noise-tail re-release. `normalize_title`
        // strips "(Official Colored)", so "Dr.STONE (Official Colored)" normalizes to plain
        // "dr stone" and TIES the work itself in the normalized tier, where `chapters DESC`
        // then puts the (longer) re-release first. This is unchanged from before the fix —
        // there both were in the single bottom tier and chapters decided identically — so
        // the tiers are a strict improvement, not a new regression. Fixing it needs a
        // punctuation-folded-but-noise-PRESERVING key that no column stores today.
        assert!(pos(&ids, &colored) < pos(&ids, &main));

        // Typed exactly, the RAW tier — which is why it must stay above the normalized
        // one — keeps the work itself above its re-release.
        let (_, ids) = search_works_fts(&pool, "Dr.STONE", false, 1, 20)
            .await
            .unwrap();
        assert_eq!(ids.first().map(String::as_str), Some(main.as_str()));
        assert!(
            pos(&ids, &main) < pos(&ids, &colored),
            "the re-release never outranks the work itself; got {ids:?}"
        );
    }

    #[tokio::test]
    async fn covers_accumulate_and_load_primary_first() {
        // F2: covers accumulate (a later sweep with just the primary must NOT wipe
        // an enriched full set), and load returns primary first then by volume.
        let pool = pool().await;
        // Enrichment stores the full set.
        let full = WorkInput {
            primary_title: Some("Cover Test".into()),
            covers: vec![
                Cover {
                    file_name: "v2.jpg".into(),
                    lang: Some("ja".into()),
                    volume: Some("2".into()),
                    is_primary: false,
                },
                Cover {
                    file_name: "v1.jpg".into(),
                    lang: Some("ja".into()),
                    volume: Some("1".into()),
                    is_primary: true,
                },
                Cover {
                    file_name: "v10.jpg".into(),
                    lang: Some("ja".into()),
                    volume: Some("10".into()),
                    is_primary: false,
                },
            ],
            ..Default::default()
        };
        let w = upsert_work_from_mangadex(&pool, "md-cov", &full)
            .await
            .unwrap();

        // A later sweep upsert carrying ONLY the primary must not delete v2/v10.
        let sweep = WorkInput {
            primary_title: Some("Cover Test".into()),
            covers: vec![Cover {
                file_name: "v1.jpg".into(),
                lang: Some("ja".into()),
                volume: Some("1".into()),
                is_primary: true,
            }],
            ..Default::default()
        };
        upsert_work_from_mangadex(&pool, "md-cov", &sweep)
            .await
            .unwrap();

        let covers = load_work_covers(&pool, &w).await.unwrap();
        assert_eq!(
            covers.len(),
            3,
            "accumulated set survives a primary-only sweep"
        );
        // Primary first, then volume-ascending numerically (10 after 2).
        assert!(covers[0].is_primary);
        assert_eq!(covers[0].file_name, "v1.jpg");
        let order: Vec<&str> = covers.iter().map(|c| c.file_name.as_str()).collect();
        assert_eq!(order, vec!["v1.jpg", "v2.jpg", "v10.jpg"]);
    }

    #[tokio::test]
    async fn descriptions_and_credits_persist_and_upsert() {
        // S2: multi-lang descriptions + full credit list are stored and a re-sync
        // updates a description in place while credits accumulate without dupes.
        let pool = pool().await;
        let mut input = slime_input();
        input.descriptions = vec![
            ("en".into(), "English blurb.".into()),
            ("ja".into(), "日本語のあらすじ".into()),
        ];
        input.credits = vec![
            ("author".into(), "Fuse".into()),
            ("artist".into(), "Mitz Vah".into()),
        ];
        let w = upsert_work_from_mangadex(&pool, "md-desc", &input)
            .await
            .unwrap();

        let descs: Vec<(String, String)> = sqlx::query_as(
            "SELECT lang, description FROM work_description WHERE work_id = ? ORDER BY lang",
        )
        .bind(&w)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            descs,
            vec![
                ("en".to_string(), "English blurb.".to_string()),
                ("ja".to_string(), "日本語のあらすじ".to_string()),
            ]
        );
        let credits: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, name FROM work_credit WHERE work_id = ? ORDER BY role, name",
        )
        .bind(&w)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            credits,
            vec![
                ("artist".to_string(), "Mitz Vah".to_string()),
                ("author".to_string(), "Fuse".to_string()),
            ]
        );

        // Re-sync: an updated English description overwrites in place; the same
        // credits don't duplicate (composite PK / INSERT OR IGNORE).
        input.descriptions = vec![("en".into(), "Revised English blurb.".into())];
        upsert_work_from_mangadex(&pool, "md-desc", &input)
            .await
            .unwrap();
        let en: String = sqlx::query_scalar(
            "SELECT description FROM work_description WHERE work_id = ? AND lang = 'en'",
        )
        .bind(&w)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(en, "Revised English blurb.");
        let credit_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_credit WHERE work_id = ?")
                .bind(&w)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(credit_count, 2, "credits don't duplicate on re-sync");
    }

    #[tokio::test]
    async fn match_data_and_token_blocking() {
        let pool = pool().await;
        let w = upsert_work_from_mangadex(&pool, "md-uuid-1", &slime_input())
            .await
            .unwrap();
        let md = load_match_data(&pool, &w).await.unwrap().unwrap();
        assert!(md.aliases_norm.iter().any(|a| a.contains("slime")));
        let ids = candidate_work_ids_by_token(&pool, "slime", 10)
            .await
            .unwrap();
        assert_eq!(ids, vec![w]);
    }

    #[tokio::test]
    async fn batch_match_data_equals_per_item() {
        // OPT-7: load_match_data_batch must return exactly what N per-item
        // load_match_data calls would, so the dedup scorer is unaffected.
        let pool = pool().await;
        let a = upsert_work_from_mangadex(&pool, "md-uuid-1", &slime_input())
            .await
            .unwrap();
        let mut second = slime_input();
        second.primary_title = Some("A Painter Who Draws Dungeons".into());
        second.aliases = vec![Alias {
            raw: "A Painter Who Draws Dungeons".into(),
            lang: Some("en".into()),
        }];
        second.external_ids = vec![("al".into(), "999".into())];
        let b = upsert_work_from_mangadex(&pool, "md-uuid-2", &second)
            .await
            .unwrap();

        // Include a bogus id to confirm it's simply absent (like None per-item).
        let ids = vec![a.clone(), b.clone(), "w_does_not_exist".into()];
        let batch = load_match_data_batch(&pool, &ids).await.unwrap();
        assert_eq!(batch.len(), 2, "missing ids are absent, not errors");
        assert!(!batch.contains_key("w_does_not_exist"));

        for id in [&a, &b] {
            let per = load_match_data(&pool, id).await.unwrap().unwrap();
            let bat = batch.get(id).expect("present in batch");
            // aliases_norm order isn't guaranteed by either query; compare as sets.
            let mut per_al = per.aliases_norm.clone();
            let mut bat_al = bat.aliases_norm.clone();
            per_al.sort();
            bat_al.sort();
            assert_eq!(per_al, bat_al, "aliases match for {id}");
            assert_eq!(per.primary_title, bat.primary_title);
            assert_eq!(per.description, bat.description);
            assert_eq!(per.author, bat.author);
            assert_eq!(per.year, bat.year);
            assert_eq!(per.original_language, bat.original_language);
            assert_eq!(per.cover_phash, bat.cover_phash);
        }

        // Empty input is a no-op, not a malformed `IN ()`.
        assert!(load_match_data_batch(&pool, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn token_index_backs_block_and_survives_merge_and_delete() {
        // H9: the exact-token block reads `work_alias_token`; folding/deleting a work
        // must fold/clean its token rows (no orphans, no lost recall).
        let pool = pool().await;
        let target = upsert_work_from_mangadex(
            &pool,
            "md-target",
            &WorkInput {
                primary_title: Some("Solo Leveling".into()),
                aliases: vec![Alias {
                    raw: "Solo Leveling".into(),
                    lang: Some("en".into()),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // A distinctive alias word ("npodamun") only the source carries.
        let source = create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Na Honjaman Level Up".into()),
                aliases: vec![Alias {
                    raw: "Nodamun Level".into(),
                    lang: Some("en".into()),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Exact-token block finds each work by a whole normalized word.
        assert_eq!(
            candidate_work_ids_by_token(&pool, "leveling", 10)
                .await
                .unwrap(),
            vec![target.clone()]
        );
        assert_eq!(
            candidate_work_ids_by_token(&pool, "nodamun", 10)
                .await
                .unwrap(),
            vec![source.clone()]
        );
        // Too-short tokens return empty.
        assert!(candidate_work_ids_by_token(&pool, "a", 10)
            .await
            .unwrap()
            .is_empty());

        // Merge folds the source's tokens into the target and leaves no orphans.
        merge_works(&pool, &source, &target).await.unwrap();
        assert_eq!(
            candidate_work_ids_by_token(&pool, "nodamun", 10)
                .await
                .unwrap(),
            vec![target.clone()],
            "source's distinctive token now blocks to the target"
        );
        let orphan: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_alias_token WHERE work_id = ?")
                .bind(&source)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(orphan, 0, "no token rows left for the deleted source work");

        // Deleting the target cascades its token rows away entirely.
        delete_work_cascade(&pool, &target).await.unwrap();
        assert!(candidate_work_ids_by_token(&pool, "leveling", 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn source_sync_due_throttles_against_recent_pass() {
        // audit #3: a pass is due when none has run; after one is stamped it's NOT due
        // again within the interval (so a restart's immediate tick is skipped), but IS
        // due once the interval has elapsed (simulated with a 0s interval).
        let pool = pool().await;
        assert!(source_sync_due(&pool, 86_400).await, "due when never run");
        mark_source_sync_pass(&pool).await.unwrap();
        assert!(
            !source_sync_due(&pool, 86_400).await,
            "not due right after a pass (skip the restart tick)"
        );
        assert!(
            source_sync_due(&pool, 0).await,
            "due again once the interval has elapsed"
        );
    }

    #[tokio::test]
    async fn source_sync_due_runs_scheduled_tick_despite_pass_duration() {
        // Regression (verify pass): a scheduled tick fires one interval after the previous
        // tick START, but the pass is stamped at COMPLETION (T later). With interval=1000s,
        // a pass that completed 960s ago (a ~40s pass in the prior window) must be DUE — a
        // full-interval threshold would see 960 < 1000 and wrongly skip, halving the cadence.
        let pool = pool().await;
        let ts = (Utc::now() - chrono::Duration::seconds(960)).to_rfc3339();
        sqlx::query("INSERT INTO sync_state (id, last_full_pass_at) VALUES (1, ?)")
            .bind(&ts)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            source_sync_due(&pool, 1000).await,
            "scheduled tick must run despite pass duration (90% threshold)"
        );
    }

    #[tokio::test]
    async fn existing_source_keys_returns_only_enrolled_suwayomi_keys() {
        // audit #10: the batched lookup returns exactly the enrolled subset for the given
        // source_type, so the LATEST walk's "new" set is the complement.
        let pool = pool().await;
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (key, ty) in [
            ("100", "suwayomi"),
            ("200", "suwayomi"),
            ("300", "mangadex"),
        ] {
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
                 VALUES (?, 'w', ?, ?, '2024-01-01T00:00:00Z')",
            )
            .bind(format!("ss_{ty}_{key}"))
            .bind(ty)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        }
        let got = existing_source_keys(
            &pool,
            "suwayomi",
            &["100".into(), "300".into(), "999".into()],
        )
        .await
        .unwrap();
        // 100 is enrolled suwayomi; 300 is mangadex (wrong type); 999 is unknown.
        assert_eq!(got, std::collections::HashSet::from(["100".to_string()]));
        // Empty input short-circuits to an empty set (no query).
        assert!(existing_source_keys(&pool, "suwayomi", &[])
            .await
            .unwrap()
            .is_empty());
    }
}
