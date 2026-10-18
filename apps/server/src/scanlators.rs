//! Scanlators and the admin's AI tags on them (migrations 0116 and 0117).
//!
//! A scanlator is one of two things:
//!
//! * a GROUP: the free-text `chapter.scanlator` credit of one chapter copy. MangaDex
//!   credits every chapter; most other sources credit none.
//! * a SITE: a source that translates its own series. Its chapters carry no credit, so
//!   the source itself is the scanlator ([`list_source_tags`] / [`set_source_tag`]).
//!
//! A chapter copy takes the stronger of the tag on its credit and the tag on its source.
//!
//! For groups this module owns the things built on the credit text:
//!
//! 1. [`name_key`] / [`effective_level`] — the identity of a credit and the tag that
//!    applies to it. A joint credit ("A & B") with no tag of its own takes the strongest
//!    tag of its members.
//! 2. [`list_tags`] / [`set_tag`] — the `scanlator_tag` table.
//! 3. [`admin_groups`] / [`page`] — the admin's scanlator list: every credit with the
//!    number of series it is credited on.
//! 4. [`public_page`] / [`listed_groups`] / [`ratings`] — the readers' list: every
//!    scanlator (sites and groups) with the 1 to 10 scores readers gave it
//!    (`scanlator_rating`, PostgreSQL migration 0004).
//!
//! THE GROUPS ARE AN INDEX, NOT A SCAN. `scanlator_group` (one row per credit) and
//! `scanlator_series` (the series each credit is on) are kept current by a trigger on
//! `chapter` (PostgreSQL migration 0018). Nothing here reads the chapter table. The
//! readers' list is filtered, ordered and paged by one query over those two tables and
//! counts no chapters and no series.

use std::collections::HashMap;
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

/// One credit of the admin list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminGroup {
    /// The spelling the index keeps for the credit.
    pub name: String,
    pub key: String,
    /// Series the credit is on, joint releases included.
    pub series: i64,
}

/// Every credit with a series on `source` (or on any source), whose key contains
/// `search` (already folded with [`name_key`]), with its series count there. One grouped
/// read of the index; admin only.
pub async fn admin_groups(
    pool: &Database,
    source: Option<&str>,
    search: Option<&str>,
) -> Result<Vec<AdminGroup>> {
    let rows: Vec<(String, String, i64)> = crate::db_types::query_tuple(
        "SELECT g.name, g.key, COUNT(*) \
           FROM scanlator_group g \
           JOIN scanlator_series l ON l.key = g.key \
           JOIN source_series s ON s.id = l.source_series_id \
          WHERE ($1::text IS NULL OR CASE WHEN s.source_type = $3 THEN $3 \
                                          ELSE COALESCE(s.source_id, '') END = $1) \
            AND ($2::text IS NULL OR strpos(g.key, $2) > 0) \
          GROUP BY g.key, g.name",
    )
    .bind(source)
    .bind(search)
    .bind(MANGADEX_SOURCE)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, key, series)| AdminGroup { name, key, series })
        .collect())
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
    /// Only credits with series on this source ([`MANGADEX_SOURCE`] or a source id).
    /// The counts are then that source's.
    pub source: Option<String>,
    /// 1-based.
    pub page: usize,
}

impl ListQuery {
    /// The search as the index stores keys; `None` when it is blank.
    pub fn folded_search(&self) -> Option<String> {
        self.search
            .as_deref()
            .map(name_key)
            .filter(|s| !s.is_empty())
    }
}

/// One row of the admin list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRow {
    pub name: String,
    pub series: i64,
    /// The tag stored for exactly this name.
    pub own: Option<AiLevel>,
    /// The tag that applies ([`effective_level`]).
    pub effective: Option<AiLevel>,
}

/// Tag, filter, order and page the credits [`admin_groups`] read for `q`. Returns the
/// page's rows and the filtered total.
///
/// Order: most series first, then by name. A tagged name that no series carries any more
/// is still listed (with zero series), so the admin can find and remove the tag; it is
/// left out under a source filter, where it has nothing to show.
pub fn page(groups: &[AdminGroup], tags: &[Tag], q: &ListQuery) -> (Vec<ListRow>, usize) {
    let index = tag_index(tags);
    let search = q.folded_search();

    let mut rows: Vec<ListRow> = groups
        .iter()
        .map(|g| ListRow {
            name: g.name.clone(),
            series: g.series,
            own: index.get(&g.key).copied(),
            effective: effective_level(&g.name, &index),
        })
        .collect();
    if q.source.is_none() {
        let listed: std::collections::HashSet<&str> =
            groups.iter().map(|g| g.key.as_str()).collect();
        rows.extend(tags.iter().filter_map(|t| {
            let key = name_key(&t.name);
            let matches = search.as_deref().is_none_or(|s| key.contains(s));
            (!listed.contains(key.as_str()) && matches).then(|| ListRow {
                name: t.name.clone(),
                series: 0,
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
        b.series
            .cmp(&a.series)
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

    /// Every entry, by id text.
    pub fn iter(&self) -> impl Iterator<Item = (String, &T)> {
        let sites = self.sites.iter().map(|(k, v)| (id_text(Kind::Site, k), v));
        let groups = self
            .groups
            .iter()
            .map(|(k, v)| (id_text(Kind::Group, k), v));
        sites.chain(groups)
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

/// How long the flags `scanlator_group_refresh()` sets are reused. A group the trigger
/// adds is shown to viewers with the adult-content opt-in at once; the flags only decide
/// when a viewer without it sees a new group, when a joint credit folds into its members
/// and when a group whose series are all gone leaves. Ten minutes old is still right.
///
/// Zero under test, like `browse::COUNT_TTL`: the memo is process-wide and every test has
/// its own database.
#[cfg(not(test))]
const FLAGS_TTL: Duration = Duration::from_secs(600);
#[cfg(test)]
const FLAGS_TTL: Duration = Duration::ZERO;

static FLAGS_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
/// Single-flight: while one request refreshes, the others read the flags as they are.
static FLAGS_REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn flags_fresh() -> bool {
    FLAGS_AT
        .lock()
        .ok()
        .and_then(|at| *at)
        .is_some_and(|at| at.elapsed() < FLAGS_TTL)
}

/// Run `scanlator_group_refresh()` when the flags are older than [`FLAGS_TTL`]. It reads
/// the index, never `chapter`, and writes only the rows that change. A failure is logged
/// and the list is served from the flags as they are.
async fn refresh_flags(pool: &Database) {
    if flags_fresh() {
        return;
    }
    let Ok(_guard) = FLAGS_REFRESH.try_lock() else {
        return;
    };
    if flags_fresh() {
        return;
    }
    // `execute` takes the write lane: the function updates rows.
    match sqlx::query("SELECT scanlator_group_refresh()")
        .execute(pool)
        .await
    {
        Ok(_) => {
            if let Ok(mut at) = FLAGS_AT.lock() {
                *at = Some(Instant::now());
            }
        }
        Err(e) => tracing::warn!(error = %e, "scanlator flags refresh failed"),
    }
}

/// Whether a viewer is shown a group (`g`), with `$show` their adult-content opt-in. A
/// joint credit whose members are all groups of their own is never shown: the members
/// are. Then a series credited to the group (its joint releases included), and without
/// the opt-in one on a work Browse lists as not adult (`browse_catalogue.is_nsfw =
/// FALSE`), the posture `browseSources` takes for a source. A group hidden from one
/// viewer still exists: its scores count and it keeps its rank.
fn shown(show: &str) -> String {
    format!("g.listed AND CASE WHEN {show} THEN g.any_series ELSE g.safe_series END")
}

/// The groups among `keys` that the readers' list shows a viewer with this opt-in, as
/// (key, name), by key. `show_nsfw: true` is every group that EXISTS, whoever asks.
pub async fn listed_groups(
    pool: &Database,
    keys: &[String],
    show_nsfw: bool,
) -> Result<Vec<(String, String)>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    refresh_flags(pool).await;
    let sql = format!(
        "SELECT g.key, g.name FROM scanlator_group g \
          WHERE g.key = ANY($1) AND {} \
          ORDER BY g.key",
        shown("$2")
    );
    Ok(crate::db_types::query_tuple(&sql)
        .bind(keys)
        .bind(show_nsfw)
        .fetch_all(pool)
        .await?)
}

/// The scores of every scanlator that exists ([`fold_ratings`]): a site when `site`
/// says so, a group when [`listed_groups`] has it. One grouped read of the scores, which
/// are few (one row per reader and scanlator they rated), and one lookup of the rated
/// groups.
///
/// A banned reader's score still counts, as their series rating does (`reviews`): a ban
/// hides what an account wrote, not the numbers it left.
pub async fn ratings(pool: &Database, site: impl Fn(&str) -> bool) -> Result<Ratings> {
    let rows: Vec<(String, i64, i64)> = crate::db_types::query_tuple(
        "SELECT scanlator_id, score, COUNT(*) FROM scanlator_rating GROUP BY scanlator_id, score",
    )
    .fetch_all(pool)
    .await?;
    let mut rated: Vec<String> = rows
        .iter()
        .filter_map(|(id, _, _)| ScanlatorId::parse(id))
        .filter(|id| id.kind == Kind::Group)
        .map(|id| id.key)
        .collect();
    rated.sort_unstable();
    rated.dedup();
    let groups: std::collections::HashSet<String> = listed_groups(pool, &rated, true)
        .await?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    Ok(fold_ratings(rows, |id| match id.kind {
        Kind::Site => site(&id.key),
        Kind::Group => groups.contains(&id.key),
    }))
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

/// The order of the readers' list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PublicSort {
    /// Ranked scanlators by rank, then the rest by number of scores.
    #[default]
    Top,
    /// Most scores first.
    Votes,
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

/// A site of the readers' list: a source `browseSources` lists, by id and display name.
#[derive(Clone, Copy, Debug)]
pub struct Site<'a> {
    pub id: &'a str,
    pub name: &'a str,
}

/// One row of a page of the readers' list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicRow {
    pub kind: Kind,
    /// The source id of a site, the name key of a group.
    pub key: String,
    pub name: String,
}

impl PublicRow {
    pub fn id(&self) -> String {
        id_text(self.kind, &self.key)
    }
}

/// One page of the readers' list, and the filtered total: `sites` and the groups shown
/// a viewer with this opt-in, filtered by `q`, ordered and paged in one query.
///
/// TOP puts the ranked first by rank, then the rest by number of scores; VOTES orders by
/// number of scores; every order ends in the name (then kind and key), so equal rows
/// keep one place across pages. The ranks come from `ratings`, which is global, so a
/// filter never moves them.
pub async fn public_page(
    pool: &Database,
    sites: &[Site<'_>],
    ratings: &Ratings,
    mine: &Scores,
    show_nsfw: bool,
    q: &PublicQuery,
) -> Result<(Vec<PublicRow>, usize)> {
    refresh_flags(pool).await;
    let site_ids: Vec<&str> = sites.iter().map(|s| s.id).collect();
    let site_names: Vec<&str> = sites.iter().map(|s| s.name).collect();
    let site_folded: Vec<String> = sites.iter().map(|s| name_key(s.name)).collect();
    let (mut rated_ids, mut ranks, mut votes) = (Vec::new(), Vec::new(), Vec::new());
    for (id, rating) in ratings.iter() {
        rated_ids.push(id);
        ranks.push(rating.rank);
        votes.push(rating.count);
    }
    let mine: Option<Vec<String>> = q.mine.then(|| mine.iter().map(|(id, _)| id).collect());
    let kind: Option<i64> = q.kind.map(|kind| match kind {
        Kind::Site => 0,
        Kind::Group => 1,
    });
    let search = q.search.as_deref().map(name_key).filter(|s| !s.is_empty());
    let sort = match q.sort {
        PublicSort::Top => "top",
        PublicSort::Votes => "votes",
        PublicSort::Name => "name",
    };
    let offset = q
        .page
        .max(1)
        .saturating_sub(1)
        .saturating_mul(PUBLIC_PAGE_SIZE) as i64;

    // `hits` is used twice, so it is computed once: its count is the total, a page past
    // the end still has it, and its row number is the order the page is cut from.
    let sql = format!(
        "WITH board AS ( \
             SELECT 0::int8 AS kind, s.key, s.name, s.folded \
               FROM unnest($1::text[], $2::text[], $3::text[]) AS s(key, name, folded) \
             UNION ALL \
             SELECT 1::int8, g.key, g.name, g.key \
               FROM scanlator_group g \
              WHERE {shown} \
         ), rated AS ( \
             SELECT * FROM unnest($5::text[], $6::int8[], $7::int8[]) AS r(id, rank, votes) \
         ), hits AS ( \
             SELECT b.kind, b.key, b.name, \
                    row_number() OVER (ORDER BY \
                        CASE WHEN $11 = 'top' THEN r.rank END ASC NULLS LAST, \
                        CASE WHEN $11 <> 'name' THEN COALESCE(r.votes, 0) END DESC, \
                        b.folded, b.kind, b.key) AS ord \
               FROM board b \
               LEFT JOIN rated r \
                 ON r.id = CASE b.kind WHEN 0 THEN 'site:' ELSE 'group:' END || b.key \
              WHERE ($8::int8 IS NULL OR b.kind = $8) \
                AND ($9::text[] IS NULL \
                     OR CASE b.kind WHEN 0 THEN 'site:' ELSE 'group:' END || b.key = ANY($9)) \
                AND ($10::text IS NULL OR strpos(b.folded, $10) > 0) \
         ) \
         SELECT t.total, p.kind, p.key, p.name \
           FROM (SELECT COUNT(*) AS total FROM hits) AS t \
           LEFT JOIN hits p ON p.ord > $12 AND p.ord <= $12 + $13 \
          ORDER BY p.ord",
        shown = shown("$4"),
    );
    // (total, kind, key, name); the last three are null on a page past the end.
    type Hit = (i64, Option<i64>, Option<String>, Option<String>);
    let rows: Vec<Hit> = crate::db_types::query_tuple(&sql)
        .bind(&site_ids)
        .bind(&site_names)
        .bind(&site_folded)
        .bind(show_nsfw)
        .bind(&rated_ids)
        .bind(&ranks)
        .bind(&votes)
        .bind(kind)
        .bind(mine)
        .bind(search)
        .bind(sort)
        .bind(offset)
        .bind(PUBLIC_PAGE_SIZE as i64)
        .fetch_all(pool)
        .await?;
    let total = rows.first().map_or(0, |row| row.0.max(0) as usize);
    let page = rows
        .into_iter()
        .filter_map(|(_, kind, key, name)| {
            Some(PublicRow {
                kind: if kind? == 0 { Kind::Site } else { Kind::Group },
                key: key?,
                name: name?,
            })
        })
        .collect();
    Ok((page, total))
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

    fn names(rows: &[ListRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    async fn admin(pool: &Database, tags: &[Tag], q: &ListQuery) -> (Vec<ListRow>, usize) {
        let groups = admin_groups(pool, q.source.as_deref(), q.folded_search().as_deref())
            .await
            .unwrap();
        page(&groups, tags, q)
    }

    #[tokio::test]
    async fn the_admin_list_counts_series_per_source_and_applies_tags() {
        let pool = pool().await;
        seed(&pool).await;

        // Untagged: three credits, most series first, spellings folded. A member of a
        // joint credit counts the joint release's series too.
        let (rows, total) = admin(&pool, &[], &ListQuery::default()).await;
        assert_eq!(total, 3);
        assert_eq!(
            rows[1],
            ListRow {
                name: "Robo Scans".into(),
                series: 2,
                own: None,
                effective: None,
            }
        );
        assert_eq!(
            names(&rows),
            ["Human Scans", "Robo Scans", "Human Scans & Robo Scans"]
        );

        // Tag one group: the joint credit takes it; a tag on a name no series carries
        // is listed too.
        let tagged = tags(&[
            ("robo scans", AiLevel::Ai),
            ("Gone Scans", AiLevel::Partial),
        ]);
        let (rows, total) = admin(&pool, &tagged, &ListQuery::default()).await;
        assert_eq!(total, 4);
        let by_name = |n: &str| rows.iter().find(|r| r.name == n).unwrap().clone();
        assert_eq!(by_name("Robo Scans").own, Some(AiLevel::Ai));
        let joint = by_name("Human Scans & Robo Scans");
        assert_eq!((joint.own, joint.effective), (None, Some(AiLevel::Ai)));
        assert_eq!(by_name("Gone Scans").series, 0);

        for (ai, want) in [
            (AiFilter::Ai, vec!["Robo Scans", "Human Scans & Robo Scans"]),
            (AiFilter::Partial, vec!["Gone Scans"]),
            (AiFilter::Untagged, vec!["Human Scans"]),
        ] {
            let q = ListQuery {
                ai: Some(ai),
                ..ListQuery::default()
            };
            assert_eq!(names(&admin(&pool, &tagged, &q).await.0), want, "{ai:?}");
        }

        // Source filter: that source's credits and counts only.
        let q = ListQuery {
            source: Some("77".into()),
            ..ListQuery::default()
        };
        let (rows, total) = admin(&pool, &tagged, &q).await;
        assert_eq!((total, names(&rows)), (1, vec!["Human Scans"]));
        assert_eq!(rows[0].series, 1);
        let q = ListQuery {
            source: Some(MANGADEX_SOURCE.into()),
            search: Some("ROBO".into()),
            ..ListQuery::default()
        };
        let (rows, _) = admin(&pool, &tagged, &q).await;
        assert_eq!(names(&rows), ["Robo Scans", "Human Scans & Robo Scans"]);

        // Paging past the end is an empty page with the real total.
        let q = ListQuery {
            page: 9,
            ..ListQuery::default()
        };
        let (rows, total) = admin(&pool, &tagged, &q).await;
        assert!(rows.is_empty());
        assert_eq!(total, 4);
    }

    /// The (key, series) pairs of the index.
    async fn index(pool: &Database) -> Vec<(String, String)> {
        crate::db_types::query_tuple(
            "SELECT key, source_series_id FROM scanlator_series ORDER BY key, source_series_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, s)| (k.to_string(), s.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn the_trigger_keeps_the_index_current() {
        let pool = pool().await;
        seed(&pool).await;
        let groups: Vec<(String, String)> =
            crate::db_types::query_tuple("SELECT key, name FROM scanlator_group ORDER BY key")
                .fetch_all(&pool)
                .await
                .unwrap();
        // "No Group", '' and NULL are not credits; the first spelling names the group.
        assert_eq!(
            groups,
            pairs(&[
                ("human scans", "Human Scans"),
                ("human scans & robo scans", "Human Scans & Robo Scans"),
                ("robo scans", "Robo Scans"),
            ])
        );
        assert_eq!(
            index(&pool).await,
            pairs(&[
                ("human scans", "md1"),
                ("human scans", "sy1"),
                ("human scans & robo scans", "md1"),
                ("robo scans", "md1"),
                ("robo scans", "md2"),
            ])
        );

        // A credit that changes adds the new pair; a chapter that gains a credit too.
        sqlx::query("UPDATE chapter SET scanlator = '  New Scans ' WHERE id = 'c5'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE chapter SET scanlator = 'Late Scans' WHERE id = 'c8'")
            .execute(&pool)
            .await
            .unwrap();
        let listed = listed_groups(
            &pool,
            &["new scans".to_string(), "late scans".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            listed,
            pairs(&[("late scans", "Late Scans"), ("new scans", "New Scans")])
        );

        // Removing a series removes its pairs, and a group with none left is not listed.
        sqlx::query("DELETE FROM source_series WHERE id = 'sy1'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!index(&pool).await.iter().any(|(_, s)| s == "sy1"));
        let keys = ["new scans".to_string(), "human scans".to_string()];
        let listed = listed_groups(&pool, &keys, true).await.unwrap();
        // Human Scans keeps its joint release on md1.
        assert_eq!(listed, pairs(&[("human scans", "Human Scans")]));
    }

    /// One more work `work`, adult or not as Browse lists it, on MangaDex as series
    /// `series`, with one chapter for each credit.
    async fn work(pool: &Database, work: &str, nsfw: bool, credits: &[&str]) {
        sqlx::query(
            "INSERT INTO work (id, primary_title, created_at, updated_at) \
             VALUES ($1, $1, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
        )
        .bind(work)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO browse_catalogue (work_id, reader_id, title, is_nsfw, created_at) \
             VALUES ($1, $1, $1, $2, '2020-01-01T00:00:00Z')",
        )
        .bind(work)
        .bind(nsfw)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ($1, $1, 'mangadex', 'mangadex', $1, '2020-01-01T00:00:00Z')",
        )
        .bind(work)
        .execute(pool)
        .await
        .unwrap();
        for (n, credit) in credits.iter().enumerate() {
            sqlx::query(
                "INSERT INTO chapter (id, source_series_id, external_id, scanlator, created_at) \
                 VALUES ($1, $2, $1, $3, '2020-01-01T00:00:00Z')",
            )
            .bind(format!("{work}-{n}"))
            .bind(work)
            .bind(credit)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn a_group_with_only_adult_chapters_is_hidden_without_the_opt_in() {
        let pool = pool().await;
        work(
            &pool,
            "safe",
            false,
            &["Mixed Scans", "Joint Scans & Mixed Scans"],
        )
        .await;
        work(
            &pool,
            "adult",
            true,
            &["Mixed Scans", "Lewd Scans", "Joint Scans"],
        )
        .await;
        // Not in Browse at all: not known to be safe.
        sqlx::query("DELETE FROM browse_catalogue WHERE work_id = 'adult'")
            .execute(&pool)
            .await
            .unwrap();
        let keys: Vec<String> = [
            "mixed scans",
            "lewd scans",
            "joint scans",
            "joint scans & mixed scans",
        ]
        .map(String::from)
        .to_vec();
        let shown = |listed: Vec<(String, String)>| {
            listed.into_iter().map(|(key, _)| key).collect::<Vec<_>>()
        };
        // Joint Scans is adult on its own, but its joint release is not. The joint credit
        // itself is not a group of the list: both members are.
        assert_eq!(
            shown(listed_groups(&pool, &keys, false).await.unwrap()),
            ["joint scans", "mixed scans"]
        );
        assert_eq!(
            shown(listed_groups(&pool, &keys, true).await.unwrap()),
            ["joint scans", "lewd scans", "mixed scans"]
        );
    }

    #[tokio::test]
    async fn the_readers_list_filters_orders_and_pages() {
        let pool = pool().await;
        work(&pool, "w1", false, &["Big Scans", "Rated Scans"]).await;
        work(
            &pool,
            "w2",
            false,
            &["Alpha Scans", "Fresh Scans", "Big Scans"],
        )
        .await;
        // Only on an adult work: listed with the opt-in only.
        work(&pool, "w3", true, &["Lewd Scans"]).await;
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
        let sites = [
            Site {
                id: "77",
                name: "Zed Site",
            },
            Site {
                id: "88",
                name: "alpha site",
            },
        ];
        let list = |q: PublicQuery, nsfw: bool| {
            let (pool, ratings, mine, sites) = (&pool, &ratings, &mine, &sites);
            async move {
                let (rows, total) = public_page(pool, sites, ratings, mine, nsfw, &q)
                    .await
                    .unwrap();
                (rows.iter().map(PublicRow::id).collect::<Vec<_>>(), total)
            }
        };
        let sorted = |sort| {
            list(
                PublicQuery {
                    sort,
                    ..PublicQuery::default()
                },
                false,
            )
        };

        // TOP: the ranked by rank, then by number of scores, then by name.
        assert_eq!(
            sorted(PublicSort::Top).await.0,
            [
                "site:77",
                "group:rated scans",
                "group:fresh scans",
                "group:alpha scans",
                "site:88",
                "group:big scans",
            ]
        );
        assert_eq!(
            sorted(PublicSort::Votes).await.0,
            [
                "group:rated scans",
                "site:77",
                "group:fresh scans",
                "group:alpha scans",
                "site:88",
                "group:big scans",
            ]
        );
        assert_eq!(
            sorted(PublicSort::Name).await.0,
            [
                "group:alpha scans",
                "site:88",
                "group:big scans",
                "group:fresh scans",
                "group:rated scans",
                "site:77",
            ]
        );
        // The opt-in shows the adult-only group too.
        let all = list(PublicQuery::default(), true).await;
        assert_eq!(
            (all.1, all.0.contains(&"group:lewd scans".into())),
            (7, true)
        );

        // Filters: kind, part of the name in any case, and the viewer's own.
        let q = PublicQuery {
            kind: Some(Kind::Site),
            ..PublicQuery::default()
        };
        assert_eq!(
            list(q, false).await,
            (vec!["site:77".to_string(), "site:88".to_string()], 2)
        );
        let q = PublicQuery {
            search: Some(" ALPHA ".into()),
            ..PublicQuery::default()
        };
        assert_eq!(list(q, false).await.0, ["group:alpha scans", "site:88"]);
        let q = PublicQuery {
            mine: true,
            ..PublicQuery::default()
        };
        assert_eq!(list(q, false).await.0, ["site:77", "group:fresh scans"]);
        // Signed out, "mine" is empty.
        let (rows, total) = public_page(
            &pool,
            &sites,
            &ratings,
            &Scores::default(),
            false,
            &PublicQuery {
                mine: true,
                ..PublicQuery::default()
            },
        )
        .await
        .unwrap();
        assert_eq!((rows.len(), total), (0, 0));

        // Paging past the end is an empty page with the real total.
        let q = PublicQuery {
            page: 2,
            ..PublicQuery::default()
        };
        assert_eq!(list(q, false).await, (vec![], 6));
    }

    #[tokio::test]
    async fn only_a_rated_group_that_exists_counts() {
        let pool = pool().await;
        work(
            &pool,
            "w1",
            true,
            &["Here Scans", "Here Scans & Other Scans", "Other Scans"],
        )
        .await;
        let user = crate::db_types::test_uuid("rater");
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, created_at) \
             VALUES ($1, 'rater', 'rater@example.com', 'x', '2020-01-01T00:00:00Z')",
        )
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        for id in [
            "group:here scans",
            "group:here scans & other scans",
            "group:gone",
            "site:77",
            "site:99",
        ] {
            sqlx::query(
                "INSERT INTO scanlator_rating (scanlator_id, user_id, score, created_at, updated_at) \
                 VALUES ($1, $2, 7, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Adult-only groups still exist; a folded joint credit and an unknown name do not.
        let ratings = ratings(&pool, |site| site == "77").await.unwrap();
        let mut ids: Vec<String> = ratings.iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, ["group:here scans", "site:77"]);
    }
}
