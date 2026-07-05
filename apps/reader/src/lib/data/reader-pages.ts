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

export interface LoadFirstReadableOptions {
	/**
	 * Give up on ONE copy after this long and fall through to the next, as if it had thrown.
	 * The shared GraphQL client has no request timeout, so a source that accepts the
	 * connection and never answers (a wedged Suwayomi engine, an upstream that stalls) used
	 * to hold the reader's `load` — and with it the whole navigation — open forever: the
	 * old chapter stayed dimmed under the loading overlay and nothing ever changed.
	 */
	timeoutMs?: number;
}

export async function loadFirstReadable<C, P>(
	copies: readonly C[],
	load: (copy: C) => Promise<P[]>,
	onError: (copy: C, err: unknown) => void = () => {},
	opts: LoadFirstReadableOptions = {},
): Promise<ReadableCopy<C, P>> {
	if (!copies.length) throw new Error('loadFirstReadable: no copies to try');
	let firstFailed = false;
	for (let i = 0; i < copies.length; i++) {
		try {
			const attempt = load(copies[i]);
			const pages = opts.timeoutMs
				? await withTimeout(attempt, opts.timeoutMs, 'page list')
				: await attempt;
			if (pages.length) return { copy: copies[i], pages, failed: false };
		} catch (err) {
			onError(copies[i], err);
			if (i === 0) firstFailed = true;
		}
	}
	return { copy: copies[0], pages: [], failed: firstFailed };
}

/** The rejection {@link withTimeout} produces, so callers can tell a stall from an error. */
export class TimeoutError extends Error {
	constructor(label: string, ms: number) {
		super(`${label} timed out after ${ms} ms`);
		this.name = 'TimeoutError';
	}
}

/**
 * `promise`, or a {@link TimeoutError} once `ms` pass without it settling. The timer is
 * always cleared, so a settled race leaves nothing behind. The original promise is marked
 * handled: if it rejects AFTER the deadline nobody is listening any more, and that must not
 * surface as an unhandled rejection.
 */
export function withTimeout<T>(promise: Promise<T>, ms: number, label = 'request'): Promise<T> {
	promise.catch(() => {});
	let timer: ReturnType<typeof setTimeout> | undefined;
	const deadline = new Promise<never>((_, reject) => {
		timer = setTimeout(() => reject(new TimeoutError(label, ms)), ms);
	});
	return Promise.race([promise, deadline]).finally(() => clearTimeout(timer));
}

/**
 * `promise`'s value, or `fallback` if it rejects or has not settled within `ms`. For the
 * reader's SECONDARY data — a series title, the version list — which must never keep a
 * chapter whose pages are already in hand off the screen.
 */
export async function settleWithin<T>(promise: Promise<T>, ms: number, fallback: T): Promise<T> {
	try {
		return await withTimeout(promise, ms);
	} catch {
		return fallback;
	}
}
