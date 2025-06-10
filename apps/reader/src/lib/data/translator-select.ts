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
	/** A redundant all.mangadex extension (see {@link isRedundantMangadexExt}). */
	redundant: boolean;
}

/**
 * Choose the default translator key from an already-ordered candidate list (spine first).
 * Precedence: a valid persisted preference, else the eligible candidate carrying the MOST
 * chapters (so a work whose spine has few/none but another source is complete — e.g. Solo
 * Leveling → Asura — defaults to the readable source), else the first eligible candidate.
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
	const preferred = preferredKey ? eligible.find((c) => c.key === preferredKey) : undefined;
	// Stable sort → preserves the incoming (spine-first) order within equal chapter counts.
	const byMostChapters = [...eligible].sort((a, b) => b.chapterCount - a.chapterCount)[0];
	return (preferred ?? byMostChapters ?? eligible[0] ?? ordered[0])?.key;
}
