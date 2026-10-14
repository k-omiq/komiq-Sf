/**
 * Small colour helpers for the theme: `#rrggbb` <-> HSV for the picker, OKLab for
 * midpoints and lightness steps, and WCAG contrast for picking readable text.
 */
export interface Hsv {
	/** 0-360 */
	h: number;
	/** 0-1 */
	s: number;
	/** 0-1 */
	v: number;
}

const HEX = /^#[0-9a-f]{6}$/i;

export function isHex(value: unknown): value is string {
	return typeof value === 'string' && HEX.test(value);
}

/** Reads what a person types: `#abc`, `abc`, `#aabbcc` or `aabbcc`. Null when it is
 * not a colour. */
export function parseHex(input: string): string | null {
	let s = input.trim().replace(/^#/, '').toLowerCase();
	if (/^[0-9a-f]{3}$/.test(s)) s = [...s].map((c) => c + c).join('');
	return /^[0-9a-f]{6}$/.test(s) ? `#${s}` : null;
}

function toRgb(hex: string): [number, number, number] {
	const n = parseInt(hex.slice(1), 16);
	return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function fromRgb(r: number, g: number, b: number): string {
	const c = (x: number) =>
		Math.round(Math.min(255, Math.max(0, x)))
			.toString(16)
			.padStart(2, '0');
	return `#${c(r)}${c(g)}${c(b)}`;
}

export function hexToHsv(hex: string): Hsv {
	const [r, g, b] = toRgb(hex).map((x) => x / 255);
	const max = Math.max(r, g, b);
	const d = max - Math.min(r, g, b);
	let h = 0;
	if (d) {
		if (max === r) h = ((g - b) / d) % 6;
		else if (max === g) h = (b - r) / d + 2;
		else h = (r - g) / d + 4;
		h = (h * 60 + 360) % 360;
	}
	return { h, s: max ? d / max : 0, v: max };
}

export function hsvToHex({ h, s, v }: Hsv): string {
	const f = (n: number) => {
		const k = (n + h / 60) % 6;
		return (v - v * s * Math.max(0, Math.min(k, 4 - k, 1))) * 255;
	};
	return fromRgb(f(5), f(3), f(1));
}

// sRGB <-> OKLab (Björn Ottosson's matrices).
const lin = (c: number) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const gam = (x: number) => {
	// A colour sRGB cannot show comes back out of 0-1 here: clip it.
	const c = Math.min(1, Math.max(0, x));
	return c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055;
};

function toOklab(hex: string): [number, number, number] {
	const [r, g, b] = toRgb(hex).map((x) => lin(x / 255));
	const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
	const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
	const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
	return [
		0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
		1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
		0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
	];
}

function fromOklab(L: number, a: number, b: number): string {
	const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
	const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
	const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
	return fromRgb(
		gam(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s) * 255,
		gam(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s) * 255,
		gam(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s) * 255,
	);
}

/** The colour halfway between two, in OKLab: what a surface sits on when the page
 * behind it runs from one to the other. */
export function midpoint(a: string, b: string): string {
	const x = toOklab(a);
	const y = toOklab(b);
	return fromOklab((x[0] + y[0]) / 2, (x[1] + y[1]) / 2, (x[2] + y[2]) / 2);
}

/** WCAG relative luminance, 0 (black) to 1 (white). */
export function luminance(hex: string): number {
	const [r, g, b] = toRgb(hex).map((x) => lin(x / 255));
	return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** WCAG contrast ratio between two colours, 1 to 21. */
export function contrast(x: string, y: string): number {
	const a = luminance(x);
	const b = luminance(y);
	return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
}

/** The same colour, lighter (positive) or darker (negative) by `d` OKLab lightness.
 * Hue and chroma stay; what sRGB cannot show is clipped. */
export function shiftL(hex: string, d: number): string {
	const [L, a, b] = toOklab(hex);
	return fromOklab(Math.min(1, Math.max(0, L + d)), a, b);
}

/** A colour from OKLCH (lightness 0-1, chroma, hue in degrees). */
export function oklchToHex(L: number, C: number, H: number): string {
	const h = (H * Math.PI) / 180;
	return fromOklab(L, C * Math.cos(h), C * Math.sin(h));
}

/** OKLCH of a colour: [lightness, chroma, hue in degrees]. */
export function hexToOklch(hex: string): [number, number, number] {
	const [L, a, b] = toOklab(hex);
	return [L, Math.hypot(a, b), ((Math.atan2(b, a) * 180) / Math.PI + 360) % 360];
}

/** `#rrggbb` as a CSS colour with an alpha. */
export function withAlpha(hex: string, alpha: number): string {
	const [r, g, b] = toRgb(hex);
	return `rgb(${r} ${g} ${b} / ${alpha})`;
}
