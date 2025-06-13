import type {
	AdminUser,
	AggregatedChapter,
	BulkAddResult,
	CanonicalUpdate,
	Chapter,
	ComicType,
	Comment,
	CommentVote,
	Notification,
	CommentTargetType,
	DiscoveryFeed,
	ExtensionInfo,
	FederatedSearchPage,
	GenreFacet,
	Id,
	LibraryStatus,
	MatchResult,
	MergeCandidate,
	MergeWorksResult,
	Page,
	Paginated,
	Report,
	ReportInput,
	Review,
	ScanStatus,
	Series,
	SeriesProgress,
	SeriesSourceGroup,
	SourceBrowsePage,
	SourceBrowseType,
	SourceIngestJob,
	SourceInfo,
	SourceScanHealth,
	SplitSourcesResult,
	UpdateFeedRow,
	WorkSource,
	WorkSourceGroup,
	WorkSourceRow,
} from '@komika/types';
import type {
	Activity,
	AdminChapter,
	Backend,
	ChapterOverrideInput,
	CommentMediaUpload,
	PostCommentInput,
	PostReviewInput,
	RegisterInput,
	SearchFilters,
	SeriesAdminInput,
	SeriesAdminMeta,
	SeriesMetadataInput,
	Session,
	UpdateProfileInput,
} from './backend.js';
import type { ContentBackend, SourceRef } from './content-backend.js';
import type { WorkSourceLike } from './local-suwayomi-backend.js';
import type { OfflineWriteQueue, WriteOp } from './offline-queue.js';

/**
 * The optional source-ref mapping capability layered on the local content backend by
 * {@link LocalSuwayomiBackend}. `ContentBackend` intentionally stays mapping-agnostic
 * (it takes a resolved `SourceRef`), so the composite feature-detects `refFor` on the
 * concrete local backend to translate a hosted `WorkSource` into an engine `SourceRef`.
 */
interface RefMapper {
	refFor(ws: WorkSourceLike, title?: string): Promise<SourceRef | null>;
}

/**
 * A {@link Backend} that splits work between a hosted server and an optional
 * on-device content engine. Auth, library, reading-progress, the social layer,
 * admin, canonical mirror, and source routing ALWAYS go to the hosted backend;
 * only live series/chapters/pages content is eligible to be served locally (see
 * the embedded-Suwayomi routing and licensing requirements).
 *
 * In Wave B there is no local engine (its transport lands in Wave C), so
 * `local` is null and every call — including the three content methods —
 * delegates to `hosted`, making this wrapper behaviorally identical to the plain
 * hosted backend. When Wave C wires a real {@link ContentBackend}, the three
 * content branches resolve a source mapping and fetch on-device instead.
 *
 * When an optional {@link OfflineWriteQueue} is supplied (native only; plan §9),
 * the two on-device writes — `mark` and `setProgress` — are captured into that
 * durable queue if the hosted write throws, and replayed when connectivity
 * returns (reconciled by the canonical id the write carries). Without a queue the
 * class behaves exactly as before: writes delegate straight to `hosted` and a
 * failure propagates unchanged, so web / flag-off / existing callers are
 * unaffected.
 */
export class CompositeBackend implements Backend {
	constructor(
		private opts: { hosted: Backend; local?: ContentBackend | null; queue?: OfflineWriteQueue },
	) {
		// Best-effort: replay any backlog persisted from a previous session, and
		// re-drain whenever the browser reports connectivity is back. Both are guarded
		// so a reject can never escape construction or the event handler.
		if (this.opts.queue) {
			void this.flushQueue();
			const g = globalThis as unknown as {
				addEventListener?: (type: string, cb: () => void) => void;
			};
			if (typeof g.addEventListener === 'function') {
				g.addEventListener('online', () => void this.flushQueue());
			}
		}
	}

	/**
	 * Drain the offline write-queue against the hosted backend, oldest-first. A
	 * no-op when no queue is configured. Never throws into a caller: {@link
	 * OfflineWriteQueue.drain} already swallows a rejecting flush (incrementing
	 * `tries` and stopping), and the whole call is additionally guarded.
	 */
	private async flushQueue(): Promise<void> {
		const queue = this.opts.queue;
		if (!queue) return;
		try {
			await queue.drain(
				(op: WriteOp) =>
					op.kind === 'progress'
						? this.opts.hosted.setProgress(op.chapterId, op.lastPageRead, op.read)
						: this.opts.hosted.mark(op.seriesId, op.marked).then(() => {}),
				// Scope the replay to the signed-in user so A's offline writes can never
				// be applied under B's account (C1). Null (identity not yet known) defers.
				this.currentUserId,
			);
		} catch {
			/* defensive: drain is self-contained, but never let a drain reject a caller */
		}
	}

	/**
	 * The currently-authenticated user id, or `null` when signed out / not yet
	 * known. Every queued offline write is stamped with this at enqueue and the
	 * drain is scoped to it, so a write enqueued by one account is never replayed
	 * under another (C1). Set via {@link setCurrentUser} by the auth layer.
	 */
	private currentUserId: Id | null = null;

	/**
	 * Reconciliation map from a hosted canonical chapter id (a globally-unique uuid) to
	 * the embedded engine's integer chapter id (as a string), populated during
	 * {@link canonicalChapters} and consulted by {@link canonicalPages} to serve page
	 * bytes on-device. Keyed flat by canonical chapter id — safe because canonical
	 * chapter ids are globally unique, and it keeps `canonicalPages` (which has no
	 * workId) a single lookup. The engine id NEVER reaches `setProgress`: only page
	 * image bytes are served locally; chapter identity/list/progress stay canonical.
	 */
	private readonly localChapterMap = new Map<string, string>();

	/**
	 * Best-effort, session-lifetime memo of canonical work ids (as strings) whose local
	 * resolution GENUINELY failed — no engine-resolvable source, {@link RefMapper.refFor}
	 * returned null / threw (extension can't install, repo down, incompatible), or the
	 * engine's `chapters` fetch threw. Once memoed, {@link canonicalChapters} skips the
	 * expensive provision+resolve+reconcile for that work and serves hosted-only, so a
	 * doomed local path isn't re-hammered on every open (plan §4: "mark that source_series
	 * unusable on-device and fall back to server fetch ... Never hard-fail the read").
	 *
	 * This is deliberately a plain in-memory `Set`: it is cleared only by constructing a
	 * new backend instance (app restart), which is acceptable per §4 — a merely transient
	 * failure just costs hosted-fetch for that work until the next launch. It is NOT
	 * populated for the not-ready engine state (that's a transient rung handled by
	 * {@link localReady}) nor for a zero-match reconciliation (a data mismatch that a later
	 * catalogue update may resolve), only for genuine per-source provisioning/resolution
	 * failures.
	 */
	private readonly localUnusable = new Set<string>();

	/** Whether the local content engine exists and reports itself ready right now. */
	private async localReady(): Promise<boolean> {
		return !!this.opts.local && (await this.opts.local.isReady());
	}

	// --- auth ---
	session(): Promise<Session | null> {
		return this.opts.hosted.session();
	}
	login(username: string, password: string): Promise<Session> {
		return this.opts.hosted.login(username, password);
	}
	register(input: RegisterInput): Promise<Session> {
		return this.opts.hosted.register(input);
	}
	logout(): Promise<void> {
		return this.opts.hosted.logout();
	}
	/** Forward the bearer token to the hosted backend only — the local engine is
	 * content-only and never receives a token. */
	setToken(token: string | null): void {
		this.opts.hosted.setToken?.(token);
	}
	/**
	 * Bind the composite to the signed-in user (C1). On any identity change —
	 * including logout (`null`) — the offline queue drops every op that doesn't
	 * belong to the new identity BEFORE it could be replayed, and subsequent
	 * enqueues/drains are scoped to this id. This, together with the per-op userId
	 * guard in {@link flushQueue}, makes cross-user replay impossible.
	 */
	setCurrentUser(userId: Id | null): void {
		if (userId === this.currentUserId) return;
		this.currentUserId = userId;
		// Clear foreign (and, on logout, all) queued ops before the token is dropped.
		this.opts.queue?.dropForeign(userId);
		// Now that identity is known, opportunistically replay this user's own backlog
		// (e.g. writes enqueued before session restore validated the token). Guarded and
		// scoped to `userId`, so it can only ever flush this account's ops.
		if (userId != null) void this.flushQueue();
	}

	// --- discovery / catalog ---
	discovery(): Promise<DiscoveryFeed[]> {
		return this.opts.hosted.discovery();
	}
	updates(page?: number): Promise<Paginated<Series>> {
		return this.opts.hosted.updates(page);
	}
	search(
		query: string,
		page?: number,
		filters?: SearchFilters,
		includeNsfw?: boolean,
	): Promise<Paginated<Series>> {
		return this.opts.hosted.search(query, page, filters, includeNsfw);
	}
	searchAllSources(query: string, page?: number): Promise<FederatedSearchPage> {
		return this.opts.hosted.searchAllSources!(query, page);
	}
	genreFacets(): Promise<GenreFacet[]> {
		return this.opts.hosted.genreFacets!();
	}
	series(id: Id): Promise<Series> {
		// Always hosted. Local serving for canonical works happens in the `canonical*`
		// overrides — the reader routes `w_` canonical ids there. These plain methods
		// are only hit for non-canonical numeric/source ids, which the native+canonical
		// path never produces, so there is no on-device branch to take here.
		return this.opts.hosted.series(id);
	}
	chapters(seriesId: Id): Promise<Chapter[]> {
		// Always hosted — see `series` above. Canonical chapter reconciliation (and any
		// on-device serving) lives in `canonicalChapters` / `canonicalPages`.
		return this.opts.hosted.chapters(seriesId);
	}
	aggregatedChapters(workId: Id): Promise<AggregatedChapter[]> {
		return this.opts.hosted.aggregatedChapters!(workId);
	}
	pages(chapterId: Id): Promise<Page[]> {
		// Always hosted — see `series` above. On-device page bytes for canonical works
		// are served by `canonicalPages`, not here.
		return this.opts.hosted.pages(chapterId);
	}

	// --- library & progress ---
	/**
	 * Library membership is a hosted write (D2). Without a queue this delegates
	 * straight through (unchanged). With a queue: on a hosted throw (offline) the
	 * membership is captured for later replay and the error is RETHROWN — the
	 * reader's `setLibraryMark` catch then keeps its optimistic UI, and we avoid
	 * fabricating a `Series` we don't have. A success opportunistically drains any
	 * backlog (connectivity is clearly back).
	 */
	async mark(seriesId: Id, marked: boolean): Promise<Series> {
		if (!this.opts.queue) return this.opts.hosted.mark(seriesId, marked);
		try {
			const s = await this.opts.hosted.mark(seriesId, marked);
			void this.flushQueue();
			return s;
		} catch (err) {
			this.opts.queue.enqueue({ kind: 'mark', seriesId, marked, userId: this.currentUserId });
			throw err;
		}
	}
	setLibraryStatus(seriesId: Id, status: LibraryStatus | null): Promise<Series> {
		return this.opts.hosted.setLibraryStatus!(seriesId, status);
	}
	setFavorite(seriesId: Id, favorite: boolean): Promise<Series> {
		return this.opts.hosted.setFavorite!(seriesId, favorite);
	}
	library(): Promise<Series[]> {
		return this.opts.hosted.library();
	}
	libraryProgress(): Promise<SeriesProgress[]> {
		return this.opts.hosted.libraryProgress!();
	}
	/**
	 * Reading progress is a hosted write (D2). Without a queue this delegates
	 * straight through (unchanged). With a queue: on a hosted throw (offline) the
	 * progress is captured for later replay and the write RESOLVES — progress is
	 * fire-and-forget and the reader already swallows it, so surfacing the error
	 * would change nothing but risk an unhandled rejection. A success
	 * opportunistically drains any backlog.
	 */
	async setProgress(chapterId: Id, lastPageRead: number, read: boolean): Promise<void> {
		if (!this.opts.queue) return this.opts.hosted.setProgress(chapterId, lastPageRead, read);
		try {
			await this.opts.hosted.setProgress(chapterId, lastPageRead, read);
			void this.flushQueue();
		} catch {
			this.opts.queue.enqueue({
				kind: 'progress',
				chapterId,
				lastPageRead,
				read,
				userId: this.currentUserId,
			});
		}
	}

	// --- viewer preferences ---
	setShowNsfw(value: boolean): Promise<boolean> {
		return this.opts.hosted.setShowNsfw!(value);
	}

	// --- profile ---
	updateProfile(input: UpdateProfileInput): Promise<Session['user']> {
		return this.opts.hosted.updateProfile!(input);
	}
	uploadAvatar(file: Blob): Promise<string> {
		return this.opts.hosted.uploadAvatar!(file);
	}
	myActivity(limit?: number): Promise<Activity[]> {
		return this.opts.hosted.myActivity!(limit);
	}

	// --- canonical catalogue (MangaDex mirror) ---
	canonicalUpdates(page?: number, includeNsfw?: boolean): Promise<CanonicalUpdate[]> {
		return this.opts.hosted.canonicalUpdates!(page, includeNsfw);
	}
	updatesFeed(page?: number, type?: ComicType): Promise<Paginated<UpdateFeedRow>> {
		return this.opts.hosted.updatesFeed!(page, type);
	}
	canonicalSeries(workId: Id): Promise<Series> {
		return this.opts.hosted.canonicalSeries!(workId);
	}
	/**
	 * The hosted canonical chapter list is ALWAYS the authoritative return — its uuid
	 * ids, order, and shape are untouched, so the reader's `setProgress(chapter.id)`
	 * keeps routing canonical uuids to canonical progress. As a best-effort side effect,
	 * when the local engine is ready this reconciles the hosted chapters against the
	 * engine's live chapters BY NUMBER (D7) and records `canonicalChapterId → engineChapterId`
	 * in {@link localChapterMap}, which {@link canonicalPages} later consults to serve
	 * page bytes on-device. Any local failure is swallowed — reconciliation never
	 * changes the returned list and never hard-fails the read.
	 *
	 * Fallback ladder (plan §4/§7): rung 2 is this local reconciliation; when a work
	 * proves unusable on-device it is memoed in {@link localUnusable} and every later
	 * open skips straight to the hosted list (rung 1) instead of re-provisioning a
	 * doomed source. The engine being not-ready is a transient state handled by
	 * {@link localReady} and is never memoed.
	 */
	async canonicalChapters(workId: Id): Promise<Chapter[]> {
		const chs = await this.opts.hosted.canonicalChapters!(workId);
		// Skip local entirely once this work is known-unusable on-device (memo short-circuit),
		// and skip when the engine isn't ready (transient — do NOT poison the memo for it).
		if (!this.localUnusable.has(String(workId)) && (await this.localReady())) {
			// `workSources` is a HOSTED read: a throw here is a transient hosted failure, not a
			// local-source problem, so fetch it OUTSIDE the mark-unusable region — it must never
			// poison the memo.
			let sources: WorkSource[];
			try {
				sources = await this.opts.hosted.workSources!(workId);
			} catch (err) {
				console.warn('[composite] workSources fetch failed; leaving local memo intact:', err);
				return chs;
			}
			const mapper = this.opts.local as unknown as Partial<RefMapper>;
			// A local backend with no `refFor` is a missing capability (instance-wide, not a
			// per-work failure), so skip without memoing — nothing to provision here.
			if (typeof mapper.refFor !== 'function') return chs;
			// MangaDex-preferred is already first; require a usable mapping (MangaDex, or
			// a suwayomi source carrying extension provisioning coords).
			const ws = sources.find((s) => s.sourceType === 'mangadex' || s.extension != null);
			if (!ws) {
				// No source this engine can resolve/provision → unusable on-device for this work.
				this.localUnusable.add(String(workId));
				return chs;
			}
			try {
				// Title hint is skipped: MangaDex resolves by uuid without it (a Keiyoushi
				// title-search fallback would cost an extra canonicalSeries round-trip).
				const ref = await mapper.refFor(ws);
				if (!ref) {
					// Provisioning/resolution genuinely failed (extension can't install, repo down,
					// incompatible, source errored) → memo so subsequent opens skip the doomed path.
					this.localUnusable.add(String(workId));
					return chs;
				}
				const engineChs = await this.opts.local!.chapters(ref);
				for (const ch of chs) {
					const matches = engineChs.filter((e) => Math.abs(e.number - ch.number) < 1e-6);
					if (matches.length === 0) continue;
					// Disambiguate same-number engine chapters by scanlator, else take the first.
					const pick =
						matches.length > 1
							? (matches.find((e) => e.scanlator != null && e.scanlator === ch.scanlator) ??
								matches[0])
							: matches[0];
					this.localChapterMap.set(ch.id, String(pick.id));
				}
				// A zero-match reconciliation is intentionally NOT memoed: the source resolved
				// fine, it's a data/number mismatch a later catalogue update may fix. Leaving it
				// unmarked lets a future open retry the match; canonicalPages still falls back to
				// hosted in the meantime.
			} catch (err) {
				// `refFor`/`chapters` threw — a genuine per-source provisioning/resolution failure
				// (§4). Memo it and fall back to hosted; reconciliation never hard-fails the read.
				this.localUnusable.add(String(workId));
				console.warn(
					'[composite] local chapter reconciliation failed; marking work unusable on-device:',
					err,
				);
			}
		}
		return chs;
	}
	/**
	 * Page image bytes are served live from the embedded engine when this canonical
	 * chapter was reconciled to an engine chapter (see {@link canonicalChapters}) and
	 * the engine is ready; on ANY local error we fall through to the hosted proxy. The
	 * chapter id here is the canonical uuid, looked up in {@link localChapterMap} to the
	 * engine's integer chapter id — the engine id never leaks back to identity/progress.
	 */
	async canonicalPages(chapterId: Id): Promise<Page[]> {
		if ((await this.localReady()) && this.localChapterMap.has(chapterId)) {
			try {
				return await this.opts.local!.pages(this.localChapterMap.get(chapterId)!);
			} catch (err) {
				// Fall through to hosted on any local failure — never hard-fail the read.
				console.warn('[composite] local canonicalPages failed, using hosted:', err);
			}
		}
		return this.opts.hosted.canonicalPages!(chapterId);
	}
	workSources(workId: Id): Promise<WorkSource[]> {
		return this.opts.hosted.workSources!(workId);
	}
	workSourcesBatch(workIds: Id[]): Promise<WorkSourceGroup[]> {
		return this.opts.hosted.workSourcesBatch!(workIds);
	}

	// --- social ---
	reviews(seriesId: Id, page?: number): Promise<Paginated<Review>> {
		return this.opts.hosted.reviews(seriesId, page);
	}
	myReview(seriesId: Id): Promise<Review | null> {
		return this.opts.hosted.myReview!(seriesId);
	}
	postReview(input: PostReviewInput): Promise<Review> {
		return this.opts.hosted.postReview(input);
	}
	comments(
		targetType: CommentTargetType,
		targetId: Id,
		page?: number,
	): Promise<Paginated<Comment>> {
		return this.opts.hosted.comments(targetType, targetId, page);
	}
	postComment(input: PostCommentInput): Promise<Comment> {
		return this.opts.hosted.postComment(input);
	}
	uploadCommentMedia(file: Blob): Promise<CommentMediaUpload> {
		return this.opts.hosted.uploadCommentMedia!(file);
	}
	// All social writes/reads delegate to the hosted backend; null-safe so a hosted
	// backend without a social layer degrades gracefully instead of throwing.
	voteComment(commentId: Id, value: number): Promise<CommentVote> {
		if (!this.opts.hosted.voteComment) return Promise.reject(new Error('Voting unavailable'));
		return this.opts.hosted.voteComment(commentId, value);
	}
	notifications(page?: number): Promise<Notification[]> {
		return this.opts.hosted.notifications?.(page) ?? Promise.resolve([]);
	}
	unreadNotificationCount(): Promise<number> {
		return this.opts.hosted.unreadNotificationCount?.() ?? Promise.resolve(0);
	}
	markNotificationsRead(ids?: Id[]): Promise<number> {
		return this.opts.hosted.markNotificationsRead?.(ids) ?? Promise.resolve(0);
	}

	/**
	 * Reader reports (Support → Report an issue) — always the hosted backend, never the
	 * native engine: the report queue is a property of the hosted catalogue, and an
	 * on-device engine has nobody to deliver a report to.
	 *
	 * NOT null-safe like the social reads above, deliberately. Those degrade to an empty
	 * list because a missing feed is survivable; a report that silently evaporates is the
	 * one outcome a reporting feature must never have, so an absent hosted implementation
	 * throws and the form shows the failure. Without this delegation at all, the desktop
	 * build's report form failed on every submit — `CompositeBackend` implements `Backend`
	 * explicitly, and `submitReport` being optional on the interface meant its absence
	 * compiled cleanly.
	 */
	submitReport(input: ReportInput): Promise<Report> {
		if (!this.opts.hosted.submitReport) {
			return Promise.reject(new Error('Reporting needs the live Komiq backend.'));
		}
		return this.opts.hosted.submitReport(input);
	}

	// --- admin "manga DB" ---
	updateSeriesAdmin(input: SeriesAdminInput): Promise<Series> {
		return this.opts.hosted.updateSeriesAdmin!(input);
	}
	scanStatus(): Promise<ScanStatus> {
		return this.opts.hosted.scanStatus!();
	}
	triggerScan(seriesId: Id): Promise<Series> {
		return this.opts.hosted.triggerScan!(seriesId);
	}

	// --- admin series-detail editor ---
	seriesAdminMeta(seriesId: Id): Promise<SeriesAdminMeta> {
		return this.opts.hosted.seriesAdminMeta!(seriesId);
	}
	updateSeriesMetadata(input: SeriesMetadataInput): Promise<Series> {
		return this.opts.hosted.updateSeriesMetadata!(input);
	}
	workChaptersAdmin(workId: Id): Promise<AdminChapter[]> {
		return this.opts.hosted.workChaptersAdmin!(workId);
	}
	setChapterOverride(input: ChapterOverrideInput): Promise<boolean> {
		return this.opts.hosted.setChapterOverride!(input);
	}
	rescanWork(workId: Id): Promise<number> {
		return this.opts.hosted.rescanWork!(workId);
	}

	// --- admin moderation ---
	banUser(userId: Id, banned: boolean): Promise<AdminUser> {
		return this.opts.hosted.banUser!(userId, banned);
	}
	deleteComment(commentId: Id): Promise<boolean> {
		return this.opts.hosted.deleteComment!(commentId);
	}

	// --- admin user management ---
	users(page?: number): Promise<Paginated<AdminUser>> {
		return this.opts.hosted.users!(page);
	}
	setUserAdmin(userId: Id, isAdmin: boolean): Promise<AdminUser> {
		return this.opts.hosted.setUserAdmin!(userId, isAdmin);
	}

	// --- admin dedup review ---
	mergeQueue(page?: number, limit?: number): Promise<Paginated<MergeCandidate>> {
		return this.opts.hosted.mergeQueue!(page, limit);
	}
	resolveMergeCandidate(id: Id, accept: boolean): Promise<boolean> {
		return this.opts.hosted.resolveMergeCandidate!(id, accept);
	}
	addSourceSeries(suwayomiMangaId: Id): Promise<MatchResult> {
		return this.opts.hosted.addSourceSeries!(suwayomiMangaId);
	}
	mergeWorks(sourceWorkId: Id, targetWorkId: Id): Promise<MergeWorksResult> {
		return this.opts.hosted.mergeWorks!(sourceWorkId, targetWorkId);
	}
	workSourceRows(workId: Id): Promise<WorkSourceRow[]> {
		return this.opts.hosted.workSourceRows!(workId);
	}
	splitSourceSeries(workId: Id, sourceSeriesIds: Id[]): Promise<SplitSourcesResult> {
		return this.opts.hosted.splitSourceSeries!(workId, sourceSeriesIds);
	}

	// --- admin sources & extensions (EXT-1/EXT-2) ---
	// All hosted-only: the embedded engine is content-fetch, not catalogue admin, so
	// these delegate straight through. Forwarded explicitly (rather than left to the
	// optional-method feature-detect) so native mode keeps the full admin surface.
	extensions(refresh?: boolean): Promise<ExtensionInfo[]> {
		return this.opts.hosted.extensions!(refresh);
	}
	sources(): Promise<SourceInfo[]> {
		return this.opts.hosted.sources!();
	}
	sourceBrowse(
		sourceId: Id,
		type: SourceBrowseType,
		page?: number,
		query?: string,
	): Promise<SourceBrowsePage> {
		return this.opts.hosted.sourceBrowse!(sourceId, type, page, query);
	}
	addExtensionRepo(indexUrl: string): Promise<number> {
		return this.opts.hosted.addExtensionRepo!(indexUrl);
	}
	installExtension(pkgName: string): Promise<ExtensionInfo> {
		return this.opts.hosted.installExtension!(pkgName);
	}
	uninstallExtension(pkgName: string): Promise<ExtensionInfo> {
		return this.opts.hosted.uninstallExtension!(pkgName);
	}
	updateExtension(pkgName: string): Promise<ExtensionInfo> {
		return this.opts.hosted.updateExtension!(pkgName);
	}
	bulkAddSourceSeries(suwayomiMangaIds: Id[]): Promise<BulkAddResult> {
		return this.opts.hosted.bulkAddSourceSeries!(suwayomiMangaIds);
	}
	seriesSourcesBatch(seriesIds: Id[]): Promise<SeriesSourceGroup[]> {
		return this.opts.hosted.seriesSourcesBatch!(seriesIds);
	}
	setSeriesPaused(seriesId: Id, paused: boolean): Promise<Series> {
		return this.opts.hosted.setSeriesPaused!(seriesId, paused);
	}

	sourceScanHealth(): Promise<SourceScanHealth[]> {
		return this.opts.hosted.sourceScanHealth!();
	}

	// --- admin background source-ingest jobs (S1) ---
	sourceIngestJobs(active?: boolean): Promise<SourceIngestJob[]> {
		return this.opts.hosted.sourceIngestJobs!(active);
	}
	startSourceIngest(sourceId: Id): Promise<SourceIngestJob> {
		return this.opts.hosted.startSourceIngest!(sourceId);
	}
	cancelSourceIngest(jobId: Id): Promise<SourceIngestJob> {
		return this.opts.hosted.cancelSourceIngest!(jobId);
	}
	startExtensionIngest(pkgName: Id): Promise<SourceIngestJob[]> {
		return this.opts.hosted.startExtensionIngest!(pkgName);
	}
	cancelExtensionIngest(pkgName: Id): Promise<SourceIngestJob[]> {
		return this.opts.hosted.cancelExtensionIngest!(pkgName);
	}
	setExtensionSubscription(pkgName: Id, subscribed: boolean): Promise<boolean> {
		return this.opts.hosted.setExtensionSubscription!(pkgName, subscribed);
	}

	// --- admin maintenance ---
	persistCatalogue(): Promise<number> {
		return this.opts.hosted.persistCatalogue!();
	}

	materializeCatalogueCovers(): Promise<number> {
		return this.opts.hosted.materializeCatalogueCovers!();
	}

	// Popularity view counting is a hosted write; the embedded engine never counts.
	// Best-effort (a hosted backend that doesn't track views simply no-ops).
	recordView(seriesId: Id): Promise<void> {
		return this.opts.hosted.recordView?.(seriesId) ?? Promise.resolve();
	}
}

/** Build a {@link CompositeBackend} from a hosted backend and an optional local
 * content engine (mirrors {@link createBackend}). */
export function createCompositeBackend(opts: {
	hosted: Backend;
	local?: ContentBackend | null;
	queue?: OfflineWriteQueue;
}): Backend {
	return new CompositeBackend(opts);
}
