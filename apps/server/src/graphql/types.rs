//! GraphQL types — a 1:1 code-first mirror of `packages/api/src/schema/komika.graphql`.

use async_graphql::{Enum, InputObject, MaybeUndefined, SimpleObject, ID};

use crate::suwayomi::SuwayomiChapter;

#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ComicType {
    Manga,
    Manhwa,
    Manhua,
    Webtoon,
    Comic,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum SeriesStatus {
    Ongoing,
    Completed,
    Hiatus,
    Cancelled,
    Unknown,
}

/// How Browse orders the catalogue. `TRENDING` is the default because it is the only
/// ordering that reflects what people are actually reading; the other four are the
/// deterministic ones a user reaches for when they want a specific slice.
///
/// `RATING` and `TRENDING` are implemented as two-phase queries (see
/// `browse::browse_catalogue`) because their ranking key exists for a tiny minority of
/// works, which makes the ranked set a strict PREFIX of the page order.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum BrowseSort {
    /// Most-viewed over the last 24 hours first, then newest release.
    Trending,
    /// Newest upstream chapter release first.
    Newest,
    /// Works Komiq most recently added to the catalogue first — ordered by
    /// `browse_catalogue.created_at`, the verbatim copy of `work.created_at`.
    LatestAdded,
    /// Highest user-review average first, then newest release for the unrated.
    Rating,
    /// Most English chapters first.
    Chapters,
}

/// The content-rating floor Browse filters by. CUMULATIVE: each value admits everything
/// the milder ones do.
///
/// This NARROWS within the viewer's NSFW gate and can never widen it — for an opted-out
/// viewer `EROTICA` and `PORNOGRAPHIC` return exactly the `SUGGESTIVE` set, because no
/// `is_nsfw = 0` work carries either rating. See `browse::build_where`.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ContentRatingFilter {
    /// No rating clause at all.
    All,
    Safe,
    Suggestive,
    Erotica,
    /// Admits every stored rating, so it is equivalent to `ALL` — kept as a member so a
    /// client can name the top tier explicitly rather than inferring it.
    Pornographic,
    /// The ONE non-cumulative member: adult-rated works *only*, i.e. the complement of
    /// `SAFE` rather than a ceiling above it.
    ///
    /// It exists because "show me only the adult titles" is not expressible as a tier —
    /// every tier admits `safe` — and it is the other half of the control that replaced
    /// Browse's NSFW toggle. It filters on the materialized `is_nsfw` flag, NOT on
    /// `content_rating`, so it tracks admin `is_nsfw_override` decisions rather than
    /// disagreeing with them.
    ///
    /// Still cannot widen the viewer's gate: for an opted-out viewer this yields an EMPTY
    /// page, which is the honest answer (they may not see those works) rather than
    /// silently degrading to "everything".
    NsfwOnly,
}

/// The `content_rating` values one cumulative tier admits, or `None` for "no clause".
///
/// `ALL` and `PORNOGRAPHIC` both return `None` rather than the full four-value list: they
/// are the same set, and omitting the predicate entirely is both cheaper (no index payload
/// check per row) and immune to a fifth rating appearing upstream — a hard-coded list would
/// silently start EXCLUDING it.
pub fn content_rating_tier(f: ContentRatingFilter) -> Option<&'static [&'static str]> {
    match f {
        // `NSFW_ONLY` is not a rating tier at all — it constrains `is_nsfw` instead, via
        // `content_rating_nsfw_only`. Emitting no rating clause here is what lets the two
        // live side by side without either having to know about the other.
        ContentRatingFilter::All
        | ContentRatingFilter::Pornographic
        | ContentRatingFilter::NsfwOnly => None,
        ContentRatingFilter::Safe => Some(&["safe"]),
        ContentRatingFilter::Suggestive => Some(&["safe", "suggestive"]),
        ContentRatingFilter::Erotica => Some(&["safe", "suggestive", "erotica"]),
    }
}

/// Whether this filter restricts to adult-rated works only (`is_nsfw = 1`).
///
/// Separate from [`content_rating_tier`] because the two constrain DIFFERENT columns:
/// tiers narrow `content_rating`, this narrows the materialized `is_nsfw` flag. Folding
/// them into one return type would force every caller to match on a shape that is
/// `None`-or-list for four members and a boolean for the fifth.
pub fn content_rating_nsfw_only(f: ContentRatingFilter) -> bool {
    matches!(f, ContentRatingFilter::NsfwOnly)
}

// ---- Browse v2 ("MangaDex advanced search") --------------------------------------------
//
// The shared contract with the reader (`browse-contract.md`): every name below is fixed by
// it, which is why `Demographic::None` and friends read like the wire format rather than
// like Rust. `search(filter: BrowseFilter)` is OPTIONAL and, when present, wins over every
// v1 argument; see `graphql::search` and `browse::BrowseQuery`.

/// How several tags combine (MangaDex's `includedTagsMode` / `excludedTagsMode`).
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum TagMode {
    And,
    Or,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// Browse v2's orderings. Every one ends in a `work_id` tiebreak in the same direction, and
/// sorts unknown (NULL) keys LAST in both directions.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum BrowseSortField {
    /// Text-search ranking; without a query the server falls back to `LATEST_UPDATE`.
    Relevance,
    /// Newest chapter release.
    LatestUpdate,
    /// When the work entered Komika's catalogue.
    RecentlyAdded,
    /// Our own 24-hour view ranking (the v1 `TRENDING`).
    MostViewed,
    /// MangaDex follows.
    MostFollows,
    /// MangaDex's bayesian rating (0-10).
    HighestRated,
    /// Our own readers' review average (the v1 `RATING`). Unrated works sort last.
    ReaderRated,
    /// English chapter count.
    MostChapters,
    /// A-Z; the one sort whose default direction is ASC.
    Title,
    /// Publication year.
    Year,
}

/// MangaDex's publication demographic. `NONE` selects works with no demographic.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum Demographic {
    Shounen,
    Shoujo,
    Seinen,
    Josei,
    None,
}

/// MangaDex's four content ratings — EXACT values, not cumulative tiers (contrast
/// [`ContentRatingFilter`]). Narrows INSIDE the viewer's NSFW gate, never widens it.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ContentRating {
    Safe,
    Suggestive,
    Erotica,
    Pornographic,
}

/// MangaDex's four tag groups.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum TagGroup {
    Genre,
    Theme,
    Format,
    Content,
}

/// Browse v2's whole filter + sort. Every field is optional; an absent field is no
/// constraint. Lists are bounded (`includedTags`/`excludedTags` 32, `originalLanguages` 16)
/// and a longer list is an ERROR rather than truncated, because truncating an AND list
/// silently widens the result.
#[derive(InputObject, Clone, Debug, Default)]
pub struct BrowseFilter {
    /// Tag ids (`TagInfo.id`). Unknown ids are not an error — they simply match no work.
    pub included_tags: Option<Vec<String>>,
    /// Default AND.
    pub included_tags_mode: Option<TagMode>,
    pub excluded_tags: Option<Vec<String>>,
    /// Default OR.
    pub excluded_tags_mode: Option<TagMode>,
    pub status: Option<Vec<SeriesStatus>>,
    /// ALL selected formats apply (OR). WEBTOON folds into MANHWA and COMIC into MANGA.
    pub types: Option<Vec<ComicType>>,
    pub demographics: Option<Vec<Demographic>>,
    /// Narrows INSIDE the viewer's NSFW gate, never widens it.
    pub content_ratings: Option<Vec<ContentRating>>,
    /// MangaDex `originalLanguage` codes: "ja", "ko", "zh", "zh-hk", "en", …
    pub original_languages: Option<Vec<String>>,
    /// Clamped to 1900..=2100; a reversed range is swapped.
    pub year_from: Option<i32>,
    pub year_to: Option<i32>,
    /// `<= 0` is no filter.
    pub min_chapters: Option<i32>,
    /// MangaDex bayesian rating, 0-10. `<= 0` is no filter; excludes works with no rating.
    pub min_rating: Option<f64>,
    /// MangaDex follows. `<= 0` is no filter; excludes works with no statistics.
    pub min_follows: Option<i32>,
    /// `true` = only works with a known chapter; `false` = only works with none.
    pub has_chapters: Option<bool>,
    /// "mangadex", "suwayomi", or a Suwayomi source id.
    pub source: Option<String>,
    /// Default: RELEVANCE with a text query, LATEST_UPDATE without.
    pub sort: Option<BrowseSortField>,
    /// Default DESC (TITLE defaults ASC).
    pub direction: Option<SortDirection>,
}

/// One entry of the `tags` query.
#[derive(SimpleObject, Clone, Debug)]
pub struct TagInfo {
    /// URL-safe slug of the canonical English name ("boys-love", "sci-fi", "4-koma").
    pub id: String,
    /// Display name ("Boys' Love").
    pub name: String,
    pub group: TagGroup,
    /// Works carrying this tag that the VIEWER may see (NSFW-gated).
    pub count: i32,
}

/// One source Browse can filter by (`browseSources`).
#[derive(SimpleObject, Clone, Debug)]
pub struct BrowseSource {
    /// What `BrowseFilter.source` takes: "mangadex", or a Suwayomi source id.
    pub id: String,
    /// The source's display name; its extension package name, then its id, when the
    /// engine could not be asked.
    pub name: String,
    /// The source's language code ("en", "all", …), when known.
    pub lang: Option<String>,
    /// Works from this source that the VIEWER may see in Browse (NSFW-gated). The same
    /// number the `source` filter returns on its own.
    pub count: i32,
}

/// What an admin says about a scanlator's use of AI.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ScanlatorAi {
    /// The translation is machine made.
    Ai,
    /// AI is part of the work, for example machine translation that a person edits.
    Partial,
}

impl From<crate::scanlators::AiLevel> for ScanlatorAi {
    fn from(level: crate::scanlators::AiLevel) -> Self {
        match level {
            crate::scanlators::AiLevel::Ai => ScanlatorAi::Ai,
            crate::scanlators::AiLevel::Partial => ScanlatorAi::Partial,
        }
    }
}

impl From<ScanlatorAi> for crate::scanlators::AiLevel {
    fn from(ai: ScanlatorAi) -> Self {
        match ai {
            ScanlatorAi::Ai => crate::scanlators::AiLevel::Ai,
            ScanlatorAi::Partial => crate::scanlators::AiLevel::Partial,
        }
    }
}

/// Which scanlators the admin list shows, by the tag that APPLIES to them (their own, or
/// one a joint credit takes from a member).
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ScanlatorAiFilter {
    /// Tagged AI or partial AI.
    Tagged,
    Ai,
    Partial,
    Untagged,
}

/// One tagged scanlator (`scanlatorTags`): what a reader app needs to mark a chapter
/// copy as AI.
#[derive(SimpleObject, Clone, Debug)]
pub struct ScanlatorTag {
    /// The credit as the admin tagged it. Match it to `Chapter.scanlator` /
    /// `ChapterSource.scanlator` trimmed and case-folded; a joint credit ("A & B") with
    /// no tag of its own takes the strongest tag of its members (AI over PARTIAL).
    pub name: String,
    pub ai: ScanlatorAi,
}

/// One row of the admin scanlator list (`scanlators`).
#[derive(SimpleObject, Clone, Debug)]
pub struct Scanlator {
    /// The credit as chapters carry it (the most common spelling).
    pub name: String,
    /// Chapter copies credited to this name; on the filtered source when one is given.
    pub chapter_count: i32,
    /// Series with at least one such chapter.
    pub series_count: i32,
    /// The tag stored for exactly this name; null when it has none.
    pub ai: Option<ScanlatorAi>,
    /// The tag that applies: `ai`, else the strongest tag among the members of a joint
    /// credit ("A & B"). This is what readers see.
    pub effective_ai: Option<ScanlatorAi>,
}

#[derive(SimpleObject, Clone, Debug)]
pub struct ScanlatorPage {
    #[graphql(complexity = "50 * child_complexity")]
    pub items: Vec<Scanlator>,
    pub page: i32,
    pub has_next_page: bool,
    /// The FILTERED count.
    pub total: i32,
}

/// One tagged source (`sourceAiTags`): a site that translates its own series, so its
/// chapters credit no group and the source is the scanlator.
#[derive(SimpleObject, Clone, Debug)]
pub struct SourceAiTag {
    /// A Suwayomi source id. Match it to `ChapterSource.sourceId`, or to the `sourceId`
    /// of the series a `Chapter` belongs to. A copy takes the stronger of this tag and
    /// the tag on its `scanlator` credit (AI over PARTIAL).
    pub source_id: String,
    pub ai: ScanlatorAi,
}

/// One row of the admin site list (`scanlatorSites`): a source, as a scanlator.
#[derive(SimpleObject, Clone, Debug)]
pub struct ScanlatorSite {
    pub source_id: String,
    /// Display name; the extension package name, then the id, when the engine did not
    /// answer.
    pub name: String,
    pub lang: Option<String>,
    /// Series in the catalogue that have this source. 0 for a tagged source that has
    /// none left.
    pub series_count: i32,
    /// The tag stored for the source; null when it has none.
    pub ai: Option<ScanlatorAi>,
}

/// Which of the two things a scanlator is.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ScanlatorKind {
    /// A source that translates its own series.
    Site,
    /// A group credited on chapters.
    Group,
}

impl From<crate::scanlators::Kind> for ScanlatorKind {
    fn from(kind: crate::scanlators::Kind) -> Self {
        match kind {
            crate::scanlators::Kind::Site => ScanlatorKind::Site,
            crate::scanlators::Kind::Group => ScanlatorKind::Group,
        }
    }
}

impl From<ScanlatorKind> for crate::scanlators::Kind {
    fn from(kind: ScanlatorKind) -> Self {
        match kind {
            ScanlatorKind::Site => crate::scanlators::Kind::Site,
            ScanlatorKind::Group => crate::scanlators::Kind::Group,
        }
    }
}

/// The order of the readers' scanlator list (`scanlatorRatings`). Equal rows order by
/// chapters, then series, then name.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ScanlatorSort {
    /// Ranked scanlators by `rank`, then the rest by number of scores.
    Top,
    /// Most scores first.
    Votes,
    /// Most chapters first. A site has no chapter count and reads as 0.
    Chapters,
    /// By name.
    Name,
}

/// One scanlator as readers see it (`scanlatorRatings`, `ratedScanlator`): a site or a
/// group, with the scores readers gave it.
#[derive(SimpleObject, Clone, Debug)]
pub struct RatedScanlator {
    /// `site:<source id>` or `group:<name>` (the name trimmed and lowercased). Pass it to
    /// `rateScanlator`, and as `targetId` of a `"scanlator"` comment thread.
    pub id: ID,
    pub kind: ScanlatorKind,
    /// A site's display name; a group's credit as chapters carry it.
    pub name: String,
    /// The site's language code ("en", "all", …), when known. Null for a group.
    pub lang: Option<String>,
    /// Series it released. For a group this includes its joint releases, and a series it
    /// released both alone and jointly counts twice.
    pub series_count: i32,
    /// Chapter copies credited to the group, joint releases included. Null for a site:
    /// the server keeps no count of a source's chapters.
    pub chapter_count: Option<i32>,
    /// The AI tag that applies: a site's own; a group's own, else the strongest among the
    /// members of its name when it is a joint credit. Null when untagged.
    pub ai: Option<ScanlatorAi>,
    /// The plain average of the scores. Null when nobody rated it.
    pub rating: Option<f64>,
    pub rating_count: i32,
    /// 1-based place among every scanlator on the server with at least 5 scores, by an
    /// average weighted toward the server's mean. Null with fewer than 5 scores. It does
    /// not change with `search`, `kind` or `mine`, nor with the viewer: a scanlator hidden
    /// from a viewer who does not see adult content keeps its place, so the places such a
    /// viewer is shown can skip a number.
    pub rank: Option<i32>,
    /// Exactly 10 numbers: how many gave score 1, 2, … 10.
    pub distribution: Vec<i32>,
    /// The viewer's own score. Null when signed out or not rated.
    pub my_score: Option<i32>,
    /// Comments a reader can read in its thread (`comments(targetType: "scanlator")`),
    /// replies included: deleted comments and those of banned accounts do not count.
    pub comment_count: i32,
}

#[derive(SimpleObject, Clone, Debug)]
pub struct RatedScanlatorPage {
    #[graphql(complexity = "30 * child_complexity")]
    pub items: Vec<RatedScanlator>,
    pub page: i32,
    pub has_next_page: bool,
    /// The FILTERED count.
    pub total: i32,
}

/// One content block rule (`siteBlockRules`, `myBlockRules`). It matches a series that
/// carries ALL of `tags` and, when `ratings` is not empty, has ONE of them. A series is
/// blocked when any rule matches it.
#[derive(SimpleObject, Clone, Debug, PartialEq, Eq)]
pub struct BlockRule {
    /// Tag ids (`TagInfo.id`), sorted.
    pub tags: Vec<String>,
    /// Mildest first. Empty: any rating. Compared with the EFFECTIVE rating, the one
    /// `BrowseFilter.contentRatings` filters on.
    pub ratings: Vec<ContentRating>,
}

impl From<&crate::block::BlockRule> for BlockRule {
    fn from(rule: &crate::block::BlockRule) -> Self {
        BlockRule {
            tags: rule.tags().to_vec(),
            ratings: rule
                .ratings()
                .iter()
                .filter_map(|word| content_rating_from_word(word))
                .collect(),
        }
    }
}

/// One rule to store. A rule needs a tag or a rating.
#[derive(InputObject, Clone, Debug)]
pub struct BlockRuleInput {
    /// At most 8.
    pub tags: Option<Vec<String>>,
    pub ratings: Option<Vec<ContentRating>>,
}

/// What a takedown names: a work (every copy of it, its covers and its merged-away ids),
/// or one copy (one upload of one chapter by one source, `Page.mediaPath`'s `mc_` id).
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum TakedownKind {
    Work,
    Copy,
}

impl From<TakedownKind> for crate::media_policy::Kind {
    fn from(kind: TakedownKind) -> Self {
        match kind {
            TakedownKind::Work => crate::media_policy::Kind::Work,
            TakedownKind::Copy => crate::media_policy::Kind::Copy,
        }
    }
}

/// One operator takedown (`mediaTakedowns`).
#[derive(SimpleObject, Clone, Debug)]
pub struct MediaTakedown {
    pub kind: TakedownKind,
    pub subject_id: ID,
    pub reason: String,
    /// Who set it: an admin's username, or `cli`.
    pub created_by: Option<String>,
    /// RFC 3339.
    pub created_at: String,
    /// For a copy row recorded with a work takedown: that work. Lifting the work lifts it.
    pub via_work: Option<ID>,
}

impl From<crate::media_policy::Takedown> for MediaTakedown {
    fn from(t: crate::media_policy::Takedown) -> Self {
        MediaTakedown {
            kind: if t.subject_kind == "copy" {
                TakedownKind::Copy
            } else {
                TakedownKind::Work
            },
            subject_id: ID(t.subject_id),
            reason: t.reason,
            created_by: t.created_by,
            created_at: t.created_at.to_rfc3339(),
            via_work: t.via_work.map(ID),
        }
    }
}

/// `TagGroup` from the stored `tag.tag_group` word. Anything unrecognized is filed under
/// GENRE, matching `tags::rebuild_tag_table`'s default for free-text admin tags.
pub fn tag_group_from_word(w: &str) -> TagGroup {
    match w {
        "theme" => TagGroup::Theme,
        "format" => TagGroup::Format,
        "content" => TagGroup::Content,
        _ => TagGroup::Genre,
    }
}

/// The stored word for a demographic filter value (`browse_catalogue.demographic`, or
/// `"none"` for the IS NULL arm — see `browse::build_where`).
pub fn demographic_word(d: Demographic) -> &'static str {
    match d {
        Demographic::Shounen => "shounen",
        Demographic::Shoujo => "shoujo",
        Demographic::Seinen => "seinen",
        Demographic::Josei => "josei",
        Demographic::None => "none",
    }
}

/// The demographic a stored word names (the inverse of [`demographic_word`]). A work
/// with no demographic stores NULL, not `"none"`.
pub fn demographic_from_word(word: &str) -> Option<Demographic> {
    match word.trim().to_lowercase().as_str() {
        "shounen" => Some(Demographic::Shounen),
        "shoujo" => Some(Demographic::Shoujo),
        "seinen" => Some(Demographic::Seinen),
        "josei" => Some(Demographic::Josei),
        _ => None,
    }
}

/// The rating a stored `content_rating` word names.
pub fn content_rating_from_word(word: &str) -> Option<ContentRating> {
    match word.trim().to_lowercase().as_str() {
        "safe" => Some(ContentRating::Safe),
        "suggestive" => Some(ContentRating::Suggestive),
        "erotica" => Some(ContentRating::Erotica),
        "pornographic" => Some(ContentRating::Pornographic),
        _ => None,
    }
}

/// The stored `content_rating` word for a v2 rating.
pub fn content_rating_word(r: ContentRating) -> &'static str {
    match r {
        ContentRating::Safe => "safe",
        ContentRating::Suggestive => "suggestive",
        ContentRating::Erotica => "erotica",
        ContentRating::Pornographic => "pornographic",
    }
}

/// The internal sort for a v2 (field, direction). TITLE defaults ASC, everything else DESC.
pub fn browse_sort(
    field: BrowseSortField,
    direction: Option<SortDirection>,
) -> crate::browse::Sort {
    use crate::browse::SortKey;
    let key = match field {
        BrowseSortField::Relevance => SortKey::Relevance,
        BrowseSortField::LatestUpdate => SortKey::LatestUpdate,
        BrowseSortField::RecentlyAdded => SortKey::RecentlyAdded,
        BrowseSortField::MostViewed => SortKey::MostViewed,
        BrowseSortField::MostFollows => SortKey::MostFollows,
        BrowseSortField::HighestRated => SortKey::HighestRated,
        BrowseSortField::ReaderRated => SortKey::CommunityRating,
        BrowseSortField::MostChapters => SortKey::MostChapters,
        BrowseSortField::Title => SortKey::Title,
        BrowseSortField::Year => SortKey::Year,
    };
    let desc = match direction {
        Some(SortDirection::Asc) => false,
        Some(SortDirection::Desc) => true,
        None => field != BrowseSortField::Title,
    };
    crate::browse::Sort { key, desc }
}

/// The COLLAPSED format word `feed_series_updates.comic_type` stores, per the reader's
/// `toViewType`: `WEBTOON` folds into `MANHWA` and `COMIC` into `MANGA`.
///
/// One function rather than a `match` copied into each of the three places that needs it
/// (the feed refresh that WRITES the column, and the `updatesFeed` + Browse filters that
/// READ it) — if the three ever disagreed, a format tab would return an empty page for a
/// format the refresh had stored under a different word. Collapsing at write time is what
/// keeps the filter a single indexed equality; see migration 0064.
pub fn collapsed_comic_type_word(t: ComicType) -> &'static str {
    match t {
        ComicType::Manhwa | ComicType::Webtoon => "MANHWA",
        ComicType::Manhua => "MANHUA",
        ComicType::Manga | ComicType::Comic => "MANGA",
    }
}

#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum DiscoveryFeedKind {
    Popular,
    Trending,
    RecentlyUpdated,
    RecentlyAdded,
    Genre,
}

#[derive(SimpleObject, Clone)]
pub struct RatingSummary {
    pub average: f64,
    pub count: i32,
    /// Length 10; index 0 => score 1.
    pub distribution: Vec<i32>,
}

impl RatingSummary {
    pub fn empty() -> Self {
        Self {
            average: 0.0,
            count: 0,
            distribution: vec![0; 10],
        }
    }
}

#[derive(SimpleObject, Clone)]
pub struct ScanPolicy {
    pub avg_interval_hours: f64,
    pub override_interval_hours: Option<f64>,
    pub poll_every_minutes: i32,
    /// Effective paused state (folded: forced override, else auto-by-status).
    pub paused: bool,
    /// Raw admin overrides, so the admin console can distinguish an explicit
    /// choice from the status default even when they coincide. `null` = no
    /// override (scanner auto-decides).
    pub status_override: Option<SeriesStatus>,
    pub paused_override: Option<bool>,
    /// Raw admin poll-interval override (minutes). `null` = no override, so the
    /// effective `poll_every_minutes` above is the folded default. The admin
    /// console decodes its field from this so an unset poll stays unset.
    pub poll_every_minutes_override: Option<i32>,
    pub last_scanned_at: Option<String>,
    pub next_scan_at: Option<String>,
}

/// One localized description of a work (S2/H2), keyed by BCP-47-ish language tag.
#[derive(SimpleObject, Clone)]
pub struct LocalizedDescription {
    pub lang: String,
    pub description: String,
}

/// One author/artist credit of a work (S2/H2). `role` is `"author"` or `"artist"`.
#[derive(SimpleObject, Clone)]
pub struct Credit {
    pub role: String,
    pub name: String,
}

#[derive(SimpleObject, Clone)]
pub struct MetadataTag {
    pub id: Option<String>,
    pub name: String,
    pub group: String,
}
#[derive(SimpleObject, Clone)]
pub struct TrackerLink {
    pub provider: String,
    pub external_id: String,
    pub url: String,
}
#[derive(SimpleObject, Clone)]
pub struct SeriesMetadata {
    pub year: Option<i32>,
    pub original_language: Option<String>,
    pub demographic: Option<String>,
    pub content_rating: Option<String>,
    pub tags: Vec<MetadataTag>,
    pub trackers: Vec<TrackerLink>,
}

/// One cover of a work (F2). `url` is a proxy-ready MangaDex cover URL (the client
/// resolves it through the Worker, like `coverUrl`); `isPrimary` marks the main
/// cover mirrored on `Series.coverUrl`.
#[derive(SimpleObject, Clone)]
pub struct Cover {
    /// The cover's `fileName` leaf (`covers/{mangadexId}/{fileName}`).
    pub file_name: String,
    /// Proxy-ready full cover URL (`uploads.mangadex.org/covers/...`).
    pub url: String,
    /// Proxy-ready 512px thumbnail URL.
    pub thumbnail_url: String,
    pub lang: Option<String>,
    pub volume: Option<String>,
    pub is_primary: bool,
}

/// Per-series view (chapter-read) counts across the three windows — the popularity
/// signal behind Trending, surfaced on the series page. Resolved lazily on `Series`
/// (only when selected), so feeds that don't ask for it pay nothing.
#[derive(SimpleObject, Clone, Copy, Default)]
pub struct SeriesViews {
    /// All-time total views.
    pub total: i32,
    /// Views in the last 7 days. Named explicitly so the GraphQL field is `last7d`
    /// (async-graphql's default camelCasing would otherwise emit `last7D`).
    #[graphql(name = "last7d")]
    pub last7d: i32,
    /// Views in the last 24 hours (see `last7d` re: the explicit name).
    #[graphql(name = "last24h")]
    pub last24h: i32,
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Series {
    pub id: ID,
    pub title: String,
    pub alt_titles: Vec<String>,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub description: Option<String>,
    pub genres: Vec<String>,
    #[graphql(name = "type")]
    pub r#type: ComicType,
    pub status: SeriesStatus,
    pub cover_url: String,
    pub source_id: String,
    pub chapter_count: i32,
    // `is_marked` (per-user library membership) is resolved dynamically per viewer
    // in the `#[ComplexObject]` impl — it is NOT a stored field, so every feed
    // reflects the signed-in user's own library rather than a shared flag.
    /// NSFW per the canonical model (CATALOGUE.md §2); false until the series is
    /// catalogued. Drives `show_nsfw` filtering of discovery/search/updates feeds.
    pub is_nsfw: bool,
    /// Our own readers' rating of the work. Never MangaDex's: see `mangadex_rating`.
    pub rating: RatingSummary,
    /// MangaDex's community rating of the work, 0-10 (statistics cache, `work_stats`).
    /// Null when MangaDex has none, when the work has no MangaDex statistics, or when
    /// the admin turned MangaDex ratings off (`show_mangadex_ratings`).
    pub mangadex_rating: Option<f64>,
    pub scan: ScanPolicy,
    pub created_at: String,
    /// Last metadata touch. Do NOT use as "recently updated" — it moves on every poll
    /// and cover/metadata refresh. For chapter recency use `latest_chapter_at`.
    pub updated_at: String,
    /// Real newest-chapter time (ISO), from the actual chapter publish/upload date.
    /// Empty when the series has no dated chapter cached yet. This is the field feeds
    /// and the reader's "· 4h" label should use.
    pub latest_chapter_at: String,
    /// The newest chapter's LABEL — "151", "10.5", "Oneshot" (migration 0095).
    ///
    /// **This is NOT [`Self::chapter_count`] and the two must never substitute for each
    /// other.** `chapter_count` is how many chapters we know of; this is what the newest
    /// one is CALLED. A series with 12 mirrored chapters whose newest is Ch. 151 is
    /// ordinary — the mirror is partial — and printing "Ch. 12" for it is F4, the specific
    /// bug the chapter-number contract exists to prevent. Browse renders both:
    /// `12 ch · Ch. 151`.
    ///
    /// `None` means WE DO NOT KNOW, and the client must then print the count alone. It is
    /// never a licence to fall back to `chapter_count`.
    ///
    /// Populated on the BROWSE path (`browse_catalogue.latest_chapter`, itself a verbatim
    /// copy of `feed_series_updates.latest_chapter`, itself the ledger projection's
    /// `release_event.label`). The Suwayomi-live path (`map_series`) leaves it `None`: that
    /// object is assembled from a `SuwayomiManga`, which carries a chapter COUNT and no
    /// label at all, and inventing one there is exactly the substitution above.
    pub latest_chapter: Option<String>,
    /// When OUR scanner first detected this series' newest chapter
    /// (`series_scan_state.last_new_chapter_at`, raw ISO). Populated by the BATCHED
    /// mappers (`map_series_batch`, `map_browse_rows`) so the GraphQL `detectedAt`
    /// field below resolves without its old per-row query — one grouped SELECT per
    /// page instead of ~80 on a home render. Skipped by the derive (not part of the
    /// schema): the public `detectedAt` is the ComplexObject resolver, which reads
    /// this and normalizes the offset. `None` = not populated / no scan-state row.
    #[graphql(skip)]
    pub detected_at: Option<String>,
}

/// Per-series read progress for the whole library, returned by `libraryProgress`
/// in ONE batched query. Lets the Library and Profile screens shelve every series
/// by progress without fetching each series' chapter list (the old N+1 that hung
/// those pages).
///
/// `read` and `total` are two counts over the SAME population — distinct chapter keys
/// on the chapter spine — so `read <= total` always holds. See the resolver for why
/// that matters.
#[derive(SimpleObject, Clone)]
pub struct SeriesProgress {
    /// The library series id — a canonical `w_` work id or a numeric Suwayomi id,
    /// matching `Series.id` for library series.
    pub id: ID,
    /// Distinct chapters the viewer has marked read, deduped across sources: a chapter
    /// carried by two scanlators (or by both MangaDex and a Suwayomi mirror) counts once,
    /// the same way `total` counts it once.
    pub read: i32,
    /// Distinct chapters that exist for this series across all its sources.
    ///
    /// 0 only when the series has no cached chapters at all; clients read it as
    /// `p.total || s.chapterCount` and so fall back to the series' own count there.
    pub total: i32,
    /// When the viewer last made progress on this series (RFC 3339), or null if never.
    ///
    /// `MAX(updated_at)` over their progress rows, so it moves on ANY progress — a
    /// mid-chapter page position counts, not just finishing one. That is what "last read"
    /// has to mean for ordering a Continue-reading shelf: a series you are three pages
    /// into is more current than one you finished a month ago.
    pub last_read_at: Option<String>,
}

#[derive(SimpleObject, Clone)]
pub struct Chapter {
    pub id: ID,
    pub series_id: ID,
    /// The chapter's number, or `0` when it has none. **Prefer [`Self::label`]** — a
    /// oneshot used to arrive here as a hard-coded `0.0` with no way to tell it from a
    /// real `Chapter 0`, which is why the reader printed "Chapter 0" for 21,422 works.
    pub number: f64,
    /// What to print: "45", "10.5", "Oneshot" (Phase A2).
    pub label: String,
    pub title: Option<String>,
    pub page_count: i32,
    /// When the chapter became READABLE — `readableAt` where MangaDex provides it, not the
    /// `publishAt` that is a 2037 sentinel on external chapters (migration 0073).
    ///
    /// This is the SOURCE's clock. For when WE first had it, see [`Self::found_at`].
    pub uploaded_at: Option<String>,
    /// When WE first recorded this chapter as available — `release_event.announce_at`
    /// (migration 0099), joined on `(work_id, chapter_key)`.
    ///
    /// The two clocks are independent and the gap between them is the point: a source that
    /// holds a chapter behind early access publishes it (per [`Self::uploaded_at`]) hours or
    /// days before anyone here can read it, and only this column says when that wait ended.
    ///
    /// **`None` MEANS "WE DO NOT KNOW" AND MUST RENDER AS AN EM DASH, NEVER AS
    /// [`Self::uploaded_at`].** That distinction is the whole value of the field: falling
    /// back would silently assert we found a chapter the instant it was published, which is
    /// the exact claim this column exists to stop the UI from making. Expect it to be null
    /// for essentially every chapter that predates migration 0099 — there is no honest
    /// record of when we first saw those, and none was invented.
    pub found_at: Option<String>,
    /// Set when the chapter is hosted off-site and has no pages for us to serve
    /// (MangaPlus, Comikey, NamiComi, BiliBili — ~35,000 chapters, 4% of the mirror). The
    /// reader must send the user here instead of asking for pages it will never get.
    /// `externalUrl IS NOT NULL` is the only valid test — a `pages` count of 0 is not.
    pub external_url: Option<String>,
    pub scanlator: Option<String>,
    pub read: bool,
    pub last_page_read: i32,
    pub bookmarked: bool,
    pub is_downloaded: bool,
}

#[derive(SimpleObject, Clone)]
pub struct Page {
    pub media_path: Option<String>,
    pub media_sha256: Option<String>,
    /// Page sources are capped at 32 MiB, within GraphQL Int.
    pub media_length: Option<i32>,
    pub media_mime: Option<String>,
    pub index: i32,
    pub source_url: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// The install/pin coordinates for one Suwayomi source's extension (§2.2), joined
/// onto a `WorkSource`. Absent for a MangaDex-native mapping (there is no extension
/// to install) and for any source that hasn't been catalogued with an extension yet.
#[derive(SimpleObject, Clone)]
pub struct SourceExtension {
    pub pkg_name: String,
    pub repo_url: String,
    pub apk_name: Option<String>,
    /// `Int` (i32); the DB column is INTEGER — cast on read.
    pub version_code: Option<i32>,
    pub lang: Option<String>,
}

/// One catalogued source mapping for a canonical work, plus the extension coordinates
/// a native client needs to install and fetch from it (§2.2). `extension` is null for
/// the MangaDex-native mapping (fetched via MangaDex@Home, not an extension).
#[derive(SimpleObject, Clone)]
pub struct WorkSource {
    pub source_type: String,
    pub source_id: String,
    pub source_key: String,
    pub source_url: Option<String>,
    pub is_nsfw: bool,
    /// The extension's language (there is no per-source lang on `source_series`);
    /// null when the source has no catalogued extension.
    pub lang: Option<String>,
    pub extension: Option<SourceExtension>,
}

/// Where a Suwayomi chapter lives at its source (`chapterOrigin`): what a reader app
/// that runs its own engine needs to find the SAME chapter there and fetch the pages
/// itself, instead of asking this server to fetch them. Ids of this server's engine are
/// deliberately absent — they mean nothing to another engine.
#[derive(SimpleObject, Clone)]
pub struct ChapterOrigin {
    pub source_id: String,
    /// The source's own key for the manga (`MangaType.url`).
    pub manga_url: String,
    /// Title as the source gives it — a search hint for an engine that has never seen
    /// the manga.
    pub manga_title: String,
    /// The source's own key for the chapter (`ChapterType.url`).
    pub chapter_url: String,
    /// The chapter's page on the source's own website (`ChapterType.realUrl`) — what the
    /// reader's "Read on source" opens. Null when the extension reports none. This is
    /// extension-controlled text, passed through verbatim: the client must accept only
    /// absolute http(s) before using it as a link.
    pub chapter_web_url: Option<String>,
    /// The series' page on the source's website (`MangaType.realUrl`); same caveats.
    pub manga_web_url: Option<String>,
    /// Null when the source has no catalogued extension yet.
    pub extension: Option<SourceExtension>,
}

/// A canonical work's source mappings, keyed by `work_id`, for the batch resolver.
/// A work with no (visible) sources yields an empty `sources` list.
#[derive(SimpleObject, Clone)]
pub struct WorkSourceGroup {
    pub work_id: ID,
    pub sources: Vec<WorkSource>,
}

/// Source provenance for one catalogue `Series` (admin console): the canonical
/// work the series is linked to and every `source_series` mapping on that work,
/// with each source's extension coordinates (`WorkSource.extension.pkgName` is
/// the "extension" column). `workId` is null — and `sources` empty — for a
/// series that hasn't been catalogued into a work yet.
#[derive(SimpleObject, Clone)]
pub struct SeriesSourceGroup {
    pub series_id: ID,
    pub work_id: Option<ID>,
    pub sources: Vec<WorkSource>,
}

#[derive(SimpleObject, Clone)]
pub struct DiscoveryFeed {
    pub kind: DiscoveryFeedKind,
    pub title: String,
    pub genre: Option<String>,
    #[graphql(complexity = "20 * child_complexity")]
    pub items: Vec<Series>,
}

#[derive(SimpleObject, Clone)]
pub struct UserRef {
    pub id: ID,
    pub username: String,
    pub avatar_url: Option<String>,
}

#[derive(SimpleObject, Clone)]
pub struct Review {
    pub id: ID,
    pub series_id: ID,
    pub author: UserRef,
    pub score: i32,
    pub body: String,
    pub has_spoiler: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// A comment on a polymorphic target: a chapter thread (`target_type = "chapter"`),
/// a series discussion (`target_type = "series"`) or a scanlator's thread
/// (`target_type = "scanlator"`, `target_id` = `RatedScanlator.id`). Comments form
/// arbitrary-depth reply trees via `parent_id` (NULL = a top-level comment) and may
/// carry one optional attached image (`media_url` + its pixel dimensions).
#[derive(SimpleObject, Clone)]
pub struct Comment {
    pub id: ID,
    pub target_type: String,
    pub target_id: ID,
    /// The comment this one replies to, or `null` for a top-level comment.
    pub parent_id: Option<ID>,
    pub author: UserRef,
    pub body: String,
    pub has_spoiler: bool,
    /// Null for live comments; "author" or "moderator" for a content-free tombstone.
    pub deleted_by: Option<String>,
    /// Path to the attached image (`/comment-media/<id>.webp`), if any.
    pub media_url: Option<String>,
    /// Attached image dimensions, so the client can reserve layout space.
    pub media_width: Option<i32>,
    pub media_height: Option<i32>,
    pub created_at: String,
    /// Like / dislike tallies for this comment, and the viewer's own vote (1 like,
    /// -1 dislike, 0 none). Populated by the `comments` query; 0 on a freshly posted one.
    pub likes: i32,
    pub dislikes: i32,
    pub my_vote: i32,
}

/// The result of `voteComment`: the comment's fresh tallies + the viewer's vote, so the
/// client updates that comment's counts without refetching the whole thread.
#[derive(SimpleObject, Clone, Copy)]
pub struct CommentVote {
    pub likes: i32,
    pub dislikes: i32,
    pub my_vote: i32,
}

/// One inbound notification for the viewer (the bell feed). `actor` is the user who
/// triggered it (a replier); null for an aggregate `like_milestone`. `commentExcerpt`
/// is a short snippet of the referenced comment — the REPLY's text for a `reply`, the
/// viewer's own comment for a `like_milestone`; `targetType`/`targetId` deep-link to
/// the thread.
#[derive(SimpleObject, Clone)]
pub struct Notification {
    pub id: ID,
    /// `'reply'` | `'like_milestone'`.
    pub kind: String,
    pub actor: Option<UserRef>,
    pub comment_id: Option<ID>,
    pub comment_excerpt: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<ID>,
    /// The owning series id for deep-linking a `chapter`-target notification to the
    /// reader (`/read/<seriesId>?ch=<targetId>`). Null for `series` targets — there
    /// `targetId` already IS the series — and when the chapter can't be resolved.
    pub series_id: Option<ID>,
    /// Milestone value for `like_milestone` (e.g. 10 = "reached 10 likes").
    pub count: Option<i32>,
    pub created_at: String,
    pub read: bool,
}

#[derive(SimpleObject, Clone)]
pub struct SessionUser {
    pub id: ID,
    pub username: String,
    /// Optional editable display name; falls back to `username` when unset.
    pub display_name: Option<String>,
    /// Optional editable "about me" text.
    pub bio: Option<String>,
    pub avatar_url: Option<String>,
    pub is_admin: bool,
    /// Whether this user has opted into seeing NSFW-flagged works (CATALOGUE.md §2).
    pub show_nsfw: bool,
    /// Account creation timestamp (ISO 8601) — the profile "joined" date.
    pub joined_at: String,
    /// Set when an admin created the account or reset its password; the client
    /// should ask for a new password (`changePassword`) before anything else.
    pub must_change_password: bool,
}

/// One entry in a user's activity feed (a review posted, a comment posted, a
/// series added to their library). Written by the corresponding mutation.
#[derive(SimpleObject, Clone)]
pub struct Activity {
    pub id: ID,
    /// `"review"` | `"comment"` | `"library_add"`.
    pub kind: String,
    /// `"series"` | `"chapter"` — the kind of thing acted on, when known.
    pub target_type: Option<String>,
    /// The series/chapter id the action targeted; the client resolves a title.
    pub target_id: Option<ID>,
    pub created_at: String,
}

#[derive(InputObject)]
pub struct UpdateProfileInput {
    /// New display name; `null` or blank clears it (falls back to username).
    pub display_name: Option<String>,
    /// New bio; `null` or blank clears it.
    pub bio: Option<String>,
}

#[derive(SimpleObject, Clone)]
pub struct Session {
    pub token: String,
    pub user: SessionUser,
}

/// Aggregate health of the background scan scheduler, for the admin console.
#[derive(SimpleObject, Clone)]
pub struct ScanStatus {
    /// Number of series in the Suwayomi library at the last tick.
    pub library_size: i32,
    /// Series found overdue (and re-scanned) at the last tick.
    pub overdue_count: i32,
    /// ISO 8601 timestamp of the last completed scan tick, if any.
    pub last_tick_at: Option<String>,
    /// Earliest upcoming `next_scan_at` across the library, if known.
    pub next_due_at: Option<String>,
    /// Series that scanned successfully on the last tick.
    pub scanned_ok: i32,
    /// Series whose scan errored (and were backed off) on the last tick.
    pub scanned_failed: i32,
    /// ISO 8601 timestamp of the last tick that made real progress, if any.
    pub last_success_at: Option<String>,
    /// Consecutive full batches that advanced nothing — a "stuck" signal (0 = healthy).
    pub stuck_ticks: i32,
}

/// A user as seen in the admin user-management console.
#[derive(SimpleObject, Clone)]
pub struct AdminUser {
    pub id: ID,
    pub username: String,
    pub email: String,
    pub avatar_url: Option<String>,
    pub is_admin: bool,
    pub is_banned: bool,
    pub created_at: String,
}

#[derive(SimpleObject, Clone)]
pub struct AdminUserPage {
    #[graphql(complexity = "50 * child_complexity")]
    pub items: Vec<AdminUser>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

#[derive(SimpleObject, Clone)]
pub struct SeriesPage {
    #[graphql(complexity = "30 * child_complexity")]
    pub items: Vec<Series>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

#[derive(SimpleObject, Clone)]
pub struct ReviewPage {
    #[graphql(complexity = "20 * child_complexity")]
    pub items: Vec<Review>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

#[derive(SimpleObject, Clone)]
pub struct CommentPage {
    #[graphql(complexity = "20 * child_complexity")]
    pub items: Vec<Comment>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

/// One of the viewer's OWN comments, as listed on their profile (`myComments`). Carries
/// what the list needs to render a row and deep-link back to the thread without a
/// per-row lookup: the owning series (resolved for a chapter target, the same way
/// notifications resolve it), that series' display title and the chapter number.
#[derive(SimpleObject, Clone)]
pub struct MyComment {
    pub id: ID,
    /// `"chapter"` | `"series"` | `"scanlator"`.
    pub target_type: String,
    pub target_id: ID,
    /// The comment this one replies to, or `null` for a top-level comment.
    pub parent_id: Option<ID>,
    pub body: String,
    pub has_spoiler: bool,
    pub created_at: String,
    pub likes: i32,
    pub dislikes: i32,
    /// Direct replies to this comment (banned authors excluded, as in the thread).
    pub reply_count: i32,
    /// The series the thread hangs off: `targetId` for a series discussion, the
    /// chapter's owning series for a chapter thread. Null when it can't be resolved,
    /// and for a scanlator's thread (`targetId` is then the scanlator's id).
    pub series_id: Option<ID>,
    /// That series' display title, when the catalogue knows it.
    pub series_title: Option<String>,
    /// Chapter number for a chapter thread (e.g. `"12"`, `"10.5"`), when known.
    pub chapter_number: Option<String>,
}

#[derive(SimpleObject, Clone)]
pub struct MyCommentPage {
    #[graphql(complexity = "20 * child_complexity")]
    pub items: Vec<MyComment>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

#[derive(InputObject)]
pub struct PostReviewInput {
    pub series_id: ID,
    pub score: i32,
    pub body: String,
    pub has_spoiler: bool,
}

#[derive(InputObject)]
pub struct PostCommentInput {
    /// `"chapter"`, `"series"` or `"scanlator"`. For a scanlator, `target_id` is a
    /// `RatedScanlator.id`.
    pub target_type: String,
    pub target_id: ID,
    /// The comment being replied to, or `null`/omitted for a top-level comment.
    /// Must belong to the same target, or the mutation is rejected.
    pub parent_id: Option<ID>,
    pub body: String,
    pub has_spoiler: bool,
    /// A previously-uploaded `comment_media` id (from `POST /comment-media`) to
    /// attach. Must be owned by the poster and not yet linked to another comment.
    pub media_id: Option<ID>,
}

#[derive(InputObject)]
pub struct RegisterInput {
    pub username: String,
    pub email: String,
    pub password: String,
}

/// Admin-created account (`createUser`).
#[derive(InputObject)]
pub struct CreateUserInput {
    pub username: String,
    pub email: String,
    /// Initial password, chosen by the admin.
    pub password: String,
    /// Grant admin on creation. Default false.
    pub is_admin: Option<bool>,
    /// Ask the user to pick their own password at first sign-in. Default true:
    /// the admin knows the initial one.
    pub must_change_password: Option<bool>,
}

/// Public facts about this server, readable without signing in. Drives the
/// first-run setup screen and the sign-up form.
#[derive(SimpleObject, Clone)]
pub struct Instance {
    /// True while no admin account exists; the owner account is then created
    /// with `createOwner` and the setup code from the server log.
    pub setup_required: bool,
    pub name: String,
    /// Server version (the `komika-server` crate version).
    pub version: String,
    /// Whether `register` accepts new accounts.
    pub registration_open: bool,
    /// Whether this server uses MangaDex (the usage switch; the catalogue crawl is a
    /// separate per-replica setting).
    pub mangadex_enabled: bool,
    /// Whether the API carries MangaDex's community rating next to our own readers'
    /// rating. Off: the reader shows our own rating only and hides its own switch.
    pub show_mangadex_ratings: bool,
    /// True when reader apps must fetch chapter pages from the sources themselves
    /// (`KOMIKA_APPS_FETCH_THEMSELVES`): an app with its own engine then does not offer
    /// fetching through this server.
    pub apps_fetch_themselves: bool,
    /// True when the server has no owner yet and THIS request came over plain HTTP from
    /// the internet: from a public address, or to a public address the client typed
    /// (the peer address proves nothing behind Docker Desktop or rootless Docker).
    /// `createOwner` is refused there (the owner password would travel in the clear):
    /// the setup screens show how to run `komika-server setup` on the server instead.
    pub setup_needs_terminal: bool,
}

/// The MangaDex switch as the admin console sees it (`mangadexSetting`).
#[derive(SimpleObject, Clone)]
pub struct MangadexSetting {
    /// Whether the server may talk to MangaDex right now.
    pub enabled: bool,
    /// True when the server environment fixes the value, so the console shows it
    /// read-only (`setMangadexEnabled` is refused).
    pub pinned: bool,
    /// The env variable that pins it: `KOMIKA_MANGADEX` or `CATALOGUE_SYNC`.
    pub pinned_by: Option<String>,
}

/// One per-source scan rate limit (`sourceRateLimits`): the server sends this source
/// at most `requests` HTTP requests per `window_secs` when it checks for new chapters.
#[derive(SimpleObject, Clone)]
pub struct SourceRateLimit {
    /// The Suwayomi source id (`sources { id }`).
    pub source_id: ID,
    pub requests: i32,
    pub window_secs: i32,
    /// True when the limit in force is the server's default for this source
    /// (`SCAN_SOURCE_RATE_LIMITS`), with no admin's limit over it.
    pub from_env: bool,
    /// True when an admin's limit replaces the server's default for this source.
    /// `removeSourceRateLimit` then restores the default instead of lifting the limit.
    pub overrides_env: bool,
}

/// Admin "manga DB" overrides. Every field is the desired whole state for the
/// series (a null override clears it), so the console sends the full form on save.
#[derive(InputObject)]
pub struct SeriesAdminInput {
    pub series_id: ID,
    pub override_interval_hours: Option<f64>,
    pub poll_every_minutes: Option<i32>,
    /// Force paused on/off; null lets the scanner auto-decide by status.
    pub paused: Option<bool>,
    /// Override the source status; null uses the source-derived status.
    pub status: Option<SeriesStatus>,
}

/// Admin edits to a canonical work's user-facing metadata (series-detail editor).
/// Each optional field uses `MaybeUndefined` three-valued semantics so a partial
/// edit is unambiguous: OMITTED => leave unchanged; NULL => clear the override
/// (fall back to the derived/source value); VALUE => set the override.
#[derive(InputObject)]
pub struct SeriesMetadataInput {
    /// A numeric Suwayomi series id or a `w_` canonical work id — both resolve to
    /// the underlying `work` the overrides are written to.
    pub series_id: ID,
    pub title: MaybeUndefined<String>,
    pub description: MaybeUndefined<String>,
    pub r#type: MaybeUndefined<ComicType>,
    pub is_nsfw: MaybeUndefined<bool>,
    /// The full curated tag/genre set (a whole-list replace). NULL clears the
    /// curated set so genres derive from the source again.
    pub tags: MaybeUndefined<Vec<String>>,
    /// The fields below are owned by the source. A VALUE pins the field, so later
    /// syncs keep it; NULL removes the pin and restores the source's value. An empty
    /// `author` / `artist` / `originalLanguage` pins the field empty.
    pub author: MaybeUndefined<String>,
    pub artist: MaybeUndefined<String>,
    /// First publication year.
    pub year: MaybeUndefined<i32>,
    /// Origin language code, e.g. `ja`, `ko`, `zh`, `zh-hk`, `en`.
    pub original_language: MaybeUndefined<String>,
    /// Publication status.
    pub status: MaybeUndefined<SeriesStatus>,
    /// `NONE` pins "no demographic".
    pub demographic: MaybeUndefined<Demographic>,
    pub content_rating: MaybeUndefined<ContentRating>,
}

/// The raw override state of a work, for the series-detail editor to show what is
/// pinned vs derived (parallel to how `ScanPolicy` exposes its raw overrides).
#[derive(SimpleObject, Clone)]
pub struct SeriesAdminMeta {
    /// The canonical work id the overrides live on (null if the series isn't
    /// catalogued yet — nothing can be pinned until it has a work).
    pub work_id: Option<ID>,
    pub title_override: Option<String>,
    pub description_override: Option<String>,
    pub content_type_override: Option<ComicType>,
    pub is_nsfw_override: Option<bool>,
    /// The effective genre list (curated set if any, else derived from the source).
    pub tags: Vec<String>,
    /// Whether `tags` is an admin-curated set (true) or derived from the source (false).
    pub has_curated_tags: bool,
    /// The work's current source-owned fields, pinned or not.
    pub author: Option<String>,
    pub artist: Option<String>,
    pub year: Option<i32>,
    pub original_language: Option<String>,
    pub status: Option<SeriesStatus>,
    pub demographic: Option<Demographic>,
    pub content_rating: Option<ContentRating>,
    /// Which of those fields an admin has pinned, by their name in this type
    /// (`author`, `originalLanguage`, …).
    pub pinned_fields: Vec<String>,
}

/// One chapter of a canonical work in the admin editor: its aggregate identity plus
/// the override state (hidden / renamed) so the console can show and toggle both.
#[derive(SimpleObject, Clone)]
pub struct AdminChapter {
    pub number: f64,
    /// The aggregate bucket key (`round(number*100)` as text) — the override key.
    pub key: String,
    pub source_title: Option<String>,
    pub title_override: Option<String>,
    pub effective_title: Option<String>,
    pub hidden: bool,
    /// How many sources provide this chapter number.
    pub source_count: i32,
}

/// Admin edit to one chapter of a work (soft-hide / rename). `MaybeUndefined` so a
/// toggle of one field doesn't clobber the other: OMITTED => unchanged.
#[derive(InputObject)]
pub struct ChapterOverrideInput {
    pub work_id: ID,
    /// The aggregate bucket key from `AdminChapter.key`.
    pub chapter_key: String,
    pub hidden: MaybeUndefined<bool>,
    pub title: MaybeUndefined<String>,
}

// ---- Sources & Extensions admin surface (EXT-1) ------------------------------

/// Which listing of a source to browse (mirrors Suwayomi's `FetchSourceMangaType`).
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum SourceBrowseType {
    Popular,
    Latest,
    Search,
}

impl From<SourceBrowseType> for crate::suwayomi::FetchType {
    fn from(t: SourceBrowseType) -> Self {
        match t {
            SourceBrowseType::Popular => crate::suwayomi::FetchType::Popular,
            SourceBrowseType::Latest => crate::suwayomi::FetchType::Latest,
            SourceBrowseType::Search => crate::suwayomi::FetchType::Search,
        }
    }
}

/// One Keiyoushi/Mihon extension as known to the Suwayomi engine — the admin
/// management view (installed or not). Mirrors `suwayomi::ExtensionListEntry`.
#[derive(SimpleObject, Clone)]
pub struct ExtensionInfo {
    pub pkg_name: String,
    pub name: String,
    pub lang: String,
    pub version_name: String,
    pub is_installed: bool,
    pub has_update: bool,
    pub is_nsfw: bool,
    pub icon_url: Option<String>,
    /// The store/repo the extension came from, when reported.
    pub repo: Option<String>,
    /// Whether this extension is subscribed for background source-sync (auto-discovery
    /// of new series). Set by the resolver from `extension_subscription`; the `From`
    /// impl defaults it to false.
    pub subscribed: bool,
}

impl From<crate::suwayomi::ExtensionListEntry> for ExtensionInfo {
    fn from(e: crate::suwayomi::ExtensionListEntry) -> Self {
        ExtensionInfo {
            pkg_name: e.pkg_name,
            name: e.name,
            lang: e.lang,
            version_name: e.version_name,
            is_installed: e.is_installed,
            has_update: e.has_update,
            is_nsfw: e.is_nsfw,
            icon_url: e.icon_url,
            repo: e.repo,
            subscribed: false,
        }
    }
}

/// One configured extension repo (an engine "extension store"). Mirrors
/// `suwayomi::ExtensionStore`.
#[derive(SimpleObject, Clone)]
pub struct ExtensionRepo {
    /// The repo's canonical index URL: its identity, what `removeExtensionRepo` takes,
    /// and what each of its extensions reports as `repo`.
    pub index_url: String,
    pub name: String,
    /// Short label (e.g. `KEI`).
    pub badge_label: String,
}

impl From<crate::suwayomi::ExtensionStore> for ExtensionRepo {
    fn from(s: crate::suwayomi::ExtensionStore) -> Self {
        ExtensionRepo {
            index_url: s.index_url,
            name: s.name,
            badge_label: s.badge_label,
        }
    }
}

/// One source's scan health (Phase E4.3) — the admin answer to "is this source actually
/// working?", which before E4 had no honest signal at all: a broken source's scans recorded
/// as successes because the client silently fell back to Suwayomi's cache (F11).
///
/// Per SOURCE rather than per series, because that is the unit a breakage has: an extension
/// whose site moved or rebranded breaks everything it carries at once.
#[derive(SimpleObject, Clone)]
pub struct SourceScanHealth {
    /// Suwayomi source id.
    pub id: ID,
    /// Display name, when the source is still installed in the engine.
    pub name: Option<String>,
    /// Owning extension package. `None` once the extension is uninstalled — its series and
    /// their scan state outlive it, which is a state worth seeing rather than hiding.
    pub pkg_name: Option<String>,
    /// Whether the extension is still installed in the Suwayomi engine.
    pub installed: bool,
    /// Whether we subscribe to its discovery walk, and whether that subscription's breaker
    /// has been tripped.
    pub subscribed: bool,
    pub subscription_disabled_at: Option<String>,
    /// Series of this source that the scanner tracks.
    pub series: i32,
    /// …with any current failure streak.
    pub failing: i32,
    /// …whose streak is long enough, for a source-side reason, to count toward an outage.
    pub confirmed_failing: i32,
    /// Of the failures, how many last failed by being served Suwayomi's CACHE (broken while
    /// looking healthy) versus failing out loud.
    pub cached_fallback: i32,
    pub fetch_error: i32,
    /// Series the scanner has never got a chapter for. Across a whole source, the signature
    /// of one that has never once worked.
    pub zero_chapter_series: i32,
    /// Works that would become unreadable if this source stays broken — i.e. reachable
    /// through no other source.
    pub exclusive_works: i32,
    pub worst_streak: i32,
    pub last_failure_at: Option<String>,
    pub last_scanned_at: Option<String>,
    /// Set while a whole-source outage is open: when it was detected, when it last alerted,
    /// the dominant failure kind, and how far out the source's series are parked.
    pub outage_detected_at: Option<String>,
    pub outage_last_alert_at: Option<String>,
    pub outage_kind: Option<String>,
    pub outage_parked_until: Option<String>,
    /// What every upstream request to this source did over the requested window, one
    /// entry per (surface, reason). Unlike the scan counts above this includes the
    /// requests READERS make, and says why the failures failed.
    pub activity: Vec<SourceHealthStat>,
}

/// One (surface, reason) total for a source over a window (migration 0110).
#[derive(SimpleObject, Clone)]
pub struct SourceHealthStat {
    /// 'chapters' (chapter-list fetch), 'pages' (a reader opening a chapter), 'image'
    /// (one page image), or 'browse' (a source listing).
    pub surface: String,
    /// 'ok', or why it failed: 'blocked', 'rate_limited', 'solver', 'not_found',
    /// 'timeout', 'network', 'server_error', 'extension', 'engine', 'other'.
    pub reason: String,
    pub count: i32,
    /// The newest error text for this reason; null for 'ok'.
    pub last_message: Option<String>,
    pub last_at: String,
}

/// The extension updater's status (`extensionAutoUpdate`).
#[derive(SimpleObject, Clone)]
pub struct ExtensionAutoUpdate {
    /// False when `EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS=0`.
    pub enabled: bool,
    pub interval_seconds: i32,
    /// A pass is in flight right now.
    pub running: bool,
    /// When the last pass finished; null if none has run since the server started.
    pub last_run_at: Option<String>,
    /// Why the last pass failed as a whole; null when it ran.
    pub last_error: Option<String>,
    /// Extensions the last pass updated, and extensions it tried and could not.
    pub last_updated: i32,
    pub last_failed: i32,
    /// Per-extension history, newest first.
    pub recent: Vec<ExtensionUpdateEvent>,
}

/// One extension an update pass touched.
#[derive(SimpleObject, Clone)]
pub struct ExtensionUpdateEvent {
    pub at: String,
    pub pkg_name: String,
    pub from_version: Option<String>,
    /// Null when the update failed.
    pub to_version: Option<String>,
    /// 'auto' (the background pass) or 'manual' (an admin asked for one).
    pub origin: String,
    /// Null when the update succeeded.
    pub error: Option<String>,
}

/// One installed Suwayomi source — the admin picker feeding `sourceBrowse(sourceId)`.
#[derive(SimpleObject, Clone)]
pub struct SourceInfo {
    /// The Suwayomi source id (the `sourceBrowse` input).
    pub id: ID,
    /// User-facing display name (per-language variants, e.g. "MangaDex (EN)").
    pub name: String,
    pub lang: String,
    pub is_nsfw: bool,
    pub icon_url: Option<String>,
    /// The owning extension's pkgName, when reported.
    pub pkg_name: Option<String>,
}

/// One manga returned by browsing a source (admin bulk-ingest picker). The id is
/// Suwayomi's internal manga id — exactly what `bulkAddSourceSeries` consumes.
#[derive(SimpleObject, Clone)]
pub struct SourceBrowseEntry {
    pub suwayomi_manga_id: ID,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub in_library: bool,
}

/// A page of source-browse results.
#[derive(SimpleObject)]
pub struct SourceBrowsePage {
    #[graphql(complexity = "30 * child_complexity")]
    pub items: Vec<SourceBrowseEntry>,
    pub page: i32,
    pub has_next_page: bool,
}

/// One "add all from source" background ingest job (S1). Mirrors
/// `ingest::IngestJob` / the `source_ingest_job` row.
#[derive(SimpleObject, Clone)]
pub struct SourceIngestJob {
    pub id: ID,
    pub source_id: String,
    /// `running` | `completed` | `cancelled` | `failed`.
    pub state: String,
    pub pages_done: i32,
    pub items_seen: i32,
    pub succeeded: i32,
    pub failed: i32,
    pub new_works: i32,
    pub auto_merged: i32,
    pub queued_for_review: i32,
    pub already_existing: i32,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

impl From<crate::ingest::IngestJob> for SourceIngestJob {
    fn from(j: crate::ingest::IngestJob) -> Self {
        SourceIngestJob {
            id: ID(j.id),
            source_id: j.source_id,
            state: j.state,
            pages_done: j.pages_done as i32,
            items_seen: j.items_seen as i32,
            succeeded: j.succeeded as i32,
            failed: j.failed as i32,
            new_works: j.new_works as i32,
            auto_merged: j.auto_merged as i32,
            queued_for_review: j.queued_for_review as i32,
            already_existing: j.already_existing as i32,
            error: j.error,
            started_at: j.started_at,
            finished_at: j.finished_at,
        }
    }
}

/// Per-id outcome of `bulkAddSourceSeries`: the dedup `MatchResult` on success,
/// or the error that made this one id fail (other ids still proceed).
#[derive(SimpleObject)]
pub struct BulkAddEntry {
    pub suwayomi_manga_id: ID,
    pub result: Option<super::MatchResult>,
    pub error: Option<String>,
}

/// Result of a bulk catalogue ingest: per-id entries plus a decision summary.
#[derive(SimpleObject)]
pub struct BulkAddResult {
    pub entries: Vec<BulkAddEntry>,
    pub total: i32,
    pub succeeded: i32,
    pub failed: i32,
    /// Decision counts across the succeeded entries.
    pub new_works: i32,
    pub auto_merged: i32,
    pub queued_for_review: i32,
    pub already_existing: i32,
}

// ---- Federated multi-extension search + translators (S3) ---------------------

/// One source/extension that carries a canonical work — a "translator" in the
/// reader UI (S3). For a Suwayomi source, `suwayomiMangaId` (= the source
/// mapping's `source_key`) is exactly the id the reader passes to
/// `chapters(seriesId:)` to fetch THIS translator's chapters; for the MangaDex
/// spine (`sourceType = "mangadex"`), chapters come from
/// `canonicalChapters(workId)` instead and `suwayomiMangaId` is null.
#[derive(SimpleObject, Clone)]
pub struct Translator {
    /// `"mangadex"` (the canonical spine) or `"suwayomi"` (an installed extension).
    pub source_type: String,
    pub source_id: String,
    /// The source's display name (e.g. "MangaDex (EN)", "Manga Plus"); null when
    /// the source isn't currently installed on the engine.
    pub source_name: Option<String>,
    pub lang: Option<String>,
    /// The Suwayomi manga id to fetch this translator's chapters with
    /// (`chapters(seriesId:)`). Null for the MangaDex spine mapping.
    pub suwayomi_manga_id: Option<ID>,
    pub extension_pkg_name: Option<String>,
    pub extension_icon_url: Option<String>,
}

/// One consolidated federated-search hit: a canonical work as a `Series`, plus
/// the per-source translator list gathered across every installed extension (S3).
#[derive(SimpleObject, Clone)]
pub struct FederatedSeries {
    pub series: Series,
    pub translators: Vec<Translator>,
}

/// A page of federated search results (S3).
#[derive(SimpleObject)]
pub struct FederatedSearchPage {
    #[graphql(complexity = "30 * child_complexity")]
    pub items: Vec<FederatedSeries>,
    pub page: i32,
    pub has_next_page: bool,
    /// How many installed sources were actually queried in the fan-out (diagnostic).
    pub sources_queried: i32,
}

/// One genre/tag facet for the search filter UI (S4): a genre name and how many
/// cached series carry it. The full set the sources provide, not a hardcoded list.
#[derive(SimpleObject, Clone)]
pub struct GenreFacet {
    pub genre: String,
    pub count: i32,
}

/// One COPY of a given chapter number of a work (S2 aggregation): one upload, by one
/// scanlation group, on one source. A number MangaDex carries from three groups has three
/// of these, each with its own `chapterId` — nothing is collapsed per source, which is what
/// lets the reader offer every group's copy by name the way MangaDex does.
///
/// `suwayomiMangaId` (for a Suwayomi source) is the id the reader passes to
/// `chapters(seriesId:)` to read this source's copy; `chapterId` is the specific
/// chapter to open (a Suwayomi chapter id, or a MangaDex chapter uuid).
#[derive(SimpleObject, Clone)]
pub struct ChapterSource {
    pub source_type: String,
    pub source_id: String,
    pub suwayomi_manga_id: Option<ID>,
    pub chapter_id: ID,
    /// Attribution for THIS copy. For MangaDex: group names joined with `" & "`, else
    /// the uploader username (or `"No Group"` if unavailable). `null` means unknown — a row the
    /// group backfill has not reached yet, or a Suwayomi source that reports no scanlator.
    pub scanlator: Option<String>,
    /// When THIS copy was released (ISO-8601) — `COALESCE(readable_at, published_at)`, the
    /// same clock as `AggregatedChapter.firstReleasedAt`, which is the MIN of these.
    ///
    /// NAMED `copyReleasedAt`, NOT `releasedAt`, for the reason `firstReleasedAt` gives: the
    /// client strips opportunistic fields by NAME across every document, and
    /// `UpdateFeedRow.releasedAt` must never be collateral damage.
    pub copy_released_at: Option<String>,
    /// Set when THIS copy is hosted off-site (MangaPlus, Comikey…), so a reader opening a
    /// non-default copy hands off instead of requesting pages that do not exist. Per copy,
    /// because one group's upload can be a redirect while another's is pages. Named
    /// `copyExternalUrl` so the name-based stripper never touches `Chapter.externalUrl`.
    pub copy_external_url: Option<String>,
}

/// One aggregated chapter of a work — a chapter NUMBER available across one or
/// more sources (S2). Deduped by number; `sources` keeps per-source availability
/// so the reader can pick a translator.
#[derive(SimpleObject, Clone)]
pub struct AggregatedChapter {
    /// The chapter's number, or `0` when it has none. **Prefer [`Self::label`]** — this
    /// stays non-null only so a client built before Phase A keeps working; a oneshot
    /// reports 0 here and "Oneshot" there.
    pub number: f64,
    /// What to print: "45", "10.5", "Oneshot". The one label rule, resolved server-side so
    /// every surface agrees (Phase A2).
    pub label: String,
    /// Cross-source grouping key: `round(number * 100)` for numbered chapters — the same
    /// key `chapter_override` matches on — or `x:<chapter id>` for unnumbered ones.
    pub key: String,
    pub title: Option<String>,
    /// When this chapter was FIRST released, across every source that carries it —
    /// ISO-8601, or null when no source dated it (F12).
    ///
    /// `first_released_at`, not `released_at`, and the name is load-bearing on the client:
    /// the reader strips unknown fields BY NAME across every document, and `UpdateFeedRow`
    /// already has a `releasedAt` that every deployed server answers. Sharing the name would
    /// let one older server rejecting this field silently disable the updates feed's date
    /// too. It is also the more accurate name.
    ///
    /// The EARLIEST of the sources' own release times, which is the same rule the release
    /// ledger stores (`release_event.first_seen_at`, first-source-wins) and therefore the
    /// same instant `/updates` sorted the work by. Taking the SELECTED source's time instead
    /// would make the row's date jump around as the reader switches translator, for a
    /// chapter that was released once.
    ///
    /// Why this field exists at all: without it the reader has no honest date for a chapter
    /// the selected translator does not carry, and `source.ts` rendered those rows with no
    /// date line — §4.13's "a wrong default traded for a date-less chapter list", which is
    /// exactly what F12's source picker was told to ship WITH, not before.
    pub first_released_at: Option<String>,
    /// The volume this chapter belongs to ("3", "12.5"), or null when no copy says — the
    /// series page's volume headers. A MangaDex copy's label wins over any other source's
    /// (Suwayomi exposes none today; the rule keeps a future one from overriding the
    /// mirror). Named `chapterVolume`, not `volume`, for the same reason as
    /// `firstReleasedAt`: the reader strips an unknown field BY NAME across every document,
    /// and `Cover.volume` is selected on the series page.
    pub chapter_volume: Option<String>,
    /// Has the VIEWER read this chapter — from ANY of the work's sources?
    ///
    /// Read state is stored per source chapter row (`canonical_progress.chapter_id` is a
    /// MangaDex uuid, `suwayomi_progress.chapter_id` a numeric Suwayomi id), so the two
    /// sources of one chapter share no key and switching translator used to blank every
    /// marker the reader had earned. This flag is DERIVED, not stored: it is set when the
    /// viewer has a `read = 1` row on any source chapter carrying this row's spine
    /// `key` (`chapter.chapter_key`), which is the identity that already folds a chapter
    /// across sources (migration 0090).
    ///
    /// Matching is pure key equality, scoped to the ONE work. Its residual risk is a source
    /// that numbers by volume rather than absolutely — its "chapter 3" is a different
    /// chapter from the other source's, and it would be marked read. No query can prove a
    /// source is volume-numbered, so what was measured is a PROXY for it: a source pair
    /// whose chapter counts agree within 25% while their highest numbers differ more than
    /// threefold. That found 3 works, against 1,335 works with chapters on more than one
    /// source and 53 whose sources share no key at all (equality there simply matches
    /// nothing, which is the right answer). Counted on a snapshot of production taken
    /// 2026-08-12, per `source_series`, over `chapter` rows (`source_type = 'suwayomi' OR
    /// lang = 'en'`), with `n = COUNT(DISTINCT chapter_key)` and `mx = MAX(CAST(chapter_key
    /// AS INTEGER)) / 100.0` for numeric keys:
    ///
    /// ```sql
    /// SELECT COUNT(*) FROM (
    ///   SELECT a.w FROM ss_stat a JOIN ss_stat b ON a.w = b.w AND a.sid < b.sid
    ///    WHERE a.n >= 10 AND b.n >= 10 AND a.mx > 0 AND b.mx > 0
    ///      AND CAST(a.n AS REAL) / b.n BETWEEN 0.8 AND 1.25
    ///      AND (a.mx / b.mx > 3 OR b.mx / a.mx > 3)
    ///    GROUP BY a.w)
    /// ```
    ///
    /// An overlap-ratio guard over that same shape was considered and rejected: it
    /// suppresses legitimate matches silently.
    ///
    /// False only ever means "not known to be read": anonymous viewers, and chapters whose
    /// key is source-local by construction (`x:<id>` — oneshots and named extras can never
    /// be matched across sources) always report false.
    pub read_anywhere: bool,
    pub sources: Vec<ChapterSource>,
}

/// Result of folding one canonical work into another (D1 admin merge).
#[derive(SimpleObject, Clone)]
pub struct MergeWorksResult {
    /// The surviving (target) work id.
    pub target_work_id: ID,
    /// How many `source_series` mappings were re-pointed onto the target.
    pub moved_source_series: i32,
}

/// One detachable source mapping on a work — the unit the admin SPLIT picker
/// (`workSourceRows`) operates on.
///
/// DISTINCT FROM `WorkSource`, which describes how a NATIVE client fetches a source
/// (extension package, repo, apk). This is the admin view of the same `source_series`
/// row: what it calls the series, how much of it we hold, and — the field `WorkSource`
/// has none of — the row's own primary key.
#[derive(SimpleObject, Clone)]
pub struct WorkSourceRow {
    /// `source_series.id`, and it is what `splitSourceSeries` takes. NOT
    /// `(sourceType, sourceKey)`: that pair is unique only in combination with
    /// `sourceId` (the table's `UNIQUE (source_type, source_id, source_key)`), so it is
    /// ambiguous the moment one work carries the same manga id under two Suwayomi
    /// sources.
    pub id: ID,
    /// `"mangadex"` or `"suwayomi"`.
    pub source_type: String,
    /// The source's display name, e.g. "MangaDex", "MANGA Plus". Never null — falls back
    /// through the extension package name to the raw source id, because the picker leads
    /// each row with it.
    pub source_name: String,
    /// Language code (`en`), or null when the source declares none.
    pub lang: Option<String>,
    /// Extension logo, preferring the store-hosted URL served from our own origin. Null
    /// for a source with no derivable icon; the UI renders an initial instead.
    pub icon_url: Option<String>,
    /// How THIS source titles the series — the disagreement that identifies a mis-merge,
    /// and the name the detached work takes. A MangaDex mapping has no per-source title
    /// on disk (the mirror's title IS the work's), so it reports the work's.
    pub title: String,
    /// Chapters held from this source; they all move with it.
    pub chapter_count: i32,
    /// Canonical URL for the series at the source, when the mapping recorded one.
    pub source_url: Option<String>,
}

/// Result of detaching source mappings off a work onto a new one (`splitSourceSeries`).
///
/// NOT an undo of `MergeWorksResult` and it cannot be one: `mergeWorks` deletes the
/// losing work row and keeps no record of what it folded. This mints a NEW work carrying
/// the detached sources and — for free, since `chapter` is keyed by `source_series_id`
/// rather than by work — every chapter they own. Reviews, library entries, reading
/// progress and view counts stay with the ORIGINAL work, which is where they already
/// were: nothing records which side of the split they came from.
#[derive(SimpleObject, Clone)]
pub struct SplitSourcesResult {
    /// The freshly minted canonical work id (`w_…`).
    pub new_work_id: ID,
    /// Where to NAVIGATE for it — the reader id, which is the numeric Suwayomi id unless
    /// the detached set includes a MangaDex source. `canonicalSeries` rejects a work with
    /// no MangaDex anchor outright, so a Suwayomi-only split must never be linked as
    /// `/series/w_…`.
    pub new_reader_id: ID,
    /// The new work's `primary_title`, taken from the detached source's own title.
    pub title: String,
    /// How many `source_series` rows moved.
    pub moved_sources: i32,
}

/// Serialize a `SeriesStatus` to its SDL name (for storage).
pub fn status_word(s: SeriesStatus) -> &'static str {
    match s {
        SeriesStatus::Ongoing => "ONGOING",
        SeriesStatus::Completed => "COMPLETED",
        SeriesStatus::Hiatus => "HIATUS",
        SeriesStatus::Cancelled => "CANCELLED",
        SeriesStatus::Unknown => "UNKNOWN",
    }
}

// ---- mapping helpers -------------------------------------------------------

/// Map a Suwayomi status string onto the Komika `SeriesStatus`.
pub fn status_from(s: &str) -> SeriesStatus {
    match s {
        "ONGOING" => SeriesStatus::Ongoing,
        "COMPLETED" | "PUBLISHING_FINISHED" | "LICENSED" => SeriesStatus::Completed,
        "CANCELLED" => SeriesStatus::Cancelled,
        "ON_HIATUS" => SeriesStatus::Hiatus,
        _ => SeriesStatus::Unknown,
    }
}

/// The scanner auto-pauses for these (effective) Komika statuses.
pub fn paused_for_status(status: SeriesStatus) -> bool {
    matches!(
        status,
        SeriesStatus::Completed | SeriesStatus::Hiatus | SeriesStatus::Cancelled
    )
}

/// Parse a stored Komika status-override string back into the enum.
pub fn komika_status(s: &str) -> Option<SeriesStatus> {
    match s {
        "ONGOING" => Some(SeriesStatus::Ongoing),
        "COMPLETED" => Some(SeriesStatus::Completed),
        "HIATUS" => Some(SeriesStatus::Hiatus),
        "CANCELLED" => Some(SeriesStatus::Cancelled),
        "UNKNOWN" => Some(SeriesStatus::Unknown),
        _ => None,
    }
}

/// The stored/serialized word for a comic type (matches the GraphQL enum + the
/// `content_type_override` column values).
pub fn content_type_word(t: ComicType) -> &'static str {
    match t {
        ComicType::Manga => "MANGA",
        ComicType::Manhwa => "MANHWA",
        ComicType::Manhua => "MANHUA",
        ComicType::Webtoon => "WEBTOON",
        ComicType::Comic => "COMIC",
    }
}

/// Parse a stored comic-type word (`content_type_override`) back to the enum.
/// Case-insensitive; `None` for an unrecognized value.
pub fn comic_type_from_word(s: &str) -> Option<ComicType> {
    match s.trim().to_uppercase().as_str() {
        "MANGA" => Some(ComicType::Manga),
        "MANHWA" => Some(ComicType::Manhwa),
        "MANHUA" => Some(ComicType::Manhua),
        "WEBTOON" => Some(ComicType::Webtoon),
        "COMIC" => Some(ComicType::Comic),
        _ => None,
    }
}

fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
}

fn is_kana(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}')
}

fn is_han(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}')
}

/// Resolve the effective comic type, in precedence order:
/// 1. explicit admin override (`content_type_override`),
/// 2. the original language (ko→Manhwa, zh→Manhua, ja→Manga),
/// 3. a genre/tag heuristic (an explicit `Manhwa`/`Manhua` tag),
/// 4. a title-script heuristic (Hangul→Manhwa; Han without kana→Manhua),
/// 5. default Manga.
///
/// Steps 3–5 only fire when the origin language is unknown — the common case for
/// series ingested from a Suwayomi/Keiyoushi extension, whose source language is
/// the TRANSLATION language and must not be treated as the origin.
pub fn resolve_comic_type(
    override_word: Option<&str>,
    original_language: Option<&str>,
    genres: &[String],
    title: &str,
) -> ComicType {
    if let Some(t) = override_word.and_then(comic_type_from_word) {
        return t;
    }
    match original_language.map(|l| l.to_lowercase()) {
        Some(l) if l.starts_with("ko") => return ComicType::Manhwa,
        Some(l) if l.starts_with("zh") => return ComicType::Manhua,
        Some(l) if l.starts_with("ja") => return ComicType::Manga,
        _ => {}
    }
    // Explicit origin tags win (format + origin-country tags — some sources carry a
    // "Korean"/"Chinese"/"Japanese" origin tag, the most precise signal after language).
    for g in genres {
        let g = g.to_lowercase();
        if g.contains("manhwa") || g.contains("korean") {
            return ComicType::Manhwa;
        }
        if g.contains("manhua") || g.contains("chinese") {
            return ComicType::Manhua;
        }
        if g.contains("japanese") {
            return ComicType::Manga;
        }
    }
    // Webtoon-format tags (e.g. "Long Strip", "Webtoon", "Web Comic") — these
    // catalogues are Korean-webtoon-dominant and the reader collapses WEBTOON→Manhwa,
    // so a long-strip series with no explicit manhua tag and unknown origin language
    // (the common Solo-Leveling shape: en source, no MangaDex anchor) reads as Manhwa
    // rather than the Manga default. A genuine Chinese webtoon is corrected by its
    // explicit "Manhua" tag above, by MangaDex enrichment, or by an admin override.
    for g in genres {
        let g = g.to_lowercase();
        if g.contains("webtoon") || g.contains("long strip") || g.contains("web comic") {
            return ComicType::Manhwa;
        }
    }
    if title.chars().any(is_hangul) {
        return ComicType::Manhwa;
    }
    if title.chars().any(is_han) && !title.chars().any(is_kana) {
        return ComicType::Manhua;
    }
    ComicType::Manga
}

/// Coerce a Suwayomi epoch timestamp (seconds or millis, as a string) to ISO 8601.
pub fn to_iso(v: Option<&str>) -> Option<String> {
    crate::catalog::suwayomi_upload_date_to_iso(v)
        .and_then(|s| crate::db_types::date(&s).ok())
        .map(crate::db_types::format_date)
}

/// Map a Suwayomi chapter onto the Komika `Chapter`. Read state is per-VIEWER
/// (`suwayomi_progress`), passed in as `progress`; the cached `is_read`/
/// `last_page_read` on the shared `suwayomi_chapter` row are ignored — they were
/// global state. Anonymous / never-read chapters default to unread (mirrors the
/// canonical `map_canonical_chapter` path).
pub fn map_chapter(c: SuwayomiChapter, progress: Option<(i32, bool)>) -> Chapter {
    let (last_page_read, read) = progress.unwrap_or((0, false));
    // Same label rule as every other surface (Phase A2): the structured number when it is
    // sane, the name's first number as the ~0.15% fallback, "Oneshot" when there is none —
    // and never `Ch.99999999` from a TEST upload.
    let label = crate::chapter_label::chapter_display(Some(c.chapter_number), None, Some(&c.name));
    Chapter {
        id: ID(c.id.to_string()),
        series_id: ID(c.manga_id.to_string()),
        number: label.number().unwrap_or(c.chapter_number),
        label: label.text(),
        // Suwayomi chapters are always served through the engine, never a redirect.
        external_url: None,
        title: Some(c.name),
        page_count: c.page_count as i32,
        uploaded_at: to_iso(c.upload_date.as_deref()),
        // Filled in by the `chapters` resolver, which already resolves the owning work and
        // this row's spine key in order to fold sibling-source read state. Left `None` here
        // so this mapper stays a pure `SuwayomiChapter` -> `Chapter` projection with no
        // database of its own.
        found_at: None,
        scanlator: c.scanlator,
        read,
        last_page_read,
        bookmarked: c.is_bookmarked,
        is_downloaded: c.is_downloaded,
    }
}

#[cfg(test)]
mod comic_type_tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn admin_override_wins_over_everything() {
        // Even a Japanese original language + kana title is Manhua if pinned.
        assert_eq!(
            resolve_comic_type(Some("MANHUA"), Some("ja"), &s(&["Action"]), "テスト"),
            ComicType::Manhua
        );
    }

    #[test]
    fn original_language_maps_ko_zh_ja() {
        assert_eq!(
            resolve_comic_type(None, Some("ko"), &[], ""),
            ComicType::Manhwa
        );
        assert_eq!(
            resolve_comic_type(None, Some("zh-hk"), &[], ""),
            ComicType::Manhua
        );
        assert_eq!(
            resolve_comic_type(None, Some("ja"), &[], ""),
            ComicType::Manga
        );
    }

    #[test]
    fn genre_heuristic_fires_when_language_unknown() {
        // The Solo-Leveling case: served by an English source (no origin language),
        // but the source tags it "Manhwa" → classified Manhwa, not the Manga default.
        assert_eq!(
            resolve_comic_type(None, None, &s(&["Action", "Manhwa"]), "Solo Leveling"),
            ComicType::Manhwa
        );
        assert_eq!(
            resolve_comic_type(None, Some("en"), &s(&["Manhua"]), "Tales"),
            ComicType::Manhua
        );
    }

    #[test]
    fn webtoon_format_tags_classify_as_manhwa() {
        // The real Solo Leveling shape: English source, no origin language, romanized
        // title, and webtoon-format tags but NO explicit "Manhwa" tag.
        let solo = s(&[
            "Adaptation",
            "Award Winning",
            "Full Color",
            "Long Strip",
            "Web Comic",
            "Action",
            "Adventure",
            "Fantasy",
        ]);
        assert_eq!(
            resolve_comic_type(None, None, &solo, "Solo Leveling"),
            ComicType::Manhwa
        );
        // An explicit Manhua tag still wins over the webtoon-format fallback.
        assert_eq!(
            resolve_comic_type(None, None, &s(&["Long Strip", "Manhua"]), "Battle Through"),
            ComicType::Manhua
        );
    }

    #[test]
    fn script_heuristic_detects_hangul_and_han() {
        assert_eq!(
            resolve_comic_type(None, None, &[], "나 혼자만 레벨업"),
            ComicType::Manhwa
        );
        assert_eq!(
            resolve_comic_type(None, None, &[], "斗破苍穹"),
            ComicType::Manhua
        );
        // Han + kana => Japanese, not Manhua.
        assert_eq!(
            resolve_comic_type(None, None, &[], "鬼滅の刃"),
            ComicType::Manga
        );
    }

    #[test]
    fn defaults_to_manga_with_no_signal() {
        assert_eq!(
            resolve_comic_type(None, None, &[], "Some Title"),
            ComicType::Manga
        );
        assert_eq!(
            resolve_comic_type(None, Some("en"), &[], "Some Title"),
            ComicType::Manga
        );
    }

    #[test]
    fn word_round_trips() {
        for t in [
            ComicType::Manga,
            ComicType::Manhwa,
            ComicType::Manhua,
            ComicType::Webtoon,
            ComicType::Comic,
        ] {
            assert_eq!(comic_type_from_word(content_type_word(t)), Some(t));
        }
        assert_eq!(comic_type_from_word("nonsense"), None);
    }
}
