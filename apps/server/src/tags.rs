//! The tag vocabulary behind Browse v2's include/exclude tag filter (migration 0106).
//!
//! THREE JOBS, ONE MODULE, because all three are "what is a tag's stable identity":
//!
//! 1. [`slug`] — the URL-safe id a tag is filtered by (`TagInfo.id`). `work_tag.tag` is a
//!    DISPLAY name ("Boys' Love"); the reader cannot put that in a URL, and two writers can
//!    spell one tag two ways. Every writer stores `slug(tag)` in `work_tag.tag_key`, and
//!    [`fill_tag_keys`] repairs any row that arrived without one.
//! 2. [`rebuild_tag_table`] — the `tag` table: one row per key with its canonical name,
//!    group, MangaDex uuid and NSFW-gated work counts, rebuilt in the same transaction as the
//!    `browse_catalogue` generation it counts.
//! 3. [`map_suwayomi_genres`] / [`sync_suwayomi_tags`] — Suwayomi-only works carry their
//!    source's FREE-TEXT genre list (`suwayomi_series.genre`) and no `work_tag` rows at all,
//!    so a tag filter could never return them. Genres that name a MangaDex tag unambiguously
//!    are written as `source = 'suwayomi'` rows under the canonical MangaDex name.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;
use sqlx::{SqliteConnection, SqlitePool};

/// Longest tag id the filter accepts. Real slugs top out around 20 characters ("traditional-
/// games"); this only bounds what a client can make the server bind and memo-key.
pub const MAX_TAG_ID_LEN: usize = 64;

/// The URL-safe id of a tag: its name lowercased, every run of non-ASCII-alphanumerics
/// folded into one `-`, leading/trailing `-` trimmed.
///
/// `"Boys' Love"` -> `"boys-love"`, `"Sci-Fi"` -> `"sci-fi"`, `"4-Koma"` -> `"4-koma"`,
/// `"Slice of Life"` -> `"slice-of-life"`.
///
/// ASCII only, so the id is URL-safe without percent-encoding. A name with NO ASCII
/// alphanumerics at all (an admin's free-text "恋愛") would slug to the empty string and
/// collide with every other such name, so it gets a stable hash-derived id instead
/// (`"tag-" + 12 hex chars of SHA-256(name)`) — distinct per name, still URL-safe.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() && !name.trim().is_empty() {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(name.trim().as_bytes());
        return format!("tag-{}", &hex::encode(digest)[..12]);
    }
    out
}

/// Whether `id` could be a tag id at all: non-empty, bounded, `[a-z0-9-]` only. Anything
/// else is not an error — it simply matches no tag — but checking lets the filter drop
/// garbage before it reaches the memo key.
pub fn is_plausible_tag_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_TAG_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// MangaDex's tag vocabulary: `(English name, group)`. The group words are MangaDex's own
/// (`attributes.group`), which is what `work_tag.tag_group` stores for ingested rows.
///
/// Used for two things that must not depend on what happens to be in the database: mapping
/// Suwayomi's free-text genres onto canonical names, and giving a known tag its group when the
/// only rows carrying it were written by a writer that does not know groups (admin curation).
/// Names only — the MangaDex uuids come from ingest (`work_tag.tag_id`), never from here.
pub const MANGADEX_TAGS: &[(&str, &str)] = &[
    // format
    ("4-Koma", "format"),
    ("Adaptation", "format"),
    ("Anthology", "format"),
    ("Award Winning", "format"),
    ("Doujinshi", "format"),
    ("Fan Colored", "format"),
    ("Full Color", "format"),
    ("Long Strip", "format"),
    ("Official Colored", "format"),
    ("Oneshot", "format"),
    ("Self-Published", "format"),
    ("User Created", "format"),
    ("Web Comic", "format"),
    // genre
    ("Action", "genre"),
    ("Adventure", "genre"),
    ("Boys' Love", "genre"),
    ("Comedy", "genre"),
    ("Crime", "genre"),
    ("Drama", "genre"),
    ("Fantasy", "genre"),
    ("Girls' Love", "genre"),
    ("Historical", "genre"),
    ("Horror", "genre"),
    ("Isekai", "genre"),
    ("Magical Girls", "genre"),
    ("Mecha", "genre"),
    ("Medical", "genre"),
    ("Mystery", "genre"),
    ("Philosophical", "genre"),
    ("Psychological", "genre"),
    ("Romance", "genre"),
    ("Sci-Fi", "genre"),
    ("Slice of Life", "genre"),
    ("Sports", "genre"),
    ("Superhero", "genre"),
    ("Thriller", "genre"),
    ("Tragedy", "genre"),
    ("Wuxia", "genre"),
    // theme
    ("Aliens", "theme"),
    ("Animals", "theme"),
    ("Cooking", "theme"),
    ("Crossdressing", "theme"),
    ("Delinquents", "theme"),
    ("Demons", "theme"),
    ("Genderswap", "theme"),
    ("Ghosts", "theme"),
    ("Gyaru", "theme"),
    ("Harem", "theme"),
    ("Incest", "theme"),
    ("Loli", "theme"),
    ("Mafia", "theme"),
    ("Magic", "theme"),
    ("Martial Arts", "theme"),
    ("Military", "theme"),
    ("Monster Girls", "theme"),
    ("Monsters", "theme"),
    ("Music", "theme"),
    ("Ninja", "theme"),
    ("Office Workers", "theme"),
    ("Police", "theme"),
    ("Post-Apocalyptic", "theme"),
    ("Reincarnation", "theme"),
    ("Reverse Harem", "theme"),
    ("Samurai", "theme"),
    ("School Life", "theme"),
    ("Shota", "theme"),
    ("Supernatural", "theme"),
    ("Survival", "theme"),
    ("Time Travel", "theme"),
    ("Traditional Games", "theme"),
    ("Vampires", "theme"),
    ("Video Games", "theme"),
    ("Villainess", "theme"),
    ("Virtual Reality", "theme"),
    ("Zombies", "theme"),
    // content
    ("Gore", "content"),
    ("Sexual Violence", "content"),
];

/// Free-text spellings Suwayomi sources use for a MangaDex tag, keyed by [`match_key`] and
/// naming the canonical [`MANGADEX_TAGS`] entry.
///
/// Only spellings with ONE reasonable reading are here. Deliberately absent:
/// * `Ecchi`, `Smut`, `Hentai`, `Mature`, `Adult` — MangaDex has no such TAG; it expresses
///   them as a content RATING, and inventing a rating from a genre string is the NSFW
///   escalation path's job (`genre_is_nsfw`), not this mapper's.
/// * `Webtoon(s)` — a source uses it for "Korean", for "long strip" and for "web comic"
///   interchangeably; picking one would mis-tag the other two. Format detection already reads
///   it (`resolve_comic_type`).
/// * `Shounen`/`Shoujo`/`Seinen`/`Josei` — demographics, not tags (see [`DEMOGRAPHIC_WORDS`]).
const SUWAYOMI_ALIASES: &[(&str, &str)] = &[
    ("sciencefiction", "Sci-Fi"),
    ("shounenai", "Boys' Love"),
    ("shonenai", "Boys' Love"),
    ("yaoi", "Boys' Love"),
    ("bl", "Boys' Love"),
    ("shoujoai", "Girls' Love"),
    ("shojoai", "Girls' Love"),
    ("yuri", "Girls' Love"),
    ("gl", "Girls' Love"),
    ("martialart", "Martial Arts"),
    ("magicalgirl", "Magical Girls"),
    ("genderbender", "Genderswap"),
    ("school", "School Life"),
    ("videogame", "Video Games"),
    ("vampire", "Vampires"),
    ("zombie", "Zombies"),
    ("ghost", "Ghosts"),
    ("monster", "Monsters"),
    ("monstergirl", "Monster Girls"),
    ("demon", "Demons"),
    ("alien", "Aliens"),
    ("animal", "Animals"),
    ("delinquent", "Delinquents"),
    ("officeworker", "Office Workers"),
    ("sport", "Sports"),
    ("history", "Historical"),
    ("yonkoma", "4-Koma"),
    ("superheroes", "Superhero"),
    ("reincarnated", "Reincarnation"),
    ("timetraveling", "Time Travel"),
    ("postapocalypse", "Post-Apocalyptic"),
];

/// Demographic words a Suwayomi source files as a GENRE, keyed by [`match_key`], naming the
/// normalized `browse_catalogue.demographic` word (`catalog::BROWSE_DEMOGRAPHIC_SQL`).
const DEMOGRAPHIC_WORDS: &[(&str, &str)] = &[
    ("shounen", "shounen"),
    ("shonen", "shounen"),
    ("shoujo", "shoujo"),
    ("shojo", "shoujo"),
    ("seinen", "seinen"),
    ("josei", "josei"),
];

/// Case- and punctuation-insensitive comparison key: lowercase ASCII alphanumerics only.
/// `"Sci fi"`, `"Sci-Fi"` and `"SCIFI"` are all `"scifi"`; `"Shounen Ai"` is `"shounenai"`,
/// which is NOT `"shounen"` — so a BL genre can never be read as a demographic.
fn match_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `(name, group)` of the MangaDex tag `name` names exactly (by [`match_key`]), if any.
pub fn known_tag(name: &str) -> Option<(&'static str, &'static str)> {
    let k = match_key(name);
    MANGADEX_TAGS
        .iter()
        .copied()
        .find(|(n, _)| match_key(n) == k)
}

/// What one Suwayomi work's genre list maps to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SuwayomiMapping {
    /// Canonical `(name, group)` pairs, deduped, in first-seen order.
    pub tags: Vec<(&'static str, &'static str)>,
    /// The demographic the genres name — `Some` only when they name exactly ONE.
    pub demographic: Option<&'static str>,
}

/// Map a Suwayomi source's free-text genre list onto MangaDex tags and a demographic.
///
/// A genre maps when its [`match_key`] equals a canonical tag's or an explicit
/// [`SUWAYOMI_ALIASES`] entry's — EXACT matches only, never substrings, so "Action Comedy"
/// maps to nothing rather than to whichever of the two a substring search finds first.
/// Unmapped genres are dropped: the free text is still shown on the work page through
/// `work_effective_genres`' Suwayomi fallback; this only decides what the FILTER can match.
///
/// A demographic is reported only when unambiguous: "Shounen" + "Seinen" on one work (it
/// happens when two sources disagree) yields none.
pub fn map_suwayomi_genres(genres: &[String]) -> SuwayomiMapping {
    let mut out = SuwayomiMapping::default();
    let mut demos: BTreeSet<&'static str> = BTreeSet::new();
    for g in genres {
        let k = match_key(g);
        if k.is_empty() {
            continue;
        }
        if let Some((_, d)) = DEMOGRAPHIC_WORDS.iter().find(|(w, _)| *w == k) {
            demos.insert(d);
            continue;
        }
        let canonical = MANGADEX_TAGS
            .iter()
            .copied()
            .find(|(n, _)| match_key(n) == k)
            .or_else(|| {
                SUWAYOMI_ALIASES
                    .iter()
                    .find(|(a, _)| *a == k)
                    .and_then(|(_, n)| known_tag(n))
            });
        if let Some(t) = canonical {
            if !out.tags.contains(&t) {
                out.tags.push(t);
            }
        }
    }
    if demos.len() == 1 {
        out.demographic = demos.into_iter().next();
    }
    out
}

/// The writes one [`sync_suwayomi_tags`] pass performs. Computed by the pure
/// [`plan_suwayomi_tags`] so the rules are testable without a database.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SuwayomiTagPlan {
    /// `(work_id, name, group, ord)` rows to insert as `source = 'suwayomi'`.
    pub insert: Vec<(String, &'static str, &'static str, i64)>,
    /// `(work_id, tag)` `source = 'suwayomi'` rows to delete.
    pub delete: Vec<(String, String)>,
    /// `(work_id, demographic)` candidates — applied only where the work has none and no
    /// MangaDex anchor (the SQL guard decides; see [`sync_suwayomi_tags`]).
    pub demographic: Vec<(String, &'static str)>,
}

/// `ord` offset for Suwayomi-derived rows, so they sort after any curated/upstream row a
/// display reader might ever mix them with.
const SUWAYOMI_ORD_BASE: i64 = 1000;

/// Diff the desired Suwayomi-derived tag rows against the existing ones.
///
/// * `genres_by_work` — every Suwayomi genre string of every source of the work.
/// * `mangadex_tagged` — works that carry MangaDex tags. They get NO Suwayomi rows: MangaDex's
///   tags are curated per title and complete, a source's genre list is neither, and mixing the
///   two would let one aggregator's sloppy "Harem" put a work in a filter MangaDex keeps it out
///   of. A work that gains MangaDex tags later has its Suwayomi rows deleted by the next pass.
/// * `existing` — the work's current `work_tag` rows as `tag -> source`, ALL sources: a name
///   another tier already carries is not re-inserted (the insert would be ignored by the
///   `(work_id, tag)` key anyway, but a plan that keeps proposing it is a pass that never
///   settles into a no-op), while only `source = 'suwayomi'` rows are ever deleted.
/// * `demographic_eligible` — works with no demographic and no MangaDex anchor.
///
/// Rows are only ever written under `source = 'suwayomi'`, inserted OR IGNORE against the
/// `(work_id, tag)` primary key, so an admin or MangaDex row of the same name always wins.
pub fn plan_suwayomi_tags(
    genres_by_work: &BTreeMap<String, Vec<String>>,
    mangadex_tagged: &HashSet<String>,
    existing: &HashMap<String, BTreeMap<String, String>>,
    demographic_eligible: &HashSet<String>,
) -> SuwayomiTagPlan {
    let mut plan = SuwayomiTagPlan::default();
    let empty = BTreeMap::new();
    for (work_id, genres) in genres_by_work {
        let mapping = map_suwayomi_genres(genres);
        if let Some(d) = mapping.demographic {
            if demographic_eligible.contains(work_id) {
                plan.demographic.push((work_id.clone(), d));
            }
        }
        let want: Vec<(&'static str, &'static str)> = if mangadex_tagged.contains(work_id) {
            Vec::new()
        } else {
            mapping.tags
        };
        let have = existing.get(work_id).unwrap_or(&empty);
        let want_names: BTreeSet<&str> = want.iter().map(|(n, _)| *n).collect();
        for (name, source) in have {
            if source == "suwayomi" && !want_names.contains(name.as_str()) {
                plan.delete.push((work_id.clone(), name.clone()));
            }
        }
        for (i, (name, group)) in want.into_iter().enumerate() {
            if !have.contains_key(name) {
                plan.insert
                    .push((work_id.clone(), name, group, SUWAYOMI_ORD_BASE + i as i64));
            }
        }
    }
    // A work whose Suwayomi source disappeared (or lost its genres) keeps no derived rows.
    for (work_id, tags) in existing {
        if !genres_by_work.contains_key(work_id) {
            for (name, source) in tags {
                if source == "suwayomi" {
                    plan.delete.push((work_id.clone(), name.clone()));
                }
            }
        }
    }
    plan
}

/// What one [`sync_suwayomi_tags`] pass changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SuwayomiTagSync {
    pub inserted: u64,
    pub deleted: u64,
    pub demographics: u64,
}

/// Bring the `source = 'suwayomi'` half of `work_tag` (and inferred demographics) in line
/// with the Suwayomi cache's genre lists.
///
/// Run at the top of `catalog::refresh_browse_catalogue`, before its transaction, so the
/// rebuild that follows counts and materializes the result in the same cycle. Diff-based —
/// a no-op cycle reads ~14k genre blobs and writes nothing — and one transaction for the
/// writes.
///
/// DEMOGRAPHIC: a Suwayomi genre of "Shounen" (etc.) is written to `work.demographic` ONLY
/// when the work has none AND no MangaDex anchor. MangaDex is authoritative where it exists —
/// a NULL there means "MangaDex says none", which a source's genre string must not overrule —
/// and the `IS NULL` guard means a value, once set (by anyone), is never changed by this pass.
pub async fn sync_suwayomi_tags(pool: &SqlitePool) -> Result<SuwayomiTagSync> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT ss.work_id, sy.genre FROM source_series ss \
           JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
          WHERE ss.source_type = 'suwayomi' AND sy.genre IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut genres_by_work: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (work_id, json) in rows {
        // A malformed blob is skipped, not fatal: it is one source's cache row, and failing
        // the pass would stop every other work's tags from refreshing.
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&json) {
            if !list.is_empty() {
                genres_by_work.entry(work_id).or_default().extend(list);
            }
        }
    }
    let mangadex_tagged: HashSet<String> = sqlx::query_scalar(
        "SELECT DISTINCT t.work_id FROM work_tag t \
          WHERE t.source = 'mangadex' \
            AND t.work_id IN (SELECT work_id FROM source_series WHERE source_type = 'suwayomi')",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    // Every tier's rows for the Suwayomi-linked works (to skip names already carried), plus
    // every derived row anywhere (to delete the ones whose source went away).
    let mut existing: HashMap<String, BTreeMap<String, String>> = HashMap::new();
    for (work_id, tag, source) in sqlx::query_as::<_, (String, String, String)>(
        "SELECT work_id, tag, source FROM work_tag \
          WHERE source = 'suwayomi' \
             OR work_id IN (SELECT work_id FROM source_series WHERE source_type = 'suwayomi')",
    )
    .fetch_all(pool)
    .await?
    {
        existing.entry(work_id).or_default().insert(tag, source);
    }
    let demographic_eligible: HashSet<String> = sqlx::query_scalar(
        "SELECT w.id FROM work w \
          WHERE w.demographic IS NULL \
            AND w.id IN (SELECT work_id FROM source_series WHERE source_type = 'suwayomi') \
            AND NOT EXISTS (SELECT 1 FROM source_series ss \
                             WHERE ss.work_id = w.id AND ss.source_type = 'mangadex')",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    let plan = plan_suwayomi_tags(
        &genres_by_work,
        &mangadex_tagged,
        &existing,
        &demographic_eligible,
    );
    let mut out = SuwayomiTagSync::default();
    if plan.insert.is_empty() && plan.delete.is_empty() && plan.demographic.is_empty() {
        return Ok(out);
    }
    let mut tx = pool.begin().await?;
    for (work_id, tag) in &plan.delete {
        out.deleted += sqlx::query(
            "DELETE FROM work_tag WHERE work_id = ? AND tag = ? AND source = 'suwayomi'",
        )
        .bind(work_id)
        .bind(tag)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    for (work_id, name, group, ord) in &plan.insert {
        out.inserted += sqlx::query(
            "INSERT OR IGNORE INTO work_tag (work_id, tag, ord, source, tag_key, tag_group) \
             VALUES (?, ?, ?, 'suwayomi', ?, ?)",
        )
        .bind(work_id)
        .bind(name)
        .bind(ord)
        .bind(slug(name))
        .bind(group)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    for (work_id, demographic) in &plan.demographic {
        out.demographics += sqlx::query(
            "UPDATE work SET demographic = ? \
              WHERE id = ? AND demographic IS NULL \
                AND NOT EXISTS (SELECT 1 FROM source_series ss \
                                 WHERE ss.work_id = work.id AND ss.source_type = 'mangadex')",
        )
        .bind(demographic)
        .bind(work_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(out)
}

/// Give every `work_tag` row without a `tag_key` its [`slug`], and every row of a KNOWN
/// MangaDex tag name without a `tag_group` its group. Returns the rows updated.
///
/// The safety net under every writer: ingest, admin curation and the Suwayomi mapper all set
/// the key themselves, but a row written by a path that predates migration 0106 (or one added
/// later that forgets) must not be invisible to the tag filter. The group half matters because
/// only the `/manga` ingest that carries `tag_details` writes groups; an admin's "Gore", or an
/// ingest that arrived without details, would otherwise file a content tag under GENRE.
///
/// One UPDATE per DISTINCT name, through `idx_work_tag_tag(tag, work_id)`, so a backfill is
/// ~100 statements rather than ~700k. A name whose only gap is a group we cannot know (admin
/// free text) is skipped rather than rewritten every cycle, so a settled table costs one read.
pub async fn fill_tag_keys(conn: &mut SqliteConnection) -> Result<u64> {
    let names: Vec<(String, bool)> = sqlx::query_as(
        "SELECT tag, MAX(tag_key IS NULL) FROM work_tag \
          WHERE tag_key IS NULL OR tag_group IS NULL GROUP BY tag",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut n = 0;
    for (name, needs_key) in names {
        let group = known_tag(&name).map(|(_, g)| g);
        if !needs_key && group.is_none() {
            continue;
        }
        n += sqlx::query(
            "UPDATE work_tag SET tag_key = COALESCE(tag_key, ?), \
                                 tag_group = COALESCE(tag_group, ?) \
              WHERE tag = ? AND (tag_key IS NULL OR (tag_group IS NULL AND ? IS NOT NULL))",
        )
        .bind(slug(&name))
        .bind(group)
        .bind(&name)
        .bind(group)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(n)
}

/// Rebuild the `tag` table from `work_tag` joined to the CURRENT `browse_catalogue`.
///
/// Called inside `refresh_browse_catalogue`'s transaction, after the upsert and the prune, for
/// the reason the genre facet rebuild beside it gives: a count must describe the generation
/// the filter will actually query. DELETE + INSERT because the table is a few hundred rows.
///
/// NO `source` predicate — the filter (`browse::build_where`) matches a tag on any tier, so the
/// count must too. `COUNT(DISTINCT work_id)` rather than `COUNT(*)`: `work_tag`'s key is
/// `(work_id, tag)`, so two NAMES that slug alike ("Sci fi" by an admin, "Sci-Fi" from
/// MangaDex) are two rows of one work under one key, and the filter counts that work once.
pub async fn rebuild_tag_table(conn: &mut SqliteConnection) -> Result<u64> {
    sqlx::query("DELETE FROM tag").execute(&mut *conn).await?;
    Ok(sqlx::query(
        "INSERT INTO tag (id, name, tag_group, mangadex_id, work_count, safe_work_count) \
         SELECT t.tag_key, \
                COALESCE(MAX(CASE WHEN t.source = 'mangadex' THEN t.tag END), MIN(t.tag)), \
                COALESCE(MAX(CASE WHEN t.tag_group IN ('genre', 'theme', 'format', 'content') \
                                  THEN t.tag_group END), 'genre'), \
                MAX(t.tag_id), \
                COUNT(DISTINCT t.work_id), \
                COUNT(DISTINCT CASE WHEN b.is_nsfw = 0 THEN t.work_id END) \
           FROM work_tag t JOIN browse_catalogue b ON b.work_id = t.work_id \
          WHERE t.tag_key IS NOT NULL AND t.tag_key <> '' \
          GROUP BY t.tag_key",
    )
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// One row of the `tags` query.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TagRow {
    pub id: String,
    pub name: String,
    pub tag_group: String,
    pub count: i64,
}

/// The tag list Browse renders, NSFW-gated exactly like `browse::genre_facets`: an opted-out
/// viewer gets the count over `is_nsfw = 0` works, and a tag only NSFW works carry is not
/// offered at all (its chip would promise results the gate then removes). Ordered by group
/// (genre, theme, format, content — MangaDex's own order), then name.
pub async fn list_tags(pool: &SqlitePool, show_nsfw: bool) -> Result<Vec<TagRow>> {
    // One of two hard-coded identifiers, never input.
    let col = if show_nsfw {
        "work_count"
    } else {
        "safe_work_count"
    };
    let sql = format!(
        "SELECT id, name, tag_group, {col} AS count FROM tag WHERE {col} > 0 \
         ORDER BY CASE tag_group WHEN 'genre' THEN 0 WHEN 'theme' THEN 1 \
                                 WHEN 'format' THEN 2 ELSE 3 END, \
                  name COLLATE NOCASE, id"
    );
    Ok(sqlx::query_as::<_, TagRow>(&sql).fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_the_lowercased_dash_folded_canonical_name() {
        assert_eq!(slug("Boys' Love"), "boys-love");
        assert_eq!(slug("Sci-Fi"), "sci-fi");
        assert_eq!(slug("4-Koma"), "4-koma");
        assert_eq!(slug("Slice of Life"), "slice-of-life");
        assert_eq!(slug("  Post-Apocalyptic  "), "post-apocalyptic");
        assert_eq!(slug("Girls' Love"), "girls-love");
        assert_eq!(slug("--Action!!"), "action");
        // No ASCII alphanumerics: a stable, distinct, URL-safe hash id rather than "".
        let a = slug("恋愛");
        let b = slug("百合");
        assert!(a.starts_with("tag-") && a.len() == 16, "{a}");
        assert_ne!(a, b);
        assert_eq!(a, slug("恋愛"), "stable");
        assert!(is_plausible_tag_id(&a));
        assert_eq!(slug("   "), "");
    }

    /// The reader's `tagSlug` (apps/reader/src/lib/data/browse-filters.ts) must produce
    /// the SAME id for a name — it turns legacy `?genre=` links into `includedTags`, which
    /// are matched against the keys this function writes. Same fixtures as the reader's
    /// `tagSlug follows the server rule…` test; change both together.
    #[test]
    fn slug_matches_the_reader_on_shared_fixtures() {
        assert_eq!(slug("Pokémon"), "pok-mon");
        assert_eq!(slug("Doesn't Matter"), "doesn-t-matter");
        assert_eq!(slug("Shōnen Ai"), "sh-nen-ai");
        assert_eq!(slug("İdol"), "dol");
        assert!(
            slug("恋愛").starts_with("tag-"),
            "hashed; the reader drops these"
        );
    }

    #[test]
    fn every_known_tag_has_a_unique_plausible_slug_and_a_mangadex_group() {
        let mut seen = HashSet::new();
        for (name, group) in MANGADEX_TAGS {
            let s = slug(name);
            assert!(is_plausible_tag_id(&s), "{name} -> {s}");
            assert!(seen.insert(s.clone()), "duplicate slug {s}");
            assert!(["genre", "theme", "format", "content"].contains(group));
        }
        // Every alias names a canonical tag — a typo there would silently map nothing.
        for (alias, target) in SUWAYOMI_ALIASES {
            assert!(known_tag(target).is_some(), "{alias} -> unknown {target}");
            assert_eq!(match_key(alias), *alias, "aliases are stored as match keys");
        }
    }

    #[test]
    fn plausible_tag_ids_are_bounded_lowercase_slugs() {
        assert!(is_plausible_tag_id("boys-love"));
        assert!(is_plausible_tag_id("4-koma"));
        assert!(!is_plausible_tag_id(""));
        assert!(!is_plausible_tag_id("Boys-Love"));
        assert!(!is_plausible_tag_id("a b"));
        assert!(!is_plausible_tag_id("x'; DROP TABLE tag; --"));
        assert!(!is_plausible_tag_id(&"a".repeat(MAX_TAG_ID_LEN + 1)));
    }

    fn g(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn suwayomi_genres_map_case_and_punctuation_insensitively_onto_canonical_tags() {
        let m = map_suwayomi_genres(&g(&[
            "action",
            "Sci fi",
            "SLICE OF LIFE",
            "Shounen Ai",
            "Martial Art",
            "4 koma",
            "Gender Bender",
            "Yuri",
        ]));
        assert_eq!(
            m.tags,
            vec![
                ("Action", "genre"),
                ("Sci-Fi", "genre"),
                ("Slice of Life", "genre"),
                ("Boys' Love", "genre"),
                ("Martial Arts", "theme"),
                ("4-Koma", "format"),
                ("Genderswap", "theme"),
                ("Girls' Love", "genre"),
            ]
        );
        assert_eq!(m.demographic, None);
    }

    #[test]
    fn unmappable_and_ambiguous_genres_map_to_nothing() {
        let m = map_suwayomi_genres(&g(&[
            "Ecchi",
            "Smut",
            "Mature",
            "Webtoons",
            "Action Comedy",
            "Manhwa",
            "",
            "!!!",
        ]));
        assert!(m.tags.is_empty(), "{m:?}");
        assert_eq!(m.demographic, None);
        // Duplicates by spelling collapse to one tag.
        let m = map_suwayomi_genres(&g(&["Sci-Fi", "sci fi", "Science Fiction"]));
        assert_eq!(m.tags, vec![("Sci-Fi", "genre")]);
    }

    #[test]
    fn a_demographic_genre_sets_a_demographic_only_when_unambiguous() {
        let m = map_suwayomi_genres(&g(&["Shounen", "Action"]));
        assert_eq!(m.demographic, Some("shounen"));
        assert_eq!(
            m.tags,
            vec![("Action", "genre")],
            "a demographic is not a tag"
        );
        assert_eq!(
            map_suwayomi_genres(&g(&["Shojo"])).demographic,
            Some("shoujo")
        );
        assert_eq!(
            map_suwayomi_genres(&g(&["Shounen", "Seinen"])).demographic,
            None,
            "two demographics is no demographic"
        );
        // "Shounen Ai" is BL, never the shounen demographic.
        let m = map_suwayomi_genres(&g(&["Shounen Ai"]));
        assert_eq!(m.demographic, None);
        assert_eq!(m.tags, vec![("Boys' Love", "genre")]);
    }

    #[test]
    fn the_plan_diffs_and_never_tags_a_mangadex_tagged_work() {
        let mut by_work = BTreeMap::new();
        by_work.insert("w_new".to_string(), g(&["Action", "Romance", "Seinen"]));
        by_work.insert("w_md".to_string(), g(&["Action", "Harem"]));
        by_work.insert("w_same".to_string(), g(&["Drama"]));
        by_work.insert("w_changed".to_string(), g(&["Horror"]));
        by_work.insert("w_curated".to_string(), g(&["Action", "Shounen"]));
        let md: HashSet<String> = ["w_md".to_string()].into();
        let row = |tag: &str, source: &str| (tag.to_string(), source.to_string());
        let mut existing: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        existing.insert("w_same".into(), [row("Drama", "suwayomi")].into());
        existing.insert("w_changed".into(), [row("Comedy", "suwayomi")].into());
        existing.insert(
            "w_md".into(),
            [row("Harem", "suwayomi"), row("Romance", "mangadex")].into(),
        );
        existing.insert("w_gone".into(), [row("Action", "suwayomi")].into());
        // An admin's row of the same name: not re-inserted, and never deleted.
        existing.insert(
            "w_curated".into(),
            [row("Action", "admin"), row("Mecha", "admin")].into(),
        );
        // `w_curated` already has a demographic, so it is not eligible.
        let eligible: HashSet<String> = ["w_new".to_string()].into();
        let plan = plan_suwayomi_tags(&by_work, &md, &existing, &eligible);
        let mut inserts: Vec<(&str, &str)> = plan
            .insert
            .iter()
            .map(|(w, n, _, _)| (w.as_str(), *n))
            .collect();
        inserts.sort();
        assert_eq!(
            inserts,
            vec![
                ("w_changed", "Horror"),
                ("w_new", "Action"),
                ("w_new", "Romance")
            ]
        );
        let mut deletes: Vec<(&str, &str)> = plan
            .delete
            .iter()
            .map(|(w, n)| (w.as_str(), n.as_str()))
            .collect();
        deletes.sort();
        assert_eq!(
            deletes,
            vec![
                ("w_changed", "Comedy"),
                ("w_gone", "Action"),
                ("w_md", "Harem"),
            ],
            "stale rows go, and a work that gained MangaDex tags loses its derived ones"
        );
        assert_eq!(plan.demographic, vec![("w_new".to_string(), "seinen")]);
        assert!(plan.insert.iter().all(|(_, _, _, ord)| *ord >= 1000));
    }

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn work(pool: &SqlitePool, id: &str, demographic: Option<&str>) {
        sqlx::query(
            "INSERT INTO work (id, primary_title, demographic, created_at, updated_at) \
             VALUES (?, ?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(id)
        .bind(demographic)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn suwayomi_source(pool: &SqlitePool, work_id: &str, sid: i64, genres: &str) {
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES (?, ?, 'suwayomi', ?, '2026-01-01T00:00:00Z')",
        )
        .bind(format!("ss_{sid}"))
        .bind(work_id)
        .bind(sid.to_string())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, genre, status, source_id, updated_at) \
             VALUES (?, 'T', ?, 'ONGOING', '1', '2026-01-01T00:00:00Z')",
        )
        .bind(sid)
        .bind(genres)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn tags_of(pool: &SqlitePool, work_id: &str) -> Vec<(String, String, Option<String>)> {
        sqlx::query_as(
            "SELECT tag, source, tag_key FROM work_tag WHERE work_id = ? ORDER BY tag, source",
        )
        .bind(work_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// The ingest end to end: Suwayomi genres become `source = 'suwayomi'` rows with keys,
    /// never displacing an admin row of the same name, never touching a MangaDex-tagged
    /// work, idempotent, and the demographic lands only on an un-anchored work without one.
    #[tokio::test]
    async fn suwayomi_genres_are_written_as_sourced_tag_rows_without_overriding_curation() {
        let pool = pool().await;
        work(&pool, "w_sw", None).await;
        suwayomi_source(&pool, "w_sw", 1, r#"["Action","sci fi","Shounen","Ecchi"]"#).await;
        work(&pool, "w_admin", None).await;
        suwayomi_source(&pool, "w_admin", 2, r#"["Action","Drama"]"#).await;
        sqlx::query(
            "INSERT INTO work_tag (work_id, tag, ord, source) VALUES ('w_admin', 'Action', 0, 'admin')",
        )
        .execute(&pool)
        .await
        .unwrap();
        work(&pool, "w_md", None).await;
        suwayomi_source(&pool, "w_md", 3, r#"["Harem","Seinen"]"#).await;
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss_md', 'w_md', 'mangadex', '00000000-0000-0000-0000-000000000001', \
                     '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO work_tag (work_id, tag, ord, source, tag_key) \
             VALUES ('w_md', 'Romance', 0, 'mangadex', 'romance')",
        )
        .execute(&pool)
        .await
        .unwrap();
        work(&pool, "w_has_demo", Some("josei")).await;
        suwayomi_source(&pool, "w_has_demo", 4, r#"["Seinen"]"#).await;

        let first = sync_suwayomi_tags(&pool).await.unwrap();
        assert_eq!(first.inserted, 3, "{first:?}");
        assert_eq!(
            tags_of(&pool, "w_sw").await,
            vec![
                ("Action".into(), "suwayomi".into(), Some("action".into())),
                ("Sci-Fi".into(), "suwayomi".into(), Some("sci-fi".into())),
            ]
        );
        assert_eq!(
            tags_of(&pool, "w_admin").await,
            vec![
                ("Action".into(), "admin".into(), None),
                ("Drama".into(), "suwayomi".into(), Some("drama".into())),
            ],
            "the admin's row keeps its tier; only the missing tag is added"
        );
        assert_eq!(
            tags_of(&pool, "w_md").await,
            vec![("Romance".into(), "mangadex".into(), Some("romance".into()))],
            "a MangaDex-tagged work gets no Suwayomi-derived tags"
        );
        let demo = |id: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT demographic FROM work WHERE id = ?")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(demo("w_sw").await.as_deref(), Some("shounen"));
        assert_eq!(demo("w_md").await, None, "MangaDex is authoritative");
        assert_eq!(
            demo("w_has_demo").await.as_deref(),
            Some("josei"),
            "never changed"
        );

        // Idempotent.
        assert_eq!(
            sync_suwayomi_tags(&pool).await.unwrap(),
            SuwayomiTagSync::default()
        );

        // The source's genres change: the stale derived row goes, the new one arrives.
        sqlx::query("UPDATE suwayomi_series SET genre = '[\"Horror\"]' WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let again = sync_suwayomi_tags(&pool).await.unwrap();
        assert_eq!((again.inserted, again.deleted), (1, 2));
        assert_eq!(
            tags_of(&pool, "w_sw").await,
            vec![("Horror".into(), "suwayomi".into(), Some("horror".into()))]
        );
    }

    /// The key backfill and the tag table: a row without a key gets one (and a known name
    /// its group), names that slug alike share one key and are counted once per work, counts
    /// follow `browse_catalogue` and the NSFW gate, and the canonical name prefers MangaDex's.
    #[tokio::test]
    async fn tag_keys_are_backfilled_and_the_tag_table_counts_the_browse_set() {
        let pool = pool().await;
        for (id, nsfw) in [("w_1", 0), ("w_2", 1), ("w_3", 0)] {
            work(&pool, id, None).await;
            sqlx::query(
                "INSERT INTO browse_catalogue (work_id, reader_id, title, is_nsfw, created_at) \
                 VALUES (?, ?, ?, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(id)
            .bind(id)
            .bind(nsfw)
            .execute(&pool)
            .await
            .unwrap();
        }
        work(&pool, "w_unbrowsable", None).await;
        for (w, tag, source, id) in [
            ("w_1", "sci fi", "admin", None),
            ("w_1", "Sci-Fi", "mangadex", Some("uuid-scifi")),
            ("w_2", "Sci-Fi", "mangadex", Some("uuid-scifi")),
            ("w_3", "My Custom Tag", "admin", None),
            ("w_3", "Gore", "admin", None),
            ("w_unbrowsable", "Sci-Fi", "mangadex", Some("uuid-scifi")),
        ] {
            sqlx::query(
                "INSERT INTO work_tag (work_id, tag, ord, source, tag_id, tag_group) \
                 VALUES (?, ?, 0, ?, ?, CASE WHEN ? = 'mangadex' THEN 'genre' END)",
            )
            .bind(w)
            .bind(tag)
            .bind(source)
            .bind(id)
            .bind(source)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(fill_tag_keys(&mut conn).await.unwrap(), 6);
        assert_eq!(fill_tag_keys(&mut conn).await.unwrap(), 0, "idempotent");
        rebuild_tag_table(&mut conn).await.unwrap();
        drop(conn);
        let all = list_tags(&pool, true).await.unwrap();
        let t = |id: &str, name: &str, group: &str, count: i64| TagRow {
            id: id.into(),
            name: name.into(),
            tag_group: group.into(),
            count,
        };
        assert_eq!(
            all,
            vec![
                t("my-custom-tag", "My Custom Tag", "genre", 1),
                t("sci-fi", "Sci-Fi", "genre", 2),
                t("gore", "Gore", "content", 1),
            ],
            "one key for both spellings, counted once per work, MangaDex's name, \
             the unbrowsable work not counted, Gore's group from the vocabulary"
        );
        let safe = list_tags(&pool, false).await.unwrap();
        assert_eq!(
            safe.iter().find(|r| r.id == "sci-fi").map(|r| r.count),
            Some(1),
            "the opted-out count excludes the NSFW work"
        );
        let md_id: Option<String> =
            sqlx::query_scalar("SELECT mangadex_id FROM tag WHERE id = 'sci-fi'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(md_id.as_deref(), Some("uuid-scifi"));
    }
}
