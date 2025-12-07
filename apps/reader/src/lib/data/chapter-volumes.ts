/**
 * Volume headers for the series page's chapter list ("Volume 3", "No volume"), MangaDex
 * style: a header opens wherever the volume CHANGES between consecutive rows, in whatever
 * order the list is sorted and paged. Grouping is by adjacency, not by collecting every
 * row of a volume, on purpose — the list is sortable (newest, oldest, …) and paged 100 at a
 * time, and re-ordering rows to gather a volume would silently override the reader's sort.
 *
 * A list where NO row carries a volume gets no headers at all (an older API, a
 * Suwayomi-only work): a lone "No volume" header over everything says nothing. Once any row
 * has one, the unlabelled rows get "No volume" so they are not read as part of the volume
 * above them. Dependency-free so it is unit testable on its own.
 */

/** The header to print ABOVE row `i`, or `null` for none. Same length as `volumes`. */
export function volumeHeaders(volumes: readonly (string | null | undefined)[]): (string | null)[] {
	const norm = volumes.map((v) => v?.trim() || null);
	if (!norm.some((v) => v !== null)) return norm.map(() => null);
	return norm.map((v, i) =>
		i === 0 || v !== norm[i - 1] ? (v === null ? 'No volume' : `Volume ${v}`) : null,
	);
}
