/**
 * Deriving a chapter's display label when there is no Komika server to ask.
 *
 * WHY THIS EXISTS AT ALL. `Chapter.label` is normally decided ONCE, server-side, by
 * `chapter_label::chapter_display` in the Rust backend, and every surface just prints it.
 * That is the whole point of the Phase A2 contract: one rule, so "Ch. N" means the same
 * thing everywhere.
 *
 * The two direct-Suwayomi adapters (`suwayomi-backend`, `local-suwayomi-backend`) are the
 * one path that has no server in it — they talk to a Suwayomi instance directly, for the
 * native/desktop build. They still have to produce a `Chapter`, so they still have to
 * produce a label, and the only honest thing to do is apply the same precedence the server
 * would.
 *
 * THIS IS A MIRROR, NOT A SECOND RULE. Keep it in step with `chapter_label.rs`. It is
 * deliberately the *minimal* half of that rule — structured number when sane, else the
 * source's own words — because the name-parsing fallback there is a ~0.15% path
 * (`suwayomi_chapter.chapter_number` already agreed with the name on 3,994 of 4,000 sampled
 * production rows) and duplicating a parser is how mirrors drift.
 */

/**
 * Above this, a "chapter number" is data corruption rather than a chapter. Mirrors
 * `SANE_MAX_CHAPTER` in the Rust `chapter_label` module.
 *
 * Measured on production: two Suwayomi series carry numbers like `99999999` (a TEST upload)
 * and `20240120` (a DATE used as a chapter number). The longest genuine series in the
 * catalogue is under 2,000 chapters.
 */
export const SANE_MAX_CHAPTER = 5000;

/**
 * Is this a number a chapter could plausibly have? Negative numbers are excluded —
 * **Suwayomi uses `-1` as its oneshot sentinel** — while `0` is kept, because `Chapter 0`
 * is real and common in webtoons.
 */
export function isSaneChapterNumber(n: number): boolean {
	return Number.isFinite(n) && n >= 0 && n <= SANE_MAX_CHAPTER;
}

/**
 * Format a chapter number the way a reader writes it: no trailing `.0`, at most two
 * decimals (all the `round(number * 100)` grouping key preserves anyway).
 */
export function formatChapterNumber(n: number): string {
	if (Number.isInteger(n)) return String(n);
	return String(Number(n.toFixed(2)));
}

/**
 * The label for one Suwayomi chapter: its structured number when that number is sane, else
 * the source's own words for it.
 *
 * The fallback is what stops `Ch. -1` and `Ch. 99999999` reaching a series page — the
 * sentinel and the corruption both fail `isSaneChapterNumber` and the chapter is labelled
 * with whatever the scanlator actually called it ("Oneshot", "Extra"). An empty name with
 * an insane number is the only case left, and "Oneshot" is a better guess there than
 * printing the corruption.
 */
export function suwayomiChapterLabel(chapterNumber: number, name: string | null): string {
	if (isSaneChapterNumber(chapterNumber)) return formatChapterNumber(chapterNumber);
	const trimmed = (name ?? '').trim();
	return trimmed === '' ? 'Oneshot' : trimmed;
}
