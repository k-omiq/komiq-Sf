//! Scanlators and the admin's AI tags on them (migrations 0116 and 0117).
//!
//! A scanlator is one of two things, and neither is a row anywhere:
//!
//! * a GROUP: the free-text `chapter.scanlator` credit of one chapter copy. MangaDex
//!   credits every chapter; most other sources credit none.
//! * a SITE: a source that translates its own series. Its chapters carry no credit, so
//!   the source itself is the scanlator ([`list_source_tags`] / [`set_source_tag`]).
//!
//! A chapter copy takes the stronger of the tag on its credit and the tag on its source.
//!
//! For groups this module owns the three things built on the credit text:
//!
//! 1. [`name_key`] / [`effective_level`] — the identity of a credit and the tag that
//!    applies to it. A joint credit ("A & B") with no tag of its own takes the strongest
//!    tag of its members.
//! 2. [`list_tags`] / [`set_tag`] — the `scanlator_tag` table.
//! 3. [`group_stats`] / [`page`] — the admin's scanlator list: every credit with its
//!    chapter and series counts, per source.
//!
//! THE LIST IS COUNTED IN ONE SCAN AND MEMOIZED. There is no index on `chapter.scanlator`
//! and no materialized table, so the counts are a full scan of `chapter` joined to
//! `source_series` (about 2 s for 560k chapters). It runs for the admin console only, at
//! most once per [`STATS_TTL`]; search, the filters and paging then work on the memoized
//! rows in memory (some 13k credits).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::Database;
use anyhow::Result;
use chrono::Utc;

/// How MangaDex joins the groups of a joint release (`mangadex::chapter_scanlator`).
const JOINT_SEPARATOR: &str = " & ";

/// Longest credit a tag can name. Real credits are far shorter; this bounds what a
/// client can store.
pub const MAX_NAME_LEN: usize = 200;

/// Rows per page of the admin list.
pub const PAGE_SIZE: usize = 50;

/// The filter id of the MangaDex mirror. A Suwayomi source is filtered by its source id.
/// Same vocabulary as `browse::parse_source`.
pub const MANGADEX_SOURCE: &str = "mangadex";

/// How long one count of the chapter table is reused. Credits appear slowly (a new group
/// starts a series), so a list that is ten minutes old is still the right list. Tags are
/// NOT part of the memo: they are read fresh on every request.
///
/// Zero under test, like `browse::COUNT_TTL`: the memo is process-wide and every test has
/// its own database.
#[cfg(not(test))]
const STATS_TTL: Duration = Duration::from_secs(600);
#[cfg(test)]
const STATS_TTL: Duration = Duration::ZERO;

/// What an admin says about a scanlator. Ordered weakest to strongest, which is what
/// [`effective_level`] takes the maximum over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AiLevel {
    /// AI is part of the work, for example machine translation that a person edits.
    Partial,
    /// The translation is machine made.
    Ai,
}

impl AiLevel {
    /// The word stored in `scanlator_tag.ai_level`.
    pub fn word(self) -> &'static str {
        match self {
            AiLevel::Partial => "partial",
            AiLevel::Ai => "ai",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "partial" => Some(AiLevel::Partial),
            "ai" => Some(AiLevel::Ai),
            _ => None,
        }
    }
}

/// The identity of a credit: trimmed and lowercased, so "Asura Scans" and "asura scans "
/// are one scanlator.
pub fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// The tag that applies to a credit: its own, else the strongest tag among the members of
/// a joint credit. Tagging "B" as AI therefore covers "A & B" too, without a row for every
/// combination. A group whose own name contains " & " is still matched whole first.
pub fn effective_level(name: &str, tags: &HashMap<String, AiLevel>) -> Option<AiLevel> {
    let key = name_key(name);
    if let Some(level) = tags.get(&key) {
        return Some(*level);
    }
    key.split(JOINT_SEPARATOR)
        .filter_map(|member| tags.get(member.trim()))
        .max()
        .copied()
}

/// One stored tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// The spelling the admin tagged.
    pub name: String,
    pub level: AiLevel,
}

/// Every tag, by name. Small (one row per decision an admin made), so it is read on
/// every request and never memoized.
pub async fn list_tags(pool: &Database) -> Result<Vec<Tag>> {
    let rows: Vec<(String, String)> =
        crate::db_types::query_tuple("SELECT name, ai_level FROM scanlator_tag ORDER BY name_key")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(name, word)| AiLevel::from_word(&word).map(|level| Tag { name, level }))
        .collect())
}

/// [`list_tags`] as the lookup [`effective_level`] takes.
pub fn tag_index(tags: &[Tag]) -> HashMap<String, AiLevel> {
    tags.iter().map(|t| (name_key(&t.name), t.level)).collect()
}

/// Set, change or (with `None`) remove the tag on a credit. `false` when nothing changed.
/// The caller validates `name` ([`MAX_NAME_LEN`], not empty).
pub async fn set_tag(pool: &Database, name: &str, level: Option<AiLevel>) -> Result<bool> {
    let key = name_key(name);
    let res = match level {
        Some(level) => {
            sqlx::query(
                "INSERT INTO scanlator_tag (name_key, name, ai_level, updated_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT(name_key) DO UPDATE SET ai_level = excluded.ai_level, \
                     updated_at = excluded.updated_at \
                 WHERE scanlator_tag.ai_level != excluded.ai_level",
            )
            .bind(&key)
            .bind(name.trim())
            .bind(level.word())
            .bind(Utc::now())
            .execute(pool)
            .await?
        }
        None => {
            sqlx::query("DELETE FROM scanlator_tag WHERE name_key = $1")
                .bind(&key)
                .execute(pool)
                .await?
        }
    };
    Ok(res.rows_affected() > 0)
}

/// Longest source id a tag can name. Engine source ids are 64-bit numbers as text.
pub const MAX_SOURCE_ID_LEN: usize = 32;

/// Whether `id` can carry a source tag: a Suwayomi source id (`source_series.source_id`).
/// Not [`MANGADEX_SOURCE`]: MangaDex credits its groups, so its groups are what is tagged.
pub fn is_taggable_source(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SOURCE_ID_LEN
        && id != MANGADEX_SOURCE
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Every tagged source: source id to level. Small and read on every request, like
/// [`list_tags`].
pub async fn list_source_tags(pool: &Database) -> Result<HashMap<String, AiLevel>> {
    let rows: Vec<(String, String)> =
        crate::db_types::query_tuple("SELECT source_id, ai_level FROM source_ai_tag")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, word)| AiLevel::from_word(&word).map(|level| (id, level)))
        .collect())
}

/// Set, change or (with `None`) remove the tag on a source. `false` when nothing changed.
/// The caller validates `source_id` ([`is_taggable_source`]).
pub async fn set_source_tag(
    pool: &Database,
    source_id: &str,
    level: Option<AiLevel>,
) -> Result<bool> {
    let res =
        match level {
            Some(level) => sqlx::query(
                "INSERT INTO source_ai_tag (source_id, ai_level, updated_at) VALUES ($1, $2, $3) \
                 ON CONFLICT(source_id) DO UPDATE SET ai_level = excluded.ai_level, \
                     updated_at = excluded.updated_at \
                 WHERE source_ai_tag.ai_level != excluded.ai_level",
            )
            .bind(source_id)
            .bind(level.word())
            .bind(Utc::now())
            .execute(pool)
            .await?,
            None => {
                sqlx::query("DELETE FROM source_ai_tag WHERE source_id = $1")
                    .bind(source_id)
                    .execute(pool)
                    .await?
            }
        };
    Ok(res.rows_affected() > 0)
}

/// What one source carries of a credit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceStat {
    /// [`MANGADEX_SOURCE`], or a Suwayomi source id ('' when the mapping has none).
    pub source: String,
    pub chapters: i64,
    pub works: i64,
}

/// One credit and where its chapters are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupStat {
    /// The most common spelling of the credit.
    pub name: String,
    pub key: String,
    pub sources: Vec<SourceStat>,
}

impl GroupStat {
    /// Chapter and series counts: on one source, or on all of them. A series that two
    /// sources carry from the same credit counts once per source in the all-sources
    /// number; that is rare and the count is a size hint, not a key.
    fn counts(&self, source: Option<&str>) -> (i64, i64) {
        self.sources
            .iter()
            .filter(|s| source.is_none_or(|want| s.source == want))
            .fold((0, 0), |(c, w), s| (c + s.chapters, w + s.works))
    }
}

/// Count every credit in `chapter`, per source. The one slow query of this module; see
/// the module header. "No Group" and the backfill's empty string are not credits.
async fn count_groups(pool: &Database) -> Result<Vec<GroupStat>> {
    // Grouped by the folded name so two spellings on one series count the series once.
    // SQLite folds ASCII only; the loop below folds the rest. `MIN` picks the spelling
    // that sorts first, which is the capitalized one ("Robo Scans" before "robo scans").
    let rows: Vec<(String, String, String, i64, i64)> = crate::db_types::query_tuple(
        "SELECT MIN(c.scanlator), ss.source_type, COALESCE(ss.source_id, ''), \
                COUNT(*), COUNT(DISTINCT ss.work_id) \
           FROM chapter c \
           JOIN source_series ss ON ss.id = c.source_series_id \
          WHERE c.scanlator IS NOT NULL AND c.scanlator != '' AND c.scanlator != $1 \
          GROUP BY translate(trim(c.scanlator), 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz'), ss.source_type, ss.source_id",
    )
    .bind(crate::mangadex::NO_GROUP)
    .fetch_all(pool)
    .await?;

    // One credit per key, named by the spelling of its biggest source.
    struct Acc {
        name: String,
        name_chapters: i64,
        sources: HashMap<String, (i64, i64)>,
    }
    let mut by_key: HashMap<String, Acc> = HashMap::new();
    for (name, source_type, source_id, chapters, works) in rows {
        let key = name_key(&name);
        if key.is_empty() {
            continue;
        }
        let source = if source_type == MANGADEX_SOURCE {
            MANGADEX_SOURCE.to_string()
        } else {
            source_id
        };
        let acc = by_key.entry(key).or_insert_with(|| Acc {
            name: name.trim().to_string(),
            name_chapters: 0,
            sources: HashMap::new(),
        });
        if chapters > acc.name_chapters {
            acc.name = name.trim().to_string();
            acc.name_chapters = chapters;
        }
        let slot = acc.sources.entry(source).or_insert((0, 0));
        slot.0 += chapters;
        slot.1 += works;
    }
    Ok(by_key
        .into_iter()
        .map(|(key, acc)| GroupStat {
            name: acc.name,
            key,
            sources: acc
                .sources
                .into_iter()
                .map(|(source, (chapters, works))| SourceStat {
                    source,
                    chapters,
                    works,
                })
                .collect(),
        })
        .collect())
}

static STATS: std::sync::Mutex<Option<(Arc<Vec<GroupStat>>, Instant)>> =
    std::sync::Mutex::new(None);
/// Single-flight: two admins opening the page together run one scan.
static STATS_REFILL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cached_stats() -> Option<Arc<Vec<GroupStat>>> {
    let memo = STATS.lock().ok()?;
    let (stats, at) = memo.as_ref()?;
    (at.elapsed() < STATS_TTL).then(|| stats.clone())
}

/// Every credit with its counts: memoized for [`STATS_TTL`], counted otherwise.
pub async fn group_stats(pool: &Database) -> Result<Arc<Vec<GroupStat>>> {
    if let Some(hit) = cached_stats() {
        return Ok(hit);
    }
    let _guard = STATS_REFILL.lock().await;
    if let Some(hit) = cached_stats() {
        return Ok(hit);
    }
    let stats = Arc::new(count_groups(pool).await?);
    if let Ok(mut memo) = STATS.lock() {
        *memo = Some((stats.clone(), Instant::now()));
    }
    Ok(stats)
}

/// Which credits the list shows, by the tag that applies to them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiFilter {
    /// Tagged AI or partial AI.
    Tagged,
    Ai,
    Partial,
    Untagged,
}

/// One request for the admin list.
#[derive(Clone, Debug, Default)]
pub struct ListQuery {
    /// Case-insensitive part of the name.
    pub search: Option<String>,
    pub ai: Option<AiFilter>,
    /// Only credits with chapters on this source ([`MANGADEX_SOURCE`] or a source id).
    /// The counts are then that source's.
    pub source: Option<String>,
    /// 1-based.
    pub page: usize,
}

/// One row of the admin list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRow {
    pub name: String,
    pub chapters: i64,
    pub works: i64,
    /// The tag stored for exactly this name.
    pub own: Option<AiLevel>,
    /// The tag that applies ([`effective_level`]).
    pub effective: Option<AiLevel>,
}

/// Filter, order and page the list. Returns the page's rows and the filtered total.
///
/// Order: most chapters first, then by name. A tagged name that no chapter carries any
/// more is still listed (with zero counts), so the admin can find and remove the tag; it
/// is left out under a source filter, where it has nothing to show.
pub fn page(stats: &[GroupStat], tags: &[Tag], q: &ListQuery) -> (Vec<ListRow>, usize) {
    let index = tag_index(tags);
    let search = q.search.as_deref().map(name_key).filter(|s| !s.is_empty());
    let source = q.source.as_deref();

    let mut rows: Vec<ListRow> = stats
        .iter()
        .filter(|g| search.as_deref().is_none_or(|s| g.key.contains(s)))
        .filter_map(|g| {
            let (chapters, works) = g.counts(source);
            (chapters > 0).then(|| ListRow {
                name: g.name.clone(),
                chapters,
                works,
                own: index.get(&g.key).copied(),
                effective: effective_level(&g.name, &index),
            })
        })
        .collect();
    if source.is_none() {
        let counted: std::collections::HashSet<&str> =
            stats.iter().map(|g| g.key.as_str()).collect();
        rows.extend(tags.iter().filter_map(|t| {
            let key = name_key(&t.name);
            let listed = counted.contains(key.as_str());
            let matches = search.as_deref().is_none_or(|s| key.contains(s));
            (!listed && matches).then(|| ListRow {
                name: t.name.clone(),
                chapters: 0,
                works: 0,
                own: Some(t.level),
                effective: Some(t.level),
            })
        }));
    }
    rows.retain(|r| match q.ai {
        None => true,
        Some(AiFilter::Tagged) => r.effective.is_some(),
        Some(AiFilter::Ai) => r.effective == Some(AiLevel::Ai),
        Some(AiFilter::Partial) => r.effective == Some(AiLevel::Partial),
        Some(AiFilter::Untagged) => r.effective.is_none(),
    });
    rows.sort_by(|a, b| {
        b.chapters
            .cmp(&a.chapters)
            .then_with(|| name_key(&a.name).cmp(&name_key(&b.name)))
    });

    let total = rows.len();
    let start = q.page.max(1).saturating_sub(1).saturating_mul(PAGE_SIZE);
    let rows = rows.into_iter().skip(start).take(PAGE_SIZE).collect();
    (rows, total)
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

    fn tags(pairs: &[(&str, AiLevel)]) -> Vec<Tag> {
        pairs
            .iter()
            .map(|(name, level)| Tag {
                name: (*name).to_string(),
                level: *level,
            })
            .collect()
    }

    #[test]
    fn a_credit_takes_its_own_tag_else_the_strongest_member_tag() {
        let index = tag_index(&tags(&[
            ("Robo Scans", AiLevel::Ai),
            ("Half Scans", AiLevel::Partial),
            ("Tiramisu & Strawberry", AiLevel::Partial),
        ]));
        // Own tag, whatever the spelling.
        assert_eq!(effective_level("  robo scans ", &index), Some(AiLevel::Ai));
        assert_eq!(effective_level("Human Scans", &index), None);
        // A joint credit takes a member's tag; the strongest when two are tagged.
        assert_eq!(
            effective_level("Human Scans & Robo Scans", &index),
            Some(AiLevel::Ai)
        );
        assert_eq!(
            effective_level("Half Scans & Human Scans", &index),
            Some(AiLevel::Partial)
        );
        assert_eq!(
            effective_level("Half Scans & Robo Scans", &index),
            Some(AiLevel::Ai)
        );
        // A name that contains the separator is matched whole first.
        assert_eq!(
            effective_level("Tiramisu & Strawberry", &index),
            Some(AiLevel::Partial)
        );
        // Part of a word is not a member.
        assert_eq!(effective_level("Robo Scans Fans", &index), None);
    }

    #[tokio::test]
    async fn set_tag_adds_changes_and_removes_by_folded_name() {
        let pool = pool().await;
        assert!(set_tag(&pool, " Robo Scans ", Some(AiLevel::Ai))
            .await
            .unwrap());
        // Same level again: nothing changed.
        assert!(!set_tag(&pool, "robo scans", Some(AiLevel::Ai))
            .await
            .unwrap());
        assert!(set_tag(&pool, "ROBO SCANS", Some(AiLevel::Partial))
            .await
            .unwrap());
        // One row, and it keeps the spelling first tagged.
        assert_eq!(
            list_tags(&pool).await.unwrap(),
            tags(&[("Robo Scans", AiLevel::Partial)])
        );
        assert!(set_tag(&pool, "robo scans", None).await.unwrap());
        assert!(!set_tag(&pool, "robo scans", None).await.unwrap());
        assert!(list_tags(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_source_tag_is_set_changed_and_removed_by_source_id() {
        let pool = pool().await;
        assert!(set_source_tag(&pool, "77", Some(AiLevel::Ai))
            .await
            .unwrap());
        // Same level again: nothing changed.
        assert!(!set_source_tag(&pool, "77", Some(AiLevel::Ai))
            .await
            .unwrap());
        assert!(set_source_tag(&pool, "77", Some(AiLevel::Partial))
            .await
            .unwrap());
        assert!(set_source_tag(&pool, "88", Some(AiLevel::Ai))
            .await
            .unwrap());
        let tags = list_source_tags(&pool).await.unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags["77"], AiLevel::Partial);
        assert_eq!(tags["88"], AiLevel::Ai);
        assert!(set_source_tag(&pool, "77", None).await.unwrap());
        assert!(!set_source_tag(&pool, "77", None).await.unwrap());
        assert_eq!(list_source_tags(&pool).await.unwrap().len(), 1);
    }

    #[test]
    fn only_an_engine_source_can_carry_a_source_tag() {
        assert!(is_taggable_source("6247824327199706550"));
        assert!(is_taggable_source("-42"));
        // MangaDex credits its groups; they are what is tagged.
        assert!(!is_taggable_source(MANGADEX_SOURCE));
        assert!(!is_taggable_source(""));
        assert!(!is_taggable_source("a b"));
        assert!(!is_taggable_source(&"9".repeat(MAX_SOURCE_ID_LEN + 1)));
    }

    /// Two works: `w1` on MangaDex and on Suwayomi source 77, `w2` on MangaDex only.
    async fn seed(pool: &Database) {
        for work in ["w1", "w2"] {
            sqlx::query(
                "INSERT INTO work (id, primary_title, created_at, updated_at) \
                 VALUES ($1, $2, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            )
            .bind(work)
            .bind(work)
            .execute(pool)
            .await
            .unwrap();
        }
        for (id, work, source_type, source_id) in [
            ("md1", "w1", "mangadex", "mangadex"),
            ("md2", "w2", "mangadex", "mangadex"),
            ("sy1", "w1", "suwayomi", "77"),
        ] {
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
                 VALUES ($1, $2, $3, $4, $5, '2020-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(work)
            .bind(source_type)
            .bind(source_id)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        let chapters: [(&str, Option<&str>); 9] = [
            ("md1", Some("Robo Scans")),
            ("md1", Some("Robo Scans")),
            ("md1", Some("robo scans")),
            ("md2", Some("Robo Scans")),
            ("md1", Some("Human Scans & Robo Scans")),
            ("sy1", Some("Human Scans")),
            // Not credits: MangaDex's "No Group", the backfill's '' and a source that
            // reports nothing.
            ("md1", Some(crate::mangadex::NO_GROUP)),
            ("md1", Some("")),
            ("sy1", None),
        ];
        for (n, (series, scanlator)) in chapters.into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO chapter (id, source_series_id, external_id, scanlator, created_at) \
                 VALUES ($1, $2, $3, $4, '2020-01-01T00:00:00Z')",
            )
            .bind(format!("c{n}"))
            .bind(series)
            .bind(format!("e{n}"))
            .bind(scanlator)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    fn names(rows: &[ListRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[tokio::test]
    async fn the_list_counts_credits_per_source_and_applies_tags() {
        let pool = pool().await;
        seed(&pool).await;
        let stats = group_stats(&pool).await.unwrap();

        // Untagged: three credits, most chapters first, spellings folded.
        let (rows, total) = page(&stats, &[], &ListQuery::default());
        assert_eq!(total, 3);
        assert_eq!(
            rows[0],
            ListRow {
                name: "Robo Scans".into(),
                chapters: 4,
                works: 2,
                own: None,
                effective: None,
            }
        );
        assert_eq!(
            names(&rows),
            ["Robo Scans", "Human Scans", "Human Scans & Robo Scans"]
        );

        // Tag one group: the joint credit takes it; a tag on a name no chapter carries
        // is listed too.
        let tagged = tags(&[
            ("robo scans", AiLevel::Ai),
            ("Gone Scans", AiLevel::Partial),
        ]);
        let (rows, total) = page(&stats, &tagged, &ListQuery::default());
        assert_eq!(total, 4);
        let by_name = |n: &str| rows.iter().find(|r| r.name == n).unwrap().clone();
        assert_eq!(by_name("Robo Scans").own, Some(AiLevel::Ai));
        let joint = by_name("Human Scans & Robo Scans");
        assert_eq!((joint.own, joint.effective), (None, Some(AiLevel::Ai)));
        assert_eq!(by_name("Gone Scans").chapters, 0);

        let filtered = |ai| {
            let q = ListQuery {
                ai: Some(ai),
                ..ListQuery::default()
            };
            let (rows, _) = page(&stats, &tagged, &q);
            rows.into_iter().map(|r| r.name).collect::<Vec<_>>()
        };
        assert_eq!(
            filtered(AiFilter::Ai),
            ["Robo Scans", "Human Scans & Robo Scans"]
        );
        assert_eq!(filtered(AiFilter::Partial), ["Gone Scans"]);
        assert_eq!(filtered(AiFilter::Untagged), ["Human Scans"]);
        assert_eq!(filtered(AiFilter::Tagged).len(), 3);

        // Source filter: that source's credits and counts only.
        let q = ListQuery {
            source: Some("77".into()),
            ..ListQuery::default()
        };
        let (rows, total) = page(&stats, &tagged, &q);
        assert_eq!((total, names(&rows)), (1, vec!["Human Scans"]));
        let q = ListQuery {
            source: Some(MANGADEX_SOURCE.into()),
            search: Some("ROBO".into()),
            ..ListQuery::default()
        };
        let (rows, _) = page(&stats, &tagged, &q);
        assert_eq!(names(&rows), ["Robo Scans", "Human Scans & Robo Scans"]);

        // Paging past the end is an empty page with the real total.
        let q = ListQuery {
            page: 9,
            ..ListQuery::default()
        };
        let (rows, total) = page(&stats, &tagged, &q);
        assert!(rows.is_empty());
        assert_eq!(total, 4);
    }
}
