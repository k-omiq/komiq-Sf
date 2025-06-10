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
/// ordering that reflects what people are actually reading; the other three are the
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
    pub rating: RatingSummary,
    pub scan: ScanPolicy,
    pub created_at: String,
    /// Last metadata touch. Do NOT use as "recently updated" — it moves on every poll
    /// and cover/metadata refresh. For chapter recency use `latest_chapter_at`.
    pub updated_at: String,
    /// Real newest-chapter time (ISO), from the actual chapter publish/upload date.
    /// Empty when the series has no dated chapter cached yet. This is the field feeds
    /// and the reader's "· 4h" label should use.
    pub latest_chapter_at: String,
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
/// those pages). `read`/`total` are counted from the cached chapter state — the
/// same source `chapters(seriesId)` reads — so the numbers match exactly.
#[derive(SimpleObject, Clone)]
pub struct SeriesProgress {
    /// The Suwayomi series id (matches `Series.id` for library series).
    pub id: ID,
    /// Chapters marked read.
    pub read: i32,
    /// Total chapters known for the series (cached chapter count).
    pub total: i32,
}

#[derive(SimpleObject, Clone)]
pub struct Chapter {
    pub id: ID,
    pub series_id: ID,
    pub number: f64,
    pub title: Option<String>,
    pub page_count: i32,
    pub uploaded_at: Option<String>,
    pub scanlator: Option<String>,
    pub read: bool,
    pub last_page_read: i32,
    pub bookmarked: bool,
    pub is_downloaded: bool,
}

#[derive(SimpleObject, Clone)]
pub struct Page {
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

/// A comment on a polymorphic target: a chapter thread (`target_type = "chapter"`)
/// or a series discussion (`target_type = "series"`). Comments form arbitrary-depth
/// reply trees via `parent_id` (NULL = a top-level comment) and may carry one
/// optional attached image (`media_url` + its pixel dimensions).
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
    pub items: Vec<AdminUser>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

#[derive(SimpleObject, Clone)]
pub struct SeriesPage {
    pub items: Vec<Series>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

#[derive(SimpleObject, Clone)]
pub struct ReviewPage {
    pub items: Vec<Review>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

#[derive(SimpleObject, Clone)]
pub struct CommentPage {
    pub items: Vec<Comment>,
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
    /// `"chapter"` or `"series"`.
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

/// One source that provides a given chapter number of a work (S2 aggregation).
/// `suwayomiMangaId` (for a Suwayomi source) is the id the reader passes to
/// `chapters(seriesId:)` to read this source's copy; `chapterId` is the specific
/// chapter to open (a Suwayomi chapter id, or a MangaDex chapter uuid).
#[derive(SimpleObject, Clone)]
pub struct ChapterSource {
    pub source_type: String,
    pub source_id: String,
    pub suwayomi_manga_id: Option<ID>,
    pub chapter_id: ID,
    pub scanlator: Option<String>,
}

/// One aggregated chapter of a work — a chapter NUMBER available across one or
/// more sources (S2). Deduped by number; `sources` keeps per-source availability
/// so the reader can pick a translator.
#[derive(SimpleObject, Clone)]
pub struct AggregatedChapter {
    pub number: f64,
    pub title: Option<String>,
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
    let s = v?;
    let n: i64 = s.parse().ok()?;
    if n <= 0 {
        return None;
    }
    // `checked_mul` so a pathological seconds value can't overflow i64 (panic in
    // debug, wrong date in release) when scaled to millis.
    let ms = if n > 1_000_000_000_000 {
        n
    } else {
        n.checked_mul(1000)?
    };
    chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339())
}

/// Map a Suwayomi chapter onto the Komika `Chapter`. Read state is per-VIEWER
/// (`suwayomi_progress`), passed in as `progress`; the cached `is_read`/
/// `last_page_read` on the shared `suwayomi_chapter` row are ignored — they were
/// global state. Anonymous / never-read chapters default to unread (mirrors the
/// canonical `map_canonical_chapter` path).
pub fn map_chapter(c: SuwayomiChapter, progress: Option<(i32, bool)>) -> Chapter {
    let (last_page_read, read) = progress.unwrap_or((0, false));
    Chapter {
        id: ID(c.id.to_string()),
        series_id: ID(c.manga_id.to_string()),
        number: c.chapter_number,
        title: Some(c.name),
        page_count: c.page_count as i32,
        uploaded_at: to_iso(c.upload_date.as_deref()),
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
