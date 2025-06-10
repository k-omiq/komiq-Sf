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
	return [card.ch, time].filter(Boolean).join(' · ');
}

/** URL-safe slug for title-based series links (fallback when no id is known). */
export function slug(title: string): string {
	return title
		.toLowerCase()
		.replace(/&/g, 'and')
		.replace(/[^a-z0-9]+/g, '-')
		.replace(/^-+|-+$/g, '');
}

/**
 * "Browse by format" card chrome (name, blurb, glow colours). Counts are
 * derived from the live catalog sample in `source.ts`; the empty default
 * renders no count line.
 */
export const FORMAT_CARDS = [
	{
		type: 'Manga' as ComicType,
		flag: '🇯🇵',
		name: 'Manga',
		desc: 'Japanese comics · read right-to-left',
		count: '',
		glow: 'rgba(224,131,105,0.16)',
		hover: 'rgba(224,131,105,0.4)',
	},
	{
		type: 'Manhwa' as ComicType,
		flag: '🇰🇷',
		name: 'Manhwa',
		desc: 'Korean webtoons · full-colour vertical scroll',
		count: '',
		glow: 'rgba(198,156,240,0.16)',
		hover: 'rgba(198,156,240,0.4)',
	},
	{
		type: 'Manhua' as ComicType,
		flag: '🇨🇳',
		name: 'Manhua',
		desc: 'Chinese comics · sweeping colour art',
		count: '',
		glow: 'rgba(95,200,207,0.16)',
		hover: 'rgba(95,200,207,0.4)',
	},
];

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
