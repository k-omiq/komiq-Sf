/**
 * The chapter reader's display choices: one object kept on the device (localStorage).
 * The reader (routes/read) restores it on open and saves on every change; the settings
 * screen edits the same object, so a choice made in one shows in the other.
 *
 * The option lists are also the cycle order of the reader's menu buttons and the
 * source of each button's label + icon.
 */
import type { IconName } from '$lib/components/Icon.svelte';

export type Mode = 'strip' | 'page' | 'double';
export type Fit = 'both' | 'width' | 'height' | 'none';
export type Dir = 'ltr' | 'rtl';
export type HeaderMode = 'auto' | 'shown' | 'hidden';
export type Bar = 'segments' | 'thin' | 'hidden';
export type Opt<T> = { value: T; label: string; icon: IconName };

export const MODES: Opt<Mode>[] = [
	{ value: 'strip', label: 'Long strip', icon: 'list' },
	{ value: 'page', label: 'Single page', icon: 'file' },
	{ value: 'double', label: 'Double page', icon: 'columns' },
];
export const FITS: Opt<Fit>[] = [
	{ value: 'both', label: 'Fit to screen', icon: 'maximize' },
	{ value: 'width', label: 'Fit width', icon: 'arrow-left-right' },
	{ value: 'height', label: 'Fit height', icon: 'chevrons-down' },
	{ value: 'none', label: 'Original size', icon: 'image' },
];
export const DIRS: Opt<Dir>[] = [
	{ value: 'ltr', label: 'Left to right', icon: 'arrow-right' },
	{ value: 'rtl', label: 'Right to left', icon: 'chevron-left' },
];
export const HEADERS: Opt<HeaderMode>[] = [
	{ value: 'auto', label: 'Header auto-hides', icon: 'panel-top' },
	{ value: 'shown', label: 'Header always shown', icon: 'panel-top' },
	{ value: 'hidden', label: 'Header hidden', icon: 'x' },
];
export const BARS: Opt<Bar>[] = [
	{ value: 'segments', label: 'Page bar', icon: 'progress' },
	{ value: 'thin', label: 'Thin progress line', icon: 'progress' },
	{ value: 'hidden', label: 'Progress hidden', icon: 'x' },
];

/** Long-strip page width, in px: the range of the reader's slider. */
export const WIDTH_MIN = 480;
export const WIDTH_MAX = 1400;
export const WIDTH_STEP = 20;

export interface ReaderPrefs {
	mode: Mode;
	fit: Fit;
	dir: Dir;
	header: HeaderMode;
	bar: Bar;
	/** Long-strip page width, in px. */
	width: number;
	/** Space between long-strip pages. */
	gaps: boolean;
	/** The reader's menu stays docked open (wide screens). */
	pinned: boolean;
}

export const DEFAULT_READER_PREFS: ReaderPrefs = {
	mode: 'strip',
	fit: 'both',
	dir: 'ltr',
	header: 'auto',
	bar: 'segments',
	width: 720,
	gaps: false,
	pinned: false,
};

const KEY = 'komiq-reader';

function pick<T>(list: Opt<T>[], v: unknown, fallback: T): T {
	return list.some((o) => o.value === v) ? (v as T) : fallback;
}

/** The saved choices, with the default for anything missing or no longer valid. */
export function loadReaderPrefs(): ReaderPrefs {
	const d = DEFAULT_READER_PREFS;
	try {
		const saved = JSON.parse(localStorage.getItem(KEY) || '{}') ?? {};
		return {
			mode: pick(MODES, saved.mode, d.mode),
			fit: pick(FITS, saved.fit, d.fit),
			dir: pick(DIRS, saved.dir, d.dir),
			header: pick(HEADERS, saved.header, d.header),
			bar: pick(BARS, saved.bar, d.bar),
			width: typeof saved.width === 'number' ? saved.width : d.width,
			gaps: saved.gaps === true,
			pinned: saved.pinned === true,
		};
	} catch {
		return { ...d };
	}
}

export function saveReaderPrefs(prefs: ReaderPrefs): void {
	try {
		localStorage.setItem(KEY, JSON.stringify(prefs));
	} catch {
		/* private mode: the choices last until the reader closes */
	}
}
