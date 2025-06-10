/**
 * Which source of a work actually carries a requested chapter.
 *
 * A work is mirrored by several sources and they do NOT share chapter ids. The
 * reader defaults to ONE of them (the persisted preference, else the one with the
 * most chapters), so a `?ch=` pointing at a different source's chapter is a routine
 * event, not a corrupt link — a "new chapter" notification is emitted for whichever
 * source the scanner saw it on, and `notifications.ts` had no `src` to attach.
 *
 * The reader used to answer that case by rendering an EMPTY view (`pages: []`),
 * which presents as a blank page that never requests a single image — the failure
 * looks like "the image proxy is broken" while the proxy is never asked for
 * anything. The right answer is to open the chapter FROM THE SOURCE THAT HAS IT.
 *
 * The invariant that motivated the old behaviour still holds and is the reason this
 * matches on chapter ID and never on number: it must be impossible to silently open
 * a DIFFERENT chapter than the one asked for. Finding the same id under another
 * source is not a substitution — it is the same chapter, correctly located.
 *
 * Kept dependency-free (no backend, no Svelte, no DOM) so the decision is unit
 * testable on its own; `source.ts` owns the I/O around it.
 */

/** The minimum a chapter must expose for ownership resolution. */
export interface OwnedChapter {
	id: string;
}

/** One candidate source of a work, with the chapters it carries. */
export interface ChapterCandidate<C extends OwnedChapter = OwnedChapter> {
	/** Translator key (`TranslatorOption.key`) — identifies the source in the picker. */
	key: string;
	/**
	 * Suwayomi manga id for a federated source, or null for the MangaDex spine.
	 * Null selects the canonical page resolver downstream, so it must be preserved
	 * exactly rather than collapsed to a string.
	 */
	suwayomiMangaId: string | null;
	chapters: C[];
}

/** Where a requested chapter was found. */
export interface ChapterOwner<C extends OwnedChapter = OwnedChapter> {
	candidate: ChapterCandidate<C>;
	chapter: C;
	/** True when the owner is a source OTHER than the one already selected. */
	switched: boolean;
}

/**
 * Locate `chapterId` among a work's sources.
 *
 * Resolution order matters: the ALREADY-SELECTED source wins when it carries the
 * chapter, so an ordinary read never switches source as a side effect of this
 * lookup (which would silently override the reader's own choice and change the
 * chapter list under them). Only a genuine miss falls through to the others, in the
 * order given — `resolveWork` orders them spine-first then by chapter count, so the
 * most complete source is preferred among equals.
 *
 * Returns null when NO source carries the id: an honestly unknown chapter, which
 * the caller should still degrade to the empty state rather than invent a target.
 */
export function findChapterOwner<C extends OwnedChapter>(
	candidates: readonly ChapterCandidate<C>[],
	chapterId: string,
	selectedKey?: string | null,
): ChapterOwner<C> | null {
	if (!chapterId) return null;

	const selected = selectedKey ? candidates.find((c) => c.key === selectedKey) : undefined;
	if (selected) {
		const hit = selected.chapters.find((c) => c.id === chapterId);
		if (hit) return { candidate: selected, chapter: hit, switched: false };
	}

	for (const candidate of candidates) {
		if (selected && candidate.key === selected.key) continue; // already missed
		const hit = candidate.chapters.find((c) => c.id === chapterId);
		if (hit) {
			return { candidate, chapter: hit, switched: candidate.key !== selectedKey };
		}
	}
	return null;
}
