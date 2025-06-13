/**
 * Pure translator-selection helpers, factored out of `resolveWork` (source.ts) so the
 * selection PRECEDENCE — the part that has regressed before — is unit-testable without a
 * live `backend`/`images` context. Mirrors the `chapter-owner.ts` split.
 *
 * The rule these encode: the MangaDex Tachiyomi extension (`all.mangadex`) is the SAME
 * content as the MangaDex-direct canonical spine, but reached through the Suwayomi engine
 * (page images proxied via `api.komiq.cc`) instead of MangaDex@Home (`*.mangadex.network`).
 * When the direct spine already carries the chapters, that extension is a redundant, slower
 * duplicate: it is kept out of the default selection and the picker so MangaDex content is
 * served from the fast CDN. Non-MangaDex extensions are the sole source of their content
 * and are never treated as redundant.
 */

/** The MangaDex Tachiyomi extension package. */
export const MANGADEX_EXT_PKG = 'eu.kanade.tachiyomi.extension.all.mangadex';

/**
 * True when a Suwayomi source is the MangaDex extension AND a readable direct spine already
 * serves that same content — i.e. a redundant duplicate that must not shadow the fast
 * `mangadex.network` route. `hasReadableSpine` is "the direct spine carries ≥1 chapter": an
 * empty spine (e.g. a licensing takedown) can serve nothing, so the extension is then the
 * only readable MangaDex path and is NOT redundant.
 */
export function isRedundantMangadexExt(
	pkgName: string | null | undefined,
	hasReadableSpine: boolean,
): boolean {
	return pkgName === MANGADEX_EXT_PKG && hasReadableSpine;
}

/** Minimal candidate shape the default-pick precedence needs. */
export interface Selectable {
	key: string;
	chapterCount: number;
	/**
	 * The highest chapter NUMBER this source carries — how far ahead you can read in it.
	 * Distinct from {@link chapterCount}, which is how much of the series it has. A source
	 * that picked the series up midway can have 70 chapters and still reach chapter 151.
	 *
	 * `undefined` when the source has no numbered chapters at all (a oneshot-only source).
	 */
	maxChapterNumber?: number;
	/** A redundant all.mangadex extension (see {@link isRedundantMangadexExt}). */
	redundant: boolean;
}

/**
 * A chapter number above this is data corruption, not a chapter — a `TEST` upload numbered
 * 99999999, or a date used as a chapter number (20240120). Mirrors `SANE_MAX_CHAPTER` in
 * the server's `chapter_label` module; the longest genuine series in the catalogue is under
 * 2,000 chapters.
 */
export const SANE_MAX_CHAPTER = 5000;

/**
 * A source must carry at least this many chapters to be allowed to win on reach.
 *
 * MIS-MERGE GUARD, and it is not hypothetical. *One Piece (Official Colored)* has a
 * 764-chapter source topping out at 763 and a **one-chapter** source claiming chapter 1183
 * — almost certainly a bad dedup. Of the 50 multi-source works whose furthest-ahead source
 * differs from their most-chapters source, 4 have a winner with fewer than 3 chapters.
 */
const MIN_CHAPTERS_TO_LEAD = 3;

/**
 * …and at least this share of the leading source's chapter count. Ten of those 50 winners
 * carry under 10% of the leader; the other 36 are genuine partial pickups (≥25%) and are
 * exactly the case this rule exists to serve.
 */
const MIN_SHARE_OF_LEADER = 0.1;

/** The highest sane chapter number in a list, or `undefined` if none is usable. */
export function maxSaneChapterNumber(numbers: readonly number[]): number | undefined {
	let best: number | undefined;
	for (const n of numbers) {
		if (!Number.isFinite(n) || n < 0 || n > SANE_MAX_CHAPTER) continue;
		if (best === undefined || n > best) best = n;
	}
	return best;
}

/**
 * Choose the default translator key from an already-ordered candidate list (spine first).
 *
 * Precedence: a valid persisted preference, else the eligible candidate that is FURTHEST
 * AHEAD — highest chapter NUMBER — else the first eligible candidate.
 *
 * WHY REACH AND NOT COUNT. This used to sort by `chapterCount`, which answers "which source
 * has the most of this series" when the question a reader is asking is "which source can I
 * read the furthest in". Those differ on 50 multi-source works today, and the requirement is
 * explicit about which one wins:
 *
 *   "some sources pick up a series from midway. Series Y has 151 chapters, source X has 150
 *    translated, but another source picked it up halfway and has 70 chapters — but their
 *    most recent chapter is ch 151. That's what I meant by most updated chapter."
 *
 * On *The Immortal Emperor Luo Wuji Has Returned* the old rule picked a 130-chapter source
 * that stops at 130 over a 46-chapter source that reaches 199 — defaulting the reader 69
 * chapters behind.
 *
 * Note this is deliberately NOT the same question as "who published the newest chapter
 * first". That is the updates feed's rule (the release ledger); this one is a property of a
 * source's chapter RANGE, and the ledger only breaks ties here.
 *
 * Selecting a midway source does not hide earlier chapters — `buildAggregatedChapters`
 * falls back per chapter to any source that carries it.
 *
 * Redundant candidates are never eligible: their content is the direct spine, so a default
 * (or a stale persisted preference) can never route MangaDex reads onto the slow proxy —
 * it heals to the spine here. Returns `undefined` only for an empty list.
 */
export function pickDefaultKey(
	ordered: Selectable[],
	preferredKey?: string | null,
): string | undefined {
	const eligible = ordered.filter((c) => !c.redundant);
	// A saved preference ALWAYS wins over anything computed here.
	const preferred = preferredKey ? eligible.find((c) => c.key === preferredKey) : undefined;
	if (preferred) return preferred.key;

	const largest = eligible.reduce((m, c) => Math.max(m, c.chapterCount), 0);
	const contenders = eligible.filter(
		(c) =>
			c.maxChapterNumber !== undefined &&
			c.chapterCount >= MIN_CHAPTERS_TO_LEAD &&
			c.chapterCount >= largest * MIN_SHARE_OF_LEADER,
	);
	// Stable sort → preserves the incoming (spine-first) order when two sources reach the
	// same chapter, which is the common case for a fully-mirrored series.
	const furthestAhead = [...contenders].sort(
		(a, b) => (b.maxChapterNumber as number) - (a.maxChapterNumber as number),
	)[0];
	// No source survived the guard (every candidate is tiny, or none has a numbered
	// chapter) — fall back to the old most-chapters rule rather than to nothing.
	const byMostChapters = [...eligible].sort((a, b) => b.chapterCount - a.chapterCount)[0];
	return (furthestAhead ?? byMostChapters ?? eligible[0] ?? ordered[0])?.key;
}
