import type { Page } from '@komika/types';
import { ENGINE_URL_PREFIX } from './content-backend.js';
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
	/**
	 * `srcset` for a cover rendered at CARD size (~150 CSS px), or `''` when this
	 * source cannot be resized and the caller should just use `resolveCoverSync`.
	 *
	 * Cover masters measure 240–364 px wide, so a 1x viewer downloads roughly four
	 * times the pixels a card can show. Our own origin serves allowlisted `?w=`
	 * derivatives, so for those URLs this hands back a three-candidate set: the narrow
	 * derivative at 1x, a mid derivative at 2x, and only then the master at 3x — which
	 * is the widest cover we hold, so there is nothing sharper to offer above it.
	 *
	 * The 2x rung is the one that matters in the field. Phones run at DPR 1.75–3, so
	 * WITHOUT it every phone fell through to the master: Lighthouse measured a 360x510
	 * master painted into a 263x373 device-pixel box on every card of the home page.
	 *
	 * Descriptors are `x`, not `w`, on purpose: `w` needs the true intrinsic width of
	 * every candidate, which varies per cover and is not known client-side, whereas
	 * `x` is exactly right for a fixed-size box. So this is ONLY valid for card-sized
	 * renderings — a hero or a poster must keep using the master.
	 */
	coverCardSrcSet?(sourceUrl: string): string;
	/** Release any object URLs created for a page (native blob URLs leak otherwise). */
	release?(url: string): void;
}

/**
 * Width (px) of the card-sized cover derivative at 1x. Must stay in the server's
 * `cover::VARIANT_WIDTHS` allowlist — a value that is not on it is silently served as
 * the full-size master, so a drift here costs bytes with nothing to notice it by
 * (`the_card_variant_width_is_actually_servable` guards the other side).
 */
export const CARD_COVER_WIDTH = 192;

/**
 * Width (px) of the card-sized cover derivative at 2x. A 150 CSS px card needs 300
 * device px at DPR 2 and 263 at DPR 1.75, and the widest auto-fill grid column reaches
 * ~200 CSS px; 320 covers that band and stays under every master. Mirrors the server's
 * `cover::CARD_VARIANT_WIDTH_2X` and carries the same allowlist obligation.
 */
export const CARD_COVER_WIDTH_2X = 320;

/** Own-origin cover paths the server can serve at an alternate `?w=` width. */
const RESIZABLE_COVER_PATHS = [/^\/covers\/[^/]+\.webp$/, /^\/api\/v1\/manga\/\d+\/thumbnail$/];

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
	 * Proxy through the Komika SERVER's own `/img` route instead of the Worker: only
	 * absolute URLs on hosts that need a proxy (MangaDex) are rewritten to
	 * `${workerBaseUrl}/img?src=…`; everything else — the server's own covers, avatars
	 * and Suwayomi pages, relative or absolute — loads directly. Used by the embedded
	 * build (`workerBaseUrl` = the page's origin) and `PUBLIC_KOMIKA_IMG_MODE=server`
	 * (`workerBaseUrl` = the API origin). Off by default, so hosted builds are unchanged.
	 */
	serverProxy?: boolean;
	/**
	 * Origin of the Komika API (the GraphQL endpoint minus `/graphql`). Covers
	 * cached in the DB are served from this origin under `/covers/...` (and
	 * avatars under `/avatars/...`); those are same-origin / CORS-safe, so they are
	 * returned directly instead of routed through the Worker. Relative `/covers/…`
	 * paths are resolved against it.
	 */
	apiOrigin?: string;
	/**
	 * Native only: the komiq server the person chose on the reader's connect screen.
	 * Its images (relative `/api/…`, `/covers/…`, `/avatars/…` paths and absolute URLs
	 * on its origin) load straight from it in the WebView instead of through the Rust
	 * fetch, whose address guard rightly refuses a LAN host it was not built for. Unset
	 * keeps the built-in routing below.
	 */
	serverOrigin?: string | null;
}

/** API-origin asset paths the reader serves itself (never Worker-proxied). */
const OWN_ASSET_PREFIXES = ['/covers/', '/avatars/'];

/** One-time rollout key: bypass browser responses cached under the former immutable policy. */
const CACHE_POLICY_REVISION = '2';

/**
 * The address scheme the native app answers engine images on (`ENGINE_IMAGE_SCHEME` in
 * `apps/reader/src-tauri/src/lib.rs`; the two must stay the same).
 */
const ENGINE_IMAGE_SCHEME = 'engine-image';

/**
 * MangaDex host patterns (uploads.mangadex.org + *.mangadex.network). Doubles as
 * the set of hosts that must go through the Worker proxy on web (CORS + hotlink;
 * `direct` breaks them) and the set the native provider fetches via Rust
 * `fetch_image` (which already sets MangaDex's UA/Referer).
 */
const MANGADEX_HOSTS = [/(^|\.)mangadex\.org$/i, /(^|\.)mangadex\.network$/i];

/** Hosts that must go through the Worker proxy (CORS + hotlink); `direct` breaks them. */
const PROXY_REQUIRED_HOSTS = MANGADEX_HOSTS;

/** True for an absolute http(s) URL on a host that must be proxied (MangaDex). */
function needsProxy(sourceUrl: string): boolean {
	let url: URL;
	try {
		url = new URL(sourceUrl);
	} catch {
		return false; // relative: the serving origin's own path
	}
	return (
		(url.protocol === 'https:' || url.protocol === 'http:') &&
		PROXY_REQUIRED_HOSTS.some((re) => re.test(url.hostname))
	);
}

/** Web provider: rewrite source URLs to Worker-proxied URLs (or pass through in direct mode). */
export class WebImageProvider implements ImageProvider {
	constructor(private readonly config: WebImageProviderConfig) {}

	/** Warn once if `direct` is paired with a host that needs proxying (I6). */
	private directGuardWarned = false;

	private resolve(sourceUrl: string): string {
		if (!sourceUrl) return '';
		if (this.config.serverProxy) {
			if (!needsProxy(sourceUrl)) return sourceUrl;
			const base = this.config.workerBaseUrl.replace(/\/$/, '');
			return `${base}/img?src=${encodeURIComponent(sourceUrl)}&cache_policy=${CACHE_POLICY_REVISION}`;
		}
		if (this.config.direct) {
			this.warnIfProxyRequired(sourceUrl);
			return sourceUrl;
		}
		const base = this.config.workerBaseUrl.replace(/\/$/, '');
		return `${base}/img?src=${encodeURIComponent(sourceUrl)}&cache_policy=${CACHE_POLICY_REVISION}`;
	}

	/** True for an asset the reader serves from its own API origin (cached covers,
	 * avatars) — relative `/covers/…` or an absolute URL under `apiOrigin`. */
	private isOwnAsset(url: string): boolean {
		if (OWN_ASSET_PREFIXES.some((p) => url.startsWith(p)) || url.startsWith('/api/v1/manga/'))
			return true;
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

	private versionedOwnAsset(sourceUrl: string): string {
		const absolute = this.toAbsoluteOwn(sourceUrl);
		// Embedded SSR can resolve its own assets before location exists. Keep those
		// paths relative; the placeholder base is used only to edit the query safely.
		const url = new URL(absolute, 'https://komika.invalid');
		url.searchParams.set('cache_policy', CACHE_POLICY_REVISION);
		return /^https?:\/\//.test(absolute)
			? url.toString()
			: `${url.pathname}${url.search}${url.hash}`;
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
		if (this.isOwnAsset(sourceUrl)) {
			return this.versionedOwnAsset(sourceUrl);
		}
		return this.resolve(sourceUrl);
	}

	/** Synchronous cover resolution (web is pure URL rewriting — no I/O). */
	resolveCoverSync(sourceUrl: string): string {
		if (!sourceUrl) return '';
		// Covers cached on our own origin (the DB cover cache) are same-origin /
		// CORS-safe — serve them directly, never through the Worker. Everything else
		// (uncached MangaDex covers) still goes through the proxy.
		if (this.isOwnAsset(sourceUrl)) {
			return this.versionedOwnAsset(sourceUrl);
		}
		return this.resolve(sourceUrl);
	}

	async resolveCover(sourceUrl: string): Promise<string> {
		return this.resolveCoverSync(sourceUrl);
	}

	/**
	 * Three-candidate `srcset` for a card-sized cover. Empty (caller falls back to the
	 * plain master) whenever the resolved URL is not one our own origin can resize:
	 * `direct` mode, a Worker-proxied foreign CDN, or any path outside
	 * `RESIZABLE_COVER_PATHS`. The `?w=` is APPENDED to whatever query the URL already
	 * carries so a cover's `?v=` cache-bust survives.
	 */
	coverCardSrcSet(sourceUrl: string): string {
		// A matching path on a foreign/signed source is not our resize service.
		if (!this.isOwnAsset(sourceUrl)) return '';
		const master = this.resolveCoverSync(sourceUrl);
		if (!master) return '';
		let url: URL;
		try {
			// A relative `/covers/…` never reaches here — resolveCoverSync has already
			// made own-origin assets absolute — but parse defensively rather than throw
			// inside a render.
			url = new URL(master);
		} catch {
			return '';
		}
		if (!RESIZABLE_COVER_PATHS.some((re) => re.test(url.pathname))) return '';
		const at = (w: number): string => {
			const variant = new URL(url.toString());
			variant.searchParams.set('w', String(w));
			return variant.toString();
		};
		// The master stays the 3x candidate rather than another derivative: at 240–364 px
		// wide it is already narrower than a 3x card wants, so a `?w=` above it would just
		// hand back the master's own bytes under a second URL.
		return `${at(CARD_COVER_WIDTH)} 1x, ${at(CARD_COVER_WIDTH_2X)} 2x, ${master} 3x`;
	}
}

/**
 * Native provider: fetch bytes through the Rust core and expose them as a blob
 * URL. The actual `invoke(…)` calls are wired when the Tauri app is generated;
 * kept dynamic so this package has no hard Tauri dependency and still builds
 * for the web target.
 *
 * Resolution is source-aware, checked in order:
 *  0. Engine image (`engine:/api/…`, tagged by the on-device backend) → an
 *     `engine-image` address that the app's Rust side answers from the embedded
 *     loopback Suwayomi engine (the extension's per-source request context is applied
 *     inside the engine). It is a plain address, not bytes: the web view loads each
 *     `<img>` by itself, so a chapter shows its first page without waiting for the last.
 *  1. With `serverOrigin` set (a server chosen on the connect screen), anything on
 *     that server loads directly from it.
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

	private async toBlobUrl(sourceUrl: string): Promise<string> {
		const bytes = await this.fetchBytes(sourceUrl);
		const blob = new Blob([bytes]);
		return URL.createObjectURL(blob);
	}

	/**
	 * The address prefix engine images load from, asked for once. The Android app answers
	 * with its loopback image listener (`engine_image_base` in the reader's `lib.rs` says
	 * why). Every other system uses the `engine-image` scheme, and Tauri knows the shape a
	 * custom scheme has there (`engine-image://localhost` or `http://engine-image.localhost`).
	 */
	private engineBase: Promise<string> | undefined;

	/** The loadable address of a relative embedded-engine path (`/api/…`). */
	private async engineUrl(path: string): Promise<string> {
		this.engineBase ??= (async () => {
			const { invoke, convertFileSrc } = await import('@tauri-apps/api/core');
			const base = await invoke<string | null>('engine_image_base').catch(() => null);
			return base ?? convertFileSrc('', ENGINE_IMAGE_SCHEME).replace(/\/$/, '');
		})();
		return (await this.engineBase) + path;
	}

	/** True for an image the on-device engine serves (`engine:/api/v1/manga/…`). */
	private isEnginePath(sourceUrl: string): boolean {
		return sourceUrl.startsWith(ENGINE_URL_PREFIX);
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
	 * content now flows through the tagged `engine:/api/…` → `engine-image` address
	 * (see `resolve`) instead of this fallback.
	 */
	private async resolveViaLocalProxy(sourceUrl: string): Promise<string> {
		return this.toBlobUrl(sourceUrl);
	}

	/**
	 * Source-aware resolution, in order: tagged engine path (`engine:/api/…`) → an
	 * address the web view loads by itself (`engineUrl`); absolute MangaDex host → `fetch_image`; everything else →
	 * local proxy. Empty input → empty string (reader renders a placeholder).
	 */
	/** `sourceUrl` as a loadable URL on the chosen server, or null when it is not one. */
	private onChosenServer(sourceUrl: string): string | null {
		const origin = this.config.serverOrigin?.replace(/\/$/, '');
		if (!origin) return null;
		if (sourceUrl.startsWith('/') && !sourceUrl.startsWith('//')) return origin + sourceUrl;
		// Origin boundary, not a bare prefix (see WebImageProvider.isOwnAsset).
		return sourceUrl === origin || sourceUrl.startsWith(`${origin}/`) ? sourceUrl : null;
	}

	private async resolve(sourceUrl: string): Promise<string> {
		if (!sourceUrl) return '';
		// Before the chosen-server check: an engine path is relative too, and must not be
		// resolved against the server.
		if (this.isEnginePath(sourceUrl)) {
			return this.engineUrl(sourceUrl.slice(ENGINE_URL_PREFIX.length));
		}
		const direct = this.onChosenServer(sourceUrl);
		if (direct) return direct;
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
