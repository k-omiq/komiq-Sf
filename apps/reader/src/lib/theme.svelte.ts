/**
 * The look of the app, kept on the device (localStorage) and applied on <html>:
 *
 *  - mode: light / dark / follow-system, as `data-theme`. The design tokens
 *    (@komika/ui) carry the light and dark values.
 *  - palette: one seed colour, as `data-palette` + `--k-seed`. `theme.css` derives the
 *    accent, the primary button and a tint for every surface from that seed, so a
 *    preset and a colour the reader picked go through the same rules. The default
 *    palette sets neither and leaves the tokens as they are.
 *  - pure black: `data-black`, a true-black background for the dark theme.
 *
 * A no-FOUC script (static/theme-init.js) applies the same three before first paint;
 * `initTheme()` re-syncs the reactive store on mount and tracks OS changes while the
 * mode is "system". Both read the same two keys: keep them in step.
 */
export type ThemePref = 'light' | 'dark' | 'system';

export interface Palette {
	id: string;
	name: string;
	/** The seed colour, `#rrggbb`. */
	seed: string;
}

/** The id of the palette that leaves the design tokens untouched. */
export const DEFAULT_PALETTE = 'default';
/** The id of the palette whose seed the reader picked. */
export const CUSTOM_PALETTE = 'custom';

/** Presets, in the order the settings screen shows them. The first is the default,
 * and its seed is only the swatch colour (the tokens' own coral accent). */
export const PALETTES: readonly Palette[] = [
	{ id: DEFAULT_PALETTE, name: 'komiq', seed: '#e0836a' },
	{ id: 'strawberry', name: 'Strawberry', seed: '#d8475d' },
	{ id: 'dusk', name: 'Midnight dusk', seed: '#e5398a' },
	{ id: 'lavender', name: 'Lavender', seed: '#9b7fe0' },
	{ id: 'sapphire', name: 'Sapphire', seed: '#3f6fe0' },
	{ id: 'tidal', name: 'Tidal wave', seed: '#2f93cf' },
	{ id: 'nord', name: 'Nord', seed: '#6f9bc4' },
	{ id: 'teal', name: 'Teal', seed: '#1fa39b' },
	{ id: 'apple', name: 'Green apple', seed: '#56a644' },
	{ id: 'honey', name: 'Honey', seed: '#d39a2c' },
	{ id: 'mocha', name: 'Mocha', seed: '#a9805c' },
	{ id: 'mono', name: 'Monochrome', seed: '#8a8a8a' },
];

const MODE_KEY = 'komika-theme';
/** `{ palette, seed, custom, black }` — `seed` is what theme-init.js applies. */
const LOOK_KEY = 'komiq-look';
const HEX = /^#[0-9a-f]{6}$/i;

export const theme = $state<{
	pref: ThemePref;
	/** A preset id, or `CUSTOM_PALETTE`. */
	palette: string;
	/** The reader's own colour, kept while a preset is in use. */
	custom: string;
	pureBlack: boolean;
}>({ pref: 'system', palette: DEFAULT_PALETTE, custom: '#7c6cf0', pureBlack: false });

function systemDark(): boolean {
	return typeof matchMedia !== 'undefined' && matchMedia('(prefers-color-scheme: dark)').matches;
}

/** The concrete theme a preference resolves to right now. */
export function resolvedTheme(pref: ThemePref = theme.pref): 'light' | 'dark' {
	return pref === 'system' ? (systemDark() ? 'dark' : 'light') : pref;
}

/** The seed colour in use, or null for the default palette. */
function seed(): string | null {
	if (theme.palette === CUSTOM_PALETTE) return theme.custom;
	if (theme.palette === DEFAULT_PALETTE) return null;
	return PALETTES.find((p) => p.id === theme.palette)?.seed ?? null;
}

function apply(): void {
	if (typeof document === 'undefined') return;
	const root = document.documentElement;
	const mode = resolvedTheme();
	root.dataset.theme = mode;
	const s = seed();
	if (s) {
		root.dataset.palette = theme.palette;
		root.style.setProperty('--k-seed', s);
	} else {
		delete root.dataset.palette;
		root.style.removeProperty('--k-seed');
	}
	if (theme.pureBlack) root.dataset.black = '';
	else delete root.dataset.black;
	// The browser / OS bar around the page. A palette tints the background by a few
	// percent only, so the untinted value is close enough here.
	const bar = mode === 'light' ? '#f4f2ee' : theme.pureBlack ? '#000000' : '#0c0c0d';
	document.querySelector('meta[name="theme-color"]')?.setAttribute('content', bar);
}

function save(): void {
	try {
		localStorage.setItem(MODE_KEY, theme.pref);
		localStorage.setItem(
			LOOK_KEY,
			JSON.stringify({
				palette: theme.palette,
				seed: seed(),
				custom: theme.custom,
				black: theme.pureBlack,
			}),
		);
	} catch {
		/* private mode: the choice lasts until reload only */
	}
}

/** Restore the saved look and start tracking OS changes. Call once on mount. */
export function initTheme(): void {
	try {
		const mode = localStorage.getItem(MODE_KEY);
		if (mode === 'light' || mode === 'dark' || mode === 'system') theme.pref = mode;
		const look = JSON.parse(localStorage.getItem(LOOK_KEY) || '{}') ?? {};
		if (typeof look.custom === 'string' && HEX.test(look.custom)) theme.custom = look.custom;
		if (look.palette === CUSTOM_PALETTE || PALETTES.some((p) => p.id === look.palette)) {
			theme.palette = look.palette;
		}
		theme.pureBlack = look.black === true;
	} catch {
		/* private mode or a damaged value: keep the defaults */
	}
	apply();
	if (typeof matchMedia !== 'undefined') {
		matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
			if (theme.pref === 'system') apply();
		});
	}
}

export function setTheme(pref: ThemePref): void {
	theme.pref = pref;
	save();
	apply();
}

/** Use a preset palette (`DEFAULT_PALETTE` for the app's own colours). */
export function setPalette(id: string): void {
	theme.palette = id;
	save();
	apply();
}

/** Use the reader's own colour (`#rrggbb`) as the palette. */
export function setCustomColor(hex: string): void {
	if (!HEX.test(hex)) return;
	theme.custom = hex;
	theme.palette = CUSTOM_PALETTE;
	save();
	apply();
}

export function setPureBlack(on: boolean): void {
	theme.pureBlack = on;
	save();
	apply();
}

/** Back to the look of a fresh install. */
export function resetTheme(): void {
	theme.pref = 'system';
	theme.palette = DEFAULT_PALETTE;
	theme.pureBlack = false;
	save();
	apply();
}

/** Single-button cycle: dark → light → system → dark. */
export function cycleTheme(): void {
	setTheme(theme.pref === 'dark' ? 'light' : theme.pref === 'light' ? 'system' : 'dark');
}
