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
	Report,
	ReportInput,
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
// Runtime check (not a type): the native build must keep the sequential page-fetch
// order for canonical chapters — see `canStartPagesEarly`.
import { isTauri } from '@komika/api';
import type { Backend } from '@komika/api';
import { backend, images } from '$lib/context';
import { invalidate } from '$app/navigation';
import { api, type LoadFetch } from './load-fetch';
import { homeLatestAdded } from './home-latest-added';
import { findChapterOwner } from './chapter-owner';
import type { ChapterCandidate } from './chapter-owner';
import { adjacentChapters, copiesOf, normalizeGroup, sortChaptersAsc } from './chapter-versions';
import { loadFirstReadable } from './reader-pages';
import { chapterChip, externalChapterHref } from './chapter-label';
import { chapterDateLabel, chapterFoundLabel } from './chapter-date';
import {
	isRedundantMangadexExt,
	maxSaneChapterNumber,
	pickDefaultKey,
	MANGADEX_EXT_PKG,
} from './translator-select';
import { getPreferredTranslator, setPreferredTranslator } from './translator-pref.svelte';
import { config, apiAssetSrc } from '$lib/config';
import { deriveShelf, FLAG, isEndedStatus } from './types';
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
		// The newest chapter's LABEL and the chapter COUNT, carried as two fields.
		//
		// `ch` was hardcoded '' here because `Series` genuinely had no latest-chapter
		// column — printing the count under a "Ch." label is F4 ("Ch. 412" on a series
		// whose newest release is 10.5), so blank was the only honest render. That
		// premise no longer holds: `SERIES_FIELDS` selects `latestChapter` and the
		// `discovery` resolver hydrates it per page (`suwayomi_latest_chapter_label_batch`),
		// which is why Browse read "115 ch · Ch. 90" while home read nothing at all.
		//
		// The count still does NOT feed `ch` — it feeds `chCount`, and `cardSub` prints
		// them as separate halves. See {@link chapterChip} (Rule 1) before touching this.
		ch: chapterChip(s.latestChapter),
		chCount: s.chapterCount,
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
		status: toViewStatus(s.status),
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
		// Already count-free; routed through {@link chapterChip} only so a WORD label
		// ("Oneshot") isn't rendered as "Ch. Oneshot" here either.
		ch: chapterChip(u.latestChapter),
		time: relTime(u.latestAt),
		timeAt: epochMs(u.latestAt),
		detected: '',
		rating: u.averageRating != null ? u.averageRating.toFixed(1) : '',
		cover: u.coverUrl ?? '',
		id: u.workId,
	};
}

/** A merged-Updates-feed row → Card. The server has already done the interleave, the
 *  by-work dedupe and the paging, so this is a pure field mapping — no sorting, no
 *  slicing, no title dedupe.
 *
 *  `ch` is the newest RELEASED chapter's label and nothing else. It used to fall back to
 *  `u.chapterCount` when the scanner half of the feed sent no number — the series' chapter
 *  COUNT under a chapter-NUMBER label, which is the defect the owner named ("By chapter, I
 *  mean the name of the chapter, not the amount of chapters a series has per source").
 *  That fallback is gone, not merely unused: the server now labels every feed row (0 of
 *  64,922 rows have a NULL `latest_chapter`; 600 sampled live rows were 100% populated),
 *  so restoring it could only ever re-introduce the lie on a future regression.
 *
 *  `rating` is '' when unrated rather than "0.0": the old scanner-half card ran
 *  `RatingSummary::empty().average.toFixed(1)` and printed a 0.0 star on every unrated
 *  series, which is the vast majority (the whole database holds a handful of reviews). */
function toFeedCard(u: UpdateFeedRow): Card {
	return {
		title: u.title,
		ch: chapterChip(u.latestChapter),
		// The count as its own half — never a fallback for `ch` above (that substitution
		// is the F4 defect this mapper's docstring describes); `cardSub` prints both.
		chCount: u.chapterCount ?? undefined,
		// THE ANNOUNCE CLOCK — when we could first show this chapter — falling back to the
		// release clock when the server predates the field or has not floored the column yet.
		//
		// This is the feed's SORT KEY (`ORDER BY announced_at`), and the visible label has to
		// be the same instant the list is ordered by or the page contradicts itself: a
		// chapter a source held behind early access for twelve days would otherwise sit at
		// the top of "what's new" wearing a "12d" label. The release clock is not lost — it
		// moves to the tooltip via `released` below.
		//
		// `?? releasedAt` is exact, not approximate: the two columns are equal on every row
		// recorded before migration 0099 and on every row no source held back.
		time: relTime(u.announcedAt ?? u.releasedAt),
		timeAt: epochMs(u.announcedAt ?? u.releasedAt),
		// The source's own release time, but ONLY when it disagrees with what is on screen —
		// otherwise the tooltip would read "We found it 4h · the source released it 4h".
		released: u.announcedAt && u.announcedAt !== u.releasedAt ? relTime(u.releasedAt) : undefined,
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
		// The column is normalized to the enum's vocabulary at write time (0068), so
		// the same `toViewStatus` fold `Series` rows get applies. `UNKNOWN` → ongoing,
		// matching every other surface; null (pre-field server) hides the badge.
		status: u.status ? toViewStatus(u.status) : undefined,
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
		// The newest chapter's NUMBER, which the COUNT above is not. Routed through
		// `chapterChip` for the same two reasons every other chapter label is: the value
		// can be a WORD ("Oneshot"), which must not come back as "Ch. Oneshot"; and
		// null/undefined/'' must all collapse to '' rather than reach a template that
		// would print "Ch. undefined".
		//
		// NO `?? s.chapterCount` FALLBACK, deliberately. That substitution is F4 — it is
		// what printed "Ch. 412" for a series whose newest chapter is 10.5 — and it is
		// why the server sends null instead of guessing. `undefined` also arrives
		// legitimately from a server that predates the field (OPTIONAL_SERIES_FIELDS
		// strips it), and "older API" must degrade to "no number", not to a wrong one.
		latestCh: chapterChip(s.latestChapter),
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

/**
 * Dedupe a series list by TITLE, preserving first-seen order.
 *
 * `dedupeSeries` (by id) cannot help the hero: the duplicate it shows is one series
 * present under two ids — its numeric Suwayomi identity and its `w_` canonical mirror —
 * which are two different ids and one title. In an 8-slide carousel that is the same
 * cover twice, three seconds apart.
 */
function dedupeSeriesByTitle(list: Series[]): Series[] {
	const seen = new Set<string>();
	const out: Series[] = [];
	for (const s of list) {
		const key = s.title.trim().toLowerCase();
		if (!seen.has(key)) {
			seen.add(key);
			out.push(s);
		}
	}
	return out;
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

/**
 * One hero slide. The hero is an editorial showcase, not a card: it shows the
 * full genre list, the synopsis and the credit, so it carries more of the
 * `Series` than {@link Card} does. Every field here is already selected by the
 * discovery query's `SeriesFields` fragment — no extra round-trip.
 */
export interface FeaturedView {
	title: string;
	genres: string[];
	description: string | null;
	author: string | null;
	type: ComicType;
	ch: number;
	/**
	 * The newest chapter's LABEL — `'Ch. 151'`, `'Oneshot'` — already formatted by
	 * `chapterChip`, or `''` when we do not know it. See {@link CatalogEntry.latestCh}:
	 * this is a SECOND, INDEPENDENT quantity from {@link ch}, which counts how many
	 * chapters we hold. The hero prints both ("Ch. 151 · 12 chapters") for the same
	 * reason Browse does — a partial mirror makes them legitimately far apart, and
	 * printing the count under a "Ch." label is F4.
	 */
	latestCh: string;
	cover: string;
	id?: string;
}

/**
 * Trim a synopsis down to the part that reads as a synopsis.
 *
 * Upstream descriptions routinely append bookkeeping to the blurb — an "Alternative
 * Titles:" list, a "Links:"/"Note:" block, a "Rank: #62 / Rating: 9.38" stat pair, or a
 * `---` rule followed by credits. In a card that text is never visible; in the hero it is
 * 5 clamped lines of the most prominent copy on the site, and the tail regularly eats two
 * of them. Cut at the first such marker, keep everything before it.
 *
 * The marker must begin a LINE (optionally behind list/emphasis punctuation or an opening
 * bracket — `- **Links:**`, `(Source: Viz Media)`), and the cut is taken at that line
 * break, not at the keyword. Both halves of that are load-bearing, and both were measured
 * against 255 live descriptions pulled from this catalogue's own discovery + browse feeds:
 *
 *  • Anchoring is NOT a no-op here, which is the usual objection. Of the 61 descriptions
 *    carrying a marker, ZERO had it mid-sentence — every one sat at a line start. So the
 *    unanchored version bought nothing and cost the whole false-positive class: it cut
 *    "…traced the plague back to its source: the well beneath the shrine" down to "…back
 *    to its", and "The detective notes: nothing here is what it seems" down to "The
 *    detective". A hero synopsis truncated mid-clause reads as a bug in the site.
 *  • Cutting at the line break, not the keyword, is what drops the punctuation that
 *    introduces the tail. Cutting at the word left Ruri Dragon's blurb ending in a
 *    dangling "(" — the opening paren of "(Source: Viz Media, edited)".
 */
const HERO_TAIL =
	/(?:^|[\r\n])[-*>+\s]*[([]?[ \t]*(?:-{3,}|_{3,}|(?:alternative|alt\.?|associated|other)\s+(?:titles?|names?)\s*:|links?\s*:|notes?\s*:|sources?\s*:|ranks?\s*:|ratings?\s*:)/i;

function heroSynopsis(description: string | null): string | null {
	if (!description) return null;
	const cut = HERO_TAIL.exec(description)?.index ?? -1;
	const head = (cut > 0 ? description.slice(0, cut) : description).trim();
	// A description that is ONLY the bookkeeping (cut === 0) has no blurb to show; fall
	// back to the raw text rather than rendering an empty paragraph.
	return head || description.trim() || null;
}

function toFeatured(s: Series): FeaturedView {
	return {
		title: s.title,
		// DEDUPED, because the hero keys its genre chips on the genre string itself and a
		// repeat is not cosmetic there: Svelte 5 throws `each_key_duplicate` in production
		// and the whole landing page dies in the error boundary. A repeat is reachable —
		// a work's tags are the union of its MangaDex tags and its Suwayomi sources' tags,
		// which overlap by construction and are not normalized to one vocabulary.
		genres: [...new Set(s.genres)],
		description: heroSynopsis(s.description),
		author: s.author ?? s.artist,
		type: toViewType(s.type),
		ch: s.chapterCount,
		latestCh: chapterChip(s.latestChapter),
		cover: s.coverUrl,
		id: s.id,
	};
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

/** Whether a series' publication has ENDED, so catching up on it really is finishing
 *  it. Gates the auto-`completed` shelf — see {@link deriveShelf}. */
function isEnded(s: Series): boolean {
	return isEndedStatus(toViewStatus(s.status));
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
	 *
	 * `name`/`iconUrl` mirror the candidate's picker metadata (including entries the
	 * picker HIDES, like the redundant all.mangadex extension) so the series page's
	 * per-chapter provenance cluster can label every source the aggregation reports.
	 */
	candidates: (ChapterCandidate<Chapter> & {
		name: string;
		iconUrl: string | null;
		/**
		 * The all.mangadex extension shadowing a readable direct spine (see
		 * `isRedundantMangadexExt`). Its copies are the spine's copies served by the slow
		 * route, so per-copy lists (the series page's provenance cluster, the reader's
		 * version switcher) drop them wherever the spine carries the same number.
		 */
		redundant: boolean;
	})[];
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
	b: Backend = backend,
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
	const bundle = b.seriesPage
		? await b.seriesPage(workId, opts?.withAggregated ?? false)
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
		const hasWorkSources = !!b.workSources;
		const hasCanonChapters = !!b.canonicalChapters;
		const hasCanonSeries = !!b.canonicalSeries;
		const [wsRes, spineRes, canonRes] = await Promise.allSettled([
			hasWorkSources ? b.workSources!(workId) : Promise.resolve([] as WorkSource[]),
			hasCanonChapters ? b.canonicalChapters!(workId) : Promise.resolve([] as Chapter[]),
			hasCanonSeries ? b.canonicalSeries!(workId) : Promise.resolve(null),
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
					b.chapters(v.view.suwayomiMangaId as string).catch(() => [] as Chapter[]),
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

	// Selection: honour a valid persisted preference, else the candidate that is FURTHEST
	// AHEAD — the highest chapter NUMBER, not the most chapters (F12). A source that picked
	// the series up midway with 70 chapters but reaching ch 151 beats one with 150 chapters
	// that stops at 150; `pickDefaultKey` documents the rule and its mis-merge guard.
	// A redundant all.mangadex extension is never eligible — its content is the direct
	// spine, so a stale persisted preference to it heals here.
	const pickKey = pickDefaultKey(
		ordered.map((c) => ({
			key: c.view.key,
			chapterCount: c.chapters.length,
			// Read off the chapter lists already fetched above — no extra round-trip. The
			// sanity clamp lives in `maxSaneChapterNumber`, and it is load-bearing: two
			// production series carry `chapter_number = 99999999` (a TEST upload) and a date
			// used as a number, either of which would otherwise win every comparison.
			maxChapterNumber: maxSaneChapterNumber(c.chapters.map((ch) => ch.number)),
			redundant: !!c.redundant,
		})),
		preferredKey,
	);
	// Resolve the selection against the PICKER LIST, not against `ordered`. The two are not
	// the same set — `translators` drops the redundant all.mangadex extension — so looking a
	// candidate up in one and reading it out of the other is what made the old
	// `as TranslatorOption` cast necessary: `find` returns `undefined` whenever the picked
	// candidate is not in the list, and the cast asserted that away. It is unreachable today
	// (only a MangaDex extension is ever redundant, and only when the spine candidate exists,
	// which sorts first and is itself never redundant) but the failure it hid is a TypeError
	// on the very next line, i.e. a blank series page.
	const selected = translators.find((t) => t.key === pickKey) ?? translators[0];
	// Every candidate was filtered out of the picker. There is nothing to select and nothing
	// to read, which is the same answer as having no candidates at all.
	if (!selected) return null;
	// …and the chapter list must come from the candidate we actually SELECTED, or the page
	// would render one source's chapters under another source's name.
	const pick = ordered.find((c) => c.view.key === selected.key) ?? ordered[0];

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
		meta = await b.series(selected.suwayomiMangaId).catch(() => null);
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
			name: c.view.name,
			iconUrl: c.view.iconUrl,
			chapters: c.chapters,
			redundant: !!c.redundant,
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
export function getGenreFacets(fetch?: LoadFetch): Promise<{ genre: string; count: number }[]> {
	const b = api(fetch);
	return live(async () => {
		if (!b.genreFacets) return [];
		return b.genreFacets();
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

export function getHome(fetch?: LoadFetch) {
	const b = api(fetch);
	const fallback = {
		featured: [] as FeaturedView[],
		latestUpdates: [] as Card[],
		trending: [] as Card[],
		latestAdded: [] as Card[],
		homeGenres: [] as string[],
	};
	return live(async () => {
		// Discovery drives the curated rows; "Latest Updates" merges OUR scanner's
		// new-chapter detections with the MangaDex mirror's freshest works (so the row
		// fills even while the scanner is still warming up). The hero is the top of the
		// live Popular feed (first feed as fallback) — empty when nothing is live.
		const [feeds, added, latest] = await Promise.all([
			b.discovery().catch(() => [] as DiscoveryFeed[]),
			// Use Browse's endpoint even while discovery is served by an older API.
			homeLatestAdded(b).catch((err) => {
				console.warn('[komika] Latest Added search failed:', err);
				return [] as Series[];
			}),
			// ONE feed, not two merged client-side — see `latestUpdates` below.
			fetchUpdatesFeed(1, null, b).catch(() => ({
				items: [] as UpdateFeedRow[],
				page: 1,
				hasNextPage: false,
				total: null,
			})),
		]);
		const byKind = (k: string) => feeds.find((f) => f.kind === k)?.items ?? [];
		const popular = byKind('POPULAR');
		const pool = dedupeSeries(feeds.flatMap((f) => f.items));
		// The hero is the "trending" showcase, so it reads the TRENDING feed first and
		// only falls back to Popular (then to whatever feed came back) when trending is
		// empty — the label on the slide has to be true. Deduped by title for the same
		// reason the rows are: the catalogue carries same-title rows per source, and two
		// identical slides in an 8-slide carousel is very visible.
		const trending = byKind('TRENDING');
		const heroPool = trending.length
			? trending
			: popular.length
				? popular
				: (feeds[0]?.items ?? []);
		const featured = dedupeSeriesByTitle(heroPool).slice(0, 8).map(toFeatured);
		// THE SAME FEED `/updates` PAGES, which is what puts this row on the announce clock.
		//
		// It used to merge page 1 of `updates` with page 1 of `canonicalUpdates` by release
		// time, client-side. That merge was already the thing `feed_series_updates` was built
		// to replace — the /updates PAGE was migrated onto it and this row was not — and it
		// cannot be ordered by "when we could first show it" at any price: neither legacy feed
		// carries an announce clock, and neither table has anywhere to put one
		// (`feed_updates.latest_at` is TEXT ISO-8601 and `suwayomi_series.latest_chapter_at`
		// is zero-padded TEXT millis, so adding one to either would mean a THIRD encoding of
		// this clock in a schema that already has two).
		//
		// Reading the unified feed instead gets the announce ordering, the by-work dedupe and
		// the interleave from the server for free, and drops a round trip. `fetchUpdatesFeed`
		// keeps its own deploy-order guard, so an older server still degrades to the old
		// two-feed merge rather than emptying the row.
		const latestUpdates = latest.items.map(toFeedCard).slice(0, 10);
		return {
			featured,
			latestUpdates,
			// Dedup by title (like latestUpdates): the catalogue carries same-title rows
			// (per-language sources, and a series under both its numeric + `w_` identity),
			// which would collide on the home row's `{#each}` key and could show twice.
			trending: dedupeCardsByTitle(byKind('TRENDING').map(toCard)).slice(0, 10),
			latestAdded: added.map(toAddedCard),
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
	fetch?: LoadFetch,
): Promise<UpdatesView> {
	const b = api(fetch);
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
			b.discovery().catch(() => [] as DiscoveryFeed[]),
			tab === 'hot' ? Promise.resolve(null) : fetchUpdatesFeed(page, type, b).catch(() => null),
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
	b: Backend = backend,
): Promise<{
	items: UpdateFeedRow[];
	page: number;
	hasNextPage: boolean;
	total: number | null;
}> {
	if (b.updatesFeed) {
		try {
			return await b.updatesFeed(page, type ? VIEW_TO_DOMAIN_TYPE[type] : undefined);
		} catch {
			// Fall through. Deliberately silent: `live()` already reports backend failure
			// to the screen, and this path is a successful degradation, not an error.
		}
	}
	const [updates, canonical] = await Promise.all([
		b.updates().catch(() => ({ items: [] as Series[] })),
		b.canonicalUpdates?.().catch(() => [] as CanonicalUpdate[]) ??
			Promise.resolve([] as CanonicalUpdate[]),
	]);
	// Legacy shape: merge by release time, dedupe by title, cap, and project onto the
	// feed-row shape so `toFeedCard` stays the single card mapper. `workId` echoes `id`
	// (it is only a dedupe key, which the server side already applied); `latestChapter`
	// carries the old mapper's already-formatted `ch` with its "Ch. " prefix stripped,
	// since `toFeedCard` re-adds it. Only the canonical half contributes a label now —
	// `toCard` has no chapter number to give (see there), and the count it used to
	// substitute was the wrong quantity.
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
		status: c.status ? VIEW_TO_DOMAIN_STATUS[c.status] : null,
		latestChapter: c.ch ? c.ch.replace(/^Ch\.\s*/, '') : null,
		latestChapterTitle: null,
		chapterCount: null,
		releasedAt: c.timeAt ? new Date(c.timeAt).toISOString() : '',
		// The legacy two-feed merge has no announce clock to offer — neither `updates` nor
		// `canonicalUpdates` carries one — so it says so, and `toFeedCard` falls back to the
		// release clock. Fabricating one here would make the fallback claim a precision the
		// old feeds never had.
		announcedAt: null,
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
	/**
	 * Whether the SERIES has finished publishing (completed/cancelled) — not whether
	 * the viewer finished it (that's {@link shelf}). Carried so the card's shelf
	 * picker can withhold "Completed" on a series that is still running, and so the
	 * derived shelf can be recomputed client-side after an optimistic edit.
	 */
	ended: boolean;
	favorite: boolean;
	read: number;
	total: number;
	/** Newest chapter's release time (RFC 3339), or '' — sorts "Latest chapter". */
	latestChapterAt: string;
	/**
	 * When the viewer last made progress (RFC 3339), or '' if never — sorts "Recently read".
	 *
	 * There is deliberately no `addedAt` beside these two: `library()` already returns rows
	 * in `user_library.created_at DESC`, so the "Added" sort is the array order it arrives
	 * in and needs no field. Exposing a real timestamp would mean a new server field for a
	 * number nothing displays.
	 */
	lastReadAt: string;
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
			return { s, p, total, read };
		});
		const libraryCatalog = rows.map(({ s, p, total, read }) => ({
			id: s.id,
			title: s.title,
			cover: s.coverUrl,
			type: toViewType(s.type),
			genre: s.genres[0] ?? '',
			rating: s.rating.average.toFixed(1),
			// An explicit shelf the viewer filed wins; otherwise derive from progress —
			// and only an ENDED series can derive to `completed`, so catching up on an
			// ongoing one leaves the card on "Reading". See {@link deriveShelf}.
			shelf: (s.libraryStatus as Shelf | null) ?? deriveShelf(read, total, isEnded(s)),
			ended: isEnded(s),
			favorite: s.isFavorite ?? false,
			read,
			total,
			latestChapterAt: s.latestChapterAt ?? '',
			lastReadAt: p?.lastReadAt ?? '',
		}));
		// Continue-reading: series with progress underway.
		//
		// `ch` is EMPTY on purpose. It used to be `Ch. ${read + 1}` — a resume label
		// derived from how many chapters have been READ, on the assumption that chapters
		// run 1..total. They do not: chapter numbering starts at 0 (`Chapter 0`,
		// `Prologue`), carries 35,091 half chapters on the Suwayomi side alone, and a
		// source that picked a series up midway starts at whatever it started at. So the
		// number shown was a progress INDEX wearing a chapter number's label, and it was
		// wrong by construction on every series that isn't numbered 1..N.
		//
		// The honest number would be the next unread chapter's own label, which needs a
		// `chapters()` call per series — the exact N+1 fan-out that was removed from this
		// screen (see `libraryProgress` above) because it hung a large library. Until the
		// batched query carries it, render nothing: the card already shows real progress
		// ("62% · Action") one line below, so nothing is lost but the false precision.
		const continueRow = rows
			.filter(({ read, total }) => read > 0 && read < total)
			.map(({ s, read, total }) => ({
				id: s.id,
				title: s.title,
				cover: s.coverUrl,
				ch: '',
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
	/**
	 * The chapter's RELEASE time, already formatted as natural English ("3 hours
	 * ago", "March 2023"), or '' when unknown. Rendered as "Released <date>". Both
	 * clocks now go through {@link chapterDateLabel}: the terse chip format was a card
	 * convention, and a "Released 3d" line read as decoration, not a date.
	 */
	date: string;
	/**
	 * When WE first had this chapter, already formatted, or '' when the row must
	 * not carry a "Found" line at all.
	 *
	 * NEVER a placeholder. The eligibility rules (last-30-days ∪ newest-3) live in
	 * {@link chapterFoundLabel}, and an em dash would assert "we looked and did not
	 * find it" when the truth is "not recorded" — migration 0099 invented no
	 * discovery time for the back catalogue.
	 */
	foundDate: string;
	isNew: boolean;
	read: boolean;
	/** The Suwayomi manga id of the source that the WHOLE row opens (the reader's
	 *  sensible default: the preferred translator when it carries the chapter, else
	 *  a deterministic fallback), null = the MangaDex mirror; undefined for a
	 *  single-source (numeric) series. */
	src?: string | null;
	/**
	 * Every source that carries this chapter number — the provenance cluster shown
	 * next to the chapter number. Each entry links to that exact source's copy
	 * (`id` + `src`); `name`/`iconUrl` come from the resolved translator metadata
	 * (the extension icon URL, with an initial-letter fallback rendered by the
	 * template), so the cluster never fabricates either. Deduped by name: the
	 * hidden redundant MangaDex extension and the direct spine are one source to
	 * the reader.
	 *
	 * Undefined on the legacy path — a numeric Suwayomi series has one implicit
	 * source and nothing to pick between.
	 */
	sources?: ChapterSourceLink[];
	/**
	 * The chapter is hosted OFF-SITE (`Chapter.externalUrl != null`) — MangaPlus, Comikey,
	 * NamiComi, BiliBili, ~35,000 chapters. Opening it shows a "read it on <host>" hand-off
	 * instead of pages.
	 *
	 * The series page is the chapter list people actually browse, so this is where knowing
	 * it beforehand is the difference between a link and a dead end. The reader's own
	 * dropdown already badges (`read/[slug]/+page.svelte`), but that is only visible AFTER
	 * you have arrived — which is exactly the surprise the badge exists to remove.
	 *
	 * `false` means "not known to be off-site", not "proven local". On the aggregated
	 * (multi-source) path a chapter the selected translator does not carry comes from an
	 * `AggregatedChapter` copy, whose `copyExternalUrl` an older API does not send — the
	 * flag then degrades to `false` and the row simply loses its badge. `externalUrl != null` is the ONLY valid test —
	 * `pageCount === 0` is not (this backend returns 0 for ordinary chapters too).
	 */
	external?: boolean;
	/** Scanlation group of the default copy (`id`/`src`); null when unknown. */
	group?: string | null;
}
/** One source's copy of a chapter, for the series-page provenance cluster. */
export interface ChapterSourceLink {
	/** The source's own chapter id to open (`?ch=`); opened WITHOUT it when
	 *  absent, mirroring {@link SeriesChapterView.id}. */
	id?: string;
	/** Suwayomi manga id to pin the reader to (`?src=`); null = the MangaDex mirror. */
	src: string | null;
	/** Display name, resolved from the translator metadata. */
	name: string;
	/** Extension icon URL, or null → the template renders an initial letter. */
	iconUrl: string | null;
	/**
	 * The scanlation group(s) behind THIS copy, e.g. "Uwaki Scans" or
	 * "Uwaki Scans & Sahel Scans"; "No Group" when MangaDex lists none. null when unknown
	 * (not backfilled yet, or a source that does not report one).
	 */
	group?: string | null;
	/** Release date of THIS copy, formatted like {@link SeriesChapterView.date}; '' when unknown. */
	date?: string;
}

/** One copy of the open chapter number — a (source, scanlation group) upload. */
export interface ReaderChapterVersion {
	/** Chapter id to open (`?ch=`). */
	id: string;
	/** Suwayomi manga id to pin (`?src=`); null = the MangaDex mirror. */
	src: string | null;
	/** Source display name ("MangaDex", an extension name, …). */
	source: string;
	iconUrl: string | null;
	/** Scanlation group label; null when unknown. */
	group: string | null;
	/** This is the copy currently open. */
	current: boolean;
}

export interface SeriesDetailView {
	/** Optional detail-only MangaDex metadata; null for older APIs and un-enriched works. */
	metadata?: Series['metadata'] | null;
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
	/**
	 * Whether publication has ENDED (completed/cancelled), the machine-readable
	 * companion to {@link statusLabel}. Gates both the auto-`completed` shelf and
	 * whether the shelf picker offers "Completed" at all — an ongoing series can't
	 * be finished, only caught up on.
	 */
	ended: boolean;
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
	 *  the crawled/derived value. Read only by the admin content-rating control; the
	 *  page shows no NSFW badge to ordinary readers, since a series they can see is
	 *  one their own `show_nsfw` preference already admits. */
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
			ended: isEnded(s),
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
			metadata: enrich?.metadata ?? s.metadata ?? null,
			credits,
			descriptions,
			descLang: desc.lang,
			covers,
			isNsfw: s.isNsfw,
		},
		// Emit chapters in a CANONICAL ascending order (oldest → newest) so the
		// series page can sort deterministically. Sources deliver chapters in
		// inconsistent orders (some newest-first, some oldest-first), so we don't
		// trust arrival order: map `asc` (sorted by number), and mark the highest 3
		// numbers as "new" (matching the aggregated path in buildAggregatedChapters).
		// Those same last-three are the "newest" half of the Found eligibility union
		// (last-30-days ∪ newest-3) — see `chapterFoundLabel`.
		chapters: asc.map((c, i) => ({
			id: c.id,
			n: c.number,
			title: c.title || `Chapter ${c.number}`,
			date: chapterDateLabel(c.uploadedAt),
			foundDate: chapterFoundLabel(c.foundAt, i >= asc.length - 3),
			isNew: i >= asc.length - 3,
			read: c.read,
			// Through the same validator the reader itself uses, so the badge and the
			// hand-off agree: a `javascript:`/relative `externalUrl` is rejected by
			// `chapterExternalUrl` and the chapter falls back to the ordinary page path —
			// badging it would promise a hand-off that never happens.
			external: !!chapterExternalUrl(c),
			// One row per chapter ROW on this path, so the row's own group is the copy's.
			group: normalizeGroup(c.scanlator),
		})),
		related: relatedFor(s, pool),
		translators: tmeta.translators,
		selectedTranslatorKey: tmeta.selectedTranslatorKey,
		workId: tmeta.workId,
	};
}

/** One COPY of an aggregated chapter number — a (source, scanlation group) upload — with
 *  the picker metadata of the source it lives on. */
interface ChapterCopy {
	source: AggregatedChapter['sources'][number];
	/** Suwayomi manga id to pin (`?src=`); null = the MangaDex mirror. */
	src: string | null;
	name: string;
	iconUrl: string | null;
	/** Scanlation group, normalised (`''` → null). */
	group: string | null;
	/** The source's own `Chapter` for this copy, when the resolver fetched it. */
	chapter?: Chapter;
}

/**
 * Expand aggregated chapters into their COPIES, labelled with the source each lives on.
 *
 * EVERY COPY SURVIVES, which is the point: `AggregatedChapter.sources` carries one entry per
 * upload (MangaDex's three groups' "Ch. 3" are three entries), and the series page and the
 * reader both offer every one of them by group name.
 *
 * ONE exclusion: copies on the all.mangadex extension that shadows a readable direct spine
 * (`candidates[].redundant`), whenever the spine itself carries that number. They are the
 * spine's own uploads reached by the slow route, so listing them would show every MangaDex
 * group twice and could open one over `api.komiq.cc`. Where the spine LACKS the number
 * they are kept — then they are the only way to read it.
 *
 * Built once per work (index maps, not a scan per copy): a long series is ~1,000 numbers ×
 * several copies, and a per-copy `find` over each source's chapter list was quadratic.
 */
function makeCopyResolver(resolved: ResolvedWork): (c: AggregatedChapter) => ChapterCopy[] {
	const byKey = new Map(
		resolved.candidates.map((cand) => [
			cand.suwayomiMangaId ?? 'spine',
			{ cand, chapters: new Map(cand.chapters.map((ch) => [ch.id, ch])) },
		]),
	);
	return (c) => {
		const spineCarries = c.sources.some((s) => !s.suwayomiMangaId);
		const seen = new Set<string>();
		const out: ChapterCopy[] = [];
		for (const s of c.sources) {
			const src = s.suwayomiMangaId ?? null;
			const entry = byKey.get(src ?? 'spine');
			if (entry?.cand.redundant && spineCarries) continue;
			const k = `${src ?? 'spine'}:${s.chapterId}`;
			if (seen.has(k)) continue;
			seen.add(k);
			const chapter = entry?.chapters.get(s.chapterId);
			out.push({
				source: s,
				src,
				name: entry?.cand.name ?? 'Source',
				iconUrl: entry?.cand.iconUrl ?? null,
				group: normalizeGroup(s.scanlator ?? chapter?.scanlator),
				chapter,
			});
		}
		return out;
	};
}

/**
 * Build the series-page chapter list from the server's multi-source aggregation
 * (S2): one row per chapter NUMBER across ALL a work's sources. Each row picks
 * the source to read from — the work's preferred translator when it carries that
 * chapter, otherwise the first source that does (per-chapter fallback, so a
 * chapter the preferred source lacks is still readable). Read-state + dates are
 * enriched from the preferred translator's own chapters where they line up.
 *
 * DATES COME FROM TWO PLACES, in this precedence: the selected translator's own
 * `uploadedAt` when it carries the chapter (that is the copy the reader will actually
 * open), otherwise the aggregation's `firstReleasedAt` — the earliest release across
 * every source. Neither is ever a fabrication: a chapter no source dated renders with
 * no date line rather than "just now".
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
	// Per-copy expansion for the provenance cluster. Source metadata comes from every
	// candidate — including sources the picker hides — so the cluster never falls back to
	// a generic label for a chapter a hidden source serves.
	const copiesFor = makeCopyResolver(resolved);
	const chaptersBySrc = new Map(
		resolved.candidates.map((candidate) => [
			candidate.suwayomiMangaId ?? 'spine',
			new Map(candidate.chapters.map((chapter) => [chapter.id, chapter])),
		]),
	);
	const asc = [...agg].sort((a, b) => a.number - b.number);
	return asc.map((c, i) => {
		const own = readable.get(c.number);
		const copies = copiesFor(c);
		let id: string | undefined;
		let src: string | null;
		let group: string | null = null;
		let external = false;
		// Cross-source read state FIRST, so it survives a translator switch: the selected
		// source's own row only knows what was read from THAT source, and the ids differ
		// per source. `readAnywhere` is opportunistically selected, so an older API leaves
		// it undefined and this degrades to exactly the old per-source behaviour.
		let read = c.readAnywhere === true;
		let date = '';
		// Aggregation has no discovery timestamp, but the resolver already fetched each
		// source's concrete Chapter. Use the earliest valid `foundAt` among the copies:
		// "Found" means when Komika first had this chapter anywhere, not when the reader's
		// preferred source happened to acquire its copy.
		const foundAt = c.sources
			.map(
				(source) =>
					chaptersBySrc.get(source.suwayomiMangaId ?? 'spine')?.get(source.chapterId)?.foundAt,
			)
			.filter((value): value is string => !!value && !Number.isNaN(Date.parse(value)))
			.sort((a, b) => Date.parse(a) - Date.parse(b))[0];
		const foundDate = chapterFoundLabel(foundAt, i >= asc.length - 3);
		if (own) {
			id = own.id;
			src = selectedSuw;
			read = read || own.read;
			date = chapterDateLabel(c.firstReleasedAt ?? own.uploadedAt);
			group = normalizeGroup(own.scanlator);
			external = !!chapterExternalUrl(own);
		} else {
			// A chapter the selected translator doesn't carry — read from whichever source
			// does, and date it from the aggregation's own clock (F12 / §4.13).
			//
			// `firstReleasedAt` is the EARLIEST release across the chapter's sources, so the
			// date does not shift as the reader switches translator and it agrees with the
			// date `/updates` gave the same chapter. It is opportunistically selected, so an
			// older API returns nothing for it — `chapterDateLabel` renders '' for an absent or
			// unparseable value and the row omits the date line rather than rendering a guess.
			const chosen = pickSource(c.sources);
			id = chosen?.chapterId;
			src = chosen?.suwayomiMangaId ?? null;
			date = chapterDateLabel(c.firstReleasedAt);
			group = normalizeGroup(chosen?.scanlator);
			// Per-copy now (`copyExternalUrl`), so a chapter this translator does not carry
			// keeps its off-site badge. Absent on an older API → `false`, i.e. "not known to
			// be off-site", the pre-field behaviour.
			external = !!externalChapterHref(chosen?.copyExternalUrl);
		}
		// The provenance cluster: ONE ENTRY PER COPY — every (source, scanlation group)
		// upload of this number, each opening its own chapter id. This used to dedupe by
		// source NAME, which collapsed MangaDex's several groups' uploads into a single
		// "MangaDex" link and made every copy but one unreachable from this page. The only
		// copies dropped now are the redundant all.mangadex shadows of spine uploads (see
		// `makeCopyResolver`). Each copy's date is its OWN release, through the same
		// formatter as the row's date, falling back to the source chapter's `uploadedAt`
		// for an API that predates `copyReleasedAt`.
		const sources: ChapterSourceLink[] = copies.map((cp) => ({
			id: cp.source.chapterId,
			src: cp.src,
			name: cp.name,
			iconUrl: cp.iconUrl,
			group: cp.group,
			date: chapterDateLabel(cp.source.copyReleasedAt ?? cp.chapter?.uploadedAt),
		}));
		return {
			id,
			n: c.number,
			title: c.title || `Chapter ${c.number}`,
			date,
			foundDate,
			// "New" = the highest-numbered chapters (shown first when sorted newest), and
			// the "newest" half of the Found eligibility union — see `chapterFoundLabel`.
			isNew: i >= asc.length - 3,
			read,
			src,
			sources,
			// The copy the row OPENS: the selected translator's real `Chapter` when it carries
			// the number, else the chosen fallback copy's `copyExternalUrl` — never a guess.
			external,
			group,
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

export async function getSeries(id: string, fetch?: LoadFetch): Promise<SeriesResult> {
	const b = api(fetch);
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
			const aggP = b.seriesPage
				? null
				: b.aggregatedChapters
					? b.aggregatedChapters(id).catch(() => [] as AggregatedChapter[])
					: Promise.resolve([] as AggregatedChapter[]);
			const resolved = await resolveWork(id, preferred, { withAggregated: true }, b);
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
			// PER-VIEWER FLAGS FOLLOW THE ID, NOT THE METADATA.
			//
			// `resolved.meta` is the SELECTED TRANSLATOR's series — for anything but the
			// MangaDex spine that's `backend.series(<numeric suwayomi id>)`, whose
			// `isMarked`/`isFavorite`/`libraryStatus` the server resolved against
			// `user_library.series_id = '<numeric>'`. Overwriting only `id` below left
			// those three source-scoped while every write on this page addresses
			// `effectiveId` (a `w_` work id), so the row was stored under one key and
			// read back under another: "Add to Library" succeeded, the series appeared
			// in /library, and a reload showed "Add to Library" again — with a second
			// click then sending `marked: false` and deleting the real row.
			//
			// `canonSeries` is the same work fetched under `effectiveId` and already
			// selects all three (CANONICAL_SERIES), so it costs no extra round trip. It
			// only applies when the ids actually agree; a federation-only work has no
			// canonical row and its numeric-scoped flags are already the right ones.
			const canon = resolved.canonSeries;
			const viewerScoped =
				canon && canon.id === effectiveId
					? {
							isMarked: canon.isMarked,
							isFavorite: canon.isFavorite ?? false,
							libraryStatus: canon.libraryStatus ?? null,
						}
					: {};
			const metaForView: Series = { ...resolved.meta, id: effectiveId, ...viewerScoped };
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
			// THE CROSS-SOURCE UNION OWNS THE CHAPTER LIST. The series page has no
			// per-source re-listing any more: one row per chapter number across ALL
			// sources, each row opening its sensible default source (the preferred
			// translator when it carries that chapter, otherwise a deterministic
			// fallback) plus a per-source provenance cluster for the exact targets.
			// `mapSeriesView` above gets look-in only when aggregation is unavailable
			// (a backend without `aggregatedChapters`), i.e. the safe legacy fallback.
			//
			// Bundle path: aggregation came back inside `seriesPage`. Legacy path: the
			// parallel request above. Drift corner (seriesPage present but the SERVER
			// predates it, so resolveWork fell back internally): fetch it now — one
			// extra round trip, only until the server deploy catches up.
			const agg =
				resolved.aggregated ??
				(await (aggP ??
					(b.aggregatedChapters
						? b.aggregatedChapters(id).catch(() => [] as AggregatedChapter[])
						: Promise.resolve([] as AggregatedChapter[]))));
			if (agg.length) {
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
		const chaptersP = b.chapters(id);
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
		const poolP = b
			.search('', 1, { sort: 'NEWEST' })
			.then((r) => r.items)
			.catch(() => [] as Series[]);
		const s = await b.series(id);
		if (s.workId && isCanonicalId(s.workId)) {
			const canonical = await getSeries(s.workId, fetch);
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
		await invalidate('komika:library');
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
	/** Set when this chapter is hosted off-site (see {@link ReaderView.externalUrl}), so
	 *  the chapter list can BADGE it — a reader shouldn't have to click to find out that
	 *  a chapter opens somewhere else. Validated, so it is safe to put in an `href`. */
	externalUrl?: string | null;
}
export interface ReaderView {
	/** Completion survives reopening a chapter, including native backends. */
	chapterRead?: boolean;
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
	/**
	 * Set when the OPEN chapter is hosted off-site and there are no pages for us to serve
	 * — MangaPlus, Comikey, NamiComi, BiliBili, ~35,000 chapters / 4% of the mirror. Then
	 * `pages` is empty *by design* and the reader must send the reader to this URL instead
	 * of showing the no-pages state, which blames "a licensing gap at the source" for a
	 * chapter that is perfectly readable one hop away.
	 *
	 * Already through {@link externalChapterHref}, so it is a validated http(s) URL or
	 * null — never raw upstream text in an `href`.
	 */
	externalUrl?: string | null;
	/** Every copy of the OPEN chapter number across sources and scanlation groups. */
	versions?: ReaderChapterVersion[];
	/** Scanlation group of the open copy; null when unknown. */
	group?: string | null;
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

/**
 * The off-site URL a chapter must be read at, validated, or `null` for the ~96% of the
 * mirror we serve pages for ourselves.
 *
 * THE CAST IS THE POINT, so read this before deleting it. The server has shipped
 * `Chapter.externalUrl` — `canonicalChapters(workId: "w_…"){ externalUrl }` answers on
 * production today — but the shared client does not SELECT it: the field is absent from
 * `CHAPTER_FIELDS` in `packages/api/src/operations.ts` and from `Chapter` in
 * `packages/types/src/index.ts`. GraphQL returns only what a document asks for, so the
 * property is genuinely missing at runtime until those two are updated, and the domain type
 * is honest to omit it.
 *
 * This reader is therefore written against the field it will receive: `undefined` today,
 * which is byte-for-byte the behaviour before this change (no badge, no redirect, the
 * existing empty state), and a working redirect the moment those two lines land. A
 * `pageCount === 0` test is NOT a substitute — sampled BiliBili externals report `pages: 45`
 * upstream, and our own `pageCount` is 0 on ordinary un-fetched chapters too, so it would
 * both miss real externals and hijack normal ones.
 */
function chapterExternalUrl(c: Chapter): string | null {
	// No cast any more: `Chapter.externalUrl` is now a real field on the type AND selected
	// by `ChapterFields`. Both were needed — GraphQL returns only what is asked for, so
	// while the fragment omitted it the property was genuinely absent at runtime and this
	// whole redirect path was inert regardless of what the type claimed.
	return externalChapterHref(c.externalUrl);
}

/**
 * Extra inputs {@link buildReaderView} needs when the open chapter number has more than
 * one copy. All optional: the single-source path passes none and gets versions built from
 * its own list.
 */
interface ReaderCopyContext {
	/**
	 * Every copy the reader can open from the ACTIVE source, for prev/next. Defaults to the
	 * list itself. On the MangaDex spine this is wider than the list: `canonicalChapters`
	 * keeps ONE copy per number, the other groups' uploads exist only in the aggregation,
	 * and "next chapter, same group" can only find them here.
	 */
	pool?: Chapter[];
	/** Every copy of the OPEN number across sources and groups (the version switcher). */
	versions?: ReaderChapterVersion[];
	/** Name/icon of the active source, for versions built from the list alone. */
	sourceName?: string;
	iconUrl?: string | null;
}

/**
 * Assemble a ReaderView from a resolved chapter list + target chapter's pages.
 *
 * ORDER IS REBUILT HERE, NEVER TRUSTED. This used to take a `preserveOrder` flag and
 * compute prev/next as `list[idx ± 1]` over the list as it arrived. That is only right
 * while every list is ascending with one row per number, and neither holds in general:
 * Suwayomi sources deliver newest-first (so an unsorted path swaps prev and next), a
 * Suwayomi list holds one row per scanlator (so "next" from one group's ch. 3 was another
 * group's ch. 3), and the spine's ascending order is a property of one server function
 * (`select_reader_chapters`), not of the query under it, which is ordered newest-FIRST.
 * {@link sortChaptersAsc} rebuilds ascending order from the labels (oneshots last, never
 * as "chapter 0") and {@link adjacentChapters} steps to the adjacent DISTINCT number,
 * preferring the open copy's scanlation group.
 */
function buildReaderView(
	seriesId: string,
	chs: Chapter[],
	target: Chapter,
	urls: string[],
	seriesTitle: string,
	tmeta: TranslatorMeta,
	readingSrc: string | null,
	ctx: ReaderCopyContext = {},
): ReaderView {
	// The dropdown lists one row per entry of the source's own list. When the open copy is
	// NOT in that list (a non-default spine copy, opened by id from the version switcher),
	// it stands in for its number's default copy, so the dropdown still highlights what is
	// actually open instead of a sibling upload.
	const listed = chs.some((c) => c.id === target.id)
		? chs
		: (() => {
				const sameNumber = new Set(copiesOf(chs, target).map((c) => c.id));
				return [...chs.filter((c) => !sameNumber.has(c.id)), target];
			})();
	const asc = sortChaptersAsc(listed);
	const listIds = new Set(chs.map((c) => c.id));
	const { prev, next } = adjacentChapters(ctx.pool ?? chs, target, (c) => listIds.has(c.id));
	const group = normalizeGroup(target.scanlator);
	const versions: ReaderChapterVersion[] =
		ctx.versions ??
		copiesOf(asc, target).map((c) => ({
			id: c.id,
			src: readingSrc,
			source: ctx.sourceName ?? 'Source',
			iconUrl: ctx.iconUrl ?? null,
			group: normalizeGroup(c.scanlator),
			current: c.id === target.id,
		}));
	return {
		seriesId,
		seriesTitle: seriesTitle || 'Reader',
		chapterId: target.id,
		chapterRead: target.read,
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
			.map((c) => ({
				id: c.id,
				n: c.number,
				title: c.title || `Chapter ${c.number}`,
				externalUrl: chapterExternalUrl(c),
			})),
		prevChapterId: prev?.id,
		nextChapterId: next?.id,
		translators: tmeta.translators,
		selectedTranslatorKey: tmeta.selectedTranslatorKey,
		workId: tmeta.workId,
		readingSrc,
		externalUrl: chapterExternalUrl(target),
		versions,
		group: group ?? versions.find((v) => v.current)?.group ?? null,
	};
}

/**
 * A reader-openable `Chapter` for one aggregated COPY the source's own list does not
 * carry — in practice a MangaDex upload other than the one `canonicalChapters` picked for
 * its number (the spine keeps one copy per number; the aggregation keeps them all).
 *
 * Everything here is what the aggregation actually reports, never a guess: the number,
 * label and title of the number, THIS copy's group, release clock and off-site URL. `read`
 * comes from the cross-source state — `readAnywhere`, or the spine's own copy of the same
 * number, whose `read` the server already folds across every copy (spine key) — because
 * read state is per NUMBER to a reader, not per upload.
 */
function chapterFromCopy(
	seriesId: string,
	agg: AggregatedChapter,
	copy: AggregatedChapter['sources'][number],
	sibling: Chapter | undefined,
): Chapter {
	return {
		id: copy.chapterId,
		seriesId,
		number: agg.number,
		label: agg.label ?? sibling?.label ?? String(agg.number),
		externalUrl: copy.copyExternalUrl ?? null,
		title: agg.title ?? sibling?.title ?? null,
		pageCount: 0,
		uploadedAt: copy.copyReleasedAt ?? null,
		foundAt: null,
		scanlator: copy.scanlator,
		read: agg.readAnywhere === true || !!sibling?.read,
		lastPageRead: 0,
		bookmarked: false,
		isDownloaded: false,
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

/** The aggregated copy a `?ch=` names: the one on `src` when several sources happen to share
 *  the id, else the first. Id-exact — never a number match. */
function findAggregatedCopy(
	agg: AggregatedChapter[],
	chapterId: string,
	src: string | null,
): { agg: AggregatedChapter; copy: AggregatedChapter['sources'][number] } | null {
	let fallback: { agg: AggregatedChapter; copy: AggregatedChapter['sources'][number] } | null =
		null;
	for (const a of agg) {
		for (const copy of a.sources) {
			if (copy.chapterId !== chapterId) continue;
			if ((copy.suwayomiMangaId ?? null) === src) return { agg: a, copy };
			fallback ??= { agg: a, copy };
		}
	}
	return fallback;
}

/**
 * Every copy openable from ONE source: its own list plus the aggregation's copies on that
 * source the list lacks. For the spine that is every MangaDex group's upload, not only the
 * one `canonicalChapters` picked per number.
 */
function copyPool(
	seriesId: string,
	agg: AggregatedChapter[],
	list: Chapter[],
	src: string | null,
): Chapter[] {
	const have = new Set(list.map((c) => c.id));
	const extra: Chapter[] = [];
	for (const a of agg) {
		for (const copy of a.sources) {
			if ((copy.suwayomiMangaId ?? null) !== src || have.has(copy.chapterId)) continue;
			have.add(copy.chapterId);
			extra.push(chapterFromCopy(seriesId, a, copy, undefined));
		}
	}
	return extra.length ? [...list, ...extra] : list;
}

/**
 * The version switcher: every copy of the OPEN chapter number across sources and groups,
 * with `current` on the one being read. Undefined when the aggregation does not know the
 * open copy (an older API, a source outside it) — `buildReaderView` then lists the open
 * source's own copies instead.
 *
 * The open copy is always present and always `current`: if the aggregation listed the
 * number but not this exact upload (a redundant-extension copy `makeCopyResolver` drops),
 * it is added rather than leaving the switcher with nothing selected.
 */
function readerVersions(
	agg: AggregatedChapter[],
	resolved: ResolvedWork,
	target: Chapter,
	src: string | null,
	activeCand: { name: string; iconUrl: string | null } | undefined,
): ReaderChapterVersion[] | undefined {
	const entry = agg.find((a) => a.sources.some((s) => s.chapterId === target.id));
	if (!entry) return undefined;
	const versions: ReaderChapterVersion[] = makeCopyResolver(resolved)(entry).map((cp) => ({
		id: cp.source.chapterId,
		src: cp.src,
		source: cp.name,
		iconUrl: cp.iconUrl,
		group: cp.group,
		current: cp.source.chapterId === target.id && cp.src === src,
	}));
	if (!versions.some((v) => v.current)) {
		versions.unshift({
			id: target.id,
			src,
			source: activeCand?.name ?? 'Source',
			iconUrl: activeCand?.iconUrl ?? null,
			group: normalizeGroup(target.scanlator),
			current: true,
		});
	}
	return versions;
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
		//
		// KNOWN COST, accepted: whether the chapter is OFF-SITE is a property of the
		// chapter list, which by definition hasn't resolved yet, so an off-site chapter
		// does spend one request that the sequential path (`chapterExternalUrl(target)
		// ? [] : …`) would have skipped. Its result is discarded. That is a few percent
		// of opens paying one wasted call so the rest save two sequential round-trips.
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
			// The cross-source aggregation rides along (`withAggregated`): it is the ONLY
			// list that carries every copy of a number — `canonicalChapters` keeps one
			// MangaDex upload per number — so it is what the version switcher lists, what
			// resolves a `?ch=` naming a non-default upload, and what lets prev/next stay
			// with the open copy's scanlation group. A backend without the bundle fetches
			// it alongside the translator fan-out instead of after it.
			const aggP =
				!backend.seriesPage && backend.aggregatedChapters
					? backend.aggregatedChapters(seriesId).catch(() => [] as AggregatedChapter[])
					: null;
			const resolved = await resolveWork(seriesId, preferred, { withAggregated: true });
			if (!resolved) return emptyReader(seriesId);
			// Absent on an API that predates the per-copy fields or the bundle: the reader
			// then degrades to exactly its old single-copy behaviour.
			const agg: AggregatedChapter[] = resolved.aggregated ?? (aggP ? await aggP : []);
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
			// `resolveWork` already fetched every mapped source's chapters into
			// `resolved.candidates` (same `backend.chapters` call, same degrade-to-`[]`
			// on failure), so re-requesting the one `?src=` names was a duplicate round
			// trip on the exact navigation that always carries `src`. The network call
			// stays as the fallback for a `src` that is NOT one of the work's mappings —
			// a hand-edited URL, or a mapping dropped since the link was made — which
			// `candidates` cannot answer for.
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
				if (!owner) {
					// Not in any source's LIST — but possibly one of the MangaDex uploads the
					// spine list does not carry: `canonicalChapters` keeps ONE copy per number,
					// and the version switcher links the others by id. The aggregation knows
					// every copy, so open that exact upload from the spine. Still matched by
					// ID, never by number: this cannot open a different chapter than asked.
					const hit = findAggregatedCopy(agg, chParam, srcParam ?? null);
					const spineCand = resolved.candidates.find((c) => c.suwayomiMangaId === null);
					// Genuinely unknown to every source — stay honest and render empty. (A
					// Suwayomi copy missing from its own source's list means that list failed
					// to load; there is nothing to read it from.)
					if (!hit || hit.copy.suwayomiMangaId || !spineCand) {
						const empty = emptyReader(effectiveId, tmeta);
						return { ...empty, seriesTitle: resolved.meta?.title ?? '' };
					}
					const provisional = chapterFromCopy(effectiveId, hit.agg, hit.copy, undefined);
					const sibling = copiesOf(spineCand.chapters, provisional)[0];
					target = chapterFromCopy(effectiveId, hit.agg, hit.copy, sibling);
					activeChs = spineCand.chapters;
					activeSrc = null;
					activeSpine = true;
					tmeta.selectedTranslatorKey = translatorKey('mangadex', null);
				} else {
					target = owner.chapter;
					activeChs = owner.candidate.chapters;
					activeSrc = owner.candidate.suwayomiMangaId;
					// Only the spine reads through `canonicalPages` — track the source we
					// actually landed on.
					activeSpine = owner.candidate.suwayomiMangaId === null;
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
			}
			const asc = sortChaptersAsc(activeChs);
			const chosen: Chapter = target ?? asc.find((c) => !c.read) ?? asc[0];
			// The page fetch must NOT be able to blank the reader. It used to run bare here,
			// so one rejection — a dead Suwayomi engine answering `pages` with "Internal
			// error", a MangaDex upload taken down upstream — escaped to `live()`, which
			// discarded the resolved title, list, versions and prev/next for `emptyReader`
			// ("Ch. 0", no pickers, nothing to switch to). Now a copy that fails or comes
			// back empty falls through to the other sources' copies of the SAME number
			// (spine first, `resolveWork`'s order), and if none reads, the chosen chapter
			// still opens with its full chrome and no pages. See {@link loadFirstReadable}.
			//
			// An off-site chapter has no pages to ask for, so don't ask: the request would
			// spend a round trip to come back empty and land the reader in the no-pages
			// state, which blames a licensing gap for a chapter that reads fine one hop
			// away. `buildReaderView` carries the URL through and the reader links out. It
			// gets no fallbacks either: off-site is an answer, not a failure.
			type PageAttempt = {
				chapter: Chapter;
				chs: Chapter[];
				src: string | null;
				spine: boolean;
				key: string | null;
			};
			const primary: PageAttempt = {
				chapter: chosen,
				chs: activeChs,
				src: activeSrc,
				spine: activeSpine,
				key: null,
			};
			const fallbacks: PageAttempt[] = chapterExternalUrl(chosen)
				? []
				: resolved.candidates
						.filter((cand) => cand.suwayomiMangaId !== activeSrc)
						.flatMap((cand) =>
							// The POOL, not the list: the spine's list keeps ONE copy per
							// number, and that one is often the off-site upload while another
							// group's readable upload exists only in the aggregation.
							copiesOf(copyPool(effectiveId, agg, cand.chapters, cand.suwayomiMangaId), chosen)
								.filter((c) => c.id !== chosen.id && !chapterExternalUrl(c))
								.map((c) => ({
									chapter: c,
									chs: cand.chapters,
									src: cand.suwayomiMangaId,
									spine: cand.suwayomiMangaId === null,
									key: cand.key,
								})),
						);
			const opened = await loadFirstReadable<PageAttempt, Page>(
				[primary, ...fallbacks],
				(a) =>
					chapterExternalUrl(a.chapter)
						? Promise.resolve([])
						: // Use the speculation only when the chapter we landed on is the one
							// it was started for. `target` can be SUBSTITUTED above (an unknown
							// `?ch=` falls back to the first unread), and those pages belong to
							// a different chapter entirely.
							speculatedPages && a.chapter.id === chParam
							? speculatedPages
							: a.spine && backend.canonicalPages
								? backend.canonicalPages(a.chapter.id)
								: backend.pages(a.chapter.id),
				(a, err) =>
					console.warn(
						`[komika] pages failed for chapter ${a.chapter.id} (${a.src ?? 'mangadex'}):`,
						err,
					),
			);
			target = opened.copy.chapter;
			if (opened.copy !== primary) {
				// Read from the copy that actually loaded — list, prev/next and the picker
				// all describe ITS source, exactly like the owner switch above.
				activeChs = opened.copy.chs;
				activeSrc = opened.copy.src;
				const key = opened.copy.key;
				tmeta.selectedTranslatorKey =
					key && resolved.translators.some((t) => t.key === key)
						? key
						: translatorKey('mangadex', null);
			}
			const urls = opened.pages.length ? await resolvePageUrls(opened.pages) : [];
			const readingSrc = activeSrc;
			const activeCand = resolved.candidates.find((c) => c.suwayomiMangaId === activeSrc);
			// `activeChs`, not `chs`: the list, the prev/next links and the target must
			// all describe the SAME source, or navigation walks a list the reader is
			// not actually on. The POOL widens that same source by the copies only the
			// aggregation carries (the spine's other groups' uploads), so "next chapter"
			// can stay with the open group; a Suwayomi source's list already holds all of
			// its copies, so nothing is added for it.
			return buildReaderView(
				effectiveId,
				activeChs,
				target,
				urls,
				resolved.meta?.title ?? 'Reader',
				tmeta,
				readingSrc,
				{
					pool: copyPool(effectiveId, agg, activeChs, readingSrc),
					versions: readerVersions(agg, resolved, target, readingSrc, activeCand),
					sourceName: activeCand?.name,
					iconUrl: activeCand?.iconUrl ?? null,
				},
			);
		}
		// Numeric Suwayomi series — single source, no translator picker. Its list holds
		// every copy (one row per scanlator), so versions and group-preferring prev/next
		// come from the list itself.
		const chs = await backend.chapters(seriesId);
		if (!chs.length) return emptyReader(seriesId);
		const asc = sortChaptersAsc(chs);
		let target = chParam ? chs.find((c) => c.id === chParam) : undefined;
		if (!target) target = asc.find((c) => !c.read) ?? asc[0];
		// Same skip as the canonical path above — an off-site chapter is served by its
		// host, not by us.
		// Same no-blanking rule as the canonical branch: a failed page fetch opens the
		// chapter with its list and pickers and no pages, never `emptyReader`. A single
		// source has no other copy to fall back to.
		const open = target;
		const { pages } = await loadFirstReadable<Chapter, Page>(
			[open],
			(c) =>
				chapterExternalUrl(c)
					? Promise.resolve([])
					: // Same substitution guard as the canonical branch above: on this path an
						// unknown `?ch=` silently falls back to the first unread chapter, so the
						// id check is what keeps the speculated pages from being shown for it.
						speculatedPages && c.id === chParam
						? speculatedPages
						: backend.pages(c.id),
			(c, err) => console.warn(`[komika] pages failed for chapter ${c.id}:`, err),
		);
		const urls = pages.length ? await resolvePageUrls(pages) : [];
		const series = await backend.series(seriesId).catch(() => null);
		return buildReaderView(
			seriesId,
			chs,
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
		type: ComicType;
		status: Status;
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
			return { s, p, total, read };
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

		// Continue reading: the six series touched most recently, newest first.
		//
		// Ordered by `lastReadAt`, NOT by the array order. `library()` comes back in
		// `user_library.created_at DESC` — when the series was ADDED — so slicing it
		// directly showed the six most recently shelved, which is a different set from
		// the six most recently read and, for anyone who adds in bulk, a mostly untouched
		// one. Series with no timestamp (progress predating `lastReadAt`) sort last
		// rather than jumping the queue on an empty string.
		const reading = [...readingNow]
			.sort((a, b) => (b.p?.lastReadAt ?? '').localeCompare(a.p?.lastReadAt ?? ''))
			.slice(0, 6)
			.map(({ s, read, total }) => ({
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
			// Format + publication status for the shared MangaCard's flag and status
			// badges — the same two overlays Browse renders, so a shelf card is the
			// same card everywhere.
			type: toViewType(s.type),
			status: toViewStatus(s.status),
			// Explicit shelf wins; else derive. Shares {@link deriveShelf} with the library
			// and the series page — this used to be its own near-copy with no `plan`
			// branch and no ongoing-series gate, so an untouched series read "Reading"
			// here and "Plan" there, and a caught-up ongoing one read "Completed" on both.
			shelf: (s.libraryStatus as string | null) ?? deriveShelf(read, total, isEnded(s)),
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

/**
 * File a reader report (Support → Report an issue).
 *
 * Deliberately NOT wrapped in `live()`: every other getter in this file resolves to an
 * empty fallback on failure, which is right for a feed and catastrophic for a form —
 * "thanks, we got it" when nothing was stored is the worst possible outcome here. The
 * server's validation messages are reader-facing on purpose, so they are re-thrown
 * verbatim for the form to display.
 */
export async function submitReport(input: ReportInput): Promise<Report> {
	if (!LIVE || !backend.submitReport) {
		throw new Error('Reporting needs the live Komiq backend.');
	}
	return backend.submitReport(input);
}
