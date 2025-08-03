/**
 * Which copy of a chapter the reader actually opens, when the first choice cannot be read.
 *
 * A chapter number of a multi-source work is routinely carried by several copies (the
 * MangaDex spine's upload, a Suwayomi source's row, …), and the copy the reader defaults to
 * can fail to load for reasons that say nothing about the others: a dead or unreachable
 * Suwayomi engine (`pages` answers a GraphQL "Internal error"), an upload MangaDex has since
 * taken down (`canonicalPages` 404s upstream). The page fetch used to run bare inside
 * `getReaderChapter`'s `live()` wrapper, so that ONE rejection discarded everything already
 * resolved — title, chapter list, versions, prev/next — and the reader rendered
 * `emptyReader`: "Ch. 0", no pickers, nothing to switch to.
 *
 * The rules here keep a failing copy from blanking the reader:
 *   - try the copies in the caller's order; the first one that yields pages wins;
 *   - a copy that throws or yields no pages falls through to the next;
 *   - if NONE yields pages, the FIRST copy stands (the reader still shows the chapter it
 *     meant to open, with its list and switcher) with no pages, and `failed` says whether
 *     that was an error rather than an honestly empty chapter.
 *
 * Fallbacks are only ever copies of the SAME chapter number (the caller builds the list),
 * so this can never silently open a different chapter. Dependency-free so it is unit
 * testable on its own; `source.ts` owns the I/O.
 */

export interface ReadableCopy<C, P> {
	/** The copy whose pages these are. */
	copy: C;
	pages: P[];
	/** True when nothing yielded pages AND the opened copy's fetch threw. */
	failed: boolean;
}

export async function loadFirstReadable<C, P>(
	copies: readonly C[],
	load: (copy: C) => Promise<P[]>,
	onError: (copy: C, err: unknown) => void = () => {},
): Promise<ReadableCopy<C, P>> {
	if (!copies.length) throw new Error('loadFirstReadable: no copies to try');
	let firstFailed = false;
	for (let i = 0; i < copies.length; i++) {
		try {
			const pages = await load(copies[i]);
			if (pages.length) return { copy: copies[i], pages, failed: false };
		} catch (err) {
			onError(copies[i], err);
			if (i === 0) firstFailed = true;
		}
	}
	return { copy: copies[0], pages: [], failed: firstFailed };
}
