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
 *   2. WHICH COPY of the adjacent number. The one from the SAME scanlation group as the
 *      open copy when there is one (reading a group's run should keep reading that group),
 *      otherwise the caller's default copy — exactly what the reader opened before copies
 *      were visible at all.
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
 * The previous and next chapter NUMBER around `target`, each resolved to one copy.
 *
 * `pool` is every copy the reader can open from the ACTIVE source (it may hold several per
 * number, in any order). `isDefault` marks the caller's default copy of a number (for the
 * MangaDex spine: the copy `canonicalChapters` chose; for a Suwayomi source: the copies in
 * its own list). Per adjacent number the pick is: same group as `target` → a default copy
 * → the first copy.
 *
 * `target` need not be in `pool` (a non-default copy opened by id); its number is located
 * by value.
 */
export function adjacentChapters<C extends VersionedChapter>(
	pool: readonly C[],
	target: C,
	isDefault: (c: C) => boolean = () => true,
): { prev?: C; next?: C } {
	const asc = sortChaptersAsc(pool.some((c) => c.id === target.id) ? pool : [...pool, target]);
	// Distinct numbers, in reading order, each with its copies.
	const slots: C[][] = [];
	const index = new Map<string, number>();
	for (const c of asc) {
		const key = slotKey(c);
		const at = index.get(key);
		if (at === undefined) {
			index.set(key, slots.length);
			slots.push([c]);
		} else {
			slots[at].push(c);
		}
	}
	const here = index.get(slotKey(target));
	if (here === undefined) return {};
	const pick = (slot: C[] | undefined): C | undefined => {
		if (!slot?.length) return undefined;
		return (
			slot.find((c) => sameGroup(c.scanlator, target.scanlator)) ??
			slot.find((c) => isDefault(c)) ??
			slot[0]
		);
	};
	return { prev: pick(slots[here - 1]), next: pick(slots[here + 1]) };
}
