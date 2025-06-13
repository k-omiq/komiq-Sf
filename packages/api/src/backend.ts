import type {
	AdminUser,
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
	SeriesProgress,
	SeriesSourceGroup,
	SeriesStatus,
	SourceBrowsePage,
	SourceBrowseType,
	SourceIngestJob,
	SourceInfo,
	SourceScanHealth,
	SplitSourcesResult,
	UpdateFeedRow,
	WorkReviewDetail,
	WorkSource,
	WorkSourceGroup,
	WorkSourceRow,
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
	 * BROWSE PAGES AT 30, not the API-wide 20 (server `BROWSE_PAGE_SIZE`); a pager built
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
	 * work with its per-source translator list. User-facing; NSFW-gated by the
	 * viewer's `show_nsfw` posture (same as {@link search}). Optional: only the
	 * unified Komika API implements it.
	 */
	searchAllSources?(query: string, page?: number): Promise<FederatedSearchPage>;
	/** The full genre/tag facet set across the persisted catalogue (S4), most-common
	 *  first, for the search genre filter. Optional: only the unified Komika API. */
	genreFacets?(): Promise<GenreFacet[]>;
	series(id: Id): Promise<Series>;
	chapters(seriesId: Id): Promise<Chapter[]>;
	/** Multi-source aggregated chapters for a canonical work (S2): one entry per
	 *  chapter number across ALL the work's sources, each carrying per-source
	 *  availability so the reader can pick/​fall-back a source. Optional: only the
	 *  unified Komika API implements it. `workId` is the `w_`-prefixed canonical id. */
	aggregatedChapters?(workId: Id): Promise<AggregatedChapter[]>;
	pages(chapterId: Id): Promise<Page[]>;

	// --- library & progress ---
	mark(seriesId: Id, marked: boolean): Promise<Series>;
	/** File a series under an explicit shelf for the viewer, or pass `null` to clear
	 * it (fall back to progress-derived shelving). Adds the series to the library if
	 * absent. Optional: only the unified Komika API implements it. */
	setLibraryStatus?(seriesId: Id, status: LibraryStatus | null): Promise<Series>;
	/** Toggle the viewer's favourite flag on a series (adds it to the library if
	 * absent). Optional: only the unified Komika API implements it. */
	setFavorite?(seriesId: Id, favorite: boolean): Promise<Series>;
	library(): Promise<Series[]>;
	/** Per-series read progress for every in-library series, batched into ONE query
	 * so the Library/Profile screens can shelve the whole library by progress
	 * without a `chapters()` fan-out per series. Optional: only the unified Komika
	 * API implements it; callers fall back to `chapterCount` (unread) when absent. */
	libraryProgress?(): Promise<SeriesProgress[]>;
	setProgress(chapterId: Id, lastPageRead: number, read: boolean): Promise<void>;
	/** Record one view (a chapter open) for a series — the popularity signal behind
	 * Trending and the series-page view counts. No auth: anonymous reads count too.
	 * Best-effort; callers ignore failures. Optional: only the unified Komika API
	 * implements it (other backends no-op). */
	recordView?(seriesId: Id): Promise<void>;

	// --- viewer preferences ---
	/** Set the signed-in user's NSFW visibility preference (CATALOGUE.md §2), returning
	 * the new value. Optional: only the unified Komika API implements it. */
	setShowNsfw?(value: boolean): Promise<boolean>;

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
	/** {@link workSources} for many works at once, grouped by `workId` — for warming
	 * a native client's source routing over a list. Optional: only the unified
	 * Komika API implements it. */
	workSourcesBatch?(workIds: Id[]): Promise<WorkSourceGroup[]>;

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
	/** Consolidate the backlog of duplicate works sharing an exact title, up to `limit`
	 * clusters per call; returns how many works were merged away. Optional. */
	consolidateExactDuplicates?(limit?: number): Promise<number>;
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
	 * Pages at the same BROWSE_PAGE_SIZE (30) as `search`, and forces `includeNsfw` —
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

	// --- admin sources & extensions (requires an admin session; EXT-1/EXT-2) ---
	/** Every Keiyoushi/Mihon extension known to the Suwayomi engine, installed or
	 * not. First use seeds the curated Keiyoushi store; NSFW extensions are hidden
	 * unless the admin opted in via show_nsfw. `refresh: true` re-fetches the
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
 * newest upstream CHAPTER release, not by date-added — label it accordingly in a UI.
 */
export type BrowseSort = 'TRENDING' | 'NEWEST' | 'RATING' | 'CHAPTERS';

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
	};
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
}

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

/** Admin edit to one chapter (soft-hide / rename). Three-valued optionals. */
export interface ChapterOverrideInput {
	workId: Id;
	chapterKey: string;
	hidden?: boolean | null;
	title?: string | null;
}
