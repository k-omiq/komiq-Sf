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
//! 4. [`public_groups`] / [`public_page`] / [`ratings`] — the readers' list: every
//!    scanlator (sites and groups) with the 1 to 10 scores readers gave it
//!    (`scanlator_rating`, PostgreSQL migration 0004).
//!
//! THE LIST IS COUNTED IN ONE SCAN AND MEMOIZED. There is no index on `chapter.scanlator`
//! and no materialized table, so the counts are a full scan of `chapter` joined to
//! `source_series` and, for the adult-content gate of the readers' list, to
//! `browse_catalogue` (about 2 s for 560k chapters before that third join, which adds
//! one lookup per chapter and a second distinct count). It runs at most once per
//! [`STATS_TTL`], for the admin console and the readers' list alike; search, the filters
//! and paging then work on the memoized rows in memory (some 13k credits).

use std::borrow::Cow;
use std::cmp::Ordering;
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
    /// Those of `chapters` on a work that is not adult: one Browse lists without the
    /// viewer's opt-in (`browse_catalogue.is_nsfw = FALSE`, the gate `browseSources`
    /// counts under). A work Browse does not list at all is not counted here.
    pub safe_chapters: i64,
    /// Those of `works` that are not adult.
    pub safe_works: i64,
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

    /// [`Self::counts`] on all sources, over the works that are not adult.
    fn safe_counts(&self) -> (i64, i64) {
        self.sources
            .iter()
            .fold((0, 0), |(c, w), s| (c + s.safe_chapters, w + s.safe_works))
    }
}

/// Count every credit in `chapter`, per source. The one slow query of this module; see
/// the module header. "No Group" and the backfill's empty string are not credits.
async fn count_groups(pool: &Database) -> Result<Vec<GroupStat>> {
    // Grouped by the folded name so two spellings on one series count the series once.
    // SQLite folds ASCII only; the loop below folds the rest. `MIN` picks the spelling
    // that sorts first, which is the capitalized one ("Robo Scans" before "robo scans").
    //
    // The two FILTER counts are the same numbers over the works that are not adult, for
    // the readers' list ([`PublicGroup::visible`]). `browse_catalogue` has one row per
    // work, so the LEFT JOIN adds no row and the first two counts are what they were.
    let rows: Vec<(String, String, String, i64, i64, i64, i64)> = crate::db_types::query_tuple(
        "SELECT MIN(c.scanlator), ss.source_type, COALESCE(ss.source_id, ''), \
                COUNT(*), COUNT(DISTINCT ss.work_id), \
                COUNT(*) FILTER (WHERE f.is_nsfw = FALSE), \
                COUNT(DISTINCT ss.work_id) FILTER (WHERE f.is_nsfw = FALSE) \
           FROM chapter c \
           JOIN source_series ss ON ss.id = c.source_series_id \
           LEFT JOIN browse_catalogue f ON f.work_id = ss.work_id \
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
        /// (chapters, works, safe chapters, safe works) per source.
        sources: HashMap<String, [i64; 4]>,
    }
    let mut by_key: HashMap<String, Acc> = HashMap::new();
    for (name, source_type, source_id, chapters, works, safe_chapters, safe_works) in rows {
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
        let slot = acc.sources.entry(source).or_insert([0; 4]);
        for (sum, n) in slot
            .iter_mut()
            .zip([chapters, works, safe_chapters, safe_works])
        {
            *sum += n;
        }
    }
    Ok(by_key
        .into_iter()
        .map(|(key, acc)| GroupStat {
            name: acc.name,
            key,
            sources: acc
                .sources
                .into_iter()
                .map(
                    |(source, [chapters, works, safe_chapters, safe_works])| SourceStat {
                        source,
                        chapters,
                        works,
                        safe_chapters,
                        safe_works,
                    },
                )
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

// ---- The readers' list: every scanlator with its rating ----

/// Rows per page of the readers' list.
pub const PUBLIC_PAGE_SIZE: usize = 30;

/// Scores a scanlator needs before it gets a rank.
pub const RANK_MIN_VOTES: i64 = 5;

/// How many scores at the server's mean a scanlator is given on top of its own before
/// it is ranked ([`weighted_score`]). It keeps five tens below two hundred nines.
pub const RANK_PRIOR_VOTES: i64 = 10;

const SITE_PREFIX: &str = "site:";
const GROUP_PREFIX: &str = "group:";

/// Which of the two things a scanlator is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Site,
    Group,
}

/// The id of a scanlator in the readers' list, in `scanlator_rating` and as a comment
/// target: `site:<source id>` or `group:<name key>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScanlatorId {
    pub kind: Kind,
    /// The source id of a site, the [`name_key`] of a group.
    pub key: String,
}

impl ScanlatorId {
    /// Read an id a client sent. A group may be written in any case ("group:Robo Scans");
    /// it is folded to its key. `None` when the text has neither shape. This says nothing
    /// about whether such a scanlator exists.
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(source) = text.strip_prefix(SITE_PREFIX) {
            return is_taggable_source(source).then(|| Self {
                kind: Kind::Site,
                key: source.to_string(),
            });
        }
        let key = name_key(text.strip_prefix(GROUP_PREFIX)?);
        (!key.is_empty()).then_some(Self {
            kind: Kind::Group,
            key,
        })
    }

    pub fn text(&self) -> String {
        id_text(self.kind, &self.key)
    }
}

/// [`ScanlatorId::text`] without building the id first.
pub fn id_text(kind: Kind, key: &str) -> String {
    match kind {
        Kind::Site => format!("{SITE_PREFIX}{key}"),
        Kind::Group => format!("{GROUP_PREFIX}{key}"),
    }
}

/// One group of the readers' list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicGroup<'a> {
    pub name: &'a str,
    pub key: &'a str,
    /// Its own chapters, plus those of the joint credits folded into it.
    pub chapters: i64,
    /// Its own series, plus those of the joint credits folded into it. A series it also
    /// released alone counts twice; like [`GroupStat::counts`] this is a size hint.
    pub works: i64,
    /// Those of `chapters` on a work that is not adult ([`SourceStat::safe_chapters`]).
    pub safe_chapters: i64,
    /// Those of `works` that are not adult.
    pub safe_works: i64,
}

impl PublicGroup<'_> {
    /// Whether a viewer is shown the group. Without the adult-content opt-in, only a
    /// group with at least one chapter on a work that is not adult: the same posture
    /// `browseSources` takes for a source. The group EXISTS either way (its scores count
    /// and it keeps its rank); this only decides who sees it.
    pub fn visible(&self, show_nsfw: bool) -> bool {
        show_nsfw || self.safe_chapters > 0
    }

    /// (chapters, series) as that viewer counts them: without the opt-in, only those on
    /// works that are not adult, as a source's `count` is.
    pub fn counts(&self, show_nsfw: bool) -> (i64, i64) {
        if show_nsfw {
            (self.chapters, self.works)
        } else {
            (self.safe_chapters, self.safe_works)
        }
    }
}

/// Whether the readers' list leaves a credit out as a joint release: it holds
/// [`JOINT_SEPARATOR`] and EVERY member is a credit on its own (`exists`, by name key).
/// When one member is not, the whole text is probably one group's real name.
fn is_joint_of_known_groups(key: &str, exists: impl Fn(&str) -> bool) -> bool {
    key.contains(JOINT_SEPARATOR)
        && key
            .split(JOINT_SEPARATOR)
            .all(|member| exists(member.trim()))
}

/// The groups of the readers' list. A joint credit ("A & B") whose members are all
/// credits on their own is not a scanlator of its own here: it is left out, and its
/// chapter and series counts are added to each member. The admin list ([`page`]) keeps
/// every credit as it is written, because a tag can name a joint credit.
pub fn public_groups(stats: &[GroupStat]) -> Vec<PublicGroup<'_>> {
    let index: HashMap<&str, usize> = stats
        .iter()
        .enumerate()
        .map(|(i, g)| (g.key.as_str(), i))
        .collect();
    let mut groups: Vec<Option<PublicGroup<'_>>> = stats
        .iter()
        .map(|g| {
            let (chapters, works) = g.counts(None);
            let (safe_chapters, safe_works) = g.safe_counts();
            Some(PublicGroup {
                name: &g.name,
                key: &g.key,
                chapters,
                works,
                safe_chapters,
                safe_works,
            })
        })
        .collect();
    for (i, joint) in stats.iter().enumerate() {
        if !is_joint_of_known_groups(&joint.key, |member| index.contains_key(member)) {
            continue;
        }
        let (chapters, works) = joint.counts(None);
        let (safe_chapters, safe_works) = joint.safe_counts();
        // "A & A" adds to A once.
        let mut members: Vec<usize> = joint
            .key
            .split(JOINT_SEPARATOR)
            .filter_map(|member| index.get(member.trim()).copied())
            .collect();
        members.sort_unstable();
        members.dedup();
        for member in members {
            if let Some(group) = groups[member].as_mut() {
                group.chapters += chapters;
                group.works += works;
                group.safe_chapters += safe_chapters;
                group.safe_works += safe_works;
            }
        }
        groups[i] = None;
    }
    groups.into_iter().flatten().collect()
}

/// The group of [`public_groups`] with this name key. `None` for a name no chapter
/// carries and for a joint credit that is counted with its members. The whole list is
/// built for it: a joint credit anywhere in it can add to this group's counts, and
/// those decide whether a viewer is shown the group ([`PublicGroup::visible`]).
pub fn public_group<'a>(stats: &'a [GroupStat], key: &str) -> Option<PublicGroup<'a>> {
    public_groups(stats).into_iter().find(|g| g.key == key)
}

/// The scores readers gave one scanlator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rating {
    /// How many gave score 1, 2, ... 10.
    pub distribution: [i64; 10],
    pub count: i64,
    pub sum: i64,
    /// 1-based place among the scanlators with at least [`RANK_MIN_VOTES`] scores.
    pub rank: Option<i64>,
}

impl Rating {
    /// The plain average; `None` without a score.
    pub fn average(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum as f64 / self.count as f64)
    }
}

/// What a scanlator is ranked by: its average, pulled toward `mean` (the mean of every
/// score on the server) by [`RANK_PRIOR_VOTES`] scores. Few scores stay near the mean;
/// many scores approach the scanlator's own average.
pub fn weighted_score(votes: i64, average: f64, mean: f64) -> f64 {
    let (votes, prior) = (votes as f64, RANK_PRIOR_VOTES as f64);
    (votes * average + prior * mean) / (votes + prior)
}

/// One value per scanlator, looked up by kind and key so that a list of 13k groups does
/// not build 13k id texts to find a handful of rated ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ById<T> {
    sites: HashMap<String, T>,
    groups: HashMap<String, T>,
}

impl<T> Default for ById<T> {
    fn default() -> Self {
        Self {
            sites: HashMap::new(),
            groups: HashMap::new(),
        }
    }
}

impl<T> ById<T> {
    pub fn get(&self, kind: Kind, key: &str) -> Option<&T> {
        match kind {
            Kind::Site => self.sites.get(key),
            Kind::Group => self.groups.get(key),
        }
    }

    fn insert(&mut self, id: ScanlatorId, value: T) {
        match id.kind {
            Kind::Site => self.sites.insert(id.key, value),
            Kind::Group => self.groups.insert(id.key, value),
        };
    }
}

/// Every rated scanlator's scores and rank.
pub type Ratings = ById<Rating>;

/// One reader's own scores.
pub type Scores = ById<i32>;

/// Fold `(scanlator id, score, how many gave it)` rows into one [`Rating`] per scanlator
/// and rank them.
///
/// Only a scanlator that `exists` now counts: the scores of one that left the list (a
/// source that was removed, a name no chapter carries any more, a joint credit that is
/// now counted with its members) stay stored, but they take no rank and do not move the
/// server's mean. `exists` must not depend on the viewer: a scanlator hidden from a
/// viewer who does not see adult content still exists and keeps its rank, so the ranks
/// one viewer is shown can skip a number.
///
/// The rank is global: it orders EVERY such scanlator with at least [`RANK_MIN_VOTES`]
/// scores by [`weighted_score`], whatever a request filters by. Equal scores order by the
/// number of scores, then by id, so two requests always agree.
pub fn fold_ratings(
    rows: impl IntoIterator<Item = (String, i64, i64)>,
    exists: impl Fn(&ScanlatorId) -> bool,
) -> Ratings {
    let mut by_id: HashMap<ScanlatorId, Rating> = HashMap::new();
    for (id, score, votes) in rows {
        let Some(id) = ScanlatorId::parse(&id) else {
            continue;
        };
        if !(1..=10).contains(&score) || votes <= 0 || !exists(&id) {
            continue;
        }
        let rating = by_id.entry(id).or_default();
        rating.distribution[(score - 1) as usize] += votes;
        rating.count += votes;
        rating.sum += score * votes;
    }
    let (sum, count) = by_id
        .values()
        .fold((0, 0), |(sum, count), r| (sum + r.sum, count + r.count));
    if count > 0 {
        let mean = sum as f64 / count as f64;
        let mut ranked: Vec<(f64, i64, ScanlatorId)> = by_id
            .iter()
            .filter(|(_, r)| r.count >= RANK_MIN_VOTES)
            .map(|(id, r)| {
                let average = r.sum as f64 / r.count as f64;
                (weighted_score(r.count, average, mean), r.count, id.clone())
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.kind.cmp(&b.2.kind))
                .then_with(|| a.2.key.cmp(&b.2.key))
        });
        for (place, (_, _, id)) in ranked.into_iter().enumerate() {
            if let Some(rating) = by_id.get_mut(&id) {
                rating.rank = Some(place as i64 + 1);
            }
        }
    }
    let mut ratings = Ratings::default();
    for (id, rating) in by_id {
        ratings.insert(id, rating);
    }
    ratings
}

/// The scores of every scanlator that `exists` ([`fold_ratings`]), in one grouped read.
/// The table is small (one row per reader and scanlator they rated), so it is read on
/// every request, like the tags.
///
/// A banned reader's score still counts, as their series rating does (`reviews`): a ban
/// hides what an account wrote, not the numbers it left.
pub async fn ratings(pool: &Database, exists: impl Fn(&ScanlatorId) -> bool) -> Result<Ratings> {
    let rows: Vec<(String, i64, i64)> = crate::db_types::query_tuple(
        "SELECT scanlator_id, score, COUNT(*) FROM scanlator_rating GROUP BY scanlator_id, score",
    )
    .fetch_all(pool)
    .await?;
    Ok(fold_ratings(rows, exists))
}

/// The scores one reader gave.
pub async fn user_scores(pool: &Database, user_id: uuid::Uuid) -> Result<Scores> {
    let rows: Vec<(String, i64)> = crate::db_types::query_tuple(
        "SELECT scanlator_id, score FROM scanlator_rating WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut scores = Scores::default();
    for (id, score) in rows {
        if let Some(id) = ScanlatorId::parse(&id) {
            scores.insert(id, score as i32);
        }
    }
    Ok(scores)
}

/// One row of the readers' list, before paging. It borrows from the memoized counts and
/// from the ratings read for the request, so building 13k of them copies no text.
#[derive(Clone, Debug, PartialEq)]
pub struct PublicRow<'a> {
    pub kind: Kind,
    /// The source id of a site, the name key of a group.
    pub key: &'a str,
    pub name: &'a str,
    /// The name as search and the name order read it ([`name_key`]).
    pub folded_name: Cow<'a, str>,
    /// Sites only.
    pub lang: Option<&'a str>,
    pub works: i64,
    /// `None` for a site: nothing counts a source's chapters without a scan.
    pub chapters: Option<i64>,
    pub rating: Option<&'a Rating>,
    pub my_score: Option<i32>,
}

impl<'a> PublicRow<'a> {
    pub fn site(
        id: &'a str,
        name: &'a str,
        lang: Option<&'a str>,
        works: i64,
        ratings: &'a Ratings,
        mine: &Scores,
    ) -> Self {
        Self {
            kind: Kind::Site,
            key: id,
            name,
            folded_name: Cow::Owned(name_key(name)),
            lang,
            works,
            chapters: None,
            rating: ratings.get(Kind::Site, id),
            my_score: mine.get(Kind::Site, id).copied(),
        }
    }

    /// A group as a viewer with this adult-content posture counts it. The caller leaves
    /// out a group that viewer is not shown ([`PublicGroup::visible`]).
    pub fn group(
        group: &PublicGroup<'a>,
        show_nsfw: bool,
        ratings: &'a Ratings,
        mine: &Scores,
    ) -> Self {
        let (chapters, works) = group.counts(show_nsfw);
        Self {
            kind: Kind::Group,
            key: group.key,
            name: group.name,
            folded_name: Cow::Borrowed(group.key),
            lang: None,
            works,
            chapters: Some(chapters),
            rating: ratings.get(Kind::Group, group.key),
            my_score: mine.get(Kind::Group, group.key).copied(),
        }
    }

    pub fn id(&self) -> String {
        id_text(self.kind, self.key)
    }

    fn votes(&self) -> i64 {
        self.rating.map_or(0, |r| r.count)
    }

    fn rank(&self) -> Option<i64> {
        self.rating.and_then(|r| r.rank)
    }
}

/// The order of the readers' list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PublicSort {
    /// Ranked scanlators by rank, then the rest by number of scores.
    #[default]
    Top,
    /// Most scores first.
    Votes,
    /// Most chapters first.
    Chapters,
    /// By name.
    Name,
}

/// One request for the readers' list.
#[derive(Clone, Debug, Default)]
pub struct PublicQuery {
    /// Case-insensitive part of the name.
    pub search: Option<String>,
    pub kind: Option<Kind>,
    /// Only the scanlators the viewer rated.
    pub mine: bool,
    pub sort: PublicSort,
    /// 1-based.
    pub page: usize,
}

/// Biggest first: chapters (a site has none counted, so it reads as 0), then series.
fn by_size(a: &PublicRow<'_>, b: &PublicRow<'_>) -> Ordering {
    b.chapters
        .unwrap_or(0)
        .cmp(&a.chapters.unwrap_or(0))
        .then_with(|| b.works.cmp(&a.works))
}

/// By name; kind and key make it a total order when a site and a group share a name.
fn by_name(a: &PublicRow<'_>, b: &PublicRow<'_>) -> Ordering {
    a.folded_name
        .cmp(&b.folded_name)
        .then_with(|| a.kind.cmp(&b.kind))
        .then_with(|| a.key.cmp(b.key))
}

/// Filter, order and page the readers' list. Returns the page's rows and the filtered
/// total. Every order ends in size, then name, so equal rows keep one place across pages.
pub fn public_page<'a>(
    mut rows: Vec<PublicRow<'a>>,
    q: &PublicQuery,
) -> (Vec<PublicRow<'a>>, usize) {
    let search = q.search.as_deref().map(name_key).filter(|s| !s.is_empty());
    rows.retain(|r| {
        q.kind.is_none_or(|kind| r.kind == kind)
            && (!q.mine || r.my_score.is_some())
            && search.as_deref().is_none_or(|s| r.folded_name.contains(s))
    });
    rows.sort_by(|a, b| {
        let first = match q.sort {
            PublicSort::Top => match (a.rank(), b.rank()) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => b.votes().cmp(&a.votes()),
            },
            PublicSort::Votes => b.votes().cmp(&a.votes()),
            PublicSort::Chapters => Ordering::Equal,
            PublicSort::Name => by_name(a, b),
        };
        first
            .then_with(|| by_size(a, b))
            .then_with(|| by_name(a, b))
    });
    let total = rows.len();
    let start = q
        .page
        .max(1)
        .saturating_sub(1)
        .saturating_mul(PUBLIC_PAGE_SIZE);
    let rows = rows
        .into_iter()
        .skip(start)
        .take(PUBLIC_PAGE_SIZE)
        .collect();
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
    async fn the_scan_counts_the_chapters_on_works_that_are_not_adult() {
        let pool = pool().await;
        seed(&pool).await;
        // (chapters, series, safe chapters, safe series) of a credit on MangaDex.
        let robo = |stats: &[GroupStat]| {
            let g = stats.iter().find(|g| g.key == "robo scans").unwrap();
            let s = g
                .sources
                .iter()
                .find(|s| s.source == MANGADEX_SOURCE)
                .unwrap();
            (s.chapters, s.works, s.safe_chapters, s.safe_works)
        };
        // Browse lists neither work yet: nothing is known to be safe, and the counts
        // the admin list shows are not touched by that.
        assert_eq!(robo(&count_groups(&pool).await.unwrap()), (4, 2, 0, 0));

        // `w1` is listed as safe, `w2` as adult.
        sqlx::query(
            "INSERT INTO browse_catalogue (work_id, reader_id, title, is_nsfw, created_at) VALUES \
               ('w1', 'w1', 'One', FALSE, '2020-01-01T00:00:00Z'), \
               ('w2', 'w2', 'Two', TRUE, '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let stats = count_groups(&pool).await.unwrap();
        assert_eq!(robo(&stats), (4, 2, 3, 1));
        let groups = public_groups(&stats);
        let human = groups.iter().find(|g| g.key == "human scans").unwrap();
        // Its own chapter on source 77 and the joint one on MangaDex, both on `w1`.
        assert_eq!((human.counts(true), human.counts(false)), ((2, 2), (2, 2)));
        let robo = groups.iter().find(|g| g.key == "robo scans").unwrap();
        assert_eq!((robo.counts(true), robo.counts(false)), ((5, 3), (4, 2)));
        assert!(groups.iter().all(|g| g.visible(false)));
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

    /// One credit on MangaDex with these counts.
    fn stat(name: &str, chapters: i64, works: i64) -> GroupStat {
        GroupStat {
            name: name.to_string(),
            key: name_key(name),
            sources: vec![SourceStat {
                source: MANGADEX_SOURCE.to_string(),
                chapters,
                works,
                safe_chapters: chapters,
                safe_works: works,
            }],
        }
    }

    /// [`stat`] with only `safe` of its chapters and series on works that are not adult.
    fn stat_with_safe(name: &str, all: (i64, i64), safe: (i64, i64)) -> GroupStat {
        let mut stat = stat(name, all.0, all.1);
        stat.sources[0].safe_chapters = safe.0;
        stat.sources[0].safe_works = safe.1;
        stat
    }

    #[test]
    fn a_scanlator_id_is_a_site_or_a_group_and_nothing_else() {
        let site = ScanlatorId::parse("site:6247824327199706550").unwrap();
        assert_eq!(
            (site.kind, site.key.as_str()),
            (Kind::Site, "6247824327199706550")
        );
        assert_eq!(site.text(), "site:6247824327199706550");
        // A group is folded to its key, so one group has one id.
        let group = ScanlatorId::parse("group:  Robo Scans ").unwrap();
        assert_eq!(
            (group.kind, group.key.as_str()),
            (Kind::Group, "robo scans")
        );
        assert_eq!(group.text(), "group:robo scans");
        assert_eq!(ScanlatorId::parse(&group.text()), Some(group));
        // MangaDex is not a site, and free text is not a source id.
        for bad in [
            "site:mangadex",
            "site:",
            "site:a b",
            "group:",
            "group:   ",
            "robo scans",
            "",
        ] {
            assert_eq!(ScanlatorId::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_joint_credit_folds_into_its_members_when_every_member_is_a_credit() {
        let stats = [
            stat("Robo Scans", 10, 2),
            stat("Human Scans", 5, 1),
            stat("Third Scans", 1, 1),
            // Both members are credits: folded into each of them.
            stat("Human Scans & Robo Scans", 3, 2),
            // Three members, any spacing and case, one of them twice.
            stat("robo scans &  THIRD SCANS & Robo Scans", 4, 1),
            // "Strawberry" is not a credit: this is one group's own name.
            stat("Robo Scans & Strawberry", 7, 3),
            // No separator (no spaces around the "&"): an ordinary name.
            stat("Salt&Pepper", 2, 1),
        ];
        let groups = public_groups(&stats);
        let row = |name: &str| {
            let g = groups
                .iter()
                .find(|g| g.name == name)
                .unwrap_or_else(|| panic!("no group {name}"));
            (g.chapters, g.works)
        };
        assert_eq!(groups.len(), 5);
        assert_eq!(row("Robo Scans"), (10 + 3 + 4, 2 + 2 + 1));
        assert_eq!(row("Human Scans"), (5 + 3, 1 + 2));
        assert_eq!(row("Third Scans"), (1 + 4, 1 + 1));
        assert_eq!(row("Robo Scans & Strawberry"), (7, 3));
        assert_eq!(row("Salt&Pepper"), (2, 1));

        // The same answer for one key.
        for (key, listed) in [
            ("robo scans", true),
            ("robo scans & strawberry", true),
            ("salt&pepper", true),
            ("human scans & robo scans", false),
            ("robo scans &  third scans & robo scans", false),
            // Never a credit at all.
            ("gone scans", false),
            ("gone scans & robo scans", false),
        ] {
            assert_eq!(public_group(&stats, key).is_some(), listed, "{key}");
        }
        assert_eq!(public_group(&stats, "human scans").unwrap().chapters, 5 + 3);
        // A list without joint credits is unchanged.
        assert_eq!(public_groups(&stats[..3]).len(), 3);
        assert!(public_groups(&[]).is_empty());
    }

    #[test]
    fn a_group_with_only_adult_chapters_is_hidden_without_the_opt_in() {
        let stats = [
            // Half of it is on adult works.
            stat_with_safe("Mixed Scans", (10, 4), (6, 2)),
            // All of it is.
            stat_with_safe("Lewd Scans", (8, 3), (0, 0)),
            // Adult on its own, but one joint release is not: that release is its too.
            stat_with_safe("Joint Scans", (5, 2), (0, 0)),
            stat_with_safe("Joint Scans & Mixed Scans", (3, 2), (1, 1)),
        ];
        let groups = public_groups(&stats);
        let group = |name: &str| groups.iter().find(|g| g.name == name).unwrap();
        assert_eq!(groups.len(), 3);

        let mixed = group("Mixed Scans");
        assert!(mixed.visible(false) && mixed.visible(true));
        // Each viewer's counts are the works they may see, the joint release included.
        assert_eq!(mixed.counts(true), (10 + 3, 4 + 2));
        assert_eq!(mixed.counts(false), (6 + 1, 2 + 1));

        let lewd = group("Lewd Scans");
        assert!(!lewd.visible(false) && lewd.visible(true));
        assert_eq!(lewd.counts(true), (8, 3));

        let joint = group("Joint Scans");
        assert!(joint.visible(false));
        assert_eq!(joint.counts(false), (1, 1));
        assert_eq!(joint.counts(true), (5 + 3, 2 + 2));

        // A row carries the viewer's counts.
        let (ratings, mine) = (Ratings::default(), Scores::default());
        let row = PublicRow::group(mixed, false, &ratings, &mine);
        assert_eq!((row.chapters, row.works), (Some(7), 3));
        let row = PublicRow::group(mixed, true, &ratings, &mine);
        assert_eq!((row.chapters, row.works), (Some(13), 6));
        // Hidden or not, the group exists: it is found by key either way.
        assert!(!public_group(&stats, "lewd scans").unwrap().visible(false));
    }

    /// `votes` scores of `score` for one scanlator, as the grouped read returns them.
    fn scores(id: &str, score: i64, votes: i64) -> (String, i64, i64) {
        (id.to_string(), score, votes)
    }

    #[test]
    fn the_weighted_score_pulls_few_scores_toward_the_server_mean() {
        // (v * avg + 10 * C) / (v + 10)
        assert_eq!(weighted_score(5, 10.0, 6.0), (50.0 + 60.0) / 15.0);
        assert_eq!(weighted_score(190, 9.0, 6.0), (1710.0 + 60.0) / 200.0);
        // No scores: the mean itself. Many scores: close to the scanlator's average.
        assert_eq!(weighted_score(0, 0.0, 6.5), 6.5);
        assert!((weighted_score(100_000, 9.0, 1.0) - 9.0).abs() < 0.001);
        // Five tens do not outrank two hundred nines.
        assert!(weighted_score(5, 10.0, 6.0) < weighted_score(200, 9.0, 6.0));
        assert_eq!((RANK_MIN_VOTES, RANK_PRIOR_VOTES), (5, 10));
    }

    #[test]
    fn only_a_scanlator_with_five_scores_is_ranked_and_the_rank_is_by_weighted_score() {
        let ratings = fold_ratings(
            [
                // 20 scores, average 9.
                scores("group:many", 9, 20),
                // 5 scores, average 10: a higher average, a lower weighted score.
                scores("site:77", 10, 5),
                // 5 scores over two values, average 3.
                scores("group:low", 2, 3),
                scores("group:low", 5, 1),
                scores("group:low", 4, 1),
                // 4 scores: not ranked, but they count toward the mean.
                scores("group:few", 1, 4),
                // Not ids and not scores: ignored.
                scores("mangadex", 10, 50),
                scores("group:many", 11, 50),
            ],
            |_| true,
        );
        let many = ratings.get(Kind::Group, "many").unwrap();
        let site = ratings.get(Kind::Site, "77").unwrap();
        let low = ratings.get(Kind::Group, "low").unwrap();
        let few = ratings.get(Kind::Group, "few").unwrap();
        assert_eq!((many.count, many.average()), (20, Some(9.0)));
        assert_eq!((site.count, site.average()), (5, Some(10.0)));
        assert_eq!((low.count, low.average()), (5, Some(3.0)));
        assert_eq!(low.distribution, [0, 3, 0, 1, 1, 0, 0, 0, 0, 0]);
        assert_eq!(site.distribution[9], 5);
        assert_eq!(Rating::default().average(), None);

        // C is the mean of all 34 scores: (180 + 50 + 15 + 4) / 34.
        let mean = 249.0 / 34.0;
        let (w_many, w_site, w_low) = (
            weighted_score(20, 9.0, mean),
            weighted_score(5, 10.0, mean),
            weighted_score(5, 3.0, mean),
        );
        assert!(w_many > w_site && w_site > w_low);
        assert_eq!(
            (many.rank, site.rank, low.rank),
            (Some(1), Some(2), Some(3))
        );
        assert_eq!(few.rank, None);
        assert_eq!(ratings.get(Kind::Site, "many"), None);

        // Equal weighted scores: more scores first is moot here, so the id decides.
        let tied = fold_ratings([scores("group:b", 7, 5), scores("group:a", 7, 5)], |_| true);
        assert_eq!(tied.get(Kind::Group, "a").unwrap().rank, Some(1));
        assert_eq!(tied.get(Kind::Group, "b").unwrap().rank, Some(2));
        // One short of the minimum: no rank at all.
        let short = fold_ratings([scores("group:a", 7, RANK_MIN_VOTES - 1)], |_| true);
        assert_eq!(short.get(Kind::Group, "a").unwrap().rank, None);
    }

    #[test]
    fn the_scores_of_a_scanlator_that_no_longer_exists_count_nowhere() {
        let rows = || {
            [
                scores("group:here", 6, 5),
                scores("site:77", 4, 5),
                // Left the list: a folded joint credit, a removed source.
                scores("group:gone & here", 10, 50),
                scores("site:99", 10, 50),
            ]
        };
        let exists = |id: &ScanlatorId| matches!(id.key.as_str(), "here" | "77");
        let ratings = fold_ratings(rows(), exists);
        // No rating, so no rank slot.
        assert_eq!(ratings.get(Kind::Group, "gone & here"), None);
        assert_eq!(ratings.get(Kind::Site, "99"), None);
        let (here, site) = (
            ratings.get(Kind::Group, "here").unwrap(),
            ratings.get(Kind::Site, "77").unwrap(),
        );
        assert_eq!((here.rank, site.rank), (Some(1), Some(2)));
        // And they do not move the mean: the result is the one of a table without them.
        assert_eq!(ratings, fold_ratings(rows().into_iter().take(2), |_| true));
        // Counted, the hundred tens would have taken the first two places.
        let all = fold_ratings(rows(), |_| true);
        assert_eq!(all.get(Kind::Group, "here").unwrap().rank, Some(3));
    }

    #[test]
    fn the_readers_list_filters_orders_and_pages() {
        let stats = [
            stat("Big Scans", 900, 40),
            stat("Rated Scans", 10, 2),
            stat("Alpha Scans", 10, 2),
            stat("Fresh Scans", 1, 1),
        ];
        let groups = public_groups(&stats);
        let ratings = fold_ratings(
            [
                scores("group:rated scans", 8, 6),
                scores("site:77", 9, 6),
                scores("group:fresh scans", 10, 2),
                scores("group:alpha scans", 1, 1),
            ],
            |_| true,
        );
        let mut mine = Scores::default();
        mine.insert(ScanlatorId::parse("group:fresh scans").unwrap(), 10);
        mine.insert(ScanlatorId::parse("site:77").unwrap(), 9);
        let rows = || {
            let mut rows = vec![
                PublicRow::site("77", "Zed Site", Some("en"), 300, &ratings, &mine),
                PublicRow::site("88", "alpha site", None, 5, &ratings, &mine),
            ];
            rows.extend(
                groups
                    .iter()
                    .map(|g| PublicRow::group(g, true, &ratings, &mine)),
            );
            rows
        };
        let ids = |q: PublicQuery| {
            let (rows, total) = public_page(rows(), &q);
            (rows.iter().map(PublicRow::id).collect::<Vec<_>>(), total)
        };
        let sorted = |sort| {
            ids(PublicQuery {
                sort,
                ..PublicQuery::default()
            })
            .0
        };

        // TOP: the ranked by rank, then by number of scores, then by size, then by name.
        assert_eq!(
            sorted(PublicSort::Top),
            [
                "site:77",
                "group:rated scans",
                "group:fresh scans",
                "group:alpha scans",
                "group:big scans",
                "site:88",
            ]
        );
        assert_eq!(
            sorted(PublicSort::Votes),
            [
                "group:rated scans",
                "site:77",
                "group:fresh scans",
                "group:alpha scans",
                "group:big scans",
                "site:88",
            ]
        );
        // A site has no chapter count: it reads as 0 and orders by its series.
        assert_eq!(
            sorted(PublicSort::Chapters),
            [
                "group:big scans",
                "group:alpha scans",
                "group:rated scans",
                "group:fresh scans",
                "site:77",
                "site:88",
            ]
        );
        assert_eq!(
            sorted(PublicSort::Name),
            [
                "group:alpha scans",
                "site:88",
                "group:big scans",
                "group:fresh scans",
                "group:rated scans",
                "site:77",
            ]
        );

        // Filters: kind, part of the name in any case, and the viewer's own.
        let q = PublicQuery {
            kind: Some(Kind::Site),
            ..PublicQuery::default()
        };
        assert_eq!(
            ids(q),
            (vec!["site:77".to_string(), "site:88".to_string()], 2)
        );
        let q = PublicQuery {
            search: Some(" ALPHA ".into()),
            ..PublicQuery::default()
        };
        assert_eq!(ids(q).0, ["group:alpha scans", "site:88"]);
        let q = PublicQuery {
            mine: true,
            ..PublicQuery::default()
        };
        assert_eq!(ids(q).0, ["site:77", "group:fresh scans"]);
        // The rank does not move with a filter: the second of the server stays second.
        let q = PublicQuery {
            kind: Some(Kind::Group),
            ..PublicQuery::default()
        };
        let (rows, _) = public_page(rows(), &q);
        assert_eq!(rows[0].rating.unwrap().rank, Some(2));

        // Paging past the end is an empty page with the real total.
        let q = PublicQuery {
            page: 2,
            ..PublicQuery::default()
        };
        assert_eq!(ids(q), (vec![], 6));
    }
}
