/**
 * The look of the app, kept on the device (localStorage) and applied on <html>:
 *
 *  - mode: light / dark / follow-system, as `data-theme`. The design tokens
 *    (@komika/ui) carry the light and dark values.
 *  - background: none, one colour or a two-colour gradient, as `data-tint` plus the
 *    `--k-tint-*` variables. `theme.css` tints the page and every surface from them.
 *  - accent: the app's own coral (nothing set), or the reader's colour as
 *    `data-accent` + `--k-accent-seed`. `theme.css` derives the accent and the
 *    primary button from it.
 *  - pure black: `data-black`, a true-black background for the dark theme.
 *  - MangaDex ratings: `data-md-ratings="off"` hides MangaDex's rating wherever a
 *    rating is shown ($lib/components/Ratings.svelte owns the rule), so a card says
 *    the same thing on its first paint as after the app has started.
 *
 * Only hue and saturation come from the reader's colours. theme.css keeps the
 * lightness of each token, so text stays readable whatever colour is picked.
 *
 * A no-FOUC script (static/theme-init.js) applies the same before first paint from
 * the `attrs` and `vars` saved here; `initTheme()` re-syncs the reactive store on
 * mount and tracks OS changes while the mode is "system". Both read the same two
 * keys: keep them in step.
 */
import { untrack } from 'svelte';
import { isHex, midpoint } from './color';

export type ThemePref = 'light' | 'dark' | 'system';
export type Backdrop = 'none' | 'solid' | 'gradient';

export interface Look {
	backdrop: Backdrop;
	/** The background colour, and where a gradient starts. `#rrggbb`. */
	a: string;
	/** Where a gradient ends. `#rrggbb`. */
	b: string;
	/** Gradient direction in degrees, 0 = towards the top. */
	angle: number;
	/** How much colour the background takes, 0-100. */
	strength: number;
	/** The accent colour, or null for the app's own. */
	accent: string | null;
}

export interface Preset {
	id: string;
	name: string;
	look: Look;
}

export const STRENGTH_DEFAULT = 45;

function solid(seed: string, strength = STRENGTH_DEFAULT): Look {
	return { backdrop: 'solid', a: seed, b: seed, angle: 160, strength, accent: seed };
}
function gradient(a: string, b: string, accent: string, angle = 160): Look {
	return { backdrop: 'gradient', a, b, angle, strength: 60, accent };
}

/** The look of a fresh install: the tokens as @komika/ui ships them. */
export const DEFAULT_LOOK: Look = {
	backdrop: 'none',
	a: '#3f6fe0',
	b: '#1fa39b',
	angle: 160,
	strength: STRENGTH_DEFAULT,
	accent: null,
};

/** Ready-made looks, in the order the settings screen shows them. The ids of the
 * one-colour presets are the palette ids an older version saved. */
export const PRESETS: readonly Preset[] = [
	{ id: 'default', name: 'komiq', look: DEFAULT_LOOK },
	{ id: 'dusk', name: 'Midnight dusk', look: gradient('#e5398a', '#5b3fd0', '#e5398a') },
	{ id: 'tidal', name: 'Tidal wave', look: gradient('#2f93cf', '#1fa39b', '#2f93cf') },
	{ id: 'ember', name: 'Ember', look: gradient('#d39a2c', '#d8475d', '#e0836a', 200) },
	{ id: 'forest', name: 'Forest', look: gradient('#1fa39b', '#56a644', '#7fc06d', 135) },
	{ id: 'strawberry', name: 'Strawberry', look: solid('#d8475d') },
	{ id: 'lavender', name: 'Lavender', look: solid('#9b7fe0') },
	{ id: 'sapphire', name: 'Sapphire', look: solid('#3f6fe0') },
	{ id: 'nord', name: 'Nord', look: solid('#6f9bc4') },
	{ id: 'teal', name: 'Teal', look: solid('#1fa39b') },
	{ id: 'apple', name: 'Green apple', look: solid('#56a644') },
	{ id: 'honey', name: 'Honey', look: solid('#d39a2c') },
	{ id: 'mocha', name: 'Mocha', look: solid('#a9805c') },
	{ id: 'mono', name: 'Monochrome', look: { ...solid('#8a8a8a'), backdrop: 'none' } },
];

/** Quick picks: the accent row and the colour picker offer these. */
export const SWATCHES: readonly { name: string; hex: string }[] = [
	{ name: 'Coral', hex: '#e0836a' },
	{ name: 'Strawberry', hex: '#d8475d' },
	{ name: 'Pink', hex: '#e5398a' },
	{ name: 'Lavender', hex: '#9b7fe0' },
	{ name: 'Indigo', hex: '#5b3fd0' },
	{ name: 'Sapphire', hex: '#3f6fe0' },
	{ name: 'Sky', hex: '#2f93cf' },
	{ name: 'Teal', hex: '#1fa39b' },
	{ name: 'Green apple', hex: '#56a644' },
	{ name: 'Honey', hex: '#d39a2c' },
	{ name: 'Mocha', hex: '#a9805c' },
	{ name: 'Grey', hex: '#8a8a8a' },
];

const MODE_KEY = 'komika-theme';
/** `{ v: 2, look, black, md, attrs, vars }`. `attrs` and `vars` are what theme-init.js
 * puts on <html>; `md` is false only when the reader turned MangaDex ratings off. An
 * older version saved `{ palette, seed, custom, black, md }`: see `fromV1`. */
const LOOK_KEY = 'komiq-look';
/** The variables `apply()` may set, so it can also clear them. */
const VARS = [
	'--k-tint-a',
	'--k-tint-b',
	'--k-tint-mid',
	'--k-tint-angle',
	'--k-tint',
	'--k-accent-seed',
];

export const theme = $state<{
	pref: ThemePref;
	look: Look;
	pureBlack: boolean;
	/** Show MangaDex's rating next to our own readers' rating. */
	mdRatings: boolean;
}>({
	pref: 'system',
	look: { ...DEFAULT_LOOK },
	pureBlack: false,
	mdRatings: true,
});

function systemDark(): boolean {
	return typeof matchMedia !== 'undefined' && matchMedia('(prefers-color-scheme: dark)').matches;
}

/** The concrete theme a preference resolves to right now. */
export function resolvedTheme(pref: ThemePref = theme.pref): 'light' | 'dark' {
	return pref === 'system' ? (systemDark() ? 'dark' : 'light') : pref;
}

export function sameLook(x: Look, y: Look): boolean {
	if (x.backdrop !== y.backdrop || x.accent !== y.accent) return false;
	if (x.backdrop === 'none') return true;
	if (x.a !== y.a || x.strength !== y.strength) return false;
	return x.backdrop === 'solid' || (x.b === y.b && x.angle === y.angle);
}

/** The preset the current look is, if it is one. */
export function activePreset(): string | null {
	return PRESETS.find((p) => sameLook(p.look, theme.look))?.id ?? null;
}

/** What goes on <html> for a look: the data attributes and the CSS variables. */
function paint(look: Look): { attrs: Record<string, string>; vars: Record<string, string> } {
	const attrs: Record<string, string> = {};
	const vars: Record<string, string> = {};
	if (look.backdrop !== 'none') {
		const b = look.backdrop === 'gradient' ? look.b : look.a;
		attrs.tint = look.backdrop;
		vars['--k-tint-a'] = look.a;
		vars['--k-tint-b'] = b;
		vars['--k-tint-mid'] = look.backdrop === 'gradient' ? midpoint(look.a, b) : look.a;
		vars['--k-tint-angle'] = `${look.angle}deg`;
		vars['--k-tint'] = String(look.strength / 100);
	}
	if (look.accent) {
		attrs.accent = '';
		vars['--k-accent-seed'] = look.accent;
	}
	return { attrs, vars };
}

function apply(): void {
	if (typeof document === 'undefined') return;
	const root = document.documentElement;
	root.dataset.theme = resolvedTheme();
	const { attrs, vars } = paint(theme.look);
	for (const key of ['tint', 'accent']) {
		if (key in attrs) root.dataset[key] = attrs[key];
		else delete root.dataset[key];
	}
	for (const name of VARS) {
		if (name in vars) root.style.setProperty(name, vars[name]);
		else root.style.removeProperty(name);
	}
	if (theme.pureBlack) root.dataset.black = '';
	else delete root.dataset.black;
	if (theme.mdRatings) delete root.dataset.mdRatings;
	else root.dataset.mdRatings = 'off';
	// The browser / OS bar around the page: whatever the page background resolved to.
	const bar = getComputedStyle(root).backgroundColor;
	if (bar) document.querySelector('meta[name="theme-color"]')?.setAttribute('content', bar);
}

function save(): void {
	try {
		localStorage.setItem(MODE_KEY, theme.pref);
		localStorage.setItem(
			LOOK_KEY,
			JSON.stringify({
				v: 2,
				look: theme.look,
				black: theme.pureBlack,
				md: theme.mdRatings,
				...paint(theme.look),
			}),
		);
	} catch {
		/* private mode: the choice lasts until reload only */
	}
}

/** A saved look, checked field by field; anything damaged falls back to the default. */
function readLook(raw: unknown): Look {
	const r = (raw ?? {}) as Partial<Record<keyof Look, unknown>>;
	const num = (x: unknown, lo: number, hi: number, d: number) =>
		typeof x === 'number' && Number.isFinite(x) ? Math.min(hi, Math.max(lo, Math.round(x))) : d;
	return {
		backdrop: r.backdrop === 'solid' || r.backdrop === 'gradient' ? r.backdrop : 'none',
		a: isHex(r.a) ? r.a.toLowerCase() : DEFAULT_LOOK.a,
		b: isHex(r.b) ? r.b.toLowerCase() : DEFAULT_LOOK.b,
		angle: num(r.angle, 0, 359, DEFAULT_LOOK.angle),
		strength: num(r.strength, 0, 100, DEFAULT_LOOK.strength),
		accent: isHex(r.accent) ? r.accent.toLowerCase() : null,
	};
}

/** The look an older version saved: one palette colour that tinted the app and was
 * its accent too. */
function fromV1(saved: { palette?: unknown; custom?: unknown }): Look {
	if (saved.palette === 'custom' && isHex(saved.custom)) return solid(saved.custom.toLowerCase());
	return PRESETS.find((p) => p.id === saved.palette)?.look ?? DEFAULT_LOOK;
}

/** Restore the saved look and start tracking OS changes. Call once on mount. It is
 * called from an effect: untracked, so that writing the look does not re-run it. */
export function initTheme(): void {
	untrack(restore);
	if (typeof matchMedia !== 'undefined') {
		matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
			if (theme.pref === 'system') apply();
		});
	}
}

function restore(): void {
	try {
		const mode = localStorage.getItem(MODE_KEY);
		if (mode === 'light' || mode === 'dark' || mode === 'system') theme.pref = mode;
		const saved = JSON.parse(localStorage.getItem(LOOK_KEY) || '{}') ?? {};
		theme.look = saved.v === 2 ? readLook(saved.look) : { ...fromV1(saved) };
		theme.pureBlack = saved.black === true;
		theme.mdRatings = saved.md !== false;
		if (saved.v !== 2 && saved.palette !== undefined) save();
	} catch {
		/* private mode or a damaged value: keep the defaults */
	}
	apply();
}

export function setTheme(pref: ThemePref): void {
	theme.pref = pref;
	save();
	apply();
}

/** Use a ready-made look. */
export function applyPreset(id: string): void {
	const preset = PRESETS.find((p) => p.id === id);
	if (!preset) return;
	theme.look = { ...preset.look };
	save();
	apply();
}

/** Change part of the look. Colours that are not `#rrggbb` are ignored. */
export function updateLook(patch: Partial<Look>): void {
	const next = { ...theme.look, ...patch };
	if (!isHex(next.a) || !isHex(next.b) || (next.accent !== null && !isHex(next.accent))) return;
	theme.look = readLook(next);
	save();
	apply();
}

export function setPureBlack(on: boolean): void {
	theme.pureBlack = on;
	save();
	apply();
}

/** Show or hide MangaDex's rating next to our own readers' rating, on this device. */
export function setMdRatings(on: boolean): void {
	theme.mdRatings = on;
	save();
	apply();
}

/** Back to the look of a fresh install. What is shown (MangaDex ratings) is not a
 * look and stays as it is. */
export function resetTheme(): void {
	theme.pref = 'system';
	theme.look = { ...DEFAULT_LOOK };
	theme.pureBlack = false;
	save();
	apply();
}

/** Single-button cycle: dark → light → system → dark. */
export function cycleTheme(): void {
	setTheme(theme.pref === 'dark' ? 'light' : theme.pref === 'light' ? 'system' : 'dark');
}
