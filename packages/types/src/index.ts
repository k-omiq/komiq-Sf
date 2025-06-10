/**
 * Shared domain types for Komika.
 *
 * Modeled on the Tachiyomi / Suwayomi source API (SManga / SChapter),
 * extended with Komika's social layer (reviews, ratings, comments) and
 * the adaptive-scan / caching metadata that lives on the backend.
 */

/** A comic of any origin — the catalog indexes all of these. */
export type ComicType = 'MANGA' | 'MANHWA' | 'MANHUA' | 'WEBTOON' | 'COMIC';

/** Publication status. Drives the adaptive scanner (see `Series.scan`). */
export type SeriesStatus = 'ONGOING' | 'COMPLETED' | 'HIATUS' | 'CANCELLED' | 'UNKNOWN';

/** Reader layout modes (full Mihon reader parity). */
export type ReadingMode =
	| 'LTR' // paged, left-to-right
	| 'RTL' // paged, right-to-left (manga)
	| 'VERTICAL' // paged, top-to-bottom
	| 'WEBTOON' // continuous vertical, no page gaps
	| 'CONTINUOUS'; // continuous horizontal

export type Id = string;

/** Adaptive-scan configuration, admin-overridable in the "manga DB" console. */
export interface ScanPolicy {
	/** Rolling average interval between chapters, in hours (derived by backend). */
	avgIntervalHours: number;
	/** Admin override; when set, takes precedence over `avgIntervalHours`. */
	overrideIntervalHours: number | null;
	/** How often to re-poll once a series is "overdue", in minutes. */
	pollEveryMinutes: number;
	/** Effective (folded) paused state: forced override, else auto-by-status. */
	paused: boolean;
	/**
	 * Raw admin overrides — let the console distinguish an explicit choice from
	 * the status default even when they coincide. `null` = no override.
	 */
	statusOverride: SeriesStatus | null;
	pausedOverride: boolean | null;
	/** Raw admin poll-interval override (minutes); `null` = no override. */
	pollEveryMinutesOverride: number | null;
	lastScannedAt: string | null;
	nextScanAt: string | null;
}

/** Aggregate health of the adaptive scan scheduler (admin console). */
export interface ScanStatus {
	librarySize: number;
	overdueCount: number;
	lastTickAt: string | null;
	nextDueAt: string | null;
}

/** A user account as shown in the admin user-management console. */
export interface AdminUser {
	id: Id;
	username: string;
	email: string;
	avatarUrl: string | null;
	isAdmin: boolean;
	isBanned: boolean;
	createdAt: string;
}

/**
 * A pending mid-confidence dedup match awaiting manual admin review
 * (CATALOGUE.md §4). The matcher couldn't confidently auto-merge the source
 * series into `candidateWork`, so an admin confirms or rejects it.
 */
export interface MergeCandidate {
	id: Id;
	/** A `source_series` row id — NOT a work id; it cannot be looked up as one. */
	sourceSeriesId: Id;
	/** The work the source series belongs to — what the console links/expands on. */
	sourceWorkId: Id;
	candidateWorkId: Id;
	/** Title of the canonical work the matcher proposes merging into. */
	candidateTitle: string | null;
	/** Current title of the source series' own (provisional) work. */
	sourceTitle: string | null;
	/** Cover of the source work; empty when it has none to show. */
	sourceCoverUrl: string;
	/** Cover of the candidate work; empty when it has none to show. */
	candidateCoverUrl: string;
	/** Confidence score in [0,1] that produced the review verdict. */
	score: number;
	/** Which signal produced the match (e.g. `title_corroborated`, `fuzzy`). */
	method: string;
	/** Lifecycle: `pending` until an admin resolves it. */
	status: string;
	createdAt: string;
}

/**
 * One side of a dedup decision, expanded — what the review panel shows when an
 * admin clicks a candidate's title or cover. Admin-only, and read straight off the
 * canonical work rather than through the public series path, so a Suwayomi-only or
 * NSFW entry still expands (those are most of the review queue).
 */
export interface WorkReviewDetail {
	workId: Id;
	title: string | null;
	/** Empty when the work has no cover to show. */
	coverUrl: string;
	author: string | null;
	artist: string | null;
	year: number | null;
	status: string | null;
	contentRating: string | null;
	originalLanguage: string | null;
	isNsfw: boolean;
	description: string | null;
	/** Distinct mirrored chapter numbers — a coarse completeness signal. */
	chapterCount: number;
	/** Alternative titles, capped server-side. */
	altTitles: string[];
	/** Every catalogued source mapping, NSFW included. */
	sources: WorkSource[];
}

/**
 * A work whose cover the crawl could not process (admin "Bugs" panel). The crawl
 * records these so it stops re-attempting deterministically-unprocessable sources.
 */
export interface CoverIssue {
	workId: Id;
	title: string | null;
	/** Best-effort current cover URL (may be empty for a Suwayomi-only work). */
	coverUrl: string;
	/** Machine code: `too_large` | `unsupported` | `empty` | `encode` | `store`. */
	reason: string;
	/** Human-readable error from the most recent attempt. */
	detail: string | null;
	attempts: number;
	firstSeen: string;
	lastSeen: string;
}

/**
 * The result of the Tier-2 `addSourceSeries` dedup add flow: what the matcher
 * decided for the added source series and which canonical work it resolved to.
 */
export interface MatchResult {
	/** `auto_merge` | `review` | `new` | `existing` (already linked). */
	decision: string;
	/** The canonical work the source series is now linked to. */
	workId: Id;
	/** The pre-existing work the matcher proposed (for `auto_merge`/`review`). */
	matchedWorkId: string | null;
	/** Confidence score in [0,1] that produced the decision, if any. */
	score: number | null;
	/** Which signal produced the match, if any. */
	method: string | null;
	/** The source_series row created/linked. */
	sourceSeriesId: string;
}

/**
 * The result of folding one canonical work into another (admin `mergeWorks`):
 * the source work's mappings + user data re-point to the target and the source
 * work is deleted. Irreversible.
 */
export interface MergeWorksResult {
	/** The surviving (target) work id. */
	targetWorkId: Id;
	/** How many `source_series` mappings were re-pointed onto the target. */
	movedSourceSeries: number;
}

/**
 * One row of the canonical updates feed (CATALOGUE.md §6): a mirrored MangaDex
 * work with its most recent stored chapter, served from the `chapter` mirror.
 * Openable in the reader via its `workId` (the `w_`-prefixed canonical id) through
 * the `canonicalSeries` path.
 */
export interface CanonicalUpdate {
	workId: Id;
	mangadexId: string;
	title: string | null;
	isNsfw: boolean;
	/** Proxy-ready MangaDex cover thumbnail URL; null until the cover is synced. */
	coverUrl: string | null;
	/** Latest stored chapter number (string — chapters can be "10.5"). */
	latestChapter: string | null;
	latestChapterTitle: string | null;
	/** Publish time of the latest stored chapter (falls back to ingest time). */
	latestAt: string | null;
}

/**
 * One row of the reader's merged Updates feed (`updatesFeed`).
 *
 * The feed is the union of the MangaDex mirror updates and our scanner's Suwayomi
 * detections, taken server-side and keyed by canonical work, so it can be paginated
 * with an honest `total`. The reader used to fetch page 1 of each feed, merge, dedupe
 * by lowercased title and cap the result at 60 cards; that is what this replaces.
 */
export interface UpdateFeedRow {
	/** The id to OPEN: a `w_`-prefixed canonical work id when the work is
	 *  MangaDex-anchored, else the numeric Suwayomi series id. Not always `workId` —
	 *  `canonicalSeries` rejects a work with no MangaDex anchor. */
	id: Id;
	/** The canonical work this row is one-per-of; the feed's dedupe key. */
	workId: Id;
	title: string;
	coverUrl: string | null;
	/** Effective format, filtered server-side via the `type` argument. */
	type: ComicType | null;
	/** Chapter NUMBER of the newest mirrored chapter ("10.5"); null on scanner-only rows. */
	latestChapter: string | null;
	latestChapterTitle: string | null;
	/** Total chapters known for the series; null on mirror-only rows. */
	chapterCount: number | null;
	/** Real upstream release time of the newest chapter — the feed's sort key AND the
	 *  instant the card labels with, so order and label can never disagree. */
	releasedAt: string;
	/** When OUR scanner noticed; null on mirror-only rows. Tooltip only, never a sort key. */
	detectedAt: string | null;
	/** Aggregate user rating 0-10, or null when unrated. */
	rating: number | null;
	isNsfw: boolean;
}

/**
 * One Keiyoushi/Mihon extension as known to the Suwayomi engine — the admin
 * management view (installed or not). CATALOGUE.md §2 Tier-2.
 */
export interface ExtensionInfo {
	/** Extension package name (e.g. `eu.kanade.tachiyomi.extension.all.mangadex`). */
	pkgName: string;
	name: string;
	lang: string;
	versionName: string;
	isInstalled: boolean;
	/** True when the store carries a newer version than the installed one. */
	hasUpdate: boolean;
	isNsfw: boolean;
	iconUrl: string | null;
	/** The store/repo the extension came from, when reported. */
	repo: string | null;
	/** Subscribed for background source-sync (auto-discovery of new series). */
	subscribed: boolean;
}

/** One installed Suwayomi source — the admin picker feeding `sourceBrowse`. */
export interface SourceInfo {
	/** The Suwayomi source id (the `sourceBrowse` input). */
	id: Id;
	/** User-facing display name (per-language variants, e.g. "MangaDex (EN)"). */
	name: string;
	lang: string;
	isNsfw: boolean;
	iconUrl: string | null;
	/** The owning extension's pkgName, when reported. */
	pkgName: string | null;
}

/** Which listing of a source to browse. */
export type SourceBrowseType = 'POPULAR' | 'LATEST' | 'SEARCH';

/**
 * One manga returned by browsing a source (admin bulk-ingest picker). The id is
 * Suwayomi's internal manga id — exactly what `bulkAddSourceSeries` consumes.
 */
export interface SourceBrowseEntry {
	suwayomiMangaId: Id;
	title: string;
	thumbnailUrl: string | null;
	inLibrary: boolean;
}

/** A page of source-browse results. */
export interface SourceBrowsePage {
	items: SourceBrowseEntry[];
	page: number;
	hasNextPage: boolean;
}

/**
 * Per-id outcome of `bulkAddSourceSeries`: the dedup {@link MatchResult} on
 * success, or the error that made this one id fail (other ids still proceed).
 */
export interface BulkAddEntry {
	suwayomiMangaId: Id;
	result: MatchResult | null;
	error: string | null;
}

/**
 * A background "add all from this source" ingest job (S1): walks a source's
 * listing page by page and runs every entry through the Tier-2 dedup add flow,
 * persisting progress counters on the row. Poll {@link Backend.sourceIngestJobs}
 * for updates.
 */
export interface SourceIngestJob {
	id: Id;
	sourceId: string;
	/** `running` | `completed` | `cancelled` | `failed`. */
	state: string;
	pagesDone: number;
	itemsSeen: number;
	succeeded: number;
	failed: number;
	newWorks: number;
	autoMerged: number;
	queuedForReview: number;
	alreadyExisting: number;
	/** Failure detail when `state` is `failed`; null otherwise. */
	error: string | null;
	startedAt: string;
	/** Set once the job reaches a terminal state (completed/cancelled/failed). */
	finishedAt: string | null;
}

/** Result of a bulk catalogue ingest: per-id entries plus a decision summary. */
export interface BulkAddResult {
	entries: BulkAddEntry[];
	total: number;
	succeeded: number;
	failed: number;
	/** Decision counts across the succeeded entries. */
	newWorks: number;
	autoMerged: number;
	queuedForReview: number;
	alreadyExisting: number;
}

/**
 * Extension coordinates for a source that a native client fetches through an
 * embedded Suwayomi runtime: where to get the APK and which package/version to
 * load. Null on a WorkSource that Komika fetches MangaDex-natively.
 */
export interface SourceExtension {
	/** Suwayomi extension package name (e.g. `eu.kanade.tachiyomi.extension.en.mangadex`). */
	pkgName: string;
	/** Base URL of the extension repository serving the APK. */
	repoUrl: string;
	/** APK filename within the repo; null when not pinned. */
	apkName: string | null;
	/** Extension version code; null when unknown. */
	versionCode: number | null;
	/** Language code the extension targets (e.g. `en`); null when N/A. */
	lang: string | null;
}

/**
 * One source mapping for a canonical work: which source (and, for Suwayomi, which
 * extension) a native client uses to fetch it, plus the source-local coordinates.
 * The canonical dedup model (CATALOGUE.md §2) may map several of these to one work.
 */
export interface WorkSource {
	/** `"mangadex"` (MangaDex-native fetch) or `"suwayomi"` (via an extension). */
	sourceType: string;
	/** Suwayomi source id. */
	sourceId: string;
	/** Manga id/slug identifying the series within the source. */
	sourceKey: string;
	/** Canonical source URL for the series; null when not available. */
	sourceUrl: string | null;
	isNsfw: boolean;
	/** Language code of this mapping (e.g. `en`); null when N/A. */
	lang: string | null;
	/** Extension coordinates; null for MangaDex-native fetch. */
	extension: SourceExtension | null;
}

/** All source mappings resolved for one canonical work (batch grouping). */
export interface WorkSourceGroup {
	workId: Id;
	sources: WorkSource[];
}

/**
 * One source/extension that carries a canonical work — a "translator" in the
 * reader UI (federated multi-extension search, S3). For a Suwayomi source,
 * `suwayomiMangaId` is exactly the id the reader passes to `chapters(seriesId:)`
 * to fetch THIS translator's chapters; for the MangaDex canonical spine
 * (`sourceType = "mangadex"`), `suwayomiMangaId` is null and chapters come from
 * `canonicalChapters(workId)` instead.
 */
export interface Translator {
	/** `"mangadex"` (the canonical spine) or `"suwayomi"` (an installed extension). */
	sourceType: string;
	sourceId: string;
	/** Display name (e.g. "MangaDex (EN)", "MANGA Plus by SHUEISHA (EN)"); null when
	 * the source isn't currently installed on the engine. */
	sourceName: string | null;
	lang: string | null;
	/** Suwayomi manga id to fetch this translator's chapters with
	 * (`chapters(seriesId:)`). Null for the MangaDex canonical spine. */
	suwayomiMangaId: Id | null;
	extensionPkgName: string | null;
	/** Browser-reachable store-hosted PNG icon for the extension; null when unknown. */
	extensionIconUrl: string | null;
}

/** One consolidated federated-search hit: a canonical work as a `Series`, plus
 * the per-source translator list gathered across every installed extension. */
export interface FederatedSeries {
	series: Series;
	translators: Translator[];
}

/** A page of federated multi-extension search results (S3). */
export interface FederatedSearchPage {
	items: FederatedSeries[];
	page: number;
	hasNextPage: boolean;
	/** How many installed sources were actually queried in the fan-out (diagnostic). */
	sourcesQueried: number;
}

/**
 * Admin catalogue provenance for one Suwayomi series (batched): the canonical
 * work it's linked to and every source mapping on that work. `workId` is null
 * (and `sources` empty) when the series hasn't been catalogued into a work yet.
 */
export interface SeriesSourceGroup {
	seriesId: Id;
	workId: Id | null;
	sources: WorkSource[];
}

/** Aggregate rating summary for a series. */
export interface RatingSummary {
	/** Mean score on a 1–10 scale. */
	average: number;
	count: number;
	/** Histogram: index 0 => score 1, index 9 => score 10. */
	distribution: number[];
}

/**
 * The shelf a viewer can explicitly file a library series under. Distinct from the
 * progress-derived shelf: an explicit value overrides derivation, `null` restores
 * it. `onhold` has no derived equivalent — it only exists when set by hand.
 */
export type LibraryStatus = 'reading' | 'completed' | 'onhold' | 'plan';

export interface Series {
	id: Id;
	title: string;
	altTitles: string[];
	author: string | null;
	artist: string | null;
	description: string | null;
	genres: string[];
	type: ComicType;
	status: SeriesStatus;
	/** Cover image — resolved through an ImageProvider, never fetched directly. */
	coverUrl: string;
	/** Opaque backend source identifier (which Suwayomi extension it came from). */
	sourceId: string;
	rating: RatingSummary;
	chapterCount: number;
	/** True if the current user marked it (raises scanning priority). */
	isMarked: boolean;
	/**
	 * The shelf the viewer explicitly filed this series under, or null to derive
	 * it from read progress. Per-viewer; only selected by library/series-detail
	 * queries (feeds omit it), so it may be `undefined` on a Series from a feed.
	 */
	libraryStatus?: LibraryStatus | null;
	/** Whether the viewer favourited this series. Per-viewer; see `libraryStatus`. */
	isFavorite?: boolean;
	/**
	 * NSFW per the canonical model (CATALOGUE.md §2); false until the series is
	 * catalogued. Discovery/search/updates already filter by the viewer's
	 * `showNsfw` preference server-side — this lets the UI badge/flag it too.
	 */
	isNsfw: boolean;
	/**
	 * The canonical `w_` work this series belongs to, or null when the catalogue has
	 * no mapping for it.
	 *
	 * Only meaningful — and only selected — on the single-series detail query: it is a
	 * resolver field costing a query per series, so feeds and browse grids omit it and
	 * see `undefined`. The reader uses it to turn a numeric Suwayomi id (which browse
	 * links for Suwayomi-anchored works) into the work whose sibling source mappings
	 * the source picker offers.
	 */
	workId?: Id | null;
	scan: ScanPolicy;
	createdAt: string;
	updatedAt: string;
	/**
	 * Real newest-chapter time (ISO) — the "recently updated" clock.
	 *
	 * `updatedAt` is a METADATA touch that moves on every poll, so labelling a card
	 * with it renders "0h" for a series whose last chapter shipped a week ago (the
	 * server documents this on `Series.updated_at`). `latestChapterAt` only advances
	 * when a new chapter is actually published upstream, so it is what feeds, cards
	 * and the series header label and sort by.
	 *
	 * Optional: adapters that don't track chapter recency (Suwayomi/mock) omit it,
	 * and the server sends an empty string when no dated chapter is cached — callers
	 * fall back to `updatedAt`.
	 */
	latestChapterAt?: string;
	/**
	 * When OUR catalogue first detected the series' newest chapter — discovery
	 * time, as distinct from the upstream publish time in `latestChapterAt`.
	 *
	 * Optional AND nullable: not every deployed server exposes it (see
	 * `OPTIONAL_SERIES_FIELDS` in `@komika/api`, which drops it from the query
	 * document against an older API), so treat its absence as "unknown" — never as
	 * "just now", and never as a substitute for `latestChapterAt`.
	 */
	detectedAt?: string | null;
	/**
	 * Per-series view (chapter-read) counts — the popularity signal. Resolved lazily
	 * server-side and selected only by the series-detail queries, so it's `undefined`
	 * on series that come from feeds/search (which don't request it).
	 */
	views?: SeriesViews;
	/**
	 * Opt-in enrichment (S2), populated only when selected AND the series id is a
	 * canonical `w_` work id — MangaDex's per-language descriptions and the full
	 * author/artist credit list. Undefined on documents that don't select them and
	 * empty for a non-canonical series or an un-enriched work.
	 */
	localizedDescriptions?: LocalizedDescription[];
	credits?: Credit[];
	/**
	 * The full MangaDex cover set (F2), opt-in and canonical-`w_`-works only:
	 * primary first, then volume-ordered. `coverUrl` (the single primary) is
	 * unchanged. Undefined on documents that don't select it, empty for a
	 * non-canonical/un-enriched work.
	 */
	covers?: Cover[];
}

/** Per-series view counts across the three windows (the popularity signal). */
export interface SeriesViews {
	/** All-time total views. */
	total: number;
	/** Views in the last 7 days. */
	last7d: number;
	/** Views in the last 24 hours. */
	last24h: number;
}

/** One MangaDex-sourced description in a specific language (S2 enrichment). */
export interface LocalizedDescription {
	/** BCP-47-ish language tag, e.g. `en`, `ja`, `pt-br`. */
	lang: string;
	description: string;
}

/** One MangaDex cover for a canonical work (F2). `url`/`thumbnailUrl` are
 *  proxy-ready (`uploads.mangadex.org` hosts) — resolve through the ImageProvider. */
export interface Cover {
	fileName: string;
	/** Proxy-ready full-resolution cover URL. */
	url: string;
	/** Proxy-ready 512px thumbnail URL. */
	thumbnailUrl: string;
	/** Cover language tag, when known (e.g. `ja`, `en`). */
	lang: string | null;
	/** Volume label this cover belongs to, when known (e.g. `1`, `27`). */
	volume: string | null;
	isPrimary: boolean;
}

/** One author/artist credit on a canonical work (S2 enrichment). */
export interface Credit {
	/** `"author"` or `"artist"`. */
	role: string;
	name: string;
}

/** One genre/tag facet across the persisted catalogue (S4) — for the search
 *  genre filter. The full set the sources provide, most-common first. */
export interface GenreFacet {
	genre: string;
	count: number;
}

/** Per-series read progress for the whole library (see `Backend.libraryProgress`).
 *  `read`/`total` are counted from the cached chapter state — the same source
 *  `chapters(seriesId)` reads — so they match. Series with no cached chapters are
 *  omitted from the list; callers treat those as unread. */
export interface SeriesProgress {
	id: Id;
	read: number;
	total: number;
}

/** One source that provides a given chapter number of a work (S2 aggregation).
 *  `suwayomiMangaId` (Suwayomi source) is the id to pass to `chapters(seriesId:)`
 *  to read this source's copy; null = the MangaDex mirror. `chapterId` is the
 *  specific chapter to open. */
export interface ChapterSource {
	sourceType: string;
	sourceId: string;
	suwayomiMangaId: Id | null;
	chapterId: Id;
	scanlator: string | null;
}

/** One aggregated chapter of a work — a chapter NUMBER available across one or
 *  more sources (S2). Deduped by number; `sources` keeps per-source availability
 *  so the reader can pick which translator/source to read it from. */
export interface AggregatedChapter {
	number: number;
	title: string | null;
	sources: ChapterSource[];
}

export interface Chapter {
	id: Id;
	seriesId: Id;
	/** Chapter number as a float (supports 10.5 interludes). */
	number: number;
	title: string | null;
	pageCount: number;
	uploadedAt: string | null;
	scanlator: string | null;
	// per-user reading state
	read: boolean;
	lastPageRead: number;
	bookmarked: boolean;
	isDownloaded: boolean;
}

/** One source mapping of a {@link SeriesPageBundle}: the mapping plus what the old
 *  per-source round trips fetched — its chapter list and its series metadata.
 *  `chapters` is empty (and `series` null) for the MangaDex mapping (the spine
 *  carries those) and for a source whose server-side fetch failed. */
export interface SeriesPageSourceEntry {
	source: WorkSource;
	chapters: Chapter[];
	series: Series | null;
}

/** P2.1: the whole series/reader page in ONE round trip (`seriesPage(workId)`).
 *  Replaces the canonicalSeries + canonicalChapters + workSources fan-out and the
 *  per-translator chapters N+1. `workId` is the EFFECTIVE id after a merge
 *  redirect; `series` is null for a federation-only work (no MangaDex anchor). */
export interface SeriesPageBundle {
	workId: Id;
	series: Series | null;
	spineChapters: Chapter[];
	sources: SeriesPageSourceEntry[];
	/** Cross-source aggregation; present only when requested (`withAggregated`). */
	aggregated?: AggregatedChapter[];
}

/**
 * A single page reference. `sourceUrl` is the upstream image URL supplied by the
 * backend; it is NEVER used as an <img src> directly. An ImageProvider turns it
 * into a displayable URL (Worker-proxied on web, Rust-fetched on native).
 */
export interface Page {
	index: number;
	sourceUrl: string;
	width?: number;
	height?: number;
}

// ---- Social layer ------------------------------------------------------------

export interface UserRef {
	id: Id;
	username: string;
	avatarUrl: string | null;
}

/** Series-level review with a 1–10 score. */
export interface Review {
	id: Id;
	seriesId: Id;
	author: UserRef;
	score: number;
	body: string;
	hasSpoiler: boolean;
	createdAt: string;
	updatedAt: string;
}

/** Comment target: a chapter thread or a series-level discussion. */
export type CommentTargetType = 'chapter' | 'series';

/**
 * A comment on a polymorphic target — a per-chapter thread (`targetType: 'chapter'`)
 * or a series-level discussion (`targetType: 'series'`). Distinct from a Review, which
 * is a scored, one-per-user verdict.
 */
export interface Comment {
	id: Id;
	targetType: CommentTargetType;
	targetId: Id;
	/** The comment this replies to, or `null` for a top-level comment. Comments
	 *  form arbitrary-depth reply trees via this field. */
	parentId: Id | null;
	author: UserRef;
	body: string;
	hasSpoiler: boolean;
	/** Path to the attached image (`/comment-media/<id>.webp`), or `null`. */
	mediaUrl: string | null;
	/** Attached image pixel dimensions, so the client can reserve layout space. */
	mediaWidth: number | null;
	mediaHeight: number | null;
	createdAt: string;
	/** Like / dislike tallies, and the viewer's own vote: 1 (like), -1 (dislike),
	 *  0 (none). */
	likes: number;
	dislikes: number;
	myVote: number;
}

/** The result of `voteComment`: a comment's fresh vote tallies + the viewer's vote. */
export interface CommentVote {
	likes: number;
	dislikes: number;
	myVote: number;
}

/** One inbound notification (the bell feed). `actor` is the triggering user (a
 *  replier); null for an aggregate `like_milestone`. `targetType`/`targetId` deep-link
 *  to the thread; `commentExcerpt` is a snippet of the viewer's own comment. */
export interface Notification {
	id: Id;
	/** `'reply'` | `'like_milestone'`. */
	kind: string;
	actor: UserRef | null;
	commentId: Id | null;
	commentExcerpt: string | null;
	targetType: string | null;
	targetId: Id | null;
	/** Owning series for a `chapter`-target notification, for deep-linking to the
	 *  reader (`/read/<seriesId>?ch=<targetId>`); null for `series` targets and
	 *  unresolvable chapters. */
	seriesId: Id | null;
	/** Milestone value for `like_milestone` (e.g. 10 = "reached 10 likes"). */
	count: number | null;
	createdAt: string;
	read: boolean;
}

// ---- Discovery ---------------------------------------------------------------

export type DiscoveryFeedKind =
	'POPULAR' | 'TRENDING' | 'RECENTLY_UPDATED' | 'RECENTLY_ADDED' | 'GENRE';

export interface DiscoveryFeed {
	kind: DiscoveryFeedKind;
	title: string;
	/** Set when kind === "GENRE". */
	genre?: string;
	items: Series[];
}

export interface Paginated<T> {
	items: T[];
	page: number;
	hasNextPage: boolean;
	total: number | null;
}
