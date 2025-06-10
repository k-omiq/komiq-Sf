/**
 * Per-work preferred translator (source) selection, persisted to localStorage.
 *
 * A "translator" is one source/extension that carries a canonical work (e.g.
 * MangaDex vs MANGA Plus). Federated search returns a work with several
 * translators; the reader remembers which one the user reads a given work from,
 * keyed by the canonical `w_` workId. The stored value is a stable translator
 * key (`sourceType:suwayomiMangaId`), resolved back to a translator by the data
 * layer — so a stale/removed source falls back to the work's default.
 *
 * SSR is disabled in this app (`ssr = false`), so localStorage is always safe to
 * read from the browser-only `load`/getters that consume this.
 */
const KEY = 'komiq-translator-prefs';

/** Reactive map so a translator switch re-renders any component reading it. */
export const translatorPrefs = $state<{ map: Record<string, string> }>({ map: load() });

function load(): Record<string, string> {
	if (typeof localStorage === 'undefined') return {};
	try {
		const raw = localStorage.getItem(KEY);
		if (!raw) return {};
		const parsed = JSON.parse(raw);
		return parsed && typeof parsed === 'object' ? (parsed as Record<string, string>) : {};
	} catch {
		return {};
	}
}

function save(map: Record<string, string>): void {
	if (typeof localStorage === 'undefined') return;
	try {
		localStorage.setItem(KEY, JSON.stringify(map));
	} catch {
		/* private mode — selection just won't persist */
	}
}

/** The persisted translator key for a work, or undefined when none chosen. */
export function getPreferredTranslator(workId: string): string | undefined {
	return translatorPrefs.map[workId];
}

/** Persist the chosen translator key for a work. */
export function setPreferredTranslator(workId: string, key: string): void {
	translatorPrefs.map = { ...translatorPrefs.map, [workId]: key };
	save(translatorPrefs.map);
}
