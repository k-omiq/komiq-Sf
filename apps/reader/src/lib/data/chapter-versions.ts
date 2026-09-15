/**
 * Several copies of one chapter number — and how to walk between numbers without
 * hopping between copies by accident.
 *
 * A chapter NUMBER is routinely carried by more than one upload: MangaDex holds "Ch. 3"
 * from "Uwaki Scans" AND from "duaduapertujuh", and a Suwayomi source can list one row per
 * scanlator (Tsukimichi: 117 chapters as 151 rows). The series page and the reader show
 * every copy by name; this module owns the two decisions that makes non-trivial:
 *
 *   1. READING ORDER. Prev/next used to be `list[idx ± 1]` over whatever order the list
 *      arrived in. That is wrong twice over once copies exist: the neighbour of copy A of
 *      ch. 3 is copy B of ch. 3 (the button "advances" to the chapter you just read), and a
 *      list that arrives newest-first silently swaps prev and next. Here the order is always
 *      rebuilt — ascending by number, unnumbered chapters (oneshots/extras) last — and
 *      prev/next move to the adjacent DISTINCT number.
 *
 *   2. WHICH COPY of the adjacent number. First the SOURCE: the series' default source
 *      when it carries the number, otherwise another source for that one chapter (the
 *      caller ranks sources with `tier`). Then, among that source's copies, the one from
 *      the SAME scanlation group as the open copy when there is one (reading a group's run
 *      should keep reading that group), otherwise the caller's default copy.
 *
 * Dependency-free (no backend, no Svelte) so it is unit-testable on its own, like
 * `chapter-owner.ts`; `source.ts` owns the I/O around it.
 */

/** What MangaDex calls an upload with no scanlation group (`mangadex::NO_GROUP`). */
export const NO_GROUP = 'No Group';

/** The minimum a chapter copy must expose for ordering and group matching. */
export interface VersionedChapter {
	id: string;
	/** Wire number; `0` for an unnumbered chapter — see {@link isNumberedLabel}. */
	number: number;
	/** The server's display label ("45", "10.5", "Oneshot"). */
	label?: string;
	/** The scanlation group(s) of this copy; null/'' = unknown. */
	scanlator?: string | null;
}

/**
 * A group label fit to show or compare: trimmed, with the server's "asked but could not
 * learn" `''` (and whitespace) folded to `null` so no surface renders an empty name.
 */
export function normalizeGroup(group: string | null | undefined): string | null {
	const g = (group ?? '').trim();
	return g === '' ? null : g;
}

/** Identify each upload by its hosting source and group (or uploader fallback). */
export function chapterAttribution(source: string, group: string | null | undefined): string {
	const credit = normalizeGroup(group);
	return credit ? `${source} · ${credit}` : source;
}

/**
 * Whether a label names a NUMBERED chapter. The wire `number` cannot tell: a oneshot
 * arrives as `0`, indistinguishable from a real Chapter 0, and sorting by it floats every
 * oneshot ahead of ch. 1 (CR4). The label is what the server decided, so it decides here.
 * A missing label (an older document) is treated as numbered — the pre-label behaviour.
 */
export function isNumberedLabel(label: string | undefined): boolean {
	if (label === undefined) return true;
	return /^\d+(\.\d+)?$/.test(label.trim());
}

/** The identity of a chapter NUMBER: `round(number * 100)` like the server's spine key,
 *  or the chapter's own id for an unnumbered one (every oneshot is its own slot). */
function slotKey(c: VersionedChapter): string {
	return isNumberedLabel(c.label) ? `n:${Math.round(c.number * 100)}` : `x:${c.id}`;
}

/**
 * Ascending reading order, rebuilt from scratch whatever order the list arrived in:
 * numbered chapters by number, unnumbered ones after them. STABLE — copies of one number,
 * and unnumbered chapters among themselves, keep their arrival order, which is what lets
 * "the first copy" mean the caller's default.
 */
export function sortChaptersAsc<C extends VersionedChapter>(chapters: readonly C[]): C[] {
	return chapters
		.map((c, i) => ({ c, i, numbered: isNumberedLabel(c.label) }))
		.sort((a, b) => {
			if (a.numbered !== b.numbered) return a.numbered ? -1 : 1;
			if (a.numbered && a.c.number !== b.c.number) return a.c.number - b.c.number;
			return a.i - b.i;
		})
		.map((x) => x.c);
}

/** Every copy that shares `target`'s chapter number (including `target` itself when it is
 *  in the list), in {@link sortChaptersAsc} order. */
export function copiesOf<C extends VersionedChapter>(chapters: readonly C[], target: C): C[] {
	const key = slotKey(target);
	return sortChaptersAsc(chapters).filter((c) => c.id === target.id || slotKey(c) === key);
}

/**
 * Whether two copies are by the same scanlation group. "No Group" is NOT an identity — two
 * group-less uploads are not by the same people in any sense a reader cares about — and an
 * unknown group matches nothing.
 */
function sameGroup(a: string | null | undefined, b: string | null | undefined): boolean {
	const ga = normalizeGroup(a);
	const gb = normalizeGroup(b);
	return ga !== null && ga !== NO_GROUP && ga === gb;
}

/**
 * The reading order around `target` — ONE copy per chapter number — and the previous and
 * next number in it.
 *
 * `pool` is every copy the reader can open (several per number, from several sources, in
 * any order). Per number the pick is, in this order:
 *
 *   1. the lowest `tier` present — the caller's source ranking, lower wins. The reader
 *      passes 0 for the series' default source, so a chapter is read from another source
 *      only when the default one lacks that number, and the chapter after it goes back;
 *   2. among those: same group as `target` → a default copy → the first copy. `isDefault`
 *      marks the caller's default copy of a number (for the MangaDex spine: the copy
 *      `canonicalChapters` chose; for a Suwayomi source: the copies in its own list).
 *
 * The open number is always represented by `target` itself. `target` need not be in
 * `pool` (a non-default copy opened by id); its number is located by value.
 */
export function adjacentChapters<C extends VersionedChapter>(
	pool: readonly C[],
	target: C,
	isDefault: (c: C) => boolean = () => true,
	tier: (c: C) => number = () => 0,
): { prev?: C; next?: C; order: C[] } {
	const asc = sortChaptersAsc(pool.some((c) => c.id === target.id) ? pool : [...pool, target]);
	// Distinct numbers, in reading order (a Map keeps insertion order), each with its copies.
	const slots = new Map<string, C[]>();
	for (const c of asc) {
		const key = slotKey(c);
		const slot = slots.get(key);
		if (slot) slot.push(c);
		else slots.set(key, [c]);
	}
	const order = [...slots.values()].map((slot) => {
		const open = slot.find((c) => c.id === target.id);
		if (open) return open;
		const best = Math.min(...slot.map(tier));
		const top = slot.filter((c) => tier(c) === best);
		return (
			top.find((c) => sameGroup(c.scanlator, target.scanlator)) ??
			top.find((c) => isDefault(c)) ??
			top[0]
		);
	});
	const here = order.findIndex((c) => c.id === target.id);
	return { prev: order[here - 1], next: order[here + 1], order };
}
