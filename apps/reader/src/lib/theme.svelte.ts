/**
 * The look of the app, kept on the device (localStorage) and applied on <html>.
 *
 * The reader owns two looks, one for the light theme and one for the dark theme, and
 * picks which one shows (light, dark, or follow the system). Each look is exactly what
 * the reader chose:
 *
 *  - background: the theme's own grey (`none`), one colour, or a two-colour gradient.
 *    The page is that colour; cards and controls are small lightness steps from it.
 *  - accent: the app's own coral (null), or any colour. It colours links, highlights,
 *    the main buttons and the komiq "k" (in the app, the tab icon and the app icon).
 *
 * The one thing the app decides is the text colour: light or dark, whichever reads
 * better on the background. That choice is `data-theme` on <html>, so the design
 * tokens (@komika/ui) supply the matching text, border and overlay colours, while the
 * background and accent tokens are set as inline variables that override theirs.
 *
 * Also on <html>: `data-tint` when the background is not the default (theme.css draws
 * a gradient from it), `data-accent` when the accent is not, and
 * `data-md-ratings="off"` when the reader hid MangaDex's rating
 * ($lib/components/Ratings.svelte owns that rule).
 *
 * A no-FOUC script (static/theme-init.js) paints the saved look before first paint
 * from the `paint` entries saved here; `initTheme()` re-syncs the reactive store on
 * mount and tracks OS changes while the choice is "system". Keep the two in step.
 */
import { untrack } from 'svelte';
import { contrast, hexToOklch, isHex, midpoint, oklchToHex, shiftL, withAlpha } from './color';
import { applyAppIcon } from './app-icon';

export type ThemePref = 'light' | 'dark' | 'system';
export type Mode = 'light' | 'dark';
export type Backdrop = 'none' | 'solid' | 'gradient';

export interface Look {
	backdrop: Backdrop;
	/** The background colour, and where a gradient starts. `#rrggbb`. */
	a: string;
	/** Where a gradient ends. `#rrggbb`. */
	b: string;
	/** Gradient direction in degrees, 0 = towards the top. */
	angle: number;
	/** The accent colour, or null for the app's own. */
	accent: string | null;
}

export interface Preset {
	id: string;
	name: string;
	look: Look;
}

/** The komiq coral: the default accent and the colour of the "k". */
export const KOMIQ_ACCENT = '#e0836a';

/** What the tokens (@komika/ui) ship for each theme: the page colour, and the text and
 * secondary text colours that sit on it. */
const TOKENS = {
	light: { bg: '#f4f2ee', text: '#1a1a1b', faint: '#626269' },
	dark: { bg: '#0c0c0d', text: '#f2f1ee', faint: '#999790' },
} as const;

/** Lightness steps (OKLab) from the page colour to each surface, as the tokens space
 * them: up from a dark page, mostly down from a light one. */
const STEPS: Record<Mode, Record<string, number>> = {
	dark: {
		'--k-surface': 0.014,
		'--k-surface-2': 0.037,
		'--k-surface-3': 0.046,
		'--k-surface-4': 0.063,
		'--k-surface-5': 0.085,
		'--k-avatar': 0.068,
		'--k-cover': 0.037,
	},
	light: {
		'--k-surface': 0.038,
		'--k-surface-2': 0.02,
		'--k-surface-3': 0,
		'--k-surface-4': -0.027,
		'--k-surface-5': -0.052,
		'--k-avatar': -0.043,
		'--k-cover': -0.027,
	},
};

function look(backdrop: Backdrop, a: string, b: string, accent: string | null, angle = 160): Look {
	return { backdrop, a, b, angle, accent };
}
const solid = (bg: string, accent: string | null) => look('solid', bg, bg, accent);
const gradient = (a: string, b: string, accent: string, angle = 160) =>
	look('gradient', a, b, accent, angle);

/** A fresh install: the tokens as they ship. `a` and `b` are where a background
 * starts when the reader first turns one on. */
export const DEFAULT_LOOKS: Record<Mode, Look> = {
	light: look('none', TOKENS.light.bg, '#e6ecf7', null),
	dark: look('none', TOKENS.dark.bg, '#141a2b', null),
};

/** Ready-made looks for each theme, in the order the settings screen shows them. */
export const PRESETS: Record<Mode, readonly Preset[]> = {
	light: [
		{ id: 'default', name: 'komiq', look: DEFAULT_LOOKS.light },
		{ id: 'white', name: 'Paper white', look: solid('#fbfbfa', null) },
		{ id: 'dusk', name: 'Sakura dusk', look: gradient('#fbe6f0', '#e8e4fb', '#c2246b') },
		{ id: 'tidal', name: 'Tidal wave', look: gradient('#e0f0fa', '#ddf3ef', '#1f6fa3') },
		{ id: 'ember', name: 'Ember', look: gradient('#fbeedf', '#fbe3e6', '#b8492f', 200) },
		{ id: 'forest', name: 'Forest', look: gradient('#e0f2ed', '#e6f2de', '#2f7a3a', 135) },
		{ id: 'strawberry', name: 'Strawberry', look: solid('#fbe8ea', '#bd2c45') },
		{ id: 'lavender', name: 'Lavender', look: solid('#eeeafb', '#6a4fc4') },
		{ id: 'sapphire', name: 'Sapphire', look: solid('#e7edfb', '#2a56c9') },
		{ id: 'nord', name: 'Nord', look: solid('#e5e9f0', '#4c6f99') },
		{ id: 'teal', name: 'Teal', look: solid('#e0f2f0', '#13756f') },
		{ id: 'honey', name: 'Honey', look: solid('#faf1dc', '#94650c') },
		{ id: 'mocha', name: 'Mocha', look: solid('#f2ebe4', '#7c5a3d') },
		{ id: 'mono', name: 'Monochrome', look: look('none', TOKENS.light.bg, '#e6ecf7', '#4a4a4a') },
	],
	dark: [
		{ id: 'default', name: 'komiq', look: DEFAULT_LOOKS.dark },
		{ id: 'black', name: 'Pure black', look: solid('#000000', null) },
		{ id: 'dusk', name: 'Midnight dusk', look: gradient('#2a0f27', '#15103a', '#ec4e95') },
		{ id: 'tidal', name: 'Tidal wave', look: gradient('#0a2236', '#08292a', '#43a6e0') },
		{ id: 'ember', name: 'Ember', look: gradient('#2c1a0b', '#2f0f16', '#e8876b', 200) },
		{ id: 'forest', name: 'Forest', look: gradient('#0b2521', '#12240d', '#7fc06d', 135) },
		{ id: 'strawberry', name: 'Strawberry', look: solid('#2a1015', '#ee5f74') },
		{ id: 'lavender', name: 'Lavender', look: solid('#1c1730', '#a891ec') },
		{ id: 'sapphire', name: 'Sapphire', look: solid('#0e1933', '#6f95f2') },
		{ id: 'nord', name: 'Nord', look: solid('#232a36', '#88c0d0') },
		{ id: 'teal', name: 'Teal', look: solid('#0c2422', '#3fc4b9') },
		{ id: 'honey', name: 'Honey', look: solid('#251c0a', '#e2ad45') },
		{ id: 'mocha', name: 'Mocha', look: solid('#231b15', '#c49c78') },
		{ id: 'mono', name: 'Monochrome', look: look('none', TOKENS.dark.bg, '#141a2b', '#b5b5b5') },
	],
};

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
/** `{ v: 3, looks, md, paint: { light, dark } }`. `paint` is what theme-init.js puts on
 * <html> for each theme; `md` is false only when the reader hid MangaDex ratings. Older
 * versions: see `migrate`. */
const LOOK_KEY = 'komiq-look';
const DATA = ['theme', 'tint', 'accent'] as const;
/** Every variable `paint` may set, so `apply` can clear the ones it does not. */
const VARS = [
	'--k-bg',
	...Object.keys(STEPS.dark),
	'--k-glass',
	'--k-glass-strong',
	'--k-bg-a',
	'--k-bg-b',
	'--k-tint-angle',
	'--k-accent',
	'--k-primary',
	'--k-primary-hover',
	'--k-on-primary',
	'--k-on-accent',
	'--k-mark',
];

export const theme = $state<{
	pref: ThemePref;
	looks: Record<Mode, Look>;
	/** Show MangaDex's rating next to our own readers' rating. */
	mdRatings: boolean;
	/** The theme the settings screen is editing, shown while it is open even when it
	 * is not the one in use. */
	preview: Mode | null;
}>({
	pref: 'system',
	looks: { light: { ...DEFAULT_LOOKS.light }, dark: { ...DEFAULT_LOOKS.dark } },
	mdRatings: true,
	preview: null,
});

function systemDark(): boolean {
	return typeof matchMedia !== 'undefined' && matchMedia('(prefers-color-scheme: dark)').matches;
}

/** The theme a preference resolves to right now. */
export function resolvedTheme(pref: ThemePref = theme.pref): Mode {
	return pref === 'system' ? (systemDark() ? 'dark' : 'light') : pref;
}

/** The colours a page with this look actually shows. */
function stops(l: Look, mode: Mode): string[] {
	if (l.backdrop === 'none') return [TOKENS[mode].bg];
	return l.backdrop === 'gradient' ? [l.a, l.b] : [l.a];
}

/** The text set that reads better on the background: 'dark' means the dark theme's
 * tokens (light text), 'light' the light theme's (dark text). */
function inkFor(l: Look, mode: Mode): Mode {
	if (l.backdrop === 'none') return mode;
	const worst = (text: string) => Math.min(...stops(l, mode).map((s) => contrast(text, s)));
	return worst(TOKENS.dark.text) >= worst(TOKENS.light.text) ? 'dark' : 'light';
}

/** How a look reads at a glance, for the small previews on the settings screen: its
 * background as a CSS fill, its text colour and its accent. */
export function swatch(l: Look, mode: Mode): { fill: string; text: string; accent: string } {
	const fill =
		l.backdrop === 'none'
			? TOKENS[mode].bg
			: l.backdrop === 'solid'
				? l.a
				: `linear-gradient(${l.angle}deg, ${l.a}, ${l.b})`;
	return { fill, text: TOKENS[inkFor(l, mode)].text, accent: l.accent ?? KOMIQ_ACCENT };
}

/** How readable a look is: the lowest contrast of secondary text, and of the reader's
 * own accent, against any part of the background. The settings screen warns below 4.5
 * and 3. The app's own accent is the brand colour and is not judged (Infinity). */
export function readability(l: Look, mode: Mode): { text: number; accent: number } {
	const faint = TOKENS[inkFor(l, mode)].faint;
	const bg = stops(l, mode);
	const accent = l.accent;
	return {
		text: Math.min(...bg.map((s) => contrast(faint, s))),
		accent: accent ? Math.min(...bg.map((s) => contrast(accent, s))) : Infinity,
	};
}

export function sameLook(x: Look, y: Look): boolean {
	if (x.backdrop !== y.backdrop || x.accent !== y.accent) return false;
	if (x.backdrop === 'none') return true;
	if (x.a !== y.a) return false;
	return x.backdrop === 'solid' || (x.b === y.b && x.angle === y.angle);
}

/** The preset a theme's look is, if it is one. */
export function activePreset(mode: Mode): string | null {
	return PRESETS[mode].find((p) => sameLook(p.look, theme.looks[mode]))?.id ?? null;
}

interface Paint {
	data: Partial<Record<(typeof DATA)[number], string>>;
	vars: Record<string, string>;
	/** The browser / OS bar colour. */
	bar: string;
}

/** What goes on <html> to show a look. */
function paint(l: Look, mode: Mode): Paint {
	const ink = inkFor(l, mode);
	const data: Paint['data'] = { theme: ink };
	const vars: Record<string, string> = {};
	let bar: string = TOKENS[mode].bg;
	if (l.backdrop !== 'none') {
		const bg = l.backdrop === 'gradient' ? midpoint(l.a, l.b) : l.a;
		data.tint = l.backdrop;
		vars['--k-bg'] = bg;
		for (const [name, d] of Object.entries(STEPS[ink])) vars[name] = shiftL(bg, d);
		vars['--k-glass'] = withAlpha(bg, ink === 'dark' ? 0.78 : 0.82);
		vars['--k-glass-strong'] = withAlpha(bg, ink === 'dark' ? 0.9 : 0.94);
		if (l.backdrop === 'gradient') {
			vars['--k-bg-a'] = l.a;
			vars['--k-bg-b'] = l.b;
			vars['--k-tint-angle'] = `${l.angle}deg`;
		}
		bar = bg;
	}
	if (l.accent) {
		const on =
			contrast('#ffffff', l.accent) >= contrast('#0c0c0d', l.accent) ? '#ffffff' : '#0c0c0d';
		data.accent = '';
		vars['--k-accent'] = l.accent;
		vars['--k-primary'] = l.accent;
		vars['--k-primary-hover'] = shiftL(l.accent, on === '#ffffff' ? -0.06 : 0.06);
		vars['--k-on-primary'] = on;
		vars['--k-on-accent'] = on;
		vars['--k-mark'] = l.accent;
	}
	return { data, vars, bar };
}

function apply(): void {
	if (typeof document === 'undefined') return;
	const root = document.documentElement;
	const mode = theme.preview ?? resolvedTheme();
	const l = theme.looks[mode];
	const p = paint(l, mode);
	for (const key of DATA) {
		const value = p.data[key];
		if (value === undefined) delete root.dataset[key];
		else root.dataset[key] = value;
	}
	for (const name of VARS) {
		if (name in p.vars) root.style.setProperty(name, p.vars[name]);
		else root.style.removeProperty(name);
	}
	if (theme.mdRatings) delete root.dataset.mdRatings;
	else root.dataset.mdRatings = 'off';
	document.querySelector('meta[name="theme-color"]')?.setAttribute('content', p.bar);
	void applyAppIcon(l.accent);
}

function save(): void {
	try {
		localStorage.setItem(MODE_KEY, theme.pref);
		localStorage.setItem(
			LOOK_KEY,
			JSON.stringify({
				v: 3,
				looks: theme.looks,
				md: theme.mdRatings,
				paint: {
					light: paint(theme.looks.light, 'light'),
					dark: paint(theme.looks.dark, 'dark'),
				},
			}),
		);
	} catch {
		/* private mode: the choice lasts until reload only */
	}
}

/** A saved look, checked field by field; anything damaged falls back to the default. */
function readLook(raw: unknown, mode: Mode): Look {
	const r = (raw ?? {}) as Partial<Record<keyof Look, unknown>>;
	const d = DEFAULT_LOOKS[mode];
	return {
		backdrop: r.backdrop === 'solid' || r.backdrop === 'gradient' ? r.backdrop : 'none',
		a: isHex(r.a) ? r.a.toLowerCase() : d.a,
		b: isHex(r.b) ? r.b.toLowerCase() : d.b,
		angle:
			typeof r.angle === 'number' && Number.isFinite(r.angle)
				? Math.min(359, Math.max(0, Math.round(r.angle)))
				: d.angle,
		accent: isHex(r.accent) ? r.accent.toLowerCase() : null,
	};
}

/**
 * The looks an older version saved, as that version showed them.
 *
 * v1 (`{ palette, seed, custom, black }`) and v2 (`{ v: 2, look, black }`) kept one
 * colour set for both themes and showed it as a tint: the hue of the colour at the
 * theme's own lightness, as strong as `strength` said, and an accent held to a band of
 * lightness. Those are worked out here once, per theme, into exact colours. Pure black
 * becomes the dark theme's background.
 */
function migrate(saved: Record<string, unknown>): Record<Mode, Look> {
	let old: {
		backdrop: Backdrop;
		a: string;
		b: string;
		angle: number;
		strength: number;
		accent: string | null;
	};
	const v1 = { backdrop: 'solid' as const, angle: 160, strength: 45 };
	if (saved.v === 2) {
		const r = (saved.look ?? {}) as Record<string, unknown>;
		const l = readLook(r, 'dark');
		const strength = typeof r.strength === 'number' ? Math.min(100, Math.max(0, r.strength)) : 45;
		old = { ...l, strength };
	} else if (saved.palette === 'custom' && isHex(saved.custom)) {
		old = { ...v1, a: saved.custom, b: saved.custom, accent: saved.custom };
	} else if (isHex(saved.seed)) {
		old = { ...v1, a: saved.seed, b: saved.seed, accent: saved.seed };
	} else {
		old = { ...v1, backdrop: 'none', a: '#000000', b: '#000000', accent: null };
	}
	const tint = (hex: string, mode: Mode) => {
		const [, c, h] = hexToOklch(hex);
		const s = old.strength / 100;
		return mode === 'dark' ? oklchToHex(0.155, c * s * 0.5, h) : oklchToHex(0.962, c * s * 0.3, h);
	};
	const accent = (hex: string, mode: Mode) => {
		const [l, c, h] = hexToOklch(hex);
		return mode === 'dark'
			? oklchToHex(Math.min(0.9, Math.max(0.72, l)), Math.min(c, 0.17), h)
			: oklchToHex(Math.min(0.52, Math.max(0.36, l)), Math.min(c, 0.2), h);
	};
	const out = {} as Record<Mode, Look>;
	for (const mode of ['light', 'dark'] as const) {
		const d = DEFAULT_LOOKS[mode];
		out[mode] =
			old.backdrop === 'none'
				? { ...d, accent: old.accent && accent(old.accent, mode) }
				: {
						backdrop: old.backdrop,
						a: tint(old.a, mode),
						b: tint(old.b, mode),
						angle: old.angle,
						accent: old.accent && accent(old.accent, mode),
					};
	}
	if (saved.black === true)
		out.dark = { ...out.dark, backdrop: 'solid', a: '#000000', b: '#000000' };
	return out;
}

/** Restore the saved looks and start tracking OS changes. Call once on mount. It is
 * called from an effect: untracked, so that writing the looks does not re-run it. */
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
		const raw = localStorage.getItem(LOOK_KEY);
		const saved = (JSON.parse(raw || '{}') ?? {}) as Record<string, unknown>;
		if (saved.v === 3) {
			const looks = (saved.looks ?? {}) as Record<string, unknown>;
			theme.looks = { light: readLook(looks.light, 'light'), dark: readLook(looks.dark, 'dark') };
		} else if (raw) {
			theme.looks = migrate(saved);
		}
		theme.mdRatings = saved.md !== false;
		if (raw && saved.v !== 3) save();
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

/** Show a theme's look while the settings screen edits it; null shows the one in use. */
export function setPreview(mode: Mode | null): void {
	theme.preview = mode === resolvedTheme() ? null : mode;
	apply();
}

/** Use a ready-made look for a theme. */
export function applyPreset(mode: Mode, id: string): void {
	const preset = PRESETS[mode].find((p) => p.id === id);
	if (!preset) return;
	theme.looks[mode] = { ...preset.look };
	save();
	apply();
}

/** Change part of a theme's look. Colours that are not `#rrggbb` are ignored. */
export function updateLook(mode: Mode, patch: Partial<Look>): void {
	const next = { ...theme.looks[mode], ...patch };
	if (!isHex(next.a) || !isHex(next.b) || (next.accent !== null && !isHex(next.accent))) return;
	theme.looks[mode] = readLook(next, mode);
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
	theme.looks = { light: { ...DEFAULT_LOOKS.light }, dark: { ...DEFAULT_LOOKS.dark } };
	theme.preview = null;
	save();
	apply();
}

/** Single-button cycle: dark → light → system → dark. */
export function cycleTheme(): void {
	setTheme(theme.pref === 'dark' ? 'light' : theme.pref === 'light' ? 'system' : 'dark');
}
