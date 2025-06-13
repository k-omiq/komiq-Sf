import type {
	AdminUser,
	CoverIssue,
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
	MergeCandidateRow,
	MergeWorksResult,
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
import * as ops from './operations.js';

export interface BackendConfig {
	/** GraphQL endpoint of the unified backend, e.g. https://api.komika.app/graphql */
	endpoint: string;
	/** Bearer token for authenticated requests, if signed in. */
	token?: string | null;
	/** Optional fetch override (SSR / tests). Defaults to global fetch. */
	fetch?: typeof fetch;
}

/**
 * A page number that is safe to send.
 *
 * Every paginated argument is `Int! = 1` SERVER-side but declared `$page: Int` in
 * `operations.ts` — which is legal (a nullable variable may fill a non-null location that
 * has a default) right up until someone sends an explicit `null`. GraphQL then rejects the
 * whole OPERATION, not just the field: one `page: null` and the entire query 400s. The
 * TypeScript default (`page = 1`) does not catch it, because a default only fires on
 * `undefined`, and `null` is exactly what a `?page=` URL param, a nullable load-function
 * property or a JS caller produces.
 *
 * Normalising here rather than widening the declaration to `$page: Int!` on purpose: the
 * server's default is the one that should decide what "no page" means, and making the
 * variable required would force every caller to state a page it does not care about.
 */
const pageArg = (page?: number | null): number => page ?? 1;

/**
 * GraphQL-backed implementation of {@link Backend}, targeting Komika's unified
 * API. Operations are domain-shaped (see `operations.ts` + `schema/komika.graphql`),
 * so responses map 1:1 onto `@komika/types` with no translation layer.
 */
export class GraphQLBackend implements Backend {
	constructor(private config: BackendConfig) {}

	setToken(token: string | null): void {
		this.config = { ...this.config, token };
	}

	/**
	 * Optional fields (see {@link ops.OPTIONAL_SERIES_FIELDS}) this API has told us
	 * it doesn't know. Process-wide: the reader and the API deploy separately, so a
	 * client shipped first must not break — and once one document is rejected every
	 * document that shares the fragment would be too.
	 */
	private static readonly unsupportedFields = new Set<string>();

	/**
	 * Optional ARGUMENTS (see {@link ops.OPTIONAL_ARGUMENTS}) this API has told us it
	 * doesn't know. Same lifetime and rationale as {@link unsupportedFields}.
	 */
	private static readonly unsupportedArgs = new Set<string>();

	/**
	 * Drop already-known-unsupported field selections (one per line by construction)
	 * and optional arguments (both the `$name: Type` declaration and the `name: $name`
	 * use, wherever they sit — the leftover whitespace/comma is an ignored token in
	 * GraphQL, so the document stays valid).
	 *
	 * This rewrites a GraphQL document with regexes, so it is deliberately CONSERVATIVE:
	 * it can only ever produce a document that still parses, or the ORIGINAL document.
	 * Two shapes would otherwise be left syntactically invalid, and both are guarded:
	 *
	 *  - removing the last argument leaves an empty `()`, which is a syntax error
	 *    (`ArgumentsDefinition`/`VariableDefinitions` must be non-empty). Collapsed
	 *    below. Trailing/duplicated commas need no handling — commas are ignored
	 *    tokens in GraphQL, so `foo(page: $page, )` is valid as-is. A variable's
	 *    DEFAULT VALUE does have to go with it: commas being ignored, an orphaned
	 *    `= false` silently re-binds to the PRECEDING variable definition
	 *    (`$page: Int, $x: Boolean = false` -> `$page: Int = false`, which parses).
	 *    Only scalar/enum defaults are matched; a list/object default is left alone
	 *    and caught by the bail-out below only if it also empties a selection set, so
	 *    keep optional arguments default-less (none in `operations.ts` use one).
	 *  - removing the last selection in a set leaves `{ }`, which is a syntax error and
	 *    CANNOT be repaired locally (the parent field would have to go too). If the
	 *    strip introduces one, we bail and return the original: the request then fails
	 *    with the server's real `Unknown field` error instead of a baffling syntax
	 *    error, which is the honest outcome.
	 */
	private stripUnsupported(query: string): string {
		if (GraphQLBackend.unsupportedFields.size === 0 && GraphQLBackend.unsupportedArgs.size === 0)
			return query;
		let out = query;
		for (const field of GraphQLBackend.unsupportedFields) {
			out = out.replace(new RegExp(`^[\\t ]*${field}[\\t ]*\\r?\\n`, 'gm'), '');
		}
		for (const arg of GraphQLBackend.unsupportedArgs) {
			out = out
				.replace(new RegExp(`\\b${arg}\\s*:\\s*\\$${arg}\\b`, 'g'), '')
				.replace(
					new RegExp(
						`\\$${arg}\\s*:\\s*\\[?[A-Za-z_][A-Za-z0-9_]*!?\\]?!?` +
							`(?:\\s*=\\s*(?:-?\\d+(?:\\.\\d+)?|"(?:[^"\\\\]|\\\\.)*"|[A-Za-z_][A-Za-z0-9_]*))?`,
						'g',
					),
					'',
				);
		}
		// `(` + nothing but ignored tokens + `)` — never valid, so dropping it is always
		// an improvement (`query Q()` -> `query Q`, `field()` -> `field`).
		if (GraphQLBackend.unsupportedArgs.size > 0) out = out.replace(/\(\s*(?:,\s*)*\)/g, '');
		// Emptied a selection set? Only count sets the strip CREATED, so a legitimately
		// empty object value in the source document (`input: {}`) can't disable this.
		const emptySets = (s: string) => (s.match(/\{\s*\}/g) ?? []).length;
		if (emptySets(out) > emptySets(query)) return query;
		return out;
	}

	/**
	 * Whether this API has told us it doesn't serve `GET /graphql` (P3). Process-wide
	 * for the same deploy-order reason as {@link unsupportedFields}: a 404/405 from one
	 * GET means every GET would fail, so remember it and go straight to POST.
	 */
	private static getUnsupported = false;

	private async gql<T>(
		query: string,
		variables?: Record<string, unknown>,
		retried = false,
	): Promise<T> {
		const doFetch = this.config.fetch ?? fetch;
		const stripped = this.stripUnsupported(query);
		// Anonymous READS go over GET so Cloudflare and the browser's HTTP cache can
		// serve them (the server sends Cache-Control + ETag on anonymous GETs; 304
		// revalidation is the browser's job, no client code needed). An authed viewer
		// must NEVER take this path: edge caches key on the URL alone, so a cached
		// response carrying viewer-specific fields (isMarked, myReview…) would be
		// replayed to other users. Mutations are detected on the document text —
		// `operations.ts` always writes a top-level `mutation Name(`/`query Name(`
		// (fragments may come first, hence the newline alternative).
		const useGet =
			!this.config.token &&
			!GraphQLBackend.getUnsupported &&
			!/(^|\n)\s*mutation[\s(]/.test(stripped);
		let res: Response;
		if (useGet) {
			// Single-operation documents, so no `operationName`; `variables` only when
			// there are any. No Authorization header by construction (no token here).
			const params =
				`query=${encodeURIComponent(stripped)}` +
				(variables ? `&variables=${encodeURIComponent(JSON.stringify(variables))}` : '');
			res = await doFetch(`${this.config.endpoint}?${params}`);
			if (res.status === 404 || res.status === 405) {
				// The server predates GET /graphql — schema-drift fallback, not an outage.
				// Memoize and re-enter: the retry re-evaluates the transport and lands on
				// POST, preserving `retried` so the unknown-field loop is unaffected. Any
				// OTHER status is NOT memoized — a 500 or a network failure must throw
				// exactly as it does on POST.
				GraphQLBackend.getUnsupported = true;
				return this.gql<T>(query, variables, retried);
			}
		} else {
			res = await doFetch(this.config.endpoint, {
				method: 'POST',
				headers: {
					'content-type': 'application/json',
					...(this.config.token ? { authorization: `Bearer ${this.config.token}` } : {}),
				},
				body: JSON.stringify({ query: stripped, variables }),
			});
		}
		if (!res.ok) throw new Error(`Backend error ${res.status}`);
		const json = (await res.json()) as { data?: T; errors?: { message: string }[] };
		if (json.errors?.length) {
			const messages = json.errors.map((e) => e.message);
			// An unknown field fails the WHOLE document, so a server that predates an
			// opportunistic field would otherwise take down every screen. Remember it,
			// strip it, retry once — the mapped type declares those fields optional and
			// callers already fall back.
			// An unknown ARGUMENT (`includeNsfw`) is a different validation error but the
			// same failure mode, so it gets the same treatment. Both are collected in one
			// pass: validation reports every error at once, and there is only one retry.
			if (!retried) {
				const dropped = ops.OPTIONAL_SERIES_FIELDS.filter(
					(f) =>
						!GraphQLBackend.unsupportedFields.has(f) &&
						messages.some((m) => m.includes(`Unknown field "${f}"`)),
				);
				const droppedArgs = ops.OPTIONAL_ARGUMENTS.filter(
					(a) =>
						!GraphQLBackend.unsupportedArgs.has(a) &&
						messages.some((m) => m.includes(`Unknown argument "${a}"`)),
				);
				if (dropped.length || droppedArgs.length) {
					for (const f of dropped) GraphQLBackend.unsupportedFields.add(f);
					for (const a of droppedArgs) GraphQLBackend.unsupportedArgs.add(a);
					return this.gql<T>(query, variables, true);
				}
			}
			throw new Error(messages.join('; '));
		}
		if (json.data == null) throw new Error('Backend returned no data');
		return json.data;
	}

	// --- auth ---
	async session(): Promise<Session | null> {
		const d = await this.gql<{ session: Session | null }>(ops.SESSION);
		return d.session;
	}
	async login(username: string, password: string): Promise<Session> {
		const d = await this.gql<{ login: Session }>(ops.LOGIN, { username, password });
		return d.login;
	}
	async register(input: RegisterInput): Promise<Session> {
		const d = await this.gql<{ register: Session }>(ops.REGISTER, { input });
		return d.register;
	}
	async logout(): Promise<void> {
		await this.gql<{ logout: boolean }>(ops.LOGOUT);
	}

	// --- catalog ---
	async discovery(): Promise<DiscoveryFeed[]> {
		const d = await this.gql<{ discovery: DiscoveryFeed[] }>(ops.DISCOVERY);
		return d.discovery;
	}
	async updates(page = 1): Promise<Paginated<Series>> {
		const d = await this.gql<{ updates: Paginated<Series> }>(ops.UPDATES, { page: pageArg(page) });
		return d.updates;
	}
	async search(
		query: string,
		page = 1,
		filters?: SearchFilters,
		includeNsfw?: boolean,
	): Promise<Paginated<Series>> {
		const d = await this.gql<{ search: Paginated<Series> }>(ops.SEARCH, {
			query,
			page: pageArg(page),
			// An EMPTY list is not "match nothing" here, it is "no filter" — send
			// `undefined` so the server omits the clause entirely. `[]` would reach the
			// resolver as an empty IN (…) and return zero rows, i.e. selecting no genre
			// chips would empty the grid.
			genres: filters?.genres && filters.genres.length ? filters.genres : undefined,
			minRating: filters?.minRating,
			maxRating: filters?.maxRating,
			types: filters?.types && filters.types.length ? filters.types : undefined,
			status: filters?.status,
			sort: filters?.sort,
			contentRating: filters?.contentRating,
			includeNsfw,
			// `undefined`, never `false`, when unset: `false` is a MEANINGFUL value here
			// (only works with no chapters), so coercing an absent filter to it would
			// hide the entire readable catalogue.
			hasChapters: filters?.hasChapters,
		});
		return d.search;
	}
	async searchAllSources(query: string, page = 1): Promise<FederatedSearchPage> {
		const d = await this.gql<{ searchAllSources: FederatedSearchPage }>(ops.SEARCH_ALL_SOURCES, {
			query,
			page: pageArg(page),
		});
		return d.searchAllSources;
	}
	async genreFacets(): Promise<GenreFacet[]> {
		const d = await this.gql<{ genreFacets: GenreFacet[] }>(ops.GENRE_FACETS);
		return d.genreFacets;
	}
	async series(id: Id): Promise<Series> {
		const d = await this.gql<{ series: Series }>(ops.SERIES, { id });
		return d.series;
	}
	async chapters(seriesId: Id): Promise<Chapter[]> {
		const d = await this.gql<{ chapters: Chapter[] }>(ops.CHAPTERS, { seriesId });
		return d.chapters;
	}
	async aggregatedChapters(workId: Id): Promise<AggregatedChapter[]> {
		const d = await this.gql<{ aggregatedChapters: AggregatedChapter[] }>(ops.AGGREGATED_CHAPTERS, {
			workId,
		});
		return d.aggregatedChapters;
	}
	async pages(chapterId: Id): Promise<Page[]> {
		const d = await this.gql<{ pages: Page[] }>(ops.PAGES, { chapterId });
		return d.pages;
	}

	// --- library & progress ---
	async mark(seriesId: Id, marked: boolean): Promise<Series> {
		const d = await this.gql<{ mark: Series }>(ops.MARK, { seriesId, marked });
		return d.mark;
	}
	async setLibraryStatus(seriesId: Id, status: LibraryStatus | null): Promise<Series> {
		const d = await this.gql<{ setLibraryStatus: Series }>(ops.SET_LIBRARY_STATUS, {
			seriesId,
			status,
		});
		return d.setLibraryStatus;
	}
	async setFavorite(seriesId: Id, favorite: boolean): Promise<Series> {
		const d = await this.gql<{ setFavorite: Series }>(ops.SET_FAVORITE, { seriesId, favorite });
		return d.setFavorite;
	}
	async library(): Promise<Series[]> {
		const d = await this.gql<{ library: Series[] }>(ops.LIBRARY);
		return d.library;
	}
	async libraryProgress(): Promise<SeriesProgress[]> {
		const d = await this.gql<{ libraryProgress: SeriesProgress[] }>(ops.LIBRARY_PROGRESS);
		return d.libraryProgress;
	}
	async setProgress(chapterId: Id, lastPageRead: number, read: boolean): Promise<void> {
		await this.gql<{ setProgress: boolean }>(ops.SET_PROGRESS, { chapterId, lastPageRead, read });
	}

	async recordView(seriesId: Id): Promise<void> {
		await this.gql<{ recordView: boolean }>(ops.RECORD_VIEW, { seriesId });
	}

	// --- social ---
	async reviews(seriesId: Id, page = 1): Promise<Paginated<Review>> {
		const d = await this.gql<{ reviews: Paginated<Review> }>(ops.REVIEWS, { seriesId, page: pageArg(page) });
		return d.reviews;
	}
	async myReview(seriesId: Id): Promise<Review | null> {
		const d = await this.gql<{ myReview: Review | null }>(ops.MY_REVIEW, { seriesId });
		return d.myReview;
	}
	async postReview(input: PostReviewInput): Promise<Review> {
		const d = await this.gql<{ postReview: Review }>(ops.POST_REVIEW, { input });
		return d.postReview;
	}
	async deleteReview(seriesId: Id): Promise<boolean> {
		const d = await this.gql<{ deleteReview: boolean }>(ops.DELETE_REVIEW, { seriesId });
		return d.deleteReview;
	}
	async comments(
		targetType: CommentTargetType,
		targetId: Id,
		page = 1,
	): Promise<Paginated<Comment>> {
		const d = await this.gql<{ comments: Paginated<Comment> }>(ops.COMMENTS, {
			targetType,
			targetId,
			page: pageArg(page),
		});
		return d.comments;
	}
	async uploadCommentMedia(file: Blob): Promise<CommentMediaUpload> {
		const doFetch = this.config.fetch ?? fetch;
		const url = this.config.endpoint.replace(/\/graphql\/?$/, '') + '/comment-media';
		const form = new FormData();
		form.append('image', file);
		const res = await doFetch(url, {
			method: 'POST',
			headers: this.config.token ? { authorization: `Bearer ${this.config.token}` } : {},
			body: form,
		});
		const json = (await res.json().catch(() => null)) as
			(Partial<CommentMediaUpload> & { message?: string }) | null;
		if (!res.ok) throw new Error(json?.message ?? `Upload failed (${res.status})`);
		if (!json?.mediaId || !json.url) throw new Error('Upload returned no media id');
		return {
			mediaId: json.mediaId,
			url: json.url,
			width: json.width ?? 0,
			height: json.height ?? 0,
		};
	}

	async postComment(input: PostCommentInput): Promise<Comment> {
		const d = await this.gql<{ postComment: Comment }>(ops.POST_COMMENT, { input });
		return d.postComment;
	}

	async voteComment(commentId: Id, value: number): Promise<CommentVote> {
		const d = await this.gql<{ voteComment: CommentVote }>(ops.VOTE_COMMENT, { commentId, value });
		return d.voteComment;
	}

	// --- notifications ---
	async notifications(page = 1): Promise<Notification[]> {
		const d = await this.gql<{ notifications: Notification[] }>(ops.NOTIFICATIONS, { page: pageArg(page) });
		return d.notifications;
	}
	async unreadNotificationCount(): Promise<number> {
		const d = await this.gql<{ unreadNotificationCount: number }>(
			ops.UNREAD_NOTIFICATION_COUNT,
			{},
		);
		return d.unreadNotificationCount;
	}
	async markNotificationsRead(ids?: Id[]): Promise<number> {
		const d = await this.gql<{ markNotificationsRead: number }>(ops.MARK_NOTIFICATIONS_READ, {
			ids: ids ?? null,
		});
		return d.markNotificationsRead;
	}

	// --- admin ---
	async updateSeriesAdmin(input: SeriesAdminInput): Promise<Series> {
		const d = await this.gql<{ updateSeriesAdmin: Series }>(ops.UPDATE_SERIES_ADMIN, { input });
		return d.updateSeriesAdmin;
	}

	async scanStatus(): Promise<ScanStatus> {
		const d = await this.gql<{ scanStatus: ScanStatus }>(ops.SCAN_STATUS);
		return d.scanStatus;
	}

	async triggerScan(seriesId: Id): Promise<Series> {
		const d = await this.gql<{ triggerScan: Series }>(ops.TRIGGER_SCAN, { seriesId });
		return d.triggerScan;
	}

	// --- admin series-detail editor ---
	async seriesAdminMeta(seriesId: Id): Promise<SeriesAdminMeta> {
		const d = await this.gql<{ seriesAdminMeta: SeriesAdminMeta }>(ops.SERIES_ADMIN_META, {
			seriesId,
		});
		return d.seriesAdminMeta;
	}

	async updateSeriesMetadata(input: SeriesMetadataInput): Promise<Series> {
		const d = await this.gql<{ updateSeriesMetadata: Series }>(ops.UPDATE_SERIES_METADATA, {
			input,
		});
		return d.updateSeriesMetadata;
	}

	async addSeriesAltTitle(id: Id, title: string): Promise<Series> {
		const d = await this.gql<{ addSeriesAltTitle: Series }>(ops.ADD_SERIES_ALT_TITLE, {
			id,
			title,
		});
		return d.addSeriesAltTitle;
	}

	async removeSeriesAltTitle(id: Id, title: string): Promise<Series> {
		const d = await this.gql<{ removeSeriesAltTitle: Series }>(ops.REMOVE_SERIES_ALT_TITLE, {
			id,
			title,
		});
		return d.removeSeriesAltTitle;
	}

	async consolidateExactDuplicates(limit?: number): Promise<number> {
		const d = await this.gql<{ consolidateExactDuplicates: number }>(
			ops.CONSOLIDATE_EXACT_DUPLICATES,
			{ limit },
		);
		return d.consolidateExactDuplicates;
	}

	async workChaptersAdmin(workId: Id): Promise<AdminChapter[]> {
		const d = await this.gql<{ workChaptersAdmin: AdminChapter[] }>(ops.WORK_CHAPTERS_ADMIN, {
			workId,
		});
		return d.workChaptersAdmin;
	}

	async setChapterOverride(input: ChapterOverrideInput): Promise<boolean> {
		const d = await this.gql<{ setChapterOverride: boolean }>(ops.SET_CHAPTER_OVERRIDE, { input });
		return d.setChapterOverride;
	}

	async rescanWork(workId: Id): Promise<number> {
		const d = await this.gql<{ rescanWork: number }>(ops.RESCAN_WORK, { workId });
		return d.rescanWork;
	}

	// --- admin moderation ---
	async banUser(userId: Id, banned: boolean): Promise<AdminUser> {
		const d = await this.gql<{ banUser: AdminUser }>(ops.BAN_USER, { userId, banned });
		return d.banUser;
	}

	async deleteComment(commentId: Id): Promise<boolean> {
		const d = await this.gql<{ deleteComment: boolean }>(ops.DELETE_COMMENT, { commentId });
		return d.deleteComment;
	}

	// --- admin user management ---
	async users(page = 1): Promise<Paginated<AdminUser>> {
		const d = await this.gql<{ users: Paginated<AdminUser> }>(ops.USERS, { page: pageArg(page) });
		return d.users;
	}

	async setUserAdmin(userId: Id, isAdmin: boolean): Promise<AdminUser> {
		const d = await this.gql<{ setUserAdmin: AdminUser }>(ops.SET_USER_ADMIN, { userId, isAdmin });
		return d.setUserAdmin;
	}

	// --- admin dedup review ---
	async mergeQueue(page = 1, limit?: number): Promise<Paginated<MergeCandidate>> {
		const d = await this.gql<{ mergeQueue: Paginated<MergeCandidate> }>(ops.MERGE_QUEUE, {
			page: pageArg(page),
			limit,
		});
		return d.mergeQueue;
	}

	async resolveMergeCandidate(id: Id, accept: boolean): Promise<boolean> {
		const d = await this.gql<{ resolveMergeCandidate: boolean }>(ops.RESOLVE_MERGE_CANDIDATE, {
			id,
			accept,
		});
		return d.resolveMergeCandidate;
	}

	async workReviewDetail(workId: Id): Promise<WorkReviewDetail> {
		const d = await this.gql<{ workReviewDetail: WorkReviewDetail }>(ops.WORK_REVIEW_DETAIL, {
			workId,
		});
		return d.workReviewDetail;
	}

	async addSourceSeries(suwayomiMangaId: Id): Promise<MatchResult> {
		const d = await this.gql<{ addSourceSeries: MatchResult }>(ops.ADD_SOURCE_SERIES, {
			suwayomiMangaId,
		});
		return d.addSourceSeries;
	}

	// --- reader reports (Support → Report an issue) ---
	async submitReport(input: ReportInput): Promise<Report> {
		const d = await this.gql<{ submitReport: Report }>(ops.SUBMIT_REPORT, { input });
		return d.submitReport;
	}

	async reports(
		status: ReportStatus | null = null,
		kind: ReportKind | null = null,
		page = 1,
	): Promise<ReportPage> {
		const d = await this.gql<{ reports: ReportPage }>(ops.REPORTS, { status, kind, page: pageArg(page) });
		return d.reports;
	}

	async resolveReport(
		reportId: Id,
		status: ReportStatus,
		note: string | null = null,
	): Promise<Report> {
		const d = await this.gql<{ resolveReport: Report }>(ops.RESOLVE_REPORT, {
			reportId,
			status,
			note,
		});
		return d.resolveReport;
	}

	// --- admin cover issues / "Bugs" panel ---
	async coverIssues(page = 1): Promise<Paginated<CoverIssue>> {
		const d = await this.gql<{ coverIssues: Paginated<CoverIssue> }>(ops.COVER_ISSUES, { page: pageArg(page) });
		return d.coverIssues;
	}

	async retryCover(workId: Id): Promise<boolean> {
		const d = await this.gql<{ retryCover: boolean }>(ops.RETRY_COVER, { workId });
		return d.retryCover;
	}

	async uploadCover(workId: Id, file: Blob): Promise<string> {
		const doFetch = this.config.fetch ?? fetch;
		const base = this.config.endpoint.replace(/\/graphql\/?$/, '');
		const url = `${base}/admin/cover/${encodeURIComponent(workId)}`;
		const form = new FormData();
		form.append('cover', file);
		const res = await doFetch(url, {
			method: 'POST',
			headers: this.config.token ? { authorization: `Bearer ${this.config.token}` } : {},
			body: form,
		});
		const json = (await res.json().catch(() => null)) as {
			coverUrl?: string;
			message?: string;
		} | null;
		if (!res.ok) throw new Error(json?.message ?? `Upload failed (${res.status})`);
		if (!json?.coverUrl) throw new Error('Upload returned no cover URL');
		return json.coverUrl;
	}

	async searchForMerge(query: string, page = 1): Promise<Paginated<MergeCandidateRow>> {
		const d = await this.gql<{ search: Paginated<MergeCandidateRow> }>(ops.SEARCH_FOR_MERGE, {
			query,
			page: pageArg(page),
			// Literal `true`, not a caller flag: the picker always wants the full
			// catalogue. The server gates it on admin, so this is not an escalation.
			includeNsfw: true,
		});
		return d.search;
	}

	async mergeWorks(sourceWorkId: Id, targetWorkId: Id): Promise<MergeWorksResult> {
		const d = await this.gql<{ mergeWorks: MergeWorksResult }>(ops.MERGE_WORKS, {
			sourceWorkId,
			targetWorkId,
		});
		return d.mergeWorks;
	}

	async workSourceRows(workId: Id): Promise<WorkSourceRow[]> {
		const d = await this.gql<{ workSourceRows: WorkSourceRow[] }>(ops.WORK_SOURCE_ROWS, {
			workId,
		});
		return d.workSourceRows;
	}

	async splitSourceSeries(workId: Id, sourceSeriesIds: Id[]): Promise<SplitSourcesResult> {
		const d = await this.gql<{ splitSourceSeries: SplitSourcesResult }>(ops.SPLIT_SOURCE_SERIES, {
			workId,
			sourceSeriesIds,
		});
		return d.splitSourceSeries;
	}

	// --- admin sources & extensions (EXT-1/EXT-2) ---
	async extensions(refresh = false): Promise<ExtensionInfo[]> {
		const d = await this.gql<{ extensions: ExtensionInfo[] }>(ops.EXTENSIONS, { refresh });
		return d.extensions;
	}

	async sources(): Promise<SourceInfo[]> {
		const d = await this.gql<{ sources: SourceInfo[] }>(ops.SOURCES);
		return d.sources;
	}

	async sourceBrowse(
		sourceId: Id,
		type: SourceBrowseType,
		page = 1,
		query?: string,
	): Promise<SourceBrowsePage> {
		const d = await this.gql<{ sourceBrowse: SourceBrowsePage }>(ops.SOURCE_BROWSE, {
			sourceId,
			type,
			page: pageArg(page),
			query: query ?? null,
		});
		return d.sourceBrowse;
	}

	async addExtensionRepo(indexUrl: string): Promise<number> {
		const d = await this.gql<{ addExtensionRepo: number }>(ops.ADD_EXTENSION_REPO, { indexUrl });
		return d.addExtensionRepo;
	}

	async installExtension(pkgName: string): Promise<ExtensionInfo> {
		const d = await this.gql<{ installExtension: ExtensionInfo }>(ops.INSTALL_EXTENSION, {
			pkgName,
		});
		return d.installExtension;
	}

	async uninstallExtension(pkgName: string): Promise<ExtensionInfo> {
		const d = await this.gql<{ uninstallExtension: ExtensionInfo }>(ops.UNINSTALL_EXTENSION, {
			pkgName,
		});
		return d.uninstallExtension;
	}

	async updateExtension(pkgName: string): Promise<ExtensionInfo> {
		const d = await this.gql<{ updateExtension: ExtensionInfo }>(ops.UPDATE_EXTENSION, {
			pkgName,
		});
		return d.updateExtension;
	}

	async bulkAddSourceSeries(suwayomiMangaIds: Id[]): Promise<BulkAddResult> {
		const d = await this.gql<{ bulkAddSourceSeries: BulkAddResult }>(ops.BULK_ADD_SOURCE_SERIES, {
			suwayomiMangaIds,
		});
		return d.bulkAddSourceSeries;
	}

	async seriesSourcesBatch(seriesIds: Id[]): Promise<SeriesSourceGroup[]> {
		const d = await this.gql<{ seriesSourcesBatch: SeriesSourceGroup[] }>(
			ops.SERIES_SOURCES_BATCH,
			{ seriesIds },
		);
		return d.seriesSourcesBatch;
	}

	async setSeriesPaused(seriesId: Id, paused: boolean): Promise<Series> {
		const d = await this.gql<{ setSeriesPaused: Series }>(ops.SET_SERIES_PAUSED, {
			seriesId,
			paused,
		});
		return d.setSeriesPaused;
	}

	async sourceScanHealth(): Promise<SourceScanHealth[]> {
		const d = await this.gql<{ sourceScanHealth: SourceScanHealth[] }>(ops.SOURCE_SCAN_HEALTH);
		return d.sourceScanHealth;
	}

	async sourceIngestJobs(active = false): Promise<SourceIngestJob[]> {
		const d = await this.gql<{ sourceIngestJobs: SourceIngestJob[] }>(ops.SOURCE_INGEST_JOBS, {
			active,
		});
		return d.sourceIngestJobs;
	}

	async startSourceIngest(sourceId: Id): Promise<SourceIngestJob> {
		const d = await this.gql<{ startSourceIngest: SourceIngestJob }>(ops.START_SOURCE_INGEST, {
			sourceId,
		});
		return d.startSourceIngest;
	}

	async cancelSourceIngest(jobId: Id): Promise<SourceIngestJob> {
		const d = await this.gql<{ cancelSourceIngest: SourceIngestJob }>(ops.CANCEL_SOURCE_INGEST, {
			jobId,
		});
		return d.cancelSourceIngest;
	}

	async startExtensionIngest(pkgName: Id): Promise<SourceIngestJob[]> {
		const d = await this.gql<{ startExtensionIngest: SourceIngestJob[] }>(
			ops.START_EXTENSION_INGEST,
			{ pkgName },
		);
		return d.startExtensionIngest;
	}

	async cancelExtensionIngest(pkgName: Id): Promise<SourceIngestJob[]> {
		const d = await this.gql<{ cancelExtensionIngest: SourceIngestJob[] }>(
			ops.CANCEL_EXTENSION_INGEST,
			{ pkgName },
		);
		return d.cancelExtensionIngest;
	}

	async setExtensionSubscription(pkgName: Id, subscribed: boolean): Promise<boolean> {
		const d = await this.gql<{ setExtensionSubscription: boolean }>(
			ops.SET_EXTENSION_SUBSCRIPTION,
			{ pkgName, subscribed },
		);
		return d.setExtensionSubscription;
	}

	// --- admin maintenance ---
	async persistCatalogue(): Promise<number> {
		const d = await this.gql<{ persistCatalogue: number }>(ops.PERSIST_CATALOGUE);
		return d.persistCatalogue;
	}

	async materializeCatalogueCovers(): Promise<number> {
		const d = await this.gql<{ materializeCatalogueCovers: number }>(
			ops.MATERIALIZE_CATALOGUE_COVERS,
		);
		return d.materializeCatalogueCovers;
	}

	// --- viewer preferences ---
	async setShowNsfw(value: boolean): Promise<boolean> {
		const d = await this.gql<{ setShowNsfw: boolean }>(ops.SET_SHOW_NSFW, { value });
		return d.setShowNsfw;
	}

	// --- profile ---
	async updateProfile(input: UpdateProfileInput): Promise<Session['user']> {
		const d = await this.gql<{ updateProfile: Session['user'] }>(ops.UPDATE_PROFILE, { input });
		return d.updateProfile;
	}
	async myActivity(limit = 20): Promise<Activity[]> {
		const d = await this.gql<{ myActivity: Activity[] }>(ops.MY_ACTIVITY, { limit });
		return d.myActivity;
	}
	/**
	 * Avatar upload is a REST multipart POST (not GraphQL): the endpoint is the
	 * API origin's `/avatar`, derived from the GraphQL `endpoint` by dropping the
	 * trailing `/graphql`. Returns the new avatar URL (a `/avatars/...` path,
	 * resolved against the API origin by the reader).
	 */
	async uploadAvatar(file: Blob): Promise<string> {
		const doFetch = this.config.fetch ?? fetch;
		const url = this.config.endpoint.replace(/\/graphql\/?$/, '') + '/avatar';
		const form = new FormData();
		form.append('avatar', file);
		const res = await doFetch(url, {
			method: 'POST',
			headers: this.config.token ? { authorization: `Bearer ${this.config.token}` } : {},
			body: form,
		});
		const json = (await res.json().catch(() => null)) as {
			avatarUrl?: string;
			message?: string;
		} | null;
		if (!res.ok) throw new Error(json?.message ?? `Upload failed (${res.status})`);
		if (!json?.avatarUrl) throw new Error('Upload returned no avatar URL');
		return json.avatarUrl;
	}

	// --- canonical catalogue ---
	async canonicalUpdates(page = 1, includeNsfw?: boolean): Promise<CanonicalUpdate[]> {
		const d = await this.gql<{ canonicalUpdates: CanonicalUpdate[] }>(ops.CANONICAL_UPDATES, {
			page: pageArg(page),
			includeNsfw,
		});
		return d.canonicalUpdates;
	}

	async updatesFeed(page = 1, type?: ComicType): Promise<Paginated<UpdateFeedRow>> {
		const d = await this.gql<{ updatesFeed: Paginated<UpdateFeedRow> }>(ops.UPDATES_FEED, {
			page: pageArg(page),
			type,
		});
		return d.updatesFeed;
	}

	// --- canonical reader path ---
	async canonicalSeries(workId: Id): Promise<Series> {
		const d = await this.gql<{ canonicalSeries: Series }>(ops.CANONICAL_SERIES, { workId });
		return d.canonicalSeries;
	}
	async canonicalChapters(workId: Id): Promise<Chapter[]> {
		const d = await this.gql<{ canonicalChapters: Chapter[] }>(ops.CANONICAL_CHAPTERS, { workId });
		return d.canonicalChapters;
	}
	async canonicalPages(chapterId: Id): Promise<Page[]> {
		const d = await this.gql<{ canonicalPages: Page[] }>(ops.CANONICAL_PAGES, { chapterId });
		return d.canonicalPages;
	}

	// --- native-embedded-Suwayomi source routing ---
	async workSources(workId: Id): Promise<WorkSource[]> {
		const d = await this.gql<{ workSources: WorkSource[] }>(ops.WORK_SOURCES, { workId });
		return d.workSources;
	}
	/**
	 * Whether the API has told us it predates `seriesPage` (P2.1). Process-wide for
	 * the same deploy-order reason as {@link unsupportedFields}: once one request
	 * gets `Unknown field "seriesPage"`, every later call would too — remember it
	 * and answer `undefined` immediately so callers fall back without a wasted
	 * round trip per page view.
	 */
	private static seriesPageUnsupported = false;

	async seriesPage(
		workId: Id,
		withAggregated = false,
	): Promise<SeriesPageBundle | null | undefined> {
		if (GraphQLBackend.seriesPageUnsupported) return undefined;
		try {
			const d = await this.gql<{ seriesPage: SeriesPageBundle }>(ops.SERIES_PAGE, {
				workId,
				withAggregated,
			});
			return d.seriesPage;
		} catch (e) {
			const msg = e instanceof Error ? e.message : String(e);
			// The server predates the resolver — schema-drift fallback, not an outage.
			if (msg.includes('Unknown field "seriesPage"')) {
				GraphQLBackend.seriesPageUnsupported = true;
				return undefined;
			}
			// The resolver's own not-found (unknown id / NSFW-gated): the page 404s,
			// exactly as the old fan-out rendering nothing did. Anything else is a real
			// failure the caller must surface honestly.
			if (msg.includes('No such work')) return null;
			throw e;
		}
	}

	async workSourcesBatch(workIds: Id[]): Promise<WorkSourceGroup[]> {
		const d = await this.gql<{ workSourcesBatch: WorkSourceGroup[] }>(ops.WORK_SOURCES_BATCH, {
			workIds,
		});
		return d.workSourcesBatch;
	}
}

export function createBackend(config: BackendConfig): Backend {
	return new GraphQLBackend(config);
}
