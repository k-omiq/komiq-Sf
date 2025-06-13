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
//!    `WHERE (? = 1 OR f.is_nsfw = 0)` is opaque to the planner: it cannot tell `is_nsfw`
//!    is constant, so it cannot use the index's leading column and falls back to sorting
//!    all 115k rows through a temp B-tree. Migration 0064's header and `updates_feed` both
//!    document this. `comic_type` is branched for the same reason — as an equality it
//!    becomes an index prefix, so one index serves filter AND order.
//! 3. **Every sort carries `f.work_id DESC` as a tiebreaker.** Not cosmetic: ~76,000 rows
//!    share an `en_chapter_count` in the 0..5 range and 67,000 share `released_at IS NULL`,
//!    and without a total order LIMIT/OFFSET repeats or skips rows at a page boundary.

use anyhow::Result;
use sqlx::SqlitePool;

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

/// How many trending keys the TRENDING sort ranks before falling back to newest-first.
///
/// Trending is a top-N ranking by construction (`views::trending_keys` aggregates the last
/// 24 h of `series_view_bucket`), so a bound here is not an approximation of something
/// exact — it IS the ranking. 500 keys is ~17 pages deep at [`BROWSE_PAGE_SIZE`], far past
/// where anyone scrolls, and matches `catalog::RANK_WINDOW`'s precedent for the same
/// "rank a bounded window, then stop" decision. Production holds 43 bucket rows / 17
/// resolvable works today, so this is entirely headroom.
const TRENDING_KEY_LIMIT: i64 = 500;

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
        "SELECT wid, user_id, score FROM ( \
             SELECT wid, user_id, score, \
                    ROW_NUMBER() OVER (PARTITION BY wid, user_id \
                                       ORDER BY updated_at DESC, rid DESC) AS rn \
             FROM ( \
                 SELECT r.series_id AS wid, r.user_id AS user_id, r.score AS score, \
                        r.updated_at AS updated_at, r.id AS rid \
                   FROM reviews r \
                   WHERE r.series_id LIKE 'w!_%' ESCAPE '!' \
               UNION ALL \
                 SELECT ss.work_id AS wid, r.user_id AS user_id, r.score AS score, \
                        r.updated_at AS updated_at, r.id AS rid \
                   FROM reviews r \
                   JOIN source_series ss ON ss.source_key = r.series_id \
                                        AND ss.source_type = 'suwayomi' \
             ) \
         ) WHERE rn = 1"
    };
}
pub(crate) use rated_votes_sql;

const RATED_PER_WORK_CTE: &str = concat!(
    "WITH rated_per_work AS (SELECT wid, AVG(score) AS avg FROM (",
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
    pub work_id: String,
    /// The id the card NAVIGATES to, which is **not** always `work_id` — see
    /// [`BrowseQuery`]'s note and migration 0069.
    pub reader_id: String,
    pub title: String,
    pub cover_url: Option<String>,
    pub suwayomi_thumbnail: Option<String>,
    pub comic_type: Option<String>,
    pub status: Option<String>,
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
    pub latest_chapter: Option<String>,
    /// Newest upstream chapter release (epoch millis), or **`None` for a work with no dated
    /// chapter** — 67,000 of the 115,567 rows. NULLABLE is the whole point of migration
    /// 0069; `feed_series_updates.released_at` is NOT NULL by contract.
    pub released_at: Option<i64>,
    pub is_nsfw: bool,
    /// When the WORK entered the catalogue (ISO-8601). Read off the row rather than from the
    /// per-page `work` lookup, which is what lets a chapterless card carry an honest
    /// `updatedAt` — the only other candidate, `released_at`, is NULL for exactly those.
    pub created_at: String,
}

/// One filtered, sorted Browse request.
///
/// Every word-valued field is a fixed-vocabulary token the resolver has already mapped
/// from a GraphQL enum (`'MANGA'`, `'ONGOING'`, `'safe'` …), never raw client text — that
/// is what lets them be formatted into the SQL as branches rather than bound. `genres` is
/// the one client-controlled field and it is always BOUND.
pub struct BrowseQuery<'a> {
    /// The viewer's NSFW posture. `false` pins `f.is_nsfw = 0`; `true` emits no predicate
    /// at all (not `is_nsfw IN (0,1)`) so the opted-in branch reads a different index.
    pub show_nsfw: bool,
    /// Collapsed format word, or `None` for all formats.
    pub type_word: Option<&'a str>,
    /// Normalized status word, or `None` for any status.
    pub status_word: Option<&'a str>,
    /// The content-rating values this tier admits, or `None` for no rating clause.
    pub ratings: Option<&'a [&'a str]>,
    /// Restrict to adult-rated works only (`ContentRatingFilter::NSFW_ONLY`). Constrains
    /// `is_nsfw`, not `content_rating`, so it is independent of `ratings` above.
    pub nsfw_only: bool,
    /// Genre ANY-match. Canonicalized by [`canonical_genres`] before it gets here.
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
    pub min_rating: Option<f64>,
    pub max_rating: Option<f64>,
    pub sort: BrowseSort,
    pub page: i64,
}

/// A value bound into a dynamically-assembled statement. The SQL text is assembled from
/// fixed vocabulary only; everything variable travels through here.
#[derive(Clone)]
enum Bind {
    Text(String),
    Real(f64),
}

fn bind_rows<'q>(
    mut q: sqlx::query::QueryAs<'q, sqlx::Sqlite, BrowseRow, sqlx::sqlite::SqliteArguments<'q>>,
    binds: &[Bind],
) -> sqlx::query::QueryAs<'q, sqlx::Sqlite, BrowseRow, sqlx::sqlite::SqliteArguments<'q>> {
    for b in binds {
        q = match b {
            Bind::Text(s) => q.bind(s.clone()),
            Bind::Real(v) => q.bind(*v),
        };
    }
    q
}

fn bind_count<'q>(
    mut q: sqlx::query::QueryScalar<'q, sqlx::Sqlite, i64, sqlx::sqlite::SqliteArguments<'q>>,
    binds: &[Bind],
) -> sqlx::query::QueryScalar<'q, sqlx::Sqlite, i64, sqlx::sqlite::SqliteArguments<'q>> {
    for b in binds {
        q = match b {
            Bind::Text(s) => q.bind(s.clone()),
            Bind::Real(v) => q.bind(*v),
        };
    }
    q
}

fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
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
    let mut out: Vec<String> = genres
        .iter()
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out.truncate(MAX_GENRE_FILTERS);
    out
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
const COUNT_CACHE_CAP: usize = 2048;

/// The memo key for one filter combination.
///
/// Extracted so its completeness is directly testable: under `cfg(test)` [`COUNT_TTL`] is
/// zero, so [`browse_catalogue`] never reads the memo and no end-to-end test can catch a
/// key that omits a filter the COUNT depends on.
///
/// It must cover every input to that COUNT — and NOTHING ELSE. In particular it must NOT
/// include `sort` or `page`: all four sorts share one WHERE, so they share one total, and
/// keying on sort would quarter the hit rate for no gain. (`page` obviously cannot change a
/// total.) The two-phase sorts' `rated_count` / trending prefix length are separate,
/// unmemoized counts; they are not this number.
///
/// `v3|` PREFIX. The memo is a process-wide static and the totals it holds are computed over
/// whichever table this module read when they were stored. Each repointing has changed that
/// table's SCALE — 13,847 (`suwayomi_series`) → 48,567 (`feed_series_updates`, `v2`) →
/// 115,567 (`browse_catalogue`, `v3`) — and a stale entry under a colliding key would serve
/// one scale's `total` for another, which the pager renders as a page count and
/// `hasNextPage` as a promise of rows that do not exist. The prefix makes that impossible by
/// construction rather than by arguing about format overlap.
///
/// GENRE ENCODING. `search_catalogue` could join genres with `\u{1}` and call the result
/// unforgeable, because it keyed on the escaped LIKE PATTERNS (`%"Action"%`) and the
/// escaping guaranteed no pattern contained a bare `%`. This keys on RAW tags, where no
/// character is off-limits, so a separator-based join is forgeable: `["a","b"]` and
/// `["a<sep>b"]` would collide for any `<sep>`. Length-prefixed instead — each tag becomes
/// `{byte_len}:{tag}` and they are concatenated with no separator. That is injective
/// because the decoder is deterministic: read digits to the `:`, then take exactly that
/// many bytes. `["a","b"]` -> `1:a1:b`, while the single tag `a1:b` -> `4:a1:b`.
fn count_key(q: &BrowseQuery<'_>) -> String {
    let mut key = String::from("v3|");
    key.push(if q.show_nsfw { '1' } else { '0' });
    key.push('|');
    // Its own dimension: NSFW_ONLY emits no rating clause, so without this a cached
    // "adult only" total would be served for the unfiltered query and vice versa.
    key.push(if q.nsfw_only { '1' } else { '0' });
    key.push('|');
    // Tiers are cumulative allow-lists; joining their members is injective over them
    // because every member is a fixed lowercase word with no ','.
    if let Some(r) = q.ratings {
        key.push_str(&r.join(","));
    }
    key.push('|');
    key.push_str(q.type_word.unwrap_or(""));
    key.push('|');
    key.push_str(q.status_word.unwrap_or(""));
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
    if let Some(v) = q.min_rating {
        key.push_str(&v.to_string());
    }
    key.push('|');
    if let Some(v) = q.max_rating {
        key.push_str(&v.to_string());
    }
    key.push('|');
    // Last, and length-prefixed — see the doc comment. Nothing may be appended after this
    // segment without its own delimiter, since a tag can contain '|'.
    for g in q.genres {
        key.push_str(&format!("{}:{}", g.len(), g));
    }
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

/// The memo key for one PAGE of rows: the sort and the (clamped) page, then the whole
/// filter signature.
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
/// argument entirely — both are fixed vocabulary (`sort_word` is a closed set, the page a
/// decimal integer) so their `|` boundaries are unforgeable, and the count key keeps its
/// "genre segment is last" invariant intact.
///
/// The page is clamped with the same `max(1)` [`browse_catalogue`] applies, so page 0 and
/// page -5 share page 1's entry rather than caching duplicate copies of it.
fn row_key(q: &BrowseQuery<'_>) -> String {
    format!("{}|{}|{}", sort_word(q.sort), q.page.max(1), count_key(q))
}

/// One fixed lowercase word per sort, for [`row_key`]. A closed vocabulary (no client
/// text), so a bare `|` after it cannot be forged into looking like another key.
fn sort_word(sort: BrowseSort) -> &'static str {
    match sort {
        BrowseSort::Trending => "trending",
        BrowseSort::Newest => "newest",
        BrowseSort::Rating => "rating",
        BrowseSort::Chapters => "chapters",
    }
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
fn build_where(q: &BrowseQuery<'_>) -> (String, Vec<Bind>, bool) {
    let mut parts: Vec<String> = Vec::new();
    let mut binds: Vec<Bind> = Vec::new();

    // The NSFW gate, BRANCHED. `false` pins the index's leading column to a constant and
    // leaves the rest of the index in ORDER BY order (the hot, edge-cacheable path);
    // `true` emits nothing, which is what lets the planner pick the `is_nsfw`-less index
    // instead of sorting 48k rows. NEVER `(? = 1 OR f.is_nsfw = 0)`.
    if !q.show_nsfw {
        parts.push("f.is_nsfw = 0".into());
    }
    // NSFW_ONLY, the one non-cumulative content-rating value: the complement of SAFE
    // rather than a ceiling above it. Pushed as its own clause because it constrains
    // `is_nsfw`, not `content_rating`.
    //
    // Deliberately emitted even when the viewer is opted OUT, where it combines with the
    // gate above into `is_nsfw = 0 AND is_nsfw = 1` and yields an empty page. That is the
    // honest answer — they may not see those works — and it is why this is NOT skipped as
    // a redundant clause: dropping it would turn "adult only" into "everything you're
    // allowed to see", which is the opposite of what was asked for. The gate still reads
    // the index first, so the empty result costs a seek, not a scan.
    if q.nsfw_only {
        parts.push("f.is_nsfw = 1".into());
    }
    // Format. "Branched" means the CLAUSE is present or absent, not that the VALUE is
    // interpolated — it is bound like everything else. What the planner needs is a
    // single-column equality it can use as a seek prefix, and a bound `?` gives it that
    // (verified: every shape plans as `SEARCH … USING INDEX idx_fsu_* (is_nsfw=? AND
    // comic_type=?)`). It is `(? = 1 OR …)` that defeats it, because there the column is not
    // constrained at all.
    if q.type_word.is_some() {
        parts.push("f.comic_type = ?".into());
    }
    // Status: one equality over the normalized five-word vocabulary migration 0068
    // materializes. Un-normalized this could not be an equality at all — `komika_status`
    // has no `ON_HIATUS` member, so those rows would be unreachable by every filter value.
    if q.status_word.is_some() {
        parts.push("f.status = ?".into());
    }
    // Content rating, CUMULATIVE tiers. This narrows WITHIN the NSFW gate and can never
    // widen it: no `is_nsfw = 0` row carries `erotica` or `pornographic` in production, so
    // for an opted-out viewer the EROTICA and PORNOGRAPHIC tiers return exactly the
    // SUGGESTIVE set rather than revealing anything. ALL and PORNOGRAPHIC emit no clause
    // (they admit every stored value), which is also why they are cheaper than SAFE.
    if let Some(r) = q.ratings {
        if r.len() == 1 {
            parts.push("f.content_rating = ?".into());
        } else {
            parts.push(format!("f.content_rating IN ({})", placeholders(r.len())));
        }
    }
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
    // Binds, in the order the clauses above were pushed.
    if let Some(t) = q.type_word {
        binds.push(Bind::Text(t.to_string()));
    }
    if let Some(s) = q.status_word {
        binds.push(Bind::Text(s.to_string()));
    }
    for v in q.ratings.unwrap_or(&[]) {
        binds.push(Bind::Text(v.to_string()));
    }
    // Genre ANY-match. Deliberately NO `t.source` predicate, unlike
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
    if !q.genres.is_empty() {
        parts.push(format!(
            "EXISTS (SELECT 1 FROM work_tag t WHERE t.work_id = f.work_id AND t.tag IN ({}))",
            placeholders(q.genres.len())
        ));
        for g in q.genres {
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
                "COALESCE((SELECT rp.avg FROM rated_per_work rp WHERE rp.wid = f.work_id), 0) {op} ?"
            ));
            binds.push(Bind::Real(v));
        }
    }

    let where_sql = if parts.is_empty() {
        "1".to_string()
    } else {
        parts.join(" AND ")
    };
    (where_sql, binds, uses_rated)
}

/// The ORDER BY for a single-phase sort. Both keys are materialized columns with a
/// dedicated index per (NSFW branch x format branch) shape, so neither sorts.
///
/// `released_at DESC` puts the 67,000 works with a NULL `released_at` LAST, because SQLite
/// treats NULL as smaller than every value — so no `NULLS LAST` clause (which SQLite only
/// gained in 3.30 and which would defeat the index anyway) is needed. VERIFIED on a
/// 115,567-row copy of production: the first row is dated, the last is NULL, the 30-row
/// window straddling row 48,567 is 15 dated then 15 NULL, and the plan at that boundary is
/// still `SEARCH … USING COVERING INDEX idx_bc_order` with no temp B-tree.
fn order_by(sort: BrowseSort) -> &'static str {
    match sort {
        BrowseSort::Chapters => "f.en_chapter_count DESC, f.work_id DESC",
        // NEWEST, and the second phase of RATING/TRENDING, which page their unranked
        // remainder on this same key.
        _ => "f.released_at DESC, f.work_id DESC",
    }
}

/// Run one page statement.
// Internal helper mirroring the SQL builder's parameter list one-to-one; folding
// the arguments into a struct would just move the same eight names one level down.
#[allow(clippy::too_many_arguments)]
async fn fetch_page(
    pool: &SqlitePool,
    cte: bool,
    where_sql: &str,
    extra: &str,
    binds: &[Bind],
    order: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<BrowseRow>> {
    let prelude = if cte { RATED_PER_WORK_CTE } else { "" };
    let sql = format!(
        "{prelude}SELECT {BROWSE_COLS} FROM browse_catalogue f \
         WHERE {where_sql}{extra} ORDER BY {order} LIMIT ? OFFSET ?"
    );
    let q = bind_rows(sqlx::query_as::<_, BrowseRow>(&sql), binds);
    Ok(q.bind(limit).bind(offset).fetch_all(pool).await?)
}

/// Fetch `browse_catalogue` rows for an explicit, already-ordered list of work ids,
/// **preserving the caller's order**. Used by text search, which ranks in `work_fts` and
/// then needs the same card payload Browse renders — above all `reader_id`, without which a
/// Suwayomi-only hit would be handed to the client as a bare `w_…` that `canonicalSeries`
/// rejects (see `graphql::map_browse_rows`).
///
/// One indexed `IN` over the PRIMARY KEY, then a reorder in Rust. SQLite has no
/// `ORDER BY FIELD(...)`, and the alternatives — a `CASE` ladder of `ids.len()` branches, or
/// a values-CTE join — are both more SQL for a reorder of at most `BROWSE_PAGE_SIZE` rows.
///
/// A work in `work_fts` with NO `browse_catalogue` row is DROPPED rather than rendered from
/// a fallback. Both tables are rebuilt from the same predicate in the same chain
/// (`refresh_feed_updates`), so a miss means the work lost its last source or its title
/// between the two rebuilds — exactly the shell Browse also refuses to show. Silent by
/// design: the caller logs the count difference.
pub async fn rows_by_work_ids(pool: &SqlitePool, ids: &[String]) -> Result<Vec<BrowseRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {BROWSE_COLS} FROM browse_catalogue f WHERE f.work_id IN ({})",
        placeholders(ids.len())
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

/// Catalogue-wide Browse. Returns `(total, page)` where `total` counts exactly the row set
/// the page query walks.
pub async fn browse_catalogue(
    pool: &SqlitePool,
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

    let rows = match q.sort {
        BrowseSort::Newest | BrowseSort::Chapters => {
            fetch_page(
                pool,
                uses_rated,
                &where_sql,
                "",
                &binds,
                order_by(q.sort),
                page_size,
                offset,
            )
            .await?
        }
        // TWO-PHASE, both of them, for the same reason: the ranking key exists for a tiny
        // minority of works ("unrated last" / "never viewed last"), which makes the ranked
        // set a strict PREFIX of the result list. The naive form — `LEFT JOIN` the
        // aggregate and `ORDER BY avg DESC` — has to sort all 48k rows through a temp
        // B-tree on every page, including the ~99.99% of pages that contain no ranked row
        // at all. Phase 1 pages the ranked prefix; phase 2 runs only when the page reaches
        // past it and pages the remainder on `released_at DESC` off an existing index.
        BrowseSort::Rating => rating_page(pool, &where_sql, &binds, page_size, offset).await?,
        BrowseSort::Trending => {
            trending_page(pool, &where_sql, &binds, uses_rated, page_size, offset).await?
        }
    };
    store_rows(rkey, rows.clone());
    Ok((total, rows))
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
    pool: &SqlitePool,
    where_sql: &str,
    binds: &[Bind],
    page_size: i64,
    offset: i64,
) -> Result<Vec<BrowseRow>> {
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
        let sql = format!(
            "{RATED_PER_WORK_CTE}SELECT {BROWSE_COLS} FROM browse_catalogue f \
             JOIN rated_per_work rp ON rp.wid = f.work_id WHERE {where_sql} \
             ORDER BY rp.avg DESC, f.work_id DESC LIMIT ? OFFSET ?"
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
                order_by(BrowseSort::Newest),
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
    pool: &SqlitePool,
    where_sql: &str,
    binds: &[Bind],
    uses_rated: bool,
    page_size: i64,
    offset: i64,
) -> Result<Vec<BrowseRow>> {
    let ranked = trending_work_ids(pool, TRENDING_KEY_LIMIT).await;
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
            order_by(BrowseSort::Newest),
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
        placeholders(ranked.len())
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
        let extra = format!(" AND f.work_id NOT IN ({})", placeholders(ranked.len()));
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
                order_by(BrowseSort::Newest),
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
async fn trending_work_ids(pool: &SqlitePool, limit: i64) -> Vec<String> {
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
            placeholders(numeric.len())
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
pub async fn genre_facets(pool: &SqlitePool, show_nsfw: bool) -> Result<Vec<(String, i64)>> {
    // One of two hard-coded identifiers, never input.
    let col = if show_nsfw { "all_count" } else { "safe_count" };
    let sql = format!(
        "SELECT tag, {col} FROM feed_genre_facet WHERE {col} > 0 \
         ORDER BY {col} DESC, tag ASC"
    );
    Ok(sqlx::query_as::<_, (String, i64)>(&sql)
        .fetch_all(pool)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Insert one canonical work plus its `browse_catalogue` row, exactly as
    /// `catalog::refresh_browse_catalogue` would have written it.
    ///
    /// `released_at: None` is the state migration 0069 exists for: a browsable work with no
    /// dated chapter, which `feed_series_updates` cannot represent at all.
    #[allow(clippy::too_many_arguments)]
    async fn seed(
        pool: &SqlitePool,
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
             VALUES (?, ?, ?, ?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(work_id)
        .bind(title)
        .bind(status)
        .bind(content_rating)
        .bind(is_nsfw)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO browse_catalogue \
               (work_id, reader_id, title, comic_type, status, content_rating, \
                en_chapter_count, released_at, is_nsfw, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '2026-01-01T00:00:00Z')",
        )
        .bind(work_id)
        .bind(work_id)
        .bind(title)
        .bind(comic_type)
        .bind(status)
        .bind(content_rating)
        .bind(en_chapters)
        .bind(released_at)
        .bind(is_nsfw)
        .execute(pool)
        .await
        .unwrap();
    }

    fn q<'a>(sort: BrowseSort, page: i64) -> BrowseQuery<'a> {
        BrowseQuery {
            show_nsfw: true,
            type_word: None,
            status_word: None,
            ratings: None,
            nsfw_only: false,
            genres: &[],
            min_rating: None,
            max_rating: None,
            has_chapters: None,
            sort,
            page,
        }
    }

    async fn titles(pool: &SqlitePool, query: &BrowseQuery<'_>) -> (i64, Vec<String>) {
        let (t, rows) = browse_catalogue(pool, query).await.unwrap();
        (t, rows.into_iter().map(|r| r.title).collect())
    }

    /// A five-work catalogue with distinct sort keys on every axis.
    async fn fixture(pool: &SqlitePool) {
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
    async fn newest_and_chapters_sorts_order_the_whole_catalogue() {
        let pool = pool().await;
        fixture(&pool).await;
        let (total, t) = titles(&pool, &q(BrowseSort::Newest, 1)).await;
        assert_eq!(total, 5);
        assert_eq!(t, ["A", "B", "C", "D", "E"], "newest release first");
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
        for sort in [BrowseSort::Newest, BrowseSort::Chapters] {
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
            pool: &SqlitePool,
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
        // `is_nsfw = 1`, and the rating filter cannot reach past the gate.
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
        async fn only(pool: &SqlitePool, nsfw: bool) -> (i64, Vec<String>) {
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
        query.type_word = Some("MANGA");
        assert_eq!(
            titles(&pool, &query).await,
            (3, vec!["A".into(), "D".into(), "E".into()])
        );
        query.status_word = Some("ONGOING");
        assert_eq!(titles(&pool, &query).await, (1, vec!["A".into()]));
        // A status word no row carries is empty, not unfiltered.
        query.status_word = Some("HIATUS");
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
            sqlx::query("INSERT INTO work_tag (work_id, tag, ord, source) VALUES (?, ?, 0, ?)")
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
            "INSERT INTO users (id, username, email, password_hash, created_at) \
             VALUES ('u1','u','u@e','h','t')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (i, (w, score)) in [("w_0005", 9), ("w_0060", 4)].iter().enumerate() {
            sqlx::query(
                "INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) \
                 VALUES (?, ?, 'u1', ?, 'b', 't', 't')",
            )
            .bind(format!("rv{i}"))
            .bind(*w)
            .bind(*score)
            .execute(&pool)
            .await
            .unwrap();
        }
        let hour = chrono::Utc::now().timestamp() / 3600;
        for (w, v) in [("w_0009", 50i64), ("w_0011", 5)] {
            sqlx::query(
                "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES (?, ?, ?)",
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
            "INSERT INTO users (id, username, email, password_hash, created_at) \
             VALUES ('u1','u','u@e','h','t')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Rated prefix: w_1 (avg 9) then w_3 (avg 5).
        for (i, (w, score)) in [("w_1", 9), ("w_3", 5)].iter().enumerate() {
            sqlx::query(
                "INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) \
                 VALUES (?, ?, 'u1', ?, 'b', 't', 't')",
            )
            .bind(format!("rv{i}"))
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
            "INSERT INTO users (id, username, email, password_hash, created_at) \
             VALUES ('u1','u','u@e','h','t')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, key, score) in [("rv1", "4242", 10), ("rv2", "w_b", 6)] {
            sqlx::query(
                "INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) \
                 VALUES (?, ?, 'u1', ?, 'b', 't', 't')",
            )
            .bind(id)
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
            "INSERT INTO users (id, username, email, password_hash, created_at) \
             VALUES ('u1','u','u@e','h','t')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO reviews (id, series_id, user_id, score, body, created_at, updated_at) \
             VALUES ('rv1', 'w_b', 'u1', 8, 'b', 't', 't')",
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
                "INSERT INTO series_view_bucket (series_key, hour_ts, views) VALUES (?, ?, ?)",
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
            with(&|c| c.type_word = Some("MANGA")),
            with(&|c| c.type_word = Some("MANHWA")),
            with(&|c| c.status_word = Some("ONGOING")),
            with(&|c| c.status_word = Some("COMPLETED")),
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
            created_at: "2026-01-01T00:00:00Z".into(),
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
            sqlx::query("INSERT INTO feed_genre_facet (tag, safe_count, all_count) VALUES (?,?,?)")
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
}
