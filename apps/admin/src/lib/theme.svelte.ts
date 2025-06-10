/**
 * Theme preference (light / dark / follow-system), persisted to localStorage and
 * applied by setting `data-theme` on <html>. The design tokens (@komika/ui) carry
 * the actual light/dark values; this just flips the attribute.
 *
 * A no-FOUC inline script in app.html sets the initial attribute before paint;
 * `initTheme()` re-syncs the reactive store on mount and tracks OS changes while
 * the preference is "system".
 */
export type ThemePref = 'light' | 'dark' | 'system';

const KEY = 'komika-theme';

export const theme = $state<{ pref: ThemePref }>({ pref: 'system' });

function systemDark(): boolean {
	return typeof matchMedia !== 'undefined' && matchMedia('(prefers-color-scheme: dark)').matches;
}

/** The concrete theme a preference resolves to right now. */
export function resolvedTheme(pref: ThemePref = theme.pref): 'light' | 'dark' {
	return pref === 'system' ? (systemDark() ? 'dark' : 'light') : pref;
}

function apply(pref: ThemePref): void {
	if (typeof document === 'undefined') return;
	document.documentElement.dataset.theme = resolvedTheme(pref);
}

/** Restore the saved preference and start tracking OS changes. Call once on mount. */
export function initTheme(): void {
	let stored: ThemePref = 'system';
	try {
		const v = localStorage.getItem(KEY);
		if (v === 'light' || v === 'dark' || v === 'system') stored = v;
	} catch {
		/* private mode — fall back to system */
	}
	theme.pref = stored;
	apply(stored);
	if (typeof matchMedia !== 'undefined') {
		matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
			if (theme.pref === 'system') apply('system');
		});
	}
}

export function setTheme(pref: ThemePref): void {
	theme.pref = pref;
	try {
		localStorage.setItem(KEY, pref);
	} catch {
		/* ignore */
	}
	apply(pref);
}

/** Single-button cycle: dark → light → system → dark. */
export function cycleTheme(): void {
	setTheme(theme.pref === 'dark' ? 'light' : theme.pref === 'light' ? 'system' : 'dark');
}
