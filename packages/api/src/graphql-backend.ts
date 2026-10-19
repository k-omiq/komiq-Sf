import type {
	ChapterOrigin,
	ChapterWork,
	ProgressResult,
	ViewerProgress,
	MediaRoute,
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
	DuplicateSweepResult,
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
	ExtensionAutoUpdate,
	SplitSourcesResult,
	TagInfo,
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
	CreateOwnerInput,
	CreateUserInput,
	InstanceInfo,
	ExtensionRepo,
	MangadexSetting,
	EngineGeneration,
	EngineStatus,
	SourceEngineRoute,
	BlockRule,
	BrowseSource,
	RatedScanlator,
	RatedScanlatorPage,
	ScanlatorAi,
	ScanlatorPage,
	ScanlatorQuery,
	ScanlatorRatingQuery,
	ScanlatorSite,
	ScanlatorTag,
	SourceAiTag,
	MetadataFieldGroup,
	SourceMetadataBar,
	SetSourceMetadataBarResult,
	WorkMetadataSource,
	SeriesDefaults,
	CoverPruneStatus,
	SourceRateLimit,
	MyComment,
	PostCommentInput,
	PostReviewInput,
	RegisterInput,
	SearchFilters,
	SaveProgressInput,
	SaveChapterReadInput,
	SeriesAdminInput,
	SeriesAdminMeta,
	SeriesMetadataInput,
	Session,
	UpdateProfileInput,
	AccessMode,
	AccessStatus,
	AccessPreflight,
	CertificateStatus,
	CloudflareConnection,
	CloudflareSetupInput,
	CloudflareTokenCheck,
	WorkChapterEdit,
	WorkChapterEntry,
	WorkChapterList,
} from './backend.js';
import * as ops from './operations.js';
import {
	isNumericChapterId,
	isStaleNumericEpoch,
	notifyStaleNumericEpoch,
} from './numeric-epoch.js';
import { notifyProgressConflict } from './progress-conflict.js';
import { NOT_AUTHENTICATED, notifyAuthRejected } from './session-keeper.js';
import { failedOnlyAt, GraphQLRequestError, tolerablePartialErrors } from './partial-errors.js';
import type { GraphQLErrorLike, PathSegment } from './partial-errors.js';
import { persistedId } from './persisted.js';

export interface BackendConfig {
	/** GraphQL endpoint of the unified backend, e.g. https://api.komika.app/graphql */
	endpoint: string;
	/** Bearer token for authenticated requests, if signed in. */
	token?: string | null;
	/** API schema/deploy generation, when known. Capability negatives never cross it. */
	schemaGeneration?: string;
	/** Optional fetch override (SSR / tests). Defaults to global fetch. */
	fetch?: typeof fetch;
	/**
	 * Send a signed-in viewer's catalogue reads ({@link PUBLIC_READS} and the series page)
	 * without the token, as GETs an edge cache can keep, while the viewer's answers are the
	 * anonymous ones (no NSFW opt-in, no block rule of their own). The viewer's own data
	 * then comes from separate POSTs. The reader sets it; the admin console does not.
	 */
	publicReads?: boolean;
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

/** A {@link BlockRule} as the `BlockRuleInput` the API takes: its two lists and nothing
 *  else, so a rule object that carries extra view state is still a valid variable. */
const blockRuleInput = (rule: BlockRule): BlockRule => ({
	tags: rule.tags,
	ratings: rule.ratings,
});

/** Reads that hand out numeric ids. The epoch is read before them, never inside their
 *  (cacheable) documents. */
const NUMERIC_ID_READS = new Set([
	ops.DISCOVERY,
	ops.UPDATES,
	ops.SEARCH,
	ops.SEARCH_ALL_SOURCES,
	ops.SEARCH_FOR_MERGE,
	ops.SERIES,
	ops.CHAPTERS,
	ops.AGGREGATED_CHAPTERS,
	ops.PAGES,
	ops.LIBRARY,
	ops.LIBRARY_PROGRESS,
	ops.CANONICAL_UPDATES,
	ops.UPDATES_FEED,
	ops.CANONICAL_SERIES,
	ops.CANONICAL_CHAPTERS,
	ops.CANONICAL_PAGES,
	ops.SERIES_PAGE,
	ops.SOURCE_BROWSE,
	ops.SERIES_SOURCES_BATCH,
	ops.REVIEWS,
	ops.MY_REVIEW,
	ops.NOTIFICATIONS,
	ops.MY_COMMENTS,
	ops.MY_ACTIVITY,
]);

const LIBRARY_WRITES = new Set([ops.MARK, ops.SET_LIBRARY_STATUS, ops.SET_FAVORITE]);

/**
 * Catalogue reads whose answer depends on the viewer only through the NSFW opt-in and
 * block rules (`viewer_show_nsfw`, `viewer_blocks` on the server). With
 * {@link BackendConfig.publicReads} a signed-in viewer without either sends them anonymous.
 * Their card fields `isMarked`, `libraryStatus` and `isFavorite` then read signed out; the
 * reader shows them only on the series page, which lays its viewer data over the page
 * (`seriesPage`).
 */
const PUBLIC_READS = new Set([
	ops.DISCOVERY,
	ops.UPDATES,
	ops.UPDATES_FEED,
	ops.CANONICAL_UPDATES,
	ops.SEARCH,
	ops.TAGS,
	ops.GENRE_FACETS,
	ops.BROWSE_SOURCES,
	ops.SCANLATOR_TAGS,
	ops.SOURCE_AI_TAGS,
	ops.PAGES,
	ops.CANONICAL_PAGES,
	ops.MEDIA_ROUTES,
]);

/** How long a viewer's posture (see {@link GraphQLBackend.posture}) is kept. */
const POSTURE_TTL_MS = 5 * 60_000;

/**
 * A signed-in viewer's catalogue posture. `anonymous`: their catalogue reads can go
 * without the token. `nsfwPass`: the shared pass those reads carry when the viewer opted
 * in to NSFW (`apps/server/src/nsfw_pass.rs`); null when they did not.
 */
interface Posture {
	anonymous: boolean;
	nsfwPass: string | null;
}
const SIGNED_IN: Posture = { anonymous: false, nsfwPass: null };

/** The server's answer to a pass that no longer verifies (`nsfw_pass::EXPIRED`). */
const NSFW_PASS_EXPIRED = 'NSFW_PASS_EXPIRED';

/**
 * How {@link GraphQLBackend.gql} sends a request. `auto`: a GET when nothing needs the
 * token, else a POST. `post`: always a POST, with the token. `public`: never the token,
 * whether or not the viewer is signed in.
 */
type Transport = 'auto' | 'post' | 'public';

/** Refusal codes of a persisted-document GET (`apps/server/src/persisted.rs`): 400 for an
 *  id the server does not hold, 405 for document text on a server that takes only ids. */
const PERSISTED_NOT_FOUND = 'PERSISTED_QUERY_NOT_FOUND';
const PERSISTED_REQUIRED = 'PERSISTED_QUERY_REQUIRED';

/** The `extensions.code` of an error answer, if it has one. */
async function refusalCode(res: Response): Promise<string | undefined> {
	try {
		const body = (await res.json()) as { errors?: { extensions?: { code?: unknown } }[] };
		const code = body.errors?.[0]?.extensions?.code;
		return typeof code === 'string' ? code : undefined;
	} catch {
		return undefined;
	}
}

/** What `seriesPageViewer` answers: the viewer's part of a `seriesPage`. */
interface SeriesPageViewerData {
	workId: Id;
	viewerProgress?: ViewerProgress | null;
	library: {
		seriesId: Id;
		isMarked: boolean;
		libraryStatus: LibraryStatus | null;
		isFavorite: boolean;
	}[];
	chapters: { sourceType: string; chapterId: Id; read: boolean; lastPageRead: number }[];
	aggregated?: { entryId?: string | null; key: string }[];
}

/**
 * `seriesPageViewer` laid over an anonymous `seriesPage`: the page the viewer would get
 * signed in. The server test `the_viewer_overlay_on_an_anonymous_page_is_the_signed_in_page`
 * applies the same rule.
 */
function layViewerOver(page: SeriesPageBundle, viewer: SeriesPageViewerData): SeriesPageBundle {
	const flags = new Map(viewer.library.map((f) => [f.seriesId, f]));
	const states = new Map(viewer.chapters.map((c) => [`${c.sourceType}:${c.chapterId}`, c]));
	const read = new Set((viewer.aggregated ?? []).map((r) => `${r.entryId ?? ''}|${r.key}`));
	const series = (s: Series | null): Series | null => {
		const f = s && flags.get(s.id);
		return s && f
			? { ...s, isMarked: f.isMarked, libraryStatus: f.libraryStatus, isFavorite: f.isFavorite }
			: s;
	};
	const chapters = (list: Chapter[], sourceType: string): Chapter[] =>
		list.map((c) => {
			const state = states.get(`${sourceType}:${c.id}`);
			return state ? { ...c, read: state.read, lastPageRead: state.lastPageRead } : c;
		});
	return {
		...page,
		viewerProgress: viewer.viewerProgress,
		series: series(page.series),
		spineChapters: chapters(page.spineChapters, 'mangadex'),
		sources: page.sources.map((s) => ({
			...s,
			series: series(s.series),
			chapters: chapters(s.chapters, 'suwayomi'),
		})),
		aggregated: page.aggregated?.map((row) => ({
			...row,
			readAnywhere: read.has(`${row.entryId ?? ''}|${row.key}`),
		})),
	};
}

/**
 * A URL string in its WHATWG-normalized form — what `new URL(s).href` produces.
 *
 * This exists for ONE reason, and it is not cosmetic. SvelteKit's SSR serializes each
 * `load` fetch into the HTML keyed on `new URL(input, event.url).href`, and the hydration
 * re-run looks that key up using the string the caller passed, VERBATIM. Any character the
 * two disagree about means a miss — the response is inlined in the page, the client cannot
 * find it, and it refetches over the network.
 *
 * Our GET query string is built with `encodeURIComponent`, whose "do not encode" set
 * includes `!'()*`; WHATWG's query percent-encode set does not spare `'`. So a document
 * containing an apostrophe — every one of them does, in a comment like "the newest
 * chapter's NUMBER" — is sent with a literal `'` and serialized with `%27`, and the lookup
 * misses on exactly the queries this optimization is for. Normalizing before the request
 * makes both sides agree, and is idempotent, so it costs nothing anywhere else.
 *
 * Falls back to the input if it will not parse (a relative endpoint in some future
 * config): a request that still works uncached beats one that throws.
 */
function canonicalUrl(url: string): string {
	try {
		return new URL(url).href;
	} catch {
		return url;
	}
}

/** Negative schema discovery is shared by fetch views, bounded and retried after upgrades. */
const CAPABILITY_NEGATIVE_TTL_MS = 5 * 60_000;

/** Auth and progress state shared by a backend and its {@link GraphQLBackend.withFetch} views. */
interface AuthCell {
	token: string | null;
	revision: number;
	progressRevisions: Map<Id, number>;
	/** The last revisioned save per work, so the next one starts after it answered. */
	progressSaves?: Map<Id, Promise<unknown>>;
	numericEpoch?: Promise<number | null>;
	staleNumericEpochNotified?: boolean;
	/** Whether the viewer's catalogue answers are the anonymous ones, and until when. */
	posture?: { expiresAt: number; posture: Promise<Posture> };
}

const CAPABILITY_ENDPOINT_CAPACITY = 64;

interface EndpointCapabilities {
	expiresAt: number;
	unsupportedFields: Set<string>;
	unsupportedArgs: Set<string>;
	unsupportedLists: Set<string>;
	getUnsupported: boolean;
	fetchPolicyUnsupported: boolean;
	tagsUnsupported: boolean;
	libraryEpochUnsupported: Set<string>;
	setChapterReadUnsupported: boolean;
	saveProgressUnsupported: boolean;
	viewerProgressUnsupported: boolean;
	chapterWorkUnsupported: boolean;
	workChapterListUnsupported: boolean;
	setSourceMetadataBarUnsupported: boolean;
	chapterOriginUnsupported: boolean;
	mediaRoutesUnsupported: boolean;
	seriesPageUnsupported: boolean;
	seriesPageViewerUnsupported: boolean;
	/** The server predates persisted documents: GETs carry the document text. */
	persistedUnsupported: boolean;
	/** Documents the server does not hold as persisted (another build, or stripped). */
	unknownPersisted: Set<string>;
	/** The server runs only persisted documents over GET (`KOMIKA_PERSISTED_ONLY`). */
	fullTextGetRefused: boolean;
}

const capabilitiesByEndpoint = new Map<string, EndpointCapabilities>();

function endpointCapabilities(config: BackendConfig): EndpointCapabilities {
	const key = JSON.stringify([canonicalUrl(config.endpoint), config.schemaGeneration ?? null]);
	const now = Date.now();
	let capabilities = capabilitiesByEndpoint.get(key);
	if (!capabilities || capabilities.expiresAt <= now) {
		capabilities = {
			expiresAt: now + CAPABILITY_NEGATIVE_TTL_MS,
			unsupportedFields: new Set(),
			unsupportedArgs: new Set(),
			unsupportedLists: new Set(),
			getUnsupported: false,
			fetchPolicyUnsupported: false,
			tagsUnsupported: false,
			libraryEpochUnsupported: new Set(),
			setChapterReadUnsupported: false,
			saveProgressUnsupported: false,
			viewerProgressUnsupported: false,
			chapterWorkUnsupported: false,
			workChapterListUnsupported: false,
			setSourceMetadataBarUnsupported: false,
			chapterOriginUnsupported: false,
			mediaRoutesUnsupported: false,
			seriesPageUnsupported: false,
			seriesPageViewerUnsupported: false,
			persistedUnsupported: false,
			unknownPersisted: new Set(),
			fullTextGetRefused: false,
		};
	}
	capabilitiesByEndpoint.delete(key);
	capabilitiesByEndpoint.set(key, capabilities);
	while (capabilitiesByEndpoint.size > CAPABILITY_ENDPOINT_CAPACITY) {
		capabilitiesByEndpoint.delete(capabilitiesByEndpoint.keys().next().value!);
	}
	return capabilities;
}

/**
 * GraphQL-backed implementation of {@link Backend}, targeting Komika's unified
 * API. Operations are domain-shaped (see `operations.ts` + `schema/komika.graphql`),
 * so responses map 1:1 onto `@komika/types` with no translation layer.
 */
export class GraphQLBackend implements Backend {
	private get capabilities(): EndpointCapabilities {
		return endpointCapabilities(this.config);
	}

	/**
	 * The bearer token, in a cell SHARED with every {@link withFetch} view of this
	 * backend rather than copied into each one.
	 *
	 * A view is created inside a `load` and used across `await`s; a login or a session
	 * restore landing in that window must reach it, and a token it captured at
	 * construction would go stale in exactly the direction that matters — a view still
	 * sending anonymous GETs after the viewer signed in would be served (and could cache)
	 * another viewer's answer.
	 */
	private readonly auth: AuthCell;

	/** `auth` is passed only by {@link withFetch}, to share the parent's cell. */
	constructor(
		private config: BackendConfig,
		auth?: AuthCell,
	) {
		this.auth = auth ?? { token: config.token ?? null, revision: 0, progressRevisions: new Map() };
	}

	withFetch(fetch: typeof globalThis.fetch): Backend {
		return new GraphQLBackend({ ...this.config, fetch }, this.auth);
	}

	setToken(token: string | null): void {
		if (this.auth.token === token) return;
		this.auth.token = token;
		this.auth.revision += 1;
		this.auth.progressRevisions.clear();
		// Saves still waiting fail their auth check; the new account must not wait on them.
		this.auth.progressSaves = undefined;
		this.auth.posture = undefined;
	}

	/**
	 * Whether this request is a catalogue read ({@link PUBLIC_READS}) that goes out without
	 * the token when {@link anonymousPosture} allows. An admin's `includeNsfw: true` is
	 * answered for the admin only, so it keeps the token.
	 */
	private publicCandidate(query: string, variables: Record<string, unknown> | undefined): boolean {
		return !!this.config.publicReads && PUBLIC_READS.has(query) && variables?.includeNsfw !== true;
	}

	/**
	 * Whether the signed-in viewer's catalogue answers are the anonymous ones: no block
	 * rule of their own (site rules apply to everyone), and either no NSFW opt-in or the
	 * shared NSFW pass that stands in for it. Read once per sign-in, again after
	 * {@link POSTURE_TTL_MS} or a change made here; a failed read keeps every read signed
	 * in. A change made on another device applies within the TTL.
	 */
	private posture(authRevision: number): Promise<Posture> {
		const cell = this.auth;
		const now = Date.now();
		if (cell.posture && cell.posture.expiresAt > now) return cell.posture.posture;
		const posture = this.gql<{
			session: { user: { showNsfw: boolean } } | null;
			myBlockRules: BlockRule[];
		}>(ops.VIEWER_POSTURE, undefined, false, undefined, authRevision, 'post').then(
			async (d): Promise<Posture> => {
				const noRules = d.myBlockRules.every(
					(rule) => rule.tags.length === 0 && rule.ratings.length === 0,
				);
				if (!d.session || !noRules) return SIGNED_IN;
				if (!d.session.user.showNsfw) return { anonymous: true, nsfwPass: null };
				// A server that predates the pass answers with an error: stay signed in.
				const pass = await this.gql<{ nsfwPass: string | null }>(
					ops.NSFW_PASS,
					undefined,
					false,
					undefined,
					authRevision,
					'post',
				).then(
					(p) => p.nsfwPass,
					() => null,
				);
				return pass ? { anonymous: true, nsfwPass: pass } : SIGNED_IN;
			},
			() => SIGNED_IN,
		);
		cell.posture = { expiresAt: now + POSTURE_TTL_MS, posture };
		return posture;
	}

	/** {@link posture}'s `anonymous`. */
	private async anonymousPosture(authRevision: number): Promise<boolean> {
		return (await this.posture(authRevision)).anonymous;
	}

	/**
	 * The server's numeric epoch, read before handing out numeric ids and then kept for the
	 * life of this backend (one page load or app start; shared with {@link withFetch}
	 * views). Kept, not refreshed: after an engine change the reader may still hold the
	 * old engine's chapter ids, and a fresh epoch would let their progress land on
	 * whatever chapter reuses the number. Those writes are refused until the app is
	 * reloaded instead. A failed write-side read is forgotten so the next call tries again.
	 */
	numericEpoch(): Promise<number | null> {
		const cell = this.auth;
		if (cell.numericEpoch) return cell.numericEpoch;
		const read = this.gql<{ numericEpoch: number }>(ops.NUMERIC_EPOCH).then(
			(d) => d.numericEpoch,
			(e: unknown) => {
				// The server predates engine generations: nothing to fence.
				if (e instanceof Error && e.message.includes('Unknown field "numericEpoch"')) return null;
				if (cell.numericEpoch === read) cell.numericEpoch = undefined;
				throw e;
			},
		);
		cell.numericEpoch = read;
		return read;
	}

	/** The epoch a write for `chapterId` carries: only numeric ids are fenced. An explicit
	 *  value (a replayed write) is used as given, `null` meaning none. */
	private async epochFor(
		chapterId: Id,
		given: number | null | undefined,
	): Promise<number | undefined> {
		if (!isNumericChapterId(chapterId)) return undefined;
		const epoch = given === undefined ? await this.numericEpoch() : given;
		return epoch ?? undefined;
	}

	private staleProgress(chapterId: Id, live: boolean, err: unknown): void {
		if (!live || !isNumericChapterId(chapterId) || !isStaleNumericEpoch(err)) return;
		if (this.auth.staleNumericEpochNotified) return;
		this.auth.staleNumericEpochNotified = true;
		notifyStaleNumericEpoch();
	}

	private assertAuthRevision(revision: number): void {
		// An ordinary error prevents callers' schema-error catches from retrying an
		// old viewer's operation using the token of the newly signed-in viewer.
		if (this.auth.revision !== revision) {
			throw new Error('Session changed while the request was in flight');
		}
	}

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
		const unsupportedArgs = new Set(this.capabilities.unsupportedArgs);
		if (this.capabilities.libraryEpochUnsupported.has(query)) unsupportedArgs.add('numericEpoch');
		if (this.capabilities.unsupportedFields.size === 0 && unsupportedArgs.size === 0) return query;
		let out = query;
		for (const field of this.capabilities.unsupportedFields) {
			// Nested optional fields have at most two levels of child objects.
			// Remove the balanced complete selection, including formatted multiline
			// children, so a newer reader still works against an older server.
			if (ops.OPTIONAL_NESTED_SERIES_FIELDS.includes(field)) {
				out = out.replace(
					new RegExp(`^[\\t ]*${field}[\\t ]*\\{(?:[^{}]|\\{[^{}]*\\})*\\}[\\t ]*\\r?\\n`, 'gm'),
					'',
				);
				continue;
			}
			out = out.replace(new RegExp(`^[\\t ]*${field}[\\t ]*\\r?\\n`, 'gm'), '');
		}
		for (const arg of unsupportedArgs) {
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
		if (unsupportedArgs.size > 0) out = out.replace(/\(\s*(?:,\s*)*\)/g, '');
		// Emptied a selection set? Only count sets the strip CREATED, so a legitimately
		// empty object value in the source document (`input: {}`) can't disable this.
		const emptySets = (s: string) => (s.match(/\{\s*\}/g) ?? []).length;
		if (emptySets(out) > emptySets(query)) return query;
		return out;
	}

	/**
	 * @param tolerate Response paths this caller can render WITHOUT. A response whose every
	 *   error sits at or under one of them returns its `data` (those fields null) instead of
	 *   throwing. See {@link tolerablePartialErrors}; omit it and any error throws, as before.
	 * @param transport See {@link Transport}.
	 */
	private async gql<T>(
		query: string,
		variables?: Record<string, unknown>,
		retried = false,
		tolerate?: readonly (readonly PathSegment[])[],
		authRevision = this.auth.revision,
		transport: Transport = 'auto',
	): Promise<T> {
		this.assertAuthRevision(authRevision);
		const token = this.auth.token;
		// The epoch numeric writes carry is read before dispatching the first request
		// that hands out numeric ids, never later at the first write. An
		// anonymous viewer cannot write, so it skips the read and keeps cacheable GETs.
		const epochRead =
			token && NUMERIC_ID_READS.has(query)
				? this.numericEpoch().catch((e: unknown) => {
						// These ids must never be paired with a newer generation's epoch. A
						// read for an account that has since signed out pins nothing.
						if (this.auth.revision === authRevision) this.auth.numericEpoch = Promise.resolve(null);
						console.warn('[komika] could not read numeric epoch:', e);
					})
				: undefined;
		if (epochRead) {
			await epochRead;
			this.assertAuthRevision(authRevision);
		}
		const doFetch = this.config.fetch ?? fetch;
		const stripped = this.stripUnsupported(query);
		// Mutations are detected on the document text — `operations.ts` always writes a
		// top-level `mutation Name(`/`query Name(` (fragments may come first, hence the
		// newline alternative).
		const mutation = /(^|\n)\s*mutation[\s(]/.test(stripped);
		// The token goes only where the answer depends on the viewer: a signed-in
		// viewer's catalogue read whose answer is the anonymous one goes without it.
		let sent: string | null = token && transport !== 'public' ? token : null;
		// The shared NSFW pass a viewer's anonymous catalogue read carries (see `posture`).
		let nsfwPass: string | null = null;
		if (sent && transport === 'auto' && !mutation && this.publicCandidate(query, variables)) {
			const posture = await this.posture(authRevision);
			this.assertAuthRevision(authRevision);
			if (posture.anonymous) {
				sent = null;
				nsfwPass = posture.nsfwPass;
			}
		} else if (token && transport === 'public' && !mutation) {
			// `public` is chosen only after the posture allowed it, so this is a cache read.
			nsfwPass = (await this.posture(authRevision)).nsfwPass;
			this.assertAuthRevision(authRevision);
		}
		// The pass rides only on a persisted GET. Where that is not possible, the read keeps
		// the token instead, so it still sees what this viewer opted in to.
		const persistable =
			!this.capabilities.getUnsupported &&
			!this.capabilities.persistedUnsupported &&
			!this.capabilities.unknownPersisted.has(stripped);
		if (nsfwPass && !persistable) {
			sent = token;
			nsfwPass = null;
		}
		// Anonymous READS go over GET so Cloudflare and the browser's HTTP cache can
		// serve them (the server sends Cache-Control + ETag on anonymous GETs; 304
		// revalidation is the browser's job, no client code needed). A request with the
		// token must NEVER take this path: edge caches key on the URL alone, so a cached
		// response carrying viewer-specific fields (isMarked, myReview…) would be
		// replayed to other users.
		const useGet = !sent && transport !== 'post' && !this.capabilities.getUnsupported && !mutation;
		// The id of the document the server holds as persisted (`persisted.ts`): the URL
		// stays short, and the edge's cache keys are the server's fixed set of documents.
		const persisted =
			useGet &&
			!this.capabilities.persistedUnsupported &&
			!this.capabilities.unknownPersisted.has(stripped)
				? persistedId(stripped)
				: null;
		let res: Response | undefined;
		if (useGet && (persisted || !this.capabilities.fullTextGetRefused)) {
			// Single-operation documents, so no `operationName`; `variables` only when
			// there are any. No Authorization header by construction.
			// The server redirects any other spelling, so this order and encoding are fixed:
			// `id`, `variables`, `nsfw` (`apps/server/src/persisted.rs`).
			const params =
				(persisted ? `id=${persisted}` : `query=${encodeURIComponent(stripped)}`) +
				(variables ? `&variables=${encodeURIComponent(JSON.stringify(variables))}` : '') +
				(persisted && nsfwPass ? `&nsfw=${nsfwPass}` : '');
			res = await doFetch(canonicalUrl(`${this.config.endpoint}?${params}`));
			if (res.status === 405 && (await refusalCode(res.clone())) === PERSISTED_REQUIRED) {
				// The server runs only persisted documents over GET, and this one it does not
				// hold: re-enter, which lands on POST for every document without an id.
				this.assertAuthRevision(authRevision);
				this.capabilities.fullTextGetRefused = true;
				return this.gql<T>(query, variables, retried, tolerate, authRevision, transport);
			}
			if (res.status === 404 || res.status === 405) {
				this.assertAuthRevision(authRevision);
				// The server predates GET /graphql — schema-drift fallback, not an outage.
				// Memoize and re-enter: the retry re-evaluates the transport and lands on
				// POST, preserving `retried` so the unknown-field loop is unaffected. Any
				// OTHER status is NOT memoized — a 500 or a network failure must throw
				// exactly as it does on POST. Force this retry to POST even if the slow
				// rejection outlived the capability capsule's TTL.
				this.capabilities.getUnsupported = true;
				return this.gql<T>(query, variables, retried, tolerate, authRevision, 'post');
			}
			if (res.status === 400 && persisted) {
				this.assertAuthRevision(authRevision);
				// The server does not hold this document (another build, or a stripped
				// retry) and ran nothing: re-enter, which sends the text.
				if ((await refusalCode(res)) === PERSISTED_NOT_FOUND) {
					this.capabilities.unknownPersisted.add(stripped);
					return this.gql<T>(query, variables, retried, tolerate, authRevision, transport);
				}
				throw new Error('Backend error 400');
			}
		} else {
			res = await doFetch(this.config.endpoint, {
				method: 'POST',
				headers: {
					'content-type': 'application/json',
					...(sent ? { authorization: `Bearer ${sent}` } : {}),
				},
				body: JSON.stringify({ query: stripped, variables }),
			});
		}
		if (!res.ok) throw new Error(`Backend error ${res.status}`);
		const json = (await res.json()) as { data?: T; errors?: GraphQLErrorLike[] };
		// The pass expired (it changes every UTC day): forget the posture, so the next
		// read fetches a fresh one, and answer this read signed in.
		if (nsfwPass && json.errors?.some((e) => e.extensions?.code === NSFW_PASS_EXPIRED)) {
			this.assertAuthRevision(authRevision);
			this.auth.posture = undefined;
			return this.gql<T>(query, variables, retried, tolerate, authRevision, 'post');
		}
		// A server that predates persisted documents parses the missing `query` as an empty
		// document (`persisted.rs` keeps the words in a test). Send the text from now on.
		if (persisted && json.errors?.some((e) => e.message.includes('executable_definition'))) {
			this.assertAuthRevision(authRevision);
			this.capabilities.persistedUnsupported = true;
			return this.gql<T>(query, variables, retried, tolerate, authRevision, transport);
		}
		// The token this request carried no longer signs anyone in. Let the auth layer
		// ask the server, which then signs this client out cleanly.
		if (
			sent &&
			this.auth.revision === authRevision &&
			json.errors?.some((e) => e.message === NOT_AUTHENTICATED)
		)
			notifyAuthRejected();
		if (json.errors?.length) this.assertAuthRevision(authRevision);
		if (tolerablePartialErrors(json.data, json.errors, tolerate)) {
			// A partial result the caller opted into: the named fields came back null and
			// everything else is real. Logged, never silent — the null is a failure, not data.
			console.warn(
				'[komika] partial GraphQL result:',
				json.errors!.map((e) => e.message),
			);
			return json.data as T;
		}
		if (json.errors?.length) {
			const messages = json.errors.map((e) => e.message);
			// An unknown field fails the WHOLE document, so a server that predates an
			// opportunistic field would otherwise take down every screen. Remember it,
			// strip it, retry once — the mapped type declares those fields optional and
			// callers already fall back.
			// An unknown ARGUMENT (`includeNsfw`) is a different validation error but the
			// same failure mode, so it gets the same treatment. Both are collected in one
			// pass: validation reports every error at once, and there is only one retry.
			// Argument-bearing queries need their own fallback because only plain nested
			// selections can be removed without emptying the document.
			if (!retried) {
				const dropped = ops.OPTIONAL_SERIES_FIELDS.filter(
					(f) =>
						!this.capabilities.unsupportedFields.has(f) &&
						(!ops.OPTIONAL_NESTED_SERIES_FIELDS.includes(f) ||
							new RegExp(`^[\\t ]*${f}[\\t ]*\\{`, 'm').test(query)) &&
						messages.some((m) => m.includes(`Unknown field "${f}"`)),
				);
				const droppedArgs = ops.OPTIONAL_ARGUMENTS.filter(
					(a) =>
						(a !== 'numericEpoch' || !this.capabilities.libraryEpochUnsupported.has(query)) &&
						!this.capabilities.unsupportedArgs.has(a) &&
						messages.some((m) => m.includes(`Unknown argument "${a}"`)),
				);
				if (dropped.length || droppedArgs.length) {
					for (const f of dropped) this.capabilities.unsupportedFields.add(f);
					for (const a of droppedArgs) {
						// Library arguments arrived after progress epochs. An older
						// library mutation must not disable the progress fence.
						if (a === 'numericEpoch' && LIBRARY_WRITES.has(query)) {
							this.capabilities.libraryEpochUnsupported.add(query);
						} else {
							this.capabilities.unsupportedArgs.add(a);
						}
					}
					return this.gql<T>(query, variables, true, tolerate, authRevision, transport);
				}
			}
			throw new GraphQLRequestError(json.errors);
		}
		if (json.data == null) throw new Error('Backend returned no data');
		return json.data;
	}

	// --- auth ---
	async session(): Promise<Session | null> {
		const d = await this.gql<{ session: Session | null }>(ops.SESSION);
		return d.session;
	}
	async login(username: string, password: string, turnstileToken?: string): Promise<Session> {
		const d = turnstileToken
			? await this.gql<{ login: Session }>(ops.LOGIN_TURNSTILE, {
					username,
					password,
					turnstileToken,
				})
			: await this.gql<{ login: Session }>(ops.LOGIN, { username, password });
		return d.login;
	}
	async register(input: RegisterInput): Promise<Session> {
		const d = await this.gql<{ register: Session }>(ops.REGISTER, { input });
		return d.register;
	}
	async logout(): Promise<void> {
		await this.gql<{ logout: boolean }>(ops.LOGOUT);
	}
	/** Endpoint/schema-scoped, like {@link seriesPageUnsupported}: one `Unknown field` answer means
	 *  every later call would get it too. */
	async instance(): Promise<InstanceInfo> {
		const authRevision = this.auth.revision;
		try {
			const d = await this.gql<{ instance: InstanceInfo }>(
				ops.instanceQuery(!this.capabilities.fetchPolicyUnsupported),
			);
			return d.instance;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			const msg = e instanceof Error ? e.message : String(e);
			// The server predates the field — deploy-order drift, not a server to refuse.
			if (
				!this.capabilities.fetchPolicyUnsupported &&
				msg.includes('Unknown field "appsFetchThemselves"')
			) {
				this.capabilities.fetchPolicyUnsupported = true;
				const d = await this.gql<{ instance: InstanceInfo }>(ops.instanceQuery(false));
				return d.instance;
			}
			throw e;
		}
	}
	async turnstileSiteKey(): Promise<string | null> {
		const authRevision = this.auth.revision;
		try {
			const d = await this.gql<{ instance: { turnstileSiteKey?: string | null } }>(
				ops.TURNSTILE_SITE_KEY,
			);
			return d.instance.turnstileSiteKey ?? null;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			// A server that predates Turnstile asks for no check.
			if (e instanceof Error && e.message.includes('Unknown field "turnstileSiteKey"')) return null;
			throw e;
		}
	}
	async createOwner(input: CreateOwnerInput): Promise<Session> {
		const d = await this.gql<{ createOwner: Session }>(ops.CREATE_OWNER, { ...input });
		return d.createOwner;
	}
	async changePassword(
		currentPassword: string,
		newPassword: string,
		turnstileToken?: string,
	): Promise<Session['user']> {
		const d = await this.gql<{ changePassword: Session['user'] }>(
			turnstileToken ? ops.CHANGE_PASSWORD_TURNSTILE : ops.CHANGE_PASSWORD,
			turnstileToken
				? { currentPassword, newPassword, turnstileToken }
				: { currentPassword, newPassword },
		);
		return d.changePassword;
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
			// Browse v2. OPTIONAL_ARGUMENTS: stripped (with its `$filter` declaration) once a
			// server rejects it, leaving the legacy arguments above to carry what they can.
			filter: filters?.filter,
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
	/**
	 * Set once the API answers `Unknown field "tags"`, so a server that predates Browse
	 * v2 costs one rejected request per endpoint/schema window rather than one per Browse visit. Same
	 * lifetime and rationale as {@link unsupportedFields}.
	 */
	async tags(): Promise<TagInfo[]> {
		const authRevision = this.auth.revision;
		if (this.capabilities.tagsUnsupported) return [];
		try {
			const d = await this.gql<{ tags: TagInfo[] }>(ops.TAGS);
			return d.tags;
		} catch (err) {
			this.assertAuthRevision(authRevision);
			// ONLY the deploy-order case degrades to []: any other failure (outage, 5xx)
			// must reach the caller as a failure, exactly like every other method here.
			const msg = err instanceof Error ? err.message : String(err);
			if (!msg.includes('Unknown field "tags"')) throw err;
			this.capabilities.tagsUnsupported = true;
			return [];
		}
	}
	/**
	 * Root fields a server that predates them rejected (`browseSources`,
	 * `scanlatorTags`, `sourceAiTags`). Same deploy-order handling as {@link tagsUnsupported}: one
	 * rejected request per endpoint/schema window, then `[]` without asking.
	 */
	private async optionalList<T>(
		field: string,
		query: string,
		variables?: Record<string, unknown>,
	): Promise<T[]> {
		const authRevision = this.auth.revision;
		if (this.capabilities.unsupportedLists.has(field)) return [];
		try {
			const d = await this.gql<Record<string, T[]>>(query, variables);
			return d[field];
		} catch (err) {
			this.assertAuthRevision(authRevision);
			const msg = err instanceof Error ? err.message : String(err);
			if (!msg.includes(`Unknown field "${field}"`)) throw err;
			this.capabilities.unsupportedLists.add(field);
			return [];
		}
	}
	browseSources(includeNsfw?: boolean): Promise<BrowseSource[]> {
		return this.optionalList<BrowseSource>('browseSources', ops.BROWSE_SOURCES, {
			includeNsfw: includeNsfw ?? null,
		});
	}
	scanlatorTags(): Promise<ScanlatorTag[]> {
		return this.optionalList<ScanlatorTag>('scanlatorTags', ops.SCANLATOR_TAGS);
	}
	sourceAiTags(): Promise<SourceAiTag[]> {
		return this.optionalList<SourceAiTag>('sourceAiTags', ops.SOURCE_AI_TAGS);
	}
	sourceMetadataBars(): Promise<SourceMetadataBar[]> {
		return this.optionalList<SourceMetadataBar>('sourceMetadataBars', ops.SOURCE_METADATA_BARS);
	}
	workMetadataSources(workId: Id): Promise<WorkMetadataSource[]> {
		return this.optionalList<WorkMetadataSource>('workMetadataSources', ops.WORK_METADATA_SOURCES, {
			workId,
		});
	}
	metadataBarsSupported(): boolean {
		return (
			!this.capabilities.unsupportedLists.has('sourceMetadataBars') &&
			!this.capabilities.setSourceMetadataBarUnsupported
		);
	}
	browseFilterSupported(): boolean {
		return !this.capabilities.unsupportedArgs.has('filter');
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
	async pages(chapterId: Id, opts?: { fresh?: boolean }): Promise<Page[]> {
		const d = opts?.fresh
			? await this.gql<{ pages: Page[] }>(
					ops.PAGES,
					{ chapterId },
					false,
					undefined,
					this.auth.revision,
					'post',
				)
			: await this.gql<{ pages: Page[] }>(ops.PAGES, { chapterId });
		return d.pages;
	}

	// --- library & progress ---
	private async editLibrary(
		field: 'mark' | 'setLibraryStatus' | 'setFavorite',
		query: string,
		seriesId: Id,
		variables: Record<string, unknown>,
		numericEpoch?: number | null,
	): Promise<Series> {
		const authRevision = this.auth.revision;
		const epoch = isNumericChapterId(seriesId)
			? await this.epochFor(seriesId, numericEpoch)
			: undefined;
		try {
			const d = await this.gql<Record<typeof field, Series>>(
				query,
				{ ...variables, seriesId, numericEpoch: epoch },
				false,
				undefined,
				authRevision,
			);
			return d[field];
		} catch (e) {
			this.assertAuthRevision(authRevision);
			this.staleProgress(seriesId, numericEpoch === undefined, e);
			throw e;
		}
	}
	mark(seriesId: Id, marked: boolean, numericEpoch?: number | null): Promise<Series> {
		return this.editLibrary('mark', ops.MARK, seriesId, { marked }, numericEpoch);
	}
	setLibraryStatus(
		seriesId: Id,
		status: LibraryStatus | null,
		numericEpoch?: number | null,
	): Promise<Series> {
		return this.editLibrary(
			'setLibraryStatus',
			ops.SET_LIBRARY_STATUS,
			seriesId,
			{ status },
			numericEpoch,
		);
	}
	setFavorite(seriesId: Id, favorite: boolean, numericEpoch?: number | null): Promise<Series> {
		return this.editLibrary('setFavorite', ops.SET_FAVORITE, seriesId, { favorite }, numericEpoch);
	}
	async library(): Promise<Series[]> {
		const d = await this.gql<{ library: Series[] }>(ops.LIBRARY);
		return d.library;
	}
	async libraryProgress(): Promise<SeriesProgress[]> {
		const d = await this.gql<{ libraryProgress: SeriesProgress[] }>(ops.LIBRARY_PROGRESS);
		return d.libraryProgress;
	}
	async setProgress(
		chapterId: Id,
		lastPageRead: number,
		read: boolean,
		numericEpoch?: number | null,
	): Promise<void> {
		const authRevision = this.auth.revision;
		const epoch = await this.epochFor(chapterId, numericEpoch);
		try {
			await this.gql<{ setProgress: boolean }>(
				ops.SET_PROGRESS,
				{ chapterId, lastPageRead, read, numericEpoch: epoch },
				false,
				undefined,
				authRevision,
			);
		} catch (e) {
			this.staleProgress(chapterId, numericEpoch === undefined, e);
			throw e;
		}
	}
	/** Endpoint/schema-scoped, like {@link seriesPageUnsupported}: one `Unknown field` answer means
	 *  every later call would get it too. */
	async setChapterRead(chapterId: Id, read: boolean): Promise<boolean> {
		const authRevision = this.auth.revision;
		if (this.capabilities.setChapterReadUnsupported) return false;
		try {
			const numericEpoch = await this.epochFor(chapterId, undefined);
			await this.gql<{ setChapterRead: boolean }>(
				ops.SET_CHAPTER_READ,
				{ chapterId, read, numericEpoch },
				false,
				undefined,
				authRevision,
			);
			return true;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			this.staleProgress(chapterId, true, e);
			const msg = e instanceof Error ? e.message : String(e);
			// The API predates the mutation — deploy-order drift, not a failure to report.
			if (msg.includes('Unknown field "setChapterRead"')) {
				this.capabilities.setChapterReadUnsupported = true;
				return false;
			}
			throw e;
		}
	}

	knownRevision(workId: Id): number | undefined {
		return this.auth.progressRevisions.get(workId);
	}

	private recordRevision(workId: Id, revision: number): void {
		this.auth.progressRevisions.set(workId, Math.max(this.knownRevision(workId) ?? 0, revision));
	}

	async viewerProgress(workId: Id): Promise<ViewerProgress | null | undefined> {
		const authRevision = this.auth.revision;
		if (this.capabilities.viewerProgressUnsupported) return undefined;
		try {
			const d = await this.gql<{ viewerProgress: ViewerProgress | null }>(ops.VIEWER_PROGRESS, {
				workId,
			});
			this.assertAuthRevision(authRevision);
			if (d.viewerProgress) this.recordRevision(workId, d.viewerProgress.revision);
			return d.viewerProgress;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			if (e instanceof Error && e.message.includes('Unknown field "viewerProgress"')) {
				this.capabilities.viewerProgressUnsupported = true;
				return undefined;
			}
			throw e;
		}
	}

	saveProgress(input: SaveProgressInput): Promise<ProgressResult | null> {
		return this.saveRevisionedProgress('saveProgress', input);
	}

	saveChapterRead(input: SaveChapterReadInput): Promise<ProgressResult | null> {
		return this.saveRevisionedProgress('saveChapterRead', input);
	}

	private async saveLegacyProgress(
		field: 'saveProgress' | 'saveChapterRead',
		input: SaveProgressInput | SaveChapterReadInput,
	): Promise<null> {
		if (field === 'saveProgress' && 'lastPageRead' in input) {
			await this.setProgress(input.chapterId, input.lastPageRead, input.read, input.numericEpoch);
		} else if (!(await this.setChapterRead(input.chapterId, input.read))) {
			throw new Error('Chapter read state is unavailable on this server.');
		}
		return null;
	}

	/**
	 * Saves of one work go out one at a time. The reader fires overlapping saves (the
	 * end-of-chapter finish and the save on leaving the chapter), and both would carry the
	 * same base revision: the first is applied and moves the revision, so the second would
	 * be refused as stale and the page refreshed for nothing. Waiting lets the second one
	 * send the revision the first answer returned.
	 */
	private saveRevisionedProgress(
		field: 'saveProgress' | 'saveChapterRead',
		input: SaveProgressInput | SaveChapterReadInput,
	): Promise<ProgressResult | null> {
		// Taken now: a save waiting behind another must never go out under a later sign-in.
		const authRevision = this.auth.revision;
		const saves = (this.auth.progressSaves ??= new Map());
		const previous = saves.get(input.workId) ?? Promise.resolve();
		const next = previous
			.catch(() => {})
			.then(() => this.sendRevisionedProgress(field, input, authRevision));
		saves.set(input.workId, next);
		const clear = () => {
			if (saves.get(input.workId) === next) saves.delete(input.workId);
		};
		next.then(clear, clear);
		return next;
	}

	private async sendRevisionedProgress(
		field: 'saveProgress' | 'saveChapterRead',
		input: SaveProgressInput | SaveChapterReadInput,
		authRevision: number,
	): Promise<ProgressResult | null> {
		this.assertAuthRevision(authRevision);
		if (this.capabilities.saveProgressUnsupported) return this.saveLegacyProgress(field, input);
		const revision =
			input.revision ??
			this.knownRevision(input.workId) ??
			(await this.viewerProgress(input.workId))?.revision ??
			0;
		this.assertAuthRevision(authRevision);
		const numericEpoch = await this.epochFor(input.chapterId, input.numericEpoch);
		try {
			const d = await this.gql<Record<string, ProgressResult>>(
				field === 'saveProgress' ? ops.SAVE_PROGRESS : ops.SAVE_CHAPTER_READ,
				{ ...input, numericEpoch, revision },
				false,
				undefined,
				authRevision,
			);
			this.assertAuthRevision(authRevision);
			const result = d[field];
			if (result.status === 'APPLIED') {
				this.recordRevision(input.workId, result.revision);
				if (result.workId != null) this.recordRevision(result.workId, result.revision);
			} else if (result.status === 'REFUSED' || result.status === 'MOVED') {
				// The next save reads the revision again, after the refresh the conflict starts.
				this.auth.progressRevisions.delete(input.workId);
				notifyProgressConflict({
					status: result.status,
					workId: input.workId,
					movedTo: result.workId,
					chapterId: input.chapterId,
				});
			}
			return result;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			this.staleProgress(input.chapterId, input.numericEpoch === undefined, e);
			if (
				e instanceof Error &&
				(e.message.includes('Unknown field "saveProgress"') ||
					e.message.includes('Unknown field "saveChapterRead"'))
			) {
				this.capabilities.saveProgressUnsupported = true;
				return this.saveLegacyProgress(field, input);
			}
			throw e;
		}
	}

	async recordView(seriesId: Id): Promise<void> {
		await this.gql<{ recordView: boolean }>(ops.RECORD_VIEW, { seriesId });
	}

	// --- social ---
	async reviews(seriesId: Id, page = 1): Promise<Paginated<Review>> {
		const d = await this.gql<{ reviews: Paginated<Review> }>(ops.REVIEWS, {
			seriesId,
			page: pageArg(page),
		});
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
			headers: this.auth.token ? { authorization: `Bearer ${this.auth.token}` } : {},
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
		const d = await this.gql<{ notifications: Notification[] }>(ops.NOTIFICATIONS, {
			page: pageArg(page),
		});
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

	async consolidateExactDuplicates(limit?: number): Promise<DuplicateSweepResult> {
		const d = await this.gql<{ consolidateExactDuplicates: DuplicateSweepResult }>(
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

	async workChapterList(workId: Id): Promise<WorkChapterList | undefined> {
		const authRevision = this.auth.revision;
		if (this.capabilities.workChapterListUnsupported) return undefined;
		try {
			const d = await this.gql<{ workChapterList: WorkChapterList }>(ops.WORK_CHAPTER_LIST, {
				workId,
			});
			return d.workChapterList;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			const msg = e instanceof Error ? e.message : String(e);
			// The server predates chapter lists, so the editor can use the old aggregates.
			if (msg.includes('Unknown field "workChapterList"')) {
				this.capabilities.workChapterListUnsupported = true;
				return undefined;
			}
			throw e;
		}
	}

	async setWorkChapter(entryId: Id, edit: WorkChapterEdit): Promise<WorkChapterEntry> {
		const variables: Record<string, unknown> = { entryId };
		for (const field of ['hidden', 'titleOverride', 'clearTitle', 'special', 'sortKey'] as const) {
			if (edit[field] !== undefined) variables[field] = edit[field];
		}
		const d = await this.gql<{ setWorkChapter: WorkChapterEntry }>(ops.SET_WORK_CHAPTER, variables);
		return d.setWorkChapter;
	}

	async linkChapterCopy(chapterId: Id, entryId: Id | null): Promise<boolean> {
		const d = await this.gql<{ linkChapterCopy: boolean }>(ops.LINK_CHAPTER_COPY, {
			chapterId,
			entryId,
		});
		return d.linkChapterCopy;
	}

	async mergeWorkChapters(keepId: Id, dropId: Id): Promise<boolean> {
		const d = await this.gql<{ mergeWorkChapters: boolean }>(ops.MERGE_WORK_CHAPTERS, {
			keepId,
			dropId,
		});
		return d.mergeWorkChapters;
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

	async createUser(input: CreateUserInput): Promise<AdminUser> {
		const d = await this.gql<{ createUser: AdminUser }>(ops.CREATE_USER, { input });
		return d.createUser;
	}

	async resetUserPassword(userId: Id, password: string, turnstileToken?: string): Promise<boolean> {
		const d = await this.gql<{ resetUserPassword: boolean }>(
			turnstileToken ? ops.RESET_USER_PASSWORD_TURNSTILE : ops.RESET_USER_PASSWORD,
			turnstileToken ? { userId, password, turnstileToken } : { userId, password },
		);
		return d.resetUserPassword;
	}

	async mangadexSetting(): Promise<MangadexSetting> {
		const d = await this.gql<{ mangadexSetting: MangadexSetting }>(ops.MANGADEX_SETTING);
		return d.mangadexSetting;
	}

	async engineGenerationState(): Promise<EngineGeneration[]> {
		const d = await this.gql<{ engineGenerationState: EngineGeneration[] }>(
			ops.ENGINE_GENERATION_STATE,
		);
		return d.engineGenerationState;
	}

	private async optionalEngineField<T>(
		field: string,
		query: string,
		fallback: T,
		variables?: Record<string, unknown>,
	): Promise<T> {
		const authRevision = this.auth.revision;
		if (this.capabilities.unsupportedLists.has(field)) return fallback;
		try {
			const d = await this.gql<Record<string, T>>(query, variables);
			return d[field];
		} catch (err) {
			this.assertAuthRevision(authRevision);
			const msg = err instanceof Error ? err.message : String(err);
			if (
				!msg.includes(`Unknown field "${field}"`) &&
				!msg.includes(`Cannot query field "${field}"`)
			) {
				throw err;
			}
			this.capabilities.unsupportedLists.add(field);
			return fallback;
		}
	}

	engines(): Promise<EngineStatus[]> {
		return this.optionalEngineField<EngineStatus[]>('engines', ops.ENGINES, []);
	}

	sourceEngineRoute(sourceId: string): Promise<SourceEngineRoute | null> {
		return this.optionalEngineField<SourceEngineRoute | null>(
			'sourceEngineRoute',
			ops.SOURCE_ENGINE_ROUTE,
			null,
			{ sourceId },
		);
	}

	setSourceEngine(
		sourceId: string,
		engineId: string | null,
		strict = false,
	): Promise<SourceEngineRoute | null> {
		return this.optionalEngineField<SourceEngineRoute | null>(
			'setSourceEngine',
			ops.SET_SOURCE_ENGINE,
			null,
			{ sourceId, engineId, strict },
		);
	}

	async startNewEngineGeneration(generationId: Id): Promise<EngineGeneration[]> {
		const d = await this.gql<{ startNewEngineGeneration: EngineGeneration[] }>(
			ops.START_NEW_ENGINE_GENERATION,
			{ generationId },
		);
		return d.startNewEngineGeneration;
	}

	async setMangadexEnabled(enabled: boolean): Promise<boolean> {
		const d = await this.gql<{ setMangadexEnabled: boolean }>(ops.SET_MANGADEX_ENABLED, {
			enabled,
		});
		return d.setMangadexEnabled;
	}

	async sourceRateLimits(): Promise<SourceRateLimit[]> {
		const d = await this.gql<{ sourceRateLimits: SourceRateLimit[] }>(ops.SOURCE_RATE_LIMITS);
		return d.sourceRateLimits;
	}

	async setSourceRateLimit(
		sourceId: Id,
		requests: number,
		windowSecs: number,
	): Promise<SourceRateLimit> {
		const d = await this.gql<{ setSourceRateLimit: SourceRateLimit }>(ops.SET_SOURCE_RATE_LIMIT, {
			sourceId,
			requests,
			windowSecs,
		});
		return d.setSourceRateLimit;
	}

	async removeSourceRateLimit(sourceId: Id): Promise<boolean> {
		const d = await this.gql<{ removeSourceRateLimit: boolean }>(ops.REMOVE_SOURCE_RATE_LIMIT, {
			sourceId,
		});
		return d.removeSourceRateLimit;
	}

	async scanlators(query: ScanlatorQuery = {}): Promise<ScanlatorPage> {
		const d = await this.gql<{ scanlators: ScanlatorPage }>(ops.SCANLATORS, {
			search: query.search?.trim() || null,
			ai: query.ai ?? null,
			source: query.source ?? null,
			page: pageArg(query.page),
		});
		return d.scanlators;
	}

	async scanlatorRatings(query: ScanlatorRatingQuery = {}): Promise<RatedScanlatorPage> {
		const d = await this.gql<{ scanlatorRatings: RatedScanlatorPage }>(ops.SCANLATOR_RATINGS, {
			search: query.search?.trim() || null,
			kind: query.kind ?? null,
			mine: query.mine ?? null,
			sort: query.sort ?? null,
			page: pageArg(query.page),
		});
		return d.scanlatorRatings;
	}

	async ratedScanlator(id: Id): Promise<RatedScanlator | null> {
		const d = await this.gql<{ ratedScanlator: RatedScanlator | null }>(ops.RATED_SCANLATOR, {
			id,
		});
		return d.ratedScanlator;
	}

	async rateScanlator(id: Id, score: number | null): Promise<RatedScanlator> {
		const d = await this.gql<{ rateScanlator: RatedScanlator }>(ops.RATE_SCANLATOR, {
			id,
			score,
		});
		return d.rateScanlator;
	}

	async setScanlatorAi(name: string, ai: ScanlatorAi | null): Promise<boolean> {
		const d = await this.gql<{ setScanlatorAi: boolean }>(ops.SET_SCANLATOR_AI, { name, ai });
		return d.setScanlatorAi;
	}

	async scanlatorSites(): Promise<ScanlatorSite[]> {
		const d = await this.gql<{ scanlatorSites: ScanlatorSite[] }>(ops.SCANLATOR_SITES);
		return d.scanlatorSites;
	}

	async setSourceAi(sourceId: string, ai: ScanlatorAi | null): Promise<boolean> {
		const d = await this.gql<{ setSourceAi: boolean }>(ops.SET_SOURCE_AI, { sourceId, ai });
		return d.setSourceAi;
	}

	async setSourceMetadataBar(
		sourceId: string,
		fieldGroup: MetadataFieldGroup,
		barred: boolean,
	): Promise<SetSourceMetadataBarResult> {
		const authRevision = this.auth.revision;
		if (!this.metadataBarsSupported())
			throw new Error('This server does not support metadata bars');
		try {
			const d = await this.gql<{ setSourceMetadataBar: SetSourceMetadataBarResult }>(
				ops.SET_SOURCE_METADATA_BAR,
				{ sourceId, fieldGroup, barred },
			);
			return d.setSourceMetadataBar;
		} catch (err) {
			this.assertAuthRevision(authRevision);
			const msg = err instanceof Error ? err.message : String(err);
			if (!msg.includes('Unknown field "setSourceMetadataBar"')) throw err;
			this.capabilities.setSourceMetadataBarUnsupported = true;
			throw new Error('This server does not support metadata bars');
		}
	}

	async seriesDefaults(workId: Id): Promise<SeriesDefaults> {
		const d = await this.gql<{ seriesDefaults: SeriesDefaults }>(ops.SERIES_DEFAULTS, { workId });
		return d.seriesDefaults;
	}

	async setSeriesDefaultCover(workId: Id, key: string | null): Promise<SeriesDefaults> {
		const d = await this.gql<{ setSeriesDefaultCover: SeriesDefaults }>(
			ops.SET_SERIES_DEFAULT_COVER,
			{ workId, key },
		);
		return d.setSeriesDefaultCover;
	}

	async coverSourceRank(): Promise<string[]> {
		const d = await this.gql<{ coverSourceRank: string[] }>(ops.COVER_SOURCE_RANK);
		return d.coverSourceRank;
	}

	async setCoverSourceRank(sourceIds: string[]): Promise<string[]> {
		const d = await this.gql<{ setCoverSourceRank: string[] }>(ops.SET_COVER_SOURCE_RANK, {
			sourceIds,
		});
		return d.setCoverSourceRank;
	}

	async coverPrune(): Promise<CoverPruneStatus> {
		const d = await this.gql<{ coverPrune: CoverPruneStatus }>(ops.COVER_PRUNE);
		return d.coverPrune;
	}

	async setCoverPrune(enabled: boolean): Promise<CoverPruneStatus> {
		const d = await this.gql<{ setCoverPrune: CoverPruneStatus }>(ops.SET_COVER_PRUNE, {
			enabled,
		});
		return d.setCoverPrune;
	}

	async siteBlockRules(): Promise<BlockRule[]> {
		const d = await this.gql<{ siteBlockRules: BlockRule[] }>(ops.SITE_BLOCK_RULES);
		return d.siteBlockRules;
	}

	async setSiteBlockRules(rules: BlockRule[]): Promise<BlockRule[]> {
		const d = await this.gql<{ setSiteBlockRules: BlockRule[] }>(ops.SET_SITE_BLOCK_RULES, {
			rules: rules.map(blockRuleInput),
		});
		return d.setSiteBlockRules;
	}

	async setRegistrationOpen(open: boolean): Promise<boolean> {
		const d = await this.gql<{ setRegistrationOpen: boolean }>(ops.SET_REGISTRATION_OPEN, { open });
		return d.setRegistrationOpen;
	}

	async setShowMangadexRatings(shown: boolean): Promise<boolean> {
		const d = await this.gql<{ setShowMangadexRatings: boolean }>(ops.SET_SHOW_MANGADEX_RATINGS, {
			shown,
		});
		return d.setShowMangadexRatings;
	}

	// --- admin access + Cloudflare setup ---
	async accessStatus(): Promise<AccessStatus> {
		const d = await this.gql<{ accessStatus: AccessStatus }>(ops.ACCESS_STATUS);
		return d.accessStatus;
	}

	async lanAddresses(): Promise<string[]> {
		const d = await this.gql<{ accessStatus: { lanAddresses: string[] } }>(ops.LAN_ADDRESSES);
		return d.accessStatus.lanAddresses;
	}

	async setAccessMode(
		mode: AccessMode,
		domain?: string | null,
		encrypt?: boolean,
	): Promise<AccessStatus> {
		const d = await this.gql<{ setAccessMode: AccessStatus }>(
			encrypt ? ops.SET_ACCESS_MODE_ENCRYPTED : ops.SET_ACCESS_MODE,
			{ mode, domain: domain ?? null },
		);
		return d.setAccessMode;
	}

	async setupNeedsTerminal(): Promise<boolean> {
		const d = await this.gql<{ instance: { setupNeedsTerminal: boolean } }>(
			ops.SETUP_NEEDS_TERMINAL,
		);
		return d.instance.setupNeedsTerminal;
	}

	async certificateStatus(): Promise<CertificateStatus> {
		const d = await this.gql<{ accessStatus: CertificateStatus }>(ops.CERTIFICATE_STATUS);
		return d.accessStatus;
	}

	async accessPreflight(): Promise<AccessPreflight> {
		const d = await this.gql<{ accessPreflight: AccessPreflight }>(ops.ACCESS_PREFLIGHT);
		return d.accessPreflight;
	}

	async lanClientSeenAt(): Promise<string | null> {
		const d = await this.gql<{ accessStatus: { lanClientSeenAt: string | null } }>(
			ops.LAN_CLIENT_SEEN_AT,
		);
		return d.accessStatus.lanClientSeenAt ?? null;
	}

	async checkCloudflareToken(
		token: string,
		domain: string,
		connection?: CloudflareConnection,
	): Promise<CloudflareTokenCheck> {
		const authRevision = this.auth.revision;
		type Found = Omit<CloudflareTokenCheck, 'missing'> & { missing?: string[] | null };
		if (connection) {
			try {
				const d = await this.gql<{ checkCloudflareToken: Found }>(
					ops.checkCloudflareTokenFor(connection),
					{ token, domain },
				);
				return { ...d.checkCloudflareToken, missing: d.checkCloudflareToken.missing ?? [] };
			} catch (e) {
				this.assertAuthRevision(authRevision);
				// The server predates the argument or the field: both fail validation, before
				// anything runs, so the plain check below is the first one Cloudflare sees.
				const msg = e instanceof Error ? e.message : String(e);
				if (!msg.includes('argument "connection"') && !msg.includes('Unknown field "missing"')) {
					throw e;
				}
			}
		}
		const d = await this.gql<{ checkCloudflareToken: Found }>(ops.CHECK_CLOUDFLARE_TOKEN, {
			token,
			domain,
		});
		return { ...d.checkCloudflareToken, missing: [] };
	}

	async startCloudflareSetup(input: CloudflareSetupInput): Promise<AccessStatus> {
		const d = await this.gql<{ startCloudflareSetup: AccessStatus }>(ops.START_CLOUDFLARE_SETUP, {
			input: { ...input, publicIp: input.publicIp ?? null },
		});
		return d.startCloudflareSetup;
	}

	async retryCloudflareSetup(token: string): Promise<AccessStatus> {
		const d = await this.gql<{ retryCloudflareSetup: AccessStatus }>(ops.RETRY_CLOUDFLARE_SETUP, {
			token,
		});
		return d.retryCloudflareSetup;
	}

	async removeCloudflareSetup(token: string): Promise<AccessStatus> {
		const d = await this.gql<{ removeCloudflareSetup: AccessStatus }>(ops.REMOVE_CLOUDFLARE_SETUP, {
			token,
		});
		return d.removeCloudflareSetup;
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
		const d = await this.gql<{ reports: ReportPage }>(ops.REPORTS, {
			status,
			kind,
			page: pageArg(page),
		});
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
		const d = await this.gql<{ coverIssues: Paginated<CoverIssue> }>(ops.COVER_ISSUES, {
			page: pageArg(page),
		});
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
			headers: this.auth.token ? { authorization: `Bearer ${this.auth.token}` } : {},
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

	async splitProgressReaders(workId: Id, sourceSeriesIds: Id[]): Promise<number> {
		const d = await this.gql<{ splitProgressReaders: number }>(ops.SPLIT_PROGRESS_READERS, {
			workId,
			sourceSeriesIds,
		});
		return d.splitProgressReaders;
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

	async extensionRepos(): Promise<ExtensionRepo[]> {
		const d = await this.gql<{ extensionRepos: ExtensionRepo[] }>(ops.EXTENSION_REPOS);
		return d.extensionRepos;
	}

	async removeExtensionRepo(indexUrl: string): Promise<boolean> {
		const d = await this.gql<{ removeExtensionRepo: boolean }>(ops.REMOVE_EXTENSION_REPO, {
			indexUrl,
		});
		return d.removeExtensionRepo;
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

	async extensionAutoUpdate(): Promise<ExtensionAutoUpdate> {
		const d = await this.gql<{ extensionAutoUpdate: ExtensionAutoUpdate }>(
			ops.EXTENSION_AUTO_UPDATE,
		);
		return d.extensionAutoUpdate;
	}

	async runExtensionUpdates(): Promise<ExtensionAutoUpdate> {
		const d = await this.gql<{ runExtensionUpdates: ExtensionAutoUpdate }>(
			ops.RUN_EXTENSION_UPDATES,
		);
		return d.runExtensionUpdates;
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
		this.auth.posture = undefined;
		return d.setShowNsfw;
	}

	async myBlockRules(): Promise<BlockRule[]> {
		const d = await this.gql<{ myBlockRules: BlockRule[] }>(ops.MY_BLOCK_RULES);
		return d.myBlockRules;
	}

	async setMyBlockRules(rules: BlockRule[]): Promise<BlockRule[]> {
		const d = await this.gql<{ setMyBlockRules: BlockRule[] }>(ops.SET_MY_BLOCK_RULES, {
			rules: rules.map(blockRuleInput),
		});
		this.auth.posture = undefined;
		return d.setMyBlockRules;
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
	async myComments(page = 1): Promise<Paginated<MyComment>> {
		const d = await this.gql<{ myComments: Paginated<MyComment> }>(ops.MY_COMMENTS, {
			page: pageArg(page),
		});
		return d.myComments;
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
			headers: this.auth.token ? { authorization: `Bearer ${this.auth.token}` } : {},
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
	/** Endpoint/schema-scoped, like {@link seriesPageUnsupported}: one `Unknown field` answer means
	 *  every later call would get it too. */
	async chapterOrigin(chapterId: Id): Promise<ChapterOrigin | null | undefined> {
		const authRevision = this.auth.revision;
		if (this.capabilities.chapterOriginUnsupported) return undefined;
		try {
			const d = await this.gql<{ chapterOrigin: ChapterOrigin | null }>(ops.CHAPTER_ORIGIN, {
				chapterId,
			});
			return d.chapterOrigin;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			const msg = e instanceof Error ? e.message : String(e);
			// The server predates the query — deploy-order drift, not an outage.
			if (msg.includes('Unknown field "chapterOrigin"')) {
				this.capabilities.chapterOriginUnsupported = true;
				return undefined;
			}
			throw e;
		}
	}

	/** Endpoint/schema-scoped like {@link chapterOrigin}. Anonymous and the same for every
	 *  viewer, so it rides the cacheable GET. */
	async mediaRoutes(): Promise<MediaRoute[] | undefined> {
		const authRevision = this.auth.revision;
		if (this.capabilities.mediaRoutesUnsupported) return undefined;
		try {
			const d = await this.gql<{ mediaRoutes: MediaRoute[] }>(ops.MEDIA_ROUTES);
			return d.mediaRoutes;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			const msg = e instanceof Error ? e.message : String(e);
			if (msg.includes('Unknown field "mediaRoutes"')) {
				this.capabilities.mediaRoutesUnsupported = true;
				return undefined;
			}
			throw e;
		}
	}

	async chapterWork(chapterId: Id): Promise<ChapterWork | null | undefined> {
		const authRevision = this.auth.revision;
		if (this.capabilities.chapterWorkUnsupported) return undefined;
		try {
			const d = await this.gql<{ chapterWork: ChapterWork | null }>(ops.CHAPTER_WORK, {
				chapterId,
			});
			return d.chapterWork;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			if (e instanceof Error && e.message.includes('Unknown field "chapterWork"')) {
				this.capabilities.chapterWorkUnsupported = true;
				return undefined;
			}
			throw e;
		}
	}

	async workSources(workId: Id): Promise<WorkSource[]> {
		const d = await this.gql<{ workSources: WorkSource[] }>(ops.WORK_SOURCES, { workId });
		return d.workSources;
	}
	/**
	 * Whether the API has told us it predates `seriesPage` (P2.1). Endpoint/schema-scoped for
	 * the same deploy-order reason as {@link unsupportedFields}: once one request
	 * gets `Unknown field "seriesPage"`, every later call would too — remember it
	 * and answer `undefined` immediately so callers fall back without a wasted
	 * round trip per page view.
	 */

	async seriesPage(
		workId: Id,
		withAggregated = false,
	): Promise<SeriesPageBundle | null | undefined> {
		const authRevision = this.auth.revision;
		if (this.capabilities.seriesPageUnsupported) return undefined;
		try {
			// A signed-in viewer whose catalogue answers are the anonymous ones gets the page
			// as an anonymous GET an edge cache can keep, and its own part by POST.
			const split =
				!!this.config.publicReads &&
				!!this.auth.token &&
				!this.capabilities.seriesPageViewerUnsupported &&
				(await this.anonymousPosture(authRevision));
			this.assertAuthRevision(authRevision);
			// The aggregation is an optional enrichment the reader can open a chapter without
			// (it lists one copy per number instead of every group's), so ITS failure must
			// not take the series, spine and source lists down with it. The server types the
			// field nullable for exactly this; here a null from a failure reads as an empty
			// aggregation — the bundle was ASKED for it, so `undefined` would send the reader
			// on a pointless second `aggregatedChapters` round trip to the same failing query.
			const [d, viewer] = await Promise.all([
				this.gql<{ seriesPage: SeriesPageBundle }>(
					ops.SERIES_PAGE,
					{ workId, withAggregated },
					false,
					[SERIES_PAGE_AGGREGATED],
					authRevision,
					split ? 'public' : 'auto',
				),
				split ? this.seriesPageViewer(workId, withAggregated, authRevision) : undefined,
			]);
			// The server predates `seriesPageViewer` (now remembered): ask signed in.
			if (viewer === null) return this.seriesPage(workId, withAggregated);
			if (viewer && d.seriesPage) d.seriesPage = layViewerOver(d.seriesPage, viewer);
			// A bundle fetched under a previous sign-in must not seed this account's base
			// revision; the page itself is handled as before.
			if (this.auth.revision === authRevision && d.seriesPage?.viewerProgress) {
				this.recordRevision(workId, d.seriesPage.viewerProgress.revision);
				this.recordRevision(d.seriesPage.workId, d.seriesPage.viewerProgress.revision);
			}
			if (withAggregated && d.seriesPage && d.seriesPage.aggregated == null) {
				return { ...d.seriesPage, aggregated: [] };
			}
			return d.seriesPage;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			// An API that still types `aggregated` NON-null: the same failure nulls all of
			// `data`, but its error path still names the field. Ask once more without it.
			if (withAggregated && failedOnlyAt(e, [SERIES_PAGE_AGGREGATED])) {
				console.warn('[komika] seriesPage aggregation failed; retrying without it:', e.message);
				const bundle = await this.seriesPage(workId, false);
				return bundle ? { ...bundle, aggregated: [] } : bundle;
			}
			const msg = e instanceof Error ? e.message : String(e);
			// The server predates the resolver — schema-drift fallback, not an outage.
			if (msg.includes('Unknown field "seriesPage"')) {
				this.capabilities.seriesPageUnsupported = true;
				return undefined;
			}
			// The resolver's own not-found (unknown id / NSFW-gated): the page 404s,
			// exactly as the old fan-out rendering nothing did. Anything else is a real
			// failure the caller must surface honestly.
			if (msg.includes('No such work')) return null;
			throw e;
		}
	}

	/** The viewer's part of {@link seriesPage}, or null when the server predates it. */
	private async seriesPageViewer(
		workId: Id,
		withAggregated: boolean,
		authRevision: number,
	): Promise<SeriesPageViewerData | null> {
		try {
			const d = await this.gql<{ seriesPageViewer: SeriesPageViewerData }>(
				ops.SERIES_PAGE_VIEWER,
				{ workId, withAggregated },
				false,
				undefined,
				authRevision,
				'post',
			);
			return d.seriesPageViewer;
		} catch (e) {
			this.assertAuthRevision(authRevision);
			if (e instanceof Error && e.message.includes('Unknown field "seriesPageViewer"')) {
				this.capabilities.seriesPageViewerUnsupported = true;
				return null;
			}
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

/** The bundle's optional aggregation — the one field a `seriesPage` read survives losing. */
const SERIES_PAGE_AGGREGATED = ['seriesPage', 'aggregated'] as const;

export function createBackend(config: BackendConfig): Backend {
	return new GraphQLBackend(config);
}
