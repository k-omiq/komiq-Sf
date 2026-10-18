import type {
	ChapterOrigin,
	AdminUser,
	MediaRoute,
	CoverIssue,
	AggregatedChapter,
	BulkAddResult,
	CanonicalUpdate,
	Chapter,
	ComicType,
	Comment,
	CommentTargetType,
	CommentVote,
	DiscoveryFeed,
	ExtensionInfo,
	FederatedSearchPage,
	GenreFacet,
	Id,
	LibraryStatus,
	MatchResult,
	MergeCandidate,
	MergeCandidateRow,
	DuplicateSweepResult,
	MergeWorksResult,
	Notification,
	Page,
	Paginated,
	Report,
	ReportInput,
	ReportKind,
	ReportPage,
	ReportStatus,
	Review,
	ScanStatus,
	Series,
	SeriesPageBundle,
	SeriesProgress,
	SeriesSourceGroup,
	SeriesStatus,
	SourceBrowsePage,
	SourceBrowseType,
	SourceIngestJob,
	SourceInfo,
	SourceScanHealth,
	ExtensionAutoUpdate,
	TagInfo,
	SplitSourcesResult,
	UpdateFeedRow,
	WorkReviewDetail,
	WorkSource,
	WorkSourceGroup,
	WorkSourceRow,
	ChapterWork,
	ProgressResult,
	ViewerProgress,
} from '@komika/types';

/**
 * The unified Komika backend (Suwayomi/Tachidesk-style server + our services).
 *
 * It owns: identity, the auto-served catalog, chapter/page metadata, library &
 * reading-progress sync, the social layer, and the admin "manga DB". Image bytes
 * are NOT served here — those flow through an ImageProvider.
 *
 * This is the contract the UI codes against. A concrete GraphQL implementation
 * is wired in `graphql-backend.ts`.
 */
export interface Backend {
	// --- auth ---
	session(): Promise<Session | null>;
	login(username: string, password: string): Promise<Session>;
	register(input: RegisterInput): Promise<Session>;
	logout(): Promise<void>;
	/** Public server facts (setup state, sign-up switch) — no sign-in needed.
	 * Optional: only the unified Komika API implements it. */
	instance?(): Promise<InstanceInfo>;
	/** First-run setup: create the owner (first admin) account with the setup
	 * code from the server log, and sign in. Works only while
	 * {@link InstanceInfo.setupRequired}. Optional. */
	createOwner?(input: CreateOwnerInput): Promise<Session>;
	/** Change the signed-in user's own password; signs out their other sessions
	 * and clears `mustChangePassword`. Optional. */
	changePassword?(currentPassword: string, newPassword: string): Promise<Session['user']>;
	/**
	 * Set (or clear) the bearer token used for authenticated requests. Optional:
	 * only backends with real auth (the unified Komika API) implement it; the
	 * Suwayomi adapter has no auth and omits it. Call after login/logout.
	 */
	setToken?(token: string | null): void;
	/**
	 * Bind the backend to the currently-authenticated user id (or `null` when
	 * signed out). Distinct from {@link setToken}: the token authenticates a
	 * request, this identifies the account so an offline write-queue can attribute
	 * and scope replays and never apply one user's queued writes under another
	 * account (C1). Optional: only the {@link CompositeBackend} (native offline
	 * queue) needs it; other backends omit it. Call on login / register / session
	 * restore / logout, before the token is dropped.
	 */
	setCurrentUser?(userId: Id | null): void;
	/**
	 * A view of this backend that issues its requests through `fetch` instead of the
	 * global one, sharing all of this backend's mutable state (the bearer token above
	 * included). Optional: only the GraphQL backend implements it; callers fall back to
	 * the backend itself.
	 *
	 * The caller this exists for is a SvelteKit `load`, which is handed a special
	 * `fetch`. Requests made with it during edge SSR are serialized into the HTML, and
	 * the hydration re-run of the same load replays them from there instead of going
	 * back over the network. On the home page that is the difference between two
	 * cross-origin GraphQL round trips on the critical path and none.
	 *
	 * The returned view is for the duration of ONE load and nothing else: hold it past
	 * that and it keeps a `fetch` bound to a request that has finished.
	 */
	withFetch?(fetch: typeof globalThis.fetch): Backend;

	// --- discovery / catalog (auto-served, no source picking) ---
	discovery(): Promise<DiscoveryFeed[]>;
	/**
	 * The Updates feed: library series with newly-detected chapters, newest-first.
	 * On the unified Komika API this is driven by the adaptive scanner
	 * (`series_scan_state.last_new_chapter_at`); the Suwayomi adapter approximates
	 * it with the source "Latest" endpoint.
	 */
	updates(page?: number): Promise<Paginated<Series>>;
	/**
	 * Catalogue search AND Browse. An EMPTY query browses the whole persisted canonical
	 * catalogue; a text query runs full-text search over it. `filters` are applied
	 * server-side — every filter and the ordering resolve in SQL, so `total` /
	 * `hasNextPage` describe the FILTERED set, not the page. (S4)
	 *
	 * The two branches honour different filters. `genres` (match ANY, case-insensitive)
	 * and the rating range apply to both; `types` / `status` / `sort` / `contentRating`
	 * apply to the BROWSE branch ONLY — a text query is ordered by relevance and ignores
	 * them. Callers must not present them as narrowing a search.
	 *
	 * BROWSE PAGES AT 40, not the API-wide 20 (server `BROWSE_PAGE_SIZE`); a pager built
	 * on the wrong constant mis-computes the last page and clamps to it.
	 *
	 * `includeNsfw` is the ADMIN CONSOLE escape hatch: it overrides the caller's own
	 * `show_nsfw` for this request only, and the server honours it solely for an admin
	 * (anonymous callers are forced to false before it is read; ordinary users fall
	 * through to their stored preference), so it grants nothing to a non-admin. Reader
	 * code paths must leave it undefined.
	 */
	search(
		query: string,
		page?: number,
		filters?: SearchFilters,
		includeNsfw?: boolean,
	): Promise<Paginated<Series>>;
	/**
	 * Federated multi-extension catalogue search (S3): fans the query out to every
	 * installed source, dedupes to one canonical work per series, and returns each
	 * work with its per-source translator list. ADMIN ONLY (it persists mappings);
	 * NSFW-gated by the caller's `show_nsfw` posture (same as {@link search}).
	 * Optional: only the unified Komika API implements it.
	 */
	searchAllSources?(query: string, page?: number): Promise<FederatedSearchPage>;
	/** The full genre/tag facet set across the persisted catalogue (S4), most-common
	 *  first, for the search genre filter. Optional: only the unified Komika API. */
	genreFacets?(): Promise<GenreFacet[]>;
	/**
	 * Browse v2's tag vocabulary, grouped and counted (NSFW-gated). Resolves to `[]`
	 * against a server that predates the query — the caller then falls back to
	 * {@link genreFacets} as an include-only genre list. Optional: only the unified API.
	 */
	tags?(): Promise<TagInfo[]>;
	/**
	 * The sources Browse can filter by ({@link BrowseFilter.source}), biggest first, each
	 * with the number of works the viewer may see from it. `includeNsfw` is the admin
	 * console's escape hatch (honoured for an admin only). Resolves to `[]` against a
	 * server that predates the query. Optional: only the unified API.
	 */
	browseSources?(includeNsfw?: boolean): Promise<BrowseSource[]>;
	/**
	 * Every scanlator an admin tagged as using AI. Public. Resolves to `[]` against a
	 * server that predates the query. Optional: only the unified API.
	 */
	scanlatorTags?(): Promise<ScanlatorTag[]>;
	/**
	 * Every source an admin tagged as using AI: a site that translates its own series
	 * and credits no group. Public. Resolves to `[]` against a server that predates the
	 * query. Optional: only the unified API.
	 */
	sourceAiTags?(): Promise<SourceAiTag[]>;
	/** Admin metadata bars. Resolves to `[]` against a server that predates the query. */
	sourceMetadataBars?(): Promise<SourceMetadataBar[]>;
	/** Admin field provenance for a canonical work or a redirected old work id. */
	workMetadataSources?(workId: Id): Promise<WorkMetadataSource[]>;
	/** False after a metadata-bars query or mutation is rejected as unknown. Call after
	 * {@link sourceMetadataBars} to distinguish an old server from an empty bar list. */
	metadataBarsSupported?(): boolean;
	/**
	 * Whether `search(filter:)` is (still believed to be) understood by the server.
	 * Turns false once the API rejects the argument and it is stripped from the
	 * document, so a caller can say honestly that only the legacy subset of the filter
	 * applied rather than present an unfiltered page under active filter chips.
	 * Optional: absent means "no filter support at all".
	 */
	browseFilterSupported?(): boolean;
	series(id: Id): Promise<Series>;
	chapters(seriesId: Id): Promise<Chapter[]>;
	/** Multi-source aggregated chapters for a canonical work (S2): one entry per
	 *  chapter number across ALL the work's sources, each carrying per-source
	 *  availability so the reader can pick/​fall-back a source. Optional: only the
	 *  unified Komika API implements it. `workId` is the `w_`-prefixed canonical id. */
	aggregatedChapters?(workId: Id): Promise<AggregatedChapter[]>;
	/** `fresh`: skip every HTTP cache (POST, never the cacheable anonymous GET), for a
	 *  chapter whose exact media paths stopped loading. */
	pages(chapterId: Id, opts?: { fresh?: boolean }): Promise<Page[]>;

	// --- library & progress ---
	/** Numeric series ids use the same epoch rules as {@link setProgress}. */
	mark(seriesId: Id, marked: boolean, numericEpoch?: number | null): Promise<Series>;
	/** File a series under an explicit shelf for the viewer, or pass `null` to clear
	 * it (fall back to progress-derived shelving). Adds the series to the library if
	 * absent. Optional: only the unified Komika API implements it. */
	setLibraryStatus?(
		seriesId: Id,
		status: LibraryStatus | null,
		numericEpoch?: number | null,
	): Promise<Series>;
	/** Toggle the viewer's favourite flag on a series (adds it to the library if
	 * absent). Optional: only the unified Komika API implements it. */
	setFavorite?(seriesId: Id, favorite: boolean, numericEpoch?: number | null): Promise<Series>;
	library(): Promise<Series[]>;
	/** Per-series read progress for every in-library series, batched into ONE query
	 * so the Library/Profile screens can shelve the whole library by progress
	 * without a `chapters()` fan-out per series. Optional: only the unified Komika
	 * API implements it; callers fall back to `chapterCount` (unread) when absent. */
	libraryProgress?(): Promise<SeriesProgress[]>;
	/**
	 * `numericEpoch` fences numeric (Suwayomi) chapter ids against an engine change. Leave it
	 * out on a live write: the unified API attaches the cached epoch for a numeric
	 * id. A replayed write passes the epoch it was queued under, or `null` for none, so a
	 * write queued before the change is refused rather than filed on a new chapter.
	 */
	setProgress(
		chapterId: Id,
		lastPageRead: number,
		read: boolean,
		numericEpoch?: number | null,
	): Promise<void>;
	/**
	 * The server's numeric epoch: read before numeric ids and kept for the backend's lifetime,
	 * so writes carry the epoch the reader's chapter ids came from. `null` when the server
	 * predates it. Rejects when the server cannot be reached and nothing is known yet.
	 * Optional: only the unified Komika API implements it.
	 */
	numericEpoch?(): Promise<number | null>;
	/**
	 * Mark ONE chapter read or unread, exactly — the series list's toggle. Unlike
	 * {@link setProgress} (which never un-reads and cascades every earlier chapter to read
	 * on a finish), this sets just this chapter, and unread clears every copy of it.
	 * Resolves `false` when the API predates it, and the caller refuses (there is no safe
	 * fallback: `setProgress` cascades).
	 * Optional: only the unified Komika API implements it.
	 */
	setChapterRead?(chapterId: Id, read: boolean): Promise<boolean>;
	/**
	 * Revisioned progress save (M9). `workId` is the reader's series id for the work
	 * (`w_` or numeric); `revision` defaults to the base revision this backend knows for
	 * the work (from `viewerProgress`, raised by every `APPLIED` answer). Resolves `null`
	 * when the server predates it: the save then went through {@link setProgress}.
	 * `REFUSED` and `MOVED` also raise {@link onProgressConflict}.
	 */
	saveProgress?(input: SaveProgressInput): Promise<ProgressResult | null>;
	/** Revisioned single-chapter tick/untick; `null` = old server, went through
	 *  {@link setChapterRead} (whose `false` then means "not done"). */
	saveChapterRead?(input: SaveChapterReadInput): Promise<ProgressResult | null>;
	/** The viewer's progress row for a work; `undefined` = the server predates it. */
	viewerProgress?(workId: Id): Promise<ViewerProgress | null | undefined>;
	/** The base revision this backend knows for a work, if any (no request). */
	knownRevision?(workId: Id): number | undefined;
	/** The work a chapter copy belongs to now; `undefined` = the server predates it,
	 *  `null` = unknown chapter. */
	chapterWork?(chapterId: Id): Promise<ChapterWork | null | undefined>;
	/** Record one view (a chapter open) for a series — the popularity signal behind
	 * Trending and the series-page view counts. No auth: anonymous reads count too.
	 * Best-effort; callers ignore failures. Optional: only the unified Komika API
	 * implements it (other backends no-op). */
	recordView?(seriesId: Id): Promise<void>;

	// --- viewer preferences ---
	/** Set the signed-in user's NSFW visibility preference (CATALOGUE.md §2), returning
	 * the new value. Optional: only the unified Komika API implements it. */
	setShowNsfw?(value: boolean): Promise<boolean>;
	/** The signed-in user's own content block rules. Optional: only the unified API. */
	myBlockRules?(): Promise<BlockRule[]>;
	/** Replace the signed-in user's content block rules; returns the stored list.
	 * Optional: only the unified API. */
	setMyBlockRules?(rules: BlockRule[]): Promise<BlockRule[]>;

	// --- profile ---
	/** Update the viewer's editable profile (display name + bio); a blank field
	 * clears it. Returns the refreshed session user. Optional: only the unified
	 * Komika API implements it. */
	updateProfile?(input: UpdateProfileInput): Promise<Session['user']>;
	/** Upload a new avatar (any JPEG/PNG/WebP). The server squares, downscales, and
	 * re-encodes it as budgeted lossless WebP on the VM data volume; returns the new
	 * avatar URL. Optional: only the unified Komika API implements it. */
	uploadAvatar?(file: Blob): Promise<string>;
	/** The signed-in user's recent activity feed (newest first; empty when signed
	 * out). Optional: only the unified Komika API implements it. */
	myActivity?(limit?: number): Promise<Activity[]>;
	/** The signed-in user's OWN comments, newest first (empty page when signed out) —
	 * the profile's "My comments" list. Optional: only the unified Komika API
	 * implements it. */
	myComments?(page?: number): Promise<Paginated<MyComment>>;

	// --- canonical catalogue (MangaDex mirror) ---
	/** Recently-updated mirrored MangaDex works + their latest stored chapter, newest
	 * first, NSFW-filtered by the viewer's preference (CATALOGUE.md §6). A data feed;
	 * open one via {@link canonicalSeries} using its `workId`. `includeNsfw` is the
	 * same admin-console-only override as on {@link search}. Optional: only the
	 * unified Komika API implements it. */
	canonicalUpdates?(page?: number, includeNsfw?: boolean): Promise<CanonicalUpdate[]>;
	/** The reader's merged Updates feed, paginated server-side: the union of
	 * {@link updates} and {@link canonicalUpdates}, keyed by canonical work, newest real
	 * upstream release first. `type` filters by format over the WHOLE feed, so `total`
	 * and `hasNextPage` describe the filtered set. NSFW-gated by the viewer's own
	 * preference. Optional: only the unified Komika API implements it, and the reader
	 * falls back to merging the two feeds when it is absent (see `getUpdates`). */
	updatesFeed?(page?: number, type?: ComicType): Promise<Paginated<UpdateFeedRow>>;
	/** A MangaDex-mirrored canonical `work` as a {@link Series}, for reader browse/read
	 * (CATALOGUE.md §6). `workId` is the `w_`-prefixed canonical id. NSFW-gated by the
	 * viewer's preference. Optional: only the unified Komika API implements it. */
	canonicalSeries?(workId: Id): Promise<Series>;
	/** Chapters of a canonical work from the stored mirror, deduped to one row per
	 * number (English preferred) and ordered ascending. Each `Chapter.id` is the
	 * MangaDex chapter uuid to pass to {@link canonicalPages}. Optional. */
	canonicalChapters?(workId: Id): Promise<Chapter[]>;
	/** Ordered page images for a mirrored MangaDex chapter via MangaDex@Home;
	 * `chapterId` is the MangaDex chapter uuid. URLs are resolved through the Worker
	 * proxy by the ImageProvider (never hotlinked). Optional. */
	canonicalPages?(chapterId: Id): Promise<Page[]>;
	/** Source mappings + extension coordinates for one canonical work, so a native
	 * client (embedded Suwayomi) can fetch it directly. `workId` is the `w_`-prefixed
	 * canonical id. Optional: only the unified Komika API implements it. */
	workSources?(workId: Id): Promise<WorkSource[]>;
	/** Where a Suwayomi chapter (a numeric chapter id) lives at its source: the source's
	 * own keys, so a native client can fetch its pages with its own engine instead of
	 * through {@link pages}, and its page on the source's website, for "Read on source".
	 * `null` = unknown chapter; `undefined` = the server predates the query. Optional:
	 * the unified Komika API and the direct Suwayomi backend implement it. */
	chapterOrigin?(chapterId: Id): Promise<ChapterOrigin | null | undefined>;
	/** Where exact media objects can be loaded from (M5 typed media routes); the same for
	 * every viewer. `undefined` = the server predates the query. Optional: only the unified
	 * Komika API implements it. */
	mediaRoutes?(): Promise<MediaRoute[] | undefined>;
	/** {@link workSources} for many works at once, grouped by `workId` — for warming
	 * a native client's source routing over a list. Optional: only the unified
	 * Komika API implements it. */
	workSourcesBatch?(workIds: Id[]): Promise<WorkSourceGroup[]>;
	/** P2.1: the whole series/reader page in ONE round trip — replaces the
	 * {@link canonicalSeries} + {@link canonicalChapters} + {@link workSources}
	 * fan-out and the per-translator {@link chapters} N+1. Resolves to `null` for
	 * an unknown / NSFW-gated work (the page 404s), and to `undefined` when the
	 * SERVER predates the resolver — the caller must then fall back to the old
	 * fan-out (deploy-order safety, same rationale as OPTIONAL_SERIES_FIELDS).
	 * Optional: only the unified Komika API implements it. */
	seriesPage?(workId: Id, withAggregated?: boolean): Promise<SeriesPageBundle | null | undefined>;

	// --- social ---
	reviews(seriesId: Id, page?: number): Promise<Paginated<Review>>;
	/** The signed-in viewer's own review for a series, regardless of pagination
	 * (null if none / signed out). Optional: only the unified Komika API implements it. */
	myReview?(seriesId: Id): Promise<Review | null>;
	postReview(input: PostReviewInput): Promise<Review>;
	/** Withdraw the viewer's own rating; resolves to false when there was nothing to
	 *  remove. `score` is 1..10 server-side, so a rating is withdrawn by deleting the
	 *  review row — NOT by posting a 0. Optional: only the unified Komika API implements
	 *  it (Suwayomi/mock have no social layer to delete from). */
	deleteReview?(seriesId: Id): Promise<boolean>;
	/** Comments on a chapter thread or a series-level discussion (polymorphic target).
	 *  Returns a page of root comments plus all their descendants (flat); the client
	 *  assembles the reply tree via `parentId`. `total`/`hasNextPage` count roots. */
	comments(targetType: CommentTargetType, targetId: Id, page?: number): Promise<Paginated<Comment>>;
	postComment(input: PostCommentInput): Promise<Comment>;
	/** Upload one image to attach to a comment. The server downscales + re-encodes it
	 *  as budgeted WebP and stages it (unlinked); pass the returned `mediaId` to
	 *  {@link postComment} to attach it. Optional: only the unified Komika API implements it. */
	uploadCommentMedia?(file: Blob): Promise<CommentMediaUpload>;
	/** Like (1), dislike (-1), or clear (0) the viewer's vote on a comment; returns the
	 *  fresh tallies. Optional: only the unified Komika API implements it. */
	voteComment?(commentId: Id, value: number): Promise<CommentVote>;

	// --- notifications (inbound bell feed) ---
	/** The viewer's notifications, newest-first. Optional: only the unified Komika API
	 *  implements it (others have no social backend). */
	notifications?(page?: number): Promise<Notification[]>;
	/** Count of the viewer's UNREAD notifications (drives the bell badge). Optional. */
	unreadNotificationCount?(): Promise<number>;
	/** Mark notifications read: pass ids for a subset, or omit to mark ALL read. Returns
	 *  how many rows changed. Optional. */
	markNotificationsRead?(ids?: Id[]): Promise<number>;

	// --- admin "manga DB" (requires an admin session) ---
	/** Upsert per-series admin overrides (scan cadence, pause, status). Optional:
	 * only the unified Komika API implements it. */
	updateSeriesAdmin?(input: SeriesAdminInput): Promise<Series>;
	/** Aggregate scan-scheduler health (admin console). Optional: only the
	 * unified Komika API implements it. */
	scanStatus?(): Promise<ScanStatus>;
	/** Force an immediate re-scan of one series, bypassing adaptive gating.
	 * Optional: only the unified Komika API implements it. */
	triggerScan?(seriesId: Id): Promise<Series>;

	// --- admin series-detail editor (requires an admin session) ---
	/** Edit a canonical work's user-facing metadata (title/description/type/nsfw/
	 * tags) as an override layer; the source stays immutable. Each field is
	 * three-valued: OMIT the key => leave unchanged; `null` => clear the override;
	 * a value => set it. `tags` is a whole-list replace. Returns the recomputed
	 * series. Optional: only the unified Komika API implements it. */
	updateSeriesMetadata?(input: SeriesMetadataInput): Promise<Series>;
	/** Add an alternative title to a work; if it exactly matches another work, those
	 * works are auto-merged into this one. Returns the recomputed series. Optional. */
	addSeriesAltTitle?(id: Id, title: string): Promise<Series>;
	/** Remove an alternative title from a work. Returns the recomputed series. Optional. */
	removeSeriesAltTitle?(id: Id, title: string): Promise<Series>;
	/** Sort one batch of the duplicate backlog: waiting reviews first, then groups of
	 * works that share a title or alt title (articles ignored). Each pair is merged,
	 * closed as two different works, or left for review. Successive calls resume;
	 * `done` marks the end of a run. Optional. */
	consolidateExactDuplicates?(limit?: number): Promise<DuplicateSweepResult>;
	/** Raw metadata-override state of a series' canonical work (pinned vs derived),
	 * for the series-detail editor. Optional. */
	seriesAdminMeta?(seriesId: Id): Promise<SeriesAdminMeta>;
	/** A work's aggregated chapters WITH override state (hidden/renamed), unfiltered
	 * — the editor needs to see and un-hide soft-hidden chapters. Optional. */
	workChaptersAdmin?(workId: Id): Promise<AdminChapter[]>;
	/** Force an immediate re-scan of every installed Suwayomi source of a work;
	 * returns how many sources were scanned. Optional. */
	rescanWork?(workId: Id): Promise<number>;
	/** Soft-hide (reversible) or rename one chapter of a work by aggregate number;
	 * non-destructive. Optional. */
	setChapterOverride?(input: ChapterOverrideInput): Promise<boolean>;
	/** The work's own chapter list (entries + linked copies) for the admin editor.
	 * `undefined` = the server predates chapter lists (use {@link workChaptersAdmin}). */
	workChapterList?(workId: Id): Promise<WorkChapterList | undefined>;
	/** Edit one chapter-list entry; returns the entry as stored. */
	setWorkChapter?(entryId: Id, edit: WorkChapterEdit): Promise<WorkChapterEntry>;
	/** Admin: link a copy to another entry of its work, or (null) to a new entry of its
	 *  own. Read state is kept by the server. */
	linkChapterCopy?(chapterId: Id, entryId: Id | null): Promise<boolean>;
	/** Admin: merge entry `dropId` into `keepId`. */
	mergeWorkChapters?(keepId: Id, dropId: Id): Promise<boolean>;

	// --- admin moderation (requires an admin session) ---
	/** Suspend (`banned: true`) or restore a user account. Optional: only the
	 * unified Komika API implements it. */
	banUser?(userId: Id, banned: boolean): Promise<AdminUser>;
	/** Delete a chapter comment. Returns false if it was already gone. Optional:
	 * only the unified Komika API implements it. */
	deleteComment?(commentId: Id): Promise<boolean>;

	// --- admin user management (requires an admin session) ---
	/** Paginated list of user accounts. Optional: only the unified Komika API
	 * implements it. */
	users?(page?: number): Promise<Paginated<AdminUser>>;
	/** Grant or revoke a user's admin flag. Optional: only the unified Komika API
	 * implements it. */
	setUserAdmin?(userId: Id, isAdmin: boolean): Promise<AdminUser>;
	/** Create an account directly (works with sign-up closed). Optional: only the
	 * unified Komika API implements it. */
	createUser?(input: CreateUserInput): Promise<AdminUser>;
	/** Set a new password for another account; signs it out everywhere and asks
	 * for a change at next sign-in. Optional. */
	resetUserPassword?(userId: Id, password: string): Promise<boolean>;
	/** Open or close public sign-up; returns the new effective value. Refused when
	 * the server pins it by env. Optional. */
	setRegistrationOpen?(open: boolean): Promise<boolean>;
	/** Show or hide MangaDex's community rating for everyone on this server; returns
	 * the new effective value. Refused when the server pins it by env
	 * (`KOMIKA_MANGADEX_RATINGS`). Optional. */
	setShowMangadexRatings?(shown: boolean): Promise<boolean>;
	/** The MangaDex switch and whether the server's env pins it. Optional. */
	mangadexSetting?(): Promise<MangadexSetting>;
	/** Every engine generation the server knows, oldest first (admin). Optional. */
	engineGenerationState?(): Promise<EngineGeneration[]>;
	/** Owner and regional engine health. Empty when the server predates routing. */
	engines?(): Promise<EngineStatus[]>;
	sourceEngineRoute?(sourceId: string): Promise<SourceEngineRoute | null>;
	/** Null removes the route. Refusals surface as errors. */
	setSourceEngine?(
		sourceId: string,
		engineId: string | null,
		strict?: boolean,
	): Promise<SourceEngineRoute | null>;
	/** Give up the quarantined engine data and start a new generation (admin). Returns the
	 * generations after the change. `generationId` is the quarantined generation the admin
	 * was shown: the server refuses if it is no longer current and quarantined. Optional. */
	startNewEngineGeneration?(generationId: Id): Promise<EngineGeneration[]>;
	/** Turn MangaDex on or off; returns the new effective value. Refused when the
	 * server pins it by env (`KOMIKA_MANGADEX`, `CATALOGUE_SYNC=on`). Optional. */
	setMangadexEnabled?(enabled: boolean): Promise<boolean>;
	/** Every per-source scan rate limit in force. Optional. */
	sourceRateLimits?(): Promise<SourceRateLimit[]>;
	/** Add or replace the scan rate limit of one source; applied live. Wins over the
	 * server's `SCAN_SOURCE_RATE_LIMITS` default for that source. Optional. */
	setSourceRateLimit?(sourceId: Id, requests: number, windowSecs: number): Promise<SourceRateLimit>;
	/** Remove an admin's scan rate limit; the source falls back to the server's default
	 * when it has one. `false` when none was stored. Optional. */
	removeSourceRateLimit?(sourceId: Id): Promise<boolean>;
	/** The admin scanlator list: every credit chapters carry, most chapters first, with
	 * its AI tag. 50 per page. Optional. */
	scanlators?(query?: ScanlatorQuery): Promise<ScanlatorPage>;
	/** Tag a scanlator as using AI, or with `null` remove its tag. `false` when nothing
	 * changed. Optional. */
	setScanlatorAi?(name: string, ai: ScanlatorAi | null): Promise<boolean>;
	/** The admin site list: every source with a series in the catalogue, biggest first,
	 * with its AI tag. Optional. */
	scanlatorSites?(): Promise<ScanlatorSite[]>;
	/** Tag a whole source as using AI, or with `null` remove its tag. `false` when
	 * nothing changed. Optional. */
	setSourceAi?(sourceId: string, ai: ScanlatorAi | null): Promise<boolean>;
	/** Bar or allow one source for a metadata group, rechecking its linked works. */
	setSourceMetadataBar?(
		sourceId: string,
		fieldGroup: MetadataFieldGroup,
		barred: boolean,
	): Promise<SetSourceMetadataBarResult>;
	/** The public scanlator list: every site and every group, each with the rating the
	 * readers of this server gave it. 30 per page. Optional: a server that predates it
	 * rejects the field, and the reader then says the server has no scanlator ratings. */
	scanlatorRatings?(query?: ScanlatorRatingQuery): Promise<RatedScanlatorPage>;
	/** One scanlator by its id; `null` when the server has no such scanlator. Optional. */
	ratedScanlator?(id: Id): Promise<RatedScanlator | null>;
	/** Set or change the viewer's rating of a scanlator (1 to 10), or with `null` remove
	 * it. Resolves to the scanlator with its new numbers. Needs sign-in. Optional. */
	rateScanlator?(id: Id, score: number | null): Promise<RatedScanlator>;
	/** The site-wide content block rules. Optional. */
	siteBlockRules?(): Promise<BlockRule[]>;
	/** Replace the site-wide content block rules; returns the stored list. Optional. */
	setSiteBlockRules?(rules: BlockRule[]): Promise<BlockRule[]>;

	// --- admin access + Cloudflare setup (requires an admin session) ---
	/** Settings › Access: access choice, public address, Cloudflare setup and the
	 * current setup/remove run. Optional. */
	accessStatus?(): Promise<AccessStatus>;
	/** `http://<address>:<port>` for each way a device on the home network reaches the
	 * server, best first. Empty when it only answers on its own computer, or cannot
	 * know (a container). Optional. */
	lanAddresses?(): Promise<string[]>;
	/** Record the setup wizard's access choice. `domain` is a domain set up by hand for
	 * `internet`, and the server's public address for `direct` (required). With
	 * `encrypt` (direct only) the server also gets a Let's Encrypt certificate for that
	 * address and serves HTTPS; the call answers once it is in place, which can take a
	 * minute. Optional. */
	setAccessMode?(
		mode: AccessMode,
		domain?: string | null,
		encrypt?: boolean,
	): Promise<AccessStatus>;
	/** Whether this connection is plain HTTP from the internet, where the server
	 * refuses `createOwner`: setup must run in a terminal on the server. Optional. */
	setupNeedsTerminal?(): Promise<boolean>;
	/** Direct mode with HTTPS: the certificate's end date and the last renewal error.
	 * Optional. */
	certificateStatus?(): Promise<CertificateStatus>;
	/** What the access screens can say before setup runs: whether the tunnel connector
	 * is on the machine, what the HTTPS ports are doing, the public address. Rejects on
	 * a server that predates it; the screens then show no such lines. Optional. */
	accessPreflight?(): Promise<AccessPreflight>;
	/** The last time a device on the home network reached this server since it started
	 * (RFC 3339), or null when none has. Rejects with `Unknown field` on a server that
	 * predates it. Optional. */
	lanClientSeenAt?(): Promise<string | null>;
	/** Verify a Cloudflare API token and find the zone for `domain`. Nothing is
	 * changed and the token is not kept. With `connection`, the answer also lists the
	 * permission rows the token seems to lack for that connection (`missing`); a server
	 * that predates the argument answers with none. Optional. */
	checkCloudflareToken?(
		token: string,
		domain: string,
		connection?: CloudflareConnection,
	): Promise<CloudflareTokenCheck>;
	/** Start the Cloudflare checklist in the background; poll {@link accessStatus}.
	 * The token is used for this run only. Optional. */
	startCloudflareSetup?(input: CloudflareSetupInput): Promise<AccessStatus>;
	/** Run the stored setup again from where it failed. Optional. */
	retryCloudflareSetup?(token: string): Promise<AccessStatus>;
	/** Delete exactly what Cloudflare setup created. Optional. */
	removeCloudflareSetup?(token: string): Promise<AccessStatus>;

	// --- admin dedup review (requires an admin session) ---
	/** Pending mid-confidence dedup matches awaiting manual review
	 * (CATALOGUE.md §4), highest score first, PAGINATED — the backlog runs to ~10k
	 * rows now that refused auto-consolidation pairs land here, so there is no
	 * unbounded form. `limit` is clamped server-side to 1..=200 (default 50).
	 * Optional: only the unified Komika API implements it. */
	mergeQueue?(page?: number, limit?: number): Promise<Paginated<MergeCandidate>>;
	/** Resolve a pending match: `accept` merges the source series into the
	 * candidate work; rejecting keeps it as a distinct first-class work. Returns
	 * true when the row was closed. Optional: only the unified Komika API
	 * implements it. */
	resolveMergeCandidate?(id: Id, accept: boolean): Promise<boolean>;
	/** Expand one side of a dedup candidate: metadata, cover, alt titles and source
	 * mappings for a single work. Reads the canonical work directly, so it also
	 * resolves the Suwayomi-only / NSFW works the public series path refuses.
	 * Optional: only the unified Komika API implements it. */
	workReviewDetail?(workId: Id): Promise<WorkReviewDetail>;
	/** Run the Tier-2 dedup add flow for a Suwayomi manga: link it to a canonical
	 * work (auto-merge / queue for review / create new), idempotently. Optional:
	 * only the unified Komika API implements it. */
	addSourceSeries?(suwayomiMangaId: Id): Promise<MatchResult>;
	/** Fold one canonical work into another: re-point the source work's
	 * source_series mappings + user data (library/progress/reviews/comments) +
	 * aliases/external-ids to the target, then DELETE the source work. The target
	 * survives as canonical. Irreversible. Optional: only the unified Komika API
	 * implements it. */
	mergeWorks?(sourceWorkId: Id, targetWorkId: Id): Promise<MergeWorksResult>;
	/**
	 * Catalogue search for the admin merge picker: the same FTS ranking `search` uses,
	 * projected to the fields needed to spot a duplicate, plus each row's canonical
	 * `workId` (which `search` deliberately omits — it is a per-row resolver field).
	 *
	 * Pages at the same BROWSE_PAGE_SIZE (40) as `search`, and forces `includeNsfw` —
	 * honoured by the server for admins only, so a non-admin caller gains nothing.
	 * Optional: only the unified Komika API implements it.
	 */
	searchForMerge?(query: string, page?: number): Promise<Paginated<MergeCandidateRow>>;
	/**
	 * The work's own `source_series` mappings, in admin shape — what the UNMERGE
	 * picker lists. Each row carries its `source_series.id`, the title THAT source
	 * gives the series, and how many chapters we hold from it.
	 *
	 * A separate call from `workSources`, which answers a different question (how a
	 * native client should FETCH a source) and has no field for the row's own id.
	 * Optional: only the unified Komika API implements it.
	 */
	workSourceRows?(workId: Id): Promise<WorkSourceRow[]>;
	/**
	 * Detach source mappings off a work into a NEW canonical work, titled from the
	 * detached source's own title.
	 *
	 * NOT the inverse of {@link mergeWorks} — see {@link SplitSourcesResult}. It is a
	 * forward-looking split: the moved rows keep every chapter they own (`chapter` is
	 * keyed by `source_series_id`), and nothing is deleted.
	 *
	 * The server refuses to move EVERY source off a work: a work with no
	 * `source_series` row is excluded from `browse_catalogue` and `work_fts`, i.e. it
	 * would vanish from the product while still existing in the database.
	 * Optional: only the unified Komika API implements it.
	 */
	splitSourceSeries?(workId: Id, sourceSeriesIds: Id[]): Promise<SplitSourcesResult>;
	/** Distinct readers whose progress follows these sources in a split (admin only). */
	splitProgressReaders?(workId: Id, sourceSeriesIds: Id[]): Promise<number>;

	// --- reader reports (Support → Report an issue) ---
	/**
	 * File a report. Works SIGNED OUT — most catalogue-accuracy reports come from
	 * readers without accounts — so the server rate-limits it per IP and enforces the
	 * per-kind required fields, rejecting with a reader-facing message.
	 * Optional: only the unified Komika API implements it.
	 */
	submitReport?(input: ReportInput): Promise<Report>;
	/** The report queue, newest first; null filters mean "every value". `openCount` is
	 * the global open backlog, not this page's. Requires an admin session.
	 * Optional: only the unified Komika API implements it. */
	reports?(
		status?: ReportStatus | null,
		kind?: ReportKind | null,
		page?: number,
	): Promise<ReportPage>;
	/** Triage one report (resolve/reject, or reopen), optionally with a note. Requires
	 * an admin session. Optional: only the unified Komika API implements it. */
	resolveReport?(reportId: Id, status: ReportStatus, note?: string | null): Promise<Report>;

	// --- admin cover issues / "Bugs" panel (requires an admin session) ---
	/** Paginated works whose cover the crawl couldn't process, most recent first.
	 * Optional: only the unified Komika API implements it. */
	coverIssues?(page?: number): Promise<Paginated<CoverIssue>>;
	/** Re-attempt cover processing for one work (after fixing an upstream image, or
	 * to re-check now the codecs/size cap widened). Returns true if a cover was
	 * stored. Optional: only the unified Komika API implements it. */
	retryCover?(workId: Id): Promise<boolean>;
	/** Replace one work's cover from an uploaded image (multipart POST /admin/cover).
	 * Returns the new cover URL. Optional: only the unified Komika API implements it. */
	uploadCover?(workId: Id, file: Blob): Promise<string>;
	/** Admin: a work's default cover and title, and the covers and titles that can
	 * replace them. Optional: only the unified Komika API implements it. */
	seriesDefaults?(workId: Id): Promise<SeriesDefaults>;
	/** Admin: make the gallery cover `key` (a {@link CoverChoice} key) the work's default
	 * cover; `null` drops the admin cover (a pick or an upload) and lets the cover ranking
	 * choose again. The title is set with {@link updateSeriesMetadata}. */
	setSeriesDefaultCover?(workId: Id, key: string | null): Promise<SeriesDefaults>;
	/** Admin: the cover ranking, best source first ("mangadex" or a source id). Empty
	 * means the default order: MangaDex first, then the oldest mapping. */
	coverSourceRank?(): Promise<string[]>;
	/** Admin: replace the cover ranking; an empty list restores the default order. */
	setCoverSourceRank?(sourceIds: string[]): Promise<string[]>;
	/** Admin: the unpicked-cover prune setting and what it would remove now. */
	coverPrune?(): Promise<CoverPruneStatus>;
	/** Admin: turn the unpicked-cover prune on or off. */
	setCoverPrune?(enabled: boolean): Promise<CoverPruneStatus>;

	// --- admin sources & extensions (requires an admin session; EXT-1/EXT-2) ---
	/** Every Mihon extension known to the Suwayomi engine, installed or not. No
	 * store is seeded — the list is empty until a repo is added; NSFW extensions
	 * are hidden unless the admin opted in via show_nsfw. `refresh: true` re-fetches the
	 * store indexes first so hasUpdate/versions are fresh. Optional: only the
	 * unified Komika API implements it. */
	extensions?(refresh?: boolean): Promise<ExtensionInfo[]>;
	/** The installed Suwayomi sources — the picker feeding {@link sourceBrowse}.
	 * NSFW sources are hidden unless the admin opted in. Optional. */
	sources?(): Promise<SourceInfo[]>;
	/** Browse/search one source's catalogue for bulk ingest. Every returned manga
	 * carries the Suwayomi id {@link bulkAddSourceSeries} consumes. Optional. */
	sourceBrowse?(
		sourceId: Id,
		type: SourceBrowseType,
		page?: number,
		query?: string,
	): Promise<SourceBrowsePage>;
	/** Register an extension repo (store) by its index URL and refresh the list;
	 * returns how many extensions are now known. Optional. */
	addExtensionRepo?(indexUrl: string): Promise<number>;
	/** The extension repos configured on the engine. Optional. */
	extensionRepos?(): Promise<ExtensionRepo[]>;
	/** Remove an extension repo by the `indexUrl` `extensionRepos` lists. Optional. */
	removeExtensionRepo?(indexUrl: string): Promise<boolean>;
	/** Install a store extension onto the Suwayomi engine (NSFW-gated). Optional. */
	installExtension?(pkgName: string): Promise<ExtensionInfo>;
	/** Uninstall an extension from the Suwayomi engine. Optional. */
	uninstallExtension?(pkgName: string): Promise<ExtensionInfo>;
	/** Update an installed extension to the store's latest version. Optional. */
	updateExtension?(pkgName: string): Promise<ExtensionInfo>;
	/** Bulk Tier-2 catalogue ingest: for each Suwayomi manga id, library-track it
	 * and run the dedup add flow. Per-id failures never abort the batch; at most
	 * 100 ids per call. Optional. */
	bulkAddSourceSeries?(suwayomiMangaIds: Id[]): Promise<BulkAddResult>;
	/** Catalogue provenance for many Suwayomi series at once: which canonical
	 * work each is linked to and every source mapping (with extension
	 * coordinates) on that work. One group per id, in input order; max 200 ids.
	 * Optional. */
	seriesSourcesBatch?(seriesIds: Id[]): Promise<SeriesSourceGroup[]>;
	/** Pause or unpause one series' scanning (targeted paused_override write —
	 * `updateSeriesAdmin` is whole-state). Unpausing triggers an immediate
	 * server-side re-scan; returns the recomputed series. Optional. */
	setSeriesPaused?(seriesId: Id, paused: boolean): Promise<Series>;

	/** Per-source scan health (Phase E4.3): series tracked, how many are failing, and how
	 * many of those failed by being served Suwayomi's CACHE rather than the source — the
	 * failure mode that used to record as a success. Worst source first. Optional. */
	sourceScanHealth?(): Promise<SourceScanHealth[]>;
	/** The extension updater's status and per-extension history. Optional. */
	extensionAutoUpdate?(): Promise<ExtensionAutoUpdate>;
	/** Start an extension update pass now (it runs server-side in the background);
	 * returns the status with `running` set. Optional. */
	runExtensionUpdates?(): Promise<ExtensionAutoUpdate>;

	// --- admin background source-ingest jobs (requires an admin session; S1) ---
	/** The "add all from this source" ingest jobs, newest first. Pass
	 * `active: true` for only currently-running ones — poll it for live progress.
	 * Optional: only the unified Komika API implements it. */
	sourceIngestJobs?(active?: boolean): Promise<SourceIngestJob[]>;
	/** Start a background ingest walking a source's catalogue through the Tier-2
	 * dedup add flow. Refused while one is already running for the source, and for
	 * an NSFW source unless the admin opted in. Optional. */
	startSourceIngest?(sourceId: Id): Promise<SourceIngestJob>;
	/** Request cancellation of a running ingest job; the runner stops between
	 * items, preserving progress. Optional. */
	cancelSourceIngest?(jobId: Id): Promise<SourceIngestJob>;
	/** Start one ingest job per installed source of an extension (F1). NSFW
	 * sources are skipped for an opted-out admin, and a source already running is
	 * returned with its existing job rather than erroring. Errors only when no
	 * source matches the package. Returns every started + already-running job.
	 * Optional: only the unified Komika API implements it. */
	startExtensionIngest?(pkgName: Id): Promise<SourceIngestJob[]>;
	/** Cancel every running ingest job for an extension's sources; returns the
	 * cancelled jobs (empty if none were running). Optional. */
	cancelExtensionIngest?(pkgName: Id): Promise<SourceIngestJob[]>;
	/** Subscribe/unsubscribe an extension for background source-sync (auto-discover
	 * new series + reconcile library membership). Enabling kicks an immediate sync
	 * pass server-side. Returns the new subscribed state. Optional. */
	setExtensionSubscription?(pkgName: Id, subscribed: boolean): Promise<boolean>;

	// --- admin maintenance (requires an admin session) ---
	/** Materialize the whole Suwayomi library into the DB read-cache: series
	 * metadata is written synchronously and the count returned; per-series chapter
	 * lists fill in a server-side background task. A production maintenance /
	 * pre-warm action, gated server-side by `require_admin`. Returns how many
	 * series were persisted. Optional: only the unified Komika API implements it. */
	persistCatalogue?(): Promise<number>;
	/** Materialize every canonical work's cover into the DB (`work_cover_blob`) so
	 * the web reader serves covers from `/covers/{id}.webp` instead of the
	 * Cloudflare image Worker. Kicks off a polite, single-flighted background crawl
	 * (bounded by the MangaDex rate limiter) and returns how many works are still
	 * uncached (queued) at kick-off. Admin-gated. Optional: only the unified Komika
	 * API implements it. */
	materializeCatalogueCovers?(): Promise<number>;
}

/**
 * How Browse (an empty-query {@link Backend.search}) orders the catalogue.
 *
 * TRENDING is the server's default and degrades to newest-release for everything with
 * no recent views, so a cold view table cannot render Browse empty; RATING likewise
 * puts rated works first and falls through to newest for the unrated. NEWEST orders by
 * newest upstream CHAPTER release. LATEST_ADDED orders by when the work entered Komiq's
 * catalogue, with work identity breaking ties.
 */
export type BrowseSort = 'TRENDING' | 'NEWEST' | 'LATEST_ADDED' | 'RATING' | 'CHAPTERS';

/**
 * The content-rating ceiling Browse filters by.
 *
 * The first five are CUMULATIVE — each admits everything the milder ones do
 * (SAFE ⊂ SUGGESTIVE ⊂ EROTICA ⊂ PORNOGRAPHIC), and ALL/PORNOGRAPHIC are equivalent.
 * `NSFW_ONLY` is the sole non-cumulative member: adult works only, which no tier can
 * express (every tier admits `safe`).
 *
 * NONE OF THESE WIDEN THE VIEWER'S NSFW GATE. The server clamps the filter to what the
 * viewer's stored `show_nsfw` already allows, so for an opted-out viewer the spicy
 * tiers collapse to SUGGESTIVE and `NSFW_ONLY` returns an EMPTY page rather than
 * revealing anything. Asking for a spicier tier is not a way to opt in — that lives on
 * the profile screen.
 */
export type ContentRatingFilter =
	'ALL' | 'SAFE' | 'SUGGESTIVE' | 'EROTICA' | 'PORNOGRAPHIC' | 'NSFW_ONLY';

/** Server-side catalogue-search filters (S4). All optional; omit to not filter. */
export interface SearchFilters {
	/** Match any of these genres (case-insensitive). Capped at 12 server-side. */
	genres?: string[];
	/** Inclusive rating bounds on the 0–10 scale. */
	minRating?: number;
	maxRating?: number;
	/** Match any of these formats. BROWSE-only (see {@link Backend.search}). */
	types?: ComicType[];
	/** Publication status. BROWSE-only. */
	status?: SeriesStatus;
	/** Result ordering; the server defaults to TRENDING. BROWSE-only. */
	sort?: BrowseSort;
	/** Content-rating ceiling, clamped to the viewer's NSFW posture. BROWSE-only. */
	contentRating?: ContentRatingFilter;
	/**
	 * Restrict to works we know a chapter for (`true`) or know none for (`false`).
	 * BROWSE-only. OMIT for the whole browsable catalogue, which is the default.
	 *
	 * Browse pages every catalogued work, including the ~67k with no chapter yet:
	 * MangaDex REMOVES chapters when a series is licensed or claimed, so "no chapters"
	 * correlates with popular (Boku no Hero Academia, Nausicaä), and many of those works
	 * already have a non-MangaDex source that will supply them. This filter is for a
	 * reader who only wants what they can read right now; it is not a quality signal.
	 */
	hasChapters?: boolean;
	/**
	 * The Browse v2 filter (`search(filter: BrowseFilter)`). When present the server
	 * applies it TOGETHER with a text query and it WINS over every legacy argument above,
	 * which therefore only matter against a server that predates it: `filter` is an
	 * OPTIONAL argument (see `OPTIONAL_ARGUMENTS`), stripped on such a server, and the
	 * legacy arguments then carry whatever subset of the filter they can express.
	 * {@link Backend.browseFilterSupported} tells the caller which of the two happened.
	 */
	filter?: BrowseFilter;
}

/** How a list of tags combines: every tag (AND) or any of them (OR). */
export type TagMode = 'AND' | 'OR';
export type SortDirection = 'ASC' | 'DESC';

/**
 * Browse v2 ordering. Unlike {@link BrowseSort} these apply to a text query too.
 * RELEVANCE is meaningful only WITH a query (the server falls back to LATEST_UPDATE
 * without one); TITLE defaults to ASC, everything else to DESC.
 */
export type BrowseSortField =
	| 'RELEVANCE'
	| 'LATEST_UPDATE'
	| 'RECENTLY_ADDED'
	| 'MOST_VIEWED'
	| 'MOST_FOLLOWS'
	| 'HIGHEST_RATED'
	| 'READER_RATED'
	| 'MOST_CHAPTERS'
	| 'TITLE'
	| 'YEAR';

export type Demographic = 'SHOUNEN' | 'SHOUJO' | 'SEINEN' | 'JOSEI' | 'NONE';

/**
 * A MangaDex content rating as a CATEGORY (Browse v2), not the cumulative ceiling of
 * {@link ContentRatingFilter}: `[EROTICA]` means erotica only. Like the ceiling it only
 * ever narrows INSIDE the viewer's NSFW gate — it is not a way to opt in.
 */
export type ContentRating = 'SAFE' | 'SUGGESTIVE' | 'EROTICA' | 'PORNOGRAPHIC';

/**
 * The `BrowseFilter` GraphQL input (Browse v2). Every field is optional; an absent
 * field is "no clause". Lists are capped server-side (tags 32, languages 16) and the
 * numbers clamped — the reader bounds them too (see `$lib/data/browse-filters`) so a
 * hand-edited URL can't make a request the server would reject.
 */
export interface BrowseFilter {
	/** Tag ids ({@link TagInfo.id}). */
	includedTags?: string[];
	/** Default AND. */
	includedTagsMode?: TagMode;
	excludedTags?: string[];
	/** Default OR — hide a work carrying ANY excluded tag. */
	excludedTagsMode?: TagMode;
	status?: SeriesStatus[];
	types?: ComicType[];
	demographics?: Demographic[];
	contentRatings?: ContentRating[];
	/** Original-language codes as MangaDex reports them (`ja`, `ko`, `zh`, `zh-hk`). */
	originalLanguages?: string[];
	yearFrom?: number;
	yearTo?: number;
	minChapters?: number;
	/** MangaDex bayesian rating, 0–10. */
	minRating?: number;
	minFollows?: number;
	/** `true` = only works with a readable chapter. Omit for everything. */
	hasChapters?: boolean;
	source?: string;
	sort?: BrowseSortField;
	direction?: SortDirection;
}

export interface Session {
	token: string;
	user: {
		id: Id;
		username: string;
		/** Editable display name; falls back to `username` when null. */
		displayName: string | null;
		/** Editable "about me" text. */
		bio: string | null;
		avatarUrl: string | null;
		isAdmin: boolean;
		/** Whether this user opted into seeing NSFW-flagged works (CATALOGUE.md §2). */
		showNsfw: boolean;
		/** Account creation timestamp (ISO 8601) — the profile "joined" date. */
		joinedAt: string;
		/** Set when an admin created the account or reset its password. Only
		 * selected by the setup/password operations so far, so absent elsewhere. */
		mustChangePassword?: boolean;
	};
}

/** Public facts about a Komika server (see {@link Backend.instance}). */
export interface InstanceInfo {
	/** True while no admin account exists — show the first-run setup screen. */
	setupRequired: boolean;
	name: string;
	version: string;
	registrationOpen: boolean;
	mangadexEnabled: boolean;
	/**
	 * False when the server's admin turned MangaDex ratings off: no response carries one,
	 * so the reader hides its own switch. Absent on a server that predates the field,
	 * which reads as true.
	 */
	showMangadexRatings?: boolean;
	/**
	 * True when the server tells reader apps to fetch chapter pages from the sources
	 * themselves: an app with its own engine then does not offer fetching through the
	 * server. Absent on a server that predates the field, which reads as false.
	 */
	appsFetchThemselves?: boolean;
}

export type EngineVersionCheck = 'MATCH' | 'OWNER_UNKNOWN' | 'NOT_INSTALLED' | 'DIFFERS';

export interface SourceEngineRoute {
	sourceId: string;
	engineId: string;
	strict: boolean;
	active: boolean;
	versionCheck: EngineVersionCheck;
}

export interface RoutedSourceStatus {
	sourceId: string;
	strict: boolean;
	active: boolean;
	versionCheck: EngineVersionCheck;
	ownerVersion: string | null;
	regionalVersion: string | null;
}

export interface EngineStatus {
	id: string;
	reachable: boolean;
	checkedAt: string | null;
	error: string | null;
	/** Decimal strings preserve the full unsigned counter range. */
	counters: {
		lookups: string;
		sourceRequests: string;
		pageFetches: string;
		pageBytes: string;
		coverFetches: string;
		failures: string;
		fallbacks: string;
	};
	routedSources: RoutedSourceStatus[];
}

/**
 * One source-engine generation (`engineGenerationState`). At most one is `active` or
 * `quarantined`; earlier ones are `detached`.
 */
export interface EngineGeneration {
	id: string;
	state: 'active' | 'quarantined' | 'detached' | (string & {});
	/** Why the server quarantined it (or detached it); null while active. */
	reason: string | null;
	/** When the generation entered its current state (server timestamp text). */
	since: string;
	/** The engine has no meta support, so the server runs without the marker check. */
	unfenced: boolean;
}

/** The MangaDex switch as the admin sees it. */
export interface MangadexSetting {
	enabled: boolean;
	/** Fixed by the server's environment; read-only here. */
	pinned: boolean;
	/** The env variable that fixes it (`KOMIKA_MANGADEX` or `CATALOGUE_SYNC`). */
	pinnedBy: string | null;
}

/** One per-source scan rate limit: at most `requests` HTTP requests per `windowSecs`. */
export interface SourceRateLimit {
	sourceId: Id;
	requests: number;
	windowSecs: number;
	/** The limit in force is the server's default (`SCAN_SOURCE_RATE_LIMITS`). */
	fromEnv: boolean;
	/** An admin's limit replaces the server's default; removing it restores the default. */
	overridesEnv: boolean;
}

/** One source Browse can filter by (`browseSources`). */
export interface BrowseSource {
	/** What {@link BrowseFilter.source} takes: `mangadex`, or a source id. */
	id: string;
	/** Display name; the extension package name or the id when the engine did not answer. */
	name: string;
	/** Language code (`en`, `all`), when known. Null for the MangaDex mirror. */
	lang: string | null;
	/** Works from this source the viewer may see in Browse (NSFW-gated). */
	count: number;
}

/** What an admin says about a scanlator: `AI` = machine made, `PARTIAL` = AI is part of
 *  the work (for example machine translation that a person edits). */
export type ScanlatorAi = 'AI' | 'PARTIAL';

/** One tagged scanlator. Match `name` to a chapter copy's `scanlator` trimmed and
 *  case-folded; a joint credit ("A & B") with no tag of its own takes the strongest tag
 *  of its members (`AI` over `PARTIAL`). */
export interface ScanlatorTag {
	name: string;
	ai: ScanlatorAi;
}

/** Which scanlators the admin list shows, by the tag that applies to them. */
export type ScanlatorAiFilter = 'TAGGED' | 'AI' | 'PARTIAL' | 'UNTAGGED';

export interface ScanlatorQuery {
	/** Part of the name, any case. */
	search?: string;
	ai?: ScanlatorAiFilter;
	/** A {@link BrowseSource.id}: only credits with series on that source, counted
	 *  on that source. */
	source?: string;
	page?: number;
}

/** One row of the admin scanlator list. */
export interface Scanlator {
	name: string;
	/** Series credited to the name, joint releases included. */
	seriesCount: number;
	/** The tag stored for exactly this name. */
	ai: ScanlatorAi | null;
	/** The tag that applies: `ai`, else one a joint credit takes from a member. */
	effectiveAi: ScanlatorAi | null;
}

export interface ScanlatorPage {
	items: Scanlator[];
	page: number;
	hasNextPage: boolean;
	total: number;
}

/** A scanlator is a site that translates its own series, or a group that chapters
 *  credit. MangaDex is not a site: its groups are the scanlators. */
export type ScanlatorKind = 'SITE' | 'GROUP';

/** `TOP`: ranked scanlators first, by rank. `VOTES`: most ratings first. `NAME`: A to
 *  Z. The server still accepts a deprecated `CHAPTERS`, as `NAME`. */
export type ScanlatorSort = 'TOP' | 'VOTES' | 'NAME';

export interface ScanlatorRatingQuery {
	/** Part of the name, any case. */
	search?: string;
	kind?: ScanlatorKind;
	/** Only the scanlators the viewer rated. An empty page when signed out. */
	mine?: boolean;
	sort?: ScanlatorSort;
	page?: number;
}

/** One row of the public scanlator list. */
export interface RatedScanlator {
	/** `site:<source id>` or `group:<name key>`. Also the `targetId` of its comment
	 *  thread (`targetType: 'scanlator'`). */
	id: Id;
	kind: ScanlatorKind;
	name: string;
	/** Sites only. */
	lang: string | null;
	/** Sites only: the site's own logo. Absent from a server that predates it. */
	logoUrl?: string | null;
	/** The AI tag that applies; null when it has none. */
	ai: ScanlatorAi | null;
	/** The plain average of the scores, 1 to 10; null when nobody rated it. */
	rating: number | null;
	ratingCount: number;
	/** 1-based place among every ranked scanlator; null below the server's minimum
	 *  number of ratings. */
	rank: number | null;
	/** Ten numbers: how many readers gave 1, 2, … 10. */
	distribution: number[];
	/** The viewer's own score; null when signed out or not rated. */
	myScore: number | null;
	commentCount: number;
}

export interface RatedScanlatorPage {
	items: RatedScanlator[];
	page: number;
	hasNextPage: boolean;
	total: number;
}

/** One tagged source. Match `sourceId` to a chapter copy's `sourceId`. A copy takes the
 *  stronger of this tag and the tag on its `scanlator` credit (`AI` over `PARTIAL`). */
export interface SourceAiTag {
	sourceId: string;
	ai: ScanlatorAi;
}

export type MetadataFieldGroup = 'TITLE' | 'DESCRIPTION' | 'CREDITS' | 'TAGS' | 'COVER' | 'STATUS';
export type MetadataSourceState = 'SOURCE' | 'KEPT_ALL_BARRED' | 'KEPT_NOT_OBSERVED' | 'NO_SOURCE';
export type MetadataAttribution = 'OBSERVED' | 'INFERRED' | 'UNKNOWN';

export interface SourceMetadataBar {
	sourceId: string;
	fieldGroup: MetadataFieldGroup;
	/** RFC 3339 timestamp. */
	createdAt: string;
}

export interface SetSourceMetadataBarResult {
	sourceId: string;
	fieldGroup: MetadataFieldGroup;
	barred: boolean;
	works: number;
}

/** One cover an admin can make a work's default. */
export interface CoverChoice {
	/** `md:<file name>` (a MangaDex cover) or `src:<source_series id>`. */
	key: string;
	/** "mangadex" or the Suwayomi source id. */
	sourceId: string;
	sourceName: string;
	/** Display URL; resolve through the ImageProvider like `coverUrl`. */
	thumbnailUrl: string;
	volume: string | null;
	lang: string | null;
	isPrimary: boolean;
	/** The cover the work shows now, as far as the server can tell. */
	current: boolean;
}

/** One title an admin can make a work's default. */
export interface TitleChoice {
	title: string;
	sourceId: string | null;
	sourceName: string | null;
}

/** A work's default cover and title, and what can replace them (admin only). */
export interface SeriesDefaults {
	workId: Id;
	covers: CoverChoice[];
	/** The picked cover's key; null when the cover is automatic or uploaded. */
	pickedCover: string | null;
	uploadedCover: boolean;
	/** The source the shown cover came from; null for an upload or unknown origin. */
	coverSource: string | null;
	titles: TitleChoice[];
	/** The title the sources give. */
	sourceTitle: string;
	titleOverride: string | null;
	/** What the change just made did not do, for the admin; null otherwise. */
	notice?: string | null;
}

export interface CoverPruneSource {
	sourceId: string;
	sourceName: string;
	covers: number;
	/** Bytes freed: files no kept cover shares. */
	bytes: number;
}

/** The unpicked-cover prune: the setting, and what it would remove now. */
export interface CoverPruneStatus {
	enabled: boolean;
	covers: number;
	bytes: number;
	sources: CoverPruneSource[];
}

export interface WorkMetadataSource {
	fieldGroup: MetadataFieldGroup;
	state: MetadataSourceState;
	sourceId: string | null;
	sourceName: string | null;
	sourceSeriesId: string | null;
	attribution: MetadataAttribution | null;
}

/** One row of the admin site list: a source, as a scanlator of its own. */
export interface ScanlatorSite {
	/** The source id ({@link BrowseSource.id}); never `mangadex`. */
	sourceId: string;
	/** Display name; the extension package name or the id when the engine did not answer. */
	name: string;
	lang: string | null;
	/** Series in the catalogue that have this source. */
	seriesCount: number;
	ai: ScanlatorAi | null;
}

/**
 * One content block rule. It matches a series that carries ALL of `tags` (tag ids, see
 * `TagInfo.id`) and, when `ratings` is not empty, has ONE of them. A rule needs a tag or
 * a rating. A series is blocked when any rule matches it.
 */
export interface BlockRule {
	tags: string[];
	ratings: ContentRating[];
}

/** One extension repo configured on the engine. */
export interface ExtensionRepo {
	/** Canonical index URL: the repo's identity, and each extension's `repo`. */
	indexUrl: string;
	name: string;
	badgeLabel: string;
}

/** `direct`: public at the server's own address, with no domain and no Cloudflare. */
export type AccessMode = 'local' | 'internet' | 'direct';
export type CloudflareConnection = 'TUNNEL' | 'PUBLIC_IP';

/** One line of the Cloudflare checklist. */
export interface CloudflareStep {
	id: string;
	label: string;
	state: 'waiting' | 'running' | 'done' | 'failed';
	message: string | null;
}

/** The current or last Cloudflare setup / remove run. */
export interface CloudflareJob {
	kind: 'setup' | 'remove';
	running: boolean;
	failed: boolean;
	steps: CloudflareStep[];
}

/** What Cloudflare setup created on this server (no secrets). */
export interface CloudflareSetup {
	domain: string;
	connection: CloudflareConnection | null;
	zoneName: string | null;
	complete: boolean;
	/** Anything left for "Remove". */
	removable: boolean;
}

/** Settings › Access (see {@link Backend.accessStatus}). */
export interface AccessStatus {
	/** `local`, `internet`, `direct`, or empty while nobody chose. */
	mode: AccessMode | '';
	domain: string | null;
	/** What readers type: `https://<domain>` (internet) or the public address (direct:
	 * `https://<address>`, or `http://<address>:<port>` without encryption). */
	address: string | null;
	cloudflare: CloudflareSetup | null;
	connector: 'off' | 'starting' | 'running' | 'failed' | 'missing';
	connectorMessage: string | null;
	https: boolean;
	trustedProxies: string[];
	trustedProxiesPinned: boolean;
	job: CloudflareJob | null;
}

/** The certificate the server's own HTTPS listener serves: the Let's Encrypt one of
 * direct mode, or the Origin CA one of Cloudflare's Public IP connection. All null
 * when neither is on. */
export interface CertificateStatus {
	/** When the current certificate stops being valid (RFC 3339). Direct mode: the
	 * server renews it long before. Public IP: nothing renews it; "Run setup again"
	 * gets a new one. */
	certificateExpires: string | null;
	/** Direct mode: why the last renewal failed, until one succeeds. Public IP: why the
	 * certificate needs a new one in its place (it ends soon, or is not for the domain). */
	certificateError: string | null;
	/** Why the HTTPS listener is down while the access mode needs it (its port was busy
	 * when the server started, for one). The server keeps trying. */
	httpsError: string | null;
	/** The plain HTTP port refuses clients on the internet. It starts to after the
	 * first visit from the internet over HTTPS. */
	plainHttpClosed: boolean;
}

export interface CloudflareTokenCheck {
	zoneName: string;
	zoneStatus: string;
	accountName: string;
	/** Permission rows the token seems to lack, e.g. `Zone › DNS`. Advice only: setup
	 * may still work. Empty when nothing is missing, or the server cannot tell. */
	missing: string[];
}

/** What a port on the server's machine is doing: nothing listens there (`free`), komiq
 * itself listens there (`serving`), another program does (`in_use`), the server is not
 * allowed to open it (`denied`), or the check itself failed (`unknown`). */
export type PortState = 'free' | 'serving' | 'in_use' | 'denied' | 'unknown';

/** What the server knows about its machine before access setup runs
 * (see {@link Backend.accessPreflight}). */
export interface AccessPreflight {
	/** The tunnel connector: `found` on the machine, `download` (tunnel setup fetches
	 * the pinned release by itself), or `unsupported` (no release fits this machine). */
	cloudflared: 'found' | 'download' | 'unsupported';
	/** A sentence about the connector, e.g. why it is unsupported. */
	cloudflaredDetail: string | null;
	/** The plain HTTP port as other machines see it: the one a container's host
	 * publishes, else the one the server listens on. */
	httpPort: number;
	/** The port HTTPS is served on (Cloudflare public IP, and direct mode). */
	httpsPort: number;
	httpsPortState: PortState;
	/** The port a Let's Encrypt check arrives on (direct mode with HTTPS). */
	acmePort: number;
	acmePortState: PortState;
	/** The server runs in a container: its ports must be published by the host too. */
	inContainer: boolean;
	/** The server's public address as the internet sees it, when it could be found. */
	publicIp: string | null;
}

export interface CloudflareSetupInput {
	token: string;
	domain: string;
	connection: CloudflareConnection;
	/** Public IP only; empty detects the server's address. */
	publicIp?: string | null;
}

export interface CreateOwnerInput {
	/** The one-time code the server printed to its log at boot. */
	setupCode: string;
	username: string;
	email: string;
	password: string;
}

export interface CreateUserInput {
	username: string;
	email: string;
	password: string;
	/** Default false. */
	isAdmin?: boolean;
	/** Default true: the admin chose the initial password. */
	mustChangePassword?: boolean;
}

/** One entry in a user's activity feed (see {@link Backend.myActivity}). */
export interface Activity {
	id: Id;
	/** "review" | "comment" | "library_add". */
	kind: string;
	/** "series" | "chapter" — the kind of thing acted on, when known. */
	targetType: string | null;
	/** The series/chapter id acted on; the client resolves a display title. */
	targetId: Id | null;
	createdAt: string;
}

/** One of the viewer's own comments (see {@link Backend.myComments}). */
export interface MyComment {
	id: Id;
	targetType: CommentTargetType;
	targetId: Id;
	/** The comment this one replies to; null for a top-level comment. */
	parentId: Id | null;
	body: string;
	hasSpoiler: boolean;
	createdAt: string;
	likes: number;
	dislikes: number;
	/** Direct replies to this comment. */
	replyCount: number;
	/** The series the thread hangs off (a chapter thread's owning series); null when
	 *  the server can't resolve it. */
	seriesId: Id | null;
	/** That series' display title, when known. */
	seriesTitle: string | null;
	/** Chapter number for a chapter thread (e.g. "12", "10.5"), when known. */
	chapterNumber: string | null;
}

export interface RegisterInput {
	username: string;
	email: string;
	password: string;
}

/** Editable profile fields. A blank/undefined field clears that value. */
export interface UpdateProfileInput {
	displayName?: string | null;
	bio?: string | null;
}

export interface PostReviewInput {
	seriesId: Id;
	score: number;
	body: string;
	hasSpoiler: boolean;
}

export interface PostCommentInput {
	targetType: CommentTargetType;
	targetId: Id;
	/** Reply target; must be a comment on the same target. Omit for a top-level comment. */
	parentId?: Id | null;
	body: string;
	hasSpoiler: boolean;
	/** A previously-uploaded comment-media id to attach (owned by the poster, unlinked). */
	mediaId?: Id | null;
}

/** Result of staging a comment image via `POST /comment-media`. Pass `mediaId` to
 *  {@link Backend.postComment} to attach it; `url`/`width`/`height` drive the preview. */
export interface CommentMediaUpload {
	mediaId: Id;
	url: string;
	width: number;
	height: number;
}

/** Admin "manga DB" overrides. Whole-state: null clears an override. */
export interface SeriesAdminInput {
	seriesId: Id;
	overrideIntervalHours?: number | null;
	pollEveryMinutes?: number | null;
	paused?: boolean | null;
	status?: SeriesStatus | null;
}

/**
 * Admin series-detail metadata edits. Each optional field is three-valued: OMIT
 * the key (`undefined`) => leave unchanged; `null` => clear the override (use the
 * derived/source value); a value => set the override. `tags` replaces the whole
 * curated set.
 */
export interface SeriesMetadataInput {
	seriesId: Id;
	title?: string | null;
	description?: string | null;
	type?: ComicType | null;
	isNsfw?: boolean | null;
	tags?: string[] | null;
	/** The fields below are owned by the source. A value PINS the field so later syncs
	 * keep it; `null` removes the pin and restores the source's value. An empty string
	 * pins the field empty. */
	author?: string | null;
	artist?: string | null;
	year?: number | null;
	/** Language code: `ja`, `ko`, `zh`, `zh-hk`, `en`, … */
	originalLanguage?: string | null;
	status?: SeriesStatus | null;
	/** `NONE` pins "no demographic". */
	demographic?: Demographic | null;
	contentRating?: ContentRating | null;
}

/** A source-owned field of {@link SeriesAdminMeta} an admin can pin. */
export type PinnableField =
	'author' | 'artist' | 'year' | 'originalLanguage' | 'status' | 'demographic' | 'contentRating';

/** Raw metadata-override state of a work (pinned vs derived) for the editor. */
export interface SeriesAdminMeta {
	/** null when the series isn't catalogued yet (nothing can be pinned). */
	workId: Id | null;
	titleOverride: string | null;
	descriptionOverride: string | null;
	contentTypeOverride: ComicType | null;
	isNsfwOverride: boolean | null;
	/** Effective genres: the curated set if any, else source-derived. */
	tags: string[];
	hasCuratedTags: boolean;
	/** The work's current source-owned fields, pinned or not. */
	author: string | null;
	artist: string | null;
	year: number | null;
	originalLanguage: string | null;
	status: SeriesStatus | null;
	demographic: Demographic | null;
	contentRating: ContentRating | null;
	/** Which of those fields an admin has pinned. */
	pinnedFields: PinnableField[];
}

/** One chapter of a work in the admin editor, with its override state. */
export interface AdminChapter {
	number: number;
	/** Aggregate bucket key — the override key passed to {@link ChapterOverrideInput}. */
	key: string;
	sourceTitle: string | null;
	titleOverride: string | null;
	effectiveTitle: string | null;
	hidden: boolean;
	sourceCount: number;
}

/** {@link Backend.saveProgress} input. */
export interface SaveProgressInput {
	chapterId: Id;
	workId: Id;
	lastPageRead: number;
	read: boolean;
	numericEpoch?: number | null;
	/** Base revision; omitted = the known one (fetched with `viewerProgress` if none). */
	revision?: number;
	/**
	 * This save's own id (at most 64 characters). Sending the same id again after a lost
	 * answer is answered APPLIED with the current revision when it was the last change.
	 */
	opId?: string;
}

/** {@link Backend.saveChapterRead} input. */
export interface SaveChapterReadInput {
	chapterId: Id;
	workId: Id;
	read: boolean;
	numericEpoch?: number | null;
	revision?: number;
	/** See {@link SaveProgressInput.opId}. */
	opId?: string;
}

/** One copy (a source's chapter) linked to a {@link WorkChapterEntry}. */
export interface WorkChapterCopy {
	chapterId: Id;
	sourceSeriesId: Id;
	sourceType: string;
	sourceId: string | null;
	number: string | null;
	title: string | null;
	lang: string | null;
	scanlator: string | null;
	releasedAt: string | null;
	/** `matcher` or `admin`. */
	linkedBy: string;
}

/** One entry of a work's own chapter list (admin view: hidden entries and entries
 *  without copies included). */
export interface WorkChapterEntry {
	id: Id;
	matchKey: string | null;
	/** List position key; entries sort by it ascending. */
	sortKey: number;
	/** The exact stored `sortKey` (numeric text). */
	sortKeyText: string;
	number: string | null;
	label: string | null;
	special: boolean;
	/** Who set `special` (`matcher`, `admin`), or null. */
	specialBy: string | null;
	hidden: boolean;
	titleOverride: string | null;
	copies: WorkChapterCopy[];
}

/** `workChapterList(workId)`: the work's chapter list for the admin editor. */
export interface WorkChapterList {
	workId: Id;
	/** Every copy of the work is linked; readers see the list only when true. */
	ready: boolean;
	entries: WorkChapterEntry[];
}

/** `setWorkChapter` patch. Omitted fields stay unchanged; `clearTitle` removes the
 *  title override (the source title shows again). */
export interface WorkChapterEdit {
	hidden?: boolean;
	titleOverride?: string;
	clearTitle?: boolean;
	special?: boolean;
	/** Finite, within -1000000..1000000. */
	sortKey?: number;
}

/** Admin edit to one chapter (soft-hide / rename). Three-valued optionals. */
export interface ChapterOverrideInput {
	workId: Id;
	chapterKey: string;
	hidden?: boolean | null;
	title?: string | null;
}
