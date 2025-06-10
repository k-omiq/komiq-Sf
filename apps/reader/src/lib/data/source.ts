/**
 * The data repository the UI consumes (via SvelteKit `load` functions).
 *
 * Each getter returns the exact view shapes the screens render. When the backend
 * is enabled (`PUBLIC_KOMIKA_BACKEND=on`) it maps live `@komika/api` domain data
 * into those shapes. Getters never reject: when the backend is off, errors, or
 * returns nothing, they resolve to honest empty results (empty arrays / null) and
 * the screens render their empty states. No sample content is ever fabricated.
 */
import type {
	AggregatedChapter,
	CanonicalUpdate,
	Chapter,
	ComicType as DomainComicType,
	DiscoveryFeed,
	Page,
	Series,
	SeriesProgress,
	SeriesStatus,
	Translator,
	UpdateFeedRow,
	WorkSource,
} from '@komika/types';
// The two Browse enums are wire vocabulary with no reader-side equivalent (there is no
// "sort" or "content rating" in the view model), so they travel as the API's own tokens
// rather than being mirrored into a view type that would only ever be mapped 1:1.
import type { BrowseSort, ContentRatingFilter } from '@komika/api';
// Runtime platform probe — used ONLY to decide whether the speculative page fetch in
// `getReaderChapter` is safe (see `canStartPagesEarly`), never to branch view shape.
import { isTauri } from '@komika/api';
import { backend, images } from '$lib/context';
import { findChapterOwner } from './chapter-owner';
import type { ChapterCandidate } from './chapter-owner';
import { isRedundantMangadexExt, pickDefaultKey, MANGADEX_EXT_PKG } from './translator-select';
import { getPreferredTranslator, setPreferredTranslator } from './translator-pref.svelte';
import { config, apiAssetSrc } from '$lib/config';
import * as content from './content';
import { FLAG, FORMAT_CARDS } from './types';
import type { Card, CatalogEntry, ComicType, Shelf, Status } from './types';

const LIVE = config.backendEnabled;

/**
 * Whether a series id addresses a canonical (MangaDex-mirrored) `work` rather than a
 * numeric Suwayomi series. Canonical work ids carry a `w_` prefix (CATALOGUE.md §6),
 * so the reader routes them to the `canonical*` backend methods; Suwayomi ids are
 * numeric and stay on the untouched Suwayomi path.
 */
function isCanonicalId(id: string): boolean {
	return id.startsWith('w_');
}

/** Run a live mapping, resolving to the (empty) fallback on any failure. */
async function live<T>(fn: () => Promise<T>, fallback: T): Promise<T> {
	if (!LIVE) return fallback;
	try {
		return await fn();
	} catch (err) {
		console.warn('[komika] backend call failed:', err);
		return fallback;
	}
}

/**
 * Like {@link live} but tells a genuine backend FAILURE apart from an honest
 * empty result: returns `{ data, error }` where `error` is true only when the
 * mapping threw. Screens use this to show an error state (during an outage)
 * instead of a misleading "not found" / "no results" empty state. Mock mode
 * (`!LIVE`) is never an error — it's just empty.
 */
async function liveResult<T>(fn: () => Promise<T>, empty: T): Promise<{ data: T; error: boolean }> {
	if (!LIVE) return { data: empty, error: false };
	try {
		return { data: await fn(), error: false };
	} catch (err) {
		console.warn('[komika] backend call failed:', err);
		return { data: empty, error: true };
	}
}

/**
 * THE relative-time formatter. Every "3h" / "3h ago" label in the reader goes
 * through this one function so the thresholds, rounding and — critically — the
 * >30d overflow behave identically everywhere. (There used to be two copies:
 * the card one had no branch above 24h, so a two-year-old chapter rendered as
 * "730d" while the profile feed correctly said "March 2023".)
 *
 * `suffix` is the only intended difference between callers: cards are terse
 * ("Ch. 12 · 4h"), the activity feed reads as a sentence ("Reviewed X · 4h ago").
 * `empty` is what an absent/unparseable timestamp renders as.
 */
function formatRelative(
	iso: string | null | undefined,
	opts: { suffix?: boolean; empty: string },
): string {
	if (!iso) return opts.empty;
	const then = Date.parse(iso);
	if (Number.isNaN(then)) return opts.empty;
	// `Date.now()` is unavailable in some sandboxes; guard it.
	const now = typeof Date !== 'undefined' && Date.now ? Date.now() : then;
	const mins = Math.max(0, Math.round((now - then) / 60000));
	const ago = opts.suffix ? ' ago' : '';
	if (mins < 1) return opts.suffix ? 'just now' : 'now';
	if (mins < 60) return `${mins}m${ago}`;
	const hrs = Math.round(mins / 60);
	if (hrs < 24) return `${hrs}h${ago}`;
	const days = Math.round(hrs / 24);
	// Past a month a duration stops being informative — show the actual date.
	if (days < 30) return `${days}d${ago}`;
	return monthYear(iso);
}

/**
 * Coarse "N ago" relative time for the activity feed.
 *
 * An ABSENT/unparseable timestamp renders as '' (the same choice {@link relTime}
 * makes), never as "just now": we don't know when it happened, and "just now" is
 * an affirmative claim that it happened seconds ago. The one caller (the profile
 * activity list) omits the time line entirely when this is empty, so the row still
 * reads as a complete sentence ("Reviewed Berserk") instead of a lie.
 */
function relTimeAgo(iso: string | null | undefined): string {
	return formatRelative(iso, { suffix: true, empty: '' });
}

/** Terse relative time for cards and the series header ("4h", "3d", "March 2023"). */
function relTime(iso: string | null | undefined): string {
	return formatRelative(iso, { empty: '' });
}

/**
 * A timestamp as sortable epoch milliseconds, or `0` when it is absent or
 * unparseable. Pairs with {@link relTime}: whatever instant a card LABELS with, it
 * also SORTS by, so the two can never disagree.
 *
 * `0` rather than `NaN` or `-Infinity` because it has to survive a descending sort
 * as "oldest/unknown, goes last" — `NaN` makes every comparison false and leaves
 * the surrounding rows in arbitrary order.
 */
function epochMs(iso: string | null | undefined): number {
	if (!iso) return 0;
	const t = Date.parse(iso);
	return Number.isNaN(t) ? 0 : t;
}

/**
 * The timestamp a "recently updated" label must use: the real upstream publish
 * time of the newest chapter, falling back to `updatedAt` only when the backend
 * can't supply it.
 *
 * `updatedAt` is a metadata touch that moves on EVERY poll — the server documents
 * it as unusable for recency — so labelling with it made the feed say "0h" for a
 * series whose last chapter landed a week ago, while the series page (reading a
 * different clock) said something else entirely. That contradiction is the bug
 * this exists to close: every surface now reads the same clock.
 */
function chapterRecency(s: Series, chapters: readonly Chapter[] = []): string | null {
	return firstDated(newestUploadAt(chapters), s.latestChapterAt, s.updatedAt);
}

/**
 * The first candidate that is actually a usable date.
 *
 * A plain `a || b || c` only falls through on a FALSY value, so it handles a null
 * or empty `latestChapterAt` but silently commits to a non-empty unparseable one —
 * and then `formatRelative` renders '' with the good `updatedAt` never consulted.
 * That distinction is about to matter: `latestChapterAt` is documented as "empty
 * when no dated chapter is cached" and is being changed server-side to stop
 * stamping a bogus `now` on a series' first touch, so an unscanned series will
 * carry no chapter time at all. Validate each candidate instead of trusting
 * truthiness, so the chain always lands on the newest timestamp that can really
 * be formatted.
 */
function firstDated(...candidates: (string | null | undefined)[]): string | null {
	for (const c of candidates) {
		if (c && !Number.isNaN(Date.parse(c))) return c;
	}
	return null;
}

/**
 * The newest `uploadedAt` in a chapter list, or null when none is dated.
 *
 * Preferred OVER `latestChapterAt` by {@link chapterRecency} whenever a caller holds
 * the chapter list, because `latestChapterAt` is a denormalized copy of exactly this
 * value that is wrong on a series' first view. Measured against the live API:
 *
 *  • Warm series — `latestChapterAt` and `max(chapters.uploadedAt)` are IDENTICAL
 *    (ids 4140, 280, 16790, 1292, 9000, to the millisecond), so preferring the list
 *    changes nothing where the column is trustworthy. On id 500 the column was two
 *    weeks STALE (2026-06-27 vs the list's 2026-07-10), so the list is also better.
 *  • Cold series — the server stamps `latest_chapter_at = now` on first touch and
 *    only corrects it once chapters are cached. Probing ids 500/3000/9000/12345/
 *    16796 cold returned `latestChapterAt == updatedAt == <the query's own clock>`;
 *    a second probe returned their real historical dates. So the first GET of
 *    /series/1292 rendered "Updated now" — with the tooltip asserting "Newest
 *    chapter released now" — directly above a chapter list whose newest row read
 *    "13d", and the next GET rendered "13d" correctly.
 *
 * A page contradicting itself one line down is worse than a coarse label, and the
 * honest answer is already in hand: the very list we are about to render. Not read
 * off the end of the array — a source orders by chapter NUMBER and upload times are
 * not monotonic in it — so take the maximum.
 */
function newestUploadAt(chapters: readonly Chapter[]): string | null {
	let best: string | null = null;
	let bestT = -Infinity;
	for (const c of chapters) {
		if (!c.uploadedAt) continue;
		const t = Date.parse(c.uploadedAt);
		if (!Number.isNaN(t) && t > bestT) {
			bestT = t;
			best = c.uploadedAt;
		}
	}
	return best;
}

/** "March 2023"-style month/year for the profile "joined" line. */
function monthYear(iso: string | null | undefined): string {
	if (!iso) return 'recently';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return 'recently';
	return d.toLocaleDateString(undefined, { month: 'long', year: 'numeric' });
}

// ---- domain → view mappers -------------------------------------------------

function toViewType(t: DomainComicType): ComicType {
	if (t === 'MANHWA' || t === 'WEBTOON') return 'Manhwa';
	if (t === 'MANHUA') return 'Manhua';
	return 'Manga';
}

function toViewStatus(s: Series['status']): Status {
	if (s === 'COMPLETED') return 'completed';
	if (s === 'HIATUS') return 'hiatus';
	if (s === 'CANCELLED') return 'cancelled';
	return 'ongoing';
}

function toCard(s: Series): Card {
	const at = chapterRecency(s);
	return {
		title: s.title,
		ch: `Ch. ${s.chapterCount}`,
		// The RELEASE time, not our poll time — see {@link chapterRecency}.
		time: relTime(at),
		timeAt: epochMs(at),
		// Our discovery time, when the backend reports it. Rendered only as a
		// hover title so the card design is unchanged but the two clocks are
		// distinguishable ("released 7d · we found it 1h ago").
		detected: relTime(s.detectedAt),
		rating: s.rating.average.toFixed(1),
		cover: s.coverUrl,
		id: s.id,
		type: toViewType(s.type),
	};
}

/** Like {@link toCard} but the time is the CATALOGUE-ADD time (`createdAt`), not the
 *  chapter-release time — for the "Latest Added" row, so "Added 2d" is faithful.
 *  This row is deliberately on a different clock: it answers "when did this series
 *  enter the catalogue", not "when did its last chapter ship". */
function toAddedCard(s: Series): Card {
	// `timeAt` MUST be re-derived from `createdAt` alongside `time`. Spreading
	// `toCard(s)` and overriding only `time` would leave the chapter-release clock
	// behind in `timeAt`, so this row would sort by release time under a
	// catalogue-add label — the exact label/order mismatch this pairing exists to
	// prevent, just moved one field over.
	return { ...toCard(s), time: relTime(s.createdAt), timeAt: epochMs(s.createdAt), detected: '' };
}

/** A canonical-updates row → home/updates Card, linking by its `w_` workId so the
 *  card opens the MangaDex-mirrored work through the canonical reader path.
 *  `latestAt` is already the real publish time on this feed. */
function toCanonicalCard(u: CanonicalUpdate): Card {
	return {
		title: u.title ?? 'Untitled',
		ch: u.latestChapter ? `Ch. ${u.latestChapter}` : '',
		time: relTime(u.latestAt),
		timeAt: epochMs(u.latestAt),
		detected: '',
		rating: '',
		cover: u.coverUrl ?? '',
		id: u.workId,
	};
}

/** A merged-Updates-feed row → Card. The server has already done the interleave, the
 *  by-work dedupe and the paging, so this is a pure field mapping — no sorting, no
 *  slicing, no title dedupe.
 *
 *  `ch` mirrors what each half of the feed showed BEFORE the merge: the mirror half
 *  knows the newest chapter's NUMBER ("Ch. 10.5"), the scanner half only the series'
 *  chapter COUNT ("Ch. 412"). They are different quantities and the server sends
 *  whichever it has, so no card's label changes.
 *
 *  `rating` is '' when unrated rather than "0.0": the old scanner-half card ran
 *  `RatingSummary::empty().average.toFixed(1)` and printed a 0.0 star on every unrated
 *  series, which is the vast majority (the whole database holds a handful of reviews). */
function toFeedCard(u: UpdateFeedRow): Card {
	const ch =
		u.latestChapter != null && u.latestChapter !== ''
			? `Ch. ${u.latestChapter}`
			: u.chapterCount != null
				? `Ch. ${u.chapterCount}`
				: '';
	return {
		title: u.title,
		ch,
		// The RELEASE clock, same as every other card — see {@link chapterRecency}.
		time: relTime(u.releasedAt),
		timeAt: epochMs(u.releasedAt),
		// Our detection time, hover-only, and null on mirror-only rows.
		detected: relTime(u.detectedAt),
		rating: u.rating != null ? u.rating.toFixed(1) : '',
		cover: u.coverUrl ?? '',
		// `|| undefined`, not `u.id`. The grid keys its `{#each}` on `item.id ?? item.title`,
		// and `''` is not nullish — so an id-less row would key on the empty string, and a
		// SECOND id-less row would collide with it. That is not cosmetic: Svelte 5 throws
		// `each_key_duplicate` in production and the page dies in the error boundary. The
		// wire type says `ID!`, but the legacy fallback in `fetchUpdatesFeed` synthesizes
		// `id: c.id ?? ''`, so the empty case is reachable from inside this module.
		// Falling through to the title is safe: that list is title-deduped.
		id: u.id || undefined,
		type: u.type ? toViewType(u.type) : undefined,
	};
}

/** A `Series` → grid card fields.
 *
 *  `ch` is the server's `chapterCount`, which is legitimately **0** now that Browse pages
 *  the whole catalogue (the ~67k works whose chapters MangaDex removed on licensing, or
 *  that will get them from another source). It is a NUMBER here on purpose — the label is
 *  the caller's, because "0" has to read as "No chapters yet" on a browse card and as
 *  nothing at all in a progress line. See `cardSub` in the browse route. */
function toCatalogEntry(s: Series, i: number): CatalogEntry {
	// `added` stays the positional (backend-order) index for back-compat; `addedAt`
	// carries the real catalogue-entry timestamp so Browse "Newest" can sort by
	// recency (b.addedAt - a.addedAt) rather than by arrival order. NaN/absent → 0.
	const addedAt = s.createdAt ? Date.parse(s.createdAt) : NaN;
	return {
		title: s.title,
		author: s.author ?? '',
		genre: s.genres[0] ?? '',
		ch: s.chapterCount,
		rating: s.rating.average,
		status: toViewStatus(s.status),
		added: i,
		addedAt: Number.isFinite(addedAt) ? addedAt : 0,
		type: toViewType(s.type),
		cover: s.coverUrl,
		id: s.id,
	};
}

/** Dedupe a Card list by title (case-insensitive), preserving first-seen order. Used
 *  where a series can arrive under two identities — e.g. a numeric Suwayomi scanner
 *  update and its `w_` MangaDex-mirror update — which share a title but not an id. */
function dedupeCardsByTitle(cards: Card[]): Card[] {
	const seen = new Set<string>();
	const out: Card[] = [];
	for (const c of cards) {
		const key = c.title.trim().toLowerCase();
		if (!seen.has(key)) {
			seen.add(key);
			out.push(c);
		}
	}
	return out;
}

/** A card's sort instant, defaulting to `0` (= "unknown, sorts last") when the card
 *  carries no `timeAt`. See {@link Card.timeAt}. */
function cardTimeAt(c: Card): number {
	return c.timeAt ?? 0;
}

/**
 * Interleave several card feeds into ONE list in true recency order, then dedupe.
 *
 * This exists because concatenating the feeds does not merge them. Two feeds that
 * are each internally newest-first produce, back to back, a list that descends
 * through the first feed's timestamps and then JUMPS BACK to "now" where the second
 * feed starts — so the Updates grid read "3h, 5h, … 2d" for twenty cards and then
 * "1h" again at card twenty-one. Every feed is on the same clock (a real upstream
 * release time), so they are directly comparable and there is no reason to show them
 * segregated.
 *
 * SORT BEFORE DEDUPE, deliberately: a series that appears on both feeds (once under
 * its numeric Suwayomi identity, once as its `w_` mirror) should keep whichever row
 * reports the FRESHER chapter, and only sorting first puts that row where the dedupe
 * will be the one to keep it. `Array#sort` is specified as stable, so genuinely tied
 * timestamps preserve the argument order of the feeds — i.e. earlier feeds still win
 * ties, which is the old concatenation's one useful property.
 */
function mergeByRecency(...feeds: Card[][]): Card[] {
	return dedupeCardsByTitle(feeds.flat().sort((a, b) => cardTimeAt(b) - cardTimeAt(a)));
}

/** Dedupe a series list by id, preserving first-seen order. */
function dedupeSeries(list: Series[]): Series[] {
	const seen = new Set<string>();
	const out: Series[] = [];
	for (const s of list) {
		if (!seen.has(s.id)) {
			seen.add(s.id);
			out.push(s);
		}
	}
	return out;
}

/** Home hero slide — carries cover + id so the hero can render art and link. */
export interface FeaturedView {
	title: string;
	genre: string;
	ch: number;
	cover: string;
	id?: string;
}

function toFeatured(s: Series): FeaturedView {
	return {
		title: s.title,
		genre: s.genres[0] ?? '',
		ch: s.chapterCount,
		cover: s.coverUrl,
		id: s.id,
	};
}

/**
 * Per-format counts backed by the live catalog sample (the union of discovery
 * feeds), keeping the cards' presentation metadata. Not the true global total —
 * the federated catalog isn't fully enumerated client-side — but a real
 * reflection of what's currently surfaced.
 */
function deriveFormatCards(pool: Series[]): typeof FORMAT_CARDS {
	return FORMAT_CARDS.map((card) => {
		const n = pool.filter((s) => toViewType(s.type) === card.type).length;
		return { ...card, count: `${n} ${n === 1 ? 'title' : 'titles'}` };
	});
}

/** The most common genres across the live catalog sample, most-frequent first. */
function deriveGenres(pool: Series[]): string[] {
	const freq = new Map<string, number>();
	for (const s of pool) for (const g of s.genres) freq.set(g, (freq.get(g) ?? 0) + 1);
	return [...freq.entries()]
		.sort((a, b) => b[1] - a[1])
		.slice(0, 9)
		.map(([g]) => g);
}

/** Classify a series onto a library shelf from its real read progress. */
function shelfFor(read: number, total: number): 'reading' | 'completed' | 'plan' {
	if (total > 0 && read >= total) return 'completed';
	if (read === 0) return 'plan';
	return 'reading';
}

// ---- translators (per-source "translator" selection, S3) -------------------

/** A selectable translator (source) for a canonical work, in view shape. */
export interface TranslatorOption {
	/** Stable selection key (`sourceType:suwayomiMangaId`), persisted per work. */
	key: string;
	/** `'mangadex'` (canonical spine) or `'suwayomi'` (an installed extension). */
	sourceType: string;
	/** Display name, e.g. "MangaDex" or "MANGA Plus". */
	name: string;
	/** Language code (e.g. `en`), or null when N/A. */
	lang: string | null;
	/** Store-hosted extension logo, or null → the UI renders an initial. */
	iconUrl: string | null;
	/** Suwayomi manga id whose `chapters(seriesId:)` gives this translator's
	 *  chapters; null → the canonical spine (`canonicalChapters(workId)`). */
	suwayomiMangaId: string | null;
	/** How many chapters this translator currently carries. */
	chapterCount: number;
}

/** A compact translator tag for cards (logo + label). */
export interface TranslatorChip {
	name: string;
	lang: string | null;
	iconUrl: string | null;
}

/** Friendly display names for the common curated extensions; falls back to a
 *  prettified last package segment (e.g. `…extension.all.foo` → "Foo"). */
const SOURCE_NAME_BY_PKG: Record<string, string> = {
	'eu.kanade.tachiyomi.extension.all.mangadex': 'MangaDex',
	'eu.kanade.tachiyomi.extension.all.mangaplus': 'MANGA Plus',
	'eu.kanade.tachiyomi.extension.all.comick': 'ComicK',
	'eu.kanade.tachiyomi.extension.en.webtoons': 'WEBTOON',
	'eu.kanade.tachiyomi.extension.all.batoto': 'Bato.to',
};

function prettySourceName(pkg: string | null | undefined): string | null {
	if (!pkg) return null;
	if (SOURCE_NAME_BY_PKG[pkg]) return SOURCE_NAME_BY_PKG[pkg];
	const seg = pkg.split('.').pop() ?? pkg;
	return seg.charAt(0).toUpperCase() + seg.slice(1);
}

/** Icon URL for an extension package, served from OUR origin.
 *
 *  Komika used to link Keiyoushi's `extensions/repo/icon/{pkg}.png` directly.
 *  They emptied that directory when they migrated their index to `index.pb`, so
 *  every icon in the product 404'd at once; the set is now vendored in
 *  `assets/ext-icons/` and served at `/ext-icons/{pkg}.png`. The package-name
 *  shape is checked here so a non-Keiyoushi package renders the UI's
 *  initial-letter placeholder instead of requesting a path we can't have.
 *  Mirrors `keiyoushi_icon_url` in the server's `graphql/mod.rs` — keep in sync. */
function iconForPkg(pkg: string | null | undefined): string | null {
	if (!pkg || !/^eu\.kanade\.tachiyomi\.extension\.[^.]+\.[^.]+$/.test(pkg)) return null;
	return apiAssetSrc(`/ext-icons/${pkg}.png`);
}

/** A normalized language label, dropping Suwayomi's catch-all "all". */
function langLabel(lang: string | null | undefined): string | null {
	return lang && lang !== 'all' ? lang : null;
}

function translatorKey(sourceType: string, suwayomiMangaId: string | null): string {
	return `${sourceType}:${suwayomiMangaId ?? 'spine'}`;
}

/** Federated-search `Translator` → compact card chip. */
function toTranslatorChip(t: Translator): TranslatorChip {
	return {
		name: t.sourceName ?? prettySourceName(t.extensionPkgName) ?? 'Source',
		lang: langLabel(t.lang),
		iconUrl: t.extensionIconUrl ?? iconForPkg(t.extensionPkgName),
	};
}

/** Dedupe chips by name+lang, preserving first-seen order (a work can carry the
 *  same source under several manga ids — collapse them for display). */
function dedupeChips(list: TranslatorChip[]): TranslatorChip[] {
	const seen = new Set<string>();
	const out: TranslatorChip[] = [];
	for (const c of list) {
		const k = `${c.name}|${c.lang ?? ''}`;
		if (!seen.has(k)) {
			seen.add(k);
			out.push(c);
		}
	}
	return out;
}

/** A resolved canonical work: its selectable translators, the chosen one, and
 *  that translator's chapters + series metadata. Null when nothing readable. */
interface ResolvedWork {
	translators: TranslatorOption[];
	selected: TranslatorOption;
	chapters: Chapter[];
	meta: Series | null;
	/** The canonical (MangaDex-mirrored) series for the work when it resolves —
	 *  carries S2 enrichment (credits + localized descriptions). Null for
	 *  federation-only works with no MangaDex anchor. */
	canonSeries: Series | null;
	/**
	 * The EFFECTIVE canonical `w_` id the backend resolved the request to.
	 *
	 * The server follows `work_redirect`: when a work has been merged away, the
	 * canonical resolvers answer with the SURVIVOR's row (and its id), so a stale
	 * bookmark still renders. That survivor id is the one every id-keyed thing
	 * downstream must use — library/favourite/shelf writes, review posts, the
	 * translator preference, chapter links — otherwise the page keeps writing to
	 * a retired id forever and the URL never heals.
	 *
	 * Null when the work has no MangaDex anchor (federation-only): `canonicalSeries`
	 * 404s there, so there is nothing to learn the survivor id from and the
	 * requested id stands.
	 */
	canonicalId: string | null;
	/** Whether `chapters` are already server-ordered (the canonical spine). */
	preserveOrder: boolean;
	/**
	 * EVERY source's chapters, not just the selected one's, in picker order.
	 *
	 * The fan-out below already fetches all of them to compute the per-source counts,
	 * so exposing them is free — and it is what lets the reader open a `?ch=` that
	 * belongs to a source other than the default instead of rendering an empty view.
	 * See {@link findChapterOwner}.
	 */
	candidates: ChapterCandidate<Chapter>[];
	/**
	 * Cross-source aggregation, when it rode along in the single-round-trip
	 * `seriesPage` bundle (requested via `withAggregated`). Undefined on the
	 * legacy fan-out path — the caller then fetches `aggregatedChapters` itself.
	 */
	aggregated?: AggregatedChapter[];
}

/**
 * Resolve a canonical (`w_`) work into its translator list + the selected
 * translator's chapters/metadata. Fans out over the work's source mappings
 * (`workSources`) plus the MangaDex canonical spine when present, fetching each
 * translator's chapters so the picker can show counts and a non-empty default is
 * chosen. The SAME resolver backs both the series page and the reader, so they
 * always agree on the default translator. Returns null when the work has no
 * readable source at all.
 */
async function resolveWork(
	workId: string,
	preferredKey?: string,
	opts?: { withAggregated?: boolean },
): Promise<ResolvedWork | null> {
	let wsList: WorkSource[];
	let spineChapters: Chapter[];
	let canonSeries: Series | null;
	/** The server-resolved survivor id, when the bundle path supplied one. */
	let effectiveId: string | null = null;
	let aggregated: AggregatedChapter[] | undefined;
	/** Per-source data carried in the bundle — set only on the bundle path; the
	 *  legacy path leaves these null and fetches per source below. */
	let chaptersByKey: Map<string, Chapter[]> | null = null;
	let metaByKey: Map<string, Series | null> | null = null;

	// P2.1 fast path: ONE `seriesPage` round trip carries everything the fan-out
	// below used to assemble from 3 parallel requests + a per-translator chapters
	// N+1 + a metadata follow-up. `null` = the work genuinely doesn't resolve
	// (unknown id / NSFW-gated) — same outcome the fan-out would reach, minus the
	// wasted requests. `undefined` = the SERVER predates the resolver (deploy-order
	// drift) — fall through to the fan-out. A genuine outage rejects and propagates,
	// matching the fan-out's honest-error guard.
	const bundle = backend.seriesPage
		? await backend.seriesPage(workId, opts?.withAggregated ?? false)
		: undefined;
	if (bundle === null) return null;
	if (bundle) {
		wsList = bundle.sources.map((s) => s.source);
		spineChapters = bundle.spineChapters;
		canonSeries = bundle.series;
		effectiveId = bundle.workId;
		aggregated = bundle.aggregated;
		chaptersByKey = new Map(bundle.sources.map((s) => [s.source.sourceKey, s.chapters]));
		metaByKey = new Map(bundle.sources.map((s) => [s.source.sourceKey, s.series]));
	} else {
		// Legacy fan-out: source mappings (suwayomi translators) + the canonical spine
		// chapters + the canonical series (S2 enrichment: credits + localized
		// descriptions), in parallel. Individual rejections are tolerated
		// (canonicalChapters/canonicalSeries legitimately 404 for a federation-only
		// work), but if every method that is actually present rejects the backend is
		// effectively down — rethrow so the caller surfaces an honest error rather
		// than a false "not found".
		const hasWorkSources = !!backend.workSources;
		const hasCanonChapters = !!backend.canonicalChapters;
		const hasCanonSeries = !!backend.canonicalSeries;
		const [wsRes, spineRes, canonRes] = await Promise.allSettled([
			hasWorkSources ? backend.workSources!(workId) : Promise.resolve([] as WorkSource[]),
			hasCanonChapters ? backend.canonicalChapters!(workId) : Promise.resolve([] as Chapter[]),
			hasCanonSeries ? backend.canonicalSeries!(workId) : Promise.resolve(null),
		]);
		// "Backend fully down" honest-error guard: an ABSENT optional method resolves to a
		// benign default and must NOT be counted as evidence the backend is up (otherwise a
		// backend missing e.g. canonicalChapters would mask a real outage of the methods it
		// DOES have and degrade to a false "not found"). Rethrow only when every method that
		// was actually present rejected — a true outage — and at least one was present.
		const present = [
			{ has: hasWorkSources, res: wsRes },
			{ has: hasCanonChapters, res: spineRes },
			{ has: hasCanonSeries, res: canonRes },
		].filter((m) => m.has);
		if (present.length > 0 && present.every((m) => m.res.status === 'rejected')) {
			throw (present.find((m) => m.res.status === 'rejected')!.res as PromiseRejectedResult).reason;
		}
		wsList = wsRes.status === 'fulfilled' ? wsRes.value : [];
		spineChapters = spineRes.status === 'fulfilled' ? spineRes.value : [];
		canonSeries = canonRes.status === 'fulfilled' ? canonRes.value : null;
	}

	// Candidate translators: the canonical spine first, then each distinct suwayomi
	// source mapping.
	//
	// The spine is offered whenever the work HAS a MangaDex mapping — NOT only when that
	// mapping currently carries chapters. Gating on `spineChapters.length` was wrong for
	// this catalogue in the exact case the picker matters most: MangaDex strips chapters
	// on a licensing takedown, so a title like Boku no Hero Academia has three source
	// mappings and zero spine chapters. The old test dropped MangaDex, leaving a single
	// candidate, which renders as a static "Only one source available" label — a source
	// picker that looks missing, on a page whose whole purpose is "this work exists
	// elsewhere, go find it". A source you cannot read from is still a source you must be
	// able to SEE, and its `chapterCount` of 0 says so honestly.
	// Keyed on `canonSeries`, not on the raw MangaDex mapping: the spine's metadata comes
	// from it below (`meta = canonSeries`), so offering a spine we have no series row for
	// would hand `getSeries` a null `meta` and 404 the page. A work whose mapping exists
	// but whose `canonicalSeries` refused (NSFW gate) correctly keeps its suwayomi
	// sources only.
	const candidates: {
		view: Omit<TranslatorOption, 'chapterCount'>;
		chapters: Chapter[];
		/**
		 * The all.mangadex extension shadowing a readable direct spine — the same MangaDex
		 * content by the slow `api.komiq.cc` route. Hidden from the default selection and the
		 * picker (the spine represents it via `mangadex.network`), but kept in the list so
		 * {@link findChapterOwner} can still resolve a `?ch=` naming one of its chapters.
		 */
		redundant?: boolean;
	}[] = [];
	if (spineChapters.length || canonSeries) {
		candidates.push({
			view: {
				key: translatorKey('mangadex', null),
				sourceType: 'mangadex',
				name: 'MangaDex',
				lang: null,
				iconUrl: iconForPkg(MANGADEX_EXT_PKG),
				suwayomiMangaId: null,
			},
			chapters: spineChapters,
		});
	}
	// A direct spine that actually carries chapters is the fast (mangadex.network) route
	// for this MangaDex work; when present, the all.mangadex Suwayomi extension below is a
	// redundant duplicate of it and must not be served over api.komiq.cc in its place.
	const hasReadableSpine = spineChapters.length > 0;

	const seenIds = new Set<string>();
	const suwViews: { view: Omit<TranslatorOption, 'chapterCount'>; redundant: boolean }[] = [];
	for (const ws of wsList) {
		if (ws.sourceType === 'mangadex') continue; // spine handled above
		const id = ws.sourceKey;
		if (!id || seenIds.has(id)) continue;
		seenIds.add(id);
		suwViews.push({
			view: {
				key: translatorKey(ws.sourceType, id),
				sourceType: ws.sourceType,
				name: prettySourceName(ws.extension?.pkgName) ?? `Source ${ws.sourceId}`,
				lang: langLabel(ws.lang),
				iconUrl: iconForPkg(ws.extension?.pkgName),
				suwayomiMangaId: id,
			},
			// The MangaDex extension duplicates the direct spine; flag it redundant only when
			// the spine can serve the content itself. Non-MangaDex extensions are never
			// redundant — they are the sole source of their content.
			redundant: isRedundantMangadexExt(ws.extension?.pkgName, hasReadableSpine),
		});
	}
	// Each suwayomi translator's chapters: carried in the bundle on the fast path,
	// otherwise fetched per translator (bounded — a work has few mappings).
	const suwChapters = chaptersByKey
		? suwViews.map((v) => chaptersByKey.get(v.view.suwayomiMangaId as string) ?? [])
		: await Promise.all(
				suwViews.map((v) =>
					backend.chapters(v.view.suwayomiMangaId as string).catch(() => [] as Chapter[]),
				),
			);
	suwViews.forEach((v, i) =>
		candidates.push({ view: v.view, chapters: suwChapters[i], redundant: v.redundant }),
	);

	if (!candidates.length) return null;

	// Order: spine first, then by chapter count desc so a populated translator
	// leads. Stable within equal counts (preserves discovery order).
	const ordered = candidates
		.map((c, i) => ({ ...c, i }))
		.sort((a, b) => {
			const aSpine = a.view.suwayomiMangaId === null ? 1 : 0;
			const bSpine = b.view.suwayomiMangaId === null ? 1 : 0;
			if (aSpine !== bSpine) return bSpine - aSpine;
			if (b.chapters.length !== a.chapters.length) return b.chapters.length - a.chapters.length;
			return a.i - b.i;
		});

	// Picker list EXCLUDES a redundant all.mangadex extension: MangaDex must not appear
	// twice, and a manual pick must not be able to route MangaDex content onto the slow
	// api.komiq.cc proxy when the direct spine serves it via mangadex.network.
	const translators: TranslatorOption[] = ordered
		.filter((c) => !c.redundant)
		.map((c) => ({ ...c.view, chapterCount: c.chapters.length }));

	// Selection: honour a valid persisted preference, else the candidate carrying
	// the MOST chapters (so a work whose spine has few/none but another source is
	// complete — e.g. Solo Leveling → Asura — defaults to the readable source),
	// else the first candidate. A redundant all.mangadex extension is never eligible —
	// its content is the direct spine, so a stale persisted preference to it heals here.
	const pickKey = pickDefaultKey(
		ordered.map((c) => ({
			key: c.view.key,
			chapterCount: c.chapters.length,
			redundant: !!c.redundant,
		})),
		preferredKey,
	);
	const pick = ordered.find((c) => c.view.key === pickKey) ?? ordered[0];

	const selected = translators.find((t) => t.key === pick.view.key) as TranslatorOption;

	// Metadata for the selected translator: the canonical mirror for the spine
	// (already fetched), otherwise the source series behind the chosen suwayomi
	// mapping.
	let meta: Series | null;
	if (selected.suwayomiMangaId === null) {
		meta = canonSeries;
	} else if (metaByKey) {
		// Bundle path: the source's series rode along; a server-side fetch failure
		// arrived as null, exactly what the legacy catch produced.
		meta = metaByKey.get(selected.suwayomiMangaId) ?? null;
	} else {
		meta = await backend.series(selected.suwayomiMangaId).catch(() => null);
	}

	// Only ever adopt a CANONICAL id here: `canonSeries.id` is a `w_` work id, but
	// guard the shape anyway so a numeric/source id could never leak onto the
	// canonical path and route a later request to `backend.series()`.
	//
	// The bundle path improves on the canonSeries-derived id: `seriesPage` reports
	// the redirect-resolved survivor id for EVERY work, including federation-only
	// ones (where canonicalSeries 404s and the legacy path has nothing to learn the
	// survivor from).
	const canonicalId =
		effectiveId && isCanonicalId(effectiveId) && effectiveId !== workId
			? effectiveId
			: canonSeries && isCanonicalId(canonSeries.id) && canonSeries.id !== workId
				? canonSeries.id
				: null;

	return {
		translators,
		selected,
		chapters: pick.chapters,
		meta,
		canonSeries,
		canonicalId,
		preserveOrder: selected.suwayomiMangaId === null,
		candidates: ordered.map((c) => ({
			key: c.view.key,
			suwayomiMangaId: c.view.suwayomiMangaId,
			chapters: c.chapters,
		})),
		aggregated,
	};
}

/**
 * The outcome of a catalogue search. The federated (`searchAllSources`) path is
 * login-gated and per-user rate-limited on the server, so callers need to tell
 * "no matches" apart from "not authenticated" (fall back to native) and
 * "rate-limited" (honest transient message) rather than collapsing every case to
 * an empty grid.
 */
export type SearchOutcome =
	| { kind: 'ok'; rows: FederatedResultView[] }
	| { kind: 'unauthenticated' }
	| { kind: 'rateLimited'; retryAfter: number | null }
	| { kind: 'error' };

/**
 * Federated multi-extension search: one deduped row per canonical work, each
 * carrying its per-source translator tags. REQUIRES a signed-in viewer (the
 * server rejects anonymous callers) — callers should only invoke it when logged
 * in and fall back to {@link getNativeSearch} otherwise. Never rejects; classifies
 * the server's auth/rate-limit errors into {@link SearchOutcome}.
 */
export async function getFederatedSearch(query: string): Promise<SearchOutcome> {
	const q = query.trim();
	if (!q) return { kind: 'ok', rows: [] };
	if (!LIVE || !backend.searchAllSources) {
		// Backend without federation — treat as unauthenticated so callers use the
		// public native path (which carries no translators).
		return { kind: 'unauthenticated' };
	}
	try {
		const page = await backend.searchAllSources(q);
		return {
			kind: 'ok',
			rows: page.items.map((fs, i) => ({
				...toCatalogEntryFull(fs.series, i),
				translators: dedupeChips(fs.translators.map(toTranslatorChip)),
			})),
		};
	} catch (err) {
		const msg = err instanceof Error ? err.message : String(err);
		if (/not authenticated/i.test(msg)) return { kind: 'unauthenticated' };
		if (/too many/i.test(msg)) {
			const m = msg.match(/(\d+)\s*s/i);
			return { kind: 'rateLimited', retryAfter: m ? Number(m[1]) : null };
		}
		console.warn('[komika] federated search failed:', err);
		return { kind: 'error' };
	}
}

/**
 * Browse-search filters applied server-side (S4).
 *
 * `types` / `status` are in the READER's vocabulary ('Manhwa', 'ongoing') because that
 * is what the Browse rail holds; {@link getNativeSearch} translates them to the wire
 * enums through the same bridges the rest of this module uses, so there is exactly one
 * place where 'Manhwa' becomes 'MANHWA'.
 *
 * `types` / `status` / `sort` / `contentRating` are honoured on the BROWSE (empty-query)
 * path ONLY — the server orders a text query by relevance and ignores them. Passing
 * them alongside a query is harmless but does nothing; callers must not tell the viewer
 * otherwise.
 */
export interface CatalogFilters {
	/** Match any of these genres (case-insensitive). Empty → no genre filter. */
	genres?: string[];
	/** Inclusive rating bounds on the 0–10 scale. */
	minRating?: number;
	maxRating?: number;
	/** Match any of these formats. Empty → no format filter. */
	types?: ComicType[];
	/** Publication status; omit for any. */
	status?: Status;
	/** Result ordering. Omit to take the server's default (TRENDING). */
	sort?: BrowseSort;
	/** Content-rating ceiling. The server clamps it to the viewer's NSFW posture, so it
	 *  can only ever narrow — it is not an opt-in. */
	contentRating?: ContentRatingFilter;
	/**
	 * Restrict to series we know a chapter for. OMIT (not `false`) for everything —
	 * `false` is a meaningful value on the wire (only series with NO chapters).
	 *
	 * Browse pages the whole catalogue, including the ~67k works with no chapter yet:
	 * MangaDex removes chapters when a series is licensed, so "no chapters" tracks
	 * POPULAR, not obscure, and those works get their chapters from another source.
	 * This is the switch for a reader who only wants what they can open right now.
	 */
	hasChapters?: boolean;
}

/**
 * Native (public) catalogue search — the pre-federation path, available to
 * anonymous viewers, and the whole-catalogue browse when `query` is empty. ALL
 * `filters` are applied server-side (S4), including format/status/sort/content-rating
 * on the browse path, so `total` and `hasNext` describe the filtered catalogue rather
 * than the page. Rows carry no translator tags (single source). Never rejects; `error`
 * is true only on a genuine backend failure (an empty query with no error is an honest
 * "no results").
 *
 * Browse pages at 30 rows, not the API-wide 20 — see `BROWSE_PAGE_SIZE` in
 * `$lib/data/paging`, which the caller's pager arithmetic must use.
 */
export async function getNativeSearch(
	query: string,
	filters: CatalogFilters = {},
	page = 1,
): Promise<{
	items: FederatedResultView[];
	error: boolean;
	hasNext: boolean;
	total: number | null;
}> {
	// Captured from the server envelope so the Browse grid can drive the page pager
	// off the server's own paging metadata (whole-catalogue pagination) rather than
	// being capped at the first page.
	let hasNext = false;
	let total: number | null = null;
	const r = await liveResult(async () => {
		const res = await backend.search(query.trim(), page, {
			genres: filters.genres,
			minRating: filters.minRating,
			maxRating: filters.maxRating,
			// View → wire, through the SAME bridges the domain→view mappers invert. An
			// empty selection is left `undefined`, not `[]`: the resolver reads `[]` as an
			// empty IN (…) and returns nothing, so "no format chosen" would empty the grid.
			types: filters.types?.length ? filters.types.map((t) => VIEW_TO_DOMAIN_TYPE[t]) : undefined,
			status: filters.status ? VIEW_TO_DOMAIN_STATUS[filters.status] : undefined,
			sort: filters.sort,
			contentRating: filters.contentRating,
			// Passed through as-is, `undefined` included: absent is "no clause" and `false`
			// means "only series with no chapters", so they cannot be conflated.
			hasChapters: filters.hasChapters,
		});
		hasNext = res.hasNextPage ?? false;
		total = res.total ?? null;
		return res.items.map((s, i) => ({
			...toCatalogEntryFull(s, i),
			translators: [] as TranslatorChip[],
		}));
	}, [] as FederatedResultView[]);
	return { items: r.data, error: r.error, hasNext, total };
}

/** The full genre facet set for the search filter (S4), most-common first. Never
 *  rejects; resolves to an empty list on failure/backend-off. */
export function getGenreFacets(): Promise<{ genre: string; count: number }[]> {
	return live(async () => {
		if (!backend.genreFacets) return [];
		return backend.genreFacets();
	}, []);
}

/** A federated search row for the Browse grid: a catalog entry + translator tags
 *  + the full genre list (for client-side ANY-genre filtering of federated rows). */
export interface FederatedResultView extends CatalogEntry {
	isNsfw: boolean;
	genres: string[];
	translators: TranslatorChip[];
}

/** Like {@link toCatalogEntry} but keeps the NSFW flag + full genre list for the grid. */
function toCatalogEntryFull(
	s: Series,
	i: number,
): CatalogEntry & { isNsfw: boolean; genres: string[] } {
	return { ...toCatalogEntry(s, i), isNsfw: s.isNsfw, genres: s.genres };
}

// ---- per-screen getters ----------------------------------------------------

export function getHome() {
	const fallback = {
		featured: [] as FeaturedView[],
		latestUpdates: [] as Card[],
		trending: [] as Card[],
		latestAdded: [] as Card[],
		formatCards: FORMAT_CARDS,
		homeGenres: [] as string[],
	};
	return live(async () => {
		// Discovery drives the curated rows; "Latest Updates" merges OUR scanner's
		// new-chapter detections with the MangaDex mirror's freshest works (so the row
		// fills even while the scanner is still warming up). The hero is the top of the
		// live Popular feed (first feed as fallback) — empty when nothing is live.
		const [feeds, updates, canonical] = await Promise.all([
			backend.discovery(),
			backend.updates().catch(() => ({ items: [] as Series[] })),
			backend.canonicalUpdates?.().catch(() => [] as CanonicalUpdate[]) ??
				Promise.resolve([] as CanonicalUpdate[]),
		]);
		const byKind = (k: string) => feeds.find((f) => f.kind === k)?.items ?? [];
		const popular = byKind('POPULAR');
		const pool = dedupeSeries(feeds.flatMap((f) => f.items));
		const featured = (popular.length ? popular : (feeds[0]?.items ?? []))
			.slice(0, 5)
			.map(toFeatured);
		// The two feeds MERGED by release time, not concatenated — see
		// {@link mergeByRecency}. Both carry a real upstream chapter-release timestamp,
		// so the row is a single descending sequence; capped at the 10 the home row
		// shows, which is now genuinely the 10 newest rather than "8 from one feed then
		// 2 from the other".
		const latestUpdates = mergeByRecency(
			updates.items.map(toCard),
			canonical.map(toCanonicalCard),
		).slice(0, 10);
		return {
			featured,
			latestUpdates,
			// Dedup by title (like latestUpdates): the catalogue carries same-title rows
			// (per-language sources, and a series under both its numeric + `w_` identity),
			// which would collide on the home row's `{#each}` key and could show twice.
			trending: dedupeCardsByTitle(byKind('TRENDING').map(toCard)).slice(0, 10),
			latestAdded: dedupeCardsByTitle(byKind('RECENTLY_ADDED').map(toAddedCard)).slice(0, 10),
			formatCards: pool.length ? deriveFormatCards(pool) : FORMAT_CARDS,
			homeGenres: deriveGenres(pool),
		};
	}, fallback);
}

/** The browse catalogue rows (with genres/translator shape) plus an honest
 *  failure flag (RC4). `error` is true only when the backend request threw — the
 *  screen shows an error state instead of a misleading "no series" empty. Genre /
 *  rating `filters` are applied server-side (S4). */
export interface CatalogResult {
	items: FederatedResultView[];
	error: boolean;
}

export function getBrowseCatalog(filters: CatalogFilters = {}): Promise<CatalogResult> {
	return getNativeSearch('', filters);
}

/** Most cards the "Hot" grid will render in one pass. `Hot` is a curated top-N ranking
 *  off the discovery TRENDING feed with no server pagination anywhere in the stack, so
 *  paging it would be meaningless — the cap is what bounds it instead. The "New" list no
 *  longer needs a cap: the server pages it at `FEED_PAGE_SIZE`. */
const HOT_MAX = 60;

/** What `getUpdates` returns — the two grids plus the "New" list's server pager state. */
export interface UpdatesView {
	trendingGroups: { label: string; items: Card[] }[];
	newUpdates: Card[];
	hotUpdates: Card[];
	/** Echoed back from the server, never assumed from the click (the admin pager's
	 *  invariant): an over-range page comes back as an empty list on the page asked for,
	 *  which the screen repairs by navigating to the last real page. */
	page: number;
	hasNext: boolean;
	total: number | null;
	/**
	 * True only when the "New" list's request genuinely FAILED — never for an honest
	 * empty page, and never in mock mode (`live()` short-circuits to the fallback, which
	 * sets this false; same doctrine as {@link liveResult}).
	 *
	 * Without it the screen cannot tell an outage from "no rows match", and it rendered
	 * the empty state either way: "Nothing matches this filter right now — try another
	 * format or tab", offering a remedy that cannot work while the backend is down. Browse
	 * already separates the two (its `catalogError` branch); this is the same distinction.
	 */
	error: boolean;
}

/**
 * The Updates screen's feeds. `page` / `type` page and filter the "New" list SERVER-SIDE.
 *
 * The "New" list used to be `mergeByRecency(page 1 of updates, page 1 of canonicalUpdates)`
 * deduped by lowercased title and capped at 60. That could not be paginated: a page of
 * the merged list is not page N of either feed (so a boundary either skips a row or emits
 * it on both pages — and a duplicate key is fatal, Svelte 5 throws `each_key_duplicate` in
 * production), and deduping after paging left short pages that made any `total` /
 * `hasNext` the pager showed wrong. The server now takes the union once, keyed by
 * canonical work (`updatesFeed`), so this function just maps rows.
 *
 * `tab` is here because "Hot" needs no `updatesFeed` request at all — it is a slice of the
 * discovery TRENDING feed — and skipping it saves the whole paginated query on that tab.
 */
export function getUpdates(
	opts: { page?: number; tab?: 'new' | 'hot'; type?: ComicType | null } = {},
): Promise<UpdatesView> {
	const page = Math.max(1, Math.trunc(opts.page ?? 1));
	const tab = opts.tab ?? 'new';
	const type = opts.type ?? null;
	const fallback: UpdatesView = {
		trendingGroups: [],
		newUpdates: [],
		hotUpdates: [],
		page,
		hasNext: false,
		total: null,
		error: false,
	};
	return live(async () => {
		// Each feed is caught independently (mirrors getHome): one outage — e.g. the
		// merged updates feed — must not collapse the whole screen (incl. Trending).
		const [feeds, feed] = await Promise.all([
			backend.discovery().catch(() => [] as DiscoveryFeed[]),
			tab === 'hot' ? Promise.resolve(null) : fetchUpdatesFeed(page, type).catch(() => null),
		]);
		const byKind = (k: string) => feeds.find((f) => f.kind === k)?.items ?? [];
		// Trending still needs the title dedupe: the catalogue carries same-title rows
		// (per-language sources, and a series under both its numeric and its `w_`
		// identity), and this grid keys its {#each} on the id falling back to the title.
		// The "New" list does NOT — the server deduped it by work identity, which is both
		// stricter and the only version that keeps page fullness honest.
		const trending = dedupeCardsByTitle(byKind('TRENDING').map(toCard));
		return {
			trendingGroups: trending.length ? [{ label: 'Trending Today', items: trending }] : [],
			newUpdates: feed?.items.map(toFeedCard) ?? [],
			hotUpdates: trending.slice(0, HOT_MAX),
			// The server echoes the page it was asked for rather than clamping it.
			page: feed?.page ?? page,
			hasNext: feed?.hasNextPage ?? false,
			total: feed?.total ?? null,
			// `feed` is null for exactly two reasons: the request threw (the `.catch`
			// above), or we never made one because this is the "Hot" tab. Only the first
			// is an error — and `fetchUpdatesFeed` already absorbs a missing `updatesFeed`
			// into the two-feed fallback, so reaching here with null means BOTH paths
			// failed, not merely an old server.
			error: tab !== 'hot' && feed === null,
		};
	}, fallback);
}

/**
 * One page of the merged Updates feed, with a fallback for a server that predates it.
 *
 * DEPLOY-ORDER GUARD, and it has to be a `catch`, not just an `if`. `updatesFeed` is
 * optional on the Backend interface, but the GraphQL backend always defines the METHOD —
 * what a stale server lacks is the FIELD, and an unknown field fails document validation
 * outright (unlike an unknown argument, which `stripUnsupported` removes). So checking
 * `if (backend.updatesFeed)` alone would never fire on the deployment that actually
 * breaks: the reader shipping ahead of the server. The request is therefore attempted and
 * a failure falls through to the old two-feed merge — correct for page 1, and reporting
 * `hasNextPage: false` so the pager stays hidden rather than offering pages nothing can
 * serve. Server first, reader second, and this is the net under that ordering.
 */
async function fetchUpdatesFeed(
	page: number,
	type: ComicType | null,
): Promise<{
	items: UpdateFeedRow[];
	page: number;
	hasNextPage: boolean;
	total: number | null;
}> {
	if (backend.updatesFeed) {
		try {
			return await backend.updatesFeed(page, type ? VIEW_TO_DOMAIN_TYPE[type] : undefined);
		} catch {
			// Fall through. Deliberately silent: `live()` already reports backend failure
			// to the screen, and this path is a successful degradation, not an error.
		}
	}
	const [updates, canonical] = await Promise.all([
		backend.updates().catch(() => ({ items: [] as Series[] })),
		backend.canonicalUpdates?.().catch(() => [] as CanonicalUpdate[]) ??
			Promise.resolve([] as CanonicalUpdate[]),
	]);
	// Legacy shape: merge by release time, dedupe by title, cap, and project onto the
	// feed-row shape so `toFeedCard` stays the single card mapper. `workId` echoes `id`
	// (it is only a dedupe key, which the server side already applied); `latestChapter`
	// carries the old mapper's already-formatted `ch` with its "Ch. " prefix stripped,
	// since `toFeedCard` re-adds it.
	// FILTER, THEN slice. The other order capped first and filtered second, so
	// `?type=Manhua` searched only the 60 newest rows of the merged feed and returned
	// however few of those happened to be manhua — usually a near-empty grid — instead of
	// the 60 newest manhua. The server path filters over the whole feed; this is the
	// closest the fallback can get to the same answer.
	const merged = mergeByRecency(updates.items.map(toCard), canonical.map(toCanonicalCard))
		.filter((c) => type == null || c.type === type)
		.slice(0, HOT_MAX);
	const items: UpdateFeedRow[] = merged.map((c) => ({
		id: c.id ?? '',
		workId: c.id ?? '',
		title: c.title,
		coverUrl: c.cover ?? null,
		type: c.type ? VIEW_TO_DOMAIN_TYPE[c.type] : null,
		latestChapter: c.ch ? c.ch.replace(/^Ch\.\s*/, '') : null,
		latestChapterTitle: null,
		chapterCount: null,
		releasedAt: c.timeAt ? new Date(c.timeAt).toISOString() : '',
		detectedAt: null,
		// `Number(c.rating)`, then reject 0 — NOT `c.rating ? …`. `toCard` formats an
		// unrated series as the STRING "0.0", which is truthy, so the truthiness test
		// forwarded 0 and `toFeedCard` rendered it back as a "0.0" star on the vast
		// majority of rows. That 0.0-on-everything star is exactly what the server path
		// returns null to avoid; the fallback has to agree with it.
		rating: unratedToNull(c.rating),
		isNsfw: false,
	}));
	return { items, page: 1, hasNextPage: false, total: null };
}

/** A card's formatted rating string back to a number, with "unrated" preserved as null.
 *  `toCard` prints `RatingSummary.average.toFixed(1)`, which is "0.0" for the (very
 *  common) unrated series — indistinguishable from a real 0.0, and the server's own feed
 *  reports both as null rather than showing a zero star. */
function unratedToNull(rating: string): number | null {
	const n = Number(rating);
	return Number.isFinite(n) && n > 0 ? n : null;
}

/** Reader format label → the domain enum the API takes. Inverse of {@link toViewType},
 *  which collapses WEBTOON→Manhwa and COMIC→Manga; the server applies the same collapse
 *  to its materialized `comic_type`, so these three words round-trip exactly.
 *
 *  Typed to `DomainComicType` rather than `string` so callers can hand the result
 *  straight to an API argument — it used to need an `as DomainComicType` at every use,
 *  which is a cast that would have kept compiling had a value here gone wrong. */
const VIEW_TO_DOMAIN_TYPE: Record<ComicType, DomainComicType> = {
	Manga: 'MANGA',
	Manhwa: 'MANHWA',
	Manhua: 'MANHUA',
};

/** Reader status word → the domain enum the API takes; the exact inverse of
 *  {@link toViewStatus} over the four statuses the reader can express.
 *
 *  Deliberately NOT `STATUS_WORD` (below), which looks similar but runs the other way
 *  (domain → display label) and folds UNKNOWN into ONGOING — feeding view words into it
 *  would not type-check, and folding is wrong in this direction: the reader has no
 *  "unknown" chip, so there is nothing to fold. */
const VIEW_TO_DOMAIN_STATUS: Record<Status, SeriesStatus> = {
	ongoing: 'ONGOING',
	completed: 'COMPLETED',
	hiatus: 'HIATUS',
	cancelled: 'CANCELLED',
};

export interface LibraryRowView {
	id?: string;
	title: string;
	cover: string;
	/** Format/country-of-origin, so the library card can show its flag badge. */
	type: ComicType;
	genre: string;
	rating: string;
	shelf: Shelf;
	favorite: boolean;
	read: number;
	total: number;
}
export interface ContinueRowView {
	id?: string;
	title: string;
	cover: string;
	ch: string;
	progress: number;
	genre: string;
}

export function getLibrary() {
	const fallback = {
		libraryCatalog: [] as LibraryRowView[],
		continueRow: [] as ContinueRowView[],
	};
	return live<typeof fallback>(async () => {
		// Per-series read progress comes from ONE batched query, joined by id. This
		// replaces a `chapters()` fetch per series — an N-round-trip fan-out that hung
		// this page on a large library (hundreds of series). Series without cached
		// progress fall back to their chapter count as unread. Backends that don't
		// expose `libraryProgress` (Suwayomi/native) degrade to all-unread.
		//
		// The two queries are INDEPENDENT (progress is keyed by series id, not by the
		// library response), so they go out together — serialising them doubled this
		// screen's time-to-content for no reason.
		const [lib, progress] = await Promise.all([
			backend.library(),
			backend.libraryProgress
				? backend.libraryProgress().catch(() => [] as SeriesProgress[])
				: Promise.resolve([] as SeriesProgress[]),
		]);
		const byId = new Map(progress.map((p) => [p.id, p]));
		const rows = lib.map((s) => {
			const p = byId.get(s.id);
			const total = p?.total || s.chapterCount;
			const read = p?.read ?? 0;
			return { s, total, read };
		});
		const libraryCatalog = rows.map(({ s, total, read }) => ({
			id: s.id,
			title: s.title,
			cover: s.coverUrl,
			type: toViewType(s.type),
			genre: s.genres[0] ?? '',
			rating: s.rating.average.toFixed(1),
			// An explicit shelf the viewer filed wins; otherwise derive from progress.
			shelf: (s.libraryStatus as Shelf | null) ?? shelfFor(read, total),
			favorite: s.isFavorite ?? false,
			read,
			total,
		}));
		// Continue-reading: series with progress underway. The resume label is the
		// next chapter number (read + 1), since chapters run 1..total.
		const continueRow = rows
			.filter(({ read, total }) => read > 0 && read < total)
			.map(({ s, read, total }) => ({
				id: s.id,
				title: s.title,
				cover: s.coverUrl,
				ch: `Ch. ${read + 1}`,
				progress: total ? Math.round((read / total) * 100) : 0,
				genre: s.genres[0] ?? '',
			}));
		return { libraryCatalog, continueRow };
	}, fallback);
}

// ---- series detail ---------------------------------------------------------

export interface SeriesChapterView {
	id?: string;
	n: number;
	title: string;
	date: string;
	isNew: boolean;
	read: boolean;
	/** The Suwayomi manga id of the source that provides this chapter (S2
	 *  aggregation), so the reader opens it from the right source; null = the
	 *  MangaDex mirror; undefined for a single-source (numeric) series. */
	src?: string | null;
}
export interface SeriesDetailView {
	id: string;
	title: string;
	/** Alternative / localized titles for the work (S2), excluding the display title.
	 *  Empty for a single-source or un-enriched work. */
	altTitles: string[];
	type: ComicType;
	flag: string;
	rating: string;
	votes: string;
	totalCh: number;
	/** All-time view count (chapter opens) — the popularity stat shown on the page.
	 *  0 until reads accrue or when the backend doesn't track views. */
	viewsTotal: number;
	/** Relative time since the newest chapter was RELEASED upstream (never our
	 *  poll time — see `chapterRecency`). "recently" when unknown. */
	updated: string;
	/** Relative time since WE detected that chapter; '' when the backend doesn't
	 *  report it. Shown as a hover title next to `updated`, not as its own line. */
	detected: string;
	statusLabel: string;
	author: string;
	artist: string;
	genres: string[];
	synopsis: string;
	cover: string;
	continueCh: number;
	startChapterId?: string;
	/** The source (Suwayomi manga id) the Continue/start chapter is read from
	 *  (S2 aggregation); null = MangaDex mirror; undefined for single-source. */
	startChapterSrc?: string | null;
	isMarked: boolean;
	/** Whether the viewer favourited this series (per-viewer; false when signed out). */
	isFavorite: boolean;
	/** The viewer's explicit library shelf, or null to derive from progress. */
	libraryStatus: Shelf | null;
	/** Full author/artist credit list (S2 enrichment); empty → fall back to the
	 *  single author/artist line. Deduped display is left to the component. */
	credits: { role: string; name: string }[];
	/** Every localized description of the work (S2); empty when un-enriched. Lets
	 *  the page offer other languages beyond the default-picked `synopsis`. */
	descriptions: { lang: string; description: string }[];
	/** The language tag of the description shown in `synopsis` (or null). */
	descLang: string | null;
	/** The full MangaDex cover set (F2), primary first then volume-ordered; empty
	 *  for a non-canonical / un-enriched work. The gallery renders only when >1. */
	covers: CoverView[];
	/** The work's EFFECTIVE NSFW flag — the admin override when one is pinned, else
	 *  the crawled/derived value. Only the admin NSFW control reads it; the page shows
	 *  no NSFW badge to ordinary readers (a series they can see is one their own
	 *  `show_nsfw` preference already admits). */
	isNsfw: boolean;
}

/** One cover in the series-page gallery (F2). URLs are proxy-ready — render via
 *  the {@link Cover} component / ImageProvider, not as a raw <img src>. */
export interface CoverView {
	thumbnailUrl: string;
	url: string;
	lang: string | null;
	volume: string | null;
	isPrimary: boolean;
}
export interface RelatedView {
	title: string;
	genre: string;
	ch: number;
	rating: string;
	cover?: string;
	id?: string;
}
export interface SeriesView {
	detail: SeriesDetailView;
	chapters: SeriesChapterView[];
	related: RelatedView[];
	/** Available translators (sources) for this work; empty for single-source
	 *  (numeric Suwayomi) series that have no per-source alternatives. */
	translators: TranslatorOption[];
	/** The selected translator's key (matches one of {@link translators}). */
	selectedTranslatorKey: string | null;
	/** The canonical `w_` workId to switch translators against; null when N/A. */
	workId: string | null;
}

const STATUS_WORD: Record<SeriesStatus, string> = {
	ONGOING: 'ONGOING',
	COMPLETED: 'COMPLETED',
	HIATUS: 'HIATUS',
	CANCELLED: 'CANCELLED',
	UNKNOWN: 'ONGOING',
};

function toRelated(p: Series): RelatedView {
	return {
		title: p.title,
		genre: p.genres[0] ?? '',
		ch: p.chapterCount,
		rating: p.rating.average.toFixed(1),
		cover: p.coverUrl,
		id: p.id,
	};
}

/** Pick related series from a candidate pool, preferring shared genres. */
function relatedFor(s: Series, pool: Series[]): RelatedView[] {
	const others = pool.filter((p) => p.id !== s.id);
	const shared = others.filter((p) => p.genres.some((g) => s.genres.includes(g)));
	const picks = (shared.length ? shared : others).slice(0, 8);
	return picks.map(toRelated);
}

interface TranslatorMeta {
	translators: TranslatorOption[];
	selectedTranslatorKey: string | null;
	workId: string | null;
}

const NO_TRANSLATORS: TranslatorMeta = {
	translators: [],
	selectedTranslatorKey: null,
	workId: null,
};

/** The viewer's/app language tag (lowercased), e.g. `en`, `pt-br`. */
function appLang(): string {
	if (typeof navigator !== 'undefined' && navigator.language)
		return navigator.language.toLowerCase();
	return 'en';
}

/**
 * Pick the localized description best matching the app language from the work's
 * enrichment: exact tag → base language (`pt` for `pt-br`) → any English → the
 * work's default description. Returns the chosen text + its language tag.
 */
function pickLocalizedDescription(
	enrich: Series | null,
	fallback: string,
): { text: string; lang: string | null } {
	const list = enrich?.localizedDescriptions ?? [];
	if (!list.length) return { text: fallback, lang: null };
	const want = appLang();
	const base = want.split('-')[0];
	const exact = list.find((d) => d.lang.toLowerCase() === want);
	const baseMatch = list.find((d) => d.lang.toLowerCase().split('-')[0] === base);
	const en = list.find((d) => d.lang.toLowerCase().split('-')[0] === 'en');
	const chosen = exact ?? baseMatch ?? en ?? null;
	return chosen ? { text: chosen.description, lang: chosen.lang } : { text: fallback, lang: null };
}

function mapSeriesView(
	s: Series,
	chs: Chapter[],
	pool: Series[],
	preserveOrder = false,
	tmeta: TranslatorMeta = NO_TRANSLATORS,
	enrich: Series | null = null,
): SeriesView {
	const type = toViewType(s.type);
	const desc = pickLocalizedDescription(enrich, s.description ?? '');
	const credits = enrich?.credits ?? [];
	const descriptions = enrich?.localizedDescriptions ?? [];
	const covers: CoverView[] = (enrich?.covers ?? []).map((c) => ({
		thumbnailUrl: c.thumbnailUrl,
		url: c.url,
		lang: c.lang,
		volume: c.volume,
		isPrimary: c.isPrimary,
	}));
	// Canonical chapters are already server-ordered ascending (number-less last); sorting
	// by number would float a oneshot (wire value 0) ahead of ch. 1 (CR4). The Suwayomi
	// path keeps its explicit ascending sort.
	const asc = preserveOrder ? chs : [...chs].sort((a, b) => a.number - b.number);
	const firstUnread = asc.find((c) => !c.read) ?? asc[0];
	return {
		detail: {
			id: s.id,
			title: s.title,
			// Dedupe defensively: a keyed {#each} on the title text throws on a repeat.
			altTitles: [...new Set(s.altTitles ?? [])],
			type,
			flag: FLAG[type],
			rating: s.rating.average.toFixed(1),
			votes: String(s.rating.count),
			totalCh: s.chapterCount || chs.length,
			viewsTotal: s.views?.total ?? 0,
			// Same clock as the cards (chapter release, not our poll) so the header and
			// the feed that linked here can never disagree — see {@link chapterRecency}.
			// `chs` is passed so a series the server hasn't cached chapters for yet
			// still reads its release time off the list rendered below, instead of
			// falling through to the poll clock and claiming "released now".
			updated: relTime(chapterRecency(s, chs)) || 'recently',
			detected: relTime(s.detectedAt),
			statusLabel: STATUS_WORD[s.status],
			author: s.author ?? '',
			artist: s.artist ?? '',
			genres: s.genres,
			synopsis: desc.text,
			cover: s.coverUrl,
			continueCh: firstUnread?.number ?? 1,
			startChapterId: firstUnread?.id,
			isMarked: s.isMarked,
			isFavorite: s.isFavorite ?? false,
			libraryStatus: (s.libraryStatus as Shelf | null) ?? null,
			credits,
			descriptions,
			descLang: desc.lang,
			covers,
			isNsfw: s.isNsfw ?? false,
		},
		// Emit chapters in a CANONICAL ascending order (oldest → newest) so the
		// series page can sort deterministically. Sources deliver chapters in
		// inconsistent orders (some newest-first, some oldest-first), so we don't
		// trust arrival order: map `asc` (sorted by number), and mark the highest 3
		// numbers as "new" (matching the aggregated path in buildAggregatedChapters).
		chapters: asc.map((c, i) => ({
			id: c.id,
			n: c.number,
			title: c.title || `Chapter ${c.number}`,
			date: relTime(c.uploadedAt),
			isNew: i >= asc.length - 3,
			read: c.read,
		})),
		related: relatedFor(s, pool),
		translators: tmeta.translators,
		selectedTranslatorKey: tmeta.selectedTranslatorKey,
		workId: tmeta.workId,
	};
}

/**
 * Build the series-page chapter list from the server's multi-source aggregation
 * (S2): one row per chapter NUMBER across ALL a work's sources. Each row picks
 * the source to read from — the work's preferred translator when it carries that
 * chapter, otherwise the first source that does (per-chapter fallback, so a
 * chapter the preferred source lacks is still readable). Read-state + dates are
 * enriched from the preferred translator's own chapters where they line up.
 */
function buildAggregatedChapters(
	agg: AggregatedChapter[],
	resolved: ResolvedWork,
): SeriesChapterView[] {
	const selectedSuw = resolved.selected.suwayomiMangaId ?? null;
	// The selected translator's OWN chapters are guaranteed readable (they came from
	// `chapters(selectedSuw)` / the mirror). Index them by number so an aggregated
	// chapter the translator also carries is read from IT (its real id + date +
	// read-state), sidestepping any drift between the aggregation and a source's
	// live `chapters()`.
	const readable = new Map(resolved.chapters.map((c) => [c.number, c]));
	// Fallback source for numbers the selected translator lacks: any Suwayomi source
	// (readable via `chapters()`), else the MangaDex mirror.
	const pickSource = (sources: AggregatedChapter['sources']) =>
		sources.find((s) => s.suwayomiMangaId) ?? sources[0];
	const asc = [...agg].sort((a, b) => a.number - b.number);
	return asc.map((c, i) => {
		const own = readable.get(c.number);
		let id: string | undefined;
		let src: string | null;
		let read = false;
		let date = '';
		if (own) {
			id = own.id;
			src = selectedSuw;
			read = own.read;
			date = relTime(own.uploadedAt);
		} else {
			// A chapter the selected translator doesn't carry. `AggregatedChapter`
			// exposes no upload timestamp, so there is genuinely no honest date to
			// show here — `date` stays '' and the row renders WITHOUT a date line
			// (the series page guards on it). Emitting an empty <div> instead left
			// blank gaps interleaved with dated rows on every multi-source work.
			const chosen = pickSource(c.sources);
			id = chosen?.chapterId;
			src = chosen?.suwayomiMangaId ?? null;
		}
		return {
			id,
			n: c.number,
			title: c.title || `Chapter ${c.number}`,
			date,
			// "New" = the highest-numbered chapters (shown first when sorted newest).
			isNew: i >= asc.length - 3,
			read,
			src,
		};
	});
}

/** A series-detail view plus an honest failure flag (RC4). `view` is null for a
 *  genuinely missing series; `error` is true only when the backend request threw
 *  (outage) — the page then shows an error state, not "series not found". */
export interface SeriesResult {
	view: SeriesView | null;
	error: boolean;
}

export async function getSeries(id: string): Promise<SeriesResult> {
	const r = await liveResult<SeriesView | null>(async () => {
		// Canonical (`w_`) works: resolve the selected translator (persisted
		// preference or a sensible default) and render ITS chapters + metadata. This
		// covers both federation-only works (no MangaDex mirror) and true mirrors
		// (the canonical spine appears as one translator). See {@link resolveWork}.
		if (isCanonicalId(id)) {
			const preferred = getPreferredTranslator(id);
			// The multi-source aggregation rides along in the single `seriesPage` round
			// trip when the backend supports it (`withAggregated`). Only the LEGACY
			// fan-out path still wants a separate `aggregatedChapters` request — kicked
			// off here, alongside the (much slower, multi-round-trip) translator
			// resolution, and consumed below.
			const aggP = backend.seriesPage
				? null
				: backend.aggregatedChapters
					? backend.aggregatedChapters(id).catch(() => [] as AggregatedChapter[])
					: Promise.resolve([] as AggregatedChapter[]);
			const resolved = await resolveWork(id, preferred, { withAggregated: true });
			if (!resolved || !resolved.meta) {
				void aggP?.catch(() => {});
				return null;
			}
			// The detail id must stay a canonical `w_` workId (even though metadata
			// comes from the selected translator's source series), so chapter links,
			// the reader route and library marking all route through the canonical
			// path — where the persisted translator preference is resolved again.
			//
			// It must NOT, however, be the REQUESTED id when the server resolved the
			// request to a different work: `work_redirect` means a merged-away id
			// answers with the SURVIVOR, and pinning the retired id back on made every
			// downstream write (mark/favourite/shelf/review), every chapter link and
			// the URL itself keep addressing a dead work forever. Prefer what the
			// backend actually resolved; fall back to the requested id for
			// federation-only works, where no canonical row is available to ask.
			const effectiveId = resolved.canonicalId ?? id;
			// The translator preference is keyed by work id, so a redirect would strand
			// it under the retired key and silently reset the reader's chosen source on
			// their next visit (which now uses the survivor's id). Carry it over — but
			// only when the survivor can actually satisfy it, i.e. the resolver picked
			// that very translator, so we never persist a key that doesn't resolve.
			if (
				resolved.canonicalId &&
				preferred &&
				resolved.selected.key === preferred &&
				!getPreferredTranslator(effectiveId)
			) {
				setPreferredTranslator(effectiveId, preferred);
			}
			const metaForView: Series = { ...resolved.meta, id: effectiveId };
			const view = mapSeriesView(
				metaForView,
				resolved.chapters,
				[],
				resolved.preserveOrder,
				{
					translators: resolved.translators,
					selectedTranslatorKey: resolved.selected.key,
					// Survivor id too: a translator switch persists its preference under
					// this key and refetches with it, so both sides of that round trip
					// must agree on WHICH work is being read.
					workId: effectiveId,
				},
				resolved.canonSeries,
			);
			// THE SELECTED SOURCE OWNS THE CHAPTER LIST. `mapSeriesView` above was
			// already given `resolved.chapters` — the selected translator's OWN chapters
			// — so switching source in the picker now visibly re-lists.
			//
			// This used to unconditionally overwrite `view.chapters` with the cross-source
			// UNION (S2), which made the picker look broken: you could switch between
			// sources with 201 and 7 chapters and the list never changed, because the
			// union won every time. The source picker is the control the reader reaches
			// for to answer "show me THIS source's chapters", and it has to actually do
			// that.
			//
			// S2's motivating case survives without the union: it was works whose MangaDex
			// spine has 0 chapters while another source has them (Solo Leveling → Asura's
			// 201), and `resolveWork` already DEFAULTS the selection to the source with
			// the most chapters, so that work still opens on Asura's 201.
			//
			// The union is kept as a FALLBACK for the one case the default can't cover: a
			// PERSISTED preference pointing at a source that has since lost its chapters.
			// Rather than show an empty chapter list, fall back to everything we know
			// exists across sources.
			// Bundle path: aggregation came back inside `seriesPage`. Legacy path: the
			// parallel request above. Drift corner (seriesPage present but the SERVER
			// predates it, so resolveWork fell back internally): fetch it now — one
			// extra round trip, only until the server deploy catches up.
			const agg =
				resolved.aggregated ??
				(await (aggP ??
					(backend.aggregatedChapters
						? backend.aggregatedChapters(id).catch(() => [] as AggregatedChapter[])
						: Promise.resolve([] as AggregatedChapter[]))));
			if (agg.length && !view.chapters.length) {
				const rows = buildAggregatedChapters(agg, resolved);
				view.chapters = rows;
				view.detail.totalCh = rows.length;
				const firstUnread = rows.find((c) => !c.read) ?? rows[0];
				view.detail.continueCh = firstUnread?.n ?? 1;
				view.detail.startChapterId = firstUnread?.id;
				view.detail.startChapterSrc = firstUnread?.src ?? null;
			}
			return view;
		}
		// A numeric Suwayomi id addresses ONE source mapping — but the WORK behind it
		// may have several, and then this page owes the reader a source picker.
		//
		// This branch used to assume "numeric ⇒ single source, no picker", which was
		// wrong for every Suwayomi-ANCHORED work: `browse_catalogue.reader_id` links
		// those by their numeric id (a work with no MangaDex mapping has no `w_` id to
		// link), so a work like `A Wimp's Strategy Guide to Conquer the Tower` — three
		// Suwayomi sources — opened at `/series/241` and rendered as if it had one. The
		// id on the URL was the only thing single about it.
		//
		// So: resolve the owning work and hand the page to the canonical branch above,
		// which already builds the translator list, honours the persisted preference and
		// keys library/progress writes to the work rather than to one of its mirrors.
		// The recursion terminates — a `w_` id can only take the canonical branch.
		//
		// `chapters`/`pool` are kicked off ALONGSIDE the series rather than after it, so
		// the fall-through path below costs exactly what it did before; when we delegate
		// they are simply discarded (pre-`catch`ed, so a discarded rejection can't
		// surface as an unhandled one).
		//
		// Falling THROUGH on a null/failed canonical view is deliberate: a work we can't
		// resolve is still a series we can render from this one source. The picker is
		// worth a round trip, never the page.
		const chaptersP = backend.chapters(id);
		// A DISCARDED rejection (delegate path) must not surface as an unhandled one —
		// but this must not become a `.catch(() => [])` fallback either: on the
		// fall-through path a chapters outage still has to fail the page honestly rather
		// than render a real series as having no chapters. Handling it on a SEPARATE
		// branch leaves `chaptersP` itself rejecting for the awaiter below.
		void chaptersP.catch(() => {});
		// A candidate pool to seed related-by-genre. A pool failure just yields no
		// related — it never fails the page.
		//
		// The sort is PINNED rather than left to the server's default. This call is not a
		// browse: it wants a stable, cheap slab of recent catalogue rows to intersect
		// genres against, and it runs on EVERY Suwayomi series view. Left unpinned it
		// would silently inherit TRENDING — whose 24h-view ranking makes the "related"
		// strip change under the reader between two visits to the same series for reasons
		// that have nothing to do with the series — and would follow any future change of
		// the default. NEWEST is deterministic over a fixed catalogue. (Page size is the
		// server's to choose: this path now receives 30 rows rather than 20, which only
		// widens the pool `relatedFor` picks its 8 from.)
		const poolP = backend
			.search('', 1, { sort: 'NEWEST' })
			.then((r) => r.items)
			.catch(() => [] as Series[]);
		const s = await backend.series(id);
		if (s.workId && isCanonicalId(s.workId)) {
			const canonical = await getSeries(s.workId);
			if (canonical.view) return canonical.view;
		}
		const [chs, pool] = await Promise.all([chaptersP, poolP]);
		return mapSeriesView(s, chs, pool);
	}, null);
	return { view: r.data, error: r.error };
}

/**
 * The outcome of an optimistic library write.
 *
 * These used to swallow the backend error and return the OPTIMISTIC argument, so a
 * caller could not tell "saved" from "failed": with an expired token the button
 * flipped to "In Library", stayed there, and a reload silently revealed nothing had
 * been saved — no error shown at any point. `ok` makes that distinguishable and
 * `value` is always the state the UI should actually display: the server's answer on
 * success, the PRE-CLICK value on failure (i.e. the optimistic update rolled back).
 */
export interface WriteResult<T> {
	ok: boolean;
	value: T;
	/** Viewer-facing message; only set when `ok` is false. */
	error?: string;
}

/** Turn a backend rejection into something worth showing a reader. */
function writeError(err: unknown, action: string): string {
	const msg = err instanceof Error ? err.message : String(err);
	if (/not authenticated|unauthenticated|unauthoriz/i.test(msg))
		return 'Your session expired — sign in again to save this.';
	return `Couldn’t ${action}. Check your connection and try again.`;
}

/**
 * Add/remove a series from the library. `previous` is the state to roll back to on
 * failure (defaults to the inverse, since this is a toggle). Mock/offline mode is
 * NOT a failure — there's no server to disagree with, so it reports `ok`.
 */
export async function setLibraryMark(
	seriesId: string,
	marked: boolean,
	previous: boolean = !marked,
): Promise<WriteResult<boolean>> {
	if (!LIVE) return { ok: true, value: marked };
	// Both numeric Suwayomi ids and `w_` canonical ids persist: the server `mark`
	// resolver routes on id shape (canonical → `canonical_library`) (CR6), so no
	// client-side id-shape guard is needed here — do NOT early-return the optimistic
	// value for `w_` ids, that would short-circuit canonical library persistence.
	try {
		const s = await backend.mark(seriesId, marked);
		return { ok: true, value: s.isMarked };
	} catch (err) {
		console.warn('[komika] mark failed:', err);
		return {
			ok: false,
			value: previous,
			error: writeError(err, marked ? 'add this to your library' : 'remove this from your library'),
		};
	}
}

/**
 * File a series under an explicit shelf (`reading`/`completed`/`onhold`/`plan`), or
 * pass `null` to clear it back to progress-derived shelving. Adds the series to the
 * library if it isn't there yet. `previous` is the shelf to roll back to on failure.
 * No-op backends (Suwayomi/native) resolve optimistically and report `ok`.
 */
export async function setLibraryStatus(
	seriesId: string,
	status: Shelf | null,
	previous: Shelf | null = null,
): Promise<WriteResult<Shelf | null>> {
	if (!LIVE || !backend.setLibraryStatus) return { ok: true, value: status };
	try {
		const s = await backend.setLibraryStatus(seriesId, status);
		return { ok: true, value: (s.libraryStatus as Shelf | null) ?? null };
	} catch (err) {
		console.warn('[komika] setLibraryStatus failed:', err);
		return { ok: false, value: previous, error: writeError(err, 'move this series') };
	}
}

/**
 * Toggle the viewer's favourite flag on a series (adds it to the library if absent).
 * `previous` is the state to roll back to on failure (defaults to the inverse).
 */
export async function setFavorite(
	seriesId: string,
	favorite: boolean,
	previous: boolean = !favorite,
): Promise<WriteResult<boolean>> {
	if (!LIVE || !backend.setFavorite) return { ok: true, value: favorite };
	try {
		const s = await backend.setFavorite(seriesId, favorite);
		return { ok: true, value: s.isFavorite ?? favorite };
	} catch (err) {
		console.warn('[komika] setFavorite failed:', err);
		return {
			ok: false,
			value: previous,
			error: writeError(err, favorite ? 'favourite this series' : 'unfavourite this series'),
		};
	}
}

/**
 * The admin NSFW override pinned on a series' work, for the reader's admin control.
 *
 * Three-valued, and the three values are NOT interchangeable:
 *  • `true` / `false` — an admin has PINNED the flag; the crawler cannot move it.
 *  • `null` — no override; the effective flag is whatever the crawler derived.
 * `undefined` means "couldn't read it" (backend off, not an admin, series not
 * catalogued) — the caller shows no control rather than guessing a state and
 * offering an edit that would be rejected.
 *
 * `seriesAdminMeta` is admin-guarded server-side (`require_admin`), so this is safe
 * to call speculatively; a non-admin simply gets `undefined`.
 */
export async function getSeriesNsfwOverride(seriesId: string): Promise<boolean | null | undefined> {
	if (!LIVE || !backend.seriesAdminMeta || !seriesId) return undefined;
	try {
		const meta = await backend.seriesAdminMeta(seriesId);
		// An UNCATALOGUED series resolves successfully with `workId: null` and every
		// override null — indistinguishable, on `isNsfwOverride` alone, from a
		// catalogued work with no pin. `workId` is what separates them, and the
		// difference matters: `updateSeriesMetadata` rejects an uncatalogued series
		// outright, so reporting `null` here would draw a control whose every option
		// errors. Report "can't read it" instead, and no control is drawn.
		if (meta.workId == null) return undefined;
		return meta.isNsfwOverride;
	} catch (err) {
		// Expected for every non-admin viewer — this is the ONLY signal the reader has
		// that the control shouldn't render, so it is not worth a console warning.
		console.debug('[komika] seriesAdminMeta unavailable:', err);
		return undefined;
	}
}

/**
 * Admin: pin a series' NSFW flag, or pass `null` to clear the pin and fall back to
 * the crawler's derived value. This is the fix for a series the crawler mis-flagged.
 *
 * Writes through `updateSeriesMetadata`, which takes either id shape (numeric
 * Suwayomi or canonical `w_`) and returns the recomputed series, so the caller gets
 * the new EFFECTIVE flag back rather than having to re-derive it — clearing a pin
 * can land on either value. `previous` is the override to roll back to on failure.
 *
 * Authorization is entirely server-side (`require_admin`); the client gate only
 * decides whether to render the control.
 */
export async function setSeriesNsfw(
	seriesId: string,
	override: boolean | null,
	previous: boolean | null,
): Promise<WriteResult<{ override: boolean | null; effective: boolean | null }>> {
	if (!LIVE || !backend.updateSeriesMetadata) {
		return { ok: true, value: { override, effective: null } };
	}
	try {
		// `isNsfw` is the only key sent: the input is three-valued per FIELD (omitted =
		// leave alone, null = clear the override, value = pin it), so sending just this
		// one cannot disturb the title/description/type/tag overrides.
		const s = await backend.updateSeriesMetadata({ seriesId, isNsfw: override });
		return { ok: true, value: { override, effective: s.isNsfw ?? null } };
	} catch (err) {
		console.warn('[komika] setSeriesNsfw failed:', err);
		return {
			ok: false,
			value: { override: previous, effective: null },
			error: writeError(err, 'change this series’ NSFW rating'),
		};
	}
}

/** Persist reading progress for a chapter (best-effort; no-op offline/mock). */
export async function saveProgress(
	chapterId: string | undefined,
	lastPageRead: number,
	read: boolean,
): Promise<void> {
	if (!LIVE || !chapterId) return;
	// Both numeric Suwayomi chapter ids and MangaDex-uuid canonical ids persist:
	// the server routes on id shape (canonical → `canonical_progress`) (CR6). The
	// non-empty guard above is the only shape check the client needs.
	try {
		await backend.setProgress(chapterId, lastPageRead, read);
	} catch (err) {
		console.warn('[komika] setProgress failed:', err);
	}
}

/**
 * Record one view (a chapter open) for a series — the popularity signal behind
 * Trending and the series-page view count. Fire-and-forget: no auth (anonymous reads
 * count too), and a failure never affects reading. `seriesId` is the reader's series
 * id (`w_` work or numeric); the server normalises it. No-op offline/mock or when the
 * backend doesn't track views.
 */
export async function recordView(seriesId: string | undefined): Promise<void> {
	if (!LIVE || !seriesId || !backend.recordView) return;
	try {
		await backend.recordView(seriesId);
	} catch (err) {
		console.warn('[komika] recordView failed:', err);
	}
}

// ---- reader ----------------------------------------------------------------

export interface ReaderPageView {
	url: string; // resolved displayable URL ('' → render placeholder)
	index: number;
	ratio: string;
	label: string;
	dim: string;
}
export interface ReaderChapterRef {
	id?: string;
	n: number;
	title: string;
}
export interface ReaderView {
	seriesId: string;
	seriesTitle: string;
	chapterId?: string;
	chNum: number;
	chTitle: string;
	pages: ReaderPageView[];
	chapters: ReaderChapterRef[];
	prevChapterId?: string;
	nextChapterId?: string;
	/** Available translators (sources) for the work; empty for single-source series. */
	translators: TranslatorOption[];
	selectedTranslatorKey: string | null;
	/** Canonical `w_` workId to switch translators against; null for single-source. */
	workId: string | null;
	/** The Suwayomi manga id of the source these chapters are read from (S2), so
	 *  prev/next stay on the same source; null = the MangaDex mirror / default. */
	readingSrc: string | null;
}

/** Honest empty reader view — no chapter/pages available for this series. */
function emptyReader(seriesId: string, tmeta: TranslatorMeta = NO_TRANSLATORS): ReaderView {
	return {
		seriesId,
		seriesTitle: '',
		chapterId: undefined,
		chNum: 0,
		chTitle: '',
		pages: [],
		chapters: [],
		prevChapterId: undefined,
		nextChapterId: undefined,
		translators: tmeta.translators,
		selectedTranslatorKey: tmeta.selectedTranslatorKey,
		workId: tmeta.workId,
		readingSrc: null,
	};
}

/**
 * Resolve a chapter's pages to displayable URLs, tolerating per-page failures.
 *
 * On web this is a pure URL rewrite that cannot reject, but the native provider
 * actually pulls the bytes through Rust, so a single page can fail on an upstream
 * 502/403, a timeout, the size cap or the SSRF guard. Under `Promise.all` that
 * first rejection propagates out of {@link live}, which swallows it and hands back
 * `emptyReader` — one bad page out of forty and the reader claims the WHOLE chapter
 * has none. Settle instead and give the failures an empty `url`: they keep their
 * index and label in {@link buildReaderView}, so the reader renders its per-page
 * placeholder for exactly those slots and every other page reads normally.
 */
async function resolvePageUrls(domainPages: Page[]): Promise<string[]> {
	const settled = await Promise.allSettled(domainPages.map((p) => images.resolvePage(p)));
	return settled.map((r, i) => {
		if (r.status === 'fulfilled') return r.value;
		console.warn(`[komika] page ${i + 1} failed to resolve:`, r.reason);
		return '';
	});
}

/** Assemble a ReaderView from a resolved chapter list + target chapter's pages. */
function buildReaderView(
	seriesId: string,
	chs: Chapter[],
	preserveOrder: boolean,
	target: Chapter,
	urls: string[],
	seriesTitle: string,
	tmeta: TranslatorMeta,
	readingSrc: string | null,
): ReaderView {
	const asc = preserveOrder ? chs : [...chs].sort((a, b) => a.number - b.number);
	const idx = asc.findIndex((c) => c.id === target.id);
	return {
		seriesId,
		seriesTitle: seriesTitle || 'Reader',
		chapterId: target.id,
		chNum: target.number,
		chTitle: target.title || `Chapter ${target.number}`,
		pages: urls.map((url, index) => ({
			url,
			index,
			ratio: '800 / 1200',
			label: String(index + 1).padStart(2, '0'),
			dim: '',
		})),
		chapters: asc
			.slice()
			.reverse()
			.map((c) => ({ id: c.id, n: c.number, title: c.title || `Chapter ${c.number}` })),
		prevChapterId: idx > 0 ? asc[idx - 1].id : undefined,
		nextChapterId: idx >= 0 && idx < asc.length - 1 ? asc[idx + 1].id : undefined,
		translators: tmeta.translators,
		selectedTranslatorKey: tmeta.selectedTranslatorKey,
		workId: tmeta.workId,
		readingSrc,
	};
}

/**
 * Whether a chapter id addresses a Suwayomi chapter (served by `backend.pages`) rather
 * than a canonical MangaDex one (served by `backend.canonicalPages`).
 *
 * The two id spaces are disjoint BY CONSTRUCTION, which is the whole reason the page
 * fetch can pick its resolver before the chapter list has come back: a Suwayomi chapter
 * id is the engine's integer row id (the server parses it with `parse::<i64>()` in both
 * `chapters` and `pages`), while a canonical chapter id is the MangaDex chapter uuid the
 * mirror stores. Neither resolver can even accept the other's ids.
 */
function isSuwayomiChapterId(id: string): boolean {
	return /^\d+$/.test(id);
}

/**
 * Start the page fetch for an ALREADY-KNOWN chapter id so it flies in parallel with the
 * chapter-list resolution instead of queueing behind it.
 *
 * Mirrors the resolver choice the sequential code makes from `activeSpine` — including
 * its `backend.canonicalPages` presence guard, so a canonical uuid on a backend that has
 * no `canonicalPages` still goes to `pages` exactly as before (and fails the same way).
 */
function startPages(chapterId: string): Promise<Page[]> {
	return !isSuwayomiChapterId(chapterId) && backend.canonicalPages
		? backend.canonicalPages(chapterId)
		: backend.pages(chapterId);
}

/**
 * Whether starting {@link startPages} early is guaranteed to return the SAME pages the
 * sequential order would have. Speculation is a latency trick; it must never change what
 * comes back.
 *
 * A Suwayomi id always resolves through `pages`, which every backend (including the
 * native composite) forwards straight to the hosted API — no ordering dependency, safe
 * everywhere. A canonical id resolves through `canonicalPages`, and the native
 * CompositeBackend serves those from the embedded engine ONLY for chapters a PRECEDING
 * `canonicalChapters` reconciled into its local chapter map. Jumping that queue would
 * silently demote the first open of a work to the hosted proxy, so the native build keeps
 * the sequential order for canonical chapters and simply forgoes the speedup.
 */
function canStartPagesEarly(chapterId: string): boolean {
	return isSuwayomiChapterId(chapterId) || !(config.nativeEngine && isTauri());
}

export function getReaderChapter(
	seriesId: string,
	chParam?: string | null,
	srcParam?: string | null,
): Promise<ReaderView> {
	return live(async () => {
		// Pages are the ONLY fetch whose result the reader needs in order to paint a
		// single image, yet they used to sit LAST, behind the entire chapter-list
		// resolution — three or four sequential round-trips before the first image was
		// even requested. When the caller already names the chapter (`?ch=`, which
		// next-chapter navigation always carries) the id by itself decides which resolver
		// serves it, so fire it NOW and await it where the sequential version issued it.
		//
		// The shape gate keeps the speculation honest for the series kind: a numeric
		// Suwayomi series has nothing but integer chapter ids, so a uuid `?ch=` there is
		// garbage and would only buy a pointless `canonicalPages` request.
		const speculatable =
			!!chParam &&
			(isCanonicalId(seriesId) || isSuwayomiChapterId(chParam)) &&
			canStartPagesEarly(chParam);
		const speculatedPages = speculatable ? startPages(chParam as string) : null;
		// A speculation the chosen path ends up NOT using — a `?ch=` no source carries, or
		// one a numeric series' list doesn't contain — must not surface as an unhandled
		// rejection. This no-op handler marks the ORIGINAL promise handled without
		// consuming it: the awaits below still see, and still propagate, the real error on
		// every path that does use the result.
		speculatedPages?.catch(() => {});
		// Canonical (`w_`) works read through the selected translator (persisted
		// preference or the same default the series page picked — shared resolver).
		if (isCanonicalId(seriesId)) {
			const preferred = getPreferredTranslator(seriesId);
			const resolved = await resolveWork(seriesId, preferred);
			if (!resolved) return emptyReader(seriesId);
			// Same `work_redirect` healing as getSeries: a reader URL bookmarked before
			// a merge resolves to the SURVIVOR, so the back-to-series link and the
			// translator switch (which does a real `goto(/read/<workId>)`) address the
			// live work instead of pinning the retired id.
			const effectiveId = resolved.canonicalId ?? seriesId;
			// S2: a chapter may be provided by a source OTHER than the preferred
			// translator (per-chapter fallback from the aggregated list). When `src`
			// names such a source, read from it (and highlight it in the switcher);
			// otherwise use the preferred translator's already-fetched chapters.
			const selectedSuw = resolved.selected.suwayomiMangaId ?? null;
			const readFromSrc = !!srcParam && srcParam !== selectedSuw;
			const srcTranslator = readFromSrc
				? resolved.translators.find((t) => t.suwayomiMangaId === srcParam)
				: undefined;
			const tmeta: TranslatorMeta = {
				translators: resolved.translators,
				selectedTranslatorKey: srcTranslator?.key ?? resolved.selected.key,
				workId: effectiveId,
			};
			const spine = readFromSrc ? false : resolved.selected.suwayomiMangaId === null;
			// `resolveWork` ALREADY fetched every candidate source's chapters — that fan-out
			// is what computes the picker's per-source counts — so a `?src=` naming one of
			// this work's own sources is a list we are already holding, and refetching it was
			// a whole extra round-trip on the path the reader takes most (next-chapter
			// navigation always re-appends `src`). Same call, same argument, same request:
			// `candidates` carries exactly what `backend.chapters(srcParam)` would return,
			// including the empty list a failed fetch degrades to.
			//
			// The network fetch stays as the fallback for a `src` that is NOT one of the
			// work's mappings — a hand-edited URL, or a mapping dropped since the link was
			// made. `candidates` cannot answer for those, and today they still read, so they
			// must keep reading.
			const cachedSrcChapters = readFromSrc
				? resolved.candidates.find((c) => c.suwayomiMangaId === srcParam)?.chapters
				: undefined;
			const chs = readFromSrc
				? (cachedSrcChapters ??
					(await backend.chapters(srcParam as string).catch(() => [] as Chapter[])))
				: resolved.chapters;
			// An empty ACTIVE source is only a dead end when there is no `?ch=` for
			// another source to satisfy — the picker deliberately offers sources that
			// carry no chapters (a MangaDex takedown leaves exactly that), so bailing
			// here unconditionally would reproduce the blank reader the lookup below
			// exists to prevent.
			if (!chs.length && !chParam) {
				const empty = emptyReader(effectiveId, tmeta);
				return { ...empty, seriesTitle: resolved.meta?.title ?? '' };
			}
			let preserveOrder = readFromSrc ? false : resolved.preserveOrder;
			let activeChs = chs;
			let activeSpine = spine;
			let activeSrc = readFromSrc
				? (srcParam as string | null)
				: (resolved.selected.suwayomiMangaId ?? null);
			let target = chParam ? chs.find((c) => c.id === chParam) : undefined;
			// A `?ch=` the ACTIVE source doesn't carry is routine, not corrupt: sources
			// don't share chapter ids, and a "new chapter" notification links whichever
			// source the scanner saw it on (`notifications.ts` has no `src` to attach)
			// while the reader defaults to the source with the most chapters. This used
			// to return the no-pages state, which renders a blank reader that requests
			// ZERO images — indistinguishable, from the outside, from a broken image
			// proxy. Open the chapter from the source that actually has it instead.
			//
			// Matching is by chapter ID only, so the original invariant is intact: it is
			// still impossible to silently open a DIFFERENT chapter. Finding the same id
			// under another source isn't a substitution — it's the same chapter, located.
			if (chParam && !target) {
				const owner = findChapterOwner(
					resolved.candidates,
					chParam,
					readFromSrc ? (srcTranslator?.key ?? null) : resolved.selected.key,
				);
				// Genuinely unknown to every source — stay honest and render empty.
				if (!owner) {
					const empty = emptyReader(effectiveId, tmeta);
					return { ...empty, seriesTitle: resolved.meta?.title ?? '' };
				}
				target = owner.chapter;
				activeChs = owner.candidate.chapters;
				activeSrc = owner.candidate.suwayomiMangaId;
				// The spine is the only server-ordered list, and only IT reads through
				// `canonicalPages` — both track the source we actually landed on.
				activeSpine = owner.candidate.suwayomiMangaId === null;
				preserveOrder = activeSpine;
				// Reflect the real source in the picker, so the switcher doesn't claim a
				// source that isn't the one being read. If the owning source is HIDDEN from
				// the picker — a redundant all.mangadex extension the direct spine stands in
				// for — highlight the spine instead of a key the picker never renders (else no
				// chip shows as selected). The content is MangaDex either way, so the spine is
				// the honest highlight.
				const ownerKey = owner.candidate.key;
				const ownerVisible = resolved.translators.some((t) => t.key === ownerKey);
				tmeta.selectedTranslatorKey = ownerVisible ? ownerKey : translatorKey('mangadex', null);
			}
			const asc = preserveOrder ? activeChs : [...activeChs].sort((a, b) => a.number - b.number);
			if (!target) target = asc.find((c) => !c.read) ?? asc[0];
			// Collect the speculative fetch when it is for the chapter we actually landed on.
			// `target.id === chParam` holds for every path that gets here with a `?ch=` —
			// each match above is by chapter ID and an id no source carries returned empty —
			// but the equality is checked rather than assumed: the speculation is valid only
			// for the chapter it was started for, and this is the line that would otherwise
			// hand back someone else's pages if that ever stopped being true.
			const domainPages =
				speculatedPages && target.id === chParam
					? await speculatedPages
					: activeSpine && backend.canonicalPages
						? await backend.canonicalPages(target.id)
						: await backend.pages(target.id);
			const urls = await resolvePageUrls(domainPages);
			const readingSrc = activeSrc;
			// `activeChs`, not `chs`: the list, the prev/next links and the target must
			// all describe the SAME source, or navigation walks a list the reader is
			// not actually on.
			return buildReaderView(
				effectiveId,
				activeChs,
				preserveOrder,
				target,
				urls,
				resolved.meta?.title ?? 'Reader',
				tmeta,
				readingSrc,
			);
		}
		// Numeric Suwayomi series — single source, no translator picker.
		const chs = await backend.chapters(seriesId);
		if (!chs.length) return emptyReader(seriesId);
		const asc = [...chs].sort((a, b) => a.number - b.number);
		let target = chParam ? chs.find((c) => c.id === chParam) : undefined;
		if (!target) target = asc.find((c) => !c.read) ?? asc[0];
		// Unlike the canonical branch, an unknown `?ch=` here silently falls back to the
		// first unread chapter, so the speculation is usable only when the list really does
		// carry the requested id — otherwise it is discarded and the real target is fetched.
		const domainPages =
			speculatedPages && target.id === chParam
				? await speculatedPages
				: await backend.pages(target.id);
		const urls = await resolvePageUrls(domainPages);
		const series = await backend.series(seriesId).catch(() => null);
		return buildReaderView(
			seriesId,
			chs,
			false,
			target,
			urls,
			series?.title ?? 'Reader',
			NO_TRANSLATORS,
			null,
		);
	}, emptyReader(seriesId));
}

export interface ProfileView {
	/** The signed-in user's id (empty for the signed-out sample profile). */
	id: string;
	name: string;
	handle: string;
	since: string;
	bio: string;
	badge: string;
	/** Stored avatar path/URL, or null → render an initial. */
	avatarUrl: string | null;
	stats: { value: string; label: string }[];
	reading: {
		id?: string;
		title: string;
		cover: string;
		genre: string;
		ch: number;
		total: number;
	}[];
	favGenres: { name: string; pct: number }[];
	activity: { id: string; icon: string; iconBg: string; text: string; time: string }[];
	shelves: {
		id?: string;
		title: string;
		cover: string;
		genre: string;
		rating: string;
		shelf: string;
		favorite: boolean;
		ch: number;
		total: number;
	}[];
}

/**
 * The signed-in user's profile, built from their session identity + real reading
 * state. Resolves to `null` when signed out (or backend off/error) — the profile
 * screen renders its sign-in state.
 *
 * NOTE: reading/library reflects the shared Suwayomi state this MVP federates,
 * and "activity" is derived from that progress — there is no per-user timestamped
 * event log yet, so a true activity stream would need a new server capability.
 */
export function getProfile(): Promise<ProfileView | null> {
	return live<ProfileView | null>(async () => {
		const session = await backend.session();
		if (!session) return null;
		const user = session.user;
		// Everything below depends only on the SESSION existing, never on each other:
		// the library, its batched read progress and the activity log are three
		// independent queries, so they go out together. (Read progress is one batched
		// query, not a chapters() fetch per series — the old N+1 that stalled this
		// page for a signed-in user with a large library.)
		const [lib, progress, rawActivity] = await Promise.all([
			backend.library().catch(() => [] as Series[]),
			backend.libraryProgress
				? backend.libraryProgress().catch(() => [] as SeriesProgress[])
				: Promise.resolve([] as SeriesProgress[]),
			backend.myActivity
				? backend.myActivity(12).catch(() => [])
				: Promise.resolve([] as Awaited<ReturnType<NonNullable<typeof backend.myActivity>>>),
		]);
		const byId = new Map(progress.map((p) => [p.id, p]));
		const rows = lib.map((s) => {
			const p = byId.get(s.id);
			const total = p?.total || s.chapterCount;
			const read = p?.read ?? 0;
			return { s, total, read };
		});
		const chaptersRead = rows.reduce((n, r) => n + r.read, 0);
		const readingNow = rows.filter((r) => r.read > 0 && r.read < r.total);
		const completed = rows.filter((r) => r.total > 0 && r.read >= r.total);
		const libAvg = lib.length ? lib.reduce((n, s) => n + s.rating.average, 0) / lib.length : 0;

		const freq = new Map<string, number>();
		for (const { s } of rows) for (const g of s.genres) freq.set(g, (freq.get(g) ?? 0) + 1);
		const totalG = [...freq.values()].reduce((a, b) => a + b, 0);
		const favGenres = [...freq.entries()]
			.sort((a, b) => b[1] - a[1])
			.slice(0, 5)
			.map(([name, c]) => ({ name, pct: totalG ? Math.round((c / totalG) * 100) : 0 }));

		const reading = readingNow.slice(0, 6).map(({ s, read, total }) => ({
			id: s.id,
			title: s.title,
			cover: s.coverUrl,
			genre: s.genres[0] ?? '',
			ch: read,
			total,
		}));

		const shelves = rows.map(({ s, read, total }) => ({
			id: s.id,
			title: s.title,
			cover: s.coverUrl,
			genre: s.genres[0] ?? '',
			rating: s.rating.average.toFixed(1),
			// Explicit shelf wins; else derive (completed when fully read, else reading).
			shelf:
				(s.libraryStatus as string | null) ??
				(total > 0 && read >= total ? 'completed' : 'reading'),
			favorite: s.isFavorite ?? false,
			ch: read,
			total,
		}));

		// Real, timestamped activity from the server's per-user event log, mapped to
		// display text via the library title index (falls back to a generic label
		// when the target isn't in the library, e.g. a chapter comment).
		const titleById = new Map(lib.map((s) => [s.id, s.title]));
		const activity = rawActivity.map((a) => {
			const title = a.targetId ? titleById.get(a.targetId) : undefined;
			const on = title ?? (a.targetType === 'chapter' ? 'a chapter' : 'a series');
			const view =
				a.kind === 'review'
					? { icon: '★', iconBg: 'rgba(246,183,60,0.16)', text: `Reviewed ${on}` }
					: a.kind === 'library_add'
						? { icon: '＋', iconBg: 'rgba(95,191,126,0.16)', text: `Added ${on} to library` }
						: { icon: '💬', iconBg: 'rgba(255,255,255,0.07)', text: `Commented on ${on}` };
			return { id: a.id, ...view, time: relTimeAgo(a.createdAt) };
		});

		return {
			id: user.id,
			name: user.displayName?.trim() || user.username,
			handle: `@${user.username}`,
			since: `Joined ${monthYear(user.joinedAt)}`,
			bio:
				user.bio?.trim() ||
				(lib.length
					? `${chaptersRead} chapters read across ${lib.length} series in your library.`
					: 'Your library is empty — add a series to start tracking your reading.'),
			badge: user.isAdmin ? 'ADMIN' : 'READER',
			avatarUrl: user.avatarUrl,
			stats: [
				{ value: String(lib.length), label: 'In library' },
				{ value: String(chaptersRead), label: 'Chapters read' },
				{ value: String(readingNow.length), label: 'Reading now' },
				{ value: libAvg ? libAvg.toFixed(1) : '—', label: 'Library avg' },
			],
			reading,
			favGenres,
			activity,
			shelves,
		};
	}, null);
}

/**
 * Update the signed-in user's editable profile (display name + bio). Returns the
 * refreshed session user, or `null` when no live backend supports it (mock mode).
 */
export async function updateProfile(input: {
	displayName?: string | null;
	bio?: string | null;
}): Promise<import('@komika/api').Session['user'] | null> {
	if (!LIVE || !backend.updateProfile) return null;
	return backend.updateProfile(input);
}

/**
 * Upload a new avatar image. The server squares/downscales it and re-encodes it
 * as budgeted lossless WebP on the VM data volume. Returns the new avatar URL.
 */
export async function uploadAvatar(file: Blob): Promise<string> {
	if (!LIVE || !backend.uploadAvatar) {
		throw new Error('Avatar upload requires the Komiq backend.');
	}
	return backend.uploadAvatar(file);
}

export function getSupport() {
	return Promise.resolve({ supportCategories: content.supportCategories, faqs: content.faqs });
}
