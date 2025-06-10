import type { Page } from '@komika/types';
import { isTauri } from './platform.js';

/**
 * Turns an upstream source image URL into something the UI can put in an
 * <img src>. This is the seam that hides Komika's web/native split from every
 * component: the reader just asks the provider to resolve a page.
 *
 *  - web    → returns a Cloudflare Worker proxy URL (bypasses CORS; the Worker
 *             fetches upstream bytes and edge-caches them).
 *  - native → invokes a Rust command that fetches the bytes directly from the
 *             source CDN and hands back a displayable URL (blob / asset proto).
 */
export interface ImageProvider {
	/** Resolve a page image to a displayable URL. */
	resolvePage(page: Page): Promise<string>;
	/** Resolve a cover / thumbnail URL (same rules as pages). */
	resolveCover(sourceUrl: string): Promise<string>;
	/**
	 * Synchronous cover resolution, present only when the provider can resolve
	 * without I/O (the web provider — a pure URL rewrite). Absent on native, where
	 * a cover becomes an async blob URL. Lets a component render the `<img src>`
	 * during SSR (LCP) instead of waiting for a browser-only async effect.
	 */
	resolveCoverSync?(sourceUrl: string): string;
	/** Release any object URLs created for a page (native blob URLs leak otherwise). */
	release?(url: string): void;
}

export interface WebImageProviderConfig {
	/** Base URL of the Cloudflare Worker image proxy, e.g. https://img.komika.app */
	workerBaseUrl: string;
	/**
	 * When true, return source URLs unchanged instead of proxying them. Use when
	 * the images are already same-origin / CORS-safe (e.g. a Suwayomi server that
	 * proxies source images itself), so no Worker is needed.
	 */
	direct?: boolean;
	/**
	 * Origin of the Komika API (the GraphQL endpoint minus `/graphql`). Covers
	 * cached in the DB are served from this origin under `/covers/...` (and
	 * avatars under `/avatars/...`); those are same-origin / CORS-safe, so they are
	 * returned directly instead of routed through the Worker. Relative `/covers/…`
	 * paths are resolved against it.
	 */
	apiOrigin?: string;
}

/** API-origin asset paths the reader serves itself (never Worker-proxied). */
const OWN_ASSET_PREFIXES = ['/covers/', '/avatars/'];

/**
 * MangaDex host patterns (uploads.mangadex.org + *.mangadex.network). Doubles as
 * the set of hosts that must go through the Worker proxy on web (CORS + hotlink;
 * `direct` breaks them) and the set the native provider fetches via Rust
 * `fetch_image` (which already sets MangaDex's UA/Referer).
 */
const MANGADEX_HOSTS = [/(^|\.)mangadex\.org$/i, /(^|\.)mangadex\.network$/i];

/** Hosts that must go through the Worker proxy (CORS + hotlink); `direct` breaks them. */
const PROXY_REQUIRED_HOSTS = MANGADEX_HOSTS;

/** Web provider: rewrite source URLs to Worker-proxied URLs (or pass through in direct mode). */
export class WebImageProvider implements ImageProvider {
	constructor(private readonly config: WebImageProviderConfig) {}

	/** Warn once if `direct` is paired with a host that needs proxying (I6). */
	private directGuardWarned = false;

	private resolve(sourceUrl: string): string {
		if (!sourceUrl) return '';
		if (this.config.direct) {
			this.warnIfProxyRequired(sourceUrl);
			return sourceUrl;
		}
		const base = this.config.workerBaseUrl.replace(/\/$/, '');
		return `${base}/img?src=${encodeURIComponent(sourceUrl)}`;
	}

	/** True for an asset the reader serves from its own API origin (cached covers,
	 * avatars) — relative `/covers/…` or an absolute URL under `apiOrigin`. */
	private isOwnAsset(url: string): boolean {
		if (OWN_ASSET_PREFIXES.some((p) => url.startsWith(p))) return true;
		const origin = this.config.apiOrigin;
		if (!origin) return false;
		// Match on an ORIGIN BOUNDARY, not a bare prefix: `startsWith(origin)` alone would
		// treat `https://api.komiq.cc.evil.com/…` as own-origin. Require the URL to be the
		// origin exactly or continue with a path separator.
		const trimmed = origin.replace(/\/$/, '');
		return url === trimmed || url.startsWith(`${trimmed}/`);
	}

	/** Resolve an own-origin asset to a loadable absolute URL (prefix relative
	 * paths with `apiOrigin`; already-absolute URLs pass through). */
	private toAbsoluteOwn(url: string): string {
		if (/^https?:\/\//.test(url)) return url;
		const origin = (this.config.apiOrigin ?? '').replace(/\/$/, '');
		return origin + (url.startsWith('/') ? '' : '/') + url;
	}

	private warnIfProxyRequired(sourceUrl: string): void {
		if (this.directGuardWarned) return;
		let host: string;
		try {
			host = new URL(sourceUrl).hostname;
		} catch {
			return;
		}
		if (PROXY_REQUIRED_HOSTS.some((re) => re.test(host))) {
			this.directGuardWarned = true;
			console.warn(
				`[komika] PUBLIC_KOMIKA_IMG_MODE=direct with a host that needs the Worker proxy (${host}). ` +
					'MangaDex covers/pages will fail via CORS/hotlink — use proxy mode for MangaDex-backed reading.',
			);
		}
	}

	async resolvePage(page: Page): Promise<string> {
		return this.resolvePageSync(page.sourceUrl);
	}

	/**
	 * Resolve a page image URL. A page already on our own API origin — a Suwayomi-source
	 * chapter page proxied by `api.komiq.cc` (`/api/v1/manga/.../page/...`) — is served
	 * DIRECTLY, exactly like an own-origin cover, and NOT laundered through the Worker.
	 *
	 * The Worker exists only to bypass CORS/hotlink protection on foreign CDNs (MangaDex);
	 * an `<img src>` needs neither for our own origin, which already sends immutable cache
	 * headers. Routing our own pages back through it is pure overhead AND a real failure
	 * mode: Suwayomi serves RAW full-resolution pages (multi-MB), and the Worker streams
	 * every byte through a JS transform + tees the whole body for its edge cache — enough
	 * per-request CPU on a large page to exceed the Worker's CPU limit and kill the
	 * response mid-stream (the "images partially load then break" report). Foreign hosts
	 * (MangaDex `*.mangadex.network`) still go through the Worker, where the proxy is
	 * actually required.
	 */
	private resolvePageSync(sourceUrl: string): string {
		if (!sourceUrl) return '';
		if (!this.config.direct && this.isOwnAsset(sourceUrl)) {
			return this.toAbsoluteOwn(sourceUrl);
		}
		return this.resolve(sourceUrl);
	}

	/** Synchronous cover resolution (web is pure URL rewriting — no I/O). */
	resolveCoverSync(sourceUrl: string): string {
		if (!sourceUrl) return '';
		// Covers cached on our own origin (the DB cover cache) are same-origin /
		// CORS-safe — serve them directly, never through the Worker. Everything else
		// (uncached MangaDex covers) still goes through the proxy.
		if (!this.config.direct && this.isOwnAsset(sourceUrl)) {
			return this.toAbsoluteOwn(sourceUrl);
		}
		return this.resolve(sourceUrl);
	}

	async resolveCover(sourceUrl: string): Promise<string> {
		return this.resolveCoverSync(sourceUrl);
	}
}

/**
 * Native provider: fetch bytes through the Rust core and expose them as a blob
 * URL. The actual `invoke(…)` calls are wired when the Tauri app is generated;
 * kept dynamic so this package has no hard Tauri dependency and still builds
 * for the web target.
 *
 * Resolution is source-aware, checked in order:
 *  1. Relative engine path (`/api/…`) → `suwayomi_image`, which streams the
 *     bytes from the embedded loopback Suwayomi engine (the extension's
 *     per-source request context is applied inside the engine). This is the
 *     primary native content path — the embedded engine hands back page/cover
 *     image URLs as relative proxy paths, not CDN URLs.
 *  2. Absolute MangaDex host → `fetch_image`, which sets MangaDex's UA/Referer.
 *  3. Everything else (absolute non-engine URLs) → `resolveViaLocalProxy`.
 */
export class NativeImageProvider implements ImageProvider {
	constructor(private readonly config: WebImageProviderConfig = { workerBaseUrl: '' }) {}

	private async fetchBytes(sourceUrl: string): Promise<ArrayBuffer> {
		const { invoke } = await import('@tauri-apps/api/core');
		// Rust `fetch_image` returns the raw image bytes as an ArrayBuffer.
		return invoke<ArrayBuffer>('fetch_image', { url: sourceUrl });
	}

	/**
	 * Fetch bytes for a relative embedded-engine proxy path (`/api/…`). Rust
	 * `suwayomi_image` streams them from the loopback Suwayomi engine and returns
	 * the raw bytes as an ArrayBuffer, mirroring `fetch_image`.
	 */
	private async fetchEngineBytes(path: string): Promise<ArrayBuffer> {
		const { invoke } = await import('@tauri-apps/api/core');
		return invoke<ArrayBuffer>('suwayomi_image', { path });
	}

	private async toBlobUrl(sourceUrl: string): Promise<string> {
		const bytes = await this.fetchBytes(sourceUrl);
		const blob = new Blob([bytes]);
		return URL.createObjectURL(blob);
	}

	/** Resolve a relative embedded-engine path (`/api/…`) to a blob URL via `suwayomi_image`. */
	private async engineToBlobUrl(path: string): Promise<string> {
		const bytes = await this.fetchEngineBytes(path);
		const blob = new Blob([bytes]);
		return URL.createObjectURL(blob);
	}

	/** True for the embedded engine's relative proxy paths (e.g. `/api/v1/manga/…`). */
	private isEnginePath(sourceUrl: string): boolean {
		return sourceUrl.startsWith('/api/');
	}

	/** True when `sourceUrl` is a MangaDex upload/network host; relative/invalid URLs → false. */
	private isMangaDexHost(sourceUrl: string): boolean {
		let host: string;
		try {
			host = new URL(sourceUrl).hostname;
		} catch {
			return false;
		}
		return MANGADEX_HOSTS.some((re) => re.test(host));
	}

	/**
	 * Resolve an absolute non-MangaDex source image (e.g. a hosted-fallback CDN
	 * URL) to a blob URL. Falls back to the generic `fetch_image` command, which
	 * does not carry any per-source request context. Native embedded-engine
	 * content now flows through the relative `/api/…` → `suwayomi_image` path
	 * (see `resolve`) instead of this fallback.
	 */
	private async resolveViaLocalProxy(sourceUrl: string): Promise<string> {
		return this.toBlobUrl(sourceUrl);
	}

	/**
	 * Source-aware byte resolution, in order: relative engine path (`/api/…`) →
	 * `suwayomi_image`; absolute MangaDex host → `fetch_image`; everything else →
	 * local proxy. Empty input → empty string (reader renders a placeholder).
	 */
	private async resolve(sourceUrl: string): Promise<string> {
		if (!sourceUrl) return '';
		if (this.isEnginePath(sourceUrl)) return this.engineToBlobUrl(sourceUrl);
		// Covers/avatars cached on the hosted API origin: fetch the absolute URL
		// (relative `/covers/…` resolved against apiOrigin) via the generic path.
		if (OWN_ASSET_PREFIXES.some((p) => sourceUrl.startsWith(p))) {
			const origin = (this.config.apiOrigin ?? '').replace(/\/$/, '');
			return this.toBlobUrl(origin + sourceUrl);
		}
		return this.isMangaDexHost(sourceUrl)
			? this.toBlobUrl(sourceUrl)
			: this.resolveViaLocalProxy(sourceUrl);
	}

	async resolvePage(page: Page): Promise<string> {
		return this.resolve(page.sourceUrl);
	}

	async resolveCover(sourceUrl: string): Promise<string> {
		return this.resolve(sourceUrl);
	}

	release(url: string): void {
		if (url.startsWith('blob:')) URL.revokeObjectURL(url);
	}
}

export function createImageProvider(config: WebImageProviderConfig): ImageProvider {
	return isTauri() ? new NativeImageProvider(config) : new WebImageProvider(config);
}
