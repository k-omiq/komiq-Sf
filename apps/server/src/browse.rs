//! Catalogue-wide Browse: the filtered, sorted, paged read behind `search(query: "")`.
//!
//! WHY THIS IS ITS OWN MODULE. Browse used to be `series_cache::search_catalogue`, and it
//! read `suwayomi_series WHERE in_library = 1` — 13,847 rows of the 115,567 browsable
//! canonical works, keyed by numeric Suwayomi id, one fixed ORDER BY, and a genre filter
//! that `LIKE`-scanned a JSON blob. That belonged in `series_cache` because it really was
//! a query over the Suwayomi source cache. This is not: it reads `browse_catalogue`
//! (migration 0069), `work_tag`, `reviews` and `series_view_bucket`, none of which
//! are Suwayomi-cache tables, and it returns canonical works. `series_cache`'s module doc
//! is "DB cache for Suwayomi source-series metadata + chapter lists"; keeping this here
//! keeps that true. The COUNT memo moved with it — `search_catalogue` was its only other
//! user and is deleted.
//!
//! WHY `browse_catalogue` AND NOT `feed_series_updates`. It paged the feed until migration
//! 0069, which is 48,567 works — the ones with a chapter we can date. That excluded 67,000
//! browsable works, and NOT the obscure tail: MangaDex removes chapters when a series is
//! licensed or claimed, so "no chapters upstream" correlates with POPULAR (`Boku no Hero
//! Academia`, `Nausicaä of the Valley of the Wind`, `Houseki no Kuni`), and 3,239 of those
//! works already have a Suwayomi source that will supply chapters. The feed's contract is
//! that a work with no dated chapter is not an update and is therefore EXCLUDED (0064), so
//! Browse moved to a table that can hold them with a NULL `released_at` instead of widening
//! the updates feed. `hasChapters` is the filter for a reader who only wants what they can
//! read right now.
//!
//! THE THREE INVARIANTS EVERYTHING BELOW IS SHAPED BY
//!
//! 1. **The WHERE is built ONCE** and formatted into both the page statement and the
//!    COUNT statement, with binds pushed in the same order. The sort may change the ORDER
//!    BY and it may add an exclusion for a two-phase sort's second half, but it must never
//!    change the filter — the moment it does, `total` stops describing the paged set and
//!    the pager's "showing 61-90 of N" becomes fiction. Same precedent as
//!    `graphql::updates_feed`.
//! 2. **Every branchable predicate is BRANCHED into the SQL text, never parameterized.**
//!    `WHERE (? = 1 OR f.is_nsfw = FALSE)` is opaque to the planner: it cannot tell `is_nsfw`
//!    is constant, so it cannot use the index's leading column and falls back to sorting
//!    all 115k rows through a temp B-tree. Migration 0064's header and `updates_feed` both
//!    document this. `comic_type` is branched for the same reason — as an equality it
//!    becomes an index prefix, so one index serves filter AND order.
//! 3. **Every sort carries `f.work_id` as a tiebreaker, in the sort's own direction.** Not
//!    cosmetic: ~76,000 rows share an `en_chapter_count` in the 0..5 range and 67,000 share
//!    `released_at IS NULL`, and without a total order LIMIT/OFFSET repeats or skips rows at a
//!    page boundary. (Same direction as the key, so a sort and its reverse walk one index.)
//!
//! BROWSE V2 ("MangaDex advanced search", migration 0106) keeps all three. It adds filters
//! (tag include/exclude with AND/OR by `work_tag.tag_key`, several statuses/formats/
//! demographics/content ratings/languages, year range, minimum chapters/follows/MangaDex
//! rating, source), sorts with a direction and NULLs last in both, and [`search_text`] — the
//! SAME WHERE applied to a full-text match set, so a text query no longer discards every
//! filter. v1 requests map onto it unchanged (`From<BrowseSort> for Sort`).

use crate::db::Database;
use anyhow::Result;

use crate::graphql::types::BrowseSort;

/// Rows per Browse page. Kept here rather than in the resolver because the COUNT memo,
/// the two-phase offset arithmetic and `has_next_page` all have to agree with it.
pub const BROWSE_PAGE_SIZE: i64 = 30;

/// Ceiling on how many genre chips one request may filter by.
///
/// The genre clause is `tag IN (…)` with one bind per genre, and the genre list comes
/// straight off the wire. Two things need bounding: the statement's bind count, and the
/// COUNT memo's key space (a request can otherwise mint an unbounded number of distinct
/// keys — see [`store_count`] for why that matters). 12 is well past what the UI shows and
/// past any useful ANY-match: MangaDex carries ~50 genre tags total, so a 12-way OR
/// already selects most of the catalogue.
pub const MAX_GENRE_FILTERS: usize = 12;
/// Bound free-text genre names in both SQL binds and memo signatures.
pub const MAX_GENRE_NAME_BYTES: usize = 128;

/// How many trending keys the TRENDING sort ranks before falling back to newest-first.
///
/// Trending is a top-N ranking by construction (`views::trending_keys` aggregates the last
/// 24 h of `series_view_bucket`), so a bound here is not an approximation of something
/// exact — it IS the ranking. 500 keys is ~17 pages deep at [`BROWSE_PAGE_SIZE`], far past
/// where anyone scrolls, and matches `catalog::RANK_WINDOW`'s precedent for the same
/// "rank a bounded window, then stop" decision. Production holds 43 bucket rows / 17
/// resolvable works today, so this is entirely headroom.
const TRENDING_KEY_LIMIT: i64 = 500;

/// Browse v2's limits on client-supplied lists (the GraphQL contract states them; the resolver
/// rejects a longer list rather than truncating it, because truncating an include-AND list
/// silently WIDENS the result). Bounded for the same two reasons as [`MAX_GENRE_FILTERS`]:
/// the statement's bind count and the memo key space.
pub const MAX_TAG_FILTERS: usize = 32;
pub const MAX_LANGUAGE_FILTERS: usize = 16;

/// The `yearFrom`/`yearTo` clamp. Wide enough for any real publication year; it exists so a
/// client cannot mint unbounded distinct memo keys out of absurd integers.
pub const YEAR_MIN: i64 = 1900;
pub const YEAR_MAX: i64 = 2100;

/// Ceilings for the numeric minimums, for the same memo-key reason. Both are far past any
/// real value (the most-followed MangaDex title has ~400k follows; the longest run ~1,100
/// chapters), so clamping never changes a result.
const MIN_CHAPTERS_CAP: i64 = 100_000;
const MIN_FOLLOWS_CAP: i64 = 100_000_000;

/// What Browse orders by. A superset of the wire-level `BrowseSort` (v1) and
/// `BrowseSortField` (v2): every v1 member maps onto one of these with `desc = true`
/// (see `From<BrowseSort>`), which is what keeps an old client's ordering byte-identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// Text search's ranking (title tier, then chapters, then bm25). Without a query there is
    /// nothing to rank, and it falls back to [`SortKey::LatestUpdate`] — the contract's rule.
    Relevance,
    /// `released_at` — the newest chapter release. NULL (no dated chapter) sorts last.
    LatestUpdate,
    /// `created_at` — when the work entered OUR catalogue (v1 `LATEST_ADDED`).
    RecentlyAdded,
    /// The 24-hour view ranking (v1 `TRENDING`), two-phase.
    MostViewed,
    /// `follows` — MangaDex statistics. NULL (not fetched / no MangaDex anchor) sorts last.
    MostFollows,
    /// `rating` — MangaDex's BAYESIAN rating. NULL sorts last.
    HighestRated,
    /// `en_chapter_count`.
    MostChapters,
    /// `title`, case-insensitively.
    Title,
    /// `year`. NULL sorts last.
    Year,
    /// OUR users' review average, two-phase: v1 `RATING` and v2 `READER_RATED`. Distinct
    /// from v2's HIGHEST_RATED, which is MangaDex's rating.
    CommunityRating,
}

/// A sort key plus its direction.
///
/// EVERY sort ends in `f.work_id` in the SAME direction as the key (invariant 3), so a sort
/// and its reverse are exact mirror images and one index serves both. NULL keys sort LAST in
/// BOTH directions — "unknown" is never the most- or least- anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub desc: bool,
}

impl Sort {
    pub const fn desc(key: SortKey) -> Self {
        Sort { key, desc: true }
    }

    /// One fixed word per (key, direction), for [`row_key`]. A closed vocabulary (no client
    /// text), so a bare `|` after it cannot be forged into looking like another key.
    fn word(self) -> &'static str {
        use SortKey::*;
        match (self.key, self.desc) {
            (Relevance, true) => "relevance",
            (Relevance, false) => "relevance-asc",
            // v1 words kept for the v1 sorts so their memo keys keep their old spelling.
            (LatestUpdate, true) => "newest",
            (LatestUpdate, false) => "newest-asc",
            (RecentlyAdded, true) => "latest-added",
            (RecentlyAdded, false) => "latest-added-asc",
            (MostViewed, true) => "trending",
            (MostViewed, false) => "trending-asc",
            (MostFollows, true) => "follows",
            (MostFollows, false) => "follows-asc",
            (HighestRated, true) => "md-rating",
            (HighestRated, false) => "md-rating-asc",
            (MostChapters, true) => "chapters",
            (MostChapters, false) => "chapters-asc",
            (Title, true) => "title-desc",
            (Title, false) => "title",
            (Year, true) => "year",
            (Year, false) => "year-asc",
            (CommunityRating, true) => "rating",
            (CommunityRating, false) => "rating-asc",
        }
    }

    /// The sort Browse (no text query) actually runs: RELEVANCE has nothing to rank there and
    /// becomes LATEST_UPDATE, keeping the requested direction.
    fn for_browse(self) -> Self {
        match self.key {
            SortKey::Relevance => Sort {
                key: SortKey::LatestUpdate,
                desc: self.desc,
            },
            _ => self,
        }
    }
}

impl From<BrowseSort> for Sort {
    /// The v1 enum, mapped so an old client's ordering is IDENTICAL: every v1 sort was
    /// descending, and each maps onto the key that implemented it.
    fn from(s: BrowseSort) -> Self {
        Sort::desc(match s {
            BrowseSort::Trending => SortKey::MostViewed,
            BrowseSort::Newest => SortKey::LatestUpdate,
            BrowseSort::LatestAdded => SortKey::RecentlyAdded,
            BrowseSort::Rating => SortKey::CommunityRating,
            BrowseSort::Chapters => SortKey::MostChapters,
        })
    }
}

/// The single-column sorts: `(column expression, nullable)`. `None` for the keys that are not
/// a `browse_catalogue` column (the two-phase rankings and RELEVANCE).
///
/// Every column here has a `(is_nsfw, key DESC, work_id DESC)` + `(key DESC, work_id DESC)`
/// index pair (0069/0102/0106), and the title sort uses the baseline ASCII-folded `sort_title` expression.
/// The expression must match its index for native ordered access.
fn sort_column(key: SortKey) -> Option<(&'static str, bool)> {
    match key {
        SortKey::LatestUpdate => Some(("f.released_at", true)),
        SortKey::RecentlyAdded => Some(("f.created_at", false)),
        SortKey::MostChapters => Some(("f.en_chapter_count", false)),
        SortKey::MostFollows => Some(("f.follows", true)),
        SortKey::HighestRated => Some(("f.rating", true)),
        SortKey::Year => Some(("f.year", true)),
        SortKey::Title => Some((
            "translate(f.title, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz')",
            false,
        )),
        SortKey::Relevance | SortKey::MostViewed | SortKey::CommunityRating => None,
    }
}

fn dir(desc: bool) -> &'static str {
    if desc {
        "DESC"
    } else {
        "ASC"
    }
}

/// The two-phase sorts' remainder order (and TRENDING's cold fallback): newest release
/// first. Fixed regardless of the requested direction — the remainder is the UNRANKED tail,
/// which sorts last either way, and "newest first" is the only order in which a reader who
/// scrolls that far finds anything.
const NEWEST_ORDER: &str = "f.released_at DESC NULLS LAST, f.work_id DESC";

/// The per-work review aggregate, keyed by **work id**, for both the RATING sort and the
/// `minRating`/`maxRating` bounds.
///
/// `reviews.series_id` is POLYMORPHIC — it holds whatever id the reader was looking at when
/// the review was posted, so a `w_…` for a canonical work and a numeric Suwayomi id
/// otherwise. Production holds both shapes (23 `w_…` rows and 8 numeric ones), and 9 of the
/// canonical ones are on works Browse links by a NUMERIC id. Keying on the literal id
/// therefore loses ratings in both directions, which is exactly how a rated work printed
/// "0 community ratings" on the page it was rated from. Every reader now resolves the id
/// onto its work first — this CTE for the RATING sort and the bounds, and
/// `graphql::rating_summary_for_series_ids` for the series/feed cards, which is what keeps
/// the star on a card and the star on that work's page the same number.
///
/// Both arms of the UNION resolve to a WORK id — the `w_…` branch because a `w_` id *is* a
/// work id, and the numeric branch via `source_series` — so the join key downstream is
/// uniformly `f.work_id`. `UNION ALL`, not `UNION`: two identical scores from two users
/// are two votes, and `UNION` would silently dedupe them.
///
/// `LIKE 'w!_%' ESCAPE '!'` and not `LIKE 'w\_%'`: the `_` has to be escaped or it matches
/// any character, which would pull ids like `w2` into the canonical arm.
///
/// ONE VOTE PER (work, user). Resolving both key shapes onto a work makes it possible for a
/// single user to hold TWO `reviews` rows for the SAME work — one filed under the `w_…` id
/// and one under the numeric id of whichever source they had open (production holds two such
/// pairs, and every future write normalizes to `w_…` while the historical numeric rows stay).
/// Summing both rows would turn the "0 ratings" bug into a double-counting bug, so the votes
/// are ranked per (work, user) and only the newest one survives — `updated_at` DESC, with the
/// row id as a deterministic tiebreak for two writes inside the same second. The same
/// window also collapses a work that maps the same `source_key` twice.
///
/// Exposed as a MACRO rather than a `const` so the aggregate readers in `graphql` can splice
/// the identical text into their own statements: "sorted by rating" and "the star on the
/// card" have to be the same arithmetic, and two copies of this SQL is exactly how they
/// would drift apart.
macro_rules! rated_votes_sql {
    () => {
        r#"SELECT wid, user_id, score FROM ( SELECT wid, user_id, score, ROW_NUMBER() OVER (PARTITION BY wid, user_id ORDER BY updated_at DESC NULLS LAST, rid DESC NULLS LAST) AS rn FROM ( SELECT r.series_id AS wid, r.user_id AS user_id, r.score AS score, r.updated_at AS updated_at, r.id AS rid FROM reviews r WHERE r.series_id LIKE 'w!_%' ESCAPE '!' UNION ALL SELECT ss.work_id AS wid, r.user_id AS user_id, r.score AS score, r.updated_at AS updated_at, r.id AS rid FROM reviews r JOIN source_series ss ON ss.source_key = r.series_id AND ss.source_type = 'suwayomi' ) ) WHERE rn = 1"#
    };
}
pub(crate) use rated_votes_sql;

/// The `rated_per_work` CTE's DEFINITION, without the `WITH` — for a statement that has
/// other CTEs of its own (text search) and must list them all after one `WITH`.
const RATED_PER_WORK_DEF: &str = concat!(
    "rated_per_work AS (SELECT wid, AVG(score)::double precision AS avg FROM (",
    rated_votes_sql!(),
    ") GROUP BY wid)"
);

const RATED_PER_WORK_CTE: &str = concat!(
    "WITH rated_per_work AS (SELECT wid, AVG(score)::double precision AS avg FROM (",
    rated_votes_sql!(),
    ") GROUP BY wid) "
);

/// The columns a Browse card needs that `browse_catalogue` already carries. Everything
/// else (author, artist, description, genres, rating) is fetched for the whole page in one
/// grouped query each — see `graphql::map_browse_rows`.
/// `content_rating` is deliberately NOT selected: it is a FILTER key (and index payload),
/// never a rendered field, so reading it back would cost a wider row for nothing.
const BROWSE_COLS: &str = "f.work_id, f.reader_id, f.title, f.cover_url, f.suwayomi_thumbnail, \
     f.comic_type, f.status, f.en_chapter_count, f.latest_chapter, f.released_at, f.is_nsfw, \
     f.created_at";

/// One `browse_catalogue` row as Browse reads it.
#[derive(sqlx::FromRow, Clone, Debug)]
pub struct BrowseRow {
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub work_id: String,
    /// The id the card NAVIGATES to, which is **not** always `work_id` — see
    /// [`BrowseQuery`]'s note and migration 0069.
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub reader_id: String,
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub title: String,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub cover_url: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub suwayomi_thumbnail: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub comic_type: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub status: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    pub en_chapter_count: i64,
    /// The newest chapter's LABEL — "151", "10.5", "Oneshot" (migration 0095). A different
    /// quantity from [`Self::en_chapter_count`], which is how many chapters we know of: the
    /// card renders both ("12 ch · Ch. 151") precisely because printing one under the
    /// other's label is F4, the bug this overhaul exists to kill.
    ///
    /// `None` for the ~67,000 works with no feed row to copy it from, and for any feed row
    /// the ledger projection has not labelled. The card must then print the count alone —
    /// never "Ch. undefined", and never the count wearing a "Ch." prefix.
    ///
    /// TEXT, and never parsed back to a float: it is `release_event.label`, whose
    /// vocabulary is defined once in `chapter_label::ChapterLabel`.
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub latest_chapter: Option<String>,
    /// Newest upstream chapter release (epoch millis), or **`None` for a work with no dated
    /// chapter** — 67,000 of the 115,567 rows. NULLABLE is the whole point of migration
    /// 0069; `feed_series_updates.released_at` is NOT NULL by contract.
    #[sqlx(try_from = "crate::db_types::DbOptionalI64")]
    pub released_at: Option<i64>,
    pub is_nsfw: bool,
    /// When the WORK entered the catalogue (ISO-8601). Read off the row rather than from the
    /// per-page `work` lookup, which is what lets a chapterless card carry an honest
    /// `updatedAt` — the only other candidate, `released_at`, is NULL for exactly those.
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub created_at: String,
}

/// How several tags combine — MangaDex's `includedTagsMode` / `excludedTagsMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TagMode {
    /// Include: the work carries ALL of them. Exclude: drop only works carrying ALL of them.
    #[default]
    And,
    /// Include: the work carries ANY of them. Exclude: drop works carrying ANY of them.
    Or,
}

/// Browse v2's `source` filter, parsed (see [`parse_source`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceFilter {
    /// Works with a MangaDex anchor.
    MangaDex,
    /// Works with ANY Suwayomi source.
    Suwayomi,
    /// Works with a Suwayomi source of this extension/source id (`source_series.source_id`).
    SuwayomiSource(String),
}

/// One filtered, sorted Browse request.
///
/// Every word-valued field is a fixed-vocabulary token the resolver has already mapped
/// from a GraphQL enum (`'MANGA'`, `'ONGOING'`, `'safe'` …), never raw client text — that
/// is what lets them be formatted into the SQL as branches rather than bound. The
/// client-controlled TEXT fields (`genres`, `languages`, the tag ids, a source id) are always
/// BOUND, and canonicalized first ([`canonical_genres`], [`canonical_tag_ids`],
/// [`canonical_languages`], [`parse_source`]) so the memo key does not depend on spelling or
/// order.
///
/// The one exception to "client text is bound" is `blocks`: a rule's values are written
/// into the SQL, which is safe because `block::BlockRule::new` admits a closed vocabulary
/// only (see the `block` module header).
pub struct BrowseQuery<'a> {
    /// The viewer's content block rules (`block.rs`), or `None` for no rule. A work a
    /// rule matches is left out of the page AND the total, like a work behind the NSFW
    /// gate. Part of the memo key, so two viewers share a memoized page only when the
    /// same works are blocked for both.
    pub blocks: Option<&'a crate::block::BlockSet>,
    /// The viewer's NSFW posture. `false` pins `f.is_nsfw = FALSE`; `true` emits no predicate
    /// at all (not `is_nsfw IN (0,1)`) so the opted-in branch reads a different index.
    pub show_nsfw: bool,
    /// Collapsed format words (`collapsed_comic_type_word`), deduped; empty = all formats.
    /// v1 honoured only the FIRST format; v2 ORs them all (one is an index-prefix equality,
    /// several an `IN`).
    pub types: Vec<&'static str>,
    /// Normalized status words, deduped; empty = any status.
    pub statuses: Vec<&'static str>,
    /// The content-rating values admitted, or `None` for no rating clause. v1's cumulative
    /// tiers and v2's exact `contentRatings` set both land here — either way an allow-list
    /// over the EFFECTIVE rating `browse_catalogue.content_rating` stores (migration 0106,
    /// `catalog::effective_content_rating_sql`), and either way INSIDE the NSFW gate.
    pub ratings: Option<&'a [&'a str]>,
    /// Restrict to adult-rated works only (`ContentRatingFilter::NSFW_ONLY`). Constrains
    /// `is_nsfw`, not `content_rating`, so it is independent of `ratings` above.
    pub nsfw_only: bool,
    /// v1 genre ANY-match over `work_tag.tag` display names. Canonicalized by
    /// [`canonical_genres`] before it gets here. (v2 filters by tag ID — `include_tags`.)
    pub genres: &'a [String],
    /// `Some(true)` keeps only works we know a chapter for (`en_chapter_count > 0`);
    /// `Some(false)` keeps only works we know none for. `None` — the default — emits NO
    /// clause, i.e. the whole browsable catalogue.
    ///
    /// It is NOT the inverse of "is in the updates feed", in either direction, and both are
    /// measured: 11,054 works IN the feed count 0 here (their mirrored chapters carry a NULL
    /// `number` — oneshots) and 1,505 works absent from it count more than 0. So this is "we
    /// know of a chapter", which is what a reader asking for something readable means.
    pub has_chapters: Option<bool>,
    /// v1 bounds on OUR users' review average (see [`RATED_PER_WORK_CTE`]).
    pub min_rating: Option<f64>,
    pub max_rating: Option<f64>,
    /// Demographic words (`'shounen'`, …) plus `'none'` for "no demographic"; empty = any.
    pub demographics: Vec<&'static str>,
    /// Original-language codes, canonicalized by [`canonical_languages`]; empty = any.
    pub languages: Vec<String>,
    /// Inclusive publication-year bounds, already clamped and ordered ([`year_range`]).
    pub year_from: Option<i64>,
    pub year_to: Option<i64>,
    /// `en_chapter_count >= n`; `None` for no clause (0 is normalized to `None`).
    pub min_chapters: Option<i64>,
    /// MangaDex follows `>= n` (`browse_catalogue.follows`); excludes works with no stats.
    pub min_follows: Option<i64>,
    /// MangaDex BAYESIAN rating `>= x` (`browse_catalogue.rating`); excludes works with no
    /// stats. Not to be confused with v1's `min_rating`, which is our own reviews.
    pub min_md_rating: Option<f64>,
    pub source: Option<SourceFilter>,
    /// Tag IDS (`work_tag.tag_key`), canonicalized by [`canonical_tag_ids`].
    pub include_tags: Vec<String>,
    pub include_mode: TagMode,
    pub exclude_tags: Vec<String>,
    pub exclude_mode: TagMode,
    pub sort: Sort,
    pub page: i64,
}

impl Default for BrowseQuery<'_> {
    /// No filter at all, NSFW gate CLOSED (the safe default), newest first, page 1.
    fn default() -> Self {
        BrowseQuery {
            blocks: None,
            show_nsfw: false,
            types: Vec::new(),
            statuses: Vec::new(),
            ratings: None,
            nsfw_only: false,
            genres: &[],
            has_chapters: None,
            min_rating: None,
            max_rating: None,
            demographics: Vec::new(),
            languages: Vec::new(),
            year_from: None,
            year_to: None,
            min_chapters: None,
            min_follows: None,
            min_md_rating: None,
            source: None,
            include_tags: Vec::new(),
            include_mode: TagMode::And,
            exclude_tags: Vec::new(),
            exclude_mode: TagMode::Or,
            sort: Sort::desc(SortKey::LatestUpdate),
            page: 1,
        }
    }
}

/// A value bound into a dynamically-assembled statement. The SQL text is assembled from
/// fixed vocabulary only; everything variable travels through here.
#[derive(Clone, Debug)]
enum Bind {
    Text(String),
    Real(f64),
    Int(i64),
}

fn bind_rows<'q>(
    mut q: sqlx::query::QueryAs<'q, sqlx::Postgres, BrowseRow, sqlx::postgres::PgArguments>,
    binds: &[Bind],
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, BrowseRow, sqlx::postgres::PgArguments> {
    for b in binds {
        q = match b {
            Bind::Text(s) => q.bind(s.clone()),
            Bind::Real(v) => q.bind(*v),
            Bind::Int(v) => q.bind(*v),
        };
    }
    q
}

fn bind_count<'q>(
    mut q: sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    binds: &[Bind],
) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments> {
    for b in binds {
        q = match b {
            Bind::Text(s) => q.bind(s.clone()),
            Bind::Real(v) => q.bind(*v),
            Bind::Int(v) => q.bind(*v),
        };
    }
    q
}

/// Trim, drop blanks, DEDUPE and SORT a client genre list, then cap it at
/// [`MAX_GENRE_FILTERS`].
///
/// Canonicalizing here rather than at the SQL is what makes the COUNT memo useful: an
/// ANY-match's result set does not depend on the order the tags arrive in, so
/// `["Action","Drama"]` and `["Drama","Action"]` must not be two cache entries for one
/// total. Sorting also makes the key deterministic across clients that build the list
/// differently (chip order vs. alphabetical).
pub fn canonical_genres(genres: &[String]) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for genre in genres {
        let genre = genre.trim();
        if genre.is_empty() {
            continue;
        }
        out.insert(if genre.len() > MAX_GENRE_NAME_BYTES {
            UNMATCHABLE.to_string()
        } else {
            genre.to_string()
        });
        if out.len() > MAX_GENRE_FILTERS {
            out.pop_last();
        }
    }
    out.into_iter().collect()
}

/// A tag id no tag can have: [`crate::tags::slug`] only ever produces `[a-z0-9-]`. An
/// implausible client id is REPLACED by this rather than dropped, so it keeps its meaning of
/// "a tag nobody carries": dropped from an include-AND list it would silently WIDEN the
/// result, while as this sentinel it correctly matches nothing (include) or excludes nothing
/// (exclude). Every implausible id collapses onto it, which also bounds the memo key.
const UNMATCHABLE: &str = "!";

/// Canonicalize a client tag-id list: trim, lowercase, implausible ids -> [`UNMATCHABLE`],
/// dedupe, sort. The caller enforces [`MAX_TAG_FILTERS`] BEFORE this (and errors on a longer
/// list) — see that constant for why truncation is not an option here.
///
/// Sorted for the memo's sake, as with [`canonical_genres`]: AND and OR are both
/// order-insensitive, so `[a, b]` and `[b, a]` must share one entry.
pub fn canonical_tag_ids(ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = ids
        .iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .map(|t| {
            if crate::tags::is_plausible_tag_id(&t) {
                t
            } else {
                UNMATCHABLE.to_string()
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Canonicalize original-language codes the way `catalog::BROWSE_LANGUAGE_SQL` stores them:
/// trimmed and lowercased. A code that is not language-shaped (`[a-z]{2,3}` plus optional
/// `-subtag`s of alphanumerics, at most 16 bytes) becomes [`UNMATCHABLE`] — a value no row
/// stores, so it matches nothing rather than widening the filter. Deduped and sorted.
pub fn canonical_languages(codes: &[String]) -> Vec<String> {
    fn plausible(c: &str) -> bool {
        let mut parts = c.split('-');
        let head = parts.next().unwrap_or("");
        c.len() <= 16
            && (2..=3).contains(&head.len())
            && head.bytes().all(|b| b.is_ascii_lowercase())
            && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric()))
    }
    let mut out: Vec<String> = codes
        .iter()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .map(|c| {
            if plausible(&c) {
                c
            } else {
                UNMATCHABLE.to_string()
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Clamp a `yearFrom`/`yearTo` pair into [`YEAR_MIN`]..=[`YEAR_MAX`] and put it in order. A
/// reversed range (`2020..2010`) is read as the range the user meant rather than as an empty
/// result — the same reading every year-range picker's two independent dropdowns invite.
pub fn year_range(from: Option<i64>, to: Option<i64>) -> (Option<i64>, Option<i64>) {
    let clamp = |y: i64| y.clamp(YEAR_MIN, YEAR_MAX);
    match (from.map(clamp), to.map(clamp)) {
        (Some(a), Some(b)) if a > b => (Some(b), Some(a)),
        other => other,
    }
}

/// `minChapters`: `<= 0` is no filter at all (every work has `>= 0`), and the ceiling only
/// bounds the memo key space.
pub fn min_chapters(n: Option<i64>) -> Option<i64> {
    n.filter(|n| *n > 0).map(|n| n.min(MIN_CHAPTERS_CAP))
}

/// `minFollows`: as [`min_chapters`] — `<= 0` is no filter. (Emitting `follows >= 0` would NOT
/// be a no-op: it would drop every work with no statistics yet.)
pub fn min_follows(n: Option<i64>) -> Option<i64> {
    n.filter(|n| *n > 0).map(|n| n.min(MIN_FOLLOWS_CAP))
}

/// `minRating` (MangaDex bayesian, 0-10): non-finite or `<= 0` is no filter (same "would drop
/// the unrated" argument as [`min_follows`]); above 10 clamps to 10. Rounded to 2 decimals so
/// the memo key space is finite — a 0.001 difference in a rating floor is not a different
/// question.
pub fn min_md_rating(x: Option<f64>) -> Option<f64> {
    x.filter(|x| x.is_finite() && *x > 0.0)
        .map(|x| (x.min(10.0) * 100.0).round() / 100.0)
}

/// Parse the `source` filter. `"mangadex"` and `"suwayomi"` (any case) are the two families;
/// anything else is a Suwayomi extension/source id, which must be short and
/// `[A-Za-z0-9_-]` (Suwayomi's are decimal longs) or it becomes [`UNMATCHABLE`] — a source no
/// row has. Blank is no filter.
pub fn parse_source(raw: Option<&str>) -> Option<SourceFilter> {
    let s = raw.map(str::trim).filter(|s| !s.is_empty())?;
    Some(match s.to_ascii_lowercase().as_str() {
        "mangadex" => SourceFilter::MangaDex,
        "suwayomi" => SourceFilter::Suwayomi,
        _ if s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            SourceFilter::SuwayomiSource(s.to_string())
        }
        _ => SourceFilter::SuwayomiSource(UNMATCHABLE.to_string()),
    })
}

// ---- the COUNT memo -------------------------------------------------------------------
//
// SHARED WITH `graphql::updates`, which memoizes its own `total` through the same static
// and the same `clear_count_cache` (see `graphql::updates_total`). One memo rather than two
// because there is exactly one invalidation event for both — `catalog::refresh_browse_catalogue`,
// which ends the chain that rewrites everything either total counts — and two caches would
// mean two chances to forget to clear one. The key spaces are kept disjoint by prefix
// (`v3|` here, `updates:v1|` there).

/// How long a memoized Browse total stays usable. Short enough that a sync cycle's
/// additions show up within a refresh or two, long enough that a reader paging through
/// Browse pays the count once rather than once per page.
#[cfg(not(test))]
pub(crate) const COUNT_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Zero under test: the memo is a process-wide static but every test builds its own
/// in-memory DB, so two tests whose filters hash to the same key would otherwise read each
/// other's totals depending on scheduling. A zero TTL makes [`browse_catalogue`] compute
/// exactly, always. The memo logic itself is covered directly, via [`cached_count_with`].
#[cfg(test)]
pub(crate) const COUNT_TTL: std::time::Duration = std::time::Duration::ZERO;

/// Ceiling on distinct memoized filter signatures.
///
/// Raised from the 512 `search_catalogue` used, because the key space grew by three
/// dimensions: it was {nsfw} x {genres} x {rating bounds} and it is now that plus
/// {5 content-rating tiers} x {4 format words} x {6 status words} — 240 combinations before
/// a single genre chip is involved, versus 2. At ~80 bytes a key this is ~160 KiB worst
/// case, which is nothing next to what it buys (the genre COUNT is 68.8 ms even index-only;
/// on the tag-dense replica it measured 150 ms).
///
/// Browse v2 made the space effectively unbounded (tag sets, year ranges, text queries), so
/// the cap is now a memory bound rather than a coverage claim: a v2 key with a few tags is
/// ~150 bytes, i.e. ~300 KiB at the cap, and the hot heads (no filter / one tab / one tag)
/// still fit many times over. Eviction is expired-first ([`store_count_with`]).
const COUNT_CACHE_CAP: usize = 2048;

/// The memo key for one filter combination.
///
/// Extracted so its completeness is directly testable: under `cfg(test)` [`COUNT_TTL`] is
/// zero, so [`browse_catalogue`] never reads the memo and no end-to-end test can catch a
/// key that omits a filter the COUNT depends on.
///
/// It must cover every input to that COUNT — and NOTHING ELSE. In particular it must NOT
/// include the sort (key OR direction) or `page`: every sort shares one WHERE, so they share
/// one total, and keying on sort would divide the hit rate for no gain. (`page` obviously
/// cannot change a total.) The two-phase rankings' `rated_count` / trending prefix length are
/// separate, unmemoized counts; a nullable ASC sort's non-NULL prefix is a separate count
/// memoized under its own `nn:` key ([`key_for_nulls`]); text search's total under an `fts|`
/// key carrying the MATCH expression, and its retry statement's under `fts-retry|`
/// ([`search_text`]). None of them is this number.
///
/// `v4|` PREFIX. The memo is a process-wide static and the totals it holds are computed over
/// whichever table this module read when they were stored. Each repointing has changed that
/// table's SCALE — 13,847 (`suwayomi_series`) → 48,567 (`feed_series_updates`, `v2`) →
/// 115,567 (`browse_catalogue`, `v3`) — and a stale entry under a colliding key would serve
/// one scale's `total` for another, which the pager renders as a page count and
/// `hasNextPage` as a promise of rows that do not exist. The prefix makes that impossible by
/// construction rather than by arguing about format overlap. `v4` is Browse v2: a wider
/// filter set, and migration 0106's change to what `content_rating` means.
///
/// FREE-TEXT ENCODING. `search_catalogue` could join genres with `\u{1}` and call the result
/// unforgeable, because it keyed on the escaped LIKE PATTERNS (`%"Action"%`) and the
/// escaping guaranteed no pattern contained a bare `%`. This keys on RAW client strings
/// (genres, languages, tag ids, a source id), where no character is off-limits, so a
/// separator-based join is forgeable: `["a","b"]` and `["a<sep>b"]` would collide for any
/// `<sep>`. Length-prefixed instead — each string becomes `{byte_len}:{s}`, each list is
/// prefixed with its count, and they are concatenated with no separator. That is injective
/// because the decoder is deterministic: read digits to the `#`/`:`, then take exactly that
/// many items/bytes. `["a","b"]` -> `2#1:a1:b`, while the single tag `a1:b` -> `1#4:a1:b`.
fn count_key(q: &BrowseQuery<'_>) -> String {
    // `v4|`: Browse v2 widened the filter set (and the effective `content_rating` changed
    // meaning in migration 0106), so no v3 entry may ever be read as a v4 one.
    let mut key = String::from("v4|");
    key.push(if q.show_nsfw { '1' } else { '0' });
    key.push('|');
    // Its own dimension: NSFW_ONLY emits no rating clause, so without this a cached
    // "adult only" total would be served for the unfiltered query and vice versa.
    key.push(if q.nsfw_only { '1' } else { '0' });
    key.push('|');
    // Fixed-vocabulary lists: joining their members with ',' is injective because every
    // member is a fixed word containing no ',' or '|'. `ratings` is NOT sorted here — v1 tiers
    // are already canonical and v2's set is sorted by the resolver.
    if let Some(r) = q.ratings {
        key.push_str(&r.join(","));
    } else {
        // Distinct from `Some(&[])`, which no caller produces but which would be a different
        // WHERE (`IN ()` matches nothing).
        key.push('*');
    }
    key.push('|');
    key.push_str(&q.types.join(","));
    key.push('|');
    key.push_str(&q.statuses.join(","));
    key.push('|');
    key.push_str(&q.demographics.join(","));
    key.push('|');
    // Its own dimension, and MANDATORY: `hasChapters` is one WHERE clause, so without it the
    // total for "everything" (115,567) would be served to a viewer who asked for "only what I
    // can read" (39,018) and vice versa — a pager offering 3,853 pages of a 1,301-page result.
    key.push(match q.has_chapters {
        Some(true) => '1',
        Some(false) => '0',
        None => '-',
    });
    key.push('|');
    // Numeric bounds, positionally: "min 3, no max" is not "no min, max 3".
    let num = |key: &mut String, v: Option<String>| {
        if let Some(v) = v {
            key.push_str(&v);
        }
        key.push('|');
    };
    num(&mut key, q.min_rating.map(|v| v.to_string()));
    num(&mut key, q.max_rating.map(|v| v.to_string()));
    num(&mut key, q.year_from.map(|v| v.to_string()));
    num(&mut key, q.year_to.map(|v| v.to_string()));
    num(&mut key, q.min_chapters.map(|v| v.to_string()));
    num(&mut key, q.min_follows.map(|v| v.to_string()));
    num(&mut key, q.min_md_rating.map(|v| v.to_string()));
    key.push(match q.include_mode {
        TagMode::And => 'a',
        TagMode::Or => 'o',
    });
    key.push(match q.exclude_mode {
        TagMode::And => 'a',
        TagMode::Or => 'o',
    });
    key.push('|');
    // THE FREE-TEXT TAIL, LAST, and self-delimiting. `search_catalogue` could join genres
    // with `\u{1}` and call the result unforgeable, because it keyed on the escaped LIKE
    // PATTERNS and the escaping guaranteed no pattern contained a bare `%`. These are RAW
    // client strings, where no character is off-limits, so a separator-based join is
    // forgeable: `["a","b"]` and `["a<sep>b"]` would collide for any `<sep>`. Every string is
    // length-prefixed (`{byte_len}:{s}`) and every LIST count-prefixed (`{n}#`), and the source
    // is tagged — so a decoder reads the tail deterministically, which is what makes it
    // injective: `["a","b"]` -> `2#1:a1:b`, while the single tag `a1:b` -> `1#4:a1:b`, and no
    // string can bleed from one list into the next.
    fn list(key: &mut String, items: &[String]) {
        key.push_str(&format!("{}#", items.len()));
        for s in items {
            key.push_str(&format!("{}:{}", s.len(), s));
        }
    }
    match &q.source {
        None => key.push('-'),
        Some(SourceFilter::MangaDex) => key.push('m'),
        Some(SourceFilter::Suwayomi) => key.push('s'),
        Some(SourceFilter::SuwayomiSource(id)) => key.push_str(&format!("i{}:{}", id.len(), id)),
    }
    list(&mut key, &q.languages);
    list(&mut key, &q.include_tags);
    list(&mut key, &q.exclude_tags);
    list(&mut key, &canonical_genres(q.genres));
    // The block rules, as one more self-delimiting string. `BlockSet::key` is canonical
    // (sorted rules, sorted members), so the key does not depend on the order the rules
    // were saved in. No rule and an empty set are the same WHERE, and the same key.
    let blocks = q.blocks.map(|b| b.key()).unwrap_or_default();
    key.push_str(&format!("{}:{}", blocks.len(), blocks));
    key
}

/// Memoized Browse totals, keyed by the filter signature.
#[allow(clippy::type_complexity)]
static COUNT_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (i64, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn cached_count(key: &str) -> Option<i64> {
    cached_count_with(key, COUNT_TTL)
}

pub(crate) fn cached_count_with(key: &str, ttl: std::time::Duration) -> Option<i64> {
    let cache = COUNT_CACHE.lock().ok()?;
    let (total, at) = cache.get(key)?;
    (at.elapsed() < ttl).then_some(*total)
}

fn store_count(key: String, total: i64) {
    store_count_with(key, total, COUNT_TTL)
}

/// Evict EXPIRED entries first, and only fall back to a wholesale clear if that frees
/// nothing.
///
/// The cap is reached by cycling distinct filter signatures, which a client controls (see
/// [`MAX_GENRE_FILTERS`]) — so clearing unconditionally hands anyone a way to evict the hot
/// anonymous-Browse key on demand and make every page load pay the full COUNT again.
/// Dropping expired entries first means a flood slower than [`COUNT_TTL`] never touches a
/// live entry, and a faster one degrades to "no memo" (each miss still computes exactly)
/// rather than to something worse. Bounded either way.
pub(crate) fn store_count_with(key: String, total: i64, ttl: std::time::Duration) {
    let Ok(mut cache) = COUNT_CACHE.lock() else {
        return; // poisoned → just stop memoizing; every count is still computed exactly
    };
    if cache.len() >= COUNT_CACHE_CAP {
        cache.retain(|_, (_, at)| at.elapsed() < ttl);
        if cache.len() >= COUNT_CACHE_CAP {
            cache.clear();
        }
    }
    cache.insert(key, (total, std::time::Instant::now()));
}

/// Drop every memoized total AND every memoized row page. Called at the end of
/// `catalog::refresh_browse_catalogue` and from `graphql::clear_feed_caches` (the admin
/// NSFW mutations).
///
/// That refresh rewrites the table these totals describe, so a surviving entry can promise a
/// page that no longer exists — `hasNextPage: true` on the last page, and a pager that
/// renders "Page 3,853 of 3,853 · showing 115,531-115,560 of 115,530". The window is up to
/// [`COUNT_TTL`], twice a day, but it costs one `HashMap::clear` to close and the alternative
/// is a user-visible dead page.
///
/// The ROW memo goes with the totals, always: both describe the same generation of the
/// table, and a row page surviving a rebuild would render cards for works the rebuild
/// deleted (a merged-away `work_id` 404s) next to a `total` that no longer counts them.
/// Keeping one `clear` entry point also means every existing invalidation hook — there are
/// two, and both predate the row memo — invalidates both memos without knowing the row
/// memo exists.
pub fn clear_count_cache() {
    if let Ok(mut c) = COUNT_CACHE.lock() {
        c.clear();
    }
    if let Ok(mut c) = ROW_CACHE.lock() {
        c.clear();
    }
}

// ---- the row-page memo ----------------------------------------------------------------

/// How long a memoized Browse page of ROWS stays usable.
///
/// Half of [`COUNT_TTL`], deliberately: the rows are what the reader actually sees, and
/// unlike the total they also depend on two inputs OUTSIDE the filter signature — the
/// 24-hour view ranking (TRENDING is the default sort) and the review aggregate (RATING).
/// Neither is in the key, so a hit serves a ranking up to this much stale. 30 s is
/// invisible for a popularity ordering aggregated over 24 h, while still collapsing the
/// request storm the p50 target is about: at the measured 223 rps saturation, one entry
/// absorbs ~6,700 identical page queries per expiry.
#[cfg(not(test))]
const ROW_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Zero under test, for exactly [`COUNT_TTL`]'s reason: the memo is a process-wide static
/// but every test builds its own in-memory DB, so two tests whose queries hash to the same
/// key would otherwise read each other's pages depending on scheduling. The memo logic
/// itself is covered directly, via [`cached_rows_with`] / [`store_rows_with`].
#[cfg(test)]
const ROW_TTL: std::time::Duration = std::time::Duration::ZERO;

/// Ceiling on distinct memoized row pages.
///
/// Much smaller than [`COUNT_CACHE_CAP`] because the VALUES are three orders of magnitude
/// bigger: a total is 8 bytes, a page is up to [`BROWSE_PAGE_SIZE`] `BrowseRow`s (~500 B
/// each), so 256 entries bound the memo at ~4 MiB worst case. And 256 is generous for what
/// this memo is FOR — the hot head of Browse (the default sort's first pages, per NSFW
/// posture, per format tab), which is a few dozen combinations; a reader deep in a
/// long-tail filter combination misses and pays the 6.6 ms query, which was the price for
/// everyone before.
const ROW_CACHE_CAP: usize = 256;

/// Memoized Browse row pages, keyed by [`row_key`].
#[allow(clippy::type_complexity)]
static ROW_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (Vec<BrowseRow>, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// The memo key for one PAGE of rows: the sort (key AND direction, [`Sort::word`]) and the
/// (clamped) page, then the whole filter signature.
///
/// Built ON [`count_key`] rather than beside it, so a filter dimension added there is
/// covered here by construction — the count key's own test asserts its completeness, and
/// everything it separates, this separates. The two extra dimensions are exactly the ones
/// `count_key` deliberately EXCLUDES (its doc explains why the total must not key on
/// them): the sort reorders the same set, and the page windows it, so both change the rows.
///
/// PREPENDED, not appended: the count key's genre segment is last and self-terminating
/// only to a decoder ("nothing may be appended after this segment without its own
/// delimiter, since a tag can contain '|'"). Putting sort and page FIRST sidesteps that
/// argument entirely — both are fixed vocabulary (`Sort::word` is a closed set, the page a
/// decimal integer) so their `|` boundaries are unforgeable, and the count key keeps its
/// "genre segment is last" invariant intact.
///
/// The page is clamped with the same `max(1)` [`browse_catalogue`] applies, so page 0 and
/// page -5 share page 1's entry rather than caching duplicate copies of it.
fn row_key(q: &BrowseQuery<'_>) -> String {
    format!("{}|{}|{}", q.sort.word(), q.page.max(1), count_key(q))
}

fn cached_rows(key: &str) -> Option<Vec<BrowseRow>> {
    cached_rows_with(key, ROW_TTL)
}

fn cached_rows_with(key: &str, ttl: std::time::Duration) -> Option<Vec<BrowseRow>> {
    let cache = ROW_CACHE.lock().ok()?;
    let (rows, at) = cache.get(key)?;
    (at.elapsed() < ttl).then(|| rows.clone())
}

fn store_rows(key: String, rows: Vec<BrowseRow>) {
    store_rows_with(key, rows, ROW_TTL)
}

/// Same bounded-eviction shape as [`store_count_with`], for the same reason: the key space
/// is client-controlled (genres, and now page), so an unconditional clear at the cap would
/// hand anyone a way to evict the hot anonymous page-1 entry on demand. Expired entries go
/// first; only a flood faster than [`ROW_TTL`] degrades the memo to "no memo", where every
/// miss still computes exactly.
fn store_rows_with(key: String, rows: Vec<BrowseRow>, ttl: std::time::Duration) {
    let Ok(mut cache) = ROW_CACHE.lock() else {
        return; // poisoned → just stop memoizing; every page is still computed exactly
    };
    if cache.len() >= ROW_CACHE_CAP {
        cache.retain(|_, (_, at)| at.elapsed() < ttl);
        if cache.len() >= ROW_CACHE_CAP {
            cache.clear();
        }
    }
    cache.insert(key, (rows, std::time::Instant::now()));
}

// ---- the query ------------------------------------------------------------------------

/// Build the WHERE fragment shared by the page query and the COUNT, plus its binds in
/// bind order.
///
/// Returns `(where_sql, binds, uses_rated_cte)`. The caller must prepend
/// [`RATED_PER_WORK_CTE`] to any statement whose flag is set.
///
/// Each clause pushes its SQL and its binds TOGETHER, so the bind order is the clause order
/// by construction rather than by a second list that has to be kept in step.
fn build_where(q: &BrowseQuery<'_>) -> (String, Vec<Bind>, bool) {
    build_where_from(q, 1)
}

fn build_where_from(q: &BrowseQuery<'_>, start: usize) -> (String, Vec<Bind>, bool) {
    let mut parts: Vec<String> = Vec::new();
    let mut binds: Vec<Bind> = Vec::new();

    // The NSFW gate, BRANCHED. `false` pins the index's leading column to a constant and
    // leaves the rest of the index in ORDER BY order (the hot, edge-cacheable path);
    // `true` emits nothing, which is what lets the planner pick the `is_nsfw`-less index
    // instead of sorting 48k rows. NEVER `(? = 1 OR f.is_nsfw = FALSE)`.
    //
    // NOTHING BELOW CAN WIDEN THIS. Every other clause is ANDed onto it, so the content-rating
    // allow-list in particular can only narrow inside it — `contentRatings: [EROTICA,
    // PORNOGRAPHIC]` from an opted-out viewer is `is_nsfw = 0 AND content_rating IN (…)`,
    // which the effective rating (migration 0106) makes empty. Tested as an explicit bypass
    // attempt.
    if !q.show_nsfw {
        parts.push("f.is_nsfw = FALSE".into());
    }
    // NSFW_ONLY, the one non-cumulative content-rating value: the complement of SAFE
    // rather than a ceiling above it. Pushed as its own clause because it constrains
    // `is_nsfw`, not `content_rating`.
    //
    // Deliberately emitted even when the viewer is opted OUT, where it combines with the
    // gate above into `is_nsfw = 0 AND is_nsfw = TRUE` and yields an empty page. That is the
    // honest answer — they may not see those works — and it is why this is NOT skipped as
    // a redundant clause: dropping it would turn "adult only" into "everything you're
    // allowed to see", which is the opposite of what was asked for. The gate still reads
    // the index first, so the empty result costs a seek, not a scan.
    if q.nsfw_only {
        parts.push("f.is_nsfw = TRUE".into());
    }
    // The content block rules. ANDed like every other clause, so they can only narrow.
    // The rating a rule names is this table's own column (the effective rating, index
    // payload); a tag is one covering probe of `idx_work_tag_work_key` per candidate row,
    // the same probe the tag filter's EXCLUDE-OR uses. No binds: see `BrowseQuery`.
    if let Some(blocked) = q
        .blocks
        .and_then(|b| b.blocked_sql("f.work_id", Some("f.content_rating")))
    {
        parts.push(format!("NOT {blocked}"));
    }
    // An `= ?` for one value, `IN (…)` for several. "Branched" means the CLAUSE is present or
    // absent, not that the VALUE is interpolated — it is bound like everything else. What the
    // planner needs is a single-column equality it can use as a seek prefix, and a bound `?`
    // gives it that (verified: every shape plans as `SEARCH … USING INDEX idx_fsu_* (is_nsfw=?
    // AND comic_type=?)`). It is `(? = 1 OR …)` that defeats it, because there the column is
    // not constrained at all.
    fn any_of(
        start: usize,
        parts: &mut Vec<String>,
        binds: &mut Vec<Bind>,
        col: &str,
        vals: &[String],
    ) {
        match vals.len() {
            0 => {}
            1 => parts.push(format!(
                "{col} = {}",
                crate::sql_params::placeholders(start + binds.len(), 1)
            )),
            n => parts.push(format!(
                "{col} IN ({})",
                crate::sql_params::placeholders(start + binds.len(), n)
            )),
        }
        binds.extend(vals.iter().cloned().map(Bind::Text));
    }
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Format. Several formats are ORed (v2); one stays the index-prefix equality v1 had.
    any_of(
        start,
        &mut parts,
        &mut binds,
        "f.comic_type",
        &owned(&q.types),
    );
    // Status over the normalized five-word vocabulary migration 0068 materializes.
    // Un-normalized this could not be an equality at all — `komika_status` has no `ON_HIATUS`
    // member, so those rows would be unreachable by every filter value.
    any_of(
        start,
        &mut parts,
        &mut binds,
        "f.status",
        &owned(&q.statuses),
    );
    // Content rating: an allow-list over the EFFECTIVE rating (see the gate note above).
    // v1's ALL and PORNOGRAPHIC tiers arrive as `None` and emit no clause (they admit every
    // stored value), which is also why they are cheaper than SAFE.
    if let Some(r) = q.ratings {
        if r.is_empty() {
            // Not produced by any caller, but an empty allow-list means "nothing", and the
            // SQL `IN ()` it would otherwise format is a syntax error.
            parts.push("0".into());
        } else {
            any_of(start, &mut parts, &mut binds, "f.content_rating", &owned(r));
        }
    }
    // Demographic. `'none'` is MangaDex's "no demographic", materialized as NULL
    // (`catalog::BROWSE_DEMOGRAPHIC_SQL`), so it is an `IS NULL` arm rather than a value.
    {
        let named: Vec<String> = q
            .demographics
            .iter()
            .filter(|d| **d != "none")
            .map(|d| d.to_string())
            .collect();
        let none = q.demographics.contains(&"none");
        match (named.is_empty(), none) {
            (true, false) => {}
            (true, true) => parts.push("f.demographic IS NULL".into()),
            (false, false) => any_of(start, &mut parts, &mut binds, "f.demographic", &named),
            (false, true) => {
                parts.push(format!(
                    "(f.demographic IN ({}) OR f.demographic IS NULL)",
                    crate::sql_params::placeholders(start + binds.len(), named.len())
                ));
                binds.extend(named.into_iter().map(Bind::Text));
            }
        }
    }
    any_of(
        start,
        &mut parts,
        &mut binds,
        "f.original_language",
        &q.languages,
    );
    // The `hasChapters` gate. `en_chapter_count` is the leading sort key of the four CHAPTERS
    // indices and index PAYLOAD on the four `released_at` ones (migration 0069), so this stays
    // index-only on both orderings — measured 2.1-2.5 ms for the COUNT at 115,567 rows versus
    // 6.2-6.6 ms unfiltered, and no plan gains a temp B-tree.
    //
    // Branched exactly like the others: absent means NO clause, not `en_chapter_count >= 0`.
    // A `> 0` range predicate on a sort key the index already leads with is a seek bound, not
    // a filter — which is why the CHAPTERS plan reads
    // `SEARCH … (is_nsfw=? AND en_chapter_count>?)`.
    match q.has_chapters {
        Some(true) => parts.push("f.en_chapter_count > 0".into()),
        Some(false) => parts.push("f.en_chapter_count = 0".into()),
        None => {}
    }
    // Range floors/ceilings. Each is a range bound on a column with its own index (0106), and
    // each EXCLUDES a NULL — "at least 1,000 follows" cannot be claimed of a work whose follows
    // we do not know, which is the honest reading and the one MangaDex's own filters use.
    for (col, op, v) in [
        ("f.year", ">=", q.year_from),
        ("f.year", "<=", q.year_to),
        ("f.en_chapter_count", ">=", q.min_chapters),
        ("f.follows", ">=", q.min_follows),
    ] {
        if let Some(v) = v {
            parts.push(format!(
                "{col} {op} {}",
                crate::sql_params::placeholders(start + binds.len(), 1)
            ));
            binds.push(Bind::Int(v));
        }
    }
    if let Some(v) = q.min_md_rating {
        parts.push(format!(
            "f.rating >= {}",
            crate::sql_params::placeholders(start + binds.len(), 1)
        ));
        binds.push(Bind::Real(v));
    }
    // Source. `EXISTS` over `source_series` by `idx_source_series_work(work_id)` — one probe per
    // candidate row; the family words are fixed vocabulary, a source id is bound.
    match &q.source {
        None => {}
        Some(SourceFilter::MangaDex) => parts.push(
            "EXISTS (SELECT 1 FROM source_series ss \
              WHERE ss.work_id = f.work_id AND ss.source_type = 'mangadex')"
                .into(),
        ),
        Some(SourceFilter::Suwayomi) => parts.push(
            "EXISTS (SELECT 1 FROM source_series ss \
              WHERE ss.work_id = f.work_id AND ss.source_type = 'suwayomi')"
                .into(),
        ),
        Some(SourceFilter::SuwayomiSource(id)) => {
            parts.push(format!(
                "EXISTS (SELECT 1 FROM source_series ss \
                  WHERE ss.work_id = f.work_id AND ss.source_type = 'suwayomi' \
                    AND ss.source_id = {})",
                crate::sql_params::placeholders(start + binds.len(), 1)
            ));
            binds.push(Bind::Text(id.clone()));
        }
    }
    // TAGS, by `work_tag.tag_key` (migration 0106), with NO `source` predicate for the reason
    // the v1 genre clause below gives: a work matches every tag it carries, on every tier.
    //
    // INCLUDE-AND: "carries ALL n". One uncorrelated subquery, driven by
    // `idx_work_tag_key_work(tag_key, work_id)` — a covering SEEK per tag — whose GROUP BY
    // keeps the works that matched all n DISTINCT keys (DISTINCT because two display names can
    // slug to one key on one work; see `tags::rebuild_tag_table`). SQLite materializes the
    // work-id list once and probes it per candidate row, so the cost is proportional to the
    // tags' postings, never a scan of `work_tag`. With one tag the GROUP BY is dropped.
    //
    // INCLUDE-OR: "carries ANY". A correlated `EXISTS` through
    // `idx_work_tag_work_key(work_id, tag_key)` — one covering probe per candidate row, which
    // is what an ANY-match over potentially huge tags ("Action": ~40% of the catalogue) wants;
    // a materialized list of every Action work would be the larger structure.
    //
    // EXCLUDE-OR: drop works carrying ANY → `NOT EXISTS` over the same probe.
    // EXCLUDE-AND: drop only works carrying ALL → `NOT IN` the include-AND list.
    //
    // An id no work carries (unknown, or the `UNMATCHABLE` sentinel) needs no special case:
    // under include-AND nothing has it so nothing matches; under include-OR it adds nothing;
    // excluded, it excludes nothing. `browse::tests::tag_filters_use_the_tag_indexes` pins all
    // four plans.
    fn all_of_list(start: usize, n: usize) -> String {
        if n == 1 {
            format!(
                "SELECT t.work_id FROM work_tag t WHERE t.tag_key = {}",
                crate::sql_params::placeholders(start, 1)
            )
        } else {
            format!(
                "SELECT t.work_id FROM work_tag t WHERE t.tag_key IN ({}) \
                 GROUP BY t.work_id HAVING COUNT(DISTINCT t.tag_key) = {n}",
                crate::sql_params::placeholders(start, n)
            )
        }
    }
    fn any_probe(start: usize, n: usize) -> String {
        format!(
            "SELECT 1 FROM work_tag t WHERE t.work_id = f.work_id AND t.tag_key IN ({})",
            crate::sql_params::placeholders(start, n)
        )
    }
    if !q.include_tags.is_empty() {
        let n = q.include_tags.len();
        parts.push(match q.include_mode {
            TagMode::And => format!("f.work_id IN ({})", all_of_list(start + binds.len(), n)),
            TagMode::Or => format!("EXISTS ({})", any_probe(start + binds.len(), n)),
        });
        binds.extend(q.include_tags.iter().cloned().map(Bind::Text));
    }
    if !q.exclude_tags.is_empty() {
        let n = q.exclude_tags.len();
        parts.push(match q.exclude_mode {
            TagMode::And if n > 1 => {
                format!("f.work_id NOT IN ({})", all_of_list(start + binds.len(), n))
            }
            // One tag: "all of {x}" and "any of {x}" are the same set; the probe is cheaper.
            TagMode::And | TagMode::Or => {
                format!("NOT EXISTS ({})", any_probe(start + binds.len(), n))
            }
        });
        binds.extend(q.exclude_tags.iter().cloned().map(Bind::Text));
    }
    // v1 genre ANY-match, by display NAME. Deliberately NO `t.source` predicate, unlike
    // `catalog::work_effective_genres`, which applies admin-over-upstream-over-source tier
    // PRECEDENCE because it is choosing what to DISPLAY. Filtering is a different question:
    // a work should match any tag it carries, on any tier, or a work whose admin curation
    // omitted "Action" would vanish from the Action filter while still being an action
    // series. Omitting the predicate also lets `idx_work_tag_tag(tag, work_id)` (migration
    // 0066) serve the seek covering — with `source` in the mix the planner has to fall back
    // to the (work_id, source) index and re-check the tag.
    //
    // `work_tag`'s PRIMARY KEY is (work_id, tag), so a work cannot carry a tag twice and
    // `EXISTS` needs no DISTINCT.
    let genres = canonical_genres(q.genres);
    if !genres.is_empty() {
        parts.push(format!(
            "EXISTS (SELECT 1 FROM work_tag t WHERE t.work_id = f.work_id AND t.tag IN ({}))",
            crate::sql_params::placeholders(start + binds.len(), genres.len())
        ));
        for g in &genres {
            binds.push(Bind::Text(g.clone()));
        }
    }
    // The pre-existing `minRating`/`maxRating` bounds. These are the ONE predicate that
    // costs a non-index-only COUNT (a correlated lookup into the review aggregate per
    // candidate row), which is why they are gated on being present rather than always
    // emitted: `reviews` holds 6 rows in production, so essentially no request sends them,
    // and the memo absorbs the ones that do. `COALESCE(…, 0)` treats unrated as 0, which is
    // what the old `search_catalogue` did — so `minRating > 0` still excludes unrated works
    // and `maxRating` still includes them.
    let mut uses_rated = false;
    for (bound, op) in [(q.min_rating, ">="), (q.max_rating, "<=")] {
        if let Some(v) = bound {
            uses_rated = true;
            parts.push(format!(
                "COALESCE((SELECT rp.avg FROM rated_per_work rp WHERE rp.wid = f.work_id), 0) {op} {}", crate::sql_params::placeholders(start + binds.len(), 1)
            ));
            binds.push(Bind::Real(v));
        }
    }

    let where_sql = if parts.is_empty() {
        "TRUE".to_string()
    } else {
        parts.join(" AND ")
    };
    (where_sql, binds, uses_rated)
}

/// The ORDER BY for a single-phase column sort: the key, then `f.work_id`, both in the
/// requested direction (invariant 3) — so every shape walks one `(…, key DESC, work_id DESC)`
/// index, forwards for DESC and backwards for ASC, and never sorts.
///
/// `released_at DESC` puts the 67,000 works with a NULL `released_at` LAST, because SQLite
/// treats NULL as smaller than every value — so no `NULLS LAST` clause (which SQLite only
/// gained in 3.30 and which would defeat the index anyway) is needed. VERIFIED on a
/// 115,567-row copy of production: the first row is dated, the last is NULL, the 30-row
/// window straddling row 48,567 is 15 dated then 15 NULL, and the plan at that boundary is
/// still `SEARCH … USING COVERING INDEX idx_bc_order` with no temp B-tree. The same holds for
/// every nullable key under DESC; under ASC the NULLs would come FIRST, which is why
/// [`browse_catalogue`] runs a nullable ASC sort through [`nulls_last_page`] instead.
///
/// LATEST_ADDED (`created_at`) is the WORK clock, not the chapter clock. `announced_at`
/// (0101's key) is `MAX(COALESCE(announce_at, first_seen_at))` over the work's release
/// events, so every new chapter of an already-tracked series bumps it to ~now — the /updates
/// order, which filled "Latest added" with actively-updating (and therefore trending) works
/// while genuinely new intake sat pages deep. `created_at` is `work.created_at` verbatim:
/// when the work entered OUR catalogue, the same field `Series.createdAt` and the reader's
/// `addedAt` resolve from. PostgreSQL orders its native timestamptz value, and
/// the API emits C10 through the shared decoder. It is NOT NULL on every row.
fn order_by(col: &str, desc: bool) -> String {
    let d = dir(desc);
    format!("{col} {d} NULLS LAST, f.work_id {d}")
}

/// Run one page statement.
// Internal helper mirroring the SQL builder's parameter list one-to-one; folding
// the arguments into a struct would just move the same eight names one level down.
#[allow(clippy::too_many_arguments)]
async fn fetch_page(
    pool: &Database,
    cte: bool,
    where_sql: &str,
    extra: &str,
    binds: &[Bind],
    order: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<BrowseRow>> {
    let prelude = if cte { RATED_PER_WORK_CTE } else { "" };
    let page_args = crate::sql_params::placeholders(binds.len() + 1, 2);
    let mut page_args = page_args.split(", ");
    let limit_arg = page_args.next().unwrap();
    let offset_arg = page_args.next().unwrap();
    let sql = format!(
        "{prelude}SELECT {BROWSE_COLS} FROM browse_catalogue f \
         WHERE {where_sql}{extra} ORDER BY {order} LIMIT {limit_arg} OFFSET {offset_arg}"
    );
    let q = bind_rows(sqlx::query_as::<_, BrowseRow>(&sql), binds);
    Ok(q.bind(limit).bind(offset).fetch_all(pool).await?)
}

/// Fetch `browse_catalogue` rows for an explicit, already-ordered list of work ids,
/// **preserving the caller's order**. Used by text search, which ranks PostgreSQL title/alias candidates and
/// then needs the same card payload Browse renders — above all `reader_id`, without which a
/// Suwayomi-only hit would be handed to the client as a bare `w_…` that `canonicalSeries`
/// rejects (see `graphql::map_browse_rows`).
///
/// One indexed `IN` over the PRIMARY KEY, then a reorder in Rust for at most
/// `BROWSE_PAGE_SIZE` rows.
///
/// A search work with NO `browse_catalogue` row is DROPPED rather than rendered from
/// a fallback. Both tables are rebuilt from the same predicate in the same chain
/// (`refresh_feed_updates`), so a miss means the work lost its last source or its title
/// between the two rebuilds — exactly the shell Browse also refuses to show. Silent by
/// design: the caller logs the count difference.
pub async fn rows_by_work_ids(pool: &Database, ids: &[String]) -> Result<Vec<BrowseRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {BROWSE_COLS} FROM browse_catalogue f WHERE f.work_id IN ({})",
        crate::sql_params::placeholders(1, ids.len())
    );
    let mut q = sqlx::query_as::<_, BrowseRow>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    let mut by_id: std::collections::HashMap<String, BrowseRow> = q
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| (r.work_id.clone(), r))
        .collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Browse v2's TEXT search: full-text matches, narrowed by the SAME WHERE Browse uses, ranked
/// by RELEVANCE unless another sort was asked for. Returns `(total, page)`.
///
/// WHY THIS EXISTS. `search` used to send any non-empty query to
/// `catalog::search_works_fts`, which knows nothing about filters, and dropped every filter
/// on the floor — so "romance, completed, Korean" plus a title word returned every title
/// match. Applying filters AFTER the fetch would have corrupted `total` and pagination (a
/// 30-row page filtered down to 4, a pager promising pages that do not exist), which is why
/// that path refused to filter at all. Here the filter is part of the candidate query, so
/// `total` counts exactly what the pages walk.
///
/// THE SHAPE. `m` is the FTS match set with its rank, already narrowed: [`build_where`]'s
/// clauses — written against `f` — go VERBATIM into the eligibility SELECT that
/// `search::match_ctes` probes by primary key for each ranked work (one WHERE for Browse and
/// search, invariant 1). `m` carries every `f` column, so the statements read it under the
/// alias `f` and join nothing again. The FTS index drives: a match set is small next to the
/// catalogue, and each candidate costs one PK probe plus the filter's own probes.
///
/// RELEVANCE keeps `search_works_fts`' ranking exactly — the title tier
/// (`catalog::fts_title_tier`, shared, not copied), then `chapters DESC`, then bm25, then
/// `work_id` — over the same bounded window of the `RANK_WINDOW` bm25-best candidates, but
/// the window is now taken AFTER the filter, so a narrow filter still fills its pages from
/// deeper in the bm25 order. `total` is capped at the window, as before, so `hasNextPage`
/// never promises a page past it. Direction is ignored for RELEVANCE (there is no "least
/// relevant first").
///
/// ANY OTHER SORT re-orders the whole filtered match set — no window, because "the 500
/// bm25-best, by follows" is not "by follows" — and `total` is the full filtered match count.
/// That sorts the matches through a temp B-tree, which is bounded by the match set (the
/// 3-character query floor keeps the worst case to tens of thousands of rows) rather than by
/// the catalogue. NULL keys sort last in both directions, as in Browse.
///
/// NO MATCH AT ALL runs one more statement, `search::retry_ctes`, with the same filter,
/// window, sorts and binds: a misspelt term that is a word of other titles, and a query
/// written without its spaces. A query with any match never runs it. `total` and the page
/// come from the same statement, so they agree.
///
/// OVER BUDGET. A first statement that runs out of the search budget is run once more
/// without typo matching (`search::plain_ctes`), as in `catalog::search_works_fts` and for
/// the reason given there. Its total has its own memo tag, so it is never read as the
/// total of the full statement. A retry statement that runs out of its budget is no error:
/// the answer is the first statement's, nothing. One request runs at most five statements
/// and they take at most `catalog::SEARCH_ALLOWANCE` together, as there.
///
/// NSFW: [`build_where`]'s gate on the materialized flag, PLUS the live
/// `COALESCE(is_nsfw_override, is_nsfw)` on `work` that `search_works_fts` always applied — an
/// override written since the last Browse rebuild is honoured here immediately, and the
/// resolver's `filter_nsfw` backstop still runs after hydration.
pub async fn search_text(
    pool: &Database,
    raw_query: &str,
    q: &BrowseQuery<'_>,
) -> Result<(i64, Vec<BrowseRow>)> {
    if !crate::search::valid_query(raw_query) {
        return Ok((0, Vec::new()));
    }
    // Resolve optional view rankings before owning a reader: this helper acquires from
    // the pool itself and would deadlock with a single-reader pool otherwise.
    let trending = if q.sort.key == SortKey::MostViewed {
        Some(trending_work_ids(pool, TRENDING_KEY_LIMIT).await)
    } else {
        None
    };
    let full = crate::search::match_ctes;
    let connection = crate::catalog::SearchConnection::new(pool).await?;
    match text_pass(connection, raw_query, q, trending.clone(), full, "fts").await? {
        Ok(found) => Ok(found),
        Err((error, left)) => {
            tracing::warn!(%error, "search: over budget, answering without typo matching");
            let plain = crate::search::plain_ctes;
            let connection = crate::catalog::SearchConnection::with_allowance(pool, left).await?;
            let found = text_pass(connection, raw_query, q, trending, plain, "fts-plain").await?;
            Ok(found.map_err(|(error, _)| error)?)
        }
    }
}

/// One text search on its reader: the `first` statement with its memo tag, then the retry
/// statement when that finds nothing. The inner error is a statement built from `first`
/// that ran out of its budget, with what the request has left; the reader is closed by then.
async fn text_pass(
    mut connection: crate::catalog::SearchConnection,
    raw_query: &str,
    q: &BrowseQuery<'_>,
    trending: Option<Vec<String>>,
    first: fn(&str, &str) -> String,
    first_memo: &str,
) -> Result<std::result::Result<(i64, Vec<BrowseRow>), (sqlx::Error, std::time::Duration)>> {
    let match_expr = raw_query.to_owned();
    let (where_sql, where_binds, uses_rated) = build_where_from(q, 2);
    let page_size = BROWSE_PAGE_SIZE;
    let page = q.page.max(1);
    let offset = (page - 1) * page_size;
    let window = crate::catalog::RANK_WINDOW;
    let gate = if q.show_nsfw {
        ""
    } else {
        " AND NOT COALESCE(w.is_nsfw_override, w.is_nsfw)"
    };
    let relevance = q.sort.key == SortKey::Relevance;
    let community = q.sort.key == SortKey::CommunityRating;
    let rated_def = if uses_rated || community {
        format!("{RATED_PER_WORK_DEF}, ")
    } else {
        String::new()
    };
    let eligible = "SELECT f.* FROM browse_catalogue f JOIN work w ON w.id=f.work_id";
    let mut head: Vec<Bind> = vec![Bind::Text(match_expr.clone())];
    head.extend(where_binds.iter().cloned());

    // total. The memo key carries the MATCH expression (length-prefixed — it is client text)
    // and the window flag, on top of the filter signature.
    //
    // The first statement, then `search::retry_ctes` when that one counts nothing: a
    // misspelt term that is a word elsewhere, a query without its spaces. `ctes` ends as
    // the statement `total` was counted with, and the page below is built with the same
    // one, so the two agree. Each has its own memo entry. `in_first` is true while the
    // statement in hand is built from `first`.
    let mut in_first = true;
    let mut ctes = first;
    let mut m_cte = String::new();
    let mut total = 0;
    for memo in [first_memo, "fts-retry"] {
        m_cte = ctes(eligible, &format!("{where_sql}{gate}"));
        let ckey = format!(
            "{memo}|{}|{}:{}|{}",
            u8::from(relevance),
            match_expr.len(),
            match_expr,
            count_key(q)
        );
        total = match cached_count(&ckey) {
            Some(t) => t,
            None => {
                let window_arg = crate::sql_params::placeholders(head.len() + 1, 1);
                let (sql, binds) = if relevance {
                    let mut b = head.clone();
                    b.push(Bind::Int(window));
                    (
                        format!(
                            "WITH {rated_def}{m_cte} SELECT COUNT(*) FROM \
                             (SELECT 1 FROM m LIMIT {window_arg})"
                        ),
                        b,
                    )
                } else {
                    (
                        format!("WITH {rated_def}{m_cte} SELECT COUNT(*) FROM m"),
                        head.clone(),
                    )
                };
                connection.next_budget().await?;
                let counted = bind_count(sqlx::query_scalar::<_, i64>(&sql), &binds)
                    .fetch_one(&mut *connection)
                    .await;
                let t = match counted {
                    Err(error) if crate::search::over_budget(&error) => {
                        return Ok(connection.cancelled(error, in_first))
                    }
                    counted => counted?,
                };
                store_count(ckey, t);
                t
            }
        };
        if total > 0 {
            break;
        }
        in_first = false;
        ctes = crate::search::retry_ctes;
    }
    if offset >= total {
        connection.finish().await?;
        return Ok(Ok((total, Vec::new())));
    }

    if relevance {
        let window_arg = crate::sql_params::placeholders(head.len() + 1, 1);
        let page_args = crate::sql_params::placeholders(head.len() + 2, 2);
        let mut page_args = page_args.split(", ");
        let limit_arg = page_args.next().unwrap();
        let offset_arg = page_args.next().unwrap();
        let cand = crate::search::window_cte(&window_arg);
        let sql = format!(
            "WITH {rated_def}{m_cte}, {cand} SELECT {BROWSE_COLS} FROM cand f \
             ORDER BY f.tier, f.chapters DESC NULLS LAST, f.rank, f.work_id \
             LIMIT {limit_arg} OFFSET {offset_arg}"
        );
        let mut binds = head;
        binds.push(Bind::Int(window));
        connection.next_budget().await?;
        let rows = bind_rows(sqlx::query_as::<_, BrowseRow>(&sql), &binds)
            .bind(page_size.min(window))
            .bind(offset)
            .fetch_all(&mut *connection)
            .await;
        let rows = match rows {
            Err(error) if crate::search::over_budget(&error) => {
                return Ok(connection.cancelled(error, in_first))
            }
            rows => rows?,
        };
        connection.finish().await?;
        return Ok(Ok((total, rows)));
    }

    // Every other sort over the filtered match set.
    let d = dir(q.sort.desc);
    let mut extra_cte = String::new();
    let mut extra_join = String::new();
    let mut cte_binds: Vec<Bind> = Vec::new();
    let order = match q.sort.key {
        SortKey::MostViewed => {
            // The view ranking as a VALUES table of (work_id, position) — positions are our
            // own integers, formatted; the ids are bound. Unviewed works (no row) sort last
            // in both directions, then newest-first like Browse's remainder.
            let ranked = trending.unwrap_or_default();
            if ranked.is_empty() {
                NEWEST_ORDER.to_string()
            } else {
                let parameters = crate::sql_params::placeholders(2, ranked.len());
                let values = parameters
                    .split(", ")
                    .enumerate()
                    .map(|(i, parameter)| format!("({parameter}, {i})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                extra_cte = format!(", tr(wid, pos) AS (VALUES {values})");
                extra_join = " LEFT JOIN tr ON tr.wid = f.work_id".into();
                cte_binds.extend(ranked.into_iter().map(Bind::Text));
                // Most viewed = position 0 first, so DESC ranks ascending positions.
                let pd = dir(!q.sort.desc);
                format!("(tr.pos IS NULL), tr.pos {pd}, {NEWEST_ORDER}")
            }
        }
        SortKey::CommunityRating => {
            extra_join = " LEFT JOIN rated_per_work rp ON rp.wid = f.work_id".into();
            format!("(rp.avg IS NULL), rp.avg {d}, {NEWEST_ORDER}")
        }
        key => match sort_column(key) {
            Some((col, true)) => format!("({col} IS NULL), {}", order_by(col, q.sort.desc)),
            Some((col, false)) => order_by(col, q.sort.desc),
            None => NEWEST_ORDER.to_string(),
        },
    };
    let (where_sql, _, _) = build_where_from(q, 2 + cte_binds.len());
    let m_cte = ctes(eligible, &format!("{where_sql}{gate}"));
    let page_args = crate::sql_params::placeholders(2 + cte_binds.len() + where_binds.len(), 2);
    let mut page_args = page_args.split(", ");
    let limit_arg = page_args.next().unwrap();
    let offset_arg = page_args.next().unwrap();
    let sql = format!(
        "WITH {rated_def}{m_cte}{extra_cte} SELECT {BROWSE_COLS} FROM m f{extra_join} \
         ORDER BY {order} LIMIT {limit_arg} OFFSET {offset_arg}"
    );
    // Bind order follows the text: the MATCH (in `m`), the VALUES ids (in `tr`), then the
    // WHERE's binds.
    let mut binds: Vec<Bind> = vec![Bind::Text(match_expr)];
    binds.extend(cte_binds);
    binds.extend(where_binds);
    connection.next_budget().await?;
    let rows = bind_rows(sqlx::query_as::<_, BrowseRow>(&sql), &binds)
        .bind(page_size)
        .bind(offset)
        .fetch_all(&mut *connection)
        .await;
    let rows = match rows {
        Err(error) if crate::search::over_budget(&error) => {
            return Ok(connection.cancelled(error, in_first))
        }
        rows => rows?,
    };
    connection.finish().await?;
    Ok(Ok((total, rows)))
}

/// Catalogue-wide Browse. Returns `(total, page)` where `total` counts exactly the row set
/// the page query walks.
pub async fn browse_catalogue(
    pool: &Database,
    q: &BrowseQuery<'_>,
) -> Result<(i64, Vec<BrowseRow>)> {
    let (where_sql, binds, uses_rated) = build_where(q);
    let page_size = BROWSE_PAGE_SIZE;
    // `page.max(1)` is the ONLY thing standing between a hand-edited `?page=-5` and a
    // negative OFFSET, which SQLite treats as 0 — i.e. page -5 would silently serve page 1
    // while echoing `page: -5`, and the two-phase sorts below would compute a negative
    // `rated_count - offset`. Clamp once, here, and derive everything from the clamped
    // value. `updates_feed` has the same clamp and a test named for it.
    let page = q.page.max(1);
    let offset = (page - 1) * page_size;

    // total, memoized. Shares the page query's WHERE verbatim.
    let key = count_key(q);
    let total = match cached_count(&key) {
        Some(t) => t,
        None => {
            let prelude = if uses_rated { RATED_PER_WORK_CTE } else { "" };
            let sql = format!("{prelude}SELECT COUNT(*) FROM browse_catalogue f WHERE {where_sql}");
            let t = bind_count(sqlx::query_scalar::<_, i64>(&sql), &binds)
                .fetch_one(pool)
                .await?;
            store_count(key, t);
            t
        }
    };

    // The row page, memoized like the total but for [`ROW_TTL`] and per (sort, page).
    // Checked AFTER the total so a hit still returns a `total` no older than [`COUNT_TTL`]
    // — the two memos are cleared together (see [`clear_count_cache`]) so they always
    // describe the same generation of the table.
    let rkey = row_key(q);
    if let Some(rows) = cached_rows(&rkey) {
        return Ok((total, rows));
    }

    let sort = q.sort.for_browse();
    let rows = match sort.key {
        // TWO-PHASE, both of them, for the same reason: the ranking key exists for a tiny
        // minority of works ("unrated last" / "never viewed last"), which makes the ranked
        // set a strict PREFIX of the result list. The naive form — `LEFT JOIN` the
        // aggregate and `ORDER BY avg DESC` — has to sort all 48k rows through a temp
        // B-tree on every page, including the ~99.99% of pages that contain no ranked row
        // at all. Phase 1 pages the ranked prefix; phase 2 runs only when the page reaches
        // past it and pages the remainder on `released_at DESC` off an existing index.
        SortKey::CommunityRating => {
            rating_page(pool, &where_sql, &binds, sort.desc, page_size, offset).await?
        }
        SortKey::MostViewed => {
            trending_page(
                pool, &where_sql, &binds, uses_rated, sort.desc, page_size, offset,
            )
            .await?
        }
        key => {
            // `for_browse` has already turned RELEVANCE into LATEST_UPDATE; every other key
            // left is a column.
            let (col, nullable) = sort_column(key).unwrap_or(("f.released_at", true));
            if nullable && !sort.desc {
                // A nullable key ASCENDING would put its NULLs FIRST (SQLite collates NULL
                // lowest) — "fewest follows" opening on 60k works nobody has counted. Split
                // instead, exactly like the two-phase rankings: the non-NULL prefix in key
                // order off the same index walked backwards, then the NULLs.
                let ctx = PageCtx {
                    pool,
                    uses_rated,
                    where_sql: &where_sql,
                    binds: &binds,
                    count_key: &key_for_nulls(col, q),
                };
                nulls_last_page(&ctx, col, page_size, offset).await?
            } else {
                fetch_page(
                    pool,
                    uses_rated,
                    &where_sql,
                    "",
                    &binds,
                    &order_by(col, sort.desc),
                    page_size,
                    offset,
                )
                .await?
            }
        }
    };
    store_rows(rkey, rows.clone());
    Ok((total, rows))
}

/// The memo key for a nullable sort's non-NULL prefix length: the column, then the filter
/// signature. A separate entry from the total (it is a different COUNT), memoized under the
/// same TTL and cleared by the same [`clear_count_cache`], so a deep ASC page does not pay
/// two COUNTs per request.
fn key_for_nulls(col: &str, q: &BrowseQuery<'_>) -> String {
    format!("nn:{col}|{}", count_key(q))
}

/// What a phase function needs to run statements over the shared WHERE.
struct PageCtx<'a> {
    pool: &'a Database,
    uses_rated: bool,
    where_sql: &'a str,
    binds: &'a [Bind],
    count_key: &'a str,
}

/// A nullable column sorted ASCENDING with its NULLs LAST: the non-NULL rows in `col ASC,
/// work_id ASC` order, then the NULL rows in `work_id ASC` order.
///
/// Same shape and the same [`split_two_phase`] arithmetic as the rankings. Phase 1 is
/// `col IS NOT NULL` — a range bound on the key's own index, walked backwards — and phase 2
/// is its exact complement, `col IS NULL`, so no row can appear in both (the reader's grid
/// keys on the id and a duplicate kills the page; see [`rating_page`]). The prefix COUNT is
/// memoized like the total.
async fn nulls_last_page(
    ctx: &PageCtx<'_>,
    col: &str,
    page_size: i64,
    offset: i64,
) -> Result<Vec<BrowseRow>> {
    let prefix_len = match cached_count(ctx.count_key) {
        Some(n) => n,
        None => {
            let prelude = if ctx.uses_rated {
                RATED_PER_WORK_CTE
            } else {
                ""
            };
            let sql = format!(
                "{prelude}SELECT COUNT(*) FROM browse_catalogue f \
                 WHERE {} AND {col} IS NOT NULL",
                ctx.where_sql
            );
            let n = bind_count(sqlx::query_scalar::<_, i64>(&sql), ctx.binds)
                .fetch_one(ctx.pool)
                .await?;
            store_count(ctx.count_key.to_string(), n);
            n
        }
    };
    let (take_prefix, take_rest, rest_offset) = split_two_phase(prefix_len, offset, page_size);
    let mut rows = Vec::with_capacity(page_size as usize);
    if take_prefix > 0 {
        rows.extend(
            fetch_page(
                ctx.pool,
                ctx.uses_rated,
                ctx.where_sql,
                &format!(" AND {col} IS NOT NULL"),
                ctx.binds,
                &order_by(col, false),
                take_prefix,
                offset,
            )
            .await?,
        );
    }
    if take_rest > 0 {
        rows.extend(
            fetch_page(
                ctx.pool,
                ctx.uses_rated,
                ctx.where_sql,
                &format!(" AND {col} IS NULL"),
                ctx.binds,
                "f.work_id ASC",
                take_rest,
                rest_offset,
            )
            .await?,
        );
    }
    Ok(rows)
}

/// How many rows a two-phase sort should take from its ranked prefix, and how many from
/// the remainder, for a page at `offset`.
///
/// Split out because the arithmetic is the sharp edge: it must never produce a negative
/// LIMIT or a negative OFFSET, and it must not double-count the boundary page (the one
/// that starts inside the prefix and ends outside it). All three inputs are already
/// non-negative — `offset` is derived from `page.max(1)`, `prefix_len` from a COUNT — so
/// the clamps are belt-and-braces against a future caller, and they are tested directly.
fn split_two_phase(prefix_len: i64, offset: i64, page_size: i64) -> (i64, i64, i64) {
    let take_prefix = (prefix_len - offset).clamp(0, page_size);
    let take_rest = page_size - take_prefix;
    // Only meaningful when `take_rest > 0`, and then `offset >= prefix_len` OR the page
    // straddles the boundary (in which case the remainder starts at its own row 0).
    let rest_offset = (offset - prefix_len).max(0);
    (take_prefix, take_rest, rest_offset)
}

/// RATING: rated works first (best average first), then everything unrated by newest.
///
/// `rated_count` is a separate, UNMEMOIZED count — it is not `total` and must not be
/// confused with it. It is cheap by construction: the CTE aggregates `reviews` (6 rows
/// live) and the join then probes `browse_catalogue` by primary key, so the plan touches
/// the rated works only. Phase 1's `USE TEMP B-TREE FOR ORDER BY` is over THAT set (5 rows
/// today), not over the feed — which is the entire point of splitting.
async fn rating_page(
    pool: &Database,
    where_sql: &str,
    binds: &[Bind],
    desc: bool,
    page_size: i64,
    offset: i64,
) -> Result<Vec<BrowseRow>> {
    let d = dir(desc);
    let count_sql = format!(
        "{RATED_PER_WORK_CTE}SELECT COUNT(*) FROM browse_catalogue f \
         JOIN rated_per_work rp ON rp.wid = f.work_id WHERE {where_sql}"
    );
    let rated_count = bind_count(sqlx::query_scalar::<_, i64>(&count_sql), binds)
        .fetch_one(pool)
        .await?;
    let (take_rated, take_rest, rest_offset) = split_two_phase(rated_count, offset, page_size);

    let mut rows = Vec::with_capacity(page_size as usize);
    if take_rated > 0 {
        let page_args = crate::sql_params::placeholders(binds.len() + 1, 2);
        let mut page_args = page_args.split(", ");
        let limit_arg = page_args.next().unwrap();
        let offset_arg = page_args.next().unwrap();
        let sql = format!(
            "{RATED_PER_WORK_CTE}SELECT {BROWSE_COLS} FROM browse_catalogue f \
             JOIN rated_per_work rp ON rp.wid = f.work_id WHERE {where_sql} \
             ORDER BY rp.avg {d}, f.work_id {d} LIMIT {limit_arg} OFFSET {offset_arg}"
        );
        let q = bind_rows(sqlx::query_as::<_, BrowseRow>(&sql), binds);
        rows.extend(q.bind(take_rated).bind(offset).fetch_all(pool).await?);
    }
    if take_rest > 0 {
        // `NOT EXISTS` over the same CTE, so phase 2 is the EXACT complement of phase 1 —
        // no cap, no approximation, no row appearing in both. A row shared between the two
        // phases would not merely look odd: the reader keys its grid `{#each}` on the row
        // id and Svelte 5 throws `each_key_duplicate` in PRODUCTION, so a boundary bug
        // kills the page (migration 0064's header documents the same trap).
        let extra = " AND NOT EXISTS (SELECT 1 FROM rated_per_work rp WHERE rp.wid = f.work_id)";
        rows.extend(
            fetch_page(
                pool,
                true,
                where_sql,
                extra,
                binds,
                NEWEST_ORDER,
                take_rest,
                rest_offset,
            )
            .await?,
        );
    }
    Ok(rows)
}

/// TRENDING: the last 24 h's most-viewed works first, then everything else by newest.
///
/// `views::trending_keys` ranks NORMALIZED view keys, which are polymorphic exactly like
/// `reviews.series_id` — a numeric Suwayomi id when the work has such a source, else a
/// `w_…`. They are resolved to work ids here in one grouped query, because the feed and
/// everything downstream key on `work_id`.
///
/// Unlike RATING this phase is bounded ([`TRENDING_KEY_LIMIT`]) rather than exact, and that
/// is correct rather than a compromise: "trending" IS a top-N over a 24-hour window. Works
/// ranked past the bound fall into phase 2 and are ordered by newest, which is where an
/// untrending work belongs anyway. Adds NO new index — phase 1 probes the primary key and
/// phase 2 reads the newest-first index that already exists.
///
/// Production holds 43 `series_view_bucket` rows resolving to 17 works, so trending is
/// nearly empty today and almost every page is served entirely by phase 2. That is the
/// expected state, not a bug.
async fn trending_page(
    pool: &Database,
    where_sql: &str,
    binds: &[Bind],
    uses_rated: bool,
    desc: bool,
    page_size: i64,
    offset: i64,
) -> Result<Vec<BrowseRow>> {
    let mut ranked = trending_work_ids(pool, TRENDING_KEY_LIMIT).await;
    // ASCENDING = least-viewed of the VIEWED works first. The never-viewed remainder stays
    // last either way: it has no view count, and an unknown sorts last in both directions.
    if !desc {
        ranked.reverse();
    }
    if ranked.is_empty() {
        // No views recorded in the window: TRENDING degenerates to NEWEST over the whole
        // filtered set. Returning nothing would be the wrong reading of "sort by trending"
        // on a cold cache — and it is the DEFAULT sort, so it would render Browse empty.
        return fetch_page(
            pool,
            uses_rated,
            where_sql,
            "",
            binds,
            NEWEST_ORDER,
            page_size,
            offset,
        )
        .await;
    }

    // Which of the ranked works survive the filter. Membership is decided in SQL (the
    // filter is the WHERE, which must not be re-implemented in Rust), the ORDER stays in
    // Rust (it is the view ranking, which no index encodes).
    let prelude = if uses_rated { RATED_PER_WORK_CTE } else { "" };
    let sql = format!(
        "{prelude}SELECT {BROWSE_COLS} FROM browse_catalogue f \
         WHERE {where_sql} AND f.work_id IN ({})",
        crate::sql_params::placeholders(binds.len() + 1, ranked.len())
    );
    let mut q = bind_rows(sqlx::query_as::<_, BrowseRow>(&sql), binds);
    for id in &ranked {
        q = q.bind(id.clone());
    }
    let matched: Vec<BrowseRow> = q.fetch_all(pool).await?;
    let by_id: std::collections::HashMap<&str, &BrowseRow> =
        matched.iter().map(|r| (r.work_id.as_str(), r)).collect();
    let prefix: Vec<BrowseRow> = ranked
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).map(|r| (*r).clone()))
        .collect();

    let (take_prefix, take_rest, rest_offset) =
        split_two_phase(prefix.len() as i64, offset, page_size);
    let mut rows: Vec<BrowseRow> = prefix
        .into_iter()
        .skip(offset as usize)
        .take(take_prefix as usize)
        .collect();
    if take_rest > 0 {
        // Excludes the WHOLE resolved key list, not just the part that passed the filter:
        // the ones that failed the filter are excluded by the WHERE anyway, so the two
        // spellings agree, and this one needs no second bind list.
        let extra = format!(
            " AND f.work_id NOT IN ({})",
            crate::sql_params::placeholders(binds.len() + 1, ranked.len())
        );
        let mut rest_binds = binds.to_vec();
        for id in &ranked {
            rest_binds.push(Bind::Text(id.clone()));
        }
        rows.extend(
            fetch_page(
                pool,
                uses_rated,
                where_sql,
                &extra,
                &rest_binds,
                NEWEST_ORDER,
                take_rest,
                rest_offset,
            )
            .await?,
        );
    }
    Ok(rows)
}

/// The trending ranking as WORK ids, most-viewed first, deduped.
///
/// A view key is either a numeric Suwayomi id or a `w_…` (see `views::view_key`). Both
/// shapes can name the same work — a `w_` work with a Suwayomi source normalizes toward the
/// numeric id, but rows written before it gained that source keep the `w_` key — so the
/// resolved list is deduped, KEEPING THE FIRST (highest-ranked) occurrence.
///
/// Failures degrade to an empty ranking, i.e. Browse falls back to newest-first, rather
/// than failing the request: this is a popularity ORDERING, not a filter.
async fn trending_work_ids(pool: &Database, limit: i64) -> Vec<String> {
    let keys = crate::views::trending_keys(pool, limit)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "browse: trending ranking unavailable"))
        .unwrap_or_default();
    if keys.is_empty() {
        return Vec::new();
    }
    // One grouped lookup for every numeric key, rather than one per key.
    let numeric: Vec<&String> = keys
        .iter()
        .map(|(k, _)| k)
        .filter(|k| !k.starts_with("w_"))
        .collect();
    let mut work_of_numeric: std::collections::HashMap<String, String> = Default::default();
    if !numeric.is_empty() {
        let sql = format!(
            "SELECT source_key, work_id FROM source_series \
             WHERE source_type = 'suwayomi' AND source_key IN ({})",
            crate::sql_params::placeholders(1, numeric.len())
        );
        let mut q = sqlx::query_as::<_, (String, String)>(&sql);
        for k in &numeric {
            q = q.bind(*k);
        }
        for (k, w) in q
            .fetch_all(pool)
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "browse: trending key resolve failed"))
            .unwrap_or_default()
        {
            work_of_numeric.insert(k, w);
        }
    }
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut out = Vec::with_capacity(keys.len());
    for (k, _) in &keys {
        let wid = if k.starts_with("w_") {
            Some(k.clone())
        } else {
            work_of_numeric.get(k).cloned()
        };
        if let Some(wid) = wid {
            if seen.insert(wid.clone()) {
                out.push(wid);
            }
        }
    }
    out
}

/// The genre facets Browse renders as filter chips, from the materialized
/// `feed_genre_facet` table (migration 0068), most common first then alphabetical.
///
/// GATED BY THE VIEWER'S NSFW POSTURE, which the old `series_cache::genre_facets` was not:
/// it JSON-parsed all 13,847 `suwayomi_series.genre` blobs on every request with no gate at
/// all. `safe_count` is the count over `is_nsfw = 0` rows, so an opted-out viewer's chips
/// carry the number they will actually get back.
///
/// `WHERE {col} > 0` drops a tag that only NSFW works carry. Offering it to an opted-out
/// viewer would render a chip labelled "0" that returns an empty page — the facet's whole
/// purpose is that its number equals the filter's result count.
pub async fn genre_facets(pool: &Database, show_nsfw: bool) -> Result<Vec<(String, i64)>> {
    // One of two hard-coded identifiers, never input.
    let col = if show_nsfw { "all_count" } else { "safe_count" };
    let sql = format!(
        r#"SELECT tag, {col} FROM feed_genre_facet WHERE {col} > 0 ORDER BY {col} DESC NULLS LAST, tag ASC NULLS FIRST"#
    );
    Ok(sqlx::query_as::<_, (String, i64)>(&sql)
        .fetch_all(pool)
        .await?)
}

/// One source the `source` filter can name, with the number of Browse works it carries.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SourceFacet {
    /// What [`parse_source`] takes: `"mangadex"`, or a Suwayomi source id.
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub id: String,
    /// The source's extension package (`source_extension`), when one is recorded. NULL
    /// for the MangaDex mirror.
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub pkg_name: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub lang: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbI64")]
    pub count: i64,
}

/// The sources Browse can filter by, biggest first. Each `count` is the number of rows
/// the same `source` filter returns for the viewer: it counts `browse_catalogue`, under
/// the same NSFW gate, so a source whose works are all adult is not offered to a viewer
/// who has not opted in.
///
/// A Suwayomi mapping with no source id (`NULL`, a backfilled row) is left out: there is no
/// id to filter by.
///
/// NOT a materialized table like the tag facets: it joins every Browse row to its
/// mappings (about 0.5 s over 115k works), so the resolver memoizes it for minutes.
pub async fn source_facets(pool: &Database, show_nsfw: bool) -> Result<Vec<SourceFacet>> {
    // One of two hard-coded fragments, never input.
    let gate = if show_nsfw {
        ""
    } else {
        "AND f.is_nsfw = FALSE"
    };
    let sql = format!(
        r#"SELECT CASE WHEN ss.source_type = 'mangadex' THEN 'mangadex' ELSE ss.source_id END AS id, MAX(e.pkg_name) AS pkg_name, MAX(e.lang) AS lang, COUNT(DISTINCT ss.work_id) AS count FROM source_series ss JOIN browse_catalogue f ON f.work_id = ss.work_id LEFT JOIN source_extension e ON e.source_id = ss.source_id AND ss.source_type = 'suwayomi' WHERE (ss.source_type = 'mangadex' OR (ss.source_type = 'suwayomi' AND ss.source_id != '')) {gate} GROUP BY 1 ORDER BY count DESC NULLS LAST, id"#
    );
    Ok(sqlx::query_as::<_, SourceFacet>(&sql)
        .fetch_all(pool)
        .await?)
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

    /// Insert one canonical work plus its `browse_catalogue` row, exactly as
    /// `catalog::refresh_browse_catalogue` would have written it.
    ///
    /// `released_at: None` is the state migration 0069 exists for: a browsable work with no
    /// dated chapter, which `feed_series_updates` cannot represent at all.
    #[allow(clippy::too_many_arguments)]
    async fn seed(
        pool: &Database,
        work_id: &str,
        title: &str,
        comic_type: &str,
        status: &str,
        content_rating: &str,
        is_nsfw: i64,
        en_chapters: i64,
        released_at: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO work (id, primary_title, status, content_rating, is_nsfw, \
                               created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(work_id)
        .bind(title)
        .bind(status)
        .bind(content_rating)
        .bind(is_nsfw != 0)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO browse_catalogue \
               (work_id, reader_id, title, comic_type, status, content_rating, \
                en_chapter_count, released_at, is_nsfw, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, '2026-01-01T00:00:00Z')",
        )
        .bind(work_id)
        .bind(work_id)
        .bind(title)
        .bind(comic_type)
        .bind(status)
        .bind(content_rating)
        .bind(en_chapters)
        .bind(crate::db_types::optional_epoch_millis(released_at).unwrap())
        .bind(is_nsfw != 0)
        .execute(pool)
        .await
        .unwrap();
    }

    fn q<'a>(sort: BrowseSort, page: i64) -> BrowseQuery<'a> {
        BrowseQuery {
            show_nsfw: true,
            sort: sort.into(),
            page,
            ..Default::default()
        }
    }

    async fn titles(pool: &Database, query: &BrowseQuery<'_>) -> (i64, Vec<String>) {
        let (t, rows) = browse_catalogue(pool, query).await.unwrap();
        (t, rows.into_iter().map(|r| r.title).collect())
    }

    /// A five-work catalogue with distinct sort keys on every axis.
    async fn fixture(pool: &Database) {
        //     id    title  type      status      rating       nsfw  ch   released
        seed(
            pool,
            "w_a",
            "A",
            "MANGA",
            "ONGOING",
            "safe",
            0,
            10,
            Some(5000),
        )
        .await;
        seed(
            pool,
            "w_b",
            "B",
            "MANHWA",
            "COMPLETED",
            "suggestive",
            0,
            40,
            Some(4000),
        )
        .await;
        seed(
            pool,
            "w_c",
            "C",
            "MANHUA",
            "HIATUS",
            "erotica",
            1,
            20,
            Some(3000),
        )
        .await;
        seed(
            pool,
            "w_d",
            "D",
            "MANGA",
            "CANCELLED",
            "safe",
            0,
            30,
            Some(2000),
        )
        .await;
        seed(
            pool,
            "w_e",
            "E",
            "MANGA",
            "UNKNOWN",
            "pornographic",
            1,
            50,
            Some(1000),
        )
        .await;
    }

    #[tokio::test]
    async fn chapter_clock_and_count_sorts_order_the_whole_catalogue() {
        let pool = pool().await;
        fixture(&pool).await;
        let (total, t) = titles(&pool, &q(BrowseSort::Newest, 1)).await;
        assert_eq!(total, 5);
        assert_eq!(t, ["A", "B", "C", "D", "E"], "newest release first");

        // Two clocks, deliberately disagreeing, so the assert below proves WHICH one the
        // sort reads. `announced_at` is the chapter clock (the /updates order, and 0101's
        // wrong key): set to imply E, C, D, A, B. `created_at` is the work clock (when the
        // work entered our catalogue): set to imply A, C, D, E, B. LatestAdded must follow
        // the WORK clock — the bug this regression-tests is exactly the opposite order.
        for (work_id, announced_at) in [
            ("w_a", Some(1000_i64)),
            ("w_b", None),
            ("w_c", Some(3000)),
            ("w_d", Some(2000)),
            ("w_e", Some(4000)),
        ] {
            sqlx::query("UPDATE browse_catalogue SET announced_at = $1 WHERE work_id = $2")
                .bind(crate::db_types::optional_epoch_millis(announced_at).unwrap())
                .bind(work_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        for (work_id, created_at) in [
            ("w_a", "2026-05-05T00:00:00.000000+00:00"),
            ("w_b", "2026-05-01T00:00:00.000000+00:00"),
            ("w_c", "2026-05-04T00:00:00.000000+00:00"),
            ("w_d", "2026-05-03T00:00:00.000000+00:00"),
            ("w_e", "2026-05-02T00:00:00.000000+00:00"),
        ] {
            sqlx::query("UPDATE browse_catalogue SET created_at = $1 WHERE work_id = $2")
                .bind(crate::db_types::date(created_at).unwrap())
                .bind(work_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let (_, t) = titles(&pool, &q(BrowseSort::LatestAdded, 1)).await;
        assert_eq!(
            t,
            ["A", "C", "D", "E", "B"],
            "catalogue-add time first, ignoring the announce clock entirely"
        );

        let (_, t) = titles(&pool, &q(BrowseSort::Chapters, 1)).await;
        assert_eq!(t, ["E", "B", "D", "C", "A"], "most chapters first");
    }

    /// The tiebreaker is mandatory: with equal sort keys the order must still be total, or
    /// LIMIT/OFFSET repeats or skips rows at a page boundary.
    #[tokio::test]
    async fn equal_sort_keys_still_produce_a_total_order() {
        let pool = pool().await;
        for i in 0..5 {
            // Every row shares BOTH sort keys.
            seed(
                &pool,
                &format!("w_{i}"),
                &format!("T{i}"),
                "MANGA",
                "ONGOING",
                "safe",
                0,
                7,
                Some(1000),
            )
            .await;
        }
        for sort in [
            BrowseSort::Newest,
            BrowseSort::LatestAdded,
            BrowseSort::Chapters,
        ] {
            let (_, t) = titles(&pool, &q(sort, 1)).await;
            assert_eq!(
                t,
                ["T4", "T3", "T2", "T1", "T0"],
                "work_id DESC must break the tie deterministically"
            );
        }
    }

    /// A work with NO dated chapter must sort LAST on the recently-updated ordering — not
    /// first, and not be dropped.
    ///
    /// This is the whole reason `browse_catalogue.released_at` is nullable where
    /// `feed_series_updates.released_at` is NOT NULL, and it is asserted rather than assumed
    /// because it rests on a SQLite collation detail: NULL compares smaller than every value,
    /// so a plain `released_at DESC` puts NULLs at the end and no `NULLS LAST` clause (which
    /// would defeat the index) is needed. Get it backwards and Browse opens on 67,000
    /// chapterless works.
    #[tokio::test]
    async fn null_released_at_sorts_last_on_the_recently_updated_order() {
        let pool = pool().await;
        fixture(&pool).await;
        // Two chapterless works, one of them with a chapter count we know from a source that
        // has no dated chapter — so this is not simply "the zero-chapter rows".
        seed(&pool, "w_f", "F", "MANGA", "ONGOING", "safe", 0, 0, None).await;
        seed(&pool, "w_g", "G", "MANGA", "ONGOING", "safe", 0, 12, None).await;
        let (total, t) = titles(&pool, &q(BrowseSort::Newest, 1)).await;
        assert_eq!(total, 7, "the chapterless works are COUNTED, not excluded");
        assert_eq!(
            t,
            ["A", "B", "C", "D", "E", "G", "F"],
            "dated works newest-first, then the undated ones by the work_id DESC tiebreak"
        );
        // The two-phase sorts page their remainder on the same key, so they inherit it.
        let (_, t) = titles(&pool, &q(BrowseSort::Trending, 1)).await;
        assert_eq!(
            &t[5..],
            ["G", "F"],
            "TRENDING's phase 2 orders by the same key"
        );
    }

    /// `hasChapters` narrows to exactly the works we know a chapter for, and `total` follows.
    ///
    /// Absent, it emits NO clause — the whole browsable catalogue, which is the default and
    /// the point of migration 0069.
    #[tokio::test]
    async fn has_chapters_narrows_to_works_with_a_known_chapter_and_total_follows() {
        let pool = pool().await;
        fixture(&pool).await;
        // A chapterless work with a KNOWN count and a dated work with an UNKNOWN one: the two
        // states are independent, so neither `released_at IS NULL` nor `en_chapter_count = 0`
        // can be used as a proxy for the other (measured in production: 11,054 feed works
        // count 0, and 1,505 non-feed works count more than 0).
        seed(&pool, "w_f", "F", "MANGA", "ONGOING", "safe", 0, 12, None).await;
        seed(
            &pool,
            "w_g",
            "G",
            "MANGA",
            "ONGOING",
            "safe",
            0,
            0,
            Some(6000),
        )
        .await;
        let ask = |has: Option<bool>| {
            let pool = &pool;
            async move {
                let mut query = q(BrowseSort::Newest, 1);
                query.has_chapters = has;
                titles(pool, &query).await
            }
        };
        assert_eq!(
            ask(None).await.0,
            7,
            "absent means no clause, i.e. the whole browsable catalogue"
        );
        assert_eq!(
            ask(Some(true)).await,
            (6, ["A", "B", "C", "D", "E", "F"].map(String::from).to_vec()),
            "every work with a known chapter — INCLUDING the undated one, which is the \
             whole point: `hasChapters` means \"we know of a chapter\", not \"is in the \
             updates feed\". `total` counts the same set."
        );
        assert_eq!(
            ask(Some(false)).await,
            (1, vec!["G".to_string()]),
            "the complement is by count, not by date: G is dated but uncountable"
        );
    }

    /// Content-rating tiers are CUMULATIVE, and each one narrows within the viewer's NSFW
    /// gate rather than widening it.
    #[tokio::test]
    async fn content_rating_tiers_are_cumulative_and_never_widen_the_nsfw_gate() {
        let pool = pool().await;
        fixture(&pool).await;
        async fn tier(
            pool: &Database,
            r: Option<&'static [&'static str]>,
            nsfw: bool,
        ) -> (i64, Vec<String>) {
            let mut query = q(BrowseSort::Newest, 1);
            query.ratings = r;
            query.show_nsfw = nsfw;
            titles(pool, &query).await
        }
        let tier = |r, nsfw| tier(&pool, r, nsfw);
        // Opted IN: each tier is a superset of the previous.
        assert_eq!(tier(Some(&["safe"]), true).await.1, ["A", "D"]);
        assert_eq!(
            tier(Some(&["safe", "suggestive"]), true).await.1,
            ["A", "B", "D"]
        );
        assert_eq!(
            tier(Some(&["safe", "suggestive", "erotica"]), true).await.1,
            ["A", "B", "C", "D"]
        );
        assert_eq!(tier(None, true).await.1, ["A", "B", "C", "D", "E"]);

        // Opted OUT: the EROTICA tier returns the SUGGESTIVE set — the erotica work is
        // `is_nsfw = TRUE`, and the rating filter cannot reach past the gate.
        let suggestive = tier(Some(&["safe", "suggestive"]), false).await;
        let erotica = tier(Some(&["safe", "suggestive", "erotica"]), false).await;
        assert_eq!(suggestive.1, ["A", "B", "D"]);
        assert_eq!(
            erotica, suggestive,
            "a rating tier must never widen the NSFW gate — total included"
        );
        assert_eq!(tier(None, false).await.1, ["A", "B", "D"]);
    }

    /// `NSFW_ONLY` is the complement of SAFE, not a tier — and it still cannot widen the
    /// gate, which for an opted-out viewer means an EMPTY page rather than "everything".
    #[tokio::test]
    async fn nsfw_only_selects_the_complement_and_still_cannot_widen_the_gate() {
        let pool = pool().await;
        fixture(&pool).await;
        async fn only(pool: &Database, nsfw: bool) -> (i64, Vec<String>) {
            let mut query = q(BrowseSort::Newest, 1);
            query.nsfw_only = true;
            query.show_nsfw = nsfw;
            titles(pool, &query).await
        }
        // Opted IN: exactly the works the SAFE-through-EROTICA tiers leave out, i.e. the
        // complement of the unfiltered opted-OUT set ["A", "B", "D"].
        let opted_in = only(&pool, true).await;
        assert_eq!(opted_in.1, ["C", "E"]);
        assert_eq!(
            opted_in.0, 2,
            "the memoized total must describe the filtered set, not the whole catalogue"
        );
        // Opted OUT: empty, NOT the safe set. Degrading to "everything you may see" would
        // turn "adult only" into its opposite.
        let opted_out = only(&pool, false).await;
        assert!(
            opted_out.1.is_empty() && opted_out.0 == 0,
            "NSFW_ONLY must yield an empty page for an opted-out viewer, got {opted_out:?}"
        );
    }

    #[tokio::test]
    async fn type_and_status_filters_are_indexed_equalities_over_the_whole_set() {
        let pool = pool().await;
        fixture(&pool).await;
        let mut query = q(BrowseSort::Newest, 1);
        query.types = vec!["MANGA"];
        assert_eq!(
            titles(&pool, &query).await,
            (3, vec!["A".into(), "D".into(), "E".into()])
        );
        query.statuses = vec!["ONGOING"];
        assert_eq!(titles(&pool, &query).await, (1, vec!["A".into()]));
        // A status word no row carries is empty, not unfiltered.
        query.statuses = vec!["HIATUS"];
        assert_eq!(titles(&pool, &query).await, (0, Vec::<String>::new()));
    }

    /// Genre filtering matches ANY tag on ANY tier — no `source` predicate — and `total`
    /// describes the filtered set.
    #[tokio::test]
    async fn genre_filter_matches_any_tag_on_any_tier() {
        let pool = pool().await;
        fixture(&pool).await;
        for (w, tag, source) in [
            ("w_a", "Action", "mangadex"),
            ("w_a", "Drama", "admin"),
            ("w_b", "Action", "admin"),
            ("w_c", "Romance", "mangadex"),
        ] {
            sqlx::query("INSERT INTO work_tag (work_id, tag, ord, source) VALUES ($1, $2, 0, $3)")
                .bind(w)
                .bind(tag)
                .bind(source)
                .execute(&pool)
                .await
                .unwrap();
        }
        let g = |v: Vec<&str>| v.into_iter().map(String::from).collect::<Vec<_>>();
        let mut query = q(BrowseSort::Newest, 1);
        let action = g(vec!["Action"]);
        query.genres = &action;
        // w_a matches on its mangadex tag, w_b on its admin tag — precedence is a DISPLAY
        // rule, not a filter rule.
        assert_eq!(
            titles(&pool, &query).await,
            (2, vec!["A".into(), "B".into()])
        );
        let any = g(vec!["Action", "Romance"]);
        query.genres = &any;
        assert_eq!(
            titles(&pool, &query).await,
            (3, vec!["A".into(), "B".into(), "C".into()]),
            "several genres match ANY, not ALL"
        );
    }

    /// `total` must equal a full walk of the filtered set, page by page, with no row seen
    /// twice and none missed — for every sort, including the two-phase ones.
    #[tokio::test]
    async fn count_matches_a_full_paged_walk_for_every_sort() {
        let pool = pool().await;
        // More rows than one page, so the walk crosses real boundaries.
        for i in 0..(BROWSE_PAGE_SIZE * 2 + 7) {
            seed(
                &pool,
                &format!("w_{i:04}"),
                &format!("T{i:04}"),
                if i % 3 == 0 { "MANHWA" } else { "MANGA" },
                "ONGOING",
                "safe",
                0,
                i % 11,
                Some(1000 + i),
            )
            .await;
        }
        // Two reviews so the RATING sort has a real prefix, and some views so does TRENDING.
        sqlx::query(
            r#"INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('11ac9418-0c9e-4237-9e42-3ace93d7c468','u','u@e','h','2020-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        for (i, (w, score)) in [("w_0005", 9), ("w_0060", 4)].iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) VALUES ($1, $2, '11ac9418-0c9e-4237-9e42-3ace93d7c468', $3, 'b', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#,
            )
            .bind(crate::db_types::test_uuid(&(format!("rv{i}"))))
            .bind(*w)
            .bind(*score)
            .execute(&pool)
            .await
            .unwrap();
        }
        let hour = chrono::Utc::now().timestamp() / 3600;
        for (w, v) in [("w_0009", 50i64), ("w_0011", 5)] {
            sqlx::query(
                "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES ($1, $2, $3)",
            )
            .bind(w)
            .bind(hour)
            .bind(v)
            .execute(&pool)
            .await
            .unwrap();
        }

        for sort in [
            BrowseSort::Newest,
            BrowseSort::LatestAdded,
            BrowseSort::Chapters,
            BrowseSort::Rating,
            BrowseSort::Trending,
        ] {
            let mut seen: Vec<String> = Vec::new();
            let mut total = 0;
            for page in 1..=6 {
                let (t, rows) = browse_catalogue(&pool, &q(sort, page)).await.unwrap();
                total = t;
                seen.extend(rows.into_iter().map(|r| r.work_id));
            }
            assert_eq!(
                seen.len() as i64,
                total,
                "{sort:?}: the walk must yield exactly `total` rows"
            );
            let mut uniq = seen.clone();
            uniq.sort();
            uniq.dedup();
            assert_eq!(
                uniq.len(),
                seen.len(),
                "{sort:?}: a row appeared on two pages — this is the each_key_duplicate crash"
            );
        }
    }

    /// The two-phase boundary: the page that starts inside the ranked prefix and ends
    /// outside it must contain the prefix's tail followed by the remainder's head, with no
    /// row from the prefix repeated in the remainder.
    #[tokio::test]
    async fn two_phase_sorts_stitch_the_prefix_boundary_exactly() {
        let pool = pool().await;
        for i in 0..8 {
            seed(
                &pool,
                &format!("w_{i}"),
                &format!("T{i}"),
                "MANGA",
                "ONGOING",
                "safe",
                0,
                i,
                Some(1000 + i),
            )
            .await;
        }
        sqlx::query(
            r#"INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('11ac9418-0c9e-4237-9e42-3ace93d7c468','u','u@e','h','2020-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        // Rated prefix: w_1 (avg 9) then w_3 (avg 5).
        for (i, (w, score)) in [("w_1", 9), ("w_3", 5)].iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) VALUES ($1, $2, '11ac9418-0c9e-4237-9e42-3ace93d7c468', $3, 'b', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#,
            )
            .bind(crate::db_types::test_uuid(&(format!("rv{i}"))))
            .bind(*w)
            .bind(*score)
            .execute(&pool)
            .await
            .unwrap();
        }
        // The whole 8-row set on one page: prefix, then the 6 unrated by newest.
        let (total, rows) = browse_catalogue(&pool, &q(BrowseSort::Rating, 1))
            .await
            .unwrap();
        assert_eq!(total, 8);
        let ids: Vec<String> = rows.into_iter().map(|r| r.work_id).collect();
        assert_eq!(
            ids,
            [
                "w_1", "w_3", // rated, best average first
                "w_7", "w_6", "w_5", "w_4", "w_2", "w_0" // then unrated, newest first
            ],
            "rated prefix then unrated remainder, and w_1/w_3 must not reappear"
        );
    }

    /// The two-phase offset arithmetic must never produce a negative LIMIT or OFFSET.
    /// `updates_feed` has the equivalent guard and a test named for it; this is the
    /// arithmetic version, which the resolver-level clamp cannot cover on its own.
    #[test]
    fn two_phase_split_never_yields_a_negative_limit_or_offset() {
        for prefix in [0i64, 1, 5, 30, 31, 1000] {
            for offset in [0i64, 30, 60, 990, 100_000] {
                let (take, rest, rest_offset) = split_two_phase(prefix, offset, 30);
                assert!(
                    take >= 0 && rest >= 0 && rest_offset >= 0,
                    "{prefix}/{offset}"
                );
                assert_eq!(take + rest, 30, "the two phases must fill exactly one page");
                assert!(take <= 30 && rest <= 30);
            }
        }
        // A prefix that swallows the page leaves nothing for the remainder...
        assert_eq!(split_two_phase(1000, 0, 30), (30, 0, 0));
        // ...and a page past the prefix takes nothing from it and offsets the remainder by
        // exactly the prefix length.
        assert_eq!(split_two_phase(5, 30, 30), (0, 30, 25));
        // The straddling page: 5 rated rows, page starts at 3 → 2 rated + 28 unrated from
        // the remainder's row 0 (NOT from row -2).
        assert_eq!(split_two_phase(5, 3, 30), (2, 28, 0));
        // A negative offset can only arrive from a caller that skipped the `page.max(1)`
        // clamp; it must still not produce a negative OFFSET.
        assert_eq!(split_two_phase(5, -30, 30), (30, 0, 0));
    }

    /// An over-range page returns no rows while `total` keeps describing the whole set, and
    /// page 0 / a negative page clamp to 1 — never a negative OFFSET.
    #[tokio::test]
    async fn over_range_and_negative_pages_are_safe() {
        let pool = pool().await;
        fixture(&pool).await;
        for sort in [
            BrowseSort::Newest,
            BrowseSort::LatestAdded,
            BrowseSort::Chapters,
            BrowseSort::Rating,
            BrowseSort::Trending,
        ] {
            let (total, rows) = browse_catalogue(&pool, &q(sort, 9999)).await.unwrap();
            assert_eq!(total, 5, "{sort:?}");
            assert!(rows.is_empty(), "{sort:?}: over-range page must be empty");
            for page in [0, -5] {
                let (_, rows) = browse_catalogue(&pool, &q(sort, page)).await.unwrap();
                assert_eq!(rows.len(), 5, "{sort:?} page {page} must clamp to page 1");
            }
        }
    }

    /// The rating aggregate must cover BOTH shapes of `reviews.series_id`. Keying on the
    /// canonical id alone loses every review filed under a work's numeric Suwayomi id — 2
    /// of the 5 readable production ratings.
    #[tokio::test]
    async fn rating_sort_sees_reviews_filed_under_either_key_shape() {
        let pool = pool().await;
        fixture(&pool).await;
        // w_a carries a Suwayomi source; its review was filed under the NUMERIC id.
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('ss1', 'w_a', 'suwayomi', 'src', '4242', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('11ac9418-0c9e-4237-9e42-3ace93d7c468','u','u@e','h','2020-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, key, score) in [("rv1", "4242", 10), ("rv2", "w_b", 6)] {
            sqlx::query(
                r#"INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) VALUES ($1, $2, '11ac9418-0c9e-4237-9e42-3ace93d7c468', $3, 'b', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#,
            )
            .bind(crate::db_types::test_uuid(id))
            .bind(key)
            .bind(score)
            .execute(&pool)
            .await
            .unwrap();
        }
        let (_, t) = titles(&pool, &q(BrowseSort::Rating, 1)).await;
        assert_eq!(
            &t[..2],
            ["A", "B"],
            "the numeric-keyed review must rank A first; keying on reader_id alone loses it"
        );
    }

    /// `minRating`/`maxRating` are pre-existing public arguments and must keep working
    /// against the same polymorphic review keys.
    #[tokio::test]
    async fn rating_bounds_filter_and_count_the_same_set() {
        let pool = pool().await;
        fixture(&pool).await;
        sqlx::query(
            r#"INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('11ac9418-0c9e-4237-9e42-3ace93d7c468','u','u@e','h','2020-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) VALUES ('a43dbc7f-2ea6-464e-8649-c9cebb9ebc82', 'w_b', '11ac9418-0c9e-4237-9e42-3ace93d7c468', 8, 'b', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut query = q(BrowseSort::Newest, 1);
        query.min_rating = Some(5.0);
        assert_eq!(
            titles(&pool, &query).await,
            (1, vec!["B".into()]),
            "only the reviewed work clears minRating 5, and `total` agrees"
        );
        query.min_rating = None;
        query.max_rating = Some(5.0);
        let (total, t) = titles(&pool, &query).await;
        assert_eq!(total, 4, "unrated counts as 0 and so clears maxRating 5");
        assert!(!t.contains(&"B".to_string()));
    }

    /// TRENDING ranks by 24-hour views and resolves both view-key shapes, then falls
    /// through to newest for everything unviewed.
    #[tokio::test]
    async fn trending_ranks_viewed_works_first_across_both_key_shapes() {
        let pool = pool().await;
        fixture(&pool).await;
        // w_d is viewed under its NUMERIC Suwayomi key; w_c under its canonical key.
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('ss1', 'w_d', 'suwayomi', 'src', '99', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let hour = chrono::Utc::now().timestamp() / 3600;
        for (key, views) in [("99", 100i64), ("w_c", 10)] {
            sqlx::query(
                "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES ($1, $2, $3)",
            )
            .bind(key)
            .bind(hour)
            .bind(views)
            .execute(&pool)
            .await
            .unwrap();
        }
        let (total, t) = titles(&pool, &q(BrowseSort::Trending, 1)).await;
        assert_eq!(total, 5);
        assert_eq!(
            t,
            ["D", "C", "A", "B", "E"],
            "most-viewed first (numeric key resolved to its work), then newest"
        );
    }

    /// With no views recorded, TRENDING must degenerate to NEWEST rather than to an empty
    /// page — it is the DEFAULT sort, so an empty answer renders Browse blank.
    #[tokio::test]
    async fn trending_with_no_views_falls_back_to_newest() {
        let pool = pool().await;
        fixture(&pool).await;
        assert_eq!(
            titles(&pool, &q(BrowseSort::Trending, 1)).await,
            titles(&pool, &q(BrowseSort::Newest, 1)).await
        );
    }

    /// `total` is memoized per filter signature, so paging through Browse pays the count
    /// once rather than once per page.
    #[test]
    fn browse_count_is_memoized_per_filter_signature() {
        let ttl = std::time::Duration::from_secs(60);
        let anon = "memo-test|v2|0|||||";
        let opted_in = "memo-test|v2|1|||||";
        assert_eq!(cached_count_with(anon, ttl), None, "cold");
        store_count(anon.to_string(), 43_730);
        assert_eq!(cached_count_with(anon, ttl), Some(43_730));
        assert_eq!(cached_count_with(opted_in, ttl), None);
        // An expired entry is ignored rather than served stale.
        assert_eq!(cached_count_with(anon, std::time::Duration::ZERO), None);
        // And the rebuild's wholesale invalidation really drops it.
        clear_count_cache();
        assert_eq!(cached_count_with(anon, ttl), None);
    }

    /// The memo key must separate EVERY input the COUNT depends on, and must NOT separate
    /// `sort` or `page` (all four sorts share one WHERE, so they share one total — keying
    /// on sort would quarter the hit rate for nothing). Nothing end-to-end can catch a gap
    /// here: `COUNT_TTL` is zero under `cfg(test)`, so `browse_catalogue` never reads the
    /// memo. Test the key directly.
    #[test]
    fn count_key_separates_every_filter_dimension_but_not_sort_or_page() {
        let action = vec!["Action".to_string()];
        let base = count_key(&q(BrowseSort::Newest, 1));
        let with = |f: &dyn Fn(&mut BrowseQuery<'_>)| {
            let mut c = q(BrowseSort::Newest, 1);
            f(&mut c);
            count_key(&c)
        };
        let mut variants = vec![
            with(&|c| c.show_nsfw = false),
            with(&|c| c.ratings = Some(&["safe"])),
            with(&|c| c.ratings = Some(&["safe", "suggestive"])),
            with(&|c| c.types = vec!["MANGA"]),
            with(&|c| c.types = vec!["MANHWA"]),
            with(&|c| c.statuses = vec!["ONGOING"]),
            with(&|c| c.statuses = vec!["COMPLETED"]),
            with(&|c| c.min_rating = Some(3.0)),
            with(&|c| c.max_rating = Some(3.0)),
            // `hasChapters` is ONE WHERE clause, so it is the easiest dimension to forget —
            // and forgetting it serves the 115,567-row total to a viewer looking at the
            // 39,018-row filtered set, i.e. a pager offering 3,853 pages of a 1,301-page
            // result. All three states must be distinct, `Some(false)` included.
            with(&|c| c.has_chapters = Some(true)),
            with(&|c| c.has_chapters = Some(false)),
        ];
        variants.push({
            let mut c = q(BrowseSort::Newest, 1);
            c.genres = &action;
            count_key(&c)
        });
        for v in &variants {
            assert_ne!(&base, v, "a filter dimension collapsed");
        }
        let mut all: Vec<String> = variants.clone();
        all.push(base.clone());
        let before = all.len();
        all.sort();
        all.dedup();
        assert_eq!(before, all.len(), "two distinct filter sets share a key");

        // Rating bounds are positionally distinct: "min 3, no max" is not "no min, max 3".
        assert_ne!(
            with(&|c| c.min_rating = Some(3.0)),
            with(&|c| c.max_rating = Some(3.0))
        );
        // Sort and page must NOT appear.
        for sort in [
            BrowseSort::LatestAdded,
            BrowseSort::Chapters,
            BrowseSort::Rating,
            BrowseSort::Trending,
        ] {
            assert_eq!(
                base,
                count_key(&q(sort, 1)),
                "sort must not be part of the key — the WHERE is identical"
            );
        }
        assert_eq!(base, count_key(&q(BrowseSort::Newest, 17)));
    }

    /// Genre filters are client-supplied, so the genre segment of the memo key must be
    /// injective — otherwise a crafted genre name serves another filter's `total`, and with
    /// it the wrong page count / `hasNext`.
    ///
    /// The old key could join escaped LIKE PATTERNS with `\u{1}` and rely on `%` being
    /// escaped to make the boundary unforgeable. This key holds RAW tags, where no
    /// character is off-limits, so ANY separator would be forgeable — `["a","b"]` vs
    /// `["a<sep>b"]`. The length prefix is what replaces that argument.
    #[test]
    fn genre_names_bound_sql_binds_and_memo_keys_even_for_uncanonicalized_queries() {
        let huge = vec!["界".repeat(100_000), "Action".to_string()];
        assert_eq!(
            canonical_genres(&huge),
            vec![UNMATCHABLE.to_string(), "Action".to_string()]
        );
        assert!(canonical_genres(&["界".repeat(42)])[0].len() <= MAX_GENRE_NAME_BYTES);
        assert_eq!(
            canonical_genres(&["界".repeat(43)]),
            vec![UNMATCHABLE.to_string()]
        );
        let q = BrowseQuery {
            genres: &huge,
            ..Default::default()
        };
        let key = count_key(&q);
        assert!(
            key.len() < 512,
            "genre memo key must be bounded: {}",
            key.len()
        );
        let (_, binds, _) = build_where(&q);
        assert!(binds.iter().all(|bind| match bind {
            Bind::Text(value) => value.len() <= MAX_GENRE_NAME_BYTES,
            _ => true,
        }));
        assert!(binds
            .iter()
            .any(|bind| matches!(bind, Bind::Text(value) if value == UNMATCHABLE)));
    }

    #[test]
    fn count_key_cannot_be_forged_by_a_crafted_genre_name() {
        let k = |v: &[&str]| {
            let owned = canonical_genres(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            let mut c = q(BrowseSort::Newest, 1);
            c.genres = &owned;
            count_key(&c)
        };
        // No separator can be smuggled into a name, because there is none.
        assert_ne!(k(&["a", "b"]), k(&["a\u{1}b"]));
        assert_ne!(k(&["a", "b"]), k(&["a1:b"]));
        assert_ne!(k(&["a", "b"]), k(&["1:a1:b"]));
        // A name containing the key's own field separator cannot shift a later field,
        // because the genre segment is LAST.
        assert_ne!(k(&["a|b"]), k(&["a", "b"]));
        // Multi-byte tags: the prefix counts BYTES, which is what the decoder consumes.
        assert_ne!(k(&["é", "b"]), k(&["éb"]));
        // Canonicalization: trimmed, blanks dropped, deduped, and ORDER-INSENSITIVE (an
        // ANY-match's result set does not depend on the order the chips arrive in, so two
        // orders must share one memo entry).
        assert_eq!(k(&["Action"]), k(&["  Action  ", "", "   ", "Action"]));
        assert_eq!(k(&["a", "b"]), k(&["b", "a"]));
    }

    /// The count memo is capped, and the cap must not be a client-triggerable flush of LIVE
    /// entries: genre/tier/type/status combinations come from the request, so an attacker
    /// who can cycle `COUNT_CACHE_CAP` distinct signatures could otherwise evict the hot
    /// Browse key on demand and force the full COUNT on every page load. Expired first.
    #[test]
    fn count_cache_evicts_expired_entries_before_live_ones() {
        let ttl = std::time::Duration::from_secs(60);
        let hot = "evict-test|hot";
        for i in 0..COUNT_CACHE_CAP + 8 {
            store_count_with(format!("evict-test|cold-{i}"), i as i64, ttl);
        }
        store_count_with(hot.to_string(), 12_345, ttl);
        assert_eq!(cached_count_with(hot, ttl), Some(12_345));
        for i in 0..COUNT_CACHE_CAP {
            store_count_with(format!("evict-test|flood-{i}"), i as i64, ttl);
        }
        let len = COUNT_CACHE.lock().unwrap().len();
        assert!(len <= COUNT_CACHE_CAP, "memo must stay bounded (len {len})");
        clear_count_cache();
    }

    /// A minimal row for exercising the ROW memo directly (its TTL is zero under test, so
    /// `browse_catalogue` never reads it end-to-end — same situation as the count memo).
    fn mem_row(id: &str) -> BrowseRow {
        BrowseRow {
            work_id: id.to_string(),
            reader_id: id.to_string(),
            title: id.to_string(),
            cover_url: None,
            suwayomi_thumbnail: None,
            comic_type: None,
            status: None,
            en_chapter_count: 0,
            released_at: None,
            is_nsfw: false,
            // Migration 0095. The row memo is keyed and sized independently of this
            // column, so `None` keeps the fixture minimal without weakening the test.
            latest_chapter: None,
            created_at: "2026-01-01T00:00:00.000000+00:00".into(),
        }
    }

    /// The row key must separate everything the count key separates PLUS sort and page —
    /// the two dimensions `count_key` deliberately excludes, because a sort reorders the
    /// same set and a page windows it, both of which change the rows but not the total.
    #[test]
    fn row_key_separates_sort_and_page_on_top_of_every_count_dimension() {
        let base = row_key(&q(BrowseSort::Newest, 1));
        // Sort IS a row-key dimension (unlike the count key, which its own test pins).
        for sort in [
            BrowseSort::LatestAdded,
            BrowseSort::Chapters,
            BrowseSort::Rating,
            BrowseSort::Trending,
        ] {
            assert_ne!(base, row_key(&q(sort, 1)), "{sort:?} must get its own page");
        }
        // Page too.
        assert_ne!(base, row_key(&q(BrowseSort::Newest, 2)));
        // …but clamped exactly as `browse_catalogue` clamps it: page 0 and a negative page
        // serve page 1, so they must SHARE page 1's entry rather than duplicate it.
        assert_eq!(base, row_key(&q(BrowseSort::Newest, 0)));
        assert_eq!(base, row_key(&q(BrowseSort::Newest, -5)));
        // Every filter dimension flows through from the count key by construction; spot-check
        // one so a future rewrite away from `count_key` cannot silently drop them.
        let mut narrowed = q(BrowseSort::Newest, 1);
        narrowed.has_chapters = Some(true);
        assert_ne!(base, row_key(&narrowed));
        // The genre segment stays last and length-prefixed; sort/page live in FRONT of the
        // filter signature, so a crafted genre cannot reach them.
        let forged = vec!["x|2".to_string()];
        let mut with_genre = q(BrowseSort::Newest, 1);
        with_genre.genres = &forged;
        let plain = vec!["x".to_string()];
        let mut with_plain = q(BrowseSort::Newest, 2);
        with_plain.genres = &plain;
        assert_ne!(row_key(&with_genre), row_key(&with_plain));
    }

    /// Row pages are memoized per (sort, page, filter signature), expire on their TTL, and
    /// are dropped by the SAME clear that drops the totals — the two memos must always
    /// describe one generation of the table.
    #[test]
    fn browse_rows_are_memoized_and_cleared_together_with_the_totals() {
        let ttl = std::time::Duration::from_secs(60);
        let k1 = "rows-test|newest|1";
        let k2 = "rows-test|newest|2";
        assert!(cached_rows_with(k1, ttl).is_none(), "cold");
        store_rows_with(k1.to_string(), vec![mem_row("w_1"), mem_row("w_2")], ttl);
        let hit = cached_rows_with(k1, ttl).expect("stored page must be served");
        assert_eq!(
            hit.iter().map(|r| r.work_id.as_str()).collect::<Vec<_>>(),
            ["w_1", "w_2"]
        );
        assert!(cached_rows_with(k2, ttl).is_none(), "pages do not bleed");
        // An expired entry is ignored rather than served stale.
        assert!(cached_rows_with(k1, std::time::Duration::ZERO).is_none());
        // The rebuild's invalidation drops rows and totals in ONE call.
        store_count(format!("{k1}|count"), 42);
        clear_count_cache();
        assert!(cached_rows_with(k1, ttl).is_none());
        assert_eq!(cached_count_with(&format!("{k1}|count"), ttl), None);
    }

    /// The row memo is capped like the count memo, and for the same reason must evict
    /// EXPIRED entries before live ones: (genres, page) come from the request, so a client
    /// cycling distinct signatures must degrade the memo, not flush the hot page-1 entry.
    #[test]
    fn row_cache_evicts_expired_entries_before_live_ones() {
        let ttl = std::time::Duration::from_secs(60);
        let hot = "rows-evict-test|hot";
        for i in 0..ROW_CACHE_CAP + 8 {
            store_rows_with(format!("rows-evict-test|cold-{i}"), vec![mem_row("w")], ttl);
        }
        store_rows_with(hot.to_string(), vec![mem_row("w_hot")], ttl);
        assert!(cached_rows_with(hot, ttl).is_some());
        for i in 0..ROW_CACHE_CAP {
            store_rows_with(
                format!("rows-evict-test|flood-{i}"),
                vec![mem_row("w")],
                ttl,
            );
        }
        let len = ROW_CACHE.lock().unwrap().len();
        assert!(len <= ROW_CACHE_CAP, "memo must stay bounded (len {len})");
        clear_count_cache();
    }

    #[tokio::test]
    async fn genre_facets_come_from_the_materialized_table_and_are_nsfw_gated() {
        let pool = pool().await;
        for (tag, safe, all) in [("Action", 10, 12), ("Romance", 3, 3), ("Hentai", 0, 40)] {
            sqlx::query(
                "INSERT INTO feed_genre_facet (tag, safe_count, all_count) VALUES ($1,$2,$3)",
            )
            .bind(tag)
            .bind(safe)
            .bind(all)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Opted out: safe counts, and a tag no safe work carries is not offered at all.
        assert_eq!(
            genre_facets(&pool, false).await.unwrap(),
            vec![("Action".into(), 10), ("Romance".into(), 3)]
        );
        // Opted in: all counts, reordered by them.
        assert_eq!(
            genre_facets(&pool, true).await.unwrap(),
            vec![
                ("Hentai".into(), 40),
                ("Action".into(), 12),
                ("Romance".into(), 3)
            ]
        );
    }

    // ---- Browse v2 -----------------------------------------------------------------------

    /// Migration 0106's columns on an already-seeded row, plus its tag KEYS.
    #[allow(clippy::too_many_arguments)]
    async fn v2(
        pool: &Database,
        work_id: &str,
        demographic: Option<&str>,
        lang: Option<&str>,
        year: Option<i64>,
        follows: Option<i64>,
        rating: Option<f64>,
        tags: &[&str],
    ) {
        sqlx::query(
            "UPDATE browse_catalogue SET demographic = $1, original_language = $2, year = $3, \
                    follows = $4, rating = $5 WHERE work_id = $6",
        )
        .bind(demographic)
        .bind(lang)
        .bind(year)
        .bind(follows)
        .bind(rating)
        .bind(work_id)
        .execute(pool)
        .await
        .unwrap();
        for t in tags {
            tag(pool, work_id, t, t, "mangadex").await;
        }
    }

    /// One `work_tag` row with an explicit display name and key.
    async fn tag(pool: &Database, work_id: &str, name: &str, key: &str, source: &str) {
        sqlx::query(
            "INSERT INTO work_tag (work_id, tag, ord, source, tag_key) VALUES ($1, $2, 0, $3, $4)",
        )
        .bind(work_id)
        .bind(name)
        .bind(source)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }

    /// [`fixture`] plus every v2 dimension, all DISTINCT per work so each assertion below
    /// proves which column it read:
    ///
    /// ```text
    ///  id  rating(eff)   nsfw demo     lang year follows md-rating ch  tags
    ///  A   safe          0    shounen  ja   2001 500     8.0       10  action comedy
    ///  B   suggestive    0    seinen   ko   2015 100     9.0       40  action romance
    ///  C   erotica       1    josei    zh   2010 900     7.0       20  romance comedy
    ///  D   safe          0    (none)   ja   -    -       -         30  comedy
    ///  E   pornographic  1    shoujo   ja   1995 50      6.0       50  action romance comedy
    /// ```
    /// `(work_id, demographic, language, year, follows, md-rating, tag keys)`.
    type V2Row<'a> = (
        &'a str,
        Option<&'a str>,
        &'a str,
        Option<i64>,
        Option<i64>,
        Option<f64>,
        &'a [&'a str],
    );

    async fn fixture_v2(pool: &Database) {
        fixture(pool).await;
        let rows: [V2Row<'_>; 5] = [
            (
                "w_a",
                Some("shounen"),
                "ja",
                Some(2001),
                Some(500),
                Some(8.0),
                &["action", "comedy"],
            ),
            (
                "w_b",
                Some("seinen"),
                "ko",
                Some(2015),
                Some(100),
                Some(9.0),
                &["action", "romance"],
            ),
            (
                "w_c",
                Some("josei"),
                "zh",
                Some(2010),
                Some(900),
                Some(7.0),
                &["romance", "comedy"],
            ),
            ("w_d", None, "ja", None, None, None, &["comedy"]),
            (
                "w_e",
                Some("shoujo"),
                "ja",
                Some(1995),
                Some(50),
                Some(6.0),
                &["action", "romance", "comedy"],
            ),
        ];
        for (w, demo, lang, year, follows, rating, tags) in rows {
            v2(pool, w, demo, Some(lang), year, follows, rating, tags).await;
        }
    }

    fn sorted(sort: SortKey, desc: bool) -> BrowseQuery<'static> {
        BrowseQuery {
            show_nsfw: true,
            sort: Sort { key: sort, desc },
            ..Default::default()
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Include/exclude × AND/OR, by tag KEY, with `total` describing the filtered set.
    #[tokio::test]
    async fn tag_filters_include_and_exclude_in_both_modes() {
        let pool = pool().await;
        fixture_v2(&pool).await;
        let run = |inc: &[&str], im: TagMode, exc: &[&str], em: TagMode| {
            let pool = pool.clone();
            let (inc, exc) = (ids(inc), ids(exc));
            async move {
                let mut query = sorted(SortKey::LatestUpdate, true);
                query.include_tags = canonical_tag_ids(&inc);
                query.include_mode = im;
                query.exclude_tags = canonical_tag_ids(&exc);
                query.exclude_mode = em;
                titles(&pool, &query).await
            }
        };
        use TagMode::{And, Or};
        let set = |v: &[&str]| (v.len() as i64, ids(v));
        assert_eq!(
            run(&["action", "romance"], And, &[], Or).await,
            set(&["B", "E"])
        );
        assert_eq!(
            run(&["action", "romance"], Or, &[], Or).await,
            set(&["A", "B", "C", "E"])
        );
        assert_eq!(run(&["romance"], And, &[], Or).await, set(&["B", "C", "E"]));
        // Exclude ANY: drop every work carrying romance.
        assert_eq!(run(&[], And, &["romance"], Or).await, set(&["A", "D"]));
        // Exclude ALL: drop only works carrying BOTH action and romance (B, E).
        assert_eq!(
            run(&[], And, &["action", "romance"], And).await,
            set(&["A", "C", "D"])
        );
        assert_eq!(
            run(&[], And, &["action", "romance"], Or).await,
            set(&["D"]),
            "exclude-OR of the same pair drops everything carrying either"
        );
        // Combined: action works that are not comedies.
        assert_eq!(run(&["action"], Or, &["comedy"], Or).await, set(&["B"]));
        // Unknown ids MATCH NOTHING: an include-AND containing one is empty, an include-OR
        // is unaffected by it, an exclude of it excludes nothing — and an implausible id
        // (the sentinel) behaves exactly the same.
        assert_eq!(
            run(&["action", "no-such-tag"], And, &[], Or).await,
            set(&[])
        );
        assert_eq!(
            run(&["action", "no-such-tag"], Or, &[], Or).await,
            set(&["A", "B", "E"])
        );
        assert_eq!(
            run(&["action", "DROP TABLE work;"], And, &[], Or).await,
            set(&[])
        );
        assert_eq!(run(&[], And, &["no-such-tag"], Or).await.0, 5);
        // Two display names that slug to ONE key on one work ("Sci fi" by an admin and
        // "Sci-Fi" from MangaDex) are one tag: AND over {sci-fi, action} still matches A,
        // which a COUNT(*) instead of COUNT(DISTINCT tag_key) would get wrong (3 rows, n=2).
        tag(&pool, "w_a", "Sci fi", "sci-fi", "admin").await;
        tag(&pool, "w_a", "Sci-Fi", "sci-fi", "mangadex").await;
        assert_eq!(run(&["sci-fi", "action"], And, &[], Or).await, set(&["A"]));
        assert_eq!(
            run(&["sci-fi", "comedy", "action"], And, &[], Or).await,
            set(&["A"])
        );
    }

    /// THE BYPASS ATTEMPT: an opted-out viewer asking for EROTICA + PORNOGRAPHIC gets
    /// nothing — not the adult works, and not "everything they may see" either. Same for
    /// every other v2 dimension that could be used to reach an NSFW row (its tags, its
    /// language, its demographic, a text query).
    #[tokio::test]
    async fn content_ratings_cannot_bypass_the_nsfw_gate() {
        let pool = pool().await;
        fixture_v2(&pool).await;
        let adult: &[&str] = &["erotica", "pornographic"];
        let mut query = sorted(SortKey::LatestUpdate, true);
        query.show_nsfw = false;
        query.ratings = Some(adult);
        assert_eq!(titles(&pool, &query).await, (0, vec![]));
        // Aimed squarely at the NSFW works' unique attributes, still nothing.
        query.ratings = None;
        query.languages = ids(&["zh"]);
        query.demographics = vec!["josei"];
        assert_eq!(titles(&pool, &query).await, (0, vec![]));
        // The same request opted IN returns exactly the two adult works.
        let mut opted_in = sorted(SortKey::LatestUpdate, true);
        opted_in.ratings = Some(adult);
        assert_eq!(titles(&pool, &opted_in).await, (2, ids(&["C", "E"])));

        // The EFFECTIVE rating: a source-flagged NSFW work with no MangaDex rating was
        // stored as 'safe'. Materialized through the real expression it reads as 'erotica', so
        // SAFE never returns an adult work, even to an opted-in viewer.
        sqlx::query("UPDATE browse_catalogue SET content_rating = 'safe', is_nsfw = TRUE WHERE work_id = 'w_c'")
            .execute(&pool)
            .await
            .unwrap();
        let eff = crate::catalog::effective_content_rating_sql("content_rating", "is_nsfw");
        sqlx::query(&format!(
            "UPDATE browse_catalogue SET content_rating = {eff}"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let safe: &[&str] = &["safe"];
        let mut q_safe = sorted(SortKey::LatestUpdate, true);
        q_safe.ratings = Some(safe);
        assert_eq!(titles(&pool, &q_safe).await, (2, ids(&["A", "D"])));
        q_safe.ratings = Some(&["erotica"]);
        assert_eq!(titles(&pool, &q_safe).await.1, ids(&["C"]));
    }

    /// Every other v2 filter dimension, alone, with `total` following.
    #[tokio::test]
    async fn v2_attribute_filters_narrow_and_total_follows() {
        let pool = pool().await;
        fixture_v2(&pool).await;
        let run = |f: &dyn Fn(&mut BrowseQuery<'static>)| {
            let mut query = sorted(SortKey::LatestUpdate, true);
            f(&mut query);
            let pool = pool.clone();
            async move { titles(&pool, &query).await }
        };
        let set = |v: &[&str]| (v.len() as i64, ids(v));
        // Several statuses / formats are ORed (v1 honoured only the first format).
        assert_eq!(
            run(&|q| q.statuses = vec!["COMPLETED", "ONGOING"]).await,
            set(&["A", "B"])
        );
        assert_eq!(
            run(&|q| q.types = vec!["MANHUA", "MANHWA"]).await,
            set(&["B", "C"])
        );
        // Demographic, with NONE as the IS NULL arm, alone and mixed.
        assert_eq!(run(&|q| q.demographics = vec!["seinen"]).await, set(&["B"]));
        assert_eq!(run(&|q| q.demographics = vec!["none"]).await, set(&["D"]));
        assert_eq!(
            run(&|q| q.demographics = vec!["none", "shounen"]).await,
            set(&["A", "D"])
        );
        assert_eq!(
            run(&|q| q.languages = ids(&["ja"])).await,
            set(&["A", "D", "E"])
        );
        assert_eq!(
            run(&|q| q.languages = ids(&["ko", "zh"])).await,
            set(&["B", "C"])
        );
        // Ranges exclude unknowns (D has no year/follows/rating).
        assert_eq!(
            run(&|q| (q.year_from, q.year_to) = (Some(2000), Some(2012))).await,
            set(&["A", "C"])
        );
        assert_eq!(run(&|q| q.year_to = Some(2000)).await, set(&["E"]));
        assert_eq!(
            run(&|q| q.min_chapters = Some(30)).await,
            set(&["B", "D", "E"])
        );
        assert_eq!(
            run(&|q| q.min_follows = Some(100)).await,
            set(&["A", "B", "C"])
        );
        assert_eq!(
            run(&|q| q.min_md_rating = Some(7.5)).await,
            set(&["A", "B"])
        );
        // Source.
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('md1', 'w_a', 'mangadex', 'mangadex', 'u-a', '2026-01-01T00:00:00Z'), \
                    ('sw1', 'w_b', 'suwayomi', '777', '1', '2026-01-01T00:00:00Z'), \
                    ('sw2', 'w_d', 'suwayomi', '888', '2', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            run(&|q| q.source = parse_source(Some("MangaDex"))).await,
            set(&["A"])
        );
        assert_eq!(
            run(&|q| q.source = parse_source(Some("suwayomi"))).await,
            set(&["B", "D"])
        );
        assert_eq!(
            run(&|q| q.source = parse_source(Some("888"))).await,
            set(&["D"])
        );
        assert_eq!(
            run(&|q| q.source = parse_source(Some("x' OR 1=1 --"))).await,
            set(&[]),
            "an implausible source id matches nothing, it is never interpolated"
        );
        // Everything at once.
        assert_eq!(
            run(&|q| {
                q.statuses = vec!["ONGOING", "COMPLETED", "UNKNOWN"];
                q.languages = ids(&["ja", "ko"]);
                q.min_follows = Some(60);
                q.include_tags = ids(&["action"]);
            })
            .await,
            set(&["A", "B"])
        );
    }

    /// The source list offers what the `source` filter can return for the viewer, with
    /// the same count, and never a source the NSFW gate would leave empty.
    #[tokio::test]
    async fn source_facets_count_what_the_source_filter_returns() {
        let pool = pool().await;
        seed(&pool, "w_a", "A", "manga", "ONGOING", "safe", 0, 5, Some(4)).await;
        seed(&pool, "w_b", "B", "manga", "ONGOING", "safe", 0, 5, Some(3)).await;
        seed(
            &pool,
            "w_c",
            "C",
            "manga",
            "ONGOING",
            "erotica",
            1,
            5,
            Some(2),
        )
        .await;
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('md1', 'w_a', 'mangadex', 'mangadex', 'u-a', '2026-01-01T00:00:00Z'), \
                    ('md2', 'w_b', 'mangadex', 'mangadex', 'u-b', '2026-01-01T00:00:00Z'), \
                    ('sw1', 'w_b', 'suwayomi', '777', '1', '2026-01-01T00:00:00Z'), \
                    ('sw2', 'w_b', 'suwayomi', '777', '2', '2026-01-01T00:00:00Z'), \
                    ('sw3', 'w_c', 'suwayomi', '888', '3', '2026-01-01T00:00:00Z'), \
                    ('sw4', 'w_a', 'suwayomi', '', '4', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_extension (source_id, pkg_name, repo_url, lang, updated_at) \
             VALUES ('777', 'ext.en.example', 'https://example.org/repo', 'en', \
                     '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let facet = |id: &str, pkg: Option<&str>, lang: Option<&str>, count| SourceFacet {
            id: id.into(),
            pkg_name: pkg.map(Into::into),
            lang: lang.map(Into::into),
            count,
        };
        // Opted out: the adult-only source 888 is not offered; a work mapped twice on one
        // source counts once; the mapping with no source id is not a filter.
        assert_eq!(
            source_facets(&pool, false).await.unwrap(),
            vec![
                facet("mangadex", None, None, 2),
                facet("777", Some("ext.en.example"), Some("en"), 1),
            ]
        );
        assert_eq!(
            source_facets(&pool, true).await.unwrap(),
            vec![
                facet("mangadex", None, None, 2),
                facet("777", Some("ext.en.example"), Some("en"), 1),
                facet("888", None, None, 1),
            ]
        );
        // Each count is the filter's own total.
        for f in source_facets(&pool, true).await.unwrap() {
            let mut query = q(BrowseSort::Newest, 1);
            query.source = parse_source(Some(&f.id));
            let (total, _) = browse_catalogue(&pool, &query).await.unwrap();
            assert_eq!(total, f.count, "source {}", f.id);
        }
    }

    /// Every v2 sort, both directions: the key, then `work_id` in the same direction, and
    /// unknown (NULL) keys LAST in BOTH directions.
    #[tokio::test]
    async fn every_sort_orders_by_its_key_in_both_directions_with_nulls_last() {
        let pool = pool().await;
        fixture_v2(&pool).await;
        // An undated work for the chapter clock; created_at spread for RECENTLY_ADDED.
        seed(&pool, "w_f", "F", "MANGA", "ONGOING", "safe", 0, 5, None).await;
        for (w, at) in [
            ("w_a", "2026-05-03T00:00:00.000000+00:00"),
            ("w_b", "2026-05-05T00:00:00.000000+00:00"),
            ("w_c", "2026-05-01T00:00:00.000000+00:00"),
            ("w_d", "2026-05-04T00:00:00.000000+00:00"),
            ("w_e", "2026-05-02T00:00:00.000000+00:00"),
            ("w_f", "2026-05-06T00:00:00.000000+00:00"),
        ] {
            sqlx::query("UPDATE browse_catalogue SET created_at = $1 WHERE work_id = $2")
                .bind(crate::db_types::date(at).unwrap())
                .bind(w)
                .execute(&pool)
                .await
                .unwrap();
        }
        // Mixed-case titles so TITLE proves it is case-insensitive.
        for (w, t) in [
            ("w_a", "delta"),
            ("w_b", "Alpha"),
            ("w_c", "charlie"),
            ("w_d", "Bravo"),
            ("w_e", "echo"),
            ("w_f", "Foxtrot"),
        ] {
            sqlx::query("UPDATE browse_catalogue SET title = $1 WHERE work_id = $2")
                .bind(t)
                .bind(w)
                .execute(&pool)
                .await
                .unwrap();
        }
        let order = |key: SortKey, desc: bool| {
            let pool = pool.clone();
            async move {
                let (total, rows) = browse_catalogue(&pool, &sorted(key, desc)).await.unwrap();
                assert_eq!(total, 6, "{key:?} {desc}: a sort never changes the total");
                rows.into_iter()
                    .map(|r| r.work_id.trim_start_matches("w_").to_uppercase())
                    .collect::<Vec<_>>()
                    .join("")
            }
        };
        use SortKey::*;
        let cases: &[(SortKey, &str, &str)] = &[
            // released_at: A 5000 … E 1000, F NULL.
            (LatestUpdate, "ABCDEF", "EDCBAF"),
            (RecentlyAdded, "FBDAEC", "CEADBF"),
            // follows: C 900, A 500, B 100, E 50; D and F unknown (work_id order, DESC/ASC).
            (MostFollows, "CABEFD", "EBACDF"),
            (HighestRated, "BACEFD", "ECABDF"),
            (Year, "BCAEFD", "EACBDF"),
            // en_chapter_count: E 50, B 40, D 30, C 20, A 10, F 5.
            (MostChapters, "EBDCAF", "FACDBE"),
            // Case-insensitive: Alpha, Bravo, charlie, delta, echo, Foxtrot.
            (Title, "FEACDB", "BDCAEF"),
            // RELEVANCE without a query falls back to LATEST_UPDATE, keeping the direction.
            (Relevance, "ABCDEF", "EDCBAF"),
        ];
        for (key, desc_order, asc_order) in cases {
            assert_eq!(order(*key, true).await, *desc_order, "{key:?} DESC");
            assert_eq!(order(*key, false).await, *asc_order, "{key:?} ASC");
        }

        // MOST_VIEWED: viewed works by views, the unviewed remainder newest-first, and ASC
        // reverses only the viewed prefix — unviewed is still last.
        let hour = chrono::Utc::now().timestamp() / 3600;
        for (key, views) in [("w_c", 30i64), ("w_e", 20), ("w_b", 10)] {
            sqlx::query(
                "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES ($1, $2, $3)",
            )
            .bind(key)
            .bind(hour)
            .bind(views)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(order(MostViewed, true).await, "CEBADF");
        assert_eq!(order(MostViewed, false).await, "BECADF");

        // READER_RATED (our own reviews): rated works by average, the unrated remainder
        // newest-first, and ASC reverses only the rated prefix — unrated is still last.
        sqlx::query(
            r#"INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('11ac9418-0c9e-4237-9e42-3ace93d7c468','u','u@e','h','2020-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        for (i, (w, score)) in [("w_d", 9), ("w_a", 4)].iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) VALUES ($1, $2, '11ac9418-0c9e-4237-9e42-3ace93d7c468', $3, 'b', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#,
            )
            .bind(crate::db_types::test_uuid(&(format!("rv{i}"))))
            .bind(*w)
            .bind(*score)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(order(CommunityRating, true).await, "DABCEF");
        assert_eq!(order(CommunityRating, false).await, "ADBCEF");
    }

    /// A nullable key sorted ASC is a two-phase walk; the pages must stitch across the
    /// NULL boundary with no row repeated or skipped, and `total` must equal the walk.
    #[tokio::test]
    async fn nulls_last_ascending_pages_stitch_exactly_across_the_boundary() {
        let pool = pool().await;
        let n = BROWSE_PAGE_SIZE * 2 + 7;
        for i in 0..n {
            let id = format!("w_{i:04}");
            seed(
                &pool,
                &id,
                &id,
                "MANGA",
                "ONGOING",
                "safe",
                0,
                1,
                Some(1000 + i),
            )
            .await;
            // Every third work has no follows; the rest descend with i so ASC reverses them.
            let follows = (i % 3 != 0).then_some(10_000 - i);
            sqlx::query("UPDATE browse_catalogue SET follows = $1 WHERE work_id = $2")
                .bind(follows)
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut seen: Vec<(Option<i64>, String)> = Vec::new();
        let mut page = 1;
        loop {
            let mut query = sorted(SortKey::MostFollows, false);
            query.page = page;
            let (total, rows) = browse_catalogue(&pool, &query).await.unwrap();
            assert_eq!(total, n);
            if rows.is_empty() {
                break;
            }
            for r in rows {
                let f: Option<i64> =
                    sqlx::query_scalar("SELECT follows FROM browse_catalogue WHERE work_id = $1")
                        .bind(&r.work_id)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                seen.push((f, r.work_id));
            }
            page += 1;
        }
        assert_eq!(seen.len() as i64, n, "every row exactly once");
        let unique: std::collections::HashSet<&String> = seen.iter().map(|(_, w)| w).collect();
        assert_eq!(
            unique.len() as i64,
            n,
            "no row repeated across a page boundary"
        );
        let first_null = seen.iter().position(|(f, _)| f.is_none()).unwrap();
        assert!(
            seen[first_null..].iter().all(|(f, _)| f.is_none()),
            "NULLs strictly last"
        );
        assert!(
            seen[..first_null].windows(2).all(|w| w[0].0 <= w[1].0),
            "the known prefix ascends"
        );
        assert!(
            seen[first_null..].windows(2).all(|w| w[0].1 < w[1].1),
            "the NULL tail is in work_id ASC order"
        );
    }

    async fn fts(pool: &Database, work_id: &str, _title: &str, chapters: i64) {
        let source_id = format!("search-source:{work_id}");
        sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) VALUES($1,$2,'mangadex',$1,'2026-01-01T00:00:00Z') ON CONFLICT DO NOTHING")
            .bind(&source_id).bind(work_id).execute(pool).await.unwrap();
        for n in 0..chapters {
            sqlx::query("INSERT INTO chapter(id,source_series_id,external_id,created_at) VALUES($1,$2,$1,'2026-01-01T00:00:00Z')")
                .bind(format!("search-chapter:{work_id}:{n}")).bind(&source_id).execute(pool).await.unwrap();
        }
    }

    #[tokio::test]
    async fn text_search_applies_the_filters_and_keeps_relevance() {
        let pool = pool().await;
        //     id      title               nsfw demo     follows ch
        for (id, title, nsfw, demo, follows, ch) in [
            ("w_1", "Solo", 0, "seinen", 10, 5),
            ("w_2", "Solo Leveling", 0, "seinen", 900, 200),
            ("w_3", "Solo Camping", 0, "josei", 500, 80),
            ("w_4", "Solo Nights", 1, "seinen", 999, 300),
            ("w_5", "Unrelated", 0, "seinen", 1000, 400),
        ] {
            seed(
                &pool,
                id,
                title,
                "MANGA",
                "ONGOING",
                "safe",
                nsfw,
                ch,
                Some(1000),
            )
            .await;
            sqlx::query("UPDATE work SET is_nsfw = $1 WHERE id = $2")
                .bind(nsfw != 0)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            v2(
                &pool,
                id,
                Some(demo),
                Some("ja"),
                None,
                Some(follows),
                None,
                &[],
            )
            .await;
            fts(&pool, id, title, ch).await;
        }
        tag(&pool, "w_3", "Comedy", "comedy", "mangadex").await;
        let search = |f: &dyn Fn(&mut BrowseQuery<'static>)| {
            let mut query = BrowseQuery {
                show_nsfw: true,
                sort: Sort::desc(SortKey::Relevance),
                ..Default::default()
            };
            f(&mut query);
            let pool = pool.clone();
            async move {
                let (t, rows) = search_text(&pool, "solo", &query).await.unwrap();
                (t, rows.into_iter().map(|r| r.title).collect::<Vec<_>>())
            }
        };
        let set = |v: &[&str]| (v.len() as i64, ids(v));
        // Relevance: the exact title first (tier 0), then prefix matches by chapters.
        assert_eq!(
            search(&|_| {}).await,
            set(&["Solo", "Solo Nights", "Solo Leveling", "Solo Camping"])
        );
        // A filter NARROWS the matches and the total follows; order is still relevance.
        assert_eq!(
            search(&|q| q.demographics = vec!["seinen"]).await,
            set(&["Solo", "Solo Nights", "Solo Leveling"])
        );
        assert_eq!(
            search(&|q| q.include_tags = ids(&["comedy"])).await,
            set(&["Solo Camping"])
        );
        // The gate: an opted-out viewer never sees the NSFW match, total included.
        assert_eq!(
            search(&|q| q.show_nsfw = false).await,
            set(&["Solo", "Solo Leveling", "Solo Camping"])
        );
        // Another sort re-orders the FILTERED matches (never the non-matching w_5, despite
        // its 1,000 follows).
        assert_eq!(
            search(&|q| q.sort = Sort::desc(SortKey::MostFollows)).await,
            set(&["Solo Nights", "Solo Leveling", "Solo Camping", "Solo"])
        );
        assert_eq!(
            search(&|q| {
                q.sort = Sort {
                    key: SortKey::MostFollows,
                    desc: false,
                };
                q.min_follows = Some(100);
            })
            .await,
            set(&["Solo Camping", "Solo Leveling", "Solo Nights"])
        );
        // READER_RATED re-orders the matches by OUR review average, both directions, with
        // the unrated matches after the rated ones either way.
        sqlx::query(
            r#"INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('11ac9418-0c9e-4237-9e42-3ace93d7c468','u','u@e','h','2020-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        for (i, (w, score)) in [("w_3", 9), ("w_1", 4)].iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) VALUES ($1, $2, '11ac9418-0c9e-4237-9e42-3ace93d7c468', $3, 'b', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#,
            )
            .bind(crate::db_types::test_uuid(&(format!("rv{i}"))))
            .bind(*w)
            .bind(*score)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (desc, rated) in [
            (true, ["Solo Camping", "Solo"]),
            (false, ["Solo", "Solo Camping"]),
        ] {
            let (t, titles) = search(&|q| {
                q.sort = Sort {
                    key: SortKey::CommunityRating,
                    desc,
                }
            })
            .await;
            assert_eq!(t, 4, "a sort never changes the total");
            assert_eq!(titles[..2], rated, "reader rating desc={desc}");
        }
        // A short prefix finds the three eligible titles; the fourth is NSFW.
        let none = search_text(&pool, "so", &BrowseQuery::default())
            .await
            .unwrap();
        assert_eq!(
            (none.0, none.1.len()),
            (3, 3),
            "short prefixes obey the anonymous NSFW gate"
        );
        // Over-range page: honest total, no rows.
        let (t, rows) = search_text(
            &pool,
            "solo",
            &BrowseQuery {
                show_nsfw: true,
                sort: Sort::desc(SortKey::Relevance),
                page: 99,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!((t, rows.len()), (4, 0));
    }

    /// Every sort key of text search, in both directions, over the same four
    /// matches: each one is its own statement over the match set. The non-matching
    /// work leads on every key and must never appear.
    #[tokio::test]
    async fn text_search_orders_the_matches_by_every_sort_key() {
        let pool = pool().await;
        //     id      title        ch  released follows md-rating year
        for (id, title, ch, released, follows, rating, year) in [
            (
                "w_1",
                "Ember Dawn",
                40,
                4000,
                Some(10),
                Some(9.0),
                Some(2001),
            ),
            (
                "w_2",
                "ember ash",
                10,
                1000,
                Some(900),
                Some(6.0),
                Some(2020),
            ),
            ("w_3", "Ember Coast", 30, 3000, None, None, None),
            (
                "w_4",
                "Ember Bay",
                20,
                2000,
                Some(500),
                Some(7.5),
                Some(2010),
            ),
            ("w_5", "Other", 99, 9000, Some(9999), Some(9.9), Some(2025)),
        ] {
            seed(
                &pool,
                id,
                title,
                "MANGA",
                "ONGOING",
                "safe",
                0,
                ch,
                Some(released),
            )
            .await;
            v2(&pool, id, None, Some("ja"), year, follows, rating, &[]).await;
            fts(&pool, id, title, 0).await;
        }
        // `created_at` in the reverse of the id order.
        sqlx::query(
            "UPDATE browse_catalogue SET created_at = \
             '2026-01-01T00:00:00Z'::timestamptz - (right(work_id, 1)::int * interval '1 day')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Views: w_4 most, then w_2; the rest unviewed (newest release first).
        let hour = chrono::Utc::now().timestamp() / 3600;
        for (key, views) in [("w_4", 100i64), ("w_2", 10)] {
            sqlx::query(
                "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES ($1, $2, $3)",
            )
            .bind(key)
            .bind(hour)
            .bind(views)
            .execute(&pool)
            .await
            .unwrap();
        }
        let order = |key: SortKey, desc: bool| {
            let pool = pool.clone();
            async move {
                let query = BrowseQuery {
                    sort: Sort { key, desc },
                    ..Default::default()
                };
                let (total, rows) = search_text(&pool, "ember", &query).await.unwrap();
                assert_eq!(total, 4, "{key:?} desc={desc}");
                rows.into_iter()
                    .map(|row| row.work_id[2..].to_owned())
                    .collect::<Vec<_>>()
                    .join("")
            }
        };
        for (key, desc, asc) in [
            (SortKey::LatestUpdate, "1342", "2431"),
            (SortKey::RecentlyAdded, "1234", "4321"),
            (SortKey::MostChapters, "1342", "2431"),
            // NULL keys last in both directions.
            (SortKey::MostFollows, "2413", "1423"),
            (SortKey::HighestRated, "1423", "2413"),
            (SortKey::Year, "2413", "1423"),
            (SortKey::Title, "1342", "2431"),
            // Viewed works first in both directions, then the newest release.
            (SortKey::MostViewed, "4213", "2413"),
        ] {
            assert_eq!(order(key, true).await, desc, "{key:?} descending");
            assert_eq!(order(key, false).await, asc, "{key:?} ascending");
        }
        pool.close().await;
    }

    /// A query the first statement finds nothing for runs `search::retry_ctes`, as in
    /// catalogue search: a misspelt term that is a word of another title, and a query
    /// without its spaces. The filter, the other sort keys and the pages apply to that
    /// answer, with one total. A query with an answer builds no retry statement.
    #[tokio::test]
    async fn text_search_retries_a_query_with_no_match() {
        let pool = pool().await;
        //     id      title              demo       follows ch
        let mut works = vec![
            ("w_1".to_owned(), "One Piece".to_owned(), "shounen", 10, 5),
            ("w_2".into(), "One Piece Party".into(), "seinen", 900, 9),
            ("w_3".into(), "Jujutsu Kaisen".into(), "shounen", 500, 7),
            ("w_4".into(), "Kaizen Notes".into(), "seinen", 1, 1),
        ];
        // More than one page of works for one joined query.
        for n in 10..42 {
            works.push((format!("e_{n}"), format!("Ember Fall {n}"), "josei", n, 0));
        }
        for (id, title, demo, follows, ch) in &works {
            seed(
                &pool,
                id,
                title,
                "MANGA",
                "ONGOING",
                "safe",
                0,
                *ch,
                Some(1000),
            )
            .await;
            v2(
                &pool,
                id,
                Some(demo),
                Some("ja"),
                None,
                Some(*follows),
                None,
                &[],
            )
            .await;
            fts(&pool, id, title, *ch).await;
        }
        let search = |text: &'static str, f: &dyn Fn(&mut BrowseQuery<'static>)| {
            let mut query = BrowseQuery {
                sort: Sort::desc(SortKey::Relevance),
                ..Default::default()
            };
            f(&mut query);
            let pool = pool.clone();
            async move {
                let before = crate::search::RETRIES.with(std::cell::Cell::get);
                let (t, rows) = search_text(&pool, text, &query).await.unwrap();
                let retried = crate::search::RETRIES.with(std::cell::Cell::get) > before;
                let titles = rows.into_iter().map(|r| r.title).collect::<Vec<_>>();
                (t, titles, retried)
            }
        };
        let set = |v: &[&str], retried: bool| (v.len() as i64, ids(v), retried);
        // The first statement answers: no retry, and its answer alone.
        assert_eq!(
            search("one piece", &|_| {}).await,
            set(&["One Piece", "One Piece Party"], false)
        );
        assert_eq!(
            search("kaizen", &|_| {}).await,
            set(&["Kaizen Notes"], false)
        );
        // Without its spaces: the equal title first, then the longer one.
        assert_eq!(
            search("onepiece", &|_| {}).await,
            set(&["One Piece", "One Piece Party"], true)
        );
        // `kaizen` is a word of w_4 only, and no work has both terms.
        assert_eq!(
            search("jujutsu kaizen", &|_| {}).await,
            set(&["Jujutsu Kaisen"], true)
        );
        // The filter narrows the retry's answer and its total.
        assert_eq!(
            search("onepiece", &|q| q.demographics = vec!["seinen"]).await,
            set(&["One Piece Party"], true)
        );
        assert_eq!(
            search("jujutsu kaizen", &|q| q.demographics = vec!["seinen"]).await,
            set(&[], true)
        );
        // This filter hides w_4, so `kaizen` has no eligible word hit and the first
        // statement gives it typo matching, as it always did.
        assert_eq!(
            search("jujutsu kaizen", &|q| q.demographics = vec!["shounen"]).await,
            set(&["Jujutsu Kaisen"], false)
        );
        // Another sort key orders the same answer, in both directions.
        for (desc, expected) in [
            (true, ["One Piece Party", "One Piece"]),
            (false, ["One Piece", "One Piece Party"]),
        ] {
            assert_eq!(
                search("onepiece", &|q| {
                    q.sort = Sort {
                        key: SortKey::MostFollows,
                        desc,
                    }
                })
                .await,
                set(&expected, true),
                "follows desc={desc}"
            );
        }
        // Pages: one total, every work once, nothing past the end.
        for sort in [SortKey::Relevance, SortKey::MostFollows] {
            let mut seen = std::collections::BTreeSet::new();
            for (page, rows) in [(1, 30), (2, 2), (3, 0)] {
                let (t, titles, retried) = search("emberfall", &|q| {
                    q.sort = Sort::desc(sort);
                    q.page = page;
                })
                .await;
                assert_eq!(
                    (t, titles.len(), retried),
                    (32, rows, true),
                    "{sort:?} {page}"
                );
                seen.extend(titles);
            }
            assert_eq!(seen.len(), 32, "{sort:?}");
        }
        pool.close().await;
    }

    /// A first statement that runs out of the search budget on typo matching is no
    /// error here either: Browse answers with what the search finds without typo
    /// matching, total and page, for relevance and for another sort key. The typo test
    /// of this database outlasts the budget from the second search on. A retry statement
    /// over the budget gives the first statement's answer, nothing. Last, the count fits
    /// the budget and each kind of page statement does not: the request starts again
    /// without typo matching and answers from that pass.
    #[tokio::test]
    async fn text_search_over_budget_answers_without_typo_matching() {
        let pool = pool().await;
        for (id, title, follows) in [
            ("w_1", "One Piece", 10),
            ("w_2", "One Piece Party", 900),
            ("w_3", "Onepiecf Saga", 5),
            ("w_4", "Jujutsu Kaisen", 7),
            ("w_5", "Kaizen Notes", 1),
        ] {
            seed(
                &pool,
                id,
                title,
                "MANGA",
                "ONGOING",
                "safe",
                0,
                1,
                Some(1000),
            )
            .await;
            v2(&pool, id, None, Some("ja"), None, Some(follows), None, &[]).await;
            fts(&pool, id, title, 1).await;
        }
        let search = |text: &'static str, key: SortKey| {
            let query = BrowseQuery {
                sort: Sort::desc(key),
                ..Default::default()
            };
            let pool = pool.clone();
            async move {
                let (t, rows) = search_text(&pool, text, &query).await.unwrap();
                (t, rows.into_iter().map(|r| r.title).collect::<Vec<_>>())
            }
        };
        assert_eq!(
            search("onepiece", SortKey::Relevance).await,
            (1, ids(&["Onepiecf Saga"]))
        );
        assert_eq!(
            search("jujutsu kaizen", SortKey::Relevance).await,
            (1, ids(&["Jujutsu Kaisen"]))
        );
        crate::search::slow_typo_test(&pool, "10").await;
        assert_eq!(
            search("onepiece", SortKey::Relevance).await,
            (2, ids(&["One Piece", "One Piece Party"]))
        );
        assert_eq!(
            search("onepiece", SortKey::MostFollows).await,
            (2, ids(&["One Piece Party", "One Piece"]))
        );
        // No term in fallback: the first statement counts nothing at once, and the
        // retry asks the typo test for `kaizen`.
        assert_eq!(
            search("jujutsu kaizen", SortKey::Relevance).await,
            (0, Vec::new())
        );
        for (key, expected) in [
            (SortKey::Relevance, ["One Piece", "One Piece Party"]),
            (SortKey::MostFollows, ["One Piece Party", "One Piece"]),
        ] {
            let slow = "CASE WHEN waited < 0.8 THEN 0.9 WHEN waited < 2.5 THEN 10 ELSE 0 END";
            crate::search::slow_typo_test(&pool, slow).await;
            let started = std::time::Instant::now();
            assert_eq!(
                search("onepiece", key).await,
                (2, ids(&expected)),
                "{key:?}"
            );
            let seconds = started.elapsed().as_secs_f64();
            assert!((2.8..4.0).contains(&seconds), "{key:?}: {seconds} s");
        }
        pool.close().await;
    }

    /// Client input is bounded and canonical before it reaches SQL or the memo key.
    #[test]
    fn v2_inputs_are_validated_bounded_and_canonical() {
        // Tag ids: trimmed, lowercased, deduped, sorted; implausible -> one sentinel that no
        // slug can equal (so it matches nothing instead of being dropped).
        assert_eq!(
            canonical_tag_ids(&ids(&[
                " Romance ",
                "action",
                "ACTION",
                "",
                "x'; --",
                "a b"
            ])),
            ids(&["!", "action", "romance"])
        );
        assert!(!crate::tags::is_plausible_tag_id(UNMATCHABLE));
        assert_eq!(
            canonical_tag_ids(&ids(&[&"a".repeat(65)])),
            ids(&["!"]),
            "over-long ids are implausible"
        );
        // Languages.
        assert_eq!(
            canonical_languages(&ids(&["JA", " zh-HK ", "ko", "ja", "english", "x"])),
            ids(&["!", "ja", "ko", "zh-hk"])
        );
        // Years: clamped, reversed range swapped.
        assert_eq!(year_range(Some(1500), Some(3000)), (Some(1900), Some(2100)));
        assert_eq!(year_range(Some(2020), Some(2010)), (Some(2010), Some(2020)));
        assert_eq!(year_range(None, Some(1)), (None, Some(1900)));
        // Minimums: <= 0 is no filter (emitting `>= 0` would drop the unknowns), capped.
        assert_eq!(min_chapters(Some(0)), None);
        assert_eq!(min_chapters(Some(-5)), None);
        assert_eq!(min_chapters(Some(i64::MAX)), Some(MIN_CHAPTERS_CAP));
        assert_eq!(min_follows(Some(0)), None);
        assert_eq!(min_follows(Some(42)), Some(42));
        assert_eq!(min_md_rating(Some(f64::NAN)), None);
        assert_eq!(min_md_rating(Some(f64::INFINITY)), None);
        assert_eq!(min_md_rating(Some(0.0)), None);
        assert_eq!(min_md_rating(Some(11.0)), Some(10.0));
        assert_eq!(min_md_rating(Some(7.4567)), Some(7.46));
        // Source.
        assert_eq!(parse_source(None), None);
        assert_eq!(parse_source(Some("   ")), None);
        assert_eq!(parse_source(Some("MANGADEX")), Some(SourceFilter::MangaDex));
        assert_eq!(parse_source(Some("suwayomi")), Some(SourceFilter::Suwayomi));
        assert_eq!(
            parse_source(Some("2499283573021220255")),
            Some(SourceFilter::SuwayomiSource("2499283573021220255".into()))
        );
        assert_eq!(
            parse_source(Some("bad id!")),
            Some(SourceFilter::SuwayomiSource(UNMATCHABLE.into()))
        );
    }

    /// The memo key must separate every v2 dimension too — same argument as the v1 test:
    /// nothing end-to-end can catch a gap, because the TTL is zero under test.
    #[test]
    fn count_key_separates_every_v2_dimension() {
        let base = count_key(&BrowseQuery::default());
        let with = |f: &dyn Fn(&mut BrowseQuery<'static>)| {
            let mut c = BrowseQuery::default();
            f(&mut c);
            count_key(&c)
        };
        let variants = vec![
            with(&|c| c.types = vec!["MANGA", "MANHWA"]),
            with(&|c| c.statuses = vec!["ONGOING", "COMPLETED"]),
            with(&|c| c.demographics = vec!["none"]),
            with(&|c| c.demographics = vec!["seinen"]),
            with(&|c| c.ratings = Some(&["erotica"])),
            with(&|c| c.ratings = Some(&[])),
            with(&|c| c.languages = ids(&["ja"])),
            with(&|c| c.year_from = Some(2000)),
            with(&|c| c.year_to = Some(2000)),
            with(&|c| c.min_chapters = Some(5)),
            with(&|c| c.min_follows = Some(5)),
            with(&|c| c.min_md_rating = Some(5.0)),
            with(&|c| c.source = Some(SourceFilter::MangaDex)),
            with(&|c| c.source = Some(SourceFilter::Suwayomi)),
            with(&|c| c.source = Some(SourceFilter::SuwayomiSource("1".into()))),
            with(&|c| c.include_tags = ids(&["action"])),
            with(&|c| c.exclude_tags = ids(&["action"])),
            with(&|c| c.include_mode = TagMode::Or),
            with(&|c| c.exclude_mode = TagMode::And),
            // The same strings in different lists must not collide.
            with(&|c| {
                c.include_tags = ids(&["a"]);
                c.exclude_tags = ids(&["b"]);
            }),
            with(&|c| {
                c.include_tags = ids(&["a", "b"]);
            }),
            with(&|c| {
                c.languages = ids(&["a"]);
                c.include_tags = ids(&["b"]);
            }),
        ];
        let mut all = variants.clone();
        all.push(base.clone());
        let before = all.len();
        all.sort();
        all.dedup();
        assert_eq!(before, all.len(), "two distinct filter sets share a key");
        // Direction is a ROW dimension, never a COUNT one.
        let mut asc = BrowseQuery::default();
        asc.sort.desc = false;
        assert_eq!(base, count_key(&asc));
        assert_ne!(row_key(&BrowseQuery::default()), row_key(&asc));
    }

    /// PostgreSQL EXPLAIN ANALYZE over realistic tag skew. Index probes and a
    /// single decorrelated hash/semi-join scan are valid; repeated full scans
    /// per candidate card are forbidden. The complete plans remain in test output.
    #[tokio::test]
    async fn tag_filters_use_the_tag_indexes() {
        let pool = pool().await;
        let mut tx = pool.begin().await.unwrap();
        let vocab: Vec<String> = (0..60).map(|i| format!("tag-{i}")).collect();
        for i in 0..3000 {
            let id = format!("w_{i:05}");
            sqlx::query(
                "INSERT INTO work (id, primary_title, created_at, updated_at) \
                 VALUES ($1, $2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(&id)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO browse_catalogue (work_id, reader_id, title, is_nsfw, \
                     en_chapter_count, released_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, '2026-01-01T00:00:00Z')",
            )
            .bind(&id)
            .bind(&id)
            .bind(&id)
            .bind(i % 10 == 0)
            .bind((i % 50) as i64)
            .bind(crate::db_types::epoch_millis(1000 + i as i64).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
            // ~6 tags per work, skewed toward the low ids (tag-0 is on every other work).
            for k in 0..6usize {
                let t = &vocab[(i * (k + 1) * 7 + k * k) % (10 + k * 10)];
                sqlx::query(
                    "INSERT INTO work_tag (work_id, tag, ord, source, tag_key) \
                     VALUES ($1, $2, 0, 'mangadex', $3) ON CONFLICT DO NOTHING",
                )
                .bind(&id)
                .bind(t)
                .bind(t)
                .execute(&mut *tx)
                .await
                .unwrap();
            }
        }
        tx.commit().await.unwrap();
        sqlx::query("ANALYZE").execute(&pool).await.unwrap();

        async fn plan(pool: &Database, sql: &str, binds: &[Bind]) -> serde_json::Value {
            let text = format!("EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS) {sql}");
            let mut qq = sqlx::query_scalar::<_, serde_json::Value>(&text);
            for b in binds {
                qq = match b {
                    Bind::Text(s) => qq.bind(s.clone()),
                    Bind::Real(v) => qq.bind(*v),
                    Bind::Int(v) => qq.bind(*v),
                };
            }
            qq.fetch_one(pool).await.unwrap()
        }
        fn nodes<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
            if value.get("Node Type").is_some() {
                out.push(value);
            }
            if let Some(children) = value.get("Plans").and_then(serde_json::Value::as_array) {
                for child in children {
                    nodes(child, out);
                }
            }
        }
        use TagMode::{And, Or};
        /// `(label, include, include mode, exclude, exclude mode, the index work_tag must use)`.
        type Shape<'a> = (
            &'a str,
            Vec<&'a str>,
            TagMode,
            Vec<&'a str>,
            TagMode,
            &'a str,
        );
        let shapes: [Shape<'_>; 5] = [
            (
                "include AND (2)",
                vec!["tag-3", "tag-7"],
                And,
                vec![],
                Or,
                "idx_work_tag_key_work",
            ),
            (
                "include AND (1)",
                vec!["tag-3"],
                And,
                vec![],
                Or,
                "idx_work_tag_key_work",
            ),
            (
                "include OR (2)",
                vec!["tag-3", "tag-7"],
                Or,
                vec![],
                Or,
                "idx_work_tag_work_key",
            ),
            (
                "exclude OR (2)",
                vec![],
                Or,
                vec!["tag-3", "tag-7"],
                Or,
                "idx_work_tag_work_key",
            ),
            (
                "exclude AND (2)",
                vec![],
                Or,
                vec!["tag-3", "tag-7"],
                And,
                "idx_work_tag_key_work",
            ),
        ];
        for (label, inc, im, exc, em, want_index) in shapes {
            for show_nsfw in [false, true] {
                let query = BrowseQuery {
                    show_nsfw,
                    include_tags: ids(&inc),
                    include_mode: im,
                    exclude_tags: ids(&exc),
                    exclude_mode: em,
                    ..Default::default()
                };
                let (where_sql, binds, _) = build_where(&query);
                for (kind, sql) in [
                    (
                        "page",
                        format!(
                            "SELECT {BROWSE_COLS} FROM browse_catalogue f WHERE {where_sql} \
                             ORDER BY {NEWEST_ORDER} LIMIT 30 OFFSET 0"
                        ),
                    ),
                    (
                        "count",
                        format!("SELECT COUNT(*) FROM browse_catalogue f WHERE {where_sql}"),
                    ),
                ] {
                    let output = plan(&pool, &sql, &binds).await;
                    eprintln!("{label} / {kind} / show_nsfw={show_nsfw}: {output}");
                    let mut all = Vec::new();
                    nodes(&output[0]["Plan"], &mut all);
                    let accesses: Vec<_> = all
                        .iter()
                        .filter(|node| {
                            node["Relation Name"] == "work_tag"
                                || node["Index Name"]
                                    .as_str()
                                    .is_some_and(|name| name.starts_with("idx_work_tag_"))
                        })
                        .collect();
                    assert!(
                        !accesses.is_empty(),
                        "{label}/{kind}: no work_tag access: {output}"
                    );
                    let bounded_per_work_seek = |node: &serde_json::Value| {
                        let condition = node["Index Cond"]
                            .as_str()
                            .or_else(|| node["Recheck Cond"].as_str())
                            .unwrap_or("");
                        let inspected = node["Actual Rows"].as_f64().unwrap_or(0.0)
                            + node["Rows Removed by Filter"].as_f64().unwrap_or(0.0);
                        // At most six tags were seeded per work; JSON rounds the
                        // average removed-row count. A parameterized work-ID seek
                        // through work_source is also bounded, even if tag_key is
                        // rechecked instead of included in that index condition.
                        condition.contains("= f.work_id")
                            && inspected <= 7.0
                            && matches!(
                                node["Node Type"].as_str(),
                                Some(
                                    "Index Scan"
                                        | "Index Only Scan"
                                        | "Bitmap Heap Scan"
                                        | "Bitmap Index Scan"
                                )
                            )
                    };
                    for node in &accesses {
                        // One decorrelated scan is valid. Every repeated access
                        // must instead prove a bounded, parameterized work-ID seek;
                        // this rejects repeated full table AND full index scans.
                        if node["Actual Loops"].as_f64().unwrap_or(0.0) > 1.0 {
                            assert!(
                                bounded_per_work_seek(node),
                                "{label}/{kind}: repeated unbounded tag access: {node}"
                            );
                        }
                    }
                    let indexed = all.iter().any(|node| {
                        node["Index Name"].as_str().is_some_and(|name| {
                            name == want_index
                                || name == "idx_work_tag_key_work"
                                || name == "idx_work_tag_work_key"
                        })
                    }) || accesses.iter().any(|node| bounded_per_work_seek(node));
                    let once = accesses.iter().any(|node| {
                        node["Relation Name"] == "work_tag"
                            && node["Actual Loops"].as_f64().unwrap_or(0.0) <= 1.0
                    });
                    assert!(indexed || once,
                        "{label}/{kind}: neither expected seek nor single decorrelated tag access: {output}");
                    assert!(
                        !all.iter().any(|node| node["Relation Name"] == "work"
                            && node["Node Type"] == "Seq Scan"
                            && node["Actual Loops"].as_f64().unwrap_or(0.0) > 1.0),
                        "{label}/{kind}: repeated work-table scan: {output}"
                    );
                }
            }
        }
    }

    /// The viewer's content block rules are one more WHERE clause: they narrow the page
    /// AND the total, they are part of the memo key, and a tag rule reads `work_tag` by a
    /// covering seek per candidate row, never by a scan.
    #[tokio::test]
    async fn block_rules_narrow_the_page_and_the_total_and_key_the_memo() {
        use crate::block::{BlockRule, BlockSet};
        let pool = pool().await;
        fixture(&pool).await;
        // A: safe, B: suggestive, C: erotica, D: safe, E: pornographic.
        for (work_id, tag) in [
            ("w_a", "gore"),
            ("w_b", "gore"),
            ("w_c", "gore"),
            ("w_d", "harem"),
        ] {
            sqlx::query(
                "INSERT INTO work_tag (work_id, tag, ord, source, tag_key) \
                 VALUES ($1, $2, 0, 'mangadex', $3) ON CONFLICT DO NOTHING",
            )
            .bind(work_id)
            .bind(tag)
            .bind(tag)
            .execute(&pool)
            .await
            .unwrap();
        }
        let none: [&str; 0] = [];
        let set = |rules: Vec<BlockRule>| BlockSet::new(rules);
        let cases: [(BlockSet, &[&str]); 5] = [
            (set(vec![]), &["A", "B", "C", "D", "E"]),
            // A tag.
            (
                set(vec![BlockRule::new(["gore"], none).unwrap()]),
                &["D", "E"],
            ),
            // A rating. `browse_catalogue.content_rating` is the effective rating.
            (
                set(vec![BlockRule::new(none, ["pornographic"]).unwrap()]),
                &["A", "B", "C", "D"],
            ),
            // A tag and a rating: Gore, but only when adult.
            (
                set(vec![
                    BlockRule::new(["gore"], ["erotica", "pornographic"]).unwrap()
                ]),
                &["A", "B", "D", "E"],
            ),
            // Two rules: either one blocks.
            (
                set(vec![
                    BlockRule::new(["harem"], none).unwrap(),
                    BlockRule::new(none, ["safe"]).unwrap(),
                ]),
                &["B", "C", "E"],
            ),
        ];
        let mut keys: Vec<String> = Vec::new();
        for (blocks, want) in &cases {
            let query = BrowseQuery {
                blocks: Some(blocks),
                ..q(BrowseSort::Newest, 1)
            };
            let (total, got) = titles(&pool, &query).await;
            assert_eq!(got, *want, "rules {:?}", blocks.key());
            assert_eq!(total as usize, want.len(), "total for {:?}", blocks.key());
            keys.push(count_key(&query));
        }
        // Five rule sets, five memo entries: a total is never served across them.
        let unique: std::collections::HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "{keys:?}");
        // No rule and an empty set are one WHERE, so one key.
        assert_eq!(keys[0], count_key(&q(BrowseSort::Newest, 1)));

        // The rules cannot widen the NSFW gate: an opted-out viewer with a rule sees
        // the gate's works minus the blocked ones.
        let query = BrowseQuery {
            blocks: Some(&cases[1].0),
            show_nsfw: false,
            ..q(BrowseSort::Newest, 1)
        };
        assert_eq!(titles(&pool, &query).await, (1, vec!["D".to_string()]));

        // Verify that the bound block predicate permits an indexed tag probe.
        // This five-row fixture is too small for PostgreSQL's cost model to
        // prefer an index by default, so inspect the available path explicitly.
        let query = BrowseQuery {
            blocks: Some(&cases[3].0),
            ..q(BrowseSort::Newest, 1)
        };
        let (where_sql, binds, _) = build_where(&query);
        assert!(binds.is_empty(), "a rule adds no bind");
        let mut reader = pool.read().acquire().await.unwrap();
        reader.close_on_drop();
        sqlx::query("SELECT set_config('enable_seqscan','off',false)")
            .execute(&mut *reader)
            .await
            .unwrap();
        let output: serde_json::Value = sqlx::query_scalar(&format!(
            "EXPLAIN (FORMAT JSON) SELECT COUNT(*) FROM browse_catalogue f WHERE {where_sql}"
        ))
        .fetch_one(&mut *reader)
        .await
        .unwrap();
        let rendered = output.to_string();
        assert!(
            rendered.contains("idx_work_tag_work_key")
                || rendered.contains("idx_work_tag_key_work"),
            "no indexed block tag probe: {output}"
        );
    }
}
