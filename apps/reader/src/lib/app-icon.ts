/**
 * The komiq icon in the reader's accent colour: the browser tab icon and the
 * home-screen icon a browser saves from now on, and in the desktop app the Dock or
 * taskbar icon (the native `set_app_icon` command). The default accent puts the
 * shipped icons back.
 *
 * The icon is redrawn from one master (`assets/app-icon.png`: a coral "k" with a soft
 * glow on a near-black tile). Every pixel there lies between the tile colour and the
 * coral, so each is redrawn the same share of the way from the tile to the accent.
 * An accent too dark to show on the dark tile gets a light tile instead.
 *
 * An installed phone app keeps the icon its package was built with: iOS and Android
 * only switch between icons shipped inside the app, so no colour reaches them.
 */
import { isTauri } from '@komika/api';
import master from '$lib/assets/app-icon.png';
import { contrast } from './color';

const TILE: [number, number, number] = [14, 14, 15];
const CORAL: [number, number, number] = [238, 121, 85];
const LIGHT_TILE = '#f4f2ee';

let source: Promise<HTMLImageElement> | null = null;
function load(): Promise<HTMLImageElement> {
	source ??= new Promise((resolve, reject) => {
		const img = new Image();
		img.onload = () => resolve(img);
		img.onerror = reject;
		img.src = master;
	});
	return source;
}

function rgb(hex: string): [number, number, number] {
	const n = parseInt(hex.slice(1), 16);
	return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/** The icon at `size` pixels in the accent colour, as a canvas. */
async function draw(accent: string, size: number): Promise<HTMLCanvasElement> {
	const img = await load();
	const canvas = document.createElement('canvas');
	canvas.width = canvas.height = size;
	const ctx = canvas.getContext('2d', { willReadFrequently: true });
	if (!ctx) throw new Error('no 2d canvas');
	ctx.imageSmoothingQuality = 'high';
	ctx.drawImage(img, 0, 0, size, size);
	const data = ctx.getImageData(0, 0, size, size);
	const px = data.data;
	const to = rgb(accent);
	const tile = contrast(accent, '#0e0e0f') >= 2.5 ? TILE : rgb(LIGHT_TILE);
	const span = CORAL.map((c, i) => c - TILE[i]);
	const len = span.reduce((s, x) => s + x * x, 0);
	for (let i = 0; i < px.length; i += 4) {
		if (px[i + 3] === 0) continue;
		let t = 0;
		for (let c = 0; c < 3; c++) t += (px[i + c] - TILE[c]) * span[c];
		t = Math.min(1, Math.max(0, t / len));
		for (let c = 0; c < 3; c++) px[i + c] = Math.round(tile[c] + t * (to[c] - tile[c]));
	}
	ctx.putImageData(data, 0, 0);
	return canvas;
}

/** Point every icon <link> at `href`, or back to what the page shipped with (null). */
function setLinks(selector: string, href: string | null): void {
	for (const link of document.querySelectorAll<HTMLLinkElement>(selector)) {
		link.dataset.shipped ??= link.getAttribute('href') ?? '';
		const next = href ?? link.dataset.shipped;
		if (link.getAttribute('href') !== next) link.setAttribute('href', next);
	}
}

function nativeIcon(png: number[]): void {
	void import('@tauri-apps/api/core')
		.then(({ invoke }) => invoke('set_app_icon', { png }))
		.catch(() => {
			/* a platform without the command, or a failed decode: the shipped icon stays */
		});
}

let timer: ReturnType<typeof setTimeout> | undefined;
let shown: string | null | undefined;

/**
 * Show the icon in `accent` (null: the shipped icon). Calls while a colour is being
 * dragged settle on the last one, a moment after it stops.
 */
export function applyAppIcon(accent: string | null): void {
	if (typeof document === 'undefined' || accent === shown) return;
	clearTimeout(timer);
	timer = setTimeout(() => void paintIcon(accent), shown === undefined ? 0 : 160);
}

async function paintIcon(accent: string | null): Promise<void> {
	const first = shown === undefined;
	shown = accent;
	try {
		if (isTauri()) {
			// The shipped icon is already showing on a fresh start.
			if (accent === null) {
				if (!first) nativeIcon([]);
				return;
			}
			const canvas = await draw(accent, 256);
			const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/png'));
			if (blob && shown === accent) nativeIcon([...new Uint8Array(await blob.arrayBuffer())]);
			return;
		}
		if (accent === null) {
			setLinks('link[rel~="icon"]', null);
			setLinks('link[rel="apple-touch-icon"]', null);
			return;
		}
		const [tab, touch] = await Promise.all([draw(accent, 64), draw(accent, 180)]);
		if (shown !== accent) return;
		setLinks('link[rel~="icon"]', tab.toDataURL('image/png'));
		setLinks('link[rel="apple-touch-icon"]', touch.toDataURL('image/png'));
	} catch {
		/* no canvas or the master did not load: the shipped icon stays */
	}
}
