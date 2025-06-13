/**
 * Presentation types, constants, and tiny formatting utils shared by the
 * screens and components. Pure view metadata — no catalog/sample data lives
 * here; all content comes from the backend via `source.ts`.
 */

export type ComicType = 'Manga' | 'Manhwa' | 'Manhua';
export type Status = 'ongoing' | 'completed' | 'hiatus' | 'cancelled';

export const FLAG: Record<ComicType, string> = {
	Manga: '🇯🇵',
	Manhwa: '🇰🇷',
	Manhua: '🇨🇳',
};

export const TYPE_COLOR: Record<ComicType, string> = {
	Manga: 'var(--k-accent)',
	Manhwa: 'var(--k-accent-purple)',
	Manhua: 'var(--k-accent-teal)',
};

export const STATUS_META: Record<Status, { label: string; color: string }> = {
	ongoing: { label: 'Ongoing', color: 'var(--k-ongoing)' },
	completed: { label: 'Completed', color: 'var(--k-completed)' },
	hiatus: { label: 'Hiatus', color: 'var(--k-hiatus)' },
	cancelled: { label: 'Cancelled', color: 'var(--k-cancelled)' },
};

/** A browse-catalog row as the Browse screen renders it. */
export interface CatalogEntry {
	title: string;
	author: string;
	genre: string;
	ch: number;
	/**
	 * The newest chapter's LABEL — `'151'`, `'10.5'`, `'Oneshot'` — already formatted by
	 * `chapterChip()`, or `''` when we do not know it.
	 *
	 * A SECOND, INDEPENDENT quantity from {@link ch}, which is how many chapters we know
	 * of. Browse shows both ("12 ch · Ch. 151") because a partial mirror makes them
	 * legitimately different, and because rendering `ch` under a "Ch." label is the F4
	 * bug the chapter-number contract exists to prevent.
	 *
	 * `''` (never `undefined`, so callers cannot accidentally print it) means the server
	 * has no label for this work — the ~67k catalogue rows with no dated chapter, plus
	 * any response from a server that predates the field. The card then shows the count
	 * alone. It must never fall back to `ch`.
	 */
	latestCh: string;
	rating: number;
	status: Status;
	/**
	 * Positional index in the backend-returned order — NOT a recency signal. Kept
	 * for back-compat; sort "Newest" on {@link addedAt} instead (a real timestamp).
	 */
	added: number;
	/**
	 * When the series entered the catalogue (epoch ms, from `Series.createdAt`), or
	 * 0 when unknown. This is the real recency key: Browse "Newest" should sort by
	 * `b.addedAt - a.addedAt` (descending). See source.ts `toCatalogEntry`.
	 */
	addedAt: number;
	type: ComicType;
	cover?: string;
	id?: string;
}

/** A rail/grid card as the Home and Updates screens render it. */
export interface Card {
	title: string;
	ch: string;
	/**
	 * How many chapters the series has, when the row's feed reports it.
	 *
	 * A SEPARATE field from {@link ch}, which is the newest chapter's own LABEL —
	 * the two are different numbers and collapsing them is the F4 bug the
	 * chapter-number contract exists to prevent ("Ch. 412" on a series whose newest
	 * release is 10.5). Carried apart so a card can print both, exactly as Browse
	 * does: "12 ch · Ch. 151".
	 *
	 * `undefined` (or 0) when the feed doesn't carry a count — {@link cardSub} drops
	 * the half rather than printing "0 ch".
	 */
	chCount?: number;
	/**
	 * The card's headline recency label. On update/trending rows this is the real
	 * upstream CHAPTER RELEASE time (`Series.latestChapterAt`), never our polling
	 * clock; on the "Latest Added" row it's the catalogue-add time. See
	 * `source.ts` → `chapterRecency`.
	 */
	time: string;
	/**
	 * The numeric companion to {@link time} — the SAME instant, as epoch
	 * milliseconds, so card lists can be sorted by recency without re-parsing a
	 * formatted string like "4h" or "March 2023" (which is lossy and unorderable).
	 *
	 * `0` when the row carries no usable timestamp, which sorts it LAST under a
	 * descending sort. That mirrors the server's `NULLS LAST` on the Updates feed,
	 * for the same reason: a row whose recency we don't know cannot be honestly
	 * placed among rows whose recency we do.
	 *
	 * Always on the same clock as `time`, per card constructor — the "Latest Added"
	 * row labels with the catalogue-add time, so its `timeAt` is that too, not the
	 * chapter-release time.
	 */
	timeAt?: number;
	/**
	 * When WE detected the release, when the backend reports it. Formatted the same
	 * way as `time` and empty when unknown — surfaced only as a hover title, so the
	 * two clocks stay distinguishable without changing the card design.
	 */
	detected?: string;
	/**
	 * The chapter's upstream RELEASE time, when it differs from the visible {@link time}.
	 *
	 * Present only on Updates-feed rows, and only when the two clocks actually disagree —
	 * i.e. when the source held the chapter back. On those rows {@link time} is the moment we
	 * could first show it (the feed's sort key), so the tooltip has to name which clock is on
	 * screen; everywhere else the two are the same instant and this stays undefined rather
	 * than printing "released 4h" next to "4h".
	 */
	released?: string;
	rating: string;
	cover?: string;
	id?: string;
	/** Format of the series, when the source feed knows it. */
	type?: ComicType;
}

/**
 * Hover text spelling out a card's terse recency label.
 *
 * The visible "· 4h" is the CHAPTER RELEASE time; when the backend also reports
 * when we detected it, the two are named apart here. Three different clocks used
 * to render under one identical-looking label (our poll time on cards, the real
 * publish time on the MangaDex rows, another on the series header), which is how
 * the feed could say "1h" and the series page "1d" for the same chapter.
 */
export function cardTimeTooltip(card: Card): string {
	if (!card.time) return '';
	// On an Updates-feed row whose source held the chapter back, the VISIBLE label is when we
	// could first show it — not the release — because that is the feed's sort key and the two
	// must agree. So the tooltip names the visible clock first and gives the release second.
	// `released` is set only when the two genuinely differ (see `Card.released`), so this
	// branch cannot produce "We found it 4h · released 4h".
	if (card.released) {
		return `We found it ${card.time} · the source released it ${card.released}`;
	}
	const released = `Chapter released ${card.time}`;
	return card.detected ? `${released} · we detected it ${card.detected}` : released;
}

/**
 * A card's terse sub-line — "Ch. 12 · 4h" — with the separator dropped whenever a
 * half is missing.
 *
 * BOTH halves are genuinely optional and were being interpolated unguarded:
 *  • `ch` is '' for a canonical update row with no chapter number, which rendered
 *    a leading "· 4h".
 *  • `time` is '' whenever no timestamp survives `chapterRecency` (an unscanned
 *    series whose `latestChapterAt` is null and whose `updatedAt` the backend
 *    didn't supply), which rendered a trailing "Ch. 12 · ".
 * Neither is a state to paper over with a fake value — the honest render is the
 * half we actually know. `prefix` labels the time where the card is on a different
 * clock ("Added 2d").
 */
export function cardSub(card: Card, prefix = ''): string {
	const time = card.time ? `${prefix}${card.time}` : '';
	// The COUNT and the newest chapter's LABEL are both printed, count first, the
	// same shape Browse uses ("12 ch · Ch. 151"). They are never substituted for one
	// another — see {@link Card.chCount}.
	const count = card.chCount && card.chCount > 0 ? `${card.chCount} ch` : '';
	return [count, card.ch, time].filter(Boolean).join(' · ');
}

/** URL-safe slug for title-based series links (fallback when no id is known). */
export function slug(title: string): string {
	return title
		.toLowerCase()
		.replace(/&/g, 'and')
		.replace(/[^a-z0-9]+/g, '-')
		.replace(/^-+|-+$/g, '');
}

// ---- Library shelves ---------------------------------------------------------

export type Shelf = 'reading' | 'completed' | 'onhold' | 'plan';

export const SHELF_META: Record<
	Shelf,
	{ label: string; color: string; bg: string; border: string }
> = {
	reading: {
		label: 'READING',
		color: 'var(--k-ongoing)',
		bg: 'rgba(95,191,126,0.16)',
		border: 'rgba(95,191,126,0.4)',
	},
	completed: {
		label: 'DONE',
		color: 'var(--k-on-primary)',
		bg: 'var(--k-primary)',
		border: 'var(--k-primary)',
	},
	onhold: {
		label: 'ON HOLD',
		color: '#e6c06a',
		bg: 'rgba(246,183,60,0.14)',
		border: 'rgba(246,183,60,0.4)',
	},
	plan: {
		label: 'PLAN',
		color: 'var(--k-text-3)',
		bg: 'rgba(255,255,255,0.08)',
		border: 'rgba(255,255,255,0.18)',
	},
};

/**
 * Whether a series will get no further chapters — the gate on the `completed` shelf.
 *
 * `hiatus` is deliberately NOT ended: a hiatus resumes, and a reader who is current
 * on one has not finished it. `unknown` folds to ongoing upstream (`STATUS_WORD`),
 * so an unclassified series is never auto-finished either.
 */
export function isEndedStatus(status: Status): boolean {
	return status === 'completed' || status === 'cancelled';
}

/**
 * The shelf a series lands on from read progress alone — the derivation used when
 * the viewer has NOT filed one by hand (an explicit `libraryStatus` always wins).
 *
 * `ended` is the SERIES' publication status, not the viewer's, and it gates
 * `completed` on purpose: reading every chapter that currently EXISTS of an ongoing
 * series does not finish it — more are coming, so the honest shelf is still
 * `reading`. Without that gate the library flipped a caught-up ongoing series to
 * DONE, which is the same word meaning two different things ({@link STATUS_META}'s
 * "Completed" = the series ended; {@link SHELF_META}'s = the viewer finished it).
 *
 * This is the ONE derivation. The library, the profile and the series page each used
 * to carry their own near-copy — the profile's had no `plan` branch and the series
 * page counted reads off a differently-deduped list — so one series could show three
 * different shelves on three screens.
 */
export function deriveShelf(read: number, total: number, ended: boolean): Shelf {
	if (ended && total > 0 && read >= total) return 'completed';
	if (read === 0) return 'plan';
	return 'reading';
}

// ---- Social view shapes --------------------------------------------------------

// Shape of a locally-persisted series review in the offline/backend-off fallback.
// No seeded data — the fallback starts empty (see CATALOGUE.md §7).
export interface SeriesComment {
	id: number;
	name: string;
	initial: string;
	chapter: string;
	time: string;
	body: string;
	hasSpoiler: boolean;
	likes: number;
	liked: boolean;
}

// Shape of a locally-persisted chapter comment in the offline/backend-off fallback.
// No seeded data — the fallback starts empty (see CATALOGUE.md §7).
export interface ReaderComment {
	id: string;
	name: string;
	initial: string;
	bg: string;
	fg: string;
	ts: number;
	time: string;
	isOp: boolean;
	hasSpoiler: boolean;
	likes: number;
	liked: boolean;
	replies: number;
	body: string;
}
