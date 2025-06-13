import type { Chapter, Id, Page, Series, SeriesStatus, ComicType } from '@komika/types';
import type { ContentBackend, SourceRef } from './content-backend.js';
import { isTauri } from './platform.js';
import { suwayomiChapterLabel } from './chapter-label';

/**
 * The engine extension package that provides MangaDex. The "all" package exposes a
 * per-language `SourceType` for every MangaDex language, so it is a single install
 * that serves all of them; the desired language is selected by `SourceType.id` at
 * resolve time (see {@link LocalSuwayomiBackend.resolveMangaDexSourceId}).
 */
const MANGADEX_PKG = 'eu.kanade.tachiyomi.extension.all.mangadex';

/**
 * The curated Keiyoushi extension-store index url the engine installs MangaDex from.
 * Adding the store is idempotent. This is the `index.pb` URL Keiyoushi publishes;
 * the older `index.min.json` on the same branch still resolves but was reduced to
 * two "update your app" stubs when they migrated, so it would seed an empty
 * catalogue. This mirrors Komika's hardcoded two-tier source strategy — MangaDex
 * provisioning coords are supplied by the client, not by `workSources` (whose
 * `extension` is null for MangaDex works).
 */
const KEIYOUSHI_REPO = 'https://github.com/keiyoushi/extensions/raw/repo/index.pb';

/**
 * The minimal structural view of a hosted `WorkSource` that {@link LocalSuwayomiBackend.refFor}
 * consumes — only the fields it reads. Declared locally (rather than importing the full
 * `@komika/types` `WorkSource`) so this content-only adapter stays decoupled from the
 * hosted catalogue shape and accepts any structurally-compatible mapping.
 */
export interface WorkSourceLike {
	/** `"mangadex"` (MangaDex-native) or `"suwayomi"` (via an extension). */
	sourceType: string;
	/** Engine source id for a suwayomi source; the literal `"mangadex"` for a MangaDex work. */
	sourceId: string;
	/** Source-local manga key (= `MangaType.url`); the bare MangaDex uuid for a MangaDex work. */
	sourceKey: string;
	/** Preferred language of this mapping (e.g. `en`); selects the MangaDex `SourceType`. */
	lang?: string | null;
	/** Extension provisioning coords for a suwayomi source; null/absent for a MangaDex work. */
	extension?: { pkgName?: string; repoUrl?: string } | null;
}

/**
 * Content-only adapter to the EMBEDDED Suwayomi engine, reached over an in-process
 * IPC transport (a Tauri `suwayomi_gql` command) instead of an HTTP `fetch`.
 *
 * This is the on-device counterpart to {@link SuwayomiBackend}: it speaks the same
 * Suwayomi GraphQL schema and maps it onto `@komika/types`, but it never opens a
 * network socket — the composite backend routes live series/chapters/pages here
 * when the embedded engine is up, while auth/library/progress/social always stay
 * on the hosted server. It deliberately implements ONLY {@link ContentBackend}.
 *
 * Status: `isReady()` is LIVE — it queries the `suwayomi_status` Tauri command and
 * returns true only when the embedded engine reports `state === "ready"`. Whether
 * the composite backend ever routes content here still depends on a local backend
 * being constructed at all (gated by the `PUBLIC_KOMIKA_NATIVE_ENGINE` flag under
 * Tauri) and on the Wave C content-routing branches being wired.
 */
export class LocalSuwayomiBackend implements ContentBackend {
	/**
	 * Cache of resolved `sourceKey` (= `MangaType.url`) → internal engine `id`. Ids are
	 * stable within a single engine datadir, so once resolved a key never needs the
	 * browse/search round-trip again for the lifetime of this instance.
	 */
	private readonly idCache = new Map<string, number>();

	/**
	 * Extension package ids known to be installed this session, so `ensureProvisioned`
	 * can short-circuit without a GraphQL round-trip after the first install.
	 */
	private readonly installed = new Set<string>();

	/**
	 * In-flight provisioning promises keyed by `pkgName`, so concurrent content calls
	 * for the same extension install it exactly once (install-once / in-flight dedupe).
	 */
	private readonly provisioning = new Map<string, Promise<void>>();

	/**
	 * Cache of resolved MangaDex `SourceType.id` keyed by language, so the `sources`
	 * round-trip in {@link refFor} runs at most once per language for the instance
	 * lifetime. The MangaDex source ids are stable within an engine datadir.
	 */
	private readonly mangaDexSourceIdCache = new Map<string, string>();

	/**
	 * Execute a GraphQL document against the embedded engine over IPC.
	 *
	 * Transports the document through the `suwayomi_gql` Tauri command, which
	 * proxies it to the loopback engine. `invoke` is imported dynamically (mirroring
	 * image-provider.ts) so this package keeps building for the web target, where
	 * `@tauri-apps/api` is absent at runtime; content calls only reach here when
	 * {@link isReady} is true (Tauri + engine ready), so the web path never runs it.
	 */
	private async gql<T>(query: string, variables?: Record<string, unknown>): Promise<T> {
		const { invoke } = await import('@tauri-apps/api/core');
		const json = await invoke<{ data?: T; errors?: { message: string }[] }>('suwayomi_gql', {
			query,
			variables,
		});
		if (json.errors?.length) throw new Error(json.errors.map((e) => e.message).join('; '));
		if (json.data == null) throw new Error('Suwayomi returned no data');
		return json.data;
	}

	/**
	 * Whether the embedded engine is up and can serve content right now.
	 *
	 * Queries the `suwayomi_status` Tauri command and returns true iff the engine
	 * reports `state === "ready"`; any other state (`starting` / `degraded` /
	 * `stopped`) yields false. Never throws: on the web target `@tauri-apps/api` is
	 * absent, so we short-circuit via {@link isTauri} before importing it, and any
	 * failure of the invoke (engine not up, command missing) is swallowed as false.
	 */
	async isReady(): Promise<boolean> {
		if (!isTauri()) return false;
		try {
			const { invoke } = await import('@tauri-apps/api/core');
			const status = await invoke<{ state?: string }>('suwayomi_status');
			return status?.state === 'ready';
		} catch {
			return false;
		}
	}

	async series(ref: SourceRef): Promise<Series> {
		await this.ensureProvisioned(ref);
		const id = await this.resolveMangaId(ref);
		if (id == null) throw new Error(`Suwayomi: cannot resolve ${ref.sourceId}/${ref.sourceKey}`);
		const d = await this.gql<{ fetchMangaAndChapters: { manga: SuwayomiManga } }>(FETCH_DETAIL, {
			id,
		});
		const manga = d.fetchMangaAndChapters?.manga;
		if (!manga) throw new Error(`Suwayomi: no manga for ${ref.sourceId}/${ref.sourceKey}`);
		return mapManga(manga);
	}

	async chapters(ref: SourceRef): Promise<Chapter[]> {
		await this.ensureProvisioned(ref);
		const id = await this.resolveMangaId(ref);
		if (id == null) throw new Error(`Suwayomi: cannot resolve ${ref.sourceId}/${ref.sourceKey}`);
		const d = await this.gql<{ fetchMangaAndChapters: { chapters: SuwayomiChapter[] } }>(
			FETCH_CHAPTERS,
			{ id },
		);
		return (d.fetchMangaAndChapters?.chapters ?? []).map((c) => mapChapter(c));
	}

	async pages(chapterId: Id): Promise<Page[]> {
		const d = await this.gql<{ fetchChapterPages: { pages: string[] } }>(FETCH_PAGES, {
			id: Number(chapterId),
		});
		// No base-URL rewrite here: the engine is reached over IPC, not an HTTP base
		// URL, so page URLs (relative Suwayomi proxy paths like
		// `/api/v1/manga/3/chapter/1/page/0`) are passed through unchanged. The
		// NativeImageProvider / local proxy resolves them against the engine base URL.
		return (d.fetchChapterPages?.pages ?? []).map((url, index) => ({ index, sourceUrl: url }));
	}

	/**
	 * Map a hosted {@link WorkSourceLike} to the engine {@link SourceRef} the content
	 * methods consume, or null when the mapping can't be resolved on-device.
	 *
	 * Two cases, per Komika's two-tier source strategy:
	 *   - **suwayomi source** — straight passthrough: the engine source id and key
	 *     already match, and the extension coords ride along from `ws.extension`.
	 *   - **mangadex source** — the client supplies the MangaDex provisioning coords
	 *     ({@link MANGADEX_PKG} / {@link KEIYOUSHI_REPO}) because `ws.extension` is null
	 *     for MangaDex works. The engine source id is resolved DYNAMICALLY from the
	 *     desired language (never hardcoded), which requires the MangaDex extension to
	 *     be installed first, so this provisions before the `sources` lookup. The engine
	 *     wants a `/manga/<uuid>` key, so a bare uuid is prefixed.
	 *
	 * Returns null (rather than throwing) for a resolution/provisioning miss so the
	 * composite falls back to hosted; genuinely unexpected errors propagate to the
	 * composite's catch.
	 */
	async refFor(ws: WorkSourceLike, title?: string): Promise<SourceRef | null> {
		if (ws.sourceType === 'suwayomi') {
			return {
				sourceId: ws.sourceId,
				sourceKey: ws.sourceKey,
				pkgName: ws.extension?.pkgName,
				repoUrl: ws.extension?.repoUrl,
				title,
			};
		}
		if (ws.sourceType === 'mangadex') {
			// Install MangaDex before the `sources` query — the SourceType only exists
			// once the extension is present. A provisioning failure is swallowed here so
			// the subsequent resolve simply misses and we return null (composite falls back).
			await this.ensureProvisionedPkg(MANGADEX_PKG, KEIYOUSHI_REPO).catch(() => undefined);
			const sourceId = await this.resolveMangaDexSourceId(ws.lang || 'en');
			if (sourceId == null) return null;
			const sourceKey = ws.sourceKey.startsWith('/manga/') ? ws.sourceKey : `/manga/${ws.sourceKey}`;
			return { sourceId, sourceKey, pkgName: MANGADEX_PKG, repoUrl: KEIYOUSHI_REPO, title };
		}
		return null;
	}

	/**
	 * Resolve the engine's MangaDex `SourceType.id` for a language, or null. Picks the
	 * source whose `extension.pkgName === MANGADEX_PKG` and `lang === lang`, falling back
	 * to the first MangaDex source when the exact language isn't present. Requires the
	 * MangaDex extension to be installed already (callers provision first). Cached per
	 * language for the instance lifetime; never throws (returns null on a query miss).
	 */
	private async resolveMangaDexSourceId(lang = 'en'): Promise<string | null> {
		const cached = this.mangaDexSourceIdCache.get(lang);
		if (cached != null) return cached;
		const d = await this.gql<{ sources: { nodes: SuwayomiSource[] } }>(MANGADEX_SOURCES).catch(
			() => null,
		);
		const mangaDex = (d?.sources?.nodes ?? []).filter((s) => s.extension?.pkgName === MANGADEX_PKG);
		const match = mangaDex.find((s) => s.lang === lang) ?? mangaDex[0];
		if (!match) return null;
		this.mangaDexSourceIdCache.set(lang, match.id);
		return match.id;
	}

	/**
	 * Resolve a work's `(sourceId, sourceKey)` to the engine's internal integer manga
	 * `id`, persisting the manga in the engine DB as a side effect when needed.
	 *
	 * v2.3.2243 has no getOrInsert-by-url: every per-manga operation keys off the
	 * internal `MangaType.id`, which only exists after a browse/search inserts the row.
	 * Three stages, cheapest first:
	 *   1. Fast path — `mangas(condition:{ sourceId, url })`; if already persisted, done.
	 *   2. MangaDex — a `/manga/<uuid>` key resolves via `fetchSourceManga(SEARCH,
	 *      query:"id:<uuid>")`, which returns exactly that manga inline (and persists it).
	 *   3. General fallback — if a `title` hint exists, `fetchSourceManga(SEARCH, title)`
	 *      then match a result whose `url === sourceKey`.
	 * Returns null when nothing resolves (the composite then falls back to hosted).
	 * The resolved id is memoised for the instance lifetime.
	 */
	async resolveMangaId(ref: SourceRef): Promise<number | null> {
		const cached = this.idCache.get(ref.sourceKey);
		if (cached != null) return cached;

		// 1. Fast path: already persisted.
		const found = await this.gql<{ mangas: { nodes: { id: number }[]; totalCount: number } }>(
			MANGAS_BY_URL,
			{ sid: ref.sourceId, url: ref.sourceKey },
		).catch(() => null);
		if (found && found.mangas.totalCount >= 1 && found.mangas.nodes[0]) {
			return this.cacheId(ref.sourceKey, found.mangas.nodes[0].id);
		}

		// 2. MangaDex resolve+persist by uuid handle.
		const uuid = ref.sourceKey.match(/\/manga\/([0-9a-f-]{36})/i)?.[1];
		if (uuid) {
			const id = await this.searchForKey(ref, `id:${uuid}`, ref.sourceKey);
			if (id != null) return this.cacheId(ref.sourceKey, id);
		}

		// 3. General fallback: title search, match by url. The `id:` handle above is
		//    MangaDex-specific, so this covers other sources (and MangaDex misses).
		if (ref.title) {
			const id = await this.searchForKey(ref, ref.title, ref.sourceKey);
			if (id != null) return this.cacheId(ref.sourceKey, id);
		}

		return null;
	}

	/** Store and return a resolved id (keeps `resolveMangaId` readable). */
	private cacheId(key: string, id: number): number {
		this.idCache.set(key, id);
		return id;
	}

	/**
	 * Browse/search the source and return the internal id of the result whose
	 * `url === wantUrl`, or null. Every returned manga is persisted by the engine.
	 */
	private async searchForKey(
		ref: SourceRef,
		query: string,
		wantUrl: string,
	): Promise<number | null> {
		const d = await this.gql<{
			fetchSourceManga: { mangas: { id: number; url: string }[] };
		}>(FETCH_SOURCE_MANGA, { source: ref.sourceId, type: 'SEARCH', page: 1, query }).catch(
			() => null,
		);
		const match = d?.fetchSourceManga?.mangas?.find((m) => m.url === wantUrl);
		return match?.id ?? null;
	}

	/**
	 * Install the extension named by `ref.pkgName` on-device before any resolve/fetch,
	 * so its source is available to the engine. No-op when `ref.pkgName` is absent or
	 * the extension is already known-installed. Install-once / in-flight deduped: a
	 * shared promise per `pkgName` collapses concurrent calls into a single install.
	 *
	 * NEVER hard-fails silently by intent, but DOES rethrow on provisioning error so
	 * the caller's content method throws and the composite falls back to hosted. The
	 * verified sequence (GQL-SCHEMA-FINDINGS §B): optional `addExtensionStore` →
	 * `fetchExtensions` → `updateExtension(patch:{ install:true })`. NSFW gating is
	 * handled upstream by the composite — here we install exactly what we're asked.
	 */
	private async ensureProvisioned(ref: SourceRef): Promise<void> {
		return this.ensureProvisionedPkg(ref.pkgName, ref.repoUrl);
	}

	/**
	 * The pkg-keyed core of {@link ensureProvisioned}, so callers that hold raw
	 * provisioning coords rather than a full `SourceRef` (e.g. {@link refFor} resolving
	 * MangaDex) can install through the same install-once / in-flight-deduped path.
	 */
	private async ensureProvisionedPkg(pkg: string | undefined, repoUrl?: string): Promise<void> {
		if (!pkg || this.installed.has(pkg)) return;

		const inflight = this.provisioning.get(pkg);
		if (inflight) return inflight;

		const task = this.provision(pkg, repoUrl)
			.then(() => {
				this.installed.add(pkg);
			})
			.finally(() => {
				this.provisioning.delete(pkg);
			});
		this.provisioning.set(pkg, task);
		return task;
	}

	/** The raw provisioning round-trips for one extension (see {@link ensureProvisioned}). */
	private async provision(pkg: string, repoUrl?: string): Promise<void> {
		// Already installed from a previous session/datadir? Skip the install work.
		const state = await this.gql<{ extension: { isInstalled: boolean } | null }>(EXTENSION_STATE, {
			pkg,
		}).catch(() => null);
		if (state?.extension?.isInstalled) return;

		// Register the store (idempotent — adding an existing store is fine; tolerate errors).
		if (repoUrl) {
			await this.gql(ADD_EXTENSION_STORE, { indexUrl: repoUrl }).catch(() => undefined);
		}
		// Refresh the extension list from all stores, then install the package.
		await this.gql(FETCH_EXTENSIONS);
		await this.gql(INSTALL_EXTENSION, { id: pkg });
	}
}

// --- Suwayomi GraphQL shapes (re-declared subset, mirroring suwayomi-backend.ts) ---

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

// Browse/search a source. This is NOT a by-key fetch: it returns a page of results
// and persists every returned manga (giving each an internal `id`). Used only to
// resolve+persist a `sourceKey` when it is not yet in the engine DB, so it selects the
// minimal `{ id url }` needed to match — detail is read separately via FETCH_DETAIL.
const FETCH_SOURCE_MANGA = /* GraphQL */ `
	mutation FetchSourceManga(
		$source: LongString!
		$type: FetchSourceMangaType!
		$page: Int!
		$query: String
	) {
		fetchSourceManga(input: { source: $source, type: $type, page: $page, query: $query }) {
			hasNextPage
			mangas {
				id
				url
			}
		}
	}
`;

// Resolve a persisted manga's internal `id` from its `(sourceId, url)`. Returns
// `totalCount: 0` for a manga that has never been browsed (no auto-insert).
const MANGAS_BY_URL = /* GraphQL */ `
	query MangasByUrl($sid: LongString!, $url: String!) {
		mangas(condition: { sourceId: $sid, url: $url }) {
			nodes {
				id
			}
			totalCount
		}
	}
`;

// Fetch full detail from the source by internal id (populates genres/status/etc.).
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

// Fetch the chapter list from the source by internal manga id (payload.chapters is a
// plain list). The returned `ChapterType.id` is the engine's internal chapter id.
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

const FETCH_PAGES = /* GraphQL */ `
	mutation FetchPages($id: Int!) {
		fetchChapterPages(input: { chapterId: $id }) {
			pages
		}
	}
`;

// --- extension provisioning (N2.1) ---

// Is a given extension already installed? (`extension` is null for an unknown pkg.)
const EXTENSION_STATE = /* GraphQL */ `
	query ExtensionState($pkg: String!) {
		extension(pkgName: $pkg) {
			isInstalled
		}
	}
`;

// Register an extension store (repo) by its index url. Idempotent server-side.
const ADD_EXTENSION_STORE = /* GraphQL */ `
	mutation AddExtensionStore($indexUrl: String!) {
		addExtensionStore(input: { indexUrl: $indexUrl }) {
			extensionStore {
				indexUrl
			}
		}
	}
`;

// Refresh the available-extension list from all configured stores.
const FETCH_EXTENSIONS = /* GraphQL */ `
	mutation FetchExtensions {
		fetchExtensions(input: {}) {
			extensions {
				pkgName
			}
		}
	}
`;

// Install an extension by pkgName (`id` on the input is the pkgName).
const INSTALL_EXTENSION = /* GraphQL */ `
	mutation InstallExtension($id: String!) {
		updateExtension(input: { id: $id, patch: { install: true } }) {
			extension {
				pkgName
				isInstalled
			}
		}
	}
`;

// List installed sources with their language + owning extension, so a MangaDex
// `SourceType.id` can be resolved dynamically for a language (the "all" MangaDex
// extension exposes one SourceType per language). `SourceType.id` is a `LongString`
// (serialized as a string). Requires the MangaDex extension installed first.
const MANGADEX_SOURCES = /* GraphQL */ `
	query MangaDexSources {
		sources {
			nodes {
				id
				lang
				extension {
					pkgName
				}
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

interface SuwayomiSource {
	/** `SourceType.id` — a `LongString`, serialized as a string. */
	id: string;
	lang: string;
	extension: { pkgName: string } | null;
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

function mapManga(m: SuwayomiManga): Series {
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
		// No base-URL rewrite: cover bytes are resolved by the image provider.
		coverUrl: m.thumbnailUrl ?? '',
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

function mapChapter(c: SuwayomiChapter): Chapter {
	return {
		id: String(c.id),
		seriesId: String(c.mangaId),
		number: c.chapterNumber,
		// No Komika server on this path, so the label is derived here by the same
		// precedence the server applies. Suwayomi's `-1` oneshot sentinel and the
		// `99999999` TEST uploads both fail the sanity check and fall back to the
		// chapter's own name — see `chapter-label.ts`.
		label: suwayomiChapterLabel(c.chapterNumber, c.name),
		// Suwayomi serves pages for everything it lists; an off-site chapter is a
		// MangaDex concept and cannot occur on this path.
		externalUrl: null,
		title: c.name,
		pageCount: c.pageCount,
		uploadedAt: toIso(c.uploadDate),
		// NULL, AND IT MUST STAY NULL ON THIS PATH. These backends talk straight to a
		// Suwayomi instance with no Komika server behind them, so there is no
		// `release_event` ledger and therefore no record of when anything was first
		// seen. Reusing `uploadedAt` here would manufacture exactly the claim the
		// field exists to prevent; the UI renders "—" for null, which is the truth.
		foundAt: null,
		scanlator: c.scanlator,
		read: c.isRead,
		lastPageRead: c.lastPageRead,
		bookmarked: c.isBookmarked,
		isDownloaded: c.isDownloaded,
	};
}

export function createLocalSuwayomiBackend(): ContentBackend {
	return new LocalSuwayomiBackend();
}
