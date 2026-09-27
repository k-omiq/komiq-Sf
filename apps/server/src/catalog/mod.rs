//! Canonical catalogue repository (CATALOGUE.md §3).
//!
//! The persistence layer over `work` / `work_alias` / `work_external_id` /
//! `source_series` / `chapter` / `merge_candidate`. Pure sqlx — no network. The
//! MangaDex sync (`crate::mangadex`) writes through `upsert_work_from_mangadex`;
//! the dedup matcher (`crate::dedup`) reads through the `find_*` / `load_match_data`
//! queries. Runtime queries only (matching the rest of the crate), so the build
//! needs no sqlx offline metadata.

pub mod ledger;
pub mod normalize;
pub mod pair;
pub mod similarity;
pub mod spine;

use crate::db::Database;
use anyhow::Result;
use chrono::Utc;
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

/// A named upstream taxonomy tag. IDs are optional for legacy API records.
#[derive(Debug, Clone)]
pub struct SourceTag {
    pub id: Option<String>,
    pub name: String,
    pub group: String,
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
    /// `(provider, value)` publisher and store links — e.g. `("engtl", "https://…")`.
    /// Shown beside the trackers, but never written to `work_external_id`: they are
    /// not identities.
    pub official_links: Vec<(String, String)>,
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
    pub tag_details: Vec<SourceTag>,
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
    /// MangaDex `publishAt` — SCHEDULING metadata. A 2037 sentinel on external chapters,
    /// and it can post-date [`Self::readable_at`] by weeks. Never a release clock on its
    /// own; see migration 0073.
    pub published_at: Option<String>,
    /// MangaDex `readableAt` — when the chapter actually became readable. The clock every
    /// feed and ordering query here should prefer.
    pub readable_at: Option<String>,
    /// Set when the chapter is hosted off-site and has no pages to serve, so the reader
    /// must redirect out instead of rendering a blank page. `externalUrl IS NOT NULL` is
    /// the only valid test — `pages == 0` is not.
    pub external_url: Option<String>,
    /// Who translated it. Suwayomi carries this per chapter; the MangaDex mirror leaves it
    /// NULL because MangaDex models translation groups as a relationship rather than a
    /// string. Present so the unified spine query can keep returning what
    /// [`work_source_chapters`] already returns for the Suwayomi half.
    pub scanlator: Option<String>,
}

/// The fields the matcher needs to corroborate a candidate work, and everything
/// [`pair::assess`] reads to decide whether two works are one series.
#[derive(Debug, Clone, Default)]
pub struct WorkMatchData {
    pub work_id: String,
    pub primary_title: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub year: Option<i64>,
    pub original_language: Option<String>,
    pub cover_phash: Option<String>,
    /// Does the work carry a MangaDex source? Two MangaDex-anchored works are two
    /// distinct entries of the spine. Derived from `source_series`, not a `work` column.
    pub mangadex_anchored: bool,
    /// Normalized alias keys for this work (includes the primary title).
    pub aliases_norm: Vec<String>,
}

impl WorkMatchData {
    /// The snapshot a merge decision was made from, in the shape `merge_works_checked`
    /// re-validates inside its transaction. `key` is the duplicate key the decision
    /// rested on, when it rested on one.
    pub fn identity(&self, key: Option<&str>) -> WorkIdentity {
        WorkIdentity {
            primary_title: self.primary_title.clone(),
            year: self.year,
            author: self.author.clone(),
            artist: self.artist.clone(),
            original_language: self.original_language.clone(),
            description: self.description.clone(),
            cover_phash: self.cover_phash.clone(),
            mangadex_anchored: self.mangadex_anchored,
            match_key: key.map(str::to_string),
        }
    }
}

/// The select list both match-data loaders use (the table must be aliased `w`).
const MATCH_DATA_COLUMNS: &str = "w.id, w.primary_title, w.description, w.author, w.artist, \
     w.year, w.original_language, w.cover_phash, \
     EXISTS (SELECT 1 FROM source_series ss \
             WHERE ss.work_id = w.id AND ss.source_type = 'mangadex') AS mangadex_anchored";

#[derive(sqlx::FromRow)]
struct MatchDataRow {
    id: String,
    primary_title: Option<String>,
    description: Option<String>,
    author: Option<String>,
    artist: Option<String>,
    year: Option<i64>,
    original_language: Option<String>,
    cover_phash: Option<String>,
    mangadex_anchored: bool,
}

impl MatchDataRow {
    fn into_match_data(self) -> WorkMatchData {
        WorkMatchData {
            work_id: self.id,
            primary_title: self.primary_title,
            description: self.description,
            author: self.author,
            artist: self.artist,
            year: self.year,
            original_language: self.original_language,
            cover_phash: self.cover_phash,
            mangadex_anchored: self.mangadex_anchored,
            aliases_norm: Vec::new(),
        }
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple())
}

/// Find the work already claiming `(provider, external_id)`, if any. The
/// highest-precision match key (CATALOGUE.md §4, step 1).
pub async fn find_work_by_external(
    pool: &Database,
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
pub async fn find_works_by_alias(pool: &Database, normalized: &str) -> Result<Vec<String>> {
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

/// Distinct work ids with an alias whose article-insensitive key
/// (`normalize::match_key`) is `key`, so "The X" finds "X". Weaker than
/// [`find_works_by_alias`]: callers treat a hit as a LOOSE title match and use it only
/// when the exact lookup found nothing.
pub async fn find_works_by_match_key(pool: &Database, key: &str) -> Result<Vec<String>> {
    if key.is_empty() {
        return Ok(Vec::new());
    }
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT work_id FROM work_alias WHERE match_key = ?",
    )
    .bind(key)
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
    pool: &Database,
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
pub async fn load_match_data(pool: &Database, work_id: &str) -> Result<Option<WorkMatchData>> {
    let row = sqlx::query_as::<_, MatchDataRow>(&format!(
        "SELECT {MATCH_DATA_COLUMNS} FROM work w WHERE w.id = ?"
    ))
    .bind(work_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let mut data = row.into_match_data();
    data.aliases_norm = sqlx::query_scalar::<_, String>(
        "SELECT normalized_title FROM work_alias WHERE work_id = ?",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    Ok(Some(data))
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
    pool: &Database,
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

    let work_sql =
        format!("SELECT {MATCH_DATA_COLUMNS} FROM work w WHERE w.id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, MatchDataRow>(&work_sql);
    for id in &ids {
        q = q.bind(*id);
    }
    for row in q.fetch_all(pool).await? {
        out.insert(row.id.clone(), row.into_match_data());
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
    /// The chapter's RELEASE time — `COALESCE(readable_at, published_at)`, not raw
    /// `published_at`. Keeps the field name the callers already use while the query
    /// underneath switched clocks (migration 0073).
    pub published_at: Option<String>,
    /// Set when the chapter is hosted off-site (MangaPlus, Comikey, NamiComi, BiliBili)
    /// and has no pages for us to serve — the reader must send the user there instead of
    /// rendering an empty page. ~35,000 mirrored chapters, 4% of the mirror.
    pub external_url: Option<String>,
    /// The scanlation group(s) of THIS copy — `"No Group"` when MangaDex lists none, NULL
    /// when unknown (not backfilled yet, or the backfill's `''` "asked, could not learn",
    /// which the query folds to NULL so no surface ever prints an empty label).
    pub scanlator: Option<String>,
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
/// A page of feed items should use [`work_effective_genres_batch_checked`] instead: this issues
/// up to three round-trips per work.
pub async fn work_effective_genres(pool: &Database, work_id: &str) -> Vec<String> {
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
/// Fallible batch lookup for request paths: a database failure must not produce a
/// partial genre map that the caller can memoize as a successful response.
pub async fn work_effective_genres_batch_checked(
    pool: &Database,
    work_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<String>>> {
    let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
    if work_ids.is_empty() {
        return Ok(out);
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
    for (wid, tag) in q.fetch_all(pool).await? {
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
        for (wid, tag) in qu.fetch_all(pool).await? {
            out.entry(wid).or_default().push(tag);
        }
    }

    // 3) Source genres for everything neither tag half covered. Same
    //    cast-the-non-indexed-side join as the single-work path above.
    let remaining: Vec<&String> = work_ids.iter().filter(|w| !out.contains_key(*w)).collect();
    if remaining.is_empty() {
        return Ok(out);
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
    for (wid, json) in q2.fetch_all(pool).await? {
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
    Ok(out)
}

/// Load a canonical work with its MangaDex anchor + cover fileName + alt titles, for
/// the reader's canonical series path. `None` if the work id is unknown.
pub async fn load_canonical_work(pool: &Database, work_id: &str) -> Result<Option<CanonicalWork>> {
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
    pool: &Database,
    work_id: &str,
) -> Result<Vec<CanonicalChapter>> {
    let rows = sqlx::query_as::<_, CanonicalChapter>(
        // RELEASED_AT, not `published_at` (migration 0073). `publishAt` is scheduling
        // metadata: MangaDex stamps external chapters 2037-12-31 and it can post-date the
        // real readable time by weeks, so ordering by it alone puts unreleased-looking
        // chapters at the top of a series' list and buries readable ones.
        "SELECT c.external_id, c.number, c.volume, c.lang, c.title, \
                COALESCE(c.readable_at, c.published_at) AS published_at, c.external_url, \
                NULLIF(c.scanlator, '') AS scanlator \
         FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
         WHERE ss.work_id = ? AND ss.source_type = 'mangadex' AND c.lang = 'en' \
         ORDER BY COALESCE(c.readable_at, c.published_at) DESC, c.external_id ASC",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    Ok(select_reader_chapters(rows))
}

/// The publish time of the work's most recent **English** chapter, ACROSS EVERY SOURCE —
/// the real "last updated" for a series' details page and the sort key behind the reader's
/// Library "recently updated" order. `None` when the work has no English chapter mirrored
/// anywhere.
///
/// This is deliberately NOT `work.updated_at`: that column is bumped on every routine
/// metadata re-sync (`Utc::now()` in `upsert_work`), so it reflects "last metadata
/// touch", not "last new chapter" — a work synced 4h ago but last updated 32 days ago
/// would wrongly read "4 hours ago". `published_at` falls back to `created_at` per row
/// only when a chapter has no publish date (mirrors `canonical_updates`).
///
/// IT USED TO CARRY `AND ss.source_type = 'mangadex'`, AND THAT WAS BUG (4). A work whose
/// new chapter arrived from a scanlator source rather than from the MangaDex mirror could
/// not move this value at all, and a work with no MangaDex-English chapter returned `None`
/// — sending the caller to the `work.updated_at` fallback the paragraph above forbids.
/// Measured on the 2026-08-11 production snapshot: of the 39 distinct canonical works in
/// `user_library`, **29 had no MangaDex-English chapter at all** and 8 more had a
/// Suwayomi source strictly ahead of the mirror, so 37 of 39 Library rows sorted on a
/// timestamp that was not their newest chapter. One example: `w_09515376…` had a chapter
/// released 2026-08-11 and reported 2026-07-31; `w_29b2e195…` had one released 2026-08-08
/// and reported 2025-08-28.
///
/// `c.lang = 'en'` STAYS, and it is not the same predicate. The MangaDex mirror really is
/// multi-language and a Spanish chapter must not set an English reader's recency; the
/// Suwayomi half is English-by-construction and stamps `lang = 'en'` on every spine row it
/// writes (`suwayomi_spine_input`), so keeping the language filter admits it in full.
pub async fn latest_english_chapter_at(pool: &Database, work_id: &str) -> Result<Option<String>> {
    let at = sqlx::query_scalar::<_, Option<String>>(
        // `readable_at` first (migration 0073): a work whose newest chapter is external
        // carries a 2037 `publish_at`, which would otherwise report a "last updated" a
        // decade in the future.
        "SELECT MAX(COALESCE(c.readable_at, c.published_at, c.created_at)) \
         FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
         WHERE ss.work_id = ? AND c.lang = 'en'",
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
/// Aggregate into private TEMP tables on a read-only-main connection, then publish fully
/// derived rows in bounded transactions. `now` excludes future scheduled chapters.
/// Rows changed by concurrent incremental writers are preserved for the next reconcile.
///
/// `is_nsfw` materializes the EFFECTIVE flag `COALESCE(is_nsfw_override, is_nsfw)`, not
/// the raw column: both admin "mark NSFW"/"mark SFW" mutations write only the override,
/// so copying `w.is_nsfw` made the feed the one surface that ignored every manual
/// reclassification until the next full sync.
///
/// ALSO refreshes [`refresh_feed_series_updates`] (the reader's merged Updates feed,
/// migration 0064), which is derived from this table plus the scanner's half — and that in
/// turn refreshes [`refresh_browse_catalogue`] (Browse's table, migration 0069), which is
/// derived from IT, and finally [`refresh_work_fts`] (search's index, 0052/0071), which
/// must cover the same works Browse does. Folded into one chain rather than wired
/// separately so every existing call site — boot and each post-sync pass — covers all four,
/// and they can never drift apart in freshness. The return value stays the `feed_updates`
/// row count.
/// The `feed_updates` columns [`feed_updates_select`] produces, in its order.
const FEED_UPDATES_COLUMNS: &str = "work_id, mangadex_id, title, is_nsfw, cover_url, \
     latest_chapter, latest_chapter_title, latest_at";

/// The SELECT behind `feed_updates` (migration 0051) — one row per MangaDex-anchored work
/// with a RELEASED English chapter, already grouped.
///
/// A function, and every output column ALIASED, because two callers share it and one of them
/// wraps it as a derived table: [`refresh_feed_updates`] (wholesale, `scope = ""`) and
/// [`publish_mirror_feed_row`] (one work, `scope = "AND ss.work_id = ?"`). Sharing the text
/// is what makes the incremental mirror writer converge with the rebuild by CONSTRUCTION
/// rather than by a field-mapping agreement that has to be re-checked by hand — the same
/// arrangement [`browse_catalogue_select`] has with its two writers.
///
/// The single `MAX()` alongside bare `c.number` / `c.title` is SQLite's documented
/// bare-columns-in-an-aggregate rule: both labels come from the row that produced the newest
/// release time, never smeared across chapters. Do not add a second aggregate here.
///
/// `?` is bound FIRST and is the release-time ceiling ("now"), which bounds out MangaDex's
/// far-future scheduled `publishAt` values. A `scope` that binds anything binds it after.
fn feed_updates_select(scope: &str) -> String {
    format!(
        "SELECT ss.work_id AS work_id, ss.source_key AS mangadex_id, \
                w.primary_title AS title, \
                COALESCE(w.is_nsfw_override, w.is_nsfw) AS is_nsfw, \
                CASE WHEN w.cover_cached_version IS NOT NULL \
                     THEN '/covers/' || w.id || '.webp?v=' || w.cover_cached_version \
                     WHEN w.cover_file_name IS NOT NULL \
                     THEN '/covers/' || w.id || '.webp' \
                     ELSE NULL END AS cover_url, \
                c.number AS latest_chapter, c.title AS latest_chapter_title, \
                MAX(COALESCE(c.readable_at, c.published_at, c.created_at)) AS latest_at \
         FROM chapter c \
         JOIN source_series ss ON ss.id = c.source_series_id \
         JOIN work w ON w.id = ss.work_id \
         WHERE ss.source_type = 'mangadex' AND c.lang = 'en' \
           AND COALESCE(c.readable_at, c.published_at, c.created_at) <= ? {scope} \
         GROUP BY ss.work_id"
    )
}

async fn repair_catalogue_tag_keys(pool: &Database) -> Result<u64> {
    let mut cursor = 0i64;
    let mut changed = 0;
    loop {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT rowid, tag FROM work_tag WHERE rowid > ? \
             AND (tag_key IS NULL OR tag_group IS NULL) ORDER BY rowid LIMIT 128",
        )
        .bind(cursor)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            return Ok(changed);
        }
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        for (rowid, name) in rows {
            cursor = rowid;
            let group = crate::tags::known_tag(&name).map(|(_, group)| group);
            changed += sqlx::query(
                "UPDATE work_tag SET tag_key = COALESCE(tag_key, ?), \
                tag_group = COALESCE(tag_group, ?) WHERE rowid = ? AND tag = ? \
                AND (tag_key IS NULL OR (tag_group IS NULL AND ? IS NOT NULL))",
            )
            .bind(crate::tags::slug(&name))
            .bind(group)
            .bind(rowid)
            .bind(name)
            .bind(group)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        tokio::task::yield_now().await;
    }
}

/// Keep expensive catalogue SELECTs off the main database's writer. The connection opens
/// the main file read-only; only its private TEMP tables can be modified. Each staged row
/// is complete (including type and count facets) before a bounded publication transaction.
struct RefreshSnapshot(sqlx::SqliteConnection);

impl std::ops::Deref for RefreshSnapshot {
    type Target = sqlx::SqliteConnection;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for RefreshSnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

const REFRESH_BATCH_ROWS: i64 = 128;

async fn refresh_snapshot(pool: &Database, tables: &[&str]) -> Result<RefreshSnapshot> {
    use sqlx::Connection;
    let options = (*pool.connect_options())
        .clone()
        .read_only(true)
        .foreign_keys(false)
        .pragma("query_only", "OFF");
    let mut conn = sqlx::SqliteConnection::connect_with(&options).await?;
    for table in tables {
        let schema: String = sqlx::query_scalar(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(&mut conn)
        .await?;
        let create = if *table == "work_fts" {
            // This shadow is never searched with MATCH. A regular keyed table avoids
            // repeated full scans when publishing/pruning FTS's UNINDEXED work_id.
            "CREATE TEMP TABLE work_fts (work_id TEXT PRIMARY KEY, chapters INTEGER, title TEXT, aliases TEXT)".to_owned()
        } else {
            let (_, definition) = schema
                .split_once('(')
                .ok_or_else(|| anyhow::anyhow!("invalid catalogue table schema: {table}"))?;
            format!("CREATE TEMP TABLE {table} ({definition}")
        };
        // Save the old version so a scanner/admin write during staging wins over stale
        // snapshot data. Publication and pruning both compare against this version.
        sqlx::query(&format!(
            "CREATE TEMP TABLE previous_{table} AS \
            SELECT rowid AS snapshot_rowid, * FROM main.{table}"
        ))
        .execute(&mut conn)
        .await?;
        let key = match *table {
            "tag" => "id",
            "feed_genre_facet" => "tag",
            _ => "work_id",
        };
        sqlx::query(&format!(
            "CREATE INDEX temp.previous_{table}_key \
            ON previous_{table}({key})"
        ))
        .execute(&mut conn)
        .await?;
        sqlx::query(&create).execute(&mut conn).await?;
    }
    Ok(RefreshSnapshot(conn))
}

#[derive(Debug, PartialEq)]
enum RefreshValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

fn refresh_values(
    row: &sqlx::sqlite::SqliteRow,
    start: usize,
    count: usize,
) -> Result<Vec<RefreshValue>> {
    use sqlx::{Row, TypeInfo, ValueRef};
    (start..start + count)
        .map(|i| {
            let value = row.try_get_raw(i)?;
            Ok(if value.is_null() {
                RefreshValue::Null
            } else {
                match value.type_info().name() {
                    "INTEGER" | "BOOLEAN" => RefreshValue::Integer(row.try_get(i)?),
                    "REAL" | "NUMERIC" => RefreshValue::Real(row.try_get(i)?),
                    "BLOB" => RefreshValue::Blob(row.try_get(i)?),
                    _ => RefreshValue::Text(row.try_get(i)?),
                }
            })
        })
        .collect()
}

fn bind_refresh_values<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    values: &'q [RefreshValue],
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    for value in values {
        query = match value {
            RefreshValue::Null => query.bind(Option::<String>::None),
            RefreshValue::Integer(v) => query.bind(*v),
            RefreshValue::Real(v) => query.bind(*v),
            RefreshValue::Text(v) => query.bind(v),
            RefreshValue::Blob(v) => query.bind(v),
        };
    }
    query
}

/// Publish in short FIFO writer turns. A failed batch leaves earlier complete rows usable;
/// a subsequent reconcile converges. Never replace a row changed since staging began.
async fn publish_snapshot(
    pool: &Database,
    snapshot: &mut RefreshSnapshot,
    table: &str,
) -> Result<u64> {
    use sqlx::Row;
    let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&format!("PRAGMA temp.table_info({table})"))
            .fetch_all(&mut **snapshot)
            .await?;
    let names: Vec<&str> = columns.iter().map(|c| c.1.as_str()).collect();
    let key = match table {
        "tag" => "id",
        "feed_genre_facet" => "tag",
        _ => "work_id",
    };
    let key_index = names.iter().position(|name| *name == key).unwrap();
    let column_list = names.join(", ");
    let placeholders = vec!["?"; names.len()].join(", ");
    let same_previous = names
        .iter()
        .map(|c| format!("{c} IS ?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let mutable = names
        .iter()
        .filter(|c| **c != key)
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let staged_names = names
        .iter()
        .map(|c| format!("s.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let previous_names = names
        .iter()
        .map(|c| format!("p.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let is_fts = table == "work_fts";
    let upsert = format!(
        "INSERT INTO main.{table} ({column_list}) SELECT {placeholders} \
        WHERE (? = 0 OR EXISTS (SELECT 1 FROM main.{table} WHERE {key} = ?)) {} \
        ON CONFLICT({key}) DO UPDATE SET {mutable} WHERE ? AND {same_previous}",
        if matches!(
            table,
            "feed_updates" | "feed_series_updates" | "browse_catalogue"
        ) {
            "AND EXISTS (SELECT 1 FROM main.work WHERE id = ?)"
        } else {
            ""
        }
    );
    let select = format!(
        "SELECT {staged_names}, p.snapshot_rowid, {previous_names} \
        FROM temp.{table} s LEFT JOIN temp.previous_{table} p ON p.{key} = s.{key} \
        WHERE s.{key} > ? ORDER BY s.{key} LIMIT {REFRESH_BATCH_ROWS}"
    );
    let mut cursor = String::new();
    let mut changed = 0;
    let mut longest_hold = std::time::Duration::ZERO;
    loop {
        let rows = sqlx::query(&select)
            .bind(&cursor)
            .fetch_all(&mut **snapshot)
            .await?;
        if rows.is_empty() {
            break;
        }
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let started = std::time::Instant::now();
        for row in rows {
            if started.elapsed() >= std::time::Duration::from_millis(100) {
                break;
            }
            let values = refresh_values(&row, 0, names.len())?;
            let old_rowid: Option<i64> = row.try_get(names.len())?;
            let old = refresh_values(&row, names.len() + 1, names.len())?;
            let id = match &values[key_index] {
                RefreshValue::Text(s) => s,
                _ => anyhow::bail!("non-text refresh key"),
            };
            cursor.clone_from(id);
            if old_rowid.is_some() && values == old {
                continue;
            }
            if is_fts {
                if let Some(rowid) = old_rowid {
                    let sql =
                        format!("DELETE FROM main.work_fts WHERE rowid = ? AND {same_previous}");
                    if bind_refresh_values(sqlx::query(&sql).bind(rowid), &old)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected()
                        == 0
                    {
                        continue;
                    }
                }
                let sql = format!(
                    "INSERT INTO main.work_fts ({column_list}) SELECT {placeholders} \
                    WHERE EXISTS (SELECT 1 FROM main.work WHERE id = ?) \
                      AND (? OR NOT EXISTS (SELECT 1 FROM main.work_fts WHERE work_id = ?))"
                );
                changed += bind_refresh_values(sqlx::query(&sql), &values)
                    .bind(id)
                    .bind(old_rowid.is_some())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            } else {
                let mut query = bind_refresh_values(sqlx::query(&upsert), &values)
                    .bind(old_rowid.is_some())
                    .bind(id);
                if matches!(
                    table,
                    "feed_updates" | "feed_series_updates" | "browse_catalogue"
                ) {
                    query = query.bind(id);
                }
                changed += bind_refresh_values(query.bind(old_rowid.is_some()), &old)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
        }
        tx.commit().await?;
        longest_hold = longest_hold.max(started.elapsed());
        tokio::task::yield_now().await;
    }
    let prune = format!(
        "SELECT p.snapshot_rowid, {previous_names} FROM temp.previous_{table} p \
        WHERE p.{key} > ? AND NOT EXISTS (SELECT 1 FROM temp.{table} s WHERE s.{key} = p.{key}) \
        ORDER BY p.{key} LIMIT {REFRESH_BATCH_ROWS}"
    );
    cursor.clear();
    loop {
        let rows = sqlx::query(&prune)
            .bind(&cursor)
            .fetch_all(&mut **snapshot)
            .await?;
        if rows.is_empty() {
            break;
        }
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let started = std::time::Instant::now();
        for row in rows {
            if started.elapsed() >= std::time::Duration::from_millis(100) {
                break;
            }
            let old = refresh_values(&row, 1, names.len())?;
            let id = match &old[key_index] {
                RefreshValue::Text(s) => s,
                _ => anyhow::bail!("non-text refresh key"),
            };
            cursor.clone_from(id);
            let sql = format!("DELETE FROM main.{table} WHERE rowid = ? AND {same_previous}");
            changed += bind_refresh_values(sqlx::query(&sql).bind(row.try_get::<i64, _>(0)?), &old)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        longest_hold = longest_hold.max(started.elapsed());
        tokio::task::yield_now().await;
    }
    tracing::info!(
        table,
        changed,
        longest_writer_hold_ms = longest_hold.as_millis(),
        "catalogue snapshot published"
    );
    Ok(changed)
}

pub async fn refresh_feed_updates(pool: &Database) -> Result<u64> {
    let now = Utc::now().to_rfc3339();
    let mut tx = refresh_snapshot(pool, &["feed_updates"]).await?;
    sqlx::query("DELETE FROM feed_updates")
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query(&format!(
        "INSERT INTO feed_updates ({FEED_UPDATES_COLUMNS}) {}",
        feed_updates_select("")
    ))
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    publish_snapshot(pool, &mut tx, "feed_updates").await?;
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
/// Build both halves, types, counts and clocks in a private read-only-main snapshot.
/// Publish complete rows in bounded transactions; a reader can see adjacent batches from
/// different reconciles, but never an untyped or incompletely counted row. Concurrent
/// incremental cache writes win over stale snapshot data.
///
/// Overwrite `feed_series_updates`' release clock and chapter label from the release
/// ledger — for one work, or for every work when `work_id` is `None`. Returns rows changed.
///
/// **THIS IS WHAT FIXES F7**, and it fixes it structurally rather than by convention.
///
/// Both halves of the feed used to merge their own clocks with
/// `released_at = MAX(feed_series_updates.released_at, excluded.released_at)`, so a second
/// source mirroring a chapter a first source had already reported moved the card's clock
/// FORWARD and re-floated it to the top of /updates. The owner's requirement is the exact
/// opposite: *"if more sources update the chapters later, they won't hit the updates page
/// since another extension updated it earlier."*
///
/// A work's release time is now `MAX(first_seen_at)` over its ledger events. Because
/// `release_event` is `PRIMARY KEY (work_id, chapter_key)` + `INSERT OR IGNORE`, a duplicate
/// chapter produces NO new event — so the `MAX` cannot move, so the card cannot re-float.
/// There is no comparison left to get backwards.
///
/// `latest_chapter` comes along because it must agree: it is the label of the event that
/// produced that `MAX`, via SQLite's bare-columns-with-one-aggregate rule. Taking the clock
/// from one place and the label from another is how the feed ended up printing a chapter
/// COUNT next to a release time from a different chapter entirely (F4).
///
/// AN `UPDATE`, NOT PART OF THE INSERTS. Deliberate: the two halves keep owning every
/// display field they already own (title, cover, reader_id, comic_type), and this pass owns
/// exactly two columns. It also means a work with no ledger events — an undated chapter
/// list, or a work whose spine has not drained yet — is simply left alone rather than
/// having its card blanked, which is what a `JOIN` in the inserts would have done.
///
/// Idempotent, so the rebuild and the incremental writer converge on it by construction:
/// both compute the same expression over the same table, and running it twice changes
/// nothing the second time.
pub async fn project_feed_from_ledger(
    tx: &mut sqlx::SqliteConnection,
    work_id: Option<&str>,
) -> Result<u64> {
    let scope = if work_id.is_some() {
        "AND re.work_id = ?"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE feed_series_updates SET \
             released_at = led.newest, \
             latest_chapter = led.label \
           FROM (SELECT re.work_id AS work_id, \
                        MAX(re.first_seen_at) AS newest, \
                        re.label AS label \
                   FROM release_event re \
                  WHERE 1 = 1 {scope} \
                  GROUP BY re.work_id) AS led \
          WHERE led.work_id = feed_series_updates.work_id"
    );
    let mut q = sqlx::query(&sql);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    let n = q.execute(&mut *tx).await?.rows_affected();

    // THE SORT KEY (migration 0099), as a SECOND statement rather than a second column on
    // the one above. Not a style choice: the statement above owes its correctness to
    // SQLite's bare-columns rule, which only holds while the group has EXACTLY ONE min/max
    // aggregate. Adding `MAX(announce_at)` beside `MAX(first_seen_at)` would make `re.label`
    // come from an arbitrary row — the clock from one chapter and the label from another,
    // which is F4 exactly. One aggregate per statement keeps both guarantees.
    //
    // `COALESCE(announce_at, first_seen_at)` is the identity on every row written before
    // 0099 (the column is NULL there, and no backfill invented a value for it), so on the
    // day this deploys `announced_at` equals `released_at` for the whole table and the
    // feed's order is byte-identical to today's. It diverges only as new events record a
    // real announce clock.
    //
    // NARROWED TO THE WORKS THAT HAVE AN ANNOUNCE CLOCK, via migration 0099's partial index
    // `idx_release_event_announced`. For every other work `MAX(COALESCE(announce_at,
    // first_seen_at))` is by definition `MAX(first_seen_at)` — the value `released_at` already
    // holds and `default_announced_at` already copies — so computing it here is pure waste.
    // Measured on a copy of production (1,031,473 events / 67,830 feed rows): the
    // unconditional form cost 1,867 ms, the narrowed form under 1 ms, inside a transaction
    // already running ~13 s against a 15 s `busy_timeout`. The `IN` subquery reads a partial
    // index that is EMPTY until a chapter is genuinely discovered late, so this pass is free
    // until it has real work to do and proportional to that work afterwards.
    let announce_sql = format!(
        "UPDATE feed_series_updates SET \
             announced_at = led.announced \
           FROM (SELECT re.work_id AS work_id, \
                        MAX(COALESCE(re.announce_at, re.first_seen_at)) AS announced \
                   FROM release_event re \
                  WHERE re.work_id IN (SELECT work_id FROM release_event \
                                        WHERE announce_at IS NOT NULL) {scope} \
                  GROUP BY re.work_id) AS led \
          WHERE led.work_id = feed_series_updates.work_id"
    );
    let mut aq = sqlx::query(&announce_sql);
    if let Some(w) = work_id {
        aq = aq.bind(w);
    }
    aq.execute(&mut *tx).await?;

    // The FIRST statement's count is the return value, unchanged. The two statements no longer
    // touch the same row set (the second is narrowed above), and neither count is the one
    // callers log — they log "how many feed rows did the ledger date", which is the first.
    Ok(n)
}

/// The work a `source_series` belongs to, or `None` if the row has gone.
pub async fn work_id_for_source_series(
    pool: &Database,
    source_series_id: &str,
) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = ?")
            .bind(source_series_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// [`project_feed_from_ledger`] for one work, off a pool, and only once the ledger is
/// complete. This is the incremental counterpart of the rebuild's pass (5) — the two run the
/// same statement over the same table, which is what makes them converge by construction
/// rather than by a field-mapping agreement that has to be tested into place.
pub async fn project_feed_from_ledger_for_work(pool: &Database, work_id: &str) -> Result<u64> {
    let complete = ledger::is_complete(pool).await.unwrap_or(false);
    let mut conn = pool.write().acquire().await?;
    let n = if complete {
        project_feed_from_ledger(&mut conn, Some(work_id)).await?
    } else {
        0
    };

    // BEST-EFFORT, AND NEVER `?`. This function is called from `scanner::
    // upsert_feed_series_update`, i.e. the scanner's hot path, and before 0099 it returned
    // `Ok(0)` without touching the database at all while the ledger was incomplete. Bubbling
    // an error out of the floor pass would newly fail a whole scan on a transient
    // `SQLITE_BUSY` — the rebuild holds a ~13 s write lock against a 15 s `busy_timeout`, so
    // that is a real collision, not a theoretical one — and the scan is worth more than the
    // sort key.
    //
    // Swallowing is safe precisely because this is a FLOOR and not the only writer: migration
    // 0099 populated every existing row, the rebuild's own ungated pass re-establishes it for
    // the whole table, and the next incremental write retries this work. The worst case of a
    // loss here is one newly-inserted row sorting last until either of those runs, which is
    // strictly better than dropping the scan that discovered it.
    if let Err(e) = default_announced_at(&mut conn, Some(work_id)).await {
        tracing::warn!(work_id, error = %e, "feed: announce-clock floor skipped");
    }
    Ok(n)
}

/// Give every feed row that has no announce clock the one honest default: its own release
/// clock. Scoped to one work, or the whole table when `work_id` is `None`.
///
/// WHY THIS EXISTS, AND WHY IT IS NOT OPTIONAL. `announced_at` (migration 0099) is the
/// column `graphql::updates_feed` now ORDERS BY, and the two INSERT halves of
/// [`refresh_feed_series_updates`] cannot fill it: only the ledger knows an announce clock,
/// and the ledger's projection is gated on `ledger::is_complete`. So between a wholesale
/// rebuild and a complete ledger — which is every fresh database, every restore, and the
/// entire Phase B/C drain window — the column would be NULL for every row and the feed's
/// order would be *arbitrary*, not merely stale. A test caught exactly that
/// (`updates_feed_pages_are_disjoint_and_total_matches_a_full_walk`): 25 rows with distinct
/// release times came back shuffled.
///
/// `WHERE announced_at IS NULL` makes it a floor and never an overwrite, so a real announce
/// clock the ledger has already projected survives every later rebuild pass. And because the
/// default IS `released_at`, a feed that has never seen an announce clock sorts EXACTLY as it
/// did before 0099 — the identity property the migration is deployed on.
///
/// A plain UPDATE, no aggregate, deliberately: the scanner half's INSERT owes its bare-column
/// correctness to having exactly ONE min/max aggregate in its GROUP BY, and computing this
/// inline would have added a second one and quietly made `title`/`cover_url`/`reader_id` come
/// from an arbitrary source row.
pub async fn default_announced_at(
    conn: &mut sqlx::SqliteConnection,
    work_id: Option<&str>,
) -> Result<u64> {
    let sql = if work_id.is_some() {
        "UPDATE feed_series_updates SET announced_at = released_at \
          WHERE announced_at IS NULL AND work_id = ?"
    } else {
        "UPDATE feed_series_updates SET announced_at = released_at WHERE announced_at IS NULL"
    };
    let mut q = sqlx::query(sql);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    Ok(q.execute(&mut *conn).await?.rows_affected())
}

pub async fn refresh_feed_series_updates(pool: &Database) -> Result<u64> {
    // This gate is a read; the expensive derivations below only modify TEMP tables.
    let ledger_ready = ledger::is_complete(pool).await.unwrap_or(false);
    let mut tx = refresh_snapshot(pool, &["feed_series_updates"]).await?;
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
    //     reader's library), and `sy.latest_chapter_at IS NOT NULL` keeps the NOT NULL sort
    //     key honest: an undated series is not an update.
    //
    //     THE F3 GATE IS GONE (owner-approved 2026-07-31). This used to carry
    //     `AND sss.last_new_chapter_at IS NOT NULL` on an INNER JOIN to
    //     `series_scan_state`, which meant membership was "OUR scanner has observed a
    //     NEW chapter on this series since we started watching it". A first observation
    //     is a baseline and deliberately never stamps that column (`scanner::record_scan`),
    //     so a series we mirrored completely but never saw *change* was locked out
    //     forever. Measured on a 2026-07-31 snapshot: it admitted 1,820 works out of the
    //     10,966 that qualify on every other ground.
    //
    //     The JOIN is now LEFT, and that is the load-bearing half of the change: dropping
    //     only the NULL test would have changed nothing, because ~11k of the 14k
    //     `series_scan_state` rows that the INNER JOIN needs either do not exist or hold a
    //     NULL, and the INNER JOIN excludes the missing ones on its own. `detected_at`
    //     becomes NULL for a series we have never recorded a detection on, which is the
    //     honest value — it is nullable, it is never a sort key here, and the conflict
    //     clause below COALESCEs rather than overwrites.
    //
    //     The clock comes from the LEDGER, not from membership: pass (5)'s
    //     `project_feed_from_ledger` overwrites `released_at` with
    //     `MAX(release_event.first_seen_at)`, a real upstream release time. That is why
    //     the newly-admitted cohort sinks instead of flooding page 1 — verified on the
    //     snapshot: of the 9,146 newly-admitted works only 1,422 are new CARDS (the rest
    //     already had a mirror-half row), all 1,422 carry a ledger clock, and they take
    //     3 of the top 100 rows and 0 of the top 20.
    //
    //     KEEP IN SYNC with `scanner::upsert_feed_series_update`, which runs the same
    //     SELECT narrowed to one series; `incremental_write_converges_with_the_periodic_rebuild`
    //     fails if the two disagree.
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
                printf('%g', lc.chapter_number), lc.name, sy.chapter_count, \
                MAX(CAST(sy.latest_chapter_at AS INTEGER)), \
                sss.last_new_chapter_at, \
                COALESCE(w.is_nsfw_override, w.is_nsfw), \
                {FSU_STATUS_SQL}, COALESCE(w.content_rating, 'safe') \
         FROM source_series ss \
         JOIN work w ON w.id = ss.work_id \
         JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
         LEFT JOIN series_scan_state sss ON sss.series_id = ss.source_key \
         LEFT JOIN (SELECT manga_id, chapter_number, name, \
                            ROW_NUMBER() OVER (PARTITION BY manga_id \
                                ORDER BY CAST(upload_date AS INTEGER) DESC, \
                                         chapter_number DESC) AS rn \
                       FROM suwayomi_chapter \
                      WHERE chapter_number >= 0 AND chapter_number <= 5000) lc \
                ON lc.manga_id = sy.id AND lc.rn = 1 \
         WHERE ss.source_type = 'suwayomi' AND sy.in_library = 1 \
           AND sy.latest_chapter_at IS NOT NULL \
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
    //     In the private snapshot, deliberately — see the atomicity note in the doc
    //     comment. It reads the rows the two INSERTs above just wrote, which are visible
    //     to this connection and to nobody else until the publication below, so each published
    //     row becomes visible fully typed.
    let typed = fill_comic_types(&mut tx, "feed_series_updates", None).await?;

    // (4) `en_chapter_count` (migration 0068) — Browse's CHAPTERS sort key and the number
    //     its cards print. Two statements, in this order, because the fallback must only
    //     ever RAISE a zero: the English mirror count is authoritative where it exists, and
    //     the Suwayomi count is the only thing available for a work whose MangaDex spine has
    //     no English chapter (the same precedence `map_canonical_series` applies between its
    //     English count and `aggregate_chapter_count`).
    //
    //     STILL In the private snapshot, for the reason the doc comment gives about
    //     `comic_type`: this is a SORT KEY, and a committed generation where every row reads
    //     0 would make `sort: CHAPTERS` degenerate to the `work_id DESC` tiebreaker — a
    //     silently wrong ordering rather than a visibly empty one, which is worse.
    let counted = fill_en_chapter_count(&mut tx, None).await?;

    // (5) THE RELEASE CLOCK, taken from the ledger rather than from either half (Phase C2).
    //     This is the pass that fixes F7. See `project_feed_from_ledger` for why, and
    //     `ledger::is_complete` for why it is gated rather than unconditional.
    if ledger_ready {
        let projected = project_feed_from_ledger(&mut tx, None).await?;
        tracing::info!(projected, "feed: release clock projected from the ledger");
    }

    // (6) THE SORT KEY'S FLOOR (migration 0099). UNGATED, and that is the point: pass (5) is
    //     the only thing that can supply a real announce clock and it does not run until the
    //     ledger is complete, so without this a freshly staged row would
    //     leave `announced_at` NULL for the whole table and `updates_feed`'s ORDER BY would
    //     have nothing to sort on. Defaulting to `released_at` reproduces the pre-0099 order
    //     exactly. In the private snapshot for the same reason pass (3) is — this is a SORT
    //     KEY, and a committed generation that cannot be ordered is worse than a missing one.
    let defaulted = default_announced_at(&mut tx, None).await?;
    tracing::info!(
        defaulted,
        "feed: announce clock defaulted to the release clock"
    );

    publish_snapshot(pool, &mut tx, "feed_series_updates").await?;

    // Derive Browse after the feed's bounded publication has completed.
    if let Err(e) = refresh_browse_catalogue(pool).await {
        tracing::warn!(error = %e, "browse_catalogue: refresh failed");
    }

    // Search follows Browse so search hits can be hydrated through its reader ids.
    if let Err(e) = refresh_work_fts(pool).await {
        tracing::warn!(error = %e, "work_fts: refresh failed");
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

/// THE MANGADEX HALF'S INCREMENTAL FEED WRITER, for one work — the mirror-side counterpart
/// of `scanner::upsert_feed_series_update`.
///
/// ## What was wrong before this existed
///
/// The firehose's per-page write was an un-extracted block inside `mangadex::sync_chapters`
/// that ran only [`project_feed_from_ledger_for_work`] — an `UPDATE … FROM`. An UPDATE
/// cannot create a row, so a work whose FIRST English chapter had just been mirrored (a
/// brand-new series, or the 4,493-record catalogue gap being back-filled) stayed invisible on
/// `/updates` until the next wholesale rebuild — which is exactly the F5 staleness the block
/// was written to fix. Nothing failed and nothing logged: the projection reported `0 rows`,
/// which is indistinguishable from "already correct". No test invoked it either, because
/// `scanner::incremental_write_converges_with_the_periodic_rebuild` drives the SCANNER half
/// only (§8h).
///
/// ## The convergence contract
///
/// The wholesale chain for this half is five passes:
/// `refresh_feed_updates` (1) → the mirror INSERT (2) → [`fill_comic_types`] (3) →
/// `en_chapter_count` (4) → [`project_feed_from_ledger`] (5). This runs all five, narrowed to
/// one work, and passes 1/3/4/5 are the SAME code the rebuild runs — [`feed_updates_select`],
/// [`fill_comic_types`], [`fill_en_chapter_count`] and [`project_feed_from_ledger`] each take
/// a scope argument rather than being copied. Only pass (2)'s conflict clause is restated,
/// and it is restated in the CONVERGED direction, not copied:
///
/// * The rebuild inserts the mirror half into an EMPTY table and then lets the scanner half
///   merge on top, so the settled row is "mirror wins the display fields, the clock is
///   `MAX(both)`, the scanner supplies `chapter_count` / `detected_at` /
///   `suwayomi_thumbnail`". Here the pre-existing row may be the scanner's (or an earlier
///   write of our own), so the same settled row is reached by ASSIGNING the display fields
///   (`reader_id`, `title`, `latest_chapter`, `latest_chapter_title`) and OMITTING the three
///   the scanner owns. Omission is the point: copying `chapter_count = excluded.…` would
///   write the mirror's literal NULL over a real Suwayomi count, and the reader renders
///   `Ch. {latest_chapter ?? chapter_count}` (F4).
/// * `released_at = MAX(existing, excluded)` matches the rebuild's scanner-half clause, so a
///   work with both halves settles on the same clock whichever writer arrives last. Pass (5)
///   then overwrites it from the ledger anyway, which is what makes the `MAX` harmless (F7:
///   a duplicate chapter creates no event, so the ledger `MAX` cannot move).
/// * `reader_id = excluded.reader_id` is safe precisely because this SELECT only ever
///   produces MangaDex-anchored works, and both halves derive `w_…` for those.
///
/// KNOWN CORNER, shared with the scanner half: if a work's mirror clock ever moved BACKWARD
/// (a chapter unpublished upstream) while the ledger is still filling, `MAX` keeps the older,
/// higher value and the next rebuild lowers it. That is a reconciliation, not a
/// contradiction, and it is why C3's drift reporter counts rows rather than trusting this.
///
/// Returns `true` when the work now has a `feed_series_updates` row from this half.
pub async fn publish_mirror_feed_row(pool: &Database, work_id: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();

    // (1) The work's `feed_updates` row (migration 0051). Maintained here rather than left to
    //     the rebuild because `canonicalUpdates` reads THAT table directly, so leaving it
    //     stale would fix one of the reader's two update surfaces and not the other.
    //
    //     Assign every column unconditionally (no "where changed" guard) so `rows_affected`
    //     answers "does this work still qualify?" — it is one row against a primary key.
    let set = FEED_UPDATES_COLUMNS
        .split(", ")
        .filter(|c| *c != "work_id")
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let qualified = sqlx::query(&format!(
        "INSERT INTO feed_updates ({FEED_UPDATES_COLUMNS}) {} \
         ON CONFLICT(work_id) DO UPDATE SET {set}",
        feed_updates_select("AND ss.work_id = ?")
    ))
    .bind(&now)
    .bind(work_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if !qualified {
        // Every English chapter is unreleased (or gone). The rebuild's DELETE + INSERT would
        // drop this row, so drop it — otherwise a scheduled-then-withdrawn chapter would
        // outlive its own release. The `feed_series_updates` row is deliberately left alone:
        // the scanner half may legitimately own it, and only the rebuild sees both halves.
        sqlx::query("DELETE FROM feed_updates WHERE work_id = ?")
            .bind(work_id)
            .execute(pool)
            .await?;
        return Ok(false);
    }

    // (2) The merged feed's mirror half, narrowed to this work. Field-for-field the rebuild's
    //     pass (2); see the doc comment for why the conflict clause is restated.
    let inserted = sqlx::query(&format!(
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
         WHERE fu.work_id = ? AND strftime('%s', fu.latest_at) IS NOT NULL \
         ON CONFLICT(work_id) DO UPDATE SET \
             reader_id = excluded.reader_id, \
             title = excluded.title, \
             latest_chapter = excluded.latest_chapter, \
             latest_chapter_title = excluded.latest_chapter_title, \
             cover_url = COALESCE(NULLIF(excluded.cover_url, ''), feed_series_updates.cover_url), \
             released_at = MAX(feed_series_updates.released_at, excluded.released_at), \
             is_nsfw = MAX(feed_series_updates.is_nsfw, excluded.is_nsfw), \
             status = excluded.status, \
             content_rating = excluded.content_rating"
    ))
    .bind(work_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if !inserted {
        // `strftime` could not parse the release time, so the rebuild would skip this work
        // too (its guard is the same expression). Nothing downstream to do.
        return Ok(false);
    }

    // (3)(4) The two passes the INSERT cannot express, scoped to this work and running the
    //        rebuild's own code. `comic_type` matters most on a row this call just CREATED:
    //        a NULL type is invisible to the reader's format tabs and to Browse's format
    //        chips, so a brand-new work would sit in the unfiltered feed and vanish from
    //        every tab until the next rebuild.
    let mut conn = pool.write().acquire().await?;
    fill_comic_types(&mut conn, "feed_series_updates", Some(work_id)).await?;
    fill_en_chapter_count(&mut conn, Some(work_id)).await?;
    drop(conn);

    // (5) The release clock, from the ledger — the same statement the rebuild's pass (5)
    //     runs, and gated on the same `ledger::is_complete`, so a half-seeded ledger cannot
    //     sink a live card.
    project_feed_from_ledger_for_work(pool, work_id).await?;
    Ok(true)
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
    browse_catalogue_select_for("")
}

/// [`browse_catalogue_select`] with an extra predicate appended to its WHERE — the ONE
/// point of variation between the wholesale rebuild (`""`) and the per-work re-projection
/// [`reproject_work_caches`] uses (`" AND w.id = ?"`).
///
/// A parameter rather than a second copy of the SELECT, because a copy is precisely the
/// failure this table has already had once: `scanner::mirror_feed_row_into_browse_catalogue`
/// hand-copied the mutable-column list and drifted from it, freezing every incrementally
/// mirrored card's chapter number (§8h). One expression, two scopes.
fn browse_catalogue_select_for(scope: &str) -> String {
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
            {rating}, \
            CASE WHEN f.work_id IS NOT NULL THEN f.is_nsfw \
                 ELSE COALESCE(w.is_nsfw_override, w.is_nsfw) END, \
            -- The BEST-SOURCED count wins, not the MangaDex one. This was
            -- `COALESCE(NULLIF(en.n,0), NULLIF(swc.n,0), 0)` — English spine first,
            -- Suwayomi only as a zero-fallback — which showed 1 ch for 495 works whose
            -- MangaDex mirror holds a stub while their Suwayomi source carries the real
            -- run (Starting Over as a Tree: en=1, suwayomi=526; median 3.7x undercount).
            -- MAX is safe here where the aggregate in `aggregate_chapter_count` is not:
            -- this reads `suwayomi_series.chapter_count`, one already-deduped number per
            -- source, and every `suwayomi_series` row is `lang='en'` (14,182/14,182), so
            -- there is no non-English inflation to guard against. The 3,737 works where
            -- the English spine is larger are unaffected — MAX picks it, as before.
            MAX(COALESCE(en.n, 0), COALESCE(swc.n, 0)), \
            f.latest_chapter, \
            f.released_at, \
            f.announced_at, \
            w.created_at, \
            {BROWSE_DEMOGRAPHIC_SQL}, \
            {BROWSE_LANGUAGE_SQL}, \
            {BROWSE_YEAR_SQL}, \
            st.follows, \
            st.rating \
       FROM work w \
       LEFT JOIN feed_series_updates f ON f.work_id = w.id \
       LEFT JOIN work_stats st ON st.work_id = w.id \
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
        AND COALESCE(f.title, w.title_override, w.primary_title, sw.title, '') <> ''{scope}",
        rating = effective_content_rating_sql(
            "CASE WHEN f.work_id IS NOT NULL THEN f.content_rating \
                  ELSE COALESCE(w.content_rating, 'safe') END",
            "CASE WHEN f.work_id IS NOT NULL THEN f.is_nsfw \
                  ELSE COALESCE(w.is_nsfw_override, w.is_nsfw) END",
        ),
    )
}

/// `browse_catalogue.demographic` (migration 0106) over a `work` row aliased `w`: MangaDex's
/// four words, lowercased, and NULL for everything else — `"none"` upstream, an unknown, or a
/// stray casing/spacing variant from a non-MangaDex writer. Normalized at WRITE time so the
/// filter is a plain `IN (…)` over a closed vocabulary, and `Demographic.NONE` is exactly
/// `IS NULL`.
///
/// Shared by the rebuild's SELECT and `scanner::mirror_feed_row_into_browse_catalogue`, so the
/// two writers of the table produce the same word (and migration 0106's backfill mirrors it by
/// hand).
pub(crate) const BROWSE_DEMOGRAPHIC_SQL: &str = "CASE lower(trim(w.demographic)) \
        WHEN 'shounen' THEN 'shounen' WHEN 'shoujo' THEN 'shoujo' \
        WHEN 'seinen' THEN 'seinen' WHEN 'josei' THEN 'josei' END";

/// `browse_catalogue.original_language` (0106): MangaDex's `originalLanguage`, lowercased,
/// blank folded to NULL. The filter's language codes are lowercased the same way
/// (`browse::canonical_languages`), so "ZH-HK" and "zh-hk" are one value.
pub(crate) const BROWSE_LANGUAGE_SQL: &str = "NULLIF(lower(trim(w.original_language)), '')";

/// `browse_catalogue.year` (0106): the publication year, or NULL when it is missing or
/// implausible. The bounds are wider than the filter's 1900..=2100 clamp on purpose — they
/// only reject garbage (a 0 or a 5-digit typo would otherwise sort first under YEAR ASC), and
/// a real 1890s work must still be sortable.
pub(crate) const BROWSE_YEAR_SQL: &str = "CASE WHEN w.year BETWEEN 1800 AND 2200 THEN w.year END";

/// The EFFECTIVE four-level content rating Browse materializes and filters on (migration
/// 0106), from a stored rating expression and an effective-NSFW expression.
///
/// HOW `is_nsfw` IS DERIVED TODAY (verified, not assumed). A MangaDex work is `is_nsfw = 1`
/// exactly when its rating is `erotica` or `pornographic` (`mangadex::to_work_input`) —
/// SUGGESTIVE IS NOT NSFW, and stays visible to an opted-out viewer. On top of that, a source
/// can ESCALATE a work (`mark_work_nsfw`: a Suwayomi source flagged NSFW, or an adult genre
/// via `genre_is_nsfw`), except where MangaDex authoritatively rated it safe/suggestive; and an
/// admin `is_nsfw_override` wins over everything. That gate is unchanged here.
///
/// WHY AN *EFFECTIVE* RATING RATHER THAN THE RAW COLUMN. The rating and the gate are two
/// columns that can disagree: a Suwayomi-only work flagged NSFW by its source has NO MangaDex
/// rating, so it was materialized as `safe` (the old `COALESCE(…, 'safe')`), and an admin can
/// pin a MangaDex-`safe` work NSFW. With the raw column, an opted-in viewer asking for
/// `contentRatings: [SAFE]` got adult works labelled safe. So a work the gate treats as NSFW is
/// never materialized below `erotica` — the lowest adult level, and the honest reading of "we
/// know it is adult but not how adult". The other direction (an `erotica` work an admin forced
/// SFW) keeps its real rating: the label stays true, and the gate, which is the authority on
/// VISIBILITY, already lets an opted-out viewer see it by the admin's decision.
///
/// The consequence that matters for the filter: `contentRatings` can only ever NARROW inside
/// the gate. `SAFE`/`SUGGESTIVE` now select exactly the gate-visible works for every viewer,
/// and `EROTICA`/`PORNOGRAPHIC` select nothing for an opted-out viewer because the gate's
/// `is_nsfw = 0` and an adult effective rating are (admin overrides aside) mutually exclusive.
///
/// Both writers of `browse_catalogue` call this with their own column expressions; for a work
/// with a feed row both pass the feed row's `content_rating`/`is_nsfw`, so they converge.
pub(crate) fn effective_content_rating_sql(rating: &str, nsfw: &str) -> String {
    format!(
        "CASE WHEN ({nsfw}) = 1 AND ({rating}) IN ('safe', 'suggestive') \
              THEN 'erotica' ELSE ({rating}) END"
    )
}

/// The `browse_catalogue` columns [`browse_catalogue_select`] produces, in its order.
/// `comic_type` is absent on purpose — see [`fill_comic_types`].
const BROWSE_CATALOGUE_COLUMNS: &str = "work_id, reader_id, title, cover_url, \
     suwayomi_thumbnail, status, content_rating, is_nsfw, en_chapter_count, latest_chapter, \
     released_at, announced_at, created_at, demographic, original_language, year, follows, \
     rating";

/// The columns the UPSERT re-assigns on conflict, i.e. everything except the primary key and
/// `comic_type`.
///
/// `pub(crate)` so `scanner::mirror_feed_row_into_browse_catalogue` — the OTHER writer of this
/// table — can use this list instead of hand-copying it. The copy had already drifted once:
/// `latest_chapter` was missing from it when migration 0095 landed, so Browse's chapter NUMBER
/// went stale on every incrementally-mirrored series while the COUNT beside it advanced (§8h).
/// One list, two writers.
pub(crate) const BROWSE_CATALOGUE_MUTABLE: &[&str] = &[
    "reader_id",
    "title",
    "cover_url",
    "suwayomi_thumbnail",
    "status",
    "content_rating",
    "is_nsfw",
    "en_chapter_count",
    // Migration 0095. In the MUTABLE set, not just the INSERT set, because the value moves:
    // the ledger projection rewrites `feed_series_updates.latest_chapter` whenever a newer
    // release arrives, and a Browse card frozen on the label it had at first insert would
    // drift away from the same work's Updates card.
    "latest_chapter",
    "released_at",
    // No longer a sort key: migration 0102 moved Browse's LATEST_ADDED ordering to
    // `created_at` and dropped this column's indices (it tracked the newest CHAPTER's
    // announce clock, so "Latest added" filled with actively-updating works). Still
    // written: it keeps this row a verbatim copy of the feed row, and a Browse card must
    // not disagree with the same work's Updates card.
    "announced_at",
    "created_at",
    // Migration 0106 — Browse v2's filter/sort keys. All five are MUTABLE: MangaDex re-syncs
    // can change demographic/language/year, and `follows`/`rating` move every time the stats
    // drain re-fetches a work. The scanner's incremental mirror writes them from `work` /
    // `work_stats` with the SAME expressions (`BROWSE_*_SQL`), which is what lets it share
    // this list.
    "demographic",
    "original_language",
    "year",
    "follows",
    "rating",
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
pub async fn refresh_browse_catalogue(pool: &Database) -> Result<u64> {
    // Suwayomi-only works' tags (migration 0106): map their sources' free-text genres onto
    // the MangaDex vocabulary BEFORE the rebuild, so the tag counts below — and the tag filter
    // — see this cycle's result. Its own (short, diff-only) transaction, ahead of this one,
    // to keep the write lock this function holds inside the pool's busy_timeout budget.
    // Best-effort: a failure leaves last cycle's derived tags in place, which is stale but
    // not wrong, and must not cost Browse its rebuild.
    match crate::tags::sync_suwayomi_tags(pool).await {
        Ok(s) if s != crate::tags::SuwayomiTagSync::default() => tracing::info!(
            inserted = s.inserted,
            deleted = s.deleted,
            demographics = s.demographics,
            "tags: synced Suwayomi-derived tags"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "tags: Suwayomi genre sync failed"),
    }

    let keyed = repair_catalogue_tag_keys(pool).await?;
    let mut tx = refresh_snapshot(pool, &["browse_catalogue", "tag", "feed_genre_facet"]).await?;

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
    let typed = fill_comic_types(&mut tx, "browse_catalogue", None).await?;

    // The tag vocabulary (migration 0106), in THIS transaction for the facet table's reason
    // below: `tag.work_count` is the number on a tag chip, and it must count the generation
    // the tag filter is about to query. Keys first (the safety net under every `work_tag`
    // writer — see `tags::fill_tag_keys`), then the counts over them.
    let tags = crate::tags::rebuild_tag_table(&mut tx).await?;

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

    publish_snapshot(pool, &mut tx, "browse_catalogue").await?;
    publish_snapshot(pool, &mut tx, "tag").await?;
    publish_snapshot(pool, &mut tx, "feed_genre_facet").await?;

    // Browse's `total` is memoized for up to `browse::COUNT_TTL`, and we have just rewritten
    // the table those totals counted. A surviving entry would promise a page that no longer
    // exists — `hasNextPage: true` on the last page, or a pager rendering
    // "showing 115,531-115,560 of 115,530". One `HashMap::clear`, twice a day.
    crate::browse::clear_count_cache();

    // The centralized bounded ANALYZE pass owns statistics for both catalogue tables.
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
        keyed,
        tags,
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
///
/// `work_id` narrows all four reads and the write to ONE work, which is what lets
/// [`publish_mirror_feed_row`] run this derivation rather than a copy of it. `f.work_id` is
/// the primary key of both tables this is ever called on, so a scoped pass is four seeks.
async fn fill_comic_types(
    conn: &mut sqlx::SqliteConnection,
    table: &'static str,
    work_id: Option<&str>,
) -> Result<u64> {
    use std::collections::HashMap;
    let scope = if work_id.is_some() {
        "AND f.work_id = ?"
    } else {
        ""
    };

    // Curated tags win outright wherever they exist — the same rule
    // `work_effective_genres` applies. (Empty in production today, but an admin can
    // populate it, and this feed must not be the one surface that ignores that.)
    let mut genres: HashMap<String, Vec<String>> = HashMap::new();
    let sql = format!(
        "SELECT wt.work_id, wt.tag FROM work_tag wt \
         JOIN {table} f ON f.work_id = wt.work_id \
         WHERE wt.source = 'admin' {scope} \
         ORDER BY wt.work_id, wt.ord, wt.tag"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    for (wid, tag) in q.fetch_all(&mut *conn).await? {
        genres.entry(wid).or_default().push(tag);
    }
    // Tier 2: upstream MangaDex tags (migration 0066). This is what actually makes the
    // `comic_type` derivation below work for MangaDex-only works — `resolve_comic_type`
    // reads genre STRINGS to spot "Manhwa"/"Manhua"/"Long Strip", and before these tags
    // existed the ~101k works with no Suwayomi link reached it with an EMPTY genre slice
    // and fell all the way through to the title-script heuristic.
    let mut upstream_genres: HashMap<String, Vec<String>> = HashMap::new();
    let sql = format!(
        "SELECT wt.work_id, wt.tag FROM work_tag wt \
         JOIN {table} f ON f.work_id = wt.work_id \
         WHERE wt.source = 'mangadex' {scope} \
         ORDER BY wt.work_id, wt.ord, wt.tag"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    for (wid, tag) in q.fetch_all(&mut *conn).await? {
        upstream_genres.entry(wid).or_default().push(tag);
    }
    // Source genres, kept separate so the curated-wins rule stays a single lookup below
    // rather than a "did this key come from tags or from genres" question. The CAST is on
    // `ss.source_key` (unindexed TEXT), never on `sw.id` — casting the indexed side makes
    // the join opaque to the planner and forces a full `suwayomi_series` scan per row
    // (`work_effective_genres` measured 14.98 ms → 0.01 ms on exactly that change).
    let mut source_genres: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();
    let sql = format!(
        "SELECT ss.work_id, sw.genre FROM source_series ss \
         JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER) \
         JOIN {table} f ON f.work_id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND sw.genre IS NOT NULL {scope} \
         ORDER BY ss.work_id, ss.id"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    for (wid, json) in q.fetch_all(&mut *conn).await? {
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

    let sql = format!(
        "SELECT f.work_id, w.content_type_override, w.original_language, f.title \
         FROM {table} f JOIN work w ON w.id = f.work_id WHERE 1 = 1 {scope}"
    );
    let mut q = sqlx::query_as::<_, (String, Option<String>, Option<String>, String)>(&sql);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    let rows = q.fetch_all(&mut *conn).await?;

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

/// `en_chapter_count` (migration 0068) — Browse's CHAPTERS sort key and the number its cards
/// print — for every row, or for one work when `work_id` is `Some`.
///
/// TWO statements, in this order, because the second must only ever RAISE the first: the
/// count a reader should see is the best-sourced one, so the English mirror count is written
/// and then the Suwayomi count overwrites it WHERE IT IS LARGER (`sw.n > en_chapter_count`),
/// which covers both a work whose MangaDex spine has no English chapter at all and the 495
/// whose spine holds a stub against a fully-populated Suwayomi source. It only raises, never
/// lowers, so a work the English mirror knows better keeps its number.
///
/// The raise is safe because `suwayomi_series.chapter_count` is one already-deduped count per
/// source and every such row is `lang='en'` — unlike `aggregate_chapter_count`, which unions
/// raw `suwayomi_chapter` rows and can inflate. Keep this expression in step with
/// `browse_catalogue_select`, which computes the same `MAX` inline for the rebuild.
///
/// ONE function for both scopes so [`publish_mirror_feed_row`] cannot drift from the
/// rebuild's pass (4) — that drift is what makes a card announce a new chapter while still
/// printing the old count. The first statement is a correlated `COALESCE(…, 0)` rather than
/// the rebuild's `FROM (GROUP BY)` join, and the difference is deliberate: the join leaves a
/// work with no English chapter UNTOUCHED, which in a rebuild means the column's
/// `NOT NULL DEFAULT 0` on the row just inserted, and in a work-scoped call would mean
/// whatever the row already held. Stating the 0 is what makes the two agree.
async fn fill_en_chapter_count(
    conn: &mut sqlx::SqliteConnection,
    work_id: Option<&str>,
) -> Result<u64> {
    let scope = if work_id.is_some() {
        "AND feed_series_updates.work_id = ?"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE feed_series_updates SET en_chapter_count = COALESCE( \
             (SELECT COUNT(DISTINCT c.number) FROM chapter c \
                JOIN source_series ss ON ss.id = c.source_series_id \
               WHERE ss.work_id = feed_series_updates.work_id \
                 AND ss.source_type = 'mangadex' AND c.lang = 'en'), 0) \
         WHERE 1 = 1 {scope}"
    );
    let mut q = sqlx::query(&sql);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    let counted = q.execute(&mut *conn).await?.rows_affected();

    let sql2 = format!(
        "UPDATE feed_series_updates SET en_chapter_count = sw.n \
             FROM (SELECT ss.work_id AS work_id, MAX(sy.chapter_count) AS n \
                     FROM source_series ss \
                     JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
                    WHERE ss.source_type = 'suwayomi' \
                    GROUP BY ss.work_id) AS sw \
            WHERE sw.work_id = feed_series_updates.work_id \
              AND sw.n > feed_series_updates.en_chapter_count {scope}"
    );
    let mut q = sqlx::query(&sql2);
    if let Some(w) = work_id {
        q = q.bind(w);
    }
    q.execute(&mut *conn).await?;
    Ok(counted)
}

/// Rebuild `work_fts` without a long main-database write transaction. Read-only-main
/// staging computes the entire corpus; short batches update changed rows and prune stale
/// ones. Unchanged rows retain their rowids and concurrent incremental updates win.
///
/// THE CORPUS IS EVERY SOURCED WORK, not just the MangaDex-anchored ones. 0052 restricted
/// it to MangaDex because `canonicalSeries` rejects a work with no MangaDex anchor, so a
/// non-anchored hit would have been a result that 404s on click. 0064's `reader_id` removed
/// that constraint — a non-anchored work navigates by its numeric Suwayomi id instead — and
/// the search resolver now hydrates through `browse_catalogue`, which carries that id. The
/// WHERE below is therefore 0069's exclusion rule verbatim (a source to open, a title to
/// label), so Browse and search agree on which works exist. Leaving 0052's predicate in
/// place hid 1,824 Suwayomi-only works from every query — see migration 0071.
///
/// Called at the end of the `refresh_feed_updates` chain, i.e. at boot and after each
/// MangaDex catalogue sync, on exactly the cadence that rebuilds `browse_catalogue`. Search
/// and Browse therefore go stale together rather than one lagging the other, which is the
/// property that made this bug invisible: the work was in the grid but not in the index.
pub async fn refresh_work_fts(pool: &Database) -> Result<u64> {
    let mut tx = refresh_snapshot(pool, &["work_fts"]).await?;
    sqlx::query("DELETE FROM work_fts")
        .execute(&mut *tx)
        .await?;
    // Mirrors migration 0071's backfill exactly (chapter count grouped once via a
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
           AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id)",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    publish_snapshot(pool, &mut tx, "work_fts").await?;
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
pub(crate) const RANK_WINDOW: i64 = 500;

/// Bound anonymous search work before allocating tokens or constructing ranking binds.
pub(crate) const MAX_FTS_QUERY_CHARS: usize = 256;
const MAX_FTS_TERMS: usize = 12;

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
pub(crate) fn fts_match_query(raw: &str) -> Option<String> {
    if raw.len() > MAX_FTS_QUERY_CHARS * 4
        || raw.chars().take(MAX_FTS_QUERY_CHARS + 1).count() > MAX_FTS_QUERY_CHARS
    {
        return None;
    }
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
    let last = terms.last()?;
    // A repeated final token already has an exact clause, so its prefix clause is
    // redundant. Retaining the exact clause preserves the original AND semantics.
    let prefix_last = terms.iter().filter(|term| *term == last).count() == 1;
    let mut seen = std::collections::HashSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    if terms.len() > MAX_FTS_TERMS {
        return None;
    }
    let chars: usize = terms.iter().map(|t| t.chars().count()).sum();
    if chars < MIN_FTS_QUERY_CHARS && terms.iter().all(|t| t.is_ascii()) {
        return None;
    }
    let last_index = terms.len() - 1;
    Some(
        terms
            .iter()
            .enumerate()
            .map(|(i, term)| {
                if prefix_last && i == last_index {
                    format!("\"{term}\"*")
                } else {
                    format!("\"{term}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Escape SQLite `LIKE` metacharacters (`\`, `%`, `_`) so a user query is matched
/// literally under `ESCAPE '\'` — a title containing `%` can't turn into a wildcard.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The exact/prefix title TIER of [`search_works_fts`]' ranking — a SQL `CASE` over a `work`
/// row aliased `w` — plus its bind values in order, chosen by script (see that function's doc
/// comment for the tiers and why the case-folding is script-dependent).
///
/// Factored out so Browse v2's filtered text search (`browse::search_text`) ranks RELEVANCE
/// with the IDENTICAL expression: "the filters narrowed my search" must never also mean "and
/// the order changed", and two copies of a six-branch CASE is exactly how that would happen.
pub(crate) fn fts_title_tier(query: &str) -> (&'static str, Vec<String>) {
    if query.is_ascii() {
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
    }
}

/// A search owns one reader until its SQLite statements have stopped. The progress
/// handler interrupts the SQLite VM itself; dropping a cancelled request also trips it.
/// An unfinished connection is closed rather than returned with a stale handler.
pub(crate) struct SearchConnection {
    connection: Option<sqlx::pool::PoolConnection<sqlx::Sqlite>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::ops::Deref for SearchConnection {
    type Target = sqlx::SqliteConnection;
    fn deref(&self) -> &Self::Target {
        self.connection.as_deref().unwrap()
    }
}

impl std::ops::DerefMut for SearchConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_deref_mut().unwrap()
    }
}

impl Drop for SearchConnection {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(connection) = &mut self.connection {
            connection.close_on_drop();
        }
    }
}

impl SearchConnection {
    pub(crate) async fn new(pool: &Database) -> Result<Self> {
        Self::with_budget(pool, std::time::Duration::from_secs(2)).await
    }

    async fn with_budget(pool: &Database, budget: std::time::Duration) -> Result<Self> {
        let mut connection = pool.acquire().await?;
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let check_cancelled = cancelled.clone();
        let deadline = std::time::Instant::now() + budget;
        {
            let mut handle = connection.lock_handle().await?;
            handle.set_progress_handler(1_000, move || {
                !check_cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    && std::time::Instant::now() < deadline
            });
        }
        Ok(Self {
            connection: Some(connection),
            cancelled,
        })
    }

    pub(crate) async fn finish(mut self) -> Result<()> {
        self.connection
            .as_mut()
            .unwrap()
            .lock_handle()
            .await?
            .remove_progress_handler();
        // Only a connection with its handler removed may return to the shared reader pool.
        self.connection.take();
        Ok(())
    }
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
    pool: &Database,
    query: &str,
    show_nsfw: bool,
    page: i64,
    page_size: i64,
) -> Result<(i64, Vec<String>)> {
    let Some(match_expr) = fts_match_query(query) else {
        return Ok((0, Vec::new()));
    };
    let mut connection = SearchConnection::new(pool).await?;
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
    .fetch_one(&mut *connection)
    .await?;
    if offset >= RANK_WINDOW {
        connection.finish().await?;
        return Ok((total, Vec::new()));
    }
    let (tier, tier_params) = fts_title_tier(query);
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
        .fetch_all(&mut *connection)
        .await?;
    connection.finish().await?;
    Ok((total, ids))
}

/// NSFW flag of the work owning a mirrored MangaDex chapter (by chapter uuid), for
/// gating `canonicalPages`. `None` if the chapter isn't in the mirror.
pub async fn chapter_owner_is_nsfw(pool: &Database, external_id: &str) -> Result<Option<bool>> {
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
        // Numbered chapters collapse per number, as before. NUMBER-LESS ones key on their
        // own chapter id instead of all sharing `""` — a work with three oneshots kept one
        // of them, which is the same F2 collapse `work_source_chapters` had.
        let key = crate::chapter_label::chapter_display(None, row.number.as_deref(), None)
            .key(&row.external_id);
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
    pool: &Database,
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
    replace_mangadex_details(&mut tx, &work_id, input).await?;
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
pub async fn create_work(pool: &Database, input: &WorkInput) -> Result<String> {
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
pub async fn delete_work_cascade(pool: &Database, work_id: &str) -> Result<()> {
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
        "work_tracker_link",
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
        // `tag_key` (migration 0106) at write time, so a freshly ingested tag is filterable
        // without waiting for the rebuild's `tags::fill_tag_keys` safety net.
        sqlx::query(
            "INSERT OR IGNORE INTO work_tag (work_id, tag, ord, source, tag_key) \
             VALUES (?, ?, ?, 'mangadex', ?)",
        )
        .bind(work_id)
        .bind(tag)
        .bind(ord as i64)
        .bind(crate::tags::slug(tag))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Refresh imported fields without changing admin overrides or historical IDs.
async fn replace_mangadex_details(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    work_id: &str,
    input: &WorkInput,
) -> Result<()> {
    replace_source_tags(tx, work_id, &input.tags).await?;
    for tag in &input.tag_details {
        sqlx::query("UPDATE work_tag SET tag_id = ?, tag_group = ? WHERE work_id = ? AND tag = ? AND source = 'mangadex'")
            .bind(&tag.id).bind(&tag.group).bind(work_id).bind(&tag.name)
            .execute(&mut **tx).await?;
    }
    sqlx::query("DELETE FROM work_tracker_link WHERE work_id = ?")
        .bind(work_id)
        .execute(&mut **tx)
        .await?;
    for (provider, external_id) in input.external_ids.iter().chain(&input.official_links) {
        if crate::mangadex::tracker_url(provider, external_id).is_some() {
            sqlx::query("INSERT OR REPLACE INTO work_tracker_link (work_id, provider, external_id) VALUES (?, ?, ?)")
                .bind(work_id).bind(provider).bind(external_id).execute(&mut **tx).await?;
        }
    }
    sqlx::query("UPDATE work SET mangadex_details_synced_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(work_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Existing entries visited by the catalogue top-up also refresh their current
/// taxonomy/tracker snapshot. Publication facts follow the normal full upsert
/// so NSFW/source/materialized projections keep their existing refresh semantics.
pub async fn refresh_mangadex_details(
    pool: &Database,
    work_id: &str,
    input: &WorkInput,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    replace_mangadex_details(&mut tx, work_id, input).await?;
    tx.commit().await?;
    Ok(())
}

/// Load a work's cover set (F2), primary first then by volume/file name.
pub async fn load_work_covers(pool: &Database, work_id: &str) -> Result<Vec<Cover>> {
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
pub async fn add_work_alias(pool: &Database, work_id: &str, raw_title: &str) -> Result<String> {
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
pub async fn pick_survivor(pool: &Database, work_ids: &[String]) -> Result<String> {
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

/// A source-owned `work` column an admin may set by hand (migration 0112). The value is
/// written to the column itself and recorded in `work_field_pin`, whose triggers restore
/// it whenever a sync path overwrites the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedField {
    Author,
    Artist,
    Year,
    OriginalLanguage,
    Status,
    Demographic,
    ContentRating,
}

impl PinnedField {
    /// The `work` column, which is also the `work_field_pin.field` value.
    pub fn column(self) -> &'static str {
        match self {
            PinnedField::Author => "author",
            PinnedField::Artist => "artist",
            PinnedField::Year => "year",
            PinnedField::OriginalLanguage => "original_language",
            PinnedField::Status => "status",
            PinnedField::Demographic => "demographic",
            PinnedField::ContentRating => "content_rating",
        }
    }

    /// SQL for the bound pin value in the column's own type.
    fn typed_value(self) -> &'static str {
        match self {
            PinnedField::Year => "CAST(? AS INTEGER)",
            _ => "?",
        }
    }
}

/// Pin `field` of a work to `value` (`None` pins it empty): the column takes the value
/// now and keeps it across later syncs. The source's own value is remembered so
/// [`unpin_work_field`] can restore it.
pub async fn pin_work_field(
    conn: &mut sqlx::SqliteConnection,
    work_id: &str,
    field: PinnedField,
    value: Option<&str>,
) -> Result<()> {
    let column = field.column();
    let now = Utc::now().to_rfc3339();
    // The pin first: with it in place the UPDATE below already agrees with it, so the
    // restore trigger stays quiet. A re-pin keeps the remembered source value.
    sqlx::query(&format!(
        "INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at) \
         VALUES (?1, ?2, ?3, (SELECT CAST({column} AS TEXT) FROM work WHERE id = ?1), ?4) \
         ON CONFLICT(work_id, field) DO UPDATE SET \
             value = excluded.value, pinned_at = excluded.pinned_at"
    ))
    .bind(work_id)
    .bind(column)
    .bind(value)
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(&format!(
        "UPDATE work SET {column} = {}, updated_at = ? WHERE id = ?",
        field.typed_value()
    ))
    .bind(value)
    .bind(&now)
    .bind(work_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Remove a pin and give the column back to the source: it takes the last value a source
/// wrote, and later syncs update it again. A no-op for a field that is not pinned.
pub async fn unpin_work_field(
    conn: &mut sqlx::SqliteConnection,
    work_id: &str,
    field: PinnedField,
) -> Result<()> {
    let column = field.column();
    let pinned: Option<Option<String>> = sqlx::query_scalar(
        "DELETE FROM work_field_pin WHERE work_id = ? AND field = ? RETURNING source_value",
    )
    .bind(work_id)
    .bind(column)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(source_value) = pinned else {
        return Ok(());
    };
    sqlx::query(&format!(
        "UPDATE work SET {column} = {}, updated_at = ? WHERE id = ?",
        field.typed_value()
    ))
    .bind(source_value)
    .bind(Utc::now().to_rfc3339())
    .bind(work_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The `work` columns an admin has pinned on this work.
pub async fn pinned_work_fields(pool: &Database, work_id: &str) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT field FROM work_field_pin WHERE work_id = ? ORDER BY field")
            .bind(work_id)
            .fetch_all(pool)
            .await?,
    )
}

/// Admin: remove an alt-title from a work — matched by its normalized key OR its exact
/// raw text — then rebuild the work's token index (tokens can be shared across
/// aliases, so a per-token delete is unsafe; rebuild from what remains).
pub async fn remove_work_alias(pool: &Database, work_id: &str, raw_title: &str) -> Result<()> {
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
    pool: &Database,
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
    pool: &Database,
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

/// The engine's NSFW flag for one source as last recorded in `source_extension`.
/// `None` means there is NO ROW — the source has not been recorded yet (a fresh server,
/// an extension installed since the last record pass, an extension with no repo). That
/// is "unknown", never "safe": the caller must ask the engine rather than default.
pub async fn source_extension_nsfw(pool: &Database, source_id: &str) -> Result<Option<bool>> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT is_nsfw FROM source_extension WHERE source_id = ?")
            .bind(source_id)
            .fetch_optional(pool)
            .await?
            .map(|v| v != 0),
    )
}

/// Whether an admin pinned this whole source adult (`markSourceNsfw`, migration 0114).
pub async fn source_nsfw_pinned(pool: &Database, source_id: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT 1 FROM source_nsfw_pin WHERE source_id = ?")
            .bind(source_id)
            .fetch_optional(pool)
            .await?
            .is_some(),
    )
}

/// Set or clear the admin's "this source is adult" pin. Idempotent both ways.
pub async fn set_source_nsfw_pin(pool: &Database, source_id: &str, pinned: bool) -> Result<()> {
    if pinned {
        sqlx::query(
            "INSERT INTO source_nsfw_pin (source_id, created_at) VALUES (?, ?) \
             ON CONFLICT(source_id) DO NOTHING",
        )
        .bind(source_id)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
    } else {
        sqlx::query("DELETE FROM source_nsfw_pin WHERE source_id = ?")
            .bind(source_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Mark the works behind these MangaDex ids as metadata-attempted (H1), so the
/// backfill advances past ids MangaDex didn't return. Idempotent; a no-op for ids
/// with no `mangadex` source_series.
pub async fn mark_metadata_synced(pool: &Database, mangadex_ids: &[String]) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    for mid in mangadex_ids {
        sqlx::query(
            "UPDATE work SET metadata_synced_at = ?, mangadex_details_synced_at = ? WHERE id IN \
             (SELECT work_id FROM source_series WHERE source_type = 'mangadex' AND source_key = ?)",
        )
        .bind(&now)
        .bind(&now)
        .bind(mid)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Mark the works behind these MangaDex ids as full-cover-fetched (F2), so the
/// cover backfill advances past ids `/cover` returned nothing for. Idempotent.
pub async fn mark_covers_synced(pool: &Database, mangadex_ids: &[String]) -> Result<()> {
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
pub async fn mark_work_nsfw(pool: &Database, work_id: &str) -> Result<()> {
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
    pool: &Database,
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
    pool: &Database,
    covers: Option<&Database>,
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
    pub artist: Option<String>,
    pub original_language: Option<String>,
    pub description: Option<String>,
    pub cover_phash: Option<String>,
    /// Whether the work carried a MangaDex source. The gate refuses a loose title match
    /// between two anchored works, so a work gaining its anchor under the gate (another
    /// merge landing first) must abort the merge.
    pub mangadex_anchored: bool,
    /// The duplicate key (`work_alias.match_key`) the gate found this work under, when
    /// the decision rested on it. Re-checked so an alias removed under the gate
    /// (`removeSeriesAltTitle`) cannot leave a merge standing on evidence that is gone.
    /// Not a column: never read back, only verified.
    #[sqlx(skip)]
    pub match_key: Option<String>,
}

/// As [`merge_works_ex`], but with an optional OPTIMISTIC-CONCURRENCY precondition:
/// `(expected_source, expected_target)` is the identity snapshot the caller's gate was
/// evaluated against, re-read inside this transaction and required to still match.
///
/// WHY. `merge_works` physically DELETEs the losing work — it is irreversible. The
/// automated consolidation gate (`pair::assess`) reads the work's titles, credits, year,
/// language, description and cover hash on a snapshot taken OUTSIDE any transaction, and
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
    pool: &Database,
    covers: Option<&Database>,
    source_work: &str,
    target_work: &str,
    expected: Option<(&WorkIdentity, &WorkIdentity)>,
) -> Result<MergeOutcome> {
    merge_works_guarded(pool, covers, source_work, target_work, expected, None).await
}

/// Claim a waiting review and merge under the same write lock. A rejection or
/// metadata edit that wins the lock prevents the automatic merge.
pub async fn merge_pending_candidate_checked(
    pool: &Database,
    covers: Option<&Database>,
    source_work: &str,
    target_work: &str,
    expected: (&WorkIdentity, &WorkIdentity),
    candidate_id: &str,
) -> Result<MergeOutcome> {
    merge_works_guarded(
        pool,
        covers,
        source_work,
        target_work,
        Some(expected),
        Some(candidate_id),
    )
    .await
}

async fn merge_works_guarded(
    pool: &Database,
    covers: Option<&Database>,
    source_work: &str,
    target_work: &str,
    expected: Option<(&WorkIdentity, &WorkIdentity)>,
    candidate_id: Option<&str>,
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
            let mut now: WorkIdentity = sqlx::query_as(
                "SELECT w.primary_title, w.year, w.author, w.artist, w.original_language, \
                        w.description, w.cover_phash, \
                        EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                                AND ss.source_type = 'mangadex') AS mangadex_anchored \
                 FROM work w WHERE w.id = ?",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if let Some(key) = &want.match_key {
                let still_named: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM work_alias WHERE work_id = ? AND match_key = ?)",
                )
                .bind(id)
                .bind(key)
                .fetch_one(&mut *tx)
                .await?;
                if still_named {
                    now.match_key = Some(key.clone());
                }
            }
            if now != *want {
                anyhow::bail!("merge precondition failed: work {id} changed under the gate");
            }
        }
    }

    if expected.is_some() {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM work_split WHERE work_a = MIN(?, ?) AND work_b = MAX(?, ?)) \
             OR EXISTS(SELECT 1 FROM merge_candidate mc JOIN source_series ss ON ss.id = mc.source_series_id \
             WHERE mc.status = 'rejected' AND ((ss.work_id = ? AND mc.candidate_work_id = ?) \
             OR (ss.work_id = ? AND mc.candidate_work_id = ?)))")
            .bind(source_work).bind(target_work).bind(source_work).bind(target_work)
            .bind(source_work).bind(target_work).bind(target_work).bind(source_work)
            .fetch_one(&mut *tx).await?;
        if blocked {
            anyhow::bail!("merge precondition failed: pair was rejected or split");
        }
    }
    if let Some(id) = candidate_id {
        let claim = sqlx::query(
            "UPDATE merge_candidate SET status = 'confirmed', resolved_at = ? \
             WHERE id = ? AND status = 'pending' AND candidate_work_id = ? \
             AND source_series_id IN (SELECT id FROM source_series WHERE work_id = ?)",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .bind(target_work)
        .bind(source_work)
        .execute(&mut *tx)
        .await?;
        if claim.rows_affected() != 1 {
            anyhow::bail!("merge precondition failed: review is no longer pending for this pair");
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
    sqlx::query("INSERT OR IGNORE INTO work_tracker_link (work_id, provider, external_id) SELECT ?, provider, external_id FROM work_tracker_link WHERE work_id = ? AND NOT EXISTS (SELECT 1 FROM work WHERE id = ? AND mangadex_details_synced_at IS NOT NULL)")
        .bind(target_work).bind(source_work).bind(target_work).execute(&mut *tx).await?;
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
        "INSERT OR IGNORE INTO work_tag (work_id, tag, ord, source, tag_key, tag_group) \
         SELECT ?, tag, ord + (SELECT COALESCE(MAX(ord), -1) + 1 FROM work_tag WHERE work_id = ?), \
                'admin', tag_key, tag_group \
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

    // A user can have both copies in their library. Preserve the favourite from
    // either copy, retain the survivor's explicit shelf (falling back to the
    // losing copy's shelf), and keep the original add date. UPDATE OR IGNORE
    // followed by deleting the loser used to silently discard these preferences.
    sqlx::query(
        "INSERT INTO user_library (user_id, series_id, created_at, status, is_favorite) \
         SELECT user_id, ?, created_at, status, is_favorite FROM user_library \
         WHERE series_id = ? \
         ON CONFLICT(user_id, series_id) DO UPDATE SET \
           created_at = MIN(user_library.created_at, excluded.created_at), \
           status = COALESCE(user_library.status, excluded.status), \
           is_favorite = MAX(user_library.is_favorite, excluded.is_favorite)",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?;

    for (table, col) in [
        ("merge_candidate", "candidate_work_id"),
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
    // Delete the folded library rows and any colliding reviews so the imminent
    // work delete can't orphan a row pointing at a nonexistent work. Reviews keep
    // the survivor's review when the same user reviewed both copies.
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
        "work_tracker_link",
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

    // BEFORE the delete. `release_event.work_id` cascades, so the losing work's
    // first-seen history is one statement away from being erased — and the earliest time
    // per chapter key is exactly what stops a merge re-announcing the merged-in work's
    // whole back catalogue on /updates.
    ledger::merge_release_events(&mut tx, source_work, target_work).await?;

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
pub async fn redirect_work_id(pool: &Database, old_id: &str) -> Result<Option<String>> {
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
pub async fn reclaim_cover_blob(covers: &Database, work_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM work_cover_blob WHERE work_id = ?")
        .bind(work_id)
        .execute(covers)
        .await?;
    Ok(())
}

/// One of a work's `source_series` mappings, in the shape the admin SPLIT picker lists
/// it: what THIS source calls the series, how much of it we hold, and the row's own id.
///
/// Distinct from `graphql::WorkSource`, which answers "how does a native client FETCH
/// this source" and carries no primary key — and a key is the whole point here, because
/// `(source_type, source_key)` is only unique in combination with `source_id` (the
/// table's `UNIQUE (source_type, source_id, source_key)`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SourceSeriesRow {
    /// `source_series.id` — the detach key [`split_source_series`] takes.
    pub id: String,
    pub source_type: String,
    pub source_id: String,
    pub source_key: String,
    pub source_url: Option<String>,
    pub is_nsfw: i64,
    pub lang: Option<String>,
    /// How this source titles the series: the cached Suwayomi title where there is one,
    /// else the work's own title. A MangaDex mapping has no per-source title on disk —
    /// the mirror's title IS the work's — so it necessarily falls back.
    pub title: String,
    /// Chapters we hold FROM THIS SOURCE (`chapter` is keyed by `source_series_id`, which
    /// is exactly why detaching the row carries its whole run with it).
    pub chapter_count: i64,
    /// Extension coordinates, for the picker's source logo. Both `None` for a MangaDex
    /// mapping and for a source whose extension isn't catalogued.
    pub pkg_name: Option<String>,
    pub repo_url: Option<String>,
}

/// Every `source_series` mapping on a work, newest-relevant first — the admin SPLIT
/// picker's list.
///
/// ORDER IS PART OF THE CONTRACT, not a display nicety: [`split_source_series`] names the
/// new work after the FIRST detached row in this order, and the picker shows the admin
/// that same name before they confirm. Same ordering expression as
/// `graphql::load_work_sources` (MangaDex anchor first, then by recency) so one work's
/// sources don't appear in two different orders on two admin surfaces.
pub async fn work_source_series_rows(
    pool: &Database,
    work_id: &str,
) -> Result<Vec<SourceSeriesRow>> {
    let rows = sqlx::query_as::<_, SourceSeriesRow>(
        "SELECT ss.id, ss.source_type, ss.source_id, ss.source_key, ss.source_url, ss.is_nsfw, \
                COALESCE(NULLIF(sy.lang, ''), NULLIF(se.lang, '')) AS lang, \
                COALESCE(NULLIF(sy.title, ''), w.title_override, w.primary_title, '') AS title, \
                (SELECT COUNT(*) FROM chapter c WHERE c.source_series_id = ss.id) AS chapter_count, \
                se.pkg_name, se.repo_url \
           FROM source_series ss \
           JOIN work w ON w.id = ss.work_id \
           LEFT JOIN suwayomi_series sy \
                  ON ss.source_type = 'suwayomi' AND sy.id = CAST(ss.source_key AS INTEGER) \
           LEFT JOIN source_extension se ON se.source_id = ss.source_id \
          WHERE ss.work_id = ? \
          ORDER BY (ss.source_type = 'mangadex') DESC, ss.last_seen DESC",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// `(description, author, artist, status, lang)` as `suwayomi_series` caches them — the
/// only metadata a split's new work inherits from anywhere other than its own title.
type SuwayomiMeta = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Outcome of detaching sources off a work onto a new one (admin split).
#[derive(Debug, Clone)]
pub struct SplitOutcome {
    /// The freshly minted canonical work.
    pub new_work_id: String,
    /// Where the reader NAVIGATES for it — see [`split_source_series`].
    pub new_reader_id: String,
    /// Its `primary_title`, taken from the first detached source's own title.
    pub title: String,
    /// How many `source_series` rows moved.
    pub moved_sources: u64,
}

/// Detach `source_series_ids` off `work_id` onto a NEW canonical work — the admin fix for
/// two different series that were merged together because they share a title.
///
/// NOT AN UNDO OF [`merge_works`], and nothing can be: that merge DELETEs the losing work
/// row and records only `(old_id, new_id)` in `work_redirect`, having already dropped the
/// colliding reviews/library rows and SUMMED the view counters. The pre-merge state is not
/// on disk. What this does instead is the thing that actually repairs the catalogue, and
/// it is cheap and lossless in the direction that matters: `chapter` is keyed by
/// `source_series_id`, not by work, so re-pointing the mapping carries its ENTIRE chapter
/// run with it and writes no chapter rows at all. Nothing is deleted here.
///
/// WHAT DOES NOT MOVE: reviews, library entries, reading progress and view counts. They
/// hang off the work and carry no per-source attribution, so there is no honest way to
/// decide which side of the split they belong to; they stay on the original. The admin UI
/// says so before confirming rather than letting it be discovered afterwards.
///
/// THE LAST SOURCE CANNOT BE DETACHED. A work with no `source_series` row is excluded from
/// both `browse_catalogue` and `work_fts` (see [`refresh_browse_catalogue`] and
/// [`refresh_work_fts`]) — it would vanish from Browse and from search while still
/// existing in the database, taking its reviews and library entries with it into a page
/// nothing links to.
///
/// The new work is titled from the FIRST detached row in [`work_source_series_rows`]'
/// order, which is the name the picker showed the admin. It is deliberately given only
/// metadata the DETACHED SOURCE itself carries (its title, description, author, artist,
/// status) and NONE of the original work's identity columns beyond the NSFW gating. That
/// is not squeamishness about copying: `consolidate_gate` merges two works that share a
/// normalized primary title as soon as year / author / cover pHash corroborates, so
/// copying the original's `year` and `cover_phash` onto a work that by construction shares
/// its title would hand the automated sweep everything it needs to undo this split. The
/// `work_split` row below is the belt to that braces.
///
/// NSFW is the one thing inherited, and inherited CONSERVATIVELY: the new work takes the
/// original's effective flag and content rating, OR'd with the detached rows' own
/// `is_nsfw`. Under-gating a split-off work would expose it to anonymous viewers on the
/// strength of an admin action that was about identity, not about rating.
pub async fn split_source_series(
    pool: &Database,
    work_id: &str,
    source_series_ids: &[String],
    actor_user_id: Option<&str>,
) -> Result<SplitOutcome> {
    if source_series_ids.is_empty() {
        anyhow::bail!("pick at least one source to detach");
    }
    let all = work_source_series_rows(pool, work_id).await?;
    if all.is_empty() {
        anyhow::bail!("no such work, or it has no sources: {work_id}");
    }
    let wanted: std::collections::HashSet<&str> =
        source_series_ids.iter().map(|s| s.as_str()).collect();
    let (detached, kept): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|r| wanted.contains(r.id.as_str()));
    if detached.len() != wanted.len() {
        // Named rather than counted: the usual cause is a stale picker (another admin
        // merged or split this work since it loaded), and the id says which row moved.
        let found: std::collections::HashSet<&str> =
            detached.iter().map(|r| r.id.as_str()).collect();
        let missing: Vec<&str> = source_series_ids
            .iter()
            .map(|s| s.as_str())
            .filter(|s| !found.contains(s))
            .collect();
        anyhow::bail!(
            "not a source of this work (reload and try again): {}",
            missing.join(", ")
        );
    }
    if kept.is_empty() {
        anyhow::bail!(
            "at least one source has to stay — a work with none disappears from Browse and search"
        );
    }

    let title = detached[0].title.trim().to_string();
    if title.is_empty() {
        anyhow::bail!("the detached source has no title to name the new series with");
    }
    // The reader id, by the SAME rule `browse_catalogue` applies (migration 0069): a work
    // with a MangaDex anchor navigates to its `w_…` page, and anything else navigates to
    // the numeric Suwayomi id of its lowest-id mapping — which is the row 0069's `sw`
    // sub-select picks with `MIN(ss.id)`. Getting this wrong produces a link that 404s:
    // `canonical_series` rejects a work with no MangaDex anchor outright.
    let new_work_id = new_id("w_");
    let new_reader_id = if detached.iter().any(|r| r.source_type == "mangadex") {
        new_work_id.clone()
    } else {
        detached
            .iter()
            .filter(|r| r.source_type == "suwayomi")
            .min_by(|a, b| a.id.cmp(&b.id))
            .map(|r| r.source_key.clone())
            .unwrap_or_else(|| new_work_id.clone())
    };

    let now = Utc::now().to_rfc3339();
    // IMMEDIATE for the reason every writer in this file takes it: the transaction reads
    // (the parent work's gating columns, the source's cached metadata) before it writes,
    // and a DEFERRED read-then-write upgrade fails with SQLITE_BUSY_SNAPSHOT, which
    // `busy_timeout` structurally cannot retry (`db::is_locked_error`).
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    // Re-read the parent under the write lock: the gating columns are what the new work
    // inherits, and `updateSeriesMetadata` is not single-flighted against this.
    let (parent_rating, parent_nsfw): (Option<String>, i64) = sqlx::query_as(
        "SELECT content_rating, COALESCE(is_nsfw_override, is_nsfw) FROM work WHERE id = ?",
    )
    .bind(work_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| anyhow::anyhow!("no such work: {work_id}"))?;
    let nsfw = if parent_nsfw != 0 || detached.iter().any(|r| r.is_nsfw != 0) {
        1_i64
    } else {
        0_i64
    };

    // The detached source's OWN metadata, where it has any on disk. A MangaDex mapping
    // has none (its metadata IS the work's), so those columns stay NULL rather than being
    // copied off the work — see the doc comment on why copying is unsafe here.
    let source_meta: Option<SuwayomiMeta> =
        match detached.iter().find(|r| r.source_type == "suwayomi") {
            Some(r) => {
                sqlx::query_as(
                    "SELECT description, author, artist, status, lang FROM suwayomi_series \
                      WHERE id = CAST(? AS INTEGER)",
                )
                .bind(&r.source_key)
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };
    // All-None when the detached set is MangaDex-only, or when the cached Suwayomi row is
    // gone: the new work then carries title + gating and nothing else, which is the safe
    // direction (see the doc comment).
    let (description, author, artist, status, lang) = source_meta.unwrap_or_default();

    sqlx::query(
        "INSERT INTO work \
           (id, primary_title, primary_lang, description, status, content_rating, is_nsfw, \
            author, artist, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&new_work_id)
    .bind(&title)
    .bind(&lang)
    .bind(&description)
    .bind(&status)
    .bind(&parent_rating)
    .bind(nsfw)
    .bind(&author)
    .bind(&artist)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    // The move itself — one UPDATE, no chapter writes, nothing deleted.
    let mut moved = 0_u64;
    for r in &detached {
        moved += sqlx::query("UPDATE source_series SET work_id = ? WHERE id = ? AND work_id = ?")
            .bind(&new_work_id)
            .bind(&r.id)
            .bind(work_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    if moved as usize != detached.len() {
        // A concurrent merge/split moved a row out from under the read above. Roll back
        // rather than mint a half-populated work.
        anyhow::bail!("sources changed while splitting — reload and try again");
    }

    // A detached MangaDex mapping takes its UUID's identity row with it. `work_external_id`
    // is `PRIMARY KEY (provider, external_id)` — globally unique, not per-work — so leaving
    // it behind would claim the new work's own anchor for the work it was detached from,
    // and `load_canonical_work` would read a MangaDex id that no longer maps to a mapping.
    for r in detached.iter().filter(|r| r.source_type == "mangadex") {
        sqlx::query(
            "UPDATE work_external_id SET work_id = ? \
              WHERE provider = 'mangadex' AND external_id = ? AND work_id = ?",
        )
        .bind(&new_work_id)
        .bind(&r.source_key)
        .bind(work_id)
        .execute(&mut *tx)
        .await?;
    }

    // Release events follow the row, but ONLY for chapter keys the detached rows were the
    // sole source of on the old work — the same rule `phase_f::split_mismatched` applies,
    // and for the same reason: the ledger's law is earliest-anywhere, so a key the old
    // work still has from a remaining source keeps its (possibly earlier) event there.
    //
    // The `source_series` UPDATE above has already committed within this transaction, so
    // `s.work_id = <old>` no longer matches the detached rows — the NOT EXISTS below asks
    // exactly "does a REMAINING source still carry this chapter key".
    for r in &detached {
        sqlx::query(
            "INSERT OR IGNORE INTO release_event \
                 (work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at) \
             SELECT ?, e.chapter_key, e.first_seen_at, e.first_source_series_id, e.label, \
                    e.announce_at \
               FROM release_event e \
              WHERE e.work_id = ? AND e.first_source_series_id = ? \
                AND NOT EXISTS (SELECT 1 FROM chapter c \
                                  JOIN source_series s ON s.id = c.source_series_id \
                                 WHERE s.work_id = ? AND c.chapter_key = e.chapter_key)",
        )
        .bind(&new_work_id)
        .bind(work_id)
        .bind(&r.id)
        .bind(work_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM release_event \
              WHERE work_id = ? AND first_source_series_id = ? \
                AND NOT EXISTS (SELECT 1 FROM chapter c \
                                  JOIN source_series s ON s.id = c.source_series_id \
                                 WHERE s.work_id = ? AND c.chapter_key = release_event.chapter_key)",
        )
        .bind(work_id)
        .bind(&r.id)
        .bind(work_id)
        .execute(&mut *tx)
        .await?;
    }

    // Index the new work's title so it is searchable under its own name (H9 keeps the
    // token index in lockstep). The original KEEPS its copy of that alias: the merge that
    // caused this may have folded a genuine alt-title, and dropping it would be a second
    // guess about identity on top of the one the admin already made.
    insert_aliases(
        &mut tx,
        &new_work_id,
        &[Alias {
            raw: title.clone(),
            lang: lang.clone(),
        }],
    )
    .await?;

    // "These two are NOT duplicates" — the memory that stops `consolidateExactDuplicates`
    // silently re-merging what was just separated (migration 0098). Written as the ordered
    // pair because the sweep chooses its own survivor.
    let (a, b) = if work_id < new_work_id.as_str() {
        (work_id, new_work_id.as_str())
    } else {
        (new_work_id.as_str(), work_id)
    };
    sqlx::query(
        "INSERT OR IGNORE INTO work_split (work_a, work_b, split_at, split_by) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(a)
    .bind(b)
    .bind(&now)
    .bind(actor_user_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Both works' derived caches are now wrong in ways a reader can see, so they are
    // re-projected HERE rather than left to the periodic rebuild — the opposite of the
    // choice `mergeWorks` and `phase_f::split_mismatched` make, and the difference is
    // scope: those re-project nothing because they run over hundreds of works at a time
    // (measured: 486 merges take ~5 s without the per-work projection and made no
    // measurable progress in 90 s with it). This is ONE admin action on exactly two works,
    // and both of them are wrong until it runs: the new work has no `browse_catalogue` or
    // `work_fts` row at all, and the original may be pointing at a `reader_id` that just
    // moved. Best-effort — the split is already durable, so a projection failure is logged,
    // not returned.
    for id in [work_id, new_work_id.as_str()] {
        if let Err(e) = reproject_work_caches(pool, id).await {
            tracing::warn!(work_id = %id, error = %e, "split: cache re-projection failed (the next refresh will fix it)");
        }
    }

    Ok(SplitOutcome {
        new_work_id,
        new_reader_id,
        title,
        moved_sources: moved,
    })
}

/// Is this pair of works one an admin explicitly SPLIT apart (migration 0098)? Order
/// doesn't matter — `work_split` stores the pair sorted and this sorts to match.
///
/// Consulted by EVERY path that can merge two works without an admin naming both of them:
/// `dedup::resolve_ex` (the post-ingest reconcile sweep — the one that gets there first,
/// because a split-off work is Suwayomi-only and an exact title hit there auto-merges with
/// no corroboration at all), `consolidate_exact_duplicates_from` (the alias-cluster sweep)
/// and the auto-merge inside `addSeriesAltTitle`. The two halves of a split share a title
/// BY CONSTRUCTION — that shared title is what caused the bad merge in the first place — so
/// without this they are re-merged on the next pass, destructively: `merge_works` deletes
/// the loser.
pub async fn is_split_pair(pool: &Database, one: &str, other: &str) -> Result<bool> {
    let (a, b) = if one < other {
        (one, other)
    } else {
        (other, one)
    };
    let hit: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM work_split WHERE work_a = ? AND work_b = ?")
            .bind(a)
            .bind(b)
            .fetch_optional(pool)
            .await?;
    Ok(hit.is_some())
}

/// Re-project ONE work's derived caches — `feed_series_updates.reader_id`,
/// `browse_catalogue` and `work_fts` — without waiting for the periodic rebuild.
///
/// Every statement here is the corresponding wholesale refresh narrowed to one work, so
/// the two converge by construction rather than by a field mapping that has to be kept in
/// agreement: the `browse_catalogue` upsert runs [`browse_catalogue_select`] itself, and
/// the `work_fts` write is [`refresh_work_fts`]'s INSERT with a `w.id = ?` added.
///
/// `reader_id` needs the explicit UPDATE because `browse_catalogue` COPIES it from
/// `feed_series_updates` for a work with no MangaDex anchor, so a stale value there
/// propagates rather than being recomputed. It goes stale in exactly one situation, which
/// is the one this function exists for: [`split_source_series`] moved the Suwayomi mapping
/// the id was derived from onto another work, leaving the original's Browse card linking
/// to the series it just detached.
///
/// NOT called from the hot path. Two `work_fts` statements scan the FTS table (its
/// `work_id` column is `UNINDEXED`), which is fine for a one-shot admin action and would
/// not be for a per-scan write.
pub async fn reproject_work_caches(pool: &Database, work_id: &str) -> Result<()> {
    // (1) `reader_id`, by the rule both feed writers apply — the MangaDex anchor wins, else
    //     the numeric Suwayomi id. `MIN(ss.id)` (via ORDER BY ... LIMIT 1) matches the
    //     `sw` sub-select in `browse_catalogue_select`, so the two surfaces agree on WHICH
    //     Suwayomi mapping names the work. COALESCE keeps the old value rather than writing
    //     NULL into a NOT NULL column if the work somehow has neither.
    sqlx::query(
        "UPDATE feed_series_updates SET reader_id = COALESCE( \
             CASE WHEN EXISTS (SELECT 1 FROM source_series md \
                                WHERE md.work_id = feed_series_updates.work_id \
                                  AND md.source_type = 'mangadex') \
                  THEN feed_series_updates.work_id \
                  ELSE (SELECT ss.source_key FROM source_series ss \
                         WHERE ss.work_id = feed_series_updates.work_id \
                           AND ss.source_type = 'suwayomi' \
                         ORDER BY ss.id LIMIT 1) END, \
             feed_series_updates.reader_id) \
          WHERE work_id = ?",
    )
    .bind(work_id)
    .execute(pool)
    .await?;

    // (2) `browse_catalogue`, the rebuild's own upsert narrowed to this work — same
    //     columns, same conflict clause, same `IS NOT` change guard.
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
    let select = browse_catalogue_select_for(" AND w.id = ?");
    sqlx::query(&format!(
        "INSERT INTO browse_catalogue ({BROWSE_CATALOGUE_COLUMNS}) {select} \
         ON CONFLICT(work_id) DO UPDATE SET {set} WHERE {changed}"
    ))
    .bind(work_id)
    .execute(pool)
    .await?;

    // `comic_type` is not in the SELECT (see `fill_comic_types`), and a NULL type is
    // invisible to Browse's format tabs — so a row this call just INSERTED would sit out
    // of every tab until the next wholesale rebuild.
    let mut conn = pool.write().acquire().await?;
    fill_comic_types(&mut conn, "browse_catalogue", Some(work_id)).await?;
    drop(conn);

    // (3) `work_fts`. A virtual FTS5 table has no unique key to upsert against, so this is
    //     delete-then-insert; the INSERT is `refresh_work_fts`'s, scoped, and it carries
    //     that function's exclusion rule (a title to label, a source to open) verbatim, so
    //     a work that no longer qualifies is simply left deleted.
    sqlx::query("DELETE FROM work_fts WHERE work_id = ?")
        .bind(work_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO work_fts (work_id, chapters, title, aliases) \
         SELECT w.id, COALESCE(cc.n, 0), \
                COALESCE(w.title_override, w.primary_title, ''), \
                COALESCE((SELECT group_concat(a.raw_title, ' ') \
                          FROM work_alias a WHERE a.work_id = w.id), '') \
         FROM work w \
         LEFT JOIN (SELECT ss.work_id, COUNT(*) AS n FROM chapter ch \
                    JOIN source_series ss ON ss.id = ch.source_series_id \
                    WHERE ss.work_id = ? GROUP BY ss.work_id) cc ON cc.work_id = w.id \
         WHERE w.id = ? \
           AND COALESCE(w.title_override, w.primary_title, '') <> '' \
           AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id)",
    )
    .bind(work_id)
    .bind(work_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// One raw chapter row from any of a work's sources (S2 aggregation input).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkChapterRow {
    /// The chapter's number when it has one. `None` is an UNNUMBERED chapter (a oneshot,
    /// an `Extra`) — not chapter 0, which is a real and common first chapter. Callers must
    /// group on [`Self::key`] rather than inventing a number for these.
    pub number: Option<f64>,
    /// What to print for this chapter: "45", "10.5", "Oneshot". Resolved once by
    /// [`crate::chapter_label::chapter_display`] so every surface says the same thing.
    pub label: String,
    /// The cross-source grouping key. `round(number * 100)` for numbered chapters (the key
    /// `chapter_override` already uses), `x:<chapter id>` for unnumbered ones so a work's
    /// several oneshots stay several rows instead of colliding on `0`.
    pub key: String,
    pub title: Option<String>,
    /// When THIS source released this chapter, ISO-8601, or `None` when the source gave us
    /// no date (an undated Suwayomi upload; a MangaDex row with neither timestamp).
    ///
    /// `COALESCE(readable_at, published_at)` — the SAME clock the release ledger seeds from
    /// (`ledger::RELEASED_AT_SQL`), deliberately, and for the reason recorded there: MangaDex
    /// stamps external chapters `publishAt = 2037-12-31`, and sampled bilibili chapters are
    /// `readableAt` two weeks BEFORE their `publishAt`, so `published_at` alone is not the
    /// release. Taking a different clock here would make a chapter's date on the series page
    /// disagree with the same chapter's position in `/updates`.
    pub released_at: Option<String>,
    pub source_type: String,
    pub source_id: String,
    pub suwayomi_manga_id: Option<String>,
    pub chapter_id: String,
    /// The scanlation group(s) of THIS copy. For the MangaDex half: the group names joined
    /// with `" & "`, `"No Group"` when MangaDex lists none, or `None` when unknown — the
    /// backfill's `''` ("asked, could not learn") is folded to `None` in SQL so no surface
    /// prints an empty label. Several rows sharing one `key` are several uploads of the same
    /// chapter, and this is what tells them apart.
    pub scanlator: Option<String>,
    /// Set when THIS copy is hosted off-site (MangaDex externals; always `None` for
    /// Suwayomi). Per copy, not per number: one group's upload of a chapter can be a
    /// MangaPlus redirect while another group's is ordinary pages.
    pub external_url: Option<String>,
    /// The volume THIS copy was published in ("3", "12.5"), as MangaDex labels it; `None`
    /// when unknown. Suwayomi sources expose no volume, so their half is always `None`.
    /// Blank is folded to `None` (SQL `NULLIF(TRIM(…), '')`) so no surface prints
    /// "Volume ".
    pub volume: Option<String>,
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
    pool: &Database,
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

/// THE TEST ORACLE for [`work_source_chapters`] — the hand-written two-branch version
/// that WAS the live path until the Phase B switchover on 2026-07-30.
///
/// It reads the Suwayomi half out of the `suwayomi_chapter` cache and the MangaDex half
/// out of `chapter`, and merges them in Rust. `the_spine_query_matches_the_two_branch_version`
/// is only evidence for the switch while the two sides are genuinely different code over
/// genuinely different tables, so this is kept, and kept reading `suwayomi_chapter`.
/// Deleting it (or re-expressing it over the spine) would turn that test into a tautology
/// that passes no matter what the live query does.
///
/// `#[cfg(test)]` rather than `#[allow(dead_code)]`: the production binary must not carry a
/// second answer to "what chapters does this work have". Having exactly one is the whole
/// point of Phase B.
#[cfg(test)]
async fn work_source_chapters_two_branch(
    pool: &Database,
    work_id: &str,
) -> Result<Vec<WorkChapterRow>> {
    let auth = authoritative_suwayomi_mappings(pool, work_id).await?;
    let mut rows: Vec<WorkChapterRow> = Vec::new();
    // Suwayomi chapters from the authoritative mapping of each source.
    for m in &auth {
        let Ok(manga_id) = m.source_key.parse::<i64>() else {
            continue;
        };
        let chapters = sqlx::query_as::<_, (f64, String, i64, Option<String>, Option<String>)>(
            "SELECT chapter_number, name, id, scanlator, upload_date \
               FROM suwayomi_chapter WHERE manga_id = ?",
        )
        .bind(manga_id)
        .fetch_all(pool)
        .await?;
        for (number, title, id, scanlator, upload_date) in chapters {
            // One label rule for both halves (Phase A2). Suwayomi's structured
            // `chapter_number` is right on ~99.85% of rows; the name is the fallback that
            // catches the rest, and the sanity clamp inside `chapter_display` is what stops
            // `Ch.99999999` and `Ch.20240120` reaching a series page.
            let label = crate::chapter_label::chapter_display(Some(number), None, Some(&title));
            let chapter_id = id.to_string();
            rows.push(WorkChapterRow {
                number: label.number(),
                key: label.key(&chapter_id),
                label: label.text(),
                title: Some(title),
                source_type: "suwayomi".into(),
                source_id: m.source_id.clone(),
                suwayomi_manga_id: Some(m.source_key.clone()),
                chapter_id,
                scanlator,
                external_url: None,
                // Converted here rather than read from the spine, so this stays a genuinely
                // INDEPENDENT implementation: it goes epoch-millis → ISO through the same
                // helper `spine::drain_suwayomi_series` uses, which is exactly what the
                // equivalence assertion is meant to check the drain got right.
                released_at: suwayomi_upload_date_to_iso(upload_date.as_deref()),
                volume: None,
            });
        }
    }
    // MangaDex mirror (English). No "looks numeric" filter, for the F2 reason spelled out
    // on `work_source_chapters` — 23,254 chapters carry a NULL or non-numeric number and
    // 21,422 works have nothing else. The raw string comes back untouched and
    // `chapter_display` decides, so "Extra" becomes an UNNUMBERED row keyed by its own
    // chapter id instead of either vanishing or colliding onto 0.
    let md = sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<String>,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT c.number, c.title, ss.source_id, c.external_id, \
                COALESCE(c.readable_at, c.published_at), c.scanlator, c.external_url, \
                c.volume \
         FROM source_series ss \
         JOIN chapter c ON c.source_series_id = ss.id \
         WHERE ss.work_id = ? AND ss.source_type = 'mangadex' AND c.lang = 'en'",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    for (number, title, source_id, external_id, released_at, scanlator, external_url, volume) in md
    {
        let label =
            crate::chapter_label::chapter_display(None, number.as_deref(), title.as_deref());
        rows.push(WorkChapterRow {
            number: label.number(),
            key: label.key(&external_id),
            label: label.text(),
            title,
            source_type: "mangadex".into(),
            source_id,
            suwayomi_manga_id: None,
            chapter_id: external_id,
            // The `''` → None fold, done in Rust here and in SQL in the live query, so the
            // equivalence test checks the two spellings agree.
            scanlator: scanlator.filter(|s| !s.is_empty()),
            external_url,
            released_at,
            volume: volume
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        });
    }
    Ok(rows)
}

/// Every chapter across a work's sources (S2/F1), as ONE query over the canonical spine:
/// the AUTHORITATIVE Suwayomi mapping per `source_id` (see `authoritative_suwayomi_mappings`)
/// UNION the English MangaDex mirror. Raw rows — the caller groups them by number. This is
/// what makes a work whose MangaDex spine has 0 chapters still show an installed source's
/// chapters, while never surfacing a redundant same-source mapping the reader can't read
/// from.
///
/// PHASE B'S EXIT CRITERION, AND SINCE 2026-07-30 THE LIVE PATH. Its predecessor —
/// `work_source_chapters_two_branch`, kept as this function's test oracle — was two
/// hand-written branches over two tables that share no key, which is why "the newest
/// chapter of this work across all of its sources", the query `/updates` is, could not be
/// written at all. Once every source writes into `chapter` it collapses to one statement,
/// and Phase C's ledger is built on top of it.
///
/// NEITHER HALF FILTERS ON "LOOKS NUMERIC" (F2). The exclusion that used to guard the
/// MangaDex arm — `AND c.number IS NOT NULL AND c.number GLOB '*[0-9]*'` — cost 21,422
/// works, 18.5% of the catalogue, their ENTIRE chapter list: they are oneshots, and a
/// oneshot's number is a word. Its stated reason was sound (`CAST('Extra' AS REAL)` is
/// `0.0`, which would masquerade as a real chapter 0) and is still honoured, but by the
/// `x:<external_id>` key namespace rather than by dropping the row.
///
/// EQUIVALENCE WITH THE ORACLE IS AS A MULTISET, not as a sequence. Neither version has
/// ever had an `ORDER BY` on its chapter reads, and the only consumer that cares
/// (`graphql::group_aggregated_chapters`) re-sorts into a `BTreeMap` keyed by
/// `(number, key)`. The `ORDER BY` here exists to keep the Suwayomi-then-MangaDex shape
/// the two-branch version happened to produce, not to promise one.
pub async fn work_source_chapters(pool: &Database, work_id: &str) -> Result<Vec<WorkChapterRow>> {
    // A PARTIALLY-DRAINED SPINE GETS NO GUARD, AND THIS IS THE ARGUMENT FOR THAT.
    //
    // Between boot and drain-completion this returns FEWER Suwayomi chapters than the
    // two-branch oracle — never wrong ones, just missing ones. That is why Phase B landed
    // dark, and the gate it waited on has been met: production logged `spine: drained —
    // chapter spine and release ledger complete events=1000753`, with `chapter` at
    // 1,442,150 rows across BOTH sources (it was 877,891 MangaDex-only), 0 rows where
    // `chapter_key IS NULL`, 0 of 11,836 Suwayomi series left to materialise, and 0 of 60
    // sampled real multi-source works diverging between the two implementations.
    //
    // What is left is the fresh-or-restored database, and it is bounded three ways:
    //   * A database with no pre-Phase-B rows never has a gap at all. Every Suwayomi
    //     chapter written since B1 enters the spine in the same call that fills the cache
    //     (`series_cache::write_chapters_to_spine`), keyed on the way in.
    //   * A restore of a pre-Phase-B backup has one only until the Suwayomi drain
    //     finishes: 11,836 series at `spine::SERIES_BATCH` = 25 per pass with a 1 s
    //     `BATCH_GAP` is ~8 minutes, behind a 90 s `BOOT_DELAY`.
    //   * An UNKEYED row is not part of the gap at all. The `unwrap_or_else` arms below
    //     compute exactly what the key drain will write, so the ~30-minute key half of the
    //     drain is invisible from here.
    //
    // A memoised completeness gate — the `ledger::is_complete` pattern, which exists
    // because `spine::remaining()` is a scan and this is a hot read path — was considered
    // and rejected. It buys ~8 minutes of a self-healing, fewer-not-wrong degradation
    // once per restore, and costs a permanent SECOND live answer to "what chapters does
    // this work have", kept alive and diverging, which is precisely the fork Phase B
    // exists to close.
    //
    // The Suwayomi half still resolves ONE mapping per source_id — the same rule
    // `authoritative_suwayomi_mappings` applies, expressed as a window function instead of
    // a fetch-and-filter loop. Ranking by the spine's own chapter count rather than by
    // `suwayomi_chapter`'s is the only substantive difference, and the two agree by
    // construction once the drain has run.
    let rows = sqlx::query_as::<_, SpineChapterRow>(
        "WITH authoritative AS ( \
             SELECT ss.id AS ssid, ss.source_id, ss.source_key, \
                    ROW_NUMBER() OVER ( \
                        PARTITION BY ss.source_id \
                        ORDER BY (SELECT COUNT(*) FROM chapter c \
                                   WHERE c.source_series_id = ss.id) DESC, \
                                 ss.last_seen DESC, ss.id ASC) AS rn \
               FROM source_series ss \
              WHERE ss.work_id = ?1 AND ss.source_type = 'suwayomi' \
         ) \
         SELECT c.number, c.title, a.source_id, c.external_id, c.chapter_key, c.label, \
                'suwayomi' AS source_type, a.source_key, NULLIF(c.scanlator, '') AS scanlator, \
                COALESCE(c.readable_at, c.published_at) AS released_at, \
                c.external_url, NULLIF(TRIM(c.volume), '') AS volume \
           FROM authoritative a \
           JOIN chapter c ON c.source_series_id = a.ssid \
          WHERE a.rn = 1 \
         UNION ALL \
         SELECT c.number, c.title, ss.source_id, c.external_id, c.chapter_key, c.label, \
                'mangadex' AS source_type, NULL, NULLIF(c.scanlator, ''), \
                COALESCE(c.readable_at, c.published_at), c.external_url, \
                NULLIF(TRIM(c.volume), '') \
           FROM source_series ss \
           JOIN chapter c ON c.source_series_id = ss.id \
          WHERE ss.work_id = ?1 AND ss.source_type = 'mangadex' AND c.lang = 'en' \
          ORDER BY source_type DESC, source_id ASC",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            // `key` and `label` are READ FROM THE COLUMNS. That is what the columns are
            // for: they are what admin overrides, the release ledger and the feed writers
            // join and display on, and re-deriving them here would create a second answer
            // that can disagree with the stored one.
            //
            // `number` has no column — it is the one part of the label that is only ever
            // consumed in Rust — so it is re-derived, from the same `chapter_display` rule
            // that produced the two stored values. The `unwrap_or_else` arms cover a row
            // the key drain has not reached yet and compute exactly what the drain will
            // write, so this function is correct mid-drain as well as after it.
            let derived = crate::chapter_label::chapter_display(
                None,
                r.number.as_deref(),
                r.title.as_deref(),
            );
            WorkChapterRow {
                number: derived.number(),
                key: r.chapter_key.unwrap_or_else(|| derived.key(&r.external_id)),
                label: r.label.unwrap_or_else(|| derived.text()),
                title: r.title,
                source_type: r.source_type,
                source_id: r.source_id,
                suwayomi_manga_id: r.source_key,
                chapter_id: r.external_id,
                scanlator: r.scanlator,
                external_url: r.external_url,
                released_at: r.released_at,
                volume: r.volume,
            }
        })
        .collect())
}

/// One row of [`work_source_chapters`]'s union.
#[derive(sqlx::FromRow)]
struct SpineChapterRow {
    number: Option<String>,
    title: Option<String>,
    source_id: String,
    external_id: String,
    chapter_key: Option<String>,
    label: Option<String>,
    source_type: String,
    /// The Suwayomi manga id for the Suwayomi half; NULL on the MangaDex half.
    source_key: Option<String>,
    scanlator: Option<String>,
    released_at: Option<String>,
    external_url: Option<String>,
    volume: Option<String>,
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
pub async fn aggregate_chapter_count(pool: &Database, work_id: &str) -> Result<i64> {
    let rows = work_source_chapters(pool, work_id).await?;
    // UNNUMBERED chapters count as one main chapter each: a oneshot work has exactly one
    // chapter, and reporting 0 is what left 21,422 works reading "No chapters yet" in
    // Browse. They cannot go through `main_chapter_count` — it groups by the integer part
    // of a number, and these have none — so they are counted alongside it.
    let (numbered, unnumbered): (Vec<_>, Vec<_>) = rows.iter().partition(|r| r.number.is_some());
    let grouped = main_chapter_count(numbered.iter().filter_map(|r| r.number));
    Ok(grouped + unnumbered.len() as i64)
}

/// The largest chapter count any of a work's Suwayomi sources reports.
///
/// This is the DETAIL-page counterpart to the `swc` sub-select in
/// [`browse_catalogue_select`], and it exists so the two surfaces answer the same question
/// the same way. It reads `suwayomi_series.chapter_count` — one already-deduped number per
/// source, written by `series_cache::persist_chapters` via `main_chapter_count` — rather
/// than unioning raw `suwayomi_chapter` rows the way [`aggregate_chapter_count`] does.
///
/// That distinction is the whole point: the aggregate counts every cached chapter row across
/// sources, so scanlator duplicates and re-numbered releases inflate it (Tsukimichi 120 →
/// 151), which is why it could only ever be used as a zero-fallback. A MAX over per-source
/// counts has no such failure mode and can therefore be trusted to RAISE a non-zero English
/// count — which is what fixes the works whose MangaDex spine is a stub.
pub async fn suwayomi_chapter_count(pool: &Database, work_id: &str) -> Result<i64> {
    let n = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(sy.chapter_count) FROM source_series ss \
           JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
          WHERE ss.work_id = ? AND ss.source_type = 'suwayomi'",
    )
    .bind(work_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or(0);
    Ok(n.max(0))
}

/// Resolve the id of an existing source_series by its natural key.
pub async fn find_source_series_id(
    pool: &Database,
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
    pool: &Database,
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
    pool: &Database,
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

/// The spine's grouping identity and display text for one chapter, from the one labelling
/// rule ([`crate::chapter_label`]). Both come out of a single `chapter_display` call, and
/// this is the only place either is computed for a `chapter` row — so there is no way to
/// insert a row whose key and label were decided by different rules, or by some fifth one.
pub fn spine_key_and_label(ch: &ChapterInput) -> (String, String) {
    let label =
        crate::chapter_label::chapter_display(None, ch.number.as_deref(), ch.title.as_deref());
    (label.key(&ch.external_id), label.text())
}

/// The ONE statement that writes a `chapter` row. Both writers — the per-chapter upsert
/// the MangaDex firehose uses and the whole-list replace the Suwayomi scan uses — bind
/// this same text, because two copies of a 14-column upsert with a 9-assignment conflict
/// clause is precisely the shape that drifts: `chapter_row_unchanged` compares exactly the
/// columns this `DO UPDATE SET` touches, so one copy gaining a column silently turns the
/// firehose's skip-if-unchanged check into a lie.
const CHAPTER_UPSERT_SQL: &str = "INSERT INTO chapter \
       (id, source_series_id, external_id, number, volume, lang, title, published_at, \
        readable_at, external_url, scanlator, chapter_key, label, created_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
     ON CONFLICT(source_series_id, external_id) DO UPDATE SET \
       number = excluded.number, volume = excluded.volume, lang = excluded.lang, \
       title = excluded.title, published_at = excluded.published_at, \
       readable_at = excluded.readable_at, external_url = excluded.external_url, \
       scanlator = excluded.scanlator, chapter_key = excluded.chapter_key, \
       label = excluded.label";

/// Upsert one mirrored chapter under a source_series (idempotent on external id).
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

/// ONE CLOCK IN THE SPINE, AND IT IS ISO-8601 UTC.
///
/// The two halves disagree on how a timestamp is spelled. `chapter.published_at` /
/// `chapter.readable_at` are ISO-8601 TEXT across 877,824 rows; `suwayomi_chapter
/// .upload_date` is 13-digit epoch-MILLIS TEXT. Migration 0064's central complaint is
/// exactly this, and it is not cosmetic: SQLite compares TEXT under BINARY collation, so
/// every `'2…'` sorts above every `'1…'` and a millis value and an ISO value in one column
/// order arbitrarily against each other. `refresh_feed_series_updates` already pays for it
/// with a `strftime`-to-millis conversion and a guard that silently drops rows the
/// conversion cannot parse.
///
/// So the spine normalises ON WRITE and stores one encoding. ISO-8601 wins over millis
/// because it is what 877,824 existing rows already hold — the alternative is rewriting
/// all of them inside a migration, for no gain.
///
/// `readable_at` is set as well as `published_at`, to the same instant. Suwayomi has no
/// scheduled-vs-actual split: `uploadDate` is when the chapter became readable, which is
/// the definition of `readable_at` (§6.4). Setting it also keeps migration 0073's
/// `idx_chapter_needs_readable_at` — the MangaDex external-URL backfill's work-list —
/// from filling up with 563,095 Suwayomi rows that backfill will never touch.
fn suwayomi_upload_date_to_iso(upload_date: Option<&str>) -> Option<String> {
    let ms: i64 = upload_date?.trim().parse().ok()?;
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
}

/// One Suwayomi chapter in the spine's shape.
///
/// `number` carries the source's number AS TEXT, corruption included (`-1`, `99999999`),
/// rather than a cleaned-up value — the spine mirrors what the source said, and
/// `chapter_display` is what decides whether that is a chapter number or noise. Storing it
/// through the `raw` slot rather than the structured one is what lets the unified read
/// path use a single `chapter_display(None, number, title)` call for both halves and get
/// byte-identical labels; see `the_spine_labels_a_suwayomi_chapter_exactly_as_the_cache_does`.
pub fn suwayomi_chapter_input(ch: &crate::suwayomi::SuwayomiChapter) -> ChapterInput {
    suwayomi_spine_input(
        ch.id,
        &ch.name,
        ch.chapter_number,
        ch.scanlator.as_deref(),
        ch.upload_date.as_deref(),
    )
}

/// [`suwayomi_chapter_input`] over loose columns, so the live scan path (which holds a
/// `SuwayomiChapter`) and the backfill (which reads `suwayomi_chapter` rows straight out
/// of SQLite) cannot drift into two different mappings.
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

/// Replace one source_series' whole chapter list in the canonical spine, in one
/// transaction (Phase B1).
///
/// This is the write the Suwayomi half was missing. `chapter` held 877,824 MangaDex rows
/// and zero Suwayomi rows, because Suwayomi chapters only ever landed in
/// `suwayomi_chapter` — a cache keyed by Suwayomi's own manga id, with no path to `work`.
///
/// UPSERT + TARGETED PRUNE, NOT DELETE-ALL + REINSERT. `series_cache::put_chapters` does
/// the latter to `suwayomi_chapter` and it is right there, because that table's rows are
/// keyed by Suwayomi's chapter id and carry no history. Spine rows do carry history:
/// `created_at` is our first-sighting record and `chapter.id` is stable. Churning both on
/// every scan that adds one chapter would destroy the first-sighting evidence Phase C's
/// ledger seeds from, and would rewrite ~1,000 rows to record one new one. So existing
/// rows are updated in place, new ones inserted, and only genuinely-vanished ones deleted
/// — which is rare enough that the DELETE is skipped entirely when nothing vanished.
pub async fn replace_source_chapters(
    pool: &Database,
    source_series_id: &str,
    chapters: &[ChapterInput],
) -> Result<()> {
    use std::collections::HashSet;

    let incoming: HashSet<&str> = chapters
        .iter()
        .map(|c| c.external_id.as_str())
        .filter(|e| !e.is_empty())
        .collect();
    let existing: Vec<(String, String)> =
        sqlx::query_as("SELECT id, external_id FROM chapter WHERE source_series_id = ?")
            .bind(source_series_id)
            .fetch_all(pool)
            .await?;
    let vanished: Vec<String> = existing
        .into_iter()
        .filter(|(_, ext)| !incoming.contains(ext.as_str()))
        .map(|(id, _)| id)
        .collect();

    let now = Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    for ch in chapters {
        if ch.external_id.is_empty() {
            continue;
        }
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
            .execute(&mut *tx)
            .await?;
    }
    for id in &vanished {
        sqlx::query("DELETE FROM chapter WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
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
/// same queue. Suppression is on the PAIR of works, in either direction, not the work:
/// a work whose match against work A was rejected can still be proposed against work B.
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
    pool: &Database,
    source_series_id: &str,
    candidate_work_id: &str,
    score: f64,
    method: &str,
) -> Result<Option<String>> {
    let id = new_id("mc_");
    let now = Utc::now().to_rfc3339();
    // The pair is the two WORKS, whichever one the row hangs off: the sweep proposes
    // "A into B" or "B into A" depending on which currently sorts first, and both are
    // the same question.
    let res = sqlx::query(
        "INSERT INTO merge_candidate \
           (id, source_series_id, candidate_work_id, score, method, status, created_at) \
         SELECT ?1, ?2, ?3, ?4, ?5, 'pending', ?6 \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM merge_candidate mc \
             JOIN source_series ss ON ss.id = mc.source_series_id \
             JOIN source_series mine ON mine.id = ?2 \
             WHERE mc.source_series_id = ?2 AND mc.candidate_work_id = ?3 \
                OR ss.work_id = mine.work_id AND mc.candidate_work_id = ?3 \
                OR ss.work_id = ?3 AND mc.candidate_work_id = mine.work_id)",
    )
    .bind(&id)
    .bind(source_series_id)
    .bind(candidate_work_id)
    .bind(score)
    .bind(method)
    .bind(&now)
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
pub async fn get_sync_state(pool: &Database, job: &str) -> Result<Option<SyncState>> {
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
pub async fn set_seed_progress(pool: &Database, job: &str, since: &str) -> Result<()> {
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
pub async fn mark_seed_done(pool: &Database, job: &str, since: &str) -> Result<()> {
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
pub async fn maintenance_flag_present(pool: &Database, key: &str) -> Result<bool> {
    let n: Option<i64> = sqlx::query_scalar("SELECT 1 FROM maintenance_flag WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(n.is_some())
}

/// Record a one-time maintenance pass as complete (idempotent).
pub async fn set_maintenance_flag(pool: &Database, key: &str) -> Result<()> {
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
pub async fn set_sync_cursor(pool: &Database, job: &str, since: &str) -> Result<()> {
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
pub async fn reset_sync_state(pool: &Database, job: &str) -> Result<()> {
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
    pool: &Database,
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
pub async fn subscribed_extensions(pool: &Database) -> Result<Vec<String>> {
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
    pool: &Database,
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
    pool: &Database,
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
pub async fn reset_subscription_breaker(pool: &Database, pkg_name: &str) -> Result<()> {
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
pub async fn rearm_subscription_breaker_probe(pool: &Database, pkg_name: &str) -> Result<()> {
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
pub async fn suwayomi_source_keys(pool: &Database) -> Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT source_key FROM source_series WHERE source_type = 'suwayomi'")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(k,)| k).collect())
}

// ── Per-source scan health (Phase E4.3) ──────────────────────────────────────────

/// One source's scan health, aggregated over its series' `series_scan_state` rows.
///
/// The unit here is the SOURCE, not the series, because that is the unit a failure
/// actually has: an extension whose site moved, rebranded or changed its markup breaks
/// every series it carries at once. Before E4 there was no such view — and no signal to
/// build one from, since a broken source's scans recorded as successes (see
/// `suwayomi::Provenance`).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct SourceScanHealth {
    /// Suwayomi source id (`source_series.source_id`).
    pub source_id: String,
    /// Owning extension package, if the extension is still known to us. `None` once an
    /// extension is uninstalled and its `source_extension` row is gone — the series and
    /// their scan state outlive it, which is exactly the state E4.4 exists to clean up.
    pub pkg_name: Option<String>,
    /// Series of this source that have a scan-state row (i.e. that the scanner tracks).
    pub series: i64,
    /// Series with any current failure streak (`consecutive_failures > 0`).
    pub failing: i64,
    /// Series whose streak has reached `SOURCE_OUTAGE_MIN_STREAK` **for a source-side
    /// reason** ('cached_fallback' or 'fetch_error'). This — not `failing` — is what an
    /// outage decision is made on: it excludes both a single blip and our own
    /// 'persist_error'.
    pub confirmed_failing: i64,
    /// Of `failing`, how many last failed by being served Suwayomi's CACHE (the F11 case,
    /// i.e. broken while looking healthy) …
    pub cached_fallback: i64,
    /// … versus failing loudly with an upstream error.
    pub fetch_error: i64,
    /// Series the scanner has never got a chapter for. On a healthy source this is a
    /// handful of genuinely empty entries; across a whole source it is the signature of a
    /// source that has never once worked (all 209 Genz Toons series sat here).
    pub zero_chapter_series: i64,
    /// Worst current streak on the source, for ordering the admin view.
    pub worst_streak: i64,
    /// Most recent failure across the source.
    pub last_failure_at: Option<String>,
    /// Most recent successful scan across the source. A source whose newest success is old
    /// is stale even if nothing is currently "failing".
    pub last_scanned_at: Option<String>,
}

/// Per-source scan health, worst first. One indexed GROUP BY over `series_scan_state`
/// joined to `source_series` (via `idx_source_series_suwayomi_key`, migration 0072) and
/// `source_extension`, so it is cheap enough for both the admin surface and the scanner's
/// post-tick outage check.
pub async fn source_scan_health(pool: &Database) -> Result<Vec<SourceScanHealth>> {
    Ok(sqlx::query_as::<_, SourceScanHealth>(
        "SELECT ss.source_id AS source_id, \
                MAX(se.pkg_name) AS pkg_name, \
                COUNT(*) AS series, \
                SUM(CASE WHEN st.consecutive_failures > 0 THEN 1 ELSE 0 END) AS failing, \
                SUM(CASE WHEN st.consecutive_failures >= ? \
                          AND st.last_failure_kind IN ('cached_fallback', 'fetch_error') \
                         THEN 1 ELSE 0 END) AS confirmed_failing, \
                SUM(CASE WHEN st.last_failure_kind = 'cached_fallback' THEN 1 ELSE 0 END) \
                  AS cached_fallback, \
                SUM(CASE WHEN st.last_failure_kind = 'fetch_error' THEN 1 ELSE 0 END) \
                  AS fetch_error, \
                SUM(CASE WHEN COALESCE(st.known_chapter_count, 0) = 0 THEN 1 ELSE 0 END) \
                  AS zero_chapter_series, \
                COALESCE(MAX(st.consecutive_failures), 0) AS worst_streak, \
                MAX(st.last_failure_at) AS last_failure_at, \
                MAX(st.last_scanned_at) AS last_scanned_at \
           FROM series_scan_state st \
           JOIN source_series ss \
             ON ss.source_key = st.series_id AND ss.source_type = 'suwayomi' \
           LEFT JOIN source_extension se ON se.source_id = ss.source_id \
          GROUP BY ss.source_id \
          ORDER BY confirmed_failing DESC, failing DESC, series DESC",
    )
    .bind(SOURCE_OUTAGE_MIN_STREAK)
    .fetch_all(pool)
    .await?)
}

/// Consecutive failures a series must reach before it counts toward a source-wide outage.
/// With `ERROR_BACKOFF_BASE_MINUTES = 30` and doubling, 3 strikes means the series has been
/// failing for ~3.5 h — long enough that a Suwayomi restart, a FlareSolverr blip or a
/// network hiccup has passed, short enough that a genuinely dead source is caught the same
/// morning. Lives here rather than in `scanner` because `source_scan_health` computes
/// `confirmed_failing` with it and both readers must agree on the definition.
pub const SOURCE_OUTAGE_MIN_STREAK: i64 = 3;

/// How many works are reachable ONLY through each source — i.e. what becomes unreadable if
/// that source stays broken. Keyed by `source_id`.
///
/// Deliberately separate from [`source_scan_health`]: this is a per-work NOT EXISTS and is
/// only wanted by the admin panel, whereas the health aggregate runs after ticks. Measured
/// on production, 53 works hang off Genz Toons alone.
pub async fn source_exclusive_work_counts(
    pool: &Database,
) -> Result<std::collections::HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT ss.source_id, COUNT(DISTINCT ss.work_id) \
           FROM source_series ss \
          WHERE ss.source_type = 'suwayomi' \
            AND NOT EXISTS (SELECT 1 FROM source_series o \
                             WHERE o.work_id = ss.work_id AND o.id <> ss.id) \
          GROUP BY ss.source_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// An open whole-source outage (`source_scan_outage`, migration 0072).
///
/// A projection, not the whole row: `pkg_name` and the counts at detection time are stored
/// for the alert text and for post-hoc debugging, but every reader wants the LIVE counts
/// from [`source_scan_health`] instead, so they are not selected here.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SourceOutage {
    pub source_id: String,
    pub detected_at: String,
    pub last_alert_at: String,
    pub kind: Option<String>,
    pub parked_until: Option<String>,
}

/// Of `source_keys`, the PARKED series worth waking because their source just published
/// for them (Phase E2).
///
/// "Parked" is expressed as a schedule, not a status flag, because that is what the pause
/// actually is: `scanner::park_paused` pushes a COMPLETED/HIATUS/CANCELLED series far out
/// so it leaves the frequent due-set. Anything scheduled past `parked_after` is therefore
/// either paused or parked by an outage — and both are worth re-examining when the source
/// is visibly publishing again (a still-broken source just fails and re-parks).
///
/// `scanned_before` is the cooldown: a series that merely SITS in a source's top 30 for
/// days must not be re-triggered every walk. One scan is enough, because a genuine reopen
/// flips the series to ONGOING and it leaves the paused cohort entirely — so a series still
/// showing up here after a trigger is one that did NOT reopen, and re-checking it weekly is
/// the intended cost.
pub async fn paused_series_due_for_trigger(
    pool: &Database,
    source_keys: &[String],
    parked_after: &str,
    scanned_before: &str,
) -> Result<Vec<String>> {
    if source_keys.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", source_keys.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT series_id FROM series_scan_state \
          WHERE series_id IN ({placeholders}) \
            AND next_scan_at > ? \
            AND (last_scanned_at IS NULL OR last_scanned_at < ?)"
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for k in source_keys {
        q = q.bind(k);
    }
    Ok(q.bind(parked_after)
        .bind(scanned_before)
        .fetch_all(pool)
        .await?)
}

// `work_comic_type_word` was removed 2026-07-31 (§8i). It selected `w.comic_type` — a column
// that has never existed on `work` (comic_type is materialised onto `feed_series_updates` (0064)
// and `browse_catalogue` (0069)) — and swallowed the resulting error with `.ok()?`, so Phase E's
// tier engine silently resolved every series to Manga/12h. Replaced by `scanner::scan_comic_type`.

/// Phase E5. The last-seen page-1 LATEST order for one source, newest-first, or `None`
/// if this source has never been snapshotted (the first discovery poll — which must
/// baseline and trigger nothing). A malformed row (should never happen; we write it) is
/// treated as "no snapshot" rather than propagating a parse error into the poll loop.
///
/// THE DEGRADATION IS RIGHT; THE SILENCE WAS NOT. Re-baselining is the safe recovery — the
/// alternative is a parse error killing the discovery pass — but a bare `.ok()` makes a
/// corrupt row indistinguishable from a never-snapshotted source, and the difference matters:
/// a source in that state re-baselines on EVERY pass, so it can never flag anything again,
/// permanently, while every pass still reports success. That is the same silent-inertness
/// class as the dead `w.comic_type` lookup above, and the reason this arm now logs.
pub async fn source_latest_snapshot(pool: &Database, source_id: &str) -> Result<Option<Vec<i64>>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT ordered_ids FROM source_latest_snapshot WHERE source_id = ?")
            .bind(source_id)
            .fetch_optional(pool)
            .await?;
    Ok(
        row.and_then(|(json,)| match serde_json::from_str::<Vec<i64>>(&json) {
            Ok(ids) => Some(ids),
            Err(e) => {
                // Truncated, because the payload is up to `SNAPSHOT_WINDOW` ids and the useful
                // part of a malformed one is its head — enough to tell a truncated write from a
                // schema change from a foreign value type.
                tracing::warn!(
                    source_id,
                    error = %e,
                    payload = %json.chars().take(120).collect::<String>(),
                    "discovery: LATEST snapshot is unparseable — re-baselining this source, which \
                     means it will detect nothing this pass"
                );
                None
            }
        }),
    )
}

/// Phase E5. Overwrite one source's page-1 LATEST snapshot. `ids` is already capped to
/// the discovery window and stored newest-first, as a JSON array.
pub async fn put_source_latest_snapshot(
    pool: &Database,
    source_id: &str,
    ids: &[i64],
) -> Result<()> {
    let json = serde_json::to_string(ids)?;
    sqlx::query(
        "INSERT INTO source_latest_snapshot (source_id, ordered_ids, captured_at) \
         VALUES (?, ?, ?) \
         ON CONFLICT(source_id) DO UPDATE SET \
           ordered_ids = excluded.ordered_ids, captured_at = excluded.captured_at",
    )
    .bind(source_id)
    .bind(&json)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether ANY source outage is currently open — a one-row existence probe, so the scan
/// loop can cheaply decide whether a clean pass still needs the health check run (a
/// recovering source's pass has successes, not failures, so gating only on failures would
/// leave the outage row and the tripped breaker behind after recovery).
pub async fn any_source_outage(pool: &Database) -> Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM source_scan_outage LIMIT 1")
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Every currently-open source outage, oldest first.
pub async fn source_outages(pool: &Database) -> Result<Vec<SourceOutage>> {
    Ok(sqlx::query_as::<_, SourceOutage>(
        "SELECT source_id, detected_at, last_alert_at, kind, parked_until \
           FROM source_scan_outage ORDER BY detected_at ASC",
    )
    .fetch_all(pool)
    .await?)
}

/// Open or refresh a source's outage row. Returns `true` when the caller should ALERT —
/// either the outage is new, or the last alert is older than `realert_hours`.
///
/// `detected_at` is preserved across refreshes so "out since" stays truthful; only
/// `last_alert_at` and the counts move. Read-then-upsert rather than one clever statement:
/// the only caller is the scan loop, whose ticks never overlap
/// (`MissedTickBehavior::Delay`), so there is no race to guard against and the alert
/// decision is worth being able to read.
#[allow(clippy::too_many_arguments)]
pub async fn record_source_outage(
    pool: &Database,
    source_id: &str,
    pkg_name: Option<&str>,
    series: i64,
    failing: i64,
    kind: &str,
    parked_until: Option<&str>,
    realert_hours: i64,
) -> Result<bool> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let prior: Option<(String, String)> = sqlx::query_as(
        "SELECT detected_at, last_alert_at FROM source_scan_outage WHERE source_id = ?",
    )
    .bind(source_id)
    .fetch_optional(pool)
    .await?;
    let cutoff = now - chrono::Duration::hours(realert_hours);
    let alert = match prior.as_ref() {
        // New outage: always alert.
        None => true,
        // Ongoing: re-alert only once the quiet window has elapsed, so a source that stays
        // dead is one line a day rather than one line per tick per series.
        Some((_, last_alert_at)) => chrono::DateTime::parse_from_rfc3339(last_alert_at)
            .map(|t| t.with_timezone(&Utc) <= cutoff)
            .unwrap_or(true),
    };
    let detected_at = prior.map(|(d, _)| d).unwrap_or_else(|| now_iso.clone());
    let last_alert_at = if alert { &now_iso } else { &detected_at };
    sqlx::query(
        "INSERT INTO source_scan_outage \
           (source_id, pkg_name, detected_at, last_alert_at, series, failing, kind, parked_until) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(source_id) DO UPDATE SET \
           pkg_name = excluded.pkg_name, \
           series = excluded.series, \
           failing = excluded.failing, \
           kind = excluded.kind, \
           parked_until = excluded.parked_until, \
           last_alert_at = CASE WHEN ? THEN excluded.last_alert_at \
                                ELSE source_scan_outage.last_alert_at END",
    )
    .bind(source_id)
    .bind(pkg_name)
    .bind(&detected_at)
    .bind(last_alert_at)
    .bind(series)
    .bind(failing)
    .bind(kind)
    .bind(parked_until)
    .bind(alert as i64)
    .execute(pool)
    .await?;
    Ok(alert)
}

/// Close a source's outage. Returns `true` if a row was actually removed, so the caller can
/// log the recovery once.
pub async fn clear_source_outage(pool: &Database, source_id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM source_scan_outage WHERE source_id = ?")
        .bind(source_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Trip the subscription breaker for a whole-source outage the SCANNER detected, without
/// touching the sync-pass strike count.
///
/// [`mark_subscription_synced`] is the sync loop's own accounting: it counts failing
/// discovery walks and trips at `SUBSCRIPTION_FAILURE_LIMIT`. A scan-side outage is
/// independent evidence — the walk can keep succeeding while every chapter fetch 404s, which
/// is exactly what Genz Toons did — so it sets `disabled_at` directly and records why in
/// `last_error`. No-op when there is no subscription row (the source may never have had one)
/// or when the breaker is already tripped. Returns `true` if this call tripped it.
pub async fn trip_subscription_breaker(
    pool: &Database,
    pkg_name: &str,
    reason: &str,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE extension_subscription \
            SET disabled_at = ?, last_error = ? \
          WHERE pkg_name = ? AND disabled_at IS NULL",
    )
    .bind(&now)
    .bind(reason)
    .bind(pkg_name)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
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
pub async fn purge_foreign_language_suwayomi(pool: &Database) -> Result<Vec<i64>> {
    // The non-English target set, as manga ids (INTEGER) and as TEXT (the form every
    // satellite table keys the Suwayomi id in).
    const NON_EN: &str = "SELECT id FROM suwayomi_series WHERE lang IS NOT NULL AND lang <> 'en' ORDER BY id LIMIT 128";
    let non_en_text = format!("(SELECT CAST(id AS TEXT) FROM ({NON_EN}) t)");

    // Cheap pre-check OUTSIDE the write lock: the steady state is "nothing to purge"
    // (this runs every reconcile pass), and opening a write transaction just to discover
    // that would contend with ingest for the single SQLite writer for no reason.
    let any: Option<i64> = sqlx::query_scalar(&format!("SELECT id FROM ({NON_EN}) LIMIT 1"))
        .fetch_optional(pool)
        .await?;
    if any.is_none() {
        return Ok(Vec::new());
    }

    let mut purged_ids = Vec::new();
    loop {
        // Each bounded group stays in ONE write transaction so recovery is atomic and idempotent: a crash
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
            return Ok(purged_ids);
        }

        // Works that will be ORPHANED by the purge: every one of their source_series is a
        // to-be-purged Suwayomi link. Computed BEFORE the deletes (it needs the links
        // intact). A work still anchored to the MangaDex catalogue or an English source is
        // excluded (the non-purged branch count is > 0), so consolidated works survive and
        // merely lose the foreign-language translator mapping.
        let orphan_works: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT ss.work_id FROM source_series ss \
         WHERE ss.work_id IN (SELECT work_id FROM source_series \
             WHERE source_type = 'suwayomi' AND source_key IN {non_en_text}) \
         GROUP BY ss.work_id \
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
        purged_ids.extend(series_ids);
        tokio::task::yield_now().await;
    }
}

/// Backfill a "due now" scan-state row for every enrolled Suwayomi series that lacks one,
/// so the DB-driven scanner (which selects work from `series_scan_state`) picks up series
/// enrolled by paths that don't scan-on-enrol (federated search) or predate scan-state.
/// One set-based INSERT…SELECT (no per-row round-trips); returns how many rows it added.
pub async fn backfill_pending_scan_states(pool: &Database) -> Result<u64> {
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
    pool: &Database,
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
pub async fn mark_source_sync_pass(pool: &Database) -> Result<()> {
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
pub async fn source_sync_due(pool: &Database, interval_seconds: u64) -> bool {
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

    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn checked_genres_propagate_query_failure() {
        let pool = pool().await;
        assert!(work_effective_genres_batch_checked(&pool, &[])
            .await
            .unwrap()
            .is_empty());
        sqlx::query("DROP TABLE work_tag")
            .execute(&pool)
            .await
            .unwrap();
        let error = work_effective_genres_batch_checked(&pool, &["w_missing".into()])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("work_tag"));
    }

    #[tokio::test]
    async fn official_links_are_shown_but_are_not_identities() {
        let pool = pool().await;
        let work = upsert_work_from_mangadex(
            &pool,
            "md-official-links",
            &WorkInput {
                external_ids: vec![("al".into(), "500".into())],
                official_links: vec![
                    ("engtl".into(), "https://publisher.example/title".into()),
                    ("raw".into(), "http://publisher.example/insecure".into()),
                ],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let links: Vec<(String, String)> = sqlx::query_as(
            "SELECT provider, external_id FROM work_tracker_link WHERE work_id = ? ORDER BY provider",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            links,
            vec![
                ("al".into(), "500".into()),
                ("engtl".into(), "https://publisher.example/title".into())
            ]
        );
        let identities: Vec<String> = sqlx::query_scalar(
            "SELECT provider FROM work_external_id WHERE work_id = ? ORDER BY provider",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(identities, vec!["al".to_string(), "mangadex".to_string()]);
    }

    #[tokio::test]
    async fn tracker_snapshots_respect_target_authority_and_survive_provisional_merges() {
        let pool = pool().await;
        let target = upsert_work_from_mangadex(
            &pool,
            "md-tracker-target",
            &WorkInput {
                external_ids: vec![("al".into(), "200".into())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let source = upsert_work_from_mangadex(
            &pool,
            "md-tracker-source",
            &WorkInput {
                external_ids: vec![("al".into(), "100".into()), ("mal".into(), "300".into())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        merge_works(&pool, &source, &target).await.unwrap();
        let links: Vec<(String, String)> = sqlx::query_as("SELECT provider, external_id FROM work_tracker_link WHERE work_id = ? ORDER BY provider")
            .bind(&target).fetch_all(&pool).await.unwrap();
        assert_eq!(
            links,
            vec![("al".into(), "200".into())],
            "a refreshed target snapshot must not mix in another snapshot's omitted providers"
        );
        let provisional = create_work(&pool, &WorkInput::default()).await.unwrap();
        let rich = upsert_work_from_mangadex(
            &pool,
            "md-tracker-rich",
            &WorkInput {
                external_ids: vec![("al".into(), "400".into())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        merge_works(&pool, &rich, &provisional).await.unwrap();
        let id: String = sqlx::query_scalar(
            "SELECT external_id FROM work_tracker_link WHERE work_id = ? AND provider = 'al'",
        )
        .bind(&provisional)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            id, "400",
            "a provisional survivor inherits imported tracker details"
        );
        delete_work_cascade(&pool, &provisional).await.unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_tracker_link WHERE work_id = ?")
                .bind(provisional)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn staged_catalogue_refresh_never_locks_main_and_preserves_newer_writes() {
        let path = std::env::temp_dir().join(format!("komiq-refresh-{}.sqlite3", Uuid::new_v4()));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_millis(100));
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(3)
            .connect_with(options)
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE work(id TEXT PRIMARY KEY)",
            "CREATE TABLE browse_catalogue(work_id TEXT PRIMARY KEY, title TEXT NOT NULL, comic_type TEXT NOT NULL)",
            "INSERT INTO work VALUES ('keep'), ('prune'), ('update')",
            "INSERT INTO browse_catalogue VALUES ('keep','original','MANGA'), ('prune','original','MANGA'), ('update','original','MANGA')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        // Staging still completes while another connection owns the main writer.
        let writer = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let mut snapshot = refresh_snapshot(&pool, &["browse_catalogue"])
            .await
            .unwrap();
        sqlx::query("INSERT INTO temp.browse_catalogue VALUES ('keep','snapshot','MANGA'), ('update','snapshot','MANHWA')")
            .execute(&mut *snapshot).await.unwrap();
        assert!(
            sqlx::query("UPDATE main.browse_catalogue SET title = 'forbidden'")
                .execute(&mut *snapshot)
                .await
                .is_err(),
            "main is opened read-only"
        );
        writer.rollback().await.unwrap();
        // A concurrent cache writer wins over both a stale replacement and stale prune.
        sqlx::query(
            "UPDATE browse_catalogue SET title = 'newer' WHERE work_id IN ('keep','prune')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            publish_snapshot(&pool, &mut snapshot, "browse_catalogue")
                .await
                .unwrap(),
            1
        );
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT work_id, title, comic_type FROM browse_catalogue ORDER BY work_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("keep".into(), "newer".into(), "MANGA".into()),
                ("prune".into(), "newer".into(), "MANGA".into()),
                ("update".into(), "snapshot".into(), "MANHWA".into())
            ]
        );
        drop(snapshot);
        pool.close().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn staged_catalogue_refresh_publishes_complete_rows_in_multiple_writer_turns() {
        let path = std::env::temp_dir().join(format!("komiq-refresh-{}.sqlite3", Uuid::new_v4()));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE work(id TEXT PRIMARY KEY)",
            "CREATE TABLE browse_catalogue(work_id TEXT PRIMARY KEY, title TEXT NOT NULL, comic_type TEXT NOT NULL)",
            "CREATE TABLE writer_turns(n INTEGER NOT NULL)",
            "INSERT INTO writer_turns VALUES (0)",
            "WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n < 385) INSERT INTO work SELECT printf('%04d', n) FROM ids",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let mut snapshot = refresh_snapshot(&pool, &["browse_catalogue"])
            .await
            .unwrap();
        sqlx::query("INSERT INTO temp.browse_catalogue SELECT id, 'typed', 'MANGA' FROM main.work")
            .execute(&mut *snapshot)
            .await
            .unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_pool = pool.clone();
        let writer_stop = stop.clone();
        let writer = tokio::spawn(async move {
            while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                sqlx::query("UPDATE writer_turns SET n = n + 1")
                    .execute(&writer_pool)
                    .await
                    .unwrap();
                let incomplete: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM browse_catalogue WHERE comic_type IS NULL OR title <> 'typed'")
                    .fetch_one(&writer_pool).await.unwrap();
                assert_eq!(incomplete, 0);
            }
        });
        assert_eq!(
            publish_snapshot(&pool, &mut snapshot, "browse_catalogue")
                .await
                .unwrap(),
            385
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.await.unwrap();
        let turns: i64 = sqlx::query_scalar("SELECT n FROM writer_turns")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            turns >= 2,
            "other writers must make progress between batches: {turns}"
        );
        drop(snapshot);
        pool.close().await;
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fts_match_query_bounds_input_and_deduplicates_terms() {
        assert!(fts_match_query(&"x".repeat(MAX_FTS_QUERY_CHARS)).is_some());
        assert!(fts_match_query(&"x".repeat(MAX_FTS_QUERY_CHARS + 1)).is_none());
        assert!(fts_match_query(&"界".repeat(MAX_FTS_QUERY_CHARS)).is_some());
        assert!(fts_match_query(&"界".repeat(MAX_FTS_QUERY_CHARS + 1)).is_none());
        let terms = (0..MAX_FTS_TERMS)
            .map(|i| format!("term{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(fts_match_query(&terms).is_some());
        assert!(fts_match_query(&format!("{terms} excess")).is_none());
        assert_eq!(
            fts_match_query("Solo solo lev").as_deref(),
            Some("\"solo\" \"lev\"*")
        );
        assert_eq!(
            fts_match_query("Naruto NARUTO").as_deref(),
            Some("\"naruto\"")
        );
        assert_eq!(fts_match_query("a a a a a"), None);
    }

    #[tokio::test]
    async fn search_deadline_interrupts_sqlite_and_cleans_up_for_the_next_reader() {
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut connection =
            SearchConnection::with_budget(&pool, std::time::Duration::from_millis(10))
                .await
                .unwrap();
        let started = std::time::Instant::now();
        let error = sqlx::query_scalar::<_, i64>(
            "WITH RECURSIVE n(x) AS \
            (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 1000000000) SELECT SUM(x) FROM n",
        )
        .fetch_one(&mut *connection)
        .await
        .unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("9")
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        connection.finish().await.unwrap();
        // More than 1,000 opcodes ensures this actually tests handler removal.
        let sum: i64 = sqlx::query_scalar(
            "WITH RECURSIVE n(x) AS \
            (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 10000) SELECT SUM(x) FROM n",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sum, 50_005_000);
        pool.close().await;
    }

    #[tokio::test]
    async fn cancelled_search_interrupts_worker_and_does_not_poison_the_reader_pool() {
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let search_pool = pool.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let search = tokio::spawn(async move {
            let mut connection =
                SearchConnection::with_budget(&search_pool, std::time::Duration::from_secs(60))
                    .await
                    .unwrap();
            ready.send(()).unwrap();
            let _ = sqlx::query_scalar::<_, i64>(
                "WITH RECURSIVE n(x) AS \
                (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 1000000000) SELECT SUM(x) FROM n",
            )
            .fetch_one(&mut *connection)
            .await;
        });
        started.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        search.abort();
        let _ = search.await;
        let replacement = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sqlx::query_scalar::<_, i64>(
                "WITH RECURSIVE n(x) AS \
                (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 10000) SELECT SUM(x) FROM n",
            )
            .fetch_one(&pool),
        )
        .await
        .expect("cancelled statement must release its worker promptly")
        .unwrap();
        assert_eq!(replacement, 50_005_000);
        pool.close().await;
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

    async fn insert_suwayomi_series(pool: &Database, id: i64, lang: Option<&str>) {
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

    /// E4.3 (F11). The health aggregate has to separate the three ways a scan can fail,
    /// because only one of them means "this source is broken while reporting healthy" — and
    /// that one had no signal at all before E4.
    ///
    /// The `persist_error` case is asserted alongside because it is the trap: it is OUR write
    /// failing, not the source's, and counting it would let a burst of SQLite contention
    /// park a perfectly good source for a week.
    #[tokio::test]
    async fn source_scan_health_separates_a_silent_cached_fallback_from_our_own_write_errors() {
        let pool = pool().await;
        let work = create_work(&pool, &slime_input()).await.unwrap();
        // Two sources: one quietly broken (the Genz Toons shape), one healthy.
        let add = |key: &'static str,
                   source_id: &'static str,
                   failures: i64,
                   kind: Option<&'static str>,
                   chapters: i64| {
            let pool = pool.clone();
            let work = work.clone();
            async move {
                insert_suwayomi_series(&pool, key.parse().unwrap(), Some("en")).await;
                upsert_source_series(&pool, &work, "suwayomi", source_id, key, None, false)
                    .await
                    .unwrap();
                sqlx::query(
                    "INSERT INTO series_scan_state \
                       (series_id, next_scan_at, known_chapter_count, consecutive_failures, \
                        last_failure_kind, last_failure_at, last_scanned_at, updated_at) \
                     VALUES (?, '2026-01-01T00:00:00Z', ?, ?, ?, '2026-01-02T00:00:00Z', \
                             '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                )
                .bind(key)
                .bind(chapters)
                .bind(failures)
                .bind(kind)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        // `broken`: 3 series, all served from cache, all at 0 chapters — never once worked.
        add("101", "broken", 4, Some("cached_fallback"), 0).await;
        add("102", "broken", 3, Some("cached_fallback"), 0).await;
        add("103", "broken", 9, Some("fetch_error"), 0).await;
        // `healthy`: one series mid-blip (1 strike, under the streak floor) and one whose
        // last failure was OUR write losing a race — neither is source-side evidence.
        add("201", "healthy", 1, Some("cached_fallback"), 40).await;
        add("202", "healthy", 7, Some("persist_error"), 12).await;
        sqlx::query(
            "INSERT INTO source_extension \
               (source_id, pkg_name, repo_url, updated_at) \
             VALUES ('broken', 'ext.broken', 'https://example.invalid', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let health = source_scan_health(&pool).await.unwrap();
        let broken = health.iter().find(|h| h.source_id == "broken").unwrap();
        let healthy = health.iter().find(|h| h.source_id == "healthy").unwrap();

        assert_eq!(
            health.first().map(|h| h.source_id.as_str()),
            Some("broken"),
            "worst source sorts first"
        );
        assert_eq!(broken.series, 3);
        assert_eq!(
            broken.confirmed_failing, 3,
            "all three count: streak >= floor and the reason is source-side"
        );
        assert_eq!(broken.cached_fallback, 2);
        assert_eq!(broken.fetch_error, 1);
        assert_eq!(
            broken.zero_chapter_series, 3,
            "never got a chapter for any of them"
        );
        assert_eq!(broken.worst_streak, 9);
        assert_eq!(broken.pkg_name.as_deref(), Some("ext.broken"));

        assert_eq!(healthy.failing, 2, "both have a live streak…");
        assert_eq!(
            healthy.confirmed_failing, 0,
            "…but one is a blip under the floor and the other is our own write error — \
             neither condemns the source"
        );
        assert_eq!(
            healthy.pkg_name, None,
            "an uninstalled/unknown extension is reported as such, not hidden"
        );
    }

    /// E4.3. "One loud alert, not 209 silent ones" — and, just as importantly, not 209 loud
    /// ones either. An ongoing outage must stay quiet between re-alert windows while still
    /// keeping its `detected_at`, and recovery must actually close it.
    #[tokio::test]
    async fn a_source_outage_alerts_once_keeps_its_detected_at_and_can_be_cleared() {
        let pool = pool().await;
        assert!(!any_source_outage(&pool).await.unwrap());

        assert!(
            record_source_outage(
                &pool,
                "broken",
                Some("ext.broken"),
                209,
                209,
                "cached_fallback",
                Some("2026-02-06T00:00:00Z"),
                24
            )
            .await
            .unwrap(),
            "a new outage alerts"
        );
        let first = source_outages(&pool).await.unwrap();
        assert_eq!(first.len(), 1);
        let detected_at = first[0].detected_at.clone();
        assert_eq!(first[0].kind.as_deref(), Some("cached_fallback"));
        assert!(any_source_outage(&pool).await.unwrap());

        // Same outage, still ongoing: silent, and `detected_at` does not move.
        for _ in 0..3 {
            assert!(
                !record_source_outage(
                    &pool,
                    "broken",
                    Some("ext.broken"),
                    209,
                    209,
                    "cached_fallback",
                    Some("2026-02-06T00:00:00Z"),
                    24
                )
                .await
                .unwrap(),
                "an ongoing outage must not re-alert inside the quiet window"
            );
        }
        let again = source_outages(&pool).await.unwrap();
        assert_eq!(
            again[0].detected_at, detected_at,
            "\"out since\" stays truthful across re-checks"
        );

        // A zero-hour window is how "the quiet period elapsed" looks to the caller.
        assert!(
            record_source_outage(
                &pool,
                "broken",
                Some("ext.broken"),
                209,
                209,
                "cached_fallback",
                None,
                0
            )
            .await
            .unwrap(),
            "once the quiet window elapses, the outage alerts again"
        );

        assert!(clear_source_outage(&pool, "broken").await.unwrap());
        assert!(
            !clear_source_outage(&pool, "broken").await.unwrap(),
            "clearing is idempotent, so recovery is logged exactly once"
        );
        assert!(!any_source_outage(&pool).await.unwrap());
    }

    /// E4.3. Scan-side evidence must be able to trip the subscription breaker on its own: the
    /// discovery walk and the chapter fetch fail independently, and Genz Toons' scans were
    /// reporting success while its LATEST walk 404'd.
    #[tokio::test]
    async fn a_scan_side_outage_trips_the_subscription_breaker_once() {
        let pool = pool().await;
        assert!(
            !trip_subscription_breaker(&pool, "ext.missing", "reason")
                .await
                .unwrap(),
            "no subscription row, nothing to trip — and no error"
        );

        set_extension_subscription(&pool, "ext.broken", true)
            .await
            .unwrap();
        assert!(
            trip_subscription_breaker(&pool, "ext.broken", "scanner: outage")
                .await
                .unwrap()
        );
        let (disabled_at, err): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT disabled_at, last_error FROM extension_subscription WHERE pkg_name='ext.broken'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(disabled_at.is_some());
        assert_eq!(err.as_deref(), Some("scanner: outage"));
        assert!(
            !subscribed_extensions(&pool)
                .await
                .unwrap()
                .contains(&"ext.broken".to_string()),
            "a tripped subscription leaves the sync work-list"
        );
        assert!(
            !trip_subscription_breaker(&pool, "ext.broken", "again")
                .await
                .unwrap(),
            "already tripped: no second alert, and the original reason is kept"
        );
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
    /// An admin's edit to a source-owned column survives the next sync, and removing
    /// the pin hands the column back with whatever the source last wrote.
    #[tokio::test]
    async fn a_pinned_field_survives_source_writes_until_it_is_unpinned() {
        let pool = pool().await;
        let id = upsert_work_from_mangadex(&pool, "md-slime", &slime_input())
            .await
            .unwrap();
        let read = |column: &'static str| {
            let (pool, id) = (pool.clone(), id.clone());
            async move {
                sqlx::query_scalar::<_, Option<String>>(&format!(
                    "SELECT CAST({column} AS TEXT) FROM work WHERE id = ?"
                ))
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let mut conn = pool.write().acquire().await.unwrap();
        pin_work_field(&mut conn, &id, PinnedField::Author, Some("Fuse"))
            .await
            .unwrap();
        pin_work_field(&mut conn, &id, PinnedField::Year, Some("2014"))
            .await
            .unwrap();
        pin_work_field(&mut conn, &id, PinnedField::OriginalLanguage, None)
            .await
            .unwrap();
        drop(conn);
        assert_eq!(read("author").await.as_deref(), Some("Fuse"));
        assert_eq!(read("year").await.as_deref(), Some("2014"));

        // The catalogue sync writes its own values again.
        let mut synced = slime_input();
        synced.author = Some("Source Author".into());
        synced.year = Some(2016);
        synced.original_language = Some("ja".into());
        upsert_work_from_mangadex(&pool, "md-slime", &synced)
            .await
            .unwrap();
        assert_eq!(read("author").await.as_deref(), Some("Fuse"));
        assert_eq!(read("year").await.as_deref(), Some("2014"));
        assert_eq!(read("original_language").await, None, "pinned empty");
        assert_eq!(
            pinned_work_fields(&pool, &id).await.unwrap(),
            ["author", "original_language", "year"]
        );

        // Unpinning restores what the source last wrote, and syncs apply again.
        let mut conn = pool.write().acquire().await.unwrap();
        for field in [
            PinnedField::Author,
            PinnedField::Year,
            PinnedField::OriginalLanguage,
            PinnedField::Status,
        ] {
            unpin_work_field(&mut conn, &id, field).await.unwrap();
        }
        drop(conn);
        assert_eq!(read("author").await.as_deref(), Some("Source Author"));
        assert_eq!(read("year").await.as_deref(), Some("2016"));
        assert_eq!(read("original_language").await.as_deref(), Some("ja"));
        synced.year = Some(2017);
        upsert_work_from_mangadex(&pool, "md-slime", &synced)
            .await
            .unwrap();
        assert_eq!(read("year").await.as_deref(), Some("2017"));
    }

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
            description: Some("Mikami Satoru is reincarnated as a slime.".into()),
            ..Default::default()
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
            external_url: None,
            scanlator: None,
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

    /// REQUIREMENT 4: the Library's "recently updated" order must see a chapter WHEREVER it
    /// came from.
    ///
    /// This function used to carry `AND ss.source_type = 'mangadex'`, so a work whose new
    /// chapter arrived from a scanlator source could not move its own recency, and a work with
    /// no MangaDex-English chapter at all returned `None` — sending `map_canonical_series` to
    /// the `work.updated_at` fallback, which is the metadata-POLL clock that `Series::updated_at`
    /// explicitly documents as unusable for this. Measured on the 2026-08-11 production
    /// snapshot, 37 of the 39 canonical works in `user_library` were mis-sorted by one of those
    /// two paths.
    ///
    /// Both halves are asserted here because the fix is a REMOVED predicate, and a removed
    /// predicate is exactly the kind of change that over-shoots: the language filter has to
    /// SURVIVE (the MangaDex mirror is genuinely multi-language) while the source filter goes.
    #[tokio::test]
    async fn latest_english_chapter_at_sees_every_source_not_just_the_mangadex_mirror() {
        let pool = pool().await;
        let w = upsert_work_from_mangadex(&pool, "md-xsrc", &slime_input())
            .await
            .unwrap();
        let md = find_source_series_id(&pool, "mangadex", "mangadex", "md-xsrc")
            .await
            .unwrap()
            .unwrap();
        upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "en1".into(),
                number: Some("1".into()),
                lang: Some("en".into()),
                published_at: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // A Spanish MangaDex chapter, NEWER than anything English. Must stay ignored.
        upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "es9".into(),
                number: Some("9".into()),
                lang: Some("es".into()),
                published_at: Some("2026-09-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            latest_english_chapter_at(&pool, &w)
                .await
                .unwrap()
                .as_deref(),
            Some("2026-01-01T00:00:00Z"),
            "only the mirror has chapters yet, and the Spanish one is not one of them"
        );

        // The same work gains a scanlator source that is three months ahead of the mirror —
        // the ordinary case for a licensed series, and the one the old predicate could not see.
        upsert_source_series(&pool, &w, "suwayomi", "asura", "77", None, false)
            .await
            .unwrap();
        let sy = find_source_series_id(&pool, "suwayomi", "asura", "77")
            .await
            .unwrap()
            .unwrap();
        replace_source_chapters(
            &pool,
            &sy,
            &[suwayomi_spine_input(
                1,
                "Chapter 40",
                40.0,
                None,
                Some(
                    &chrono::DateTime::parse_from_rfc3339("2026-04-01T00:00:00Z")
                        .unwrap()
                        .timestamp_millis()
                        .to_string(),
                ),
            )],
        )
        .await
        .unwrap();

        let got = latest_english_chapter_at(&pool, &w).await.unwrap().unwrap();
        assert!(
            got.starts_with("2026-04-01"),
            "the Suwayomi chapter is the work's newest and must win; got {got}"
        );
        assert!(
            !got.starts_with("2026-09-01"),
            "the Spanish mirror chapter must STILL be ignored — the language filter survives \
             the source filter's removal; got {got}"
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
        // The live read path is the spine, so the fixture has to reach it — the drain is
        // how the 564,259 pre-Phase-B Suwayomi rows got there in production. 366 has no
        // cached chapters, so the drain's `EXISTS` guard skips it and the authoritative
        // tie-break below is still being asked a real question.
        assert_eq!(spine::drain_suwayomi_series(&pool).await.unwrap(), 2);

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
        // Into the spine, which is what `work_source_chapters` reads since the Phase B
        // switchover; both mappings have cached chapters, so both materialise.
        assert_eq!(spine::drain_suwayomi_series(&pool).await.unwrap(), 2);

        // Aggregate count = 3 distinct numbers (1,2,3), not 4 rows.
        assert_eq!(aggregate_chapter_count(&pool, &work).await.unwrap(), 3);
        // Raw rows carry per-source availability (number 1 from BOTH sources).
        let rows = work_source_chapters(&pool, &work).await.unwrap();
        let n1: Vec<&str> = rows
            .iter()
            .filter(|r| r.number == Some(1.0))
            .map(|r| r.suwayomi_manga_id.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(n1.len(), 2, "chapter 1 available from two sources");
        assert!(n1.contains(&"333") && n1.contains(&"999"));
    }

    /// A `WorkChapterRow` reduced to its identity + everything that renders, so two
    /// producers of the same chapter list can be compared as multisets. Sorted rather
    /// than order-compared because neither producer has an `ORDER BY` on its chapter
    /// reads and the only consumer re-sorts (see `work_source_chapters`).
    fn comparable(rows: Vec<WorkChapterRow>) -> Vec<String> {
        let mut out: Vec<String> = rows
            .into_iter()
            .map(|r| {
                format!(
                    "{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                    r.source_type,
                    r.source_id,
                    r.chapter_id,
                    r.key,
                    r.number,
                    r.label,
                    r.title,
                    r.scanlator,
                    // F12: the two implementations must also agree about WHEN, or the
                    // series page's dates would depend on which query served them.
                    r.released_at,
                    // Per-copy hand-off: a copy the reader opens from the series page must
                    // redirect off-site exactly when the spine says it is external.
                    r.external_url,
                    // The series page's volume headers: same label, same blank fold.
                    r.volume,
                )
            })
            .collect();
        out.sort();
        out
    }

    /// PHASE B EXIT CRITERION, and after the switchover the proof that it was a no-op: the
    /// live one-query spine version must return exactly what the retired two-branch version
    /// returns — same chapters, same keys, same labels, same per-source attribution — for a
    /// work that exercises every shape at once: two Suwayomi sources (one of them with a
    /// redundant empty mapping that must lose the authoritative tie-break), a MangaDex
    /// spine, a half chapter, a oneshot, and the `-1` sentinel that used to be the
    /// divergence between the two label paths.
    ///
    /// The oracle reads `suwayomi_chapter` and the live path reads `chapter`. Pointing both
    /// sides at the same function would make this pass unconditionally and prove nothing,
    /// which is the only reason `work_source_chapters_two_branch` still exists.
    #[tokio::test]
    async fn the_spine_query_matches_the_two_branch_version() {
        let pool = pool().await;
        let work = upsert_work_from_mangadex(
            &pool,
            "md-both",
            &WorkInput {
                primary_title: Some("Both Halves".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let md_ssid = find_source_series_id(&pool, "mangadex", "mangadex", "md-both")
            .await
            .unwrap()
            .unwrap();
        // Every scanlator state a MangaDex row can be in (named, "No Group", the backfill's
        // `''`, never-asked NULL) plus an off-site copy, so the two versions must also agree
        // per COPY — what the series page's version list and the reader's switcher read.
        for (ext, number, title, scanlator, external_url) in [
            (
                "u-1",
                Some("1"),
                Some("The Start"),
                Some("Uwaki Scans"),
                None,
            ),
            ("u-1b", Some("1"), Some("The Start"), Some("No Group"), None),
            ("u-15", Some("1.5"), Some("An Interlude"), Some(""), None),
            (
                "u-one",
                Some("Oneshot"),
                Some("A Day Out"),
                None,
                Some("https://mangaplus.shueisha.co.jp/viewer/1"),
            ),
            ("u-null", None, Some("Chapter 3: The 100 Kings"), None, None),
        ] {
            upsert_chapter(
                &pool,
                &md_ssid,
                &ChapterInput {
                    external_id: ext.into(),
                    number: number.map(Into::into),
                    lang: Some("en".into()),
                    title: title.map(Into::into),
                    published_at: Some("2026-01-01T00:00:00Z".into()),
                    scanlator: scanlator.map(Into::into),
                    external_url: external_url.map(Into::into),
                    // A real volume label, and a blank one both versions must fold to None.
                    volume: match ext {
                        "u-1" => Some("3".into()),
                        "u-15" => Some("  ".into()),
                        _ => None,
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        // A non-English mirrored chapter, which BOTH versions must exclude.
        upsert_chapter(
            &pool,
            &md_ssid,
            &ChapterInput {
                external_id: "u-es".into(),
                number: Some("1".into()),
                lang: Some("es".into()),
                title: Some("El Principio".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Two Suwayomi sources, plus a redundant same-source mapping with no chapters
        // that the authoritative rule must drop.
        for (src, key) in [("asura-src", "333"), ("asura-src", "334"), ("other", "999")] {
            upsert_source_series(&pool, &work, "suwayomi", src, key, None, false)
                .await
                .unwrap();
        }
        let sy = [
            (1i64, 333i64, 1.0f64, "Chapter 1", Some("Asura")),
            (2, 333, 1.5, "Chapter 1.5: Omake", Some("Asura")),
            (3, 333, 2.0, "Chapter 2", None),
            // Suwayomi's oneshot sentinel. Its label must come from the NAME, not from
            // the literal "-1", whichever slot the number arrives in.
            (4, 333, -1.0, "Oneshot", Some("Asura")),
            (5, 999, 1.0, "Chapter 1", Some("Other")),
        ];
        for (id, manga, num, name, scanlator) in sy {
            sqlx::query(
                "INSERT INTO suwayomi_chapter \
                   (id, manga_id, name, chapter_number, scanlator, upload_date, page_count, updated_at) \
                 VALUES (?, ?, ?, ?, ?, '1767225600000', 0, '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(manga)
            .bind(name)
            .bind(num)
            .bind(scanlator)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Materialise the Suwayomi half into the spine, exactly as the B3 drain does.
        let moved = spine::drain_suwayomi_series(&pool).await.unwrap();
        assert_eq!(moved, 2, "both non-empty Suwayomi mappings materialised");

        let legacy = work_source_chapters_two_branch(&pool, &work).await.unwrap();
        let spine_rows = work_source_chapters(&pool, &work).await.unwrap();
        // The `''` sentinel never reaches a caller as a label, and a real group does.
        let group_of = |ext: &str| {
            spine_rows
                .iter()
                .find(|r| r.chapter_id == ext)
                .map(|r| r.scanlator.clone())
                .expect("row present")
        };
        assert_eq!(group_of("u-1").as_deref(), Some("Uwaki Scans"));
        assert_eq!(group_of("u-1b").as_deref(), Some("No Group"));
        assert_eq!(
            group_of("u-15"),
            None,
            "the backfill's '' must read as unknown"
        );
        assert_eq!(group_of("u-null"), None);
        let volume_of = |ext: &str| {
            spine_rows
                .iter()
                .find(|r| r.chapter_id == ext)
                .and_then(|r| r.volume.clone())
        };
        assert_eq!(volume_of("u-1").as_deref(), Some("3"));
        assert_eq!(volume_of("u-15"), None, "a blank volume reads as unknown");
        assert_eq!(
            comparable(spine_rows),
            comparable(legacy),
            "the spine query must be a drop-in for the two-branch version"
        );
    }

    /// The clock decision, pinned. `suwayomi_chapter.upload_date` is 13-digit epoch-millis
    /// TEXT and `chapter.published_at` is ISO-8601 TEXT; storing both encodings in one
    /// column makes it sort under BINARY collation, where every '2…' outranks every '1…'.
    #[tokio::test]
    async fn the_spine_stores_one_clock_and_it_is_iso_8601() {
        let pool = pool().await;
        let work = upsert_work_from_mangadex(&pool, "md-clock", &WorkInput::default())
            .await
            .unwrap();
        upsert_source_series(&pool, &work, "suwayomi", "src", "777", None, false)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO suwayomi_chapter \
               (id, manga_id, name, chapter_number, upload_date, page_count, updated_at) \
             VALUES (9, 777, 'Chapter 9', 9.0, '1767225600000', 0, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        spine::drain_suwayomi_series(&pool).await.unwrap();

        let (published, readable): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT published_at, readable_at FROM chapter WHERE external_id = '9'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(published.as_deref(), Some("2026-01-01T00:00:00+00:00"));
        // `readable_at` is set too, so migration 0073's partial index — the MangaDex
        // external-URL backfill's work-list — does not fill with Suwayomi rows it will
        // never touch.
        assert_eq!(readable, published);
        // Everything in the column parses as a date. A millis string would not.
        let unparseable: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chapter \
             WHERE published_at IS NOT NULL AND strftime('%s', published_at) IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unparseable, 0, "one encoding in the column, and it parses");
    }

    /// The key drain must reproduce exactly what the write path would have written —
    /// including the `x:` namespace for unnumbered chapters, which a SQL
    /// `round(number * 100)` would have collapsed onto 0.
    #[tokio::test]
    async fn the_key_drain_reproduces_the_write_paths_keys() {
        let pool = pool().await;
        upsert_work_from_mangadex(&pool, "md-keys", &WorkInput::default())
            .await
            .unwrap();
        let ssid = find_source_series_id(&pool, "mangadex", "mangadex", "md-keys")
            .await
            .unwrap()
            .unwrap();
        for (ext, number, title) in [
            ("k-1", Some("1"), None),
            ("k-105", Some("10.5"), None),
            ("k-extra", Some("Extra"), None),
            ("k-zero", Some("0"), Some("Prologue")),
        ] {
            upsert_chapter(
                &pool,
                &ssid,
                &ChapterInput {
                    external_id: ext.into(),
                    number: number.map(Into::into),
                    title: title.map(Into::into),
                    lang: Some("en".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let written: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT external_id, chapter_key FROM chapter ORDER BY external_id")
                .fetch_all(&pool)
                .await
                .unwrap();

        // Now blank them and let the drain recompute.
        sqlx::query("UPDATE chapter SET chapter_key = NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(spine::drain_chapter_keys(&pool).await.unwrap(), 4);
        assert_eq!(spine::drain_chapter_keys(&pool).await.unwrap(), 0, "drains");

        let drained: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT external_id, chapter_key FROM chapter ORDER BY external_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(drained, written);
        // And the shapes that matter, spelled out.
        let by_ext: std::collections::HashMap<_, _> = drained.into_iter().collect();
        assert_eq!(by_ext["k-1"].as_deref(), Some("100"));
        assert_eq!(by_ext["k-105"].as_deref(), Some("1050"));
        assert_eq!(by_ext["k-zero"].as_deref(), Some("0"));
        assert_eq!(
            by_ext["k-extra"].as_deref(),
            Some("x:k-extra"),
            "an unnumbered chapter must NOT be keyed onto chapter 0"
        );
    }

    /// A chapter that disappears upstream must leave the spine, but a scan that merely
    /// adds one must not churn the rows that were already there — `created_at` is our
    /// first-sighting evidence and Phase C's ledger seeds from it.
    #[tokio::test]
    async fn the_spine_prunes_vanished_chapters_without_churning_the_survivors() {
        let pool = pool().await;
        let work = upsert_work_from_mangadex(&pool, "md-prune", &WorkInput::default())
            .await
            .unwrap();
        upsert_source_series(&pool, &work, "suwayomi", "src", "555", None, false)
            .await
            .unwrap();
        let ssid = find_source_series_id(&pool, "suwayomi", "src", "555")
            .await
            .unwrap()
            .unwrap();
        let input =
            |id: i64, n: f64| suwayomi_spine_input(id, &format!("Chapter {n}"), n, None, None);

        replace_source_chapters(&pool, &ssid, &[input(1, 1.0), input(2, 2.0)])
            .await
            .unwrap();
        let before: Vec<(String, String, String)> =
            sqlx::query_as("SELECT external_id, id, created_at FROM chapter ORDER BY external_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(before.len(), 2);

        // Chapter 2 is taken down and chapter 3 appears.
        replace_source_chapters(&pool, &ssid, &[input(1, 1.0), input(3, 3.0)])
            .await
            .unwrap();
        let after: Vec<(String, String, String)> =
            sqlx::query_as("SELECT external_id, id, created_at FROM chapter ORDER BY external_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let ids: Vec<&str> = after.iter().map(|(e, _, _)| e.as_str()).collect();
        assert_eq!(ids, vec!["1", "3"], "2 pruned, 3 added");
        assert_eq!(
            after[0], before[0],
            "the surviving chapter keeps its id AND its first-sighting time"
        );
    }

    /// F2, the second-largest defect in the audit: a work whose chapters are all
    /// NON-NUMERIC had no chapter list at all — nothing to click on its series page and a
    /// blank label on /updates. 21,422 works, 18.5% of the catalogue, and every one of
    /// them a oneshot: a legitimate content type with exactly one chapter.
    ///
    /// The old filter was `AND c.number IS NOT NULL AND c.number GLOB '*[0-9]*'`, whose
    /// stated purpose — stop `CAST('Extra' AS REAL) = 0.0` masquerading as chapter 0 — is
    /// still honoured here, but by giving unnumbered chapters their own key instead of
    /// dropping them.
    #[tokio::test]
    async fn a_oneshot_work_has_a_chapter_list_and_never_collides_with_chapter_zero() {
        let pool = pool().await;
        let work = upsert_work_from_mangadex(
            &pool,
            "md-oneshot",
            &WorkInput {
                primary_title: Some("A Oneshot".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ssid = find_source_series_id(&pool, "mangadex", "mangadex", "md-oneshot")
            .await
            .unwrap()
            .expect("the mangadex mapping was ensured by the upsert");
        // Exactly the shapes production carries: MangaDex's own "Oneshot"/"Extra" labels,
        // a NULL number, and — the trap — a real `Chapter 0`, which must stay separate.
        for (ext, number, title) in [
            ("u-one", Some("Oneshot"), Some("A Day Out")),
            ("u-extra", Some("Extra"), None),
            ("u-null", None, Some("Bonus")),
            ("u-zero", Some("0"), Some("Prologue")),
        ] {
            upsert_chapter(
                &pool,
                &ssid,
                &ChapterInput {
                    external_id: ext.into(),
                    number: number.map(Into::into),
                    volume: None,
                    lang: Some("en".into()),
                    title: title.map(Into::into),
                    published_at: Some("2026-01-01T00:00:00Z".into()),
                    readable_at: None,
                    external_url: None,
                    scanlator: None,
                },
            )
            .await
            .unwrap();
        }

        let rows = work_source_chapters(&pool, &work).await.unwrap();
        assert_eq!(rows.len(), 4, "every chapter is listed; got {rows:?}");

        // Each unnumbered chapter is its own row, keyed by its own id — they used to
        // either vanish entirely or collapse onto a single `0` bucket.
        let keys: std::collections::HashSet<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys.len(), 4, "no two chapters share a key; got {keys:?}");
        assert!(
            keys.contains("x:u-one") && keys.contains("x:u-extra") && keys.contains("x:u-null")
        );
        assert!(
            keys.contains("0"),
            "a real Chapter 0 keeps the numeric key, distinct from every oneshot"
        );

        // The label is what the source called it, never an invented number.
        let label_of = |id: &str| {
            rows.iter()
                .find(|r| r.chapter_id == id)
                .map(|r| (r.label.clone(), r.number))
                .unwrap()
        };
        assert_eq!(label_of("u-one"), ("Oneshot".into(), None));
        assert_eq!(label_of("u-extra"), ("Extra".into(), None));
        assert_eq!(label_of("u-null"), ("Bonus".into(), None));
        assert_eq!(label_of("u-zero"), ("0".into(), Some(0.0)));

        // And the count Browse renders stops reading "No chapters yet": 3 unnumbered
        // chapters plus chapter 0.
        assert_eq!(aggregate_chapter_count(&pool, &work).await.unwrap(), 4);
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
    async fn merge_works_preserves_library_preferences_and_progress_on_both_copies() {
        let pool = pool().await;
        let source = create_work(&pool, &WorkInput::default()).await.unwrap();
        let target = create_work(&pool, &WorkInput::default()).await.unwrap();
        for user in ["both", "fallback", "source-only"] {
            sqlx::query("INSERT INTO users (id, username, email, password_hash, created_at) VALUES (?, ?, ?, 'h', 'now')")
                .bind(user).bind(user).bind(format!("{user}@x"))
                .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO user_library (user_id, series_id, created_at, status, is_favorite) VALUES (?, ?, '2026-01-01T00:00:00Z', 'reading', 1)")
                .bind(user).bind(&source).execute(&pool).await.unwrap();
        }
        for (user, status) in [("both", Some("onhold")), ("fallback", None)] {
            sqlx::query("INSERT INTO user_library (user_id, series_id, created_at, status, is_favorite) VALUES (?, ?, '2026-02-01T00:00:00Z', ?, 0)")
                .bind(user).bind(&target).bind(status)
                .execute(&pool).await.unwrap();
        }
        for (work, chapter, page, read) in [
            (&source, "chapter-24", 7i64, 1i64),
            (&target, "chapter-26", 3, 0),
        ] {
            sqlx::query("INSERT INTO canonical_progress (user_id, chapter_id, work_id, last_page_read, read, updated_at) VALUES ('both', ?, ?, ?, ?, 'now')")
                .bind(chapter).bind(work).bind(page).bind(read)
                .execute(&pool).await.unwrap();
        }

        merge_works(&pool, &source, &target).await.unwrap();

        for (user, expected_status) in [
            ("both", "onhold"),
            ("fallback", "reading"),
            ("source-only", "reading"),
        ] {
            let rows: Vec<(String, String, Option<String>, i64)> = sqlx::query_as(
                "SELECT series_id, created_at, status, is_favorite FROM user_library WHERE user_id = ?",
            )
            .bind(user)
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(rows.len(), 1, "only the surviving library card remains");
            assert_eq!(rows[0].0, target);
            assert_eq!(rows[0].1, "2026-01-01T00:00:00Z");
            assert_eq!(rows[0].2.as_deref(), Some(expected_status));
            assert_eq!(rows[0].3, 1, "a favourite on either copy survives");
        }
        let progress: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT chapter_id, last_page_read, read FROM canonical_progress WHERE work_id = ? ORDER BY chapter_id",
        )
        .bind(&target)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            progress,
            vec![("chapter-24".into(), 7, 1), ("chapter-26".into(), 3, 0)]
        );
        let orphan_progress: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM canonical_progress WHERE work_id = ?")
                .bind(&source)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(orphan_progress, 0);
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

        async fn count(pool: &Database, sql: &str, wid: &str) -> i64 {
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
                // `work_fts` indexes a work only once it has a source (any source, since
                // 0071 — see `refresh_work_fts`). MangaDex here because this test is about
                // the ranking tiers, not the corpus.
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

    /// Migration 0071. A work reachable only through Suwayomi must be SEARCHABLE, not just
    /// browsable.
    ///
    /// 0052 indexed only MangaDex-anchored works, so 1,824 production works — including the
    /// reported "My Brother is a Vicious Dog" — matched no query at any spelling while
    /// sitting visibly in the Browse grid. The sourceless case is asserted alongside it
    /// because it is the OTHER half of the predicate: widening the corpus must not start
    /// indexing shells with nothing to open, which is the exclusion `browse_catalogue`
    /// (0069) already makes.
    #[tokio::test]
    async fn work_fts_indexes_suwayomi_only_works_and_still_excludes_the_sourceless() {
        let pool = pool().await;
        let mk = |title: &'static str| {
            let pool = pool.clone();
            async move {
                create_work(
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
                .unwrap()
            }
        };

        let suwa = mk("My Brother is a Vicious Dog").await;
        upsert_source_series(&pool, &suwa, "suwayomi", "suwayomi", "16635", None, false)
            .await
            .unwrap();
        let md = mk("My Brother is a Mild Dog").await;
        upsert_source_series(&pool, &md, "mangadex", "mangadex", "u-md", None, false)
            .await
            .unwrap();
        // No `upsert_source_series` at all: nothing to open on either path.
        let orphan = mk("My Brother is a Sourceless Dog").await;

        refresh_work_fts(&pool).await.unwrap();

        let (total, ids) = search_works_fts(&pool, "my brother is a", false, 1, 20)
            .await
            .unwrap();
        assert!(
            ids.contains(&suwa),
            "a Suwayomi-only work must be findable — this is the 0052 bug; got {ids:?}"
        );
        assert!(
            ids.contains(&md),
            "the MangaDex work still matches; got {ids:?}"
        );
        assert!(
            !ids.contains(&orphan),
            "a work with no source has no page to open, so it must stay out of the index; \
             got {ids:?}"
        );
        assert_eq!(total, 2, "`total` counts exactly the indexed matches");
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

    // ---- admin split (detach sources onto a new work) ----

    /// A work with one Suwayomi mapping per `keys`, each carrying one chapter.
    ///
    /// `last_seen` is stamped explicitly and ascending: `work_source_series_rows` orders by
    /// it DESC, and that order decides which detached row NAMES the new work — a test that
    /// let two rows tie on the same timestamp would assert on an arbitrary winner.
    async fn work_with_suwayomi_sources(
        pool: &Database,
        title: &str,
        keys: &[i64],
    ) -> (String, Vec<String>) {
        let work = create_work(
            pool,
            &WorkInput {
                primary_title: Some(title.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut ids = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            insert_suwayomi_series(pool, *key, Some("en")).await;
            let ss = upsert_source_series(
                pool,
                &work,
                "suwayomi",
                "src",
                &key.to_string(),
                None,
                false,
            )
            .await
            .unwrap();
            sqlx::query("UPDATE source_series SET last_seen = ? WHERE id = ?")
                .bind(format!("2026-01-0{}T00:00:00Z", i + 1))
                .bind(&ss)
                .execute(pool)
                .await
                .unwrap();
            ids.push(ss);
        }
        (work, ids)
    }

    async fn add_chapter(pool: &Database, ss_id: &str, chapter_key: &str) -> String {
        let id = format!("ch_{ss_id}_{chapter_key}");
        sqlx::query(
            "INSERT INTO chapter (id, source_series_id, external_id, number, lang, chapter_key, created_at) \
             VALUES (?, ?, ?, ?, 'en', ?, '2026-01-01T00:00:00Z')",
        )
        .bind(&id)
        .bind(ss_id)
        .bind(&id)
        .bind(chapter_key)
        .bind(chapter_key)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn work_of(pool: &Database, ss_id: &str) -> String {
        sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = ?")
            .bind(ss_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn split_moves_the_mapping_and_carries_its_chapters_without_writing_any() {
        let pool = pool().await;
        let (work, ss) = work_with_suwayomi_sources(&pool, "Ouroboros", &[500, 501]).await;
        let keep_ch = add_chapter(&pool, &ss[0], "1").await;
        let move_ch = add_chapter(&pool, &ss[1], "7").await;

        // ss[1] has the later `last_seen`, so it heads the picker's list and names the new
        // work after its OWN Suwayomi title — not the shared title that caused the merge.
        let out = split_source_series(&pool, &work, &[ss[1].clone()], Some("u_admin"))
            .await
            .unwrap();
        assert_eq!(out.title, "Series 501");
        assert_eq!(out.moved_sources, 1);
        // No MangaDex anchor anywhere in the detached set, so the reader navigates by the
        // numeric Suwayomi key (0069) — `w_…` would 404 in `canonical_series`.
        assert_eq!(out.new_reader_id, "501");

        assert_eq!(work_of(&pool, &ss[0]).await, work);
        assert_eq!(work_of(&pool, &ss[1]).await, out.new_work_id);

        // The point of the whole design: the chapter rows are untouched — same ids, same
        // `source_series_id`, nothing deleted — and they follow the mapping purely because
        // that is what they are keyed by.
        let owner = |ch: &str| {
            let pool = pool.clone();
            let ch = ch.to_string();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT s.work_id FROM chapter c JOIN source_series s ON s.id = c.source_series_id \
                      WHERE c.id = ?",
                )
                .bind(&ch)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(owner(&keep_ch).await, work);
        assert_eq!(owner(&move_ch).await, out.new_work_id);
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            total, 2,
            "a split must not delete or duplicate chapter rows"
        );

        // The new work is on Browse and in search under its own name straight away, rather
        // than only after the next periodic rebuild.
        let browsed: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM browse_catalogue WHERE work_id = ?")
                .bind(&out.new_work_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            browsed, 1,
            "the new work must be projected into browse_catalogue"
        );
        let (_, found) = search_works_fts(&pool, "Series 501", false, 1, 10)
            .await
            .unwrap();
        assert!(
            found.contains(&out.new_work_id),
            "the new work must be searchable under its own title; got {found:?}"
        );
    }

    #[tokio::test]
    async fn split_records_the_pair_so_the_consolidator_cannot_re_merge_it() {
        let pool = pool().await;
        let (work, ss) = work_with_suwayomi_sources(&pool, "Ouroboros", &[510, 511]).await;
        let out = split_source_series(&pool, &work, &[ss[1].clone()], Some("u_admin"))
            .await
            .unwrap();

        // Unordered: the sweep picks its own survivor, so the guard has to answer the same
        // either way round.
        assert!(is_split_pair(&pool, &work, &out.new_work_id).await.unwrap());
        assert!(is_split_pair(&pool, &out.new_work_id, &work).await.unwrap());
        assert!(!is_split_pair(&pool, &work, "w_unrelated").await.unwrap());

        // The corroboration `consolidate_gate` needs must NOT have been copied over: the two
        // halves share a title by construction, so a copied year/author/pHash would let the
        // automated sweep destroy the split on its next pass even with the guard in place.
        let (year, author, phash): (Option<i64>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT year, author, cover_phash FROM work WHERE id = ?")
                .bind(&out.new_work_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(year.is_none() && author.is_none() && phash.is_none());
    }

    #[tokio::test]
    async fn split_refuses_an_empty_pick_a_foreign_row_and_the_last_source() {
        let pool = pool().await;
        let (work, ss) = work_with_suwayomi_sources(&pool, "Ouroboros", &[520, 521]).await;

        assert!(split_source_series(&pool, &work, &[], None).await.is_err());

        // A row of a DIFFERENT work is named in the error, because the usual cause is a
        // picker that loaded before someone else merged or split this work.
        let (_, other) = work_with_suwayomi_sources(&pool, "Elsewhere", &[522]).await;
        let err = split_source_series(&pool, &work, &[other[0].clone()], None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&other[0]),
            "error should name the missing row: {err}"
        );

        // Detaching everything would leave a work with no `source_series` row, which
        // `browse_catalogue` and `work_fts` both exclude — it would vanish from Browse and
        // search while keeping its reviews and library entries.
        assert!(
            split_source_series(&pool, &work, &[ss[0].clone(), ss[1].clone()], None)
                .await
                .is_err()
        );
        // Every rejection rolls back whole: nothing moved, no work was minted.
        assert_eq!(work_of(&pool, &ss[0]).await, work);
        assert_eq!(work_of(&pool, &ss[1]).await, work);
    }

    #[tokio::test]
    async fn split_takes_the_mangadex_anchor_with_the_detached_mapping() {
        let pool = pool().await;
        let work = upsert_work_from_mangadex(&pool, "md-split", &slime_input())
            .await
            .unwrap();
        insert_suwayomi_series(&pool, 530, Some("en")).await;
        let sw = upsert_source_series(&pool, &work, "suwayomi", "src", "530", None, false)
            .await
            .unwrap();
        let md: String = sqlx::query_scalar(
            "SELECT id FROM source_series WHERE work_id = ? AND source_type = 'mangadex'",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();

        let out = split_source_series(&pool, &work, std::slice::from_ref(&md), Some("u_admin"))
            .await
            .unwrap();
        // A detached MangaDex mapping makes the new work anchored, so it addresses by `w_…`.
        assert_eq!(out.new_reader_id, out.new_work_id);
        assert_eq!(work_of(&pool, &md).await, out.new_work_id);
        assert_eq!(work_of(&pool, &sw).await, work);
        // `work_external_id` is PK (provider, external_id) — globally unique, not per-work —
        // so the uuid has to move with the mapping or it claims an anchor the original no
        // longer has a mapping for.
        let anchor: String = sqlx::query_scalar(
            "SELECT work_id FROM work_external_id WHERE provider = 'mangadex' AND external_id = 'md-split'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(anchor, out.new_work_id);
    }

    #[tokio::test]
    async fn split_moves_release_events_only_where_the_detached_row_was_the_sole_source() {
        let pool = pool().await;
        let (work, ss) = work_with_suwayomi_sources(&pool, "Ouroboros", &[540, 541]).await;
        // Key "1" is carried by BOTH sources; key "9" only by the one being detached.
        add_chapter(&pool, &ss[0], "1").await;
        add_chapter(&pool, &ss[1], "1").await;
        add_chapter(&pool, &ss[1], "9").await;
        for key in ["1", "9"] {
            sqlx::query(
                "INSERT INTO release_event (work_id, chapter_key, first_seen_at, first_source_series_id, label) \
                 VALUES (?, ?, 1000, ?, ?)",
            )
            .bind(&work)
            .bind(key)
            .bind(&ss[1])
            .bind(format!("Ch. {key}"))
            .execute(&pool)
            .await
            .unwrap();
        }

        let out = split_source_series(&pool, &work, &[ss[1].clone()], Some("u_admin"))
            .await
            .unwrap();

        let keys_of = |w: &str| {
            let pool = pool.clone();
            let w = w.to_string();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT chapter_key FROM release_event WHERE work_id = ? ORDER BY chapter_key",
                )
                .bind(&w)
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        // "1" stays: the ledger's law is earliest-ANYWHERE, and a remaining source still
        // carries that chapter, so the original keeps its (possibly earlier) event.
        assert_eq!(keys_of(&work).await, vec!["1".to_string()]);
        assert_eq!(keys_of(&out.new_work_id).await, vec!["9".to_string()]);
    }
}

#[cfg(test)]
include!("../../bench/sqlite_contention.rs");
