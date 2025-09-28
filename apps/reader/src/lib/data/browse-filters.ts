/**
 * Browse v2's state model — PURE, so it can be tested under `node --test` and reasoned
 * about without a browser.
 *
 * THE URL IS THE STATE. `/browse` is public, edge-SSR'd and shareable, so every filter,
 * the sort and the page live in the query string: a reload, a pasted link and the
 * browser's Back button must all land on the same result set. Everything here exists to
 * make that round trip safe and canonical:
 *
 *  - {@link parseBrowseUrl} is TOLERANT. The query string is user-editable, so an unknown
 *    value is dropped rather than rejected, lists are deduped and bounded, and numbers
 *    are clamped — a hand-edited link can never become a request the server refuses, or
 *    an unbounded `IN (…)`. It also maps every param the PREVIOUS Browse page wrote
 *    (`genre`, capitalised `type`, single `status`, the old `sort` words, `content`,
 *    `chapters=1`, `minRating`), so links already shared — and the home page's genre
 *    links — keep working.
 *  - {@link toSearchParams} is CANONICAL: defaults are written as ABSENCE and every list
 *    in one fixed order, so one result set has exactly one URL. That is not cosmetic —
 *    the edge cache and the back-nav cache both key on it, and two spellings of the same
 *    filter would be two cache entries and two crawlable addresses.
 *
 * Every mutator returns a NEW normalised state with the page reset to 1: a different
 * filter is a different result set, and page 7 of the old one is meaningless.
 */
import type {
	BrowseFilter,
	BrowseSort,
	BrowseSortField,
	ContentRating,
	ContentRatingFilter,
	Demographic,
	SearchFilters,
	SortDirection,
	TagMode,
} from '@komika/api';
import type { ComicType as WireComicType, SeriesStatus, TagGroup, TagInfo } from '@komika/types';
import { pageParam } from './paging.ts';

// ---- vocabulary --------------------------------------------------------------

/** `UNKNOWN` is a real wire value but not something a reader asks for. */
export type BrowseStatus = Exclude<SeriesStatus, 'UNKNOWN'>;
/** The three formats the reader distinguishes; the server folds WEBTOON→MANHWA and
 *  COMIC→MANGA, so offering those would be two chips for one result set. */
export type BrowseType = Extract<WireComicType, 'MANGA' | 'MANHWA' | 'MANHUA'>;

export const SORT_FIELDS: readonly BrowseSortField[] = [
	'RELEVANCE',
	'LATEST_UPDATE',
	'RECENTLY_ADDED',
	'MOST_VIEWED',
	'MOST_FOLLOWS',
	'HIGHEST_RATED',
	'MOST_CHAPTERS',
	'TITLE',
	'YEAR',
];
export const SORT_LABEL: Record<BrowseSortField, string> = {
	RELEVANCE: 'Relevance',
	LATEST_UPDATE: 'Latest update',
	RECENTLY_ADDED: 'Recently added',
	MOST_VIEWED: 'Most viewed',
	MOST_FOLLOWS: 'Most follows',
	HIGHEST_RATED: 'Highest rated',
	MOST_CHAPTERS: 'Most chapters',
	TITLE: 'Title A–Z',
	YEAR: 'Year',
};
/** The URL spelling of each sort. Short, readable, and a SUPERSET of the old page's
 *  words where the meaning is the same (`added`, `rating`, `chapters`). */
const SORT_TOKEN: Record<BrowseSortField, string> = {
	RELEVANCE: 'relevance',
	LATEST_UPDATE: 'latest',
	RECENTLY_ADDED: 'added',
	MOST_VIEWED: 'views',
	MOST_FOLLOWS: 'follows',
	HIGHEST_RATED: 'rating',
	MOST_CHAPTERS: 'chapters',
	TITLE: 'title',
	YEAR: 'year',
};
/**
 * The previous page's sort words that DON'T coincide with a new token. `trending` was
 * the old default ("most viewed in 24h, then newest"), which is what MOST_VIEWED is;
 * `newest` ordered by newest chapter release, which is LATEST_UPDATE.
 */
const LEGACY_SORT_TOKEN: Record<string, BrowseSortField> = {
	trending: 'MOST_VIEWED',
	newest: 'LATEST_UPDATE',
};

export const STATUSES: readonly BrowseStatus[] = ['ONGOING', 'COMPLETED', 'HIATUS', 'CANCELLED'];
export const STATUS_LABEL: Record<BrowseStatus, string> = {
	ONGOING: 'Ongoing',
	COMPLETED: 'Completed',
	HIATUS: 'Hiatus',
	CANCELLED: 'Cancelled',
};
export const TYPES: readonly BrowseType[] = ['MANGA', 'MANHWA', 'MANHUA'];
export const TYPE_LABEL: Record<BrowseType, string> = {
	MANGA: 'Manga',
	MANHWA: 'Manhwa',
	MANHUA: 'Manhua',
};
/** Aliases the server folds anyway (see {@link BrowseType}). */
const TYPE_ALIAS: Record<string, BrowseType> = { webtoon: 'MANHWA', comic: 'MANGA' };

export const DEMOGRAPHICS: readonly Demographic[] = [
	'SHOUNEN',
	'SHOUJO',
	'SEINEN',
	'JOSEI',
	'NONE',
];
export const DEMOGRAPHIC_LABEL: Record<Demographic, string> = {
	SHOUNEN: 'Shounen',
	SHOUJO: 'Shoujo',
	SEINEN: 'Seinen',
	JOSEI: 'Josei',
	NONE: 'None',
};

export const CONTENT_RATINGS: readonly ContentRating[] = [
	'SAFE',
	'SUGGESTIVE',
	'EROTICA',
	'PORNOGRAPHIC',
];
export const CONTENT_RATING_LABEL: Record<ContentRating, string> = {
	SAFE: 'Safe',
	SUGGESTIVE: 'Suggestive',
	EROTICA: 'Erotica',
	PORNOGRAPHIC: 'Pornographic',
};
/**
 * The ratings a viewer WITHOUT the NSFW opt-in can usefully ask for. The server never
 * widens past the viewer's gate, so for them EROTICA/PORNOGRAPHIC would be a guaranteed
 * empty grid, not a way in — the opt-in lives on the profile screen.
 */
export const SFW_CONTENT_RATINGS: readonly ContentRating[] = ['SAFE', 'SUGGESTIVE'];

/**
 * Original languages the picker offers, in the codes MangaDex reports. A URL may carry
 * any well-formed code (see {@link LANG_RE}); this is only the curated shortlist plus
 * the labels for chips.
 */
export const LANGUAGES: readonly { code: string; label: string }[] = [
	{ code: 'ja', label: 'Japanese' },
	{ code: 'ko', label: 'Korean' },
	{ code: 'zh', label: 'Chinese' },
	{ code: 'zh-hk', label: 'Cantonese' },
	{ code: 'en', label: 'English' },
	{ code: 'id', label: 'Indonesian' },
	{ code: 'th', label: 'Thai' },
	{ code: 'vi', label: 'Vietnamese' },
	{ code: 'es', label: 'Spanish' },
	{ code: 'fr', label: 'French' },
	{ code: 'de', label: 'German' },
	{ code: 'it', label: 'Italian' },
	{ code: 'pt-br', label: 'Portuguese (BR)' },
	{ code: 'ru', label: 'Russian' },
];
const LANGUAGE_LABEL = new Map(LANGUAGES.map((l) => [l.code, l.label]));

export const TAG_GROUPS: readonly TagGroup[] = ['GENRE', 'THEME', 'FORMAT', 'CONTENT'];
export const TAG_GROUP_LABEL: Record<TagGroup, string> = {
	GENRE: 'Genre',
	THEME: 'Theme',
	FORMAT: 'Format',
	CONTENT: 'Content',
};

// ---- bounds (mirror the server's; see the BrowseFilter contract) --------------

export const MAX_TAGS = 32;
export const MAX_LANGUAGES = 16;
export const YEAR_MIN = 1900;
export const YEAR_MAX = 2100;
/** Sanity ceilings, not real bounds — they exist so `?minch=1e300` can't reach SQL. */
export const MAX_MIN_CHAPTERS = 100_000;
export const MAX_MIN_FOLLOWS = 10_000_000;
export const MAX_QUERY_LENGTH = 200;

const TAG_ID_RE = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const TAG_ID_MAX = 64;
/** `ja`, `zh-hk`, `pt-br`, `es-la` — ISO-ish, lowercase, as MangaDex reports them. */
const LANG_RE = /^[a-z]{2,3}(?:-[a-z0-9]{2,8})?$/;

// ---- the state ---------------------------------------------------------------

export interface BrowseState {
	/** Trimmed search text; '' browses the whole catalogue. */
	q: string;
	/** `null` = the default for the query ({@link defaultSort}). Kept null rather than
	 *  resolved so typing a query can switch the implicit default to Relevance while an
	 *  explicit choice ("Most follows") survives it. */
	sort: BrowseSortField | null;
	/** `null` = the default for the effective sort ({@link defaultDir}). */
	dir: SortDirection | null;
	includedTags: string[];
	excludedTags: string[];
	inclMode: TagMode;
	exclMode: TagMode;
	status: BrowseStatus[];
	types: BrowseType[];
	demographics: Demographic[];
	contentRatings: ContentRating[];
	languages: string[];
	yearFrom: number | null;
	yearTo: number | null;
	minChapters: number | null;
	minRating: number | null;
	minFollows: number | null;
	hasChapters: boolean;
	page: number;
}

export const DEFAULT_INCL_MODE: TagMode = 'AND';
export const DEFAULT_EXCL_MODE: TagMode = 'OR';

export function emptyState(): BrowseState {
	return {
		q: '',
		sort: null,
		dir: null,
		includedTags: [],
		excludedTags: [],
		inclMode: DEFAULT_INCL_MODE,
		exclMode: DEFAULT_EXCL_MODE,
		status: [],
		types: [],
		demographics: [],
		contentRatings: [],
		languages: [],
		yearFrom: null,
		yearTo: null,
		minChapters: null,
		minRating: null,
		minFollows: null,
		hasChapters: false,
		page: 1,
	};
}

// ---- sort --------------------------------------------------------------------

/** Relevance with a query, newest chapter release without one (the contract's default). */
export function defaultSort(q: string): BrowseSortField {
	return q.trim() ? 'RELEVANCE' : 'LATEST_UPDATE';
}

/**
 * The sort actually requested. RELEVANCE without a query has nothing to rank by, so it
 * falls back exactly as the server would — the dropdown must never claim an order the
 * results don't have.
 */
export function effectiveSort(s: Pick<BrowseState, 'q' | 'sort'>): BrowseSortField {
	if (s.sort == null) return defaultSort(s.q);
	if (s.sort === 'RELEVANCE' && !s.q.trim()) return 'LATEST_UPDATE';
	return s.sort;
}

/** A–Z reads ascending; every other ordering is "most/newest first". */
export function defaultDir(sort: BrowseSortField): SortDirection {
	return sort === 'TITLE' ? 'ASC' : 'DESC';
}

export function effectiveDir(s: Pick<BrowseState, 'q' | 'sort' | 'dir'>): SortDirection {
	const sort = effectiveSort(s);
	// Relevance has one meaningful direction; "least relevant first" is not an order
	// anyone wants, so the toggle is disabled for it and a stray `?dir=asc` is ignored.
	if (sort === 'RELEVANCE') return 'DESC';
	return s.dir ?? defaultDir(sort);
}

/** The options the dropdown offers: Relevance only when there is a query to rank. */
export function sortOptions(q: string): BrowseSortField[] {
	return SORT_FIELDS.filter((f) => f !== 'RELEVANCE' || q.trim().length > 0);
}

/** Whether the direction toggle means anything for this sort. */
export function dirApplies(sort: BrowseSortField): boolean {
	return sort !== 'RELEVANCE';
}

/** Human wording for the direction toggle, per sort ("A → Z", "Newest first"). */
export function dirLabel(sort: BrowseSortField, dir: SortDirection): string {
	switch (sort) {
		case 'TITLE':
			return dir === 'ASC' ? 'A → Z' : 'Z → A';
		case 'YEAR':
		case 'LATEST_UPDATE':
		case 'RECENTLY_ADDED':
			return dir === 'DESC' ? 'Newest first' : 'Oldest first';
		case 'RELEVANCE':
			return 'Best match first';
		default:
			return dir === 'DESC' ? 'Highest first' : 'Lowest first';
	}
}

// ---- normalisation -----------------------------------------------------------

function uniq<T>(xs: readonly T[]): T[] {
	return [...new Set(xs)];
}
/** Dedupe and put an enum list in its declared order (canonical URL, stable cache key). */
function inOrder<T>(xs: readonly T[], order: readonly T[]): T[] {
	const set = new Set(xs);
	return order.filter((v) => set.has(v));
}
function clampInt(n: number | null, lo: number, hi: number): number | null {
	if (n == null || !Number.isFinite(n)) return null;
	return Math.min(hi, Math.max(lo, Math.floor(n)));
}

export function isTagId(id: string): boolean {
	return id.length <= TAG_ID_MAX && TAG_ID_RE.test(id);
}

/**
 * Bring any state into canonical form: bounded, deduped, ordered, defaults as null.
 * Idempotent; every mutator and both URL directions go through it, so two states that
 * mean the same thing are structurally equal.
 */
export function normalize(s: BrowseState): BrowseState {
	const q = s.q.trim().slice(0, MAX_QUERY_LENGTH);
	const includedTags = uniq(s.includedTags.filter(isTagId)).sort().slice(0, MAX_TAGS);
	// A tag can't be both. Include wins: it is the more specific request, and the URL
	// order that produced the conflict carries no intent worth honouring.
	const inc = new Set(includedTags);
	const excludedTags = uniq(s.excludedTags.filter((t) => isTagId(t) && !inc.has(t)))
		.sort()
		.slice(0, MAX_TAGS);

	let yearFrom = clampInt(s.yearFrom, YEAR_MIN, YEAR_MAX);
	let yearTo = clampInt(s.yearTo, YEAR_MIN, YEAR_MAX);
	// A reversed range is a typo, not a request for nothing.
	if (yearFrom != null && yearTo != null && yearFrom > yearTo)
		[yearFrom, yearTo] = [yearTo, yearFrom];

	// 0 is "no minimum" for every lower bound, so it is stored (and written) as absence.
	const minChapters = clampInt(s.minChapters, 0, MAX_MIN_CHAPTERS) || null;
	const minFollows = clampInt(s.minFollows, 0, MAX_MIN_FOLLOWS) || null;
	let minRating: number | null = null;
	if (s.minRating != null && Number.isFinite(s.minRating)) {
		// One decimal: the scale is 0–10 and anything finer is noise in a URL.
		minRating = Math.round(Math.min(10, Math.max(0, s.minRating)) * 10) / 10 || null;
	}

	let sort = s.sort && SORT_FIELDS.includes(s.sort) ? s.sort : null;
	if (sort === 'RELEVANCE' && !q) sort = null; // nothing to rank: fall back
	if (sort === defaultSort(q)) sort = null;
	const eff = effectiveSort({ q, sort });
	let dir = s.dir === 'ASC' || s.dir === 'DESC' ? s.dir : null;
	if (eff === 'RELEVANCE' || dir === defaultDir(eff)) dir = null;

	return {
		q,
		sort,
		dir,
		includedTags,
		excludedTags,
		inclMode: s.inclMode === 'OR' ? 'OR' : 'AND',
		exclMode: s.exclMode === 'AND' ? 'AND' : 'OR',
		status: inOrder(s.status, STATUSES),
		types: inOrder(s.types, TYPES),
		demographics: inOrder(s.demographics, DEMOGRAPHICS),
		contentRatings: inOrder(s.contentRatings, CONTENT_RATINGS),
		languages: uniq(s.languages.map((l) => l.toLowerCase()).filter((l) => LANG_RE.test(l)))
			.sort()
			.slice(0, MAX_LANGUAGES),
		yearFrom,
		yearTo,
		minChapters,
		minRating,
		minFollows,
		hasChapters: s.hasChapters === true,
		page: Number.isInteger(s.page) && s.page >= 1 && s.page <= 100_000 ? s.page : 1,
	};
}

// ---- URL ⇄ state ---------------------------------------------------------------

/** Every param this page reads or writes, legacy spellings included. */
export const BROWSE_PARAMS = [
	'q',
	'sort',
	'dir',
	'tag',
	'tagmode',
	'xtag',
	'xtagmode',
	'genre',
	'status',
	'type',
	'demo',
	'content',
	'lang',
	'from',
	'to',
	'minch',
	'minrating',
	'minRating',
	'minfollows',
	'chapters',
	'page',
] as const;

/**
 * The slug a tag id is derived from its display name with — used to map the legacy
 * `?genre=Boys' Love` links (and the genre-facet fallback) onto tag ids. Mirrors the
 * contract's examples: "Boys' Love" → `boys-love`, "Sci-Fi" → `sci-fi`, "4-Koma" →
 * `4-koma`.
 *
 * MUST be the server's `tags::slug` (apps/server/src/tags.rs) rule exactly, because the
 * result is compared against `work_tag.tag_key`, which the server writes: every run of
 * characters that is not an ASCII letter or digit is ONE separator — accented letters and
 * inner apostrophes included ("Pokémon" → `pok-mon`, "Doesn't" → `doesn-t`). Folding
 * diacritics or deleting apostrophes here would mint ids the server never stores, and a
 * legacy link would filter the grid to nothing.
 *
 * The one case it cannot mirror is a name with NO ASCII alphanumerics ("恋愛"): the server
 * keys those by a SHA-256 prefix, which needs async `crypto.subtle` here. Those return ''
 * and the caller drops them — the filter widens rather than silently emptying the grid.
 */
export function tagSlug(name: string): string {
	// Separators BEFORE lowercasing: `toLowerCase` maps a few non-ASCII letters onto ASCII
	// ("İ" → "i̇", the Kelvin sign → "k"), where Rust's `is_ascii_alphanumeric` sees them
	// as separators.
	return name
		.replace(/[^A-Za-z0-9]+/g, '-')
		.toLowerCase()
		.replace(/^-+|-+$/g, '')
		.slice(0, TAG_ID_MAX);
}

/** Read a numeric param; '' / junk → null. */
function num(raw: string | null): number | null {
	if (raw == null || raw.trim() === '') return null;
	const n = Number(raw);
	return Number.isFinite(n) ? n : null;
}

/** A capped `getAll`, so a link with ten thousand `tag=` params can't cost ten
 *  thousand regex tests before the bound in {@link normalize} applies. */
function many(sp: URLSearchParams, key: string, cap = 64): string[] {
	return sp.getAll(key).slice(0, cap);
}

function pickEnum<T extends string>(raws: string[], order: readonly T[]): T[] {
	return raws.map((r) => r.trim().toUpperCase()).filter((r): r is T => order.includes(r as T));
}

export function parseBrowseUrl(sp: URLSearchParams): BrowseState {
	const q = sp.get('q') ?? '';

	const sortRaw = (sp.get('sort') ?? '').trim().toLowerCase();
	const sort =
		(Object.keys(SORT_TOKEN) as BrowseSortField[]).find((k) => SORT_TOKEN[k] === sortRaw) ??
		LEGACY_SORT_TOKEN[sortRaw] ??
		null;
	const dirRaw = (sp.get('dir') ?? '').trim().toLowerCase();
	const dir: SortDirection | null = dirRaw === 'asc' ? 'ASC' : dirRaw === 'desc' ? 'DESC' : null;

	// `genre` is the OLD page's include list, by display name (`?genre=Action` from the
	// home page). Slugged onto the tag-id space it now shares with `tag`.
	const includedTags = [
		...many(sp, 'tag').map((t) => t.trim().toLowerCase()),
		...many(sp, 'genre').map(tagSlug),
	];
	const excludedTags = many(sp, 'xtag').map((t) => t.trim().toLowerCase());

	const types = many(sp, 'type')
		.map((t) => t.trim().toLowerCase())
		.map((t) => TYPE_ALIAS[t] ?? t.toUpperCase())
		.filter((t): t is BrowseType => TYPES.includes(t as BrowseType));

	// `content` is also the old page's single-valued tier: `safe` (the SAFE ceiling —
	// which admits safe only, the same set) and `nsfw_only` (adult works only). `all` is
	// the absence of a filter and falls out of the enum match below.
	const contentRatings: ContentRating[] = [];
	for (const raw of many(sp, 'content')) {
		const v = raw.trim().toUpperCase();
		if (v === 'NSFW_ONLY') contentRatings.push('EROTICA', 'PORNOGRAPHIC');
		else if (CONTENT_RATINGS.includes(v as ContentRating)) contentRatings.push(v as ContentRating);
	}

	const tagmode = (sp.get('tagmode') ?? '').trim().toUpperCase();
	const xtagmode = (sp.get('xtagmode') ?? '').trim().toUpperCase();

	return normalize({
		q,
		sort,
		dir,
		includedTags,
		excludedTags,
		inclMode: tagmode === 'OR' ? 'OR' : 'AND',
		exclMode: xtagmode === 'AND' ? 'AND' : 'OR',
		status: pickEnum(many(sp, 'status'), STATUSES),
		types,
		demographics: pickEnum(many(sp, 'demo'), DEMOGRAPHICS),
		contentRatings,
		languages: many(sp, 'lang'),
		yearFrom: num(sp.get('from')),
		yearTo: num(sp.get('to')),
		minChapters: num(sp.get('minch')),
		// `minRating` is the old page's spelling. It filtered on local review averages and
		// was hidden; the new scale is MangaDex's bayesian rating, same 0–10 range.
		minRating: num(sp.get('minrating') ?? sp.get('minRating')),
		minFollows: num(sp.get('minfollows')),
		// The old page wrote the bare presence `chapters=1`; anything else reads as off.
		hasChapters: sp.get('chapters') === '1',
		page: pageParam(sp),
	});
}

/**
 * The canonical query string for a state: defaults omitted, lists in canonical order,
 * params in one fixed order. `toSearchParams(parseBrowseUrl(x))` is a fixed point.
 */
export function toSearchParams(state: BrowseState): URLSearchParams {
	const s = normalize(state);
	const sp = new URLSearchParams();
	if (s.q) sp.set('q', s.q);
	if (s.sort) sp.set('sort', SORT_TOKEN[s.sort]);
	if (s.dir) sp.set('dir', s.dir.toLowerCase());
	for (const t of s.includedTags) sp.append('tag', t);
	if (s.includedTags.length && s.inclMode !== DEFAULT_INCL_MODE) sp.set('tagmode', 'or');
	for (const t of s.excludedTags) sp.append('xtag', t);
	if (s.excludedTags.length && s.exclMode !== DEFAULT_EXCL_MODE) sp.set('xtagmode', 'and');
	for (const v of s.status) sp.append('status', v.toLowerCase());
	for (const v of s.types) sp.append('type', v.toLowerCase());
	for (const v of s.demographics) sp.append('demo', v.toLowerCase());
	for (const v of s.contentRatings) sp.append('content', v.toLowerCase());
	for (const v of s.languages) sp.append('lang', v);
	if (s.yearFrom != null) sp.set('from', String(s.yearFrom));
	if (s.yearTo != null) sp.set('to', String(s.yearTo));
	if (s.minChapters != null) sp.set('minch', String(s.minChapters));
	if (s.minRating != null) sp.set('minrating', String(s.minRating));
	if (s.minFollows != null) sp.set('minfollows', String(s.minFollows));
	if (s.hasChapters) sp.set('chapters', '1');
	if (s.page > 1) sp.set('page', String(s.page));
	return sp;
}

/** `/browse?…` for a state (no `?` when everything is default). */
export function browseHref(pathname: string, state: BrowseState): string {
	const qs = toSearchParams(state).toString();
	return qs ? `${pathname}?${qs}` : pathname;
}

/**
 * The signature of everything that changes the result set EXCEPT the page — the
 * filter half of the back-nav cache key, and how "did the filters change?" is asked.
 */
export function filterSig(state: BrowseState): string {
	return toSearchParams({ ...state, page: 1 }).toString();
}

/**
 * The signature of the FILTERS alone — no query, sort or page. The filter panel edits a
 * draft of exactly this part, and re-bases the draft only when THIS changes: typing in
 * the search box (which rewrites `q`) must not wipe edits pending in the panel.
 */
export function filtersOnlySig(state: BrowseState): string {
	return filterSig({ ...state, q: '', sort: null, dir: null });
}

/**
 * The state the panel's Apply produces: the draft's filters under the CURRENT query and
 * sort (the draft's copies of those may be stale — see {@link filtersOnlySig}), page 1.
 */
export function applyDraft(applied: BrowseState, draft: BrowseState, q: string): BrowseState {
	return normalize({ ...draft, q, sort: applied.sort, dir: applied.dir, page: 1 });
}

/** How many filter clauses the draft would add, remove or change — the Apply button's
 *  "N changes". Tag modes count only where they qualify at least one tag. */
export function draftChanges(applied: BrowseState, draft: BrowseState): number {
	const a = new Set(activeChips(applied, () => undefined).map((c) => c.key + '|' + c.label));
	const d = new Set(activeChips(draft, () => undefined).map((c) => c.key + '|' + c.label));
	let n = 0;
	for (const k of a) if (!d.has(k)) n++;
	for (const k of d) if (!a.has(k)) n++;
	const A = normalize(applied);
	const D = normalize(draft);
	if (A.inclMode !== D.inclMode && A.includedTags.length && D.includedTags.length) n++;
	if (A.exclMode !== D.exclMode && A.excludedTags.length && D.excludedTags.length) n++;
	return n;
}

/** Whether two states request the same results (page included). */
export function sameState(a: BrowseState, b: BrowseState): boolean {
	return toSearchParams(a).toString() === toSearchParams(b).toString();
}

// ---- what the viewer is actually allowed / able to apply -----------------------

export interface ApplyContext {
	/** The viewer opted into NSFW (signed in + `showNsfw`). */
	nsfw: boolean;
	/**
	 * `'tags'` when the v2 tag vocabulary loaded; `'genres'` when the page fell back to
	 * the legacy genre facets (a server that predates `tags`). The fallback list is
	 * include-only and matches ANY — the only thing the legacy `genres` argument can say.
	 */
	tagSource: 'tags' | 'genres';
}

/**
 * The state with everything the viewer can't apply removed — the request, the chips
 * and the panel are all built from THIS, so nothing is shown as active that isn't being
 * sent. (The raw URL keeps the rest until the next write, so signing in to an NSFW
 * account still honours a shared `?content=erotica` link.)
 */
export function applicableState(state: BrowseState, ctx: ApplyContext): BrowseState {
	let s = state;
	if (!ctx.nsfw && s.contentRatings.some((r) => !SFW_CONTENT_RATINGS.includes(r))) {
		s = { ...s, contentRatings: s.contentRatings.filter((r) => SFW_CONTENT_RATINGS.includes(r)) };
	}
	if (ctx.tagSource === 'genres' && (s.excludedTags.length || s.inclMode !== 'OR')) {
		s = { ...s, excludedTags: [], inclMode: 'OR' };
	}
	return normalize(s);
}

// ---- state → wire ----------------------------------------------------------------

/**
 * The `BrowseFilter` input for a state. Absent keys are "no clause" — an empty list is
 * never sent, because `[]` reaches the resolver as an empty `IN (…)` and matches nothing.
 * The sort is always explicit (the server's default would agree, but an explicit
 * order makes the request self-describing in logs and cache keys).
 */
export function toFilterInput(state: BrowseState): BrowseFilter {
	const s = normalize(state);
	const f: BrowseFilter = {};
	if (s.includedTags.length) {
		f.includedTags = s.includedTags;
		f.includedTagsMode = s.inclMode;
	}
	if (s.excludedTags.length) {
		f.excludedTags = s.excludedTags;
		f.excludedTagsMode = s.exclMode;
	}
	if (s.status.length) f.status = s.status;
	if (s.types.length) f.types = s.types;
	if (s.demographics.length) f.demographics = s.demographics;
	if (s.contentRatings.length) f.contentRatings = s.contentRatings;
	if (s.languages.length) f.originalLanguages = s.languages;
	if (s.yearFrom != null) f.yearFrom = s.yearFrom;
	if (s.yearTo != null) f.yearTo = s.yearTo;
	if (s.minChapters != null) f.minChapters = s.minChapters;
	if (s.minRating != null) f.minRating = s.minRating;
	if (s.minFollows != null) f.minFollows = s.minFollows;
	if (s.hasChapters) f.hasChapters = true;
	f.sort = effectiveSort(s);
	if (dirApplies(f.sort)) f.direction = effectiveDir(s);
	return f;
}

const LEGACY_SORT: Partial<Record<BrowseSortField, BrowseSort>> = {
	LATEST_UPDATE: 'NEWEST',
	RECENTLY_ADDED: 'LATEST_ADDED',
	MOST_VIEWED: 'TRENDING',
	HIGHEST_RATED: 'RATING',
	MOST_CHAPTERS: 'CHAPTERS',
};
const RATING_LADDER: readonly ContentRating[] = CONTENT_RATINGS;

/** The cumulative legacy ceiling equal to a rating SET, or undefined if none is. */
function legacyContentRating(rs: readonly ContentRating[]): ContentRatingFilter | undefined {
	if (!rs.length) return undefined;
	// A prefix of the ladder IS a ceiling: {safe, suggestive} = SUGGESTIVE.
	const prefix = RATING_LADDER.slice(0, rs.length);
	if (prefix.every((r) => rs.includes(r))) return prefix[prefix.length - 1];
	if (rs.length === 2 && rs.includes('EROTICA') && rs.includes('PORNOGRAPHIC')) return 'NSFW_ONLY';
	return undefined;
}

/**
 * The legacy `search` arguments carrying as much of a state as they can express.
 *
 * Sent ALONGSIDE `filter` on every request. A current server ignores them (`filter`
 * wins); a server that predates `filter` strips it (OPTIONAL_ARGUMENTS) and applies
 * these instead. Everything here is a SUPERSET of the v2 filter's result set — `genres`
 * is ANY where the filter may say ALL, an inexpressible clause is omitted — so even a
 * server that applied both could only ever agree with the filter.
 *
 * `tagName` maps a tag id to the display name the legacy `genres` argument matches on.
 */
export function toLegacyFilters(
	state: BrowseState,
	tagName: (id: string) => string,
): Omit<SearchFilters, 'filter'> {
	const s = normalize(state);
	const out: Omit<SearchFilters, 'filter'> = {};
	// The legacy argument is capped at 12 server-side.
	if (s.includedTags.length) out.genres = s.includedTags.slice(0, 12).map(tagName);
	if (s.types.length) out.types = s.types;
	if (s.status.length === 1) out.status = s.status[0];
	const sort = effectiveSort(s);
	if (LEGACY_SORT[sort] && effectiveDir(s) === 'DESC') out.sort = LEGACY_SORT[sort];
	const cr = legacyContentRating(s.contentRatings);
	if (cr) out.contentRating = cr;
	if (s.hasChapters) out.hasChapters = true;
	return out;
}

/**
 * The labels of the filters a LEGACY server (one that stripped `filter`) did not apply,
 * so the page can say so instead of presenting a wider result set under active chips.
 * Mirrors {@link toLegacyFilters}, plus the old server's rule that everything except
 * `genres` is ignored for a text query.
 */
export function legacyUnapplied(state: BrowseState): string[] {
	const s = normalize(state);
	const q = !!s.q;
	const out: string[] = [];
	if (s.includedTags.length > 12 || (s.includedTags.length > 1 && s.inclMode === 'AND'))
		out.push('Match all tags');
	if (s.excludedTags.length) out.push('Excluded tags');
	if (s.status.length && (q || s.status.length > 1)) out.push('Status');
	if (s.types.length && q) out.push('Format');
	if (s.demographics.length) out.push('Demographic');
	if (s.contentRatings.length && (q || !legacyContentRating(s.contentRatings)))
		out.push('Content rating');
	if (s.languages.length) out.push('Original language');
	if (s.yearFrom != null || s.yearTo != null) out.push('Year');
	if (s.minChapters != null) out.push('Min chapters');
	if (s.minRating != null) out.push('Min rating');
	if (s.minFollows != null) out.push('Min follows');
	if (s.hasChapters && q) out.push('Has readable chapters');
	const sort = effectiveSort(s);
	const sortOk = q ? sort === 'RELEVANCE' : !!LEGACY_SORT[sort] && effectiveDir(s) === 'DESC';
	if (!sortOk) out.push('Sort');
	return out;
}

// ---- mutators (all pure; all reset the page) -------------------------------------

export type TagState = 'include' | 'exclude' | 'off';

export function tagState(s: BrowseState, id: string): TagState {
	if (s.includedTags.includes(id)) return 'include';
	if (s.excludedTags.includes(id)) return 'exclude';
	return 'off';
}

/** Put a tag in exactly one of include / exclude / neither. A full list (MAX_TAGS)
 *  refuses the move and returns the state unchanged rather than silently evicting. */
export function setTagState(s: BrowseState, id: string, next: TagState): BrowseState {
	if (!isTagId(id)) return s;
	const inc = s.includedTags.filter((t) => t !== id);
	const exc = s.excludedTags.filter((t) => t !== id);
	if (next === 'include') {
		if (inc.length >= MAX_TAGS) return s;
		inc.push(id);
	} else if (next === 'exclude') {
		if (exc.length >= MAX_TAGS) return s;
		exc.push(id);
	}
	return normalize({ ...s, includedTags: inc, excludedTags: exc, page: 1 });
}

/**
 * One tap on a tag: off → include → exclude → off. When the next list is full the cycle
 * skips past it (off → exclude when include is full; include → off when exclude is), so
 * a tap always does SOMETHING visible rather than appearing broken.
 */
export function cycleTag(s: BrowseState, id: string): BrowseState {
	const order: TagState[] = ['off', 'include', 'exclude'];
	const cur = tagState(s, id);
	for (let step = 1; step <= 2; step++) {
		const next = order[(order.indexOf(cur) + step) % 3];
		const out = setTagState(s, id, next);
		if (out !== s) return out;
	}
	return s;
}

export function withQuery(s: BrowseState, q: string): BrowseState {
	return normalize({ ...s, q, page: 1 });
}
export function withSort(s: BrowseState, sort: BrowseSortField): BrowseState {
	// A new sort starts from ITS natural direction (A–Z for title, most-first for counts):
	// carrying "ascending" from Most follows into Title would silently mean Z–A.
	return normalize({ ...s, sort, dir: null, page: 1 });
}
export function withDir(s: BrowseState, dir: SortDirection): BrowseState {
	return normalize({ ...s, sort: effectiveSort(s), dir, page: 1 });
}
export function toggleDir(s: BrowseState): BrowseState {
	return withDir(s, effectiveDir(s) === 'ASC' ? 'DESC' : 'ASC');
}
export function withPageNum(s: BrowseState, page: number): BrowseState {
	return normalize({ ...s, page });
}

type ListKey = 'status' | 'types' | 'demographics' | 'contentRatings' | 'languages';
type ListValue<K extends ListKey> = BrowseState[K][number];

/** Toggle one value of a multi-select facet. */
export function toggleIn<K extends ListKey>(
	s: BrowseState,
	key: K,
	value: ListValue<K>,
): BrowseState {
	const cur = s[key] as ListValue<K>[];
	const next = cur.includes(value) ? cur.filter((v) => v !== value) : [...cur, value];
	return normalize({ ...s, [key]: next, page: 1 });
}

type ScalarKey =
	| 'inclMode'
	| 'exclMode'
	| 'yearFrom'
	| 'yearTo'
	| 'minChapters'
	| 'minRating'
	| 'minFollows'
	| 'hasChapters';

export function withField<K extends ScalarKey>(
	s: BrowseState,
	key: K,
	value: BrowseState[K],
): BrowseState {
	return normalize({ ...s, [key]: value, page: 1 });
}

/** Every FILTER back to default; the query and the chosen sort survive — "clear
 *  filters" is not "start over". */
export function clearFilters(s: BrowseState): BrowseState {
	return normalize({ ...emptyState(), q: s.q, sort: s.sort, dir: s.dir });
}

/** How many filter clauses are active (the Filters button's badge). */
export function activeFilterCount(s: BrowseState): number {
	return activeChips(s, () => '').length;
}

// ---- chips ---------------------------------------------------------------------

export interface FilterChip {
	/** Unique across the row (Svelte keys on it; a duplicate key throws in production). */
	key: string;
	label: string;
	/** `exclude` chips render as a negation. */
	tone: 'include' | 'exclude' | 'neutral';
	/** Screen-reader name of the remove button. */
	ariaLabel: string;
	/** The state without this chip (page reset). Pure: the caller decides how to apply. */
	remove: () => BrowseState;
}

export function languageLabel(code: string): string {
	return LANGUAGE_LABEL.get(code) ?? code.toUpperCase();
}

/** A tag's display name, or a readable guess from its slug while the vocabulary loads
 *  (or for an id the server no longer knows) — never a blank chip. */
export function tagLabel(id: string, names: (id: string) => string | undefined): string {
	const n = names(id);
	if (n) return n;
	return id
		.split('-')
		.map((w) => (w ? w[0].toUpperCase() + w.slice(1) : w))
		.join(' ');
}

const fmtInt = (n: number) => n.toLocaleString('en-US');

/**
 * The active-filter chip row, in panel order. Sort and query are NOT chips — they have
 * their own controls in the header — and neither are the tag MODES on their own (they
 * qualify the tag chips; "Clear all" resets them).
 */
export function activeChips(
	state: BrowseState,
	names: (id: string) => string | undefined,
): FilterChip[] {
	const s = normalize(state);
	const chips: FilterChip[] = [];
	const push = (
		key: string,
		label: string,
		tone: FilterChip['tone'],
		ariaLabel: string,
		next: () => BrowseState,
	) => chips.push({ key, label, tone, ariaLabel, remove: next });

	for (const id of s.includedTags) {
		const name = tagLabel(id, names);
		push(`tag+:${id}`, name, 'include', `Remove tag ${name}`, () => setTagState(s, id, 'off'));
	}
	for (const id of s.excludedTags) {
		const name = tagLabel(id, names);
		push(`tag-:${id}`, name, 'exclude', `Remove excluded tag ${name}`, () =>
			setTagState(s, id, 'off'),
		);
	}
	for (const v of s.status)
		push(`status:${v}`, STATUS_LABEL[v], 'neutral', `Remove status ${STATUS_LABEL[v]}`, () =>
			toggleIn(s, 'status', v),
		);
	for (const v of s.types)
		push(`type:${v}`, TYPE_LABEL[v], 'neutral', `Remove format ${TYPE_LABEL[v]}`, () =>
			toggleIn(s, 'types', v),
		);
	for (const v of s.demographics) {
		const label = v === 'NONE' ? 'No demographic' : DEMOGRAPHIC_LABEL[v];
		push(`demo:${v}`, label, 'neutral', `Remove demographic ${label}`, () =>
			toggleIn(s, 'demographics', v),
		);
	}
	for (const v of s.contentRatings)
		push(
			`content:${v}`,
			CONTENT_RATING_LABEL[v],
			'neutral',
			`Remove content rating ${CONTENT_RATING_LABEL[v]}`,
			() => toggleIn(s, 'contentRatings', v),
		);
	for (const v of s.languages) {
		const label = languageLabel(v);
		push(`lang:${v}`, label, 'neutral', `Remove original language ${label}`, () =>
			toggleIn(s, 'languages', v),
		);
	}
	if (s.yearFrom != null || s.yearTo != null) {
		const label =
			s.yearFrom != null && s.yearTo != null
				? s.yearFrom === s.yearTo
					? `Year ${s.yearFrom}`
					: `${s.yearFrom}–${s.yearTo}`
				: s.yearFrom != null
					? `From ${s.yearFrom}`
					: `Until ${s.yearTo}`;
		push('year', label, 'neutral', `Remove year filter ${label}`, () =>
			normalize({ ...s, yearFrom: null, yearTo: null, page: 1 }),
		);
	}
	if (s.minChapters != null) {
		const label = `${fmtInt(s.minChapters)}+ chapters`;
		push('minch', label, 'neutral', `Remove ${label}`, () => withField(s, 'minChapters', null));
	}
	if (s.minRating != null) {
		const label = `Rated ${s.minRating}+`;
		push('minrating', label, 'neutral', `Remove ${label}`, () => withField(s, 'minRating', null));
	}
	if (s.minFollows != null) {
		const label = `${fmtInt(s.minFollows)}+ follows`;
		push('minfollows', label, 'neutral', `Remove ${label}`, () => withField(s, 'minFollows', null));
	}
	if (s.hasChapters)
		push('chapters', 'Readable now', 'neutral', 'Remove has readable chapters', () =>
			withField(s, 'hasChapters', false),
		);
	return chips;
}

// ---- tag picker ----------------------------------------------------------------

/** Case- and accent-insensitive key for type-to-search. */
export function foldForSearch(s: string): string {
	return s.normalize('NFKD').replace(/[̀-ͯ]/g, '').toLowerCase().replace(/['’]/g, '');
}

export interface TagShelf {
	group: TagGroup;
	label: string;
	tags: TagInfo[];
}

/**
 * The picker's shelves: tags grouped in {@link TAG_GROUPS} order, A–Z within a group,
 * narrowed by `needle`. A tag that is already included/excluded stays visible whatever
 * the needle, so a filter can always be seen (and undone) where it was set. Empty
 * shelves are dropped.
 */
export function shelveTags(
	tags: readonly TagInfo[],
	needle: string,
	s: Pick<BrowseState, 'includedTags' | 'excludedTags'>,
): TagShelf[] {
	const n = foldForSearch(needle.trim());
	const keep = (t: TagInfo) =>
		!n ||
		foldForSearch(t.name).includes(n) ||
		s.includedTags.includes(t.id) ||
		s.excludedTags.includes(t.id);
	return TAG_GROUPS.map((group) => ({
		group,
		label: TAG_GROUP_LABEL[group],
		tags: tags
			.filter((t) => t.group === group && keep(t))
			.sort((a, b) => a.name.localeCompare(b.name, 'en')),
	})).filter((sh) => sh.tags.length > 0);
}

/**
 * The legacy genre facets as tags (a server that predates `tags`): GENRE shelf, ids
 * slugged from the names so the URL vocabulary is the same either way. Duplicate slugs
 * (two spellings of one genre) keep the first — the facets arrive most-common first.
 */
export function tagsFromGenreFacets(
	facets: readonly { genre: string; count: number }[],
): TagInfo[] {
	const seen = new Set<string>();
	const out: TagInfo[] = [];
	for (const f of facets) {
		const id = tagSlug(f.genre);
		if (!id || seen.has(id)) continue;
		seen.add(id);
		out.push({ id, name: f.genre, group: 'GENRE', count: f.count });
	}
	return out;
}

// ---- federated rows --------------------------------------------------------------

/**
 * Client-side filtering for the FEDERATED (external sources) rows, which come from live
 * extension searches that take no filter arguments. Only the facets those rows actually
 * carry can be honoured — tags by name, format, status; everything else is reported by
 * the page as not applying rather than pretended.
 */
export function matchesFederated(
	row: { genres: readonly string[]; type: string; status: string },
	state: BrowseState,
	names: (id: string) => string | undefined,
): boolean {
	const s = normalize(state);
	const have = new Set(row.genres.map((g) => tagSlug(g)));
	const want = (id: string) => have.has(id) || have.has(tagSlug(tagLabel(id, names)));
	if (s.includedTags.length) {
		const ok = s.inclMode === 'AND' ? s.includedTags.every(want) : s.includedTags.some(want);
		if (!ok) return false;
	}
	if (s.excludedTags.length) {
		const hit = s.exclMode === 'OR' ? s.excludedTags.some(want) : s.excludedTags.every(want);
		if (hit) return false;
	}
	if (s.types.length && !s.types.some((t) => TYPE_LABEL[t] === row.type)) return false;
	if (s.status.length && !s.status.some((v) => v.toLowerCase() === row.status)) return false;
	return true;
}

// ---- debounce ----------------------------------------------------------------------

export interface Debounced<A extends unknown[]> {
	(...args: A): void;
	/** Drop a pending call. */
	cancel(): void;
	/** Run a pending call now (no-op when none is pending). */
	flush(): void;
	readonly pending: boolean;
}

/** Trailing-edge debounce with `cancel`/`flush` — the search box uses `flush` on Enter
 *  so submitting doesn't wait out the timer. */
export function debounce<A extends unknown[]>(fn: (...args: A) => void, ms: number): Debounced<A> {
	let timer: ReturnType<typeof setTimeout> | null = null;
	let last: A | null = null;
	const run = () => {
		timer = null;
		const args = last;
		last = null;
		if (args) fn(...args);
	};
	const d = ((...args: A) => {
		last = args;
		if (timer) clearTimeout(timer);
		timer = setTimeout(run, ms);
	}) as Debounced<A>;
	d.cancel = () => {
		if (timer) clearTimeout(timer);
		timer = null;
		last = null;
	};
	d.flush = () => {
		if (!timer) return;
		clearTimeout(timer);
		run();
	};
	Object.defineProperty(d, 'pending', { get: () => timer !== null });
	return d;
}

// ---- numeric inputs -----------------------------------------------------------------

/** A number field's text → value: '' is "no filter" (null), junk is also null. */
export function parseNumberInput(raw: string): number | null {
	return num(raw);
}
