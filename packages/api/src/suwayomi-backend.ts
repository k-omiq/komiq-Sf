import type {
	Chapter,
	Comment,
	CommentTargetType,
	DiscoveryFeed,
	Id,
	Page,
	Paginated,
	Review,
	Series,
	SeriesStatus,
	ComicType,
} from '@komika/types';
import type {
	Backend,
	PostCommentInput,
	PostReviewInput,
	RegisterInput,
	Session,
} from './backend.js';

/**
 * Direct adapter to a Suwayomi/Tachidesk server's GraphQL API.
 *
 * This talks to Suwayomi's real schema and maps it onto `@komika/types`, so the
 * reader can show live catalog/chapters/pages WITHOUT the (not-yet-built) Komika
 * unified server in front. Suwayomi has no social layer, so reviews/comments/auth
 * return empty/throw — those stay on mock until Komika's own services exist.
 */
export interface SuwayomiConfig {
	/** Base URL of the Suwayomi server, e.g. http://localhost:4567 */
	baseUrl: string;
	/** Pin a specific source id to browse; otherwise an English source is auto-picked. */
	sourceId?: string;
	fetch?: typeof fetch;
}

const MANGA_FIELDS = /* GraphQL */ `
	fragment MangaFields on MangaType {
		id
		title
		thumbnailUrl
		author
		artist
		description
		genre
		status
		inLibrary
		inLibraryAt
		lastFetchedAt
		sourceId
		source {
			lang
		}
		chapters {
			totalCount
		}
	}
`;

const CHAPTER_FIELDS = /* GraphQL */ `
	fragment ChapterFields on ChapterType {
		id
		mangaId
		name
		chapterNumber
		scanlator
		uploadDate
		isRead
		isBookmarked
		isDownloaded
		lastPageRead
		pageCount
	}
`;

const SOURCES = /* GraphQL */ `
	query Sources {
		sources {
			nodes {
				id
				lang
			}
		}
	}
`;

const FETCH_SOURCE_MANGA = /* GraphQL */ `
	${MANGA_FIELDS}
	mutation FetchSourceManga(
		$source: LongString!
		$type: FetchSourceMangaType!
		$page: Int!
		$query: String
	) {
		fetchSourceManga(input: { source: $source, type: $type, page: $page, query: $query }) {
			hasNextPage
			mangas {
				...MangaFields
			}
		}
	}
`;

const MANGA = /* GraphQL */ `
	${MANGA_FIELDS}
	query Manga($id: Int!) {
		manga(id: $id) {
			...MangaFields
		}
	}
`;

// Fetch full detail from the source (populates genres/status/description).
const FETCH_DETAIL = /* GraphQL */ `
	${MANGA_FIELDS}
	mutation FetchDetail($id: Int!) {
		fetchMangaAndChapters(input: { id: $id, fetchManga: true, fetchChapters: false }) {
			manga {
				...MangaFields
			}
		}
	}
`;

// Fetch the chapter list from the source (payload.chapters is a plain list).
const FETCH_CHAPTERS = /* GraphQL */ `
	${CHAPTER_FIELDS}
	mutation FetchChapters($id: Int!) {
		fetchMangaAndChapters(input: { id: $id, fetchManga: false, fetchChapters: true }) {
			chapters {
				...ChapterFields
			}
		}
	}
`;

const CHAPTERS = /* GraphQL */ `
	${CHAPTER_FIELDS}
	query Chapters($id: Int!) {
		chapters(condition: { mangaId: $id }, order: { by: SOURCE_ORDER, byType: DESC }) {
			nodes {
				...ChapterFields
			}
		}
	}
`;

const FETCH_PAGES = /* GraphQL */ `
	mutation FetchPages($id: Int!) {
		fetchChapterPages(input: { chapterId: $id }) {
			pages
		}
	}
`;

const LIBRARY = /* GraphQL */ `
	${MANGA_FIELDS}
	query Library {
		mangas(condition: { inLibrary: true }) {
			nodes {
				...MangaFields
			}
		}
	}
`;

const UPDATE_MANGA = /* GraphQL */ `
	mutation UpdateManga($id: Int!, $inLibrary: Boolean!) {
		updateManga(input: { id: $id, patch: { inLibrary: $inLibrary } }) {
			manga {
				id
			}
		}
	}
`;

const UPDATE_CHAPTER = /* GraphQL */ `
	mutation UpdateChapter($id: Int!, $lastPageRead: Int!, $isRead: Boolean!) {
		updateChapter(input: { id: $id, patch: { lastPageRead: $lastPageRead, isRead: $isRead } }) {
			chapter {
				id
			}
		}
	}
`;

interface SuwayomiManga {
	id: number;
	title: string;
	thumbnailUrl: string | null;
	author: string | null;
	artist: string | null;
	description: string | null;
	genre: string[];
	status: string;
	inLibrary: boolean;
	inLibraryAt: string | null;
	lastFetchedAt: string | null;
	sourceId: string;
	source: { lang: string } | null;
	chapters: { totalCount: number };
}

interface SuwayomiChapter {
	id: number;
	mangaId: number;
	name: string;
	chapterNumber: number;
	scanlator: string | null;
	uploadDate: string | null;
	isRead: boolean;
	isBookmarked: boolean;
	isDownloaded: boolean;
	lastPageRead: number;
	pageCount: number;
}

const STATUS_MAP: Record<string, SeriesStatus> = {
	ONGOING: 'ONGOING',
	COMPLETED: 'COMPLETED',
	PUBLISHING_FINISHED: 'COMPLETED',
	LICENSED: 'COMPLETED',
	CANCELLED: 'CANCELLED',
	ON_HIATUS: 'HIATUS',
	UNKNOWN: 'UNKNOWN',
};

/** Map a source language to a Komika comic type (best-effort). */
function typeFromLang(lang: string | undefined): ComicType {
	if (!lang) return 'MANGA';
	const l = lang.toLowerCase();
	if (l.startsWith('ko')) return 'MANHWA';
	if (l.startsWith('zh')) return 'MANHUA';
	return 'MANGA';
}

/** Suwayomi timestamps are epoch (seconds or millis, as strings). Coerce to ISO. */
function toIso(v: string | null): string {
	if (!v) return '';
	const n = Number(v);
	if (!Number.isFinite(n) || n <= 0) return '';
	const ms = n > 1e12 ? n : n * 1000;
	try {
		return new Date(ms).toISOString();
	} catch {
		return '';
	}
}

export class SuwayomiBackend implements Backend {
	private sourceId: string | null = null;

	constructor(private config: SuwayomiConfig) {}

	private get endpoint(): string {
		return this.config.baseUrl.replace(/\/$/, '') + '/api/graphql';
	}

	private abs(url: string | null): string {
		if (!url) return '';
		if (url.startsWith('http')) return url;
		return this.config.baseUrl.replace(/\/$/, '') + url;
	}

	private async gql<T>(query: string, variables?: Record<string, unknown>): Promise<T> {
		const doFetch = this.config.fetch ?? fetch;
		const res = await doFetch(this.endpoint, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ query, variables }),
		});
		if (!res.ok) throw new Error(`Suwayomi error ${res.status}`);
		const json = (await res.json()) as { data?: T; errors?: { message: string }[] };
		if (json.errors?.length) throw new Error(json.errors.map((e) => e.message).join('; '));
		if (json.data == null) throw new Error('Suwayomi returned no data');
		return json.data;
	}

	/**
	 * Pick a source to browse; cached. Prefers `config.sourceId`, then an English
	 * source, then the first real (non-local) source. Throws if none installed.
	 */
	private async resolveSource(): Promise<string> {
		if (this.sourceId) return this.sourceId;
		if (this.config.sourceId) {
			this.sourceId = this.config.sourceId;
			return this.sourceId;
		}
		const d = await this.gql<{ sources: { nodes: { id: string; lang: string }[] } }>(SOURCES);
		const real = d.sources.nodes.filter((s) => s.id !== '0');
		const chosen = real.find((s) => s.lang === 'en') ?? real[0];
		if (!chosen) throw new Error('No Suwayomi source installed — add one in Suwayomi first');
		this.sourceId = chosen.id;
		return this.sourceId;
	}

	private mapManga(m: SuwayomiManga): Series {
		return {
			id: String(m.id),
			title: m.title,
			altTitles: [],
			author: m.author,
			artist: m.artist,
			description: m.description,
			genres: m.genre ?? [],
			type: typeFromLang(m.source?.lang),
			status: STATUS_MAP[m.status] ?? 'UNKNOWN',
			coverUrl: this.abs(m.thumbnailUrl),
			sourceId: String(m.sourceId),
			chapterCount: m.chapters?.totalCount ?? 0,
			isMarked: m.inLibrary,
			isNsfw: false, // canonical NSFW flag is a Komika service; Suwayomi has none
			rating: { average: 0, count: 0, distribution: [] },
			scan: {
				avgIntervalHours: 0,
				overrideIntervalHours: null,
				pollEveryMinutes: 30,
				pollEveryMinutesOverride: null,
				paused: m.status === 'COMPLETED' || m.status === 'ON_HIATUS',
				// The Suwayomi adapter has no Komika admin overrides.
				statusOverride: null,
				pausedOverride: null,
				lastScannedAt: toIso(m.lastFetchedAt),
				nextScanAt: null,
			},
			createdAt: toIso(m.inLibraryAt),
			updatedAt: toIso(m.lastFetchedAt),
		};
	}

	private mapChapter(c: SuwayomiChapter): Chapter {
		return {
			id: String(c.id),
			seriesId: String(c.mangaId),
			number: c.chapterNumber,
			title: c.name,
			pageCount: c.pageCount,
			uploadedAt: toIso(c.uploadDate),
			scanlator: c.scanlator,
			read: c.isRead,
			lastPageRead: c.lastPageRead,
			bookmarked: c.isBookmarked,
			isDownloaded: c.isDownloaded,
		};
	}

	private async fetchSource(
		type: 'POPULAR' | 'LATEST' | 'SEARCH',
		page: number,
		query?: string,
	): Promise<{ hasNextPage: boolean; items: Series[] }> {
		const source = await this.resolveSource();
		const d = await this.gql<{
			fetchSourceManga: { hasNextPage: boolean; mangas: SuwayomiManga[] };
		}>(FETCH_SOURCE_MANGA, { source, type, page, query });
		return {
			hasNextPage: d.fetchSourceManga.hasNextPage,
			items: d.fetchSourceManga.mangas.map((m) => this.mapManga(m)),
		};
	}

	// --- catalog ---
	async discovery(): Promise<DiscoveryFeed[]> {
		const [popular, latest] = await Promise.all([
			this.fetchSource('POPULAR', 1),
			this.fetchSource('LATEST', 1).catch(() => ({ items: [] as Series[], hasNextPage: false })),
		]);
		const feeds: DiscoveryFeed[] = [
			{ kind: 'POPULAR', title: 'Popular', items: popular.items },
			{ kind: 'TRENDING', title: 'Trending', items: popular.items.slice(0, 6) },
		];
		if (latest.items.length) {
			feeds.push({ kind: 'RECENTLY_UPDATED', title: 'Latest Updates', items: latest.items });
			feeds.push({
				kind: 'RECENTLY_ADDED',
				title: 'Latest Added',
				items: latest.items.slice(0, 6),
			});
		}
		return feeds;
	}

	async updates(page = 1): Promise<Paginated<Series>> {
		// Suwayomi has no adaptive scanner; approximate the Updates feed with the
		// source "Latest" endpoint (newest chapters upstream).
		const { items, hasNextPage } = await this.fetchSource('LATEST', page);
		return { items, page, hasNextPage, total: null };
	}

	async search(query: string, page = 1): Promise<Paginated<Series>> {
		const type = query.trim() ? 'SEARCH' : 'POPULAR';
		const { items, hasNextPage } = await this.fetchSource(type, page, query.trim() || undefined);
		return { items, page, hasNextPage, total: null };
	}

	async series(id: Id): Promise<Series> {
		try {
			const d = await this.gql<{ fetchMangaAndChapters: { manga: SuwayomiManga } }>(FETCH_DETAIL, {
				id: Number(id),
			});
			return this.mapManga(d.fetchMangaAndChapters.manga);
		} catch {
			// Fall back to whatever is cached in the DB.
			const d = await this.gql<{ manga: SuwayomiManga }>(MANGA, { id: Number(id) });
			return this.mapManga(d.manga);
		}
	}

	async chapters(seriesId: Id): Promise<Chapter[]> {
		try {
			const d = await this.gql<{ fetchMangaAndChapters: { chapters: SuwayomiChapter[] } }>(
				FETCH_CHAPTERS,
				{ id: Number(seriesId) },
			);
			return (d.fetchMangaAndChapters.chapters ?? []).map((c) => this.mapChapter(c));
		} catch (err) {
			if (String(err).includes('No chapters')) return [];
			// Fall back to already-fetched chapters in the DB.
			const d = await this.gql<{ chapters: { nodes: SuwayomiChapter[] } }>(CHAPTERS, {
				id: Number(seriesId),
			});
			return d.chapters.nodes.map((c) => this.mapChapter(c));
		}
	}

	async pages(chapterId: Id): Promise<Page[]> {
		const d = await this.gql<{ fetchChapterPages: { pages: string[] } }>(FETCH_PAGES, {
			id: Number(chapterId),
		});
		return d.fetchChapterPages.pages.map((url, index) => ({ index, sourceUrl: this.abs(url) }));
	}

	// --- library & progress ---
	async mark(seriesId: Id, marked: boolean): Promise<Series> {
		await this.gql(UPDATE_MANGA, { id: Number(seriesId), inLibrary: marked });
		return this.series(seriesId);
	}

	async library(): Promise<Series[]> {
		const d = await this.gql<{ mangas: { nodes: SuwayomiManga[] } }>(LIBRARY);
		return d.mangas.nodes.map((m) => this.mapManga(m));
	}

	async setProgress(chapterId: Id, lastPageRead: number, read: boolean): Promise<void> {
		await this.gql(UPDATE_CHAPTER, { id: Number(chapterId), lastPageRead, isRead: read });
	}

	// --- social / auth (not available in Suwayomi — stay on Komika's own services) ---
	async session(): Promise<Session | null> {
		return null;
	}
	async login(): Promise<Session> {
		throw new Error('Auth is a Komika service, not available via Suwayomi');
	}
	async register(_input: RegisterInput): Promise<Session> {
		throw new Error('Auth is a Komika service, not available via Suwayomi');
	}
	async logout(): Promise<void> {
		/* no-op */
	}
	async reviews(_seriesId: Id, page = 1): Promise<Paginated<Review>> {
		return { items: [], page, hasNextPage: false, total: 0 };
	}
	async postReview(_input: PostReviewInput): Promise<Review> {
		throw new Error('Reviews are a Komika service, not available via Suwayomi');
	}
	async comments(
		_targetType: CommentTargetType,
		_targetId: Id,
		page = 1,
	): Promise<Paginated<Comment>> {
		return { items: [], page, hasNextPage: false, total: 0 };
	}
	async postComment(_input: PostCommentInput): Promise<Comment> {
		throw new Error('Comments are a Komika service, not available via Suwayomi');
	}
}

export function createSuwayomiBackend(config: SuwayomiConfig): Backend {
	return new SuwayomiBackend(config);
}
