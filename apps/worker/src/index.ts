/**
 * Komika image Worker.
 *
 * The web build can't cross-origin-fetch arbitrary source CDNs (CORS), so this
 * Worker fetches image bytes server-side and re-serves them with permissive
 * CORS + bounded cache headers. Popular images stay hot in Cloudflare's edge cache
 * (caches.default); there is no object-storage tier — the Worker is a pure
 * proxy backed by the edge cache.
 *
 * URL scheme (must match packages/api/src/image-provider.ts):
 *
 *   GET /img?src=<url-encoded upstream image URL>
 *
 *   - src : absolute http(s) URL of the upstream image (required). The host must
 *           be in ALLOWED_SOURCE_HOSTS; an empty allowlist denies all (fail closed).
 *
 * Caching layers, in order:
 *   1. Cloudflare edge cache (caches.default) — always on.
 *   2. Upstream fetch — origin of last resort; the response is cached at the edge.
 */

interface Env {
	ALLOWED_SOURCE_HOSTS?: string;
	ALLOWED_ORIGINS?: string;
	// Cloudflare native per-IP rate limiter for the upstream-fetch path (bandwidth
	// abuse cap). Configured as an [[unsafe.bindings]] ratelimit in wrangler.toml,
	// so the limit/period live in config — no dashboard step.
	IMG_RATE_LIMITER: RateLimit;
}

const IMAGE_CACHE = 'public, max-age=300, must-revalidate';
// Image size ceiling, mirroring the native reference (`MAX_IMAGE_BYTES`,
// src-tauri/src/lib.rs). Enforced both from the upstream Content-Length (fail
// fast) and while streaming (in case the length is absent or lies).
const MAX_IMAGE_BYTES = 32 * 1024 * 1024; // 32 MiB
// Redirect hops we follow manually so the host allowlist is re-checked on every
// hop (matches native `MAX_REDIRECTS`); a 3xx must never smuggle us to a
// non-allowlisted host.
const MAX_REDIRECTS = 4;

/** Error carrying the HTTP status the proxy should surface to its caller. */
class ProxyError extends Error {
	constructor(
		readonly status: number,
		message: string,
	) {
		super(message);
	}
}
const CORS_HEADERS: Record<string, string> = {
	'Access-Control-Allow-Origin': '*',
	'Access-Control-Allow-Methods': 'GET, HEAD, OPTIONS',
	'Access-Control-Allow-Headers': '*',
	'Access-Control-Max-Age': '86400',
};

export default {
	async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
		// CORS preflight.
		if (request.method === 'OPTIONS') {
			return new Response(null, { status: 204, headers: CORS_HEADERS });
		}

		if (request.method !== 'GET' && request.method !== 'HEAD') {
			return errorResponse(405, 'Method Not Allowed');
		}

		const url = new URL(request.url);
		if (url.pathname !== '/img') {
			// Trivial health check / anything else.
			if (url.pathname === '/' || url.pathname === '/healthz') {
				return new Response('ok', { status: 200, headers: CORS_HEADERS });
			}
			return errorResponse(404, 'Not Found');
		}

		// --- Validate `src`. -----------------------------------------------------
		const src = url.searchParams.get('src');
		if (!src) return errorResponse(400, 'Missing ?src');

		let upstream: URL;
		try {
			upstream = new URL(src);
		} catch {
			return errorResponse(400, 'Invalid ?src URL');
		}
		if (!safeUpstreamUrl(upstream)) {
			return errorResponse(400, 'src must be http(s) without credentials or nonstandard ports');
		}
		if (!hostAllowed(upstream.hostname, env.ALLOWED_SOURCE_HOSTS)) {
			return errorResponse(400, 'src host not allowed');
		}

		// --- Hotlink protection. -------------------------------------------------
		if (!originAllowed(request, env.ALLOWED_ORIGINS)) {
			return errorResponse(403, 'Forbidden origin');
		}

		// --- 1. Edge cache. ------------------------------------------------------
		// Key on the normalized upstream URL so /img?src=X and any hotlink query
		// noise share one edge entry.
		const cacheKey = new Request(
			`${url.origin}/img?src=${encodeURIComponent(upstream.toString())}&validated=1`,
			{ method: 'GET' },
		);
		const edge = caches.default;
		// An explicit reader recovery bypasses a stale image, still subject to the
		// normal miss rate limit and host validation. Refresh the shared cache on success.
		const cached = url.searchParams.get('retry') === '1' ? undefined : await edge.match(cacheKey);
		if (cached) return withMethod(cached, request.method);

		// --- Rate limit (per client IP) — only reached on a cache MISS. ----------
		// Hot images served from the edge cache above are never throttled; this caps
		// the bandwidth-abuse path that actually hits the upstream CDN. Keyed on the
		// client IP (CF-Connecting-IP); requests without one share the "unknown"
		// bucket. Kept BEFORE the upstream fetch so throttled requests never hit it.
		const clientIp = request.headers.get('CF-Connecting-IP') ?? 'unknown';
		const { success } = await env.IMG_RATE_LIMITER.limit({ key: clientIp });
		if (!success) return errorResponse(429, 'Too Many Requests');

		// --- 2. Upstream fetch. --------------------------------------------------
		// Follow redirects MANUALLY (`redirect: 'manual'`), re-running the host
		// allowlist on every hop. `redirect: 'follow'` would let a 3xx from an
		// allowlisted host bounce us to an arbitrary internal/attacker host,
		// bypassing the guard (SSRF / open proxy / cache poisoning). Mirrors the
		// native reference `fetch_image` (redirect::Policy::none + per-hop revalidation).
		let originResp: Response;
		try {
			originResp = await fetchFollowingRedirects(upstream, env.ALLOWED_SOURCE_HOSTS);
		} catch (err) {
			if (err instanceof ProxyError) {
				console.error(`img proxy: ${err.message} for ${upstream.origin}`);
				return errorResponse(err.status, err.message);
			}
			console.error(`img proxy: upstream fetch failed for ${upstream.origin}`);
			return errorResponse(502, 'Upstream fetch failed');
		}
		if (!originResp.ok || !originResp.body) {
			console.error(`img proxy: upstream ${originResp.status} for ${upstream.origin}`);
			return errorResponse(502, `Upstream ${originResp.status}`);
		}

		// Only ever re-serve actual images. Without this, an upstream `text/html`
		// (or any non-image) body would be laundered under our trusted origin with
		// permissive CORS + a shared cache (content laundering / cache
		// poisoning) — see I3. Accept only recognized raster media types.
		const contentType = originResp.headers.get('Content-Type') ?? '';
		if (!isImageContentType(contentType)) {
			console.error(`img proxy: non-image Content-Type "${contentType}" for ${upstream.origin}`);
			return errorResponse(502, 'Upstream is not an image');
		}

		// Fail fast on an oversized image before we stream a single byte. The body
		// is also capped while streaming (finalizeImage) in case this header is
		// absent or lies.
		const declaredLen = originResp.headers.get('Content-Length');
		if (declaredLen && Number(declaredLen) > MAX_IMAGE_BYTES) {
			console.error(
				`img proxy: upstream Content-Length ${declaredLen} exceeds cap for ${upstream.origin}`,
			);
			return errorResponse(502, 'Upstream image too large');
		}

		let bytes: Uint8Array;
		try {
			bytes = await readBoundedImage(originResp);
		} catch {
			return errorResponse(502, 'Upstream image is incomplete or too large');
		}
		if (!validateImageBody(bytes, contentType)) {
			return errorResponse(502, 'Upstream image is invalid or incomplete');
		}
		const res = finalizeImage(bytes, contentType);
		ctx.waitUntil(edge.put(cacheKey, res.clone()));
		return withMethod(res, request.method);
	},
} satisfies ExportedHandler<Env>;

// --- helpers ---------------------------------------------------------------

/**
 * Fetch `start`, following up to `MAX_REDIRECTS` redirects MANUALLY and
 * re-validating every hop's resolved host against the allowlist. Relative
 * `Location`s are resolved against the current URL. Any hop to a
 * non-allowlisted host (or non-http(s) scheme) rejects — this is what keeps a
 * 3xx from turning the proxy into an SSRF/open-proxy primitive.
 */
async function fetchFollowingRedirects(
	start: URL,
	allowRaw: string | undefined,
): Promise<Response> {
	let current = start;
	for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
		const resp = await fetch(current.toString(), {
			headers: {
				// Some CDNs require a Referer / UA to serve images; be polite.
				Referer: `${current.protocol}//${current.host}/`,
				'User-Agent': 'KomikaImageWorker/1.0 (+https://komika.app)',
				Accept: 'image/*,*/*;q=0.8',
			},
			redirect: 'manual',
			signal: AbortSignal.timeout(15_000),
		});

		if (!isRedirectStatus(resp.status)) return resp;

		const location = resp.headers.get('Location');
		if (!location) throw new ProxyError(502, 'Upstream redirect without a Location header');

		let next: URL;
		try {
			next = new URL(location, current); // resolves relative Locations
		} catch {
			throw new ProxyError(502, 'Upstream redirect to an invalid Location');
		}
		if (!safeUpstreamUrl(next)) {
			throw new ProxyError(
				400,
				'redirect target must be http(s) without credentials or nonstandard ports',
			);
		}
		if (!hostAllowed(next.hostname, allowRaw)) {
			throw new ProxyError(400, 'redirect host not allowed');
		}
		current = next;
	}
	throw new ProxyError(502, 'Too many redirects');
}

/** 3xx statuses that carry a Location we should follow. */
function isRedirectStatus(status: number): boolean {
	return status === 301 || status === 302 || status === 303 || status === 307 || status === 308;
}

/** Build the response we serve + cache: image bytes, CORS, long cache. */
function finalizeImage(bytes: Uint8Array, contentType: string): Response {
	const headers = new Headers();
	headers.set('Content-Type', contentType || 'application/octet-stream');
	headers.set('Cache-Control', IMAGE_CACHE);
	// Defense in depth against content-sniffing: never let a browser reinterpret
	// these bytes as anything other than the declared image type, and force an
	// inline disposition so a mislabeled body can't be treated as a download.
	headers.set('X-Content-Type-Options', 'nosniff');
	headers.set('Content-Disposition', 'inline');
	for (const [k, v] of Object.entries(CORS_HEADERS)) headers.set(k, v);
	return new Response(bytes, { status: 200, headers });
}

/** Read under the byte cap even when Content-Length is present. No response or
 * cache entry is published until container completeness has been checked. */
async function readBoundedImage(source: Response): Promise<Uint8Array> {
	const reader = source.body!.getReader();
	const chunks: Uint8Array[] = [];
	let length = 0;
	try {
		while (true) {
			const { done, value } = await reader.read();
			if (done) break;
			length += value.byteLength;
			if (length > MAX_IMAGE_BYTES) throw new Error('image body exceeds size cap');
			chunks.push(value);
		}
	} catch (error) {
		await reader.cancel().catch(() => {});
		throw error;
	} finally {
		reader.releaseLock();
	}
	const declared = source.headers.get('Content-Length');
	if (
		declared !== null &&
		source.headers.get('Transfer-Encoding') === null &&
		!source.headers.has('Content-Encoding') &&
		Number(declared) !== length
	) {
		throw new Error('image length mismatch');
	}
	const bytes = new Uint8Array(length);
	let offset = 0;
	for (const chunk of chunks) {
		bytes.set(chunk, offset);
		offset += chunk.byteLength;
	}
	return bytes;
}

/** Raster container validation, without image decoding or transformations. */
export function validateImageBody(bytes: Uint8Array, contentType: string): boolean {
	const type = contentType.split(';', 1)[0].trim().toLowerCase();
	const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
	const tag = (at: number, text: string) =>
		at + text.length <= bytes.length &&
		[...text].every((char, i) => bytes[at + i] === char.charCodeAt(0));
	try {
		if (type === 'image/jpeg') {
			if (bytes.length < 4 || bytes[0] !== 0xff || bytes[1] !== 0xd8 || bytes[2] !== 0xff)
				return false;
			let cursor = 2,
				frame = false,
				scan = false;
			while (cursor < bytes.length) {
				if (bytes[cursor++] !== 0xff) return false;
				while (bytes[cursor] === 0xff) cursor++;
				const marker = bytes[cursor++];
				if (marker === 0xd9) return frame && scan;
				if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd7)) continue;
				const length = view.getUint16(cursor);
				if (length < 2 || cursor + length > bytes.length) return false;
				if (
					[0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(
						marker,
					)
				) {
					if (length < 8 || view.getUint16(cursor + 3) === 0 || view.getUint16(cursor + 5) === 0)
						return false;
					frame = true;
				}
				cursor += length;
				if (marker === 0xda) {
					scan = true;
					while (cursor < bytes.length) {
						if (bytes[cursor] !== 0xff) {
							cursor++;
							continue;
						}
						if (
							bytes[cursor + 1] === 0 ||
							(bytes[cursor + 1] >= 0xd0 && bytes[cursor + 1] <= 0xd7)
						) {
							cursor += 2;
							continue;
						}
						break;
					}
				}
			}
			return false;
		}
		if (type === 'image/png') {
			if (![137, 80, 78, 71, 13, 10, 26, 10].every((n, i) => bytes[i] === n)) return false;
			let cursor = 8,
				data = false;
			while (cursor + 12 <= bytes.length) {
				const length = view.getUint32(cursor);
				if (cursor + length + 12 > bytes.length) return false;
				if (
					cursor === 8 &&
					(!tag(cursor + 4, 'IHDR') ||
						length !== 13 ||
						view.getUint32(cursor + 8) === 0 ||
						view.getUint32(cursor + 12) === 0)
				)
					return false;
				if (tag(cursor + 4, 'IDAT')) data = true;
				if (tag(cursor + 4, 'IEND')) return length === 0 && data;
				cursor += length + 12;
			}
			return false;
		}
		if (type === 'image/webp') {
			if (!tag(0, 'RIFF') || !tag(8, 'WEBP')) return false;
			const end = view.getUint32(4, true) + 8;
			if (end > bytes.length) return false;
			let cursor = 12,
				data = false;
			while (cursor + 8 <= end) {
				const length = view.getUint32(cursor + 4, true);
				data ||= ['VP8 ', 'VP8L', 'ANMF'].some((kind) => tag(cursor, kind));
				cursor += 8 + length + (length & 1);
				if (cursor > end) return false;
			}
			return cursor === end && data;
		}
		if (type === 'image/gif') {
			if (
				(!tag(0, 'GIF87a') && !tag(0, 'GIF89a')) ||
				bytes.length < 13 ||
				view.getUint16(6, true) === 0 ||
				view.getUint16(8, true) === 0
			)
				return false;
			const table = (packed: number) => (packed & 128 ? 3 * (2 << (packed & 7)) : 0);
			let cursor = 13 + table(bytes[10]),
				image = false;
			while (cursor < bytes.length) {
				const kind = bytes[cursor++];
				if (kind === 0x3b) return image;
				if (kind === 0x21) cursor++;
				else if (kind === 0x2c) {
					if (cursor + 9 > bytes.length) return false;
					cursor += 9 + table(bytes[cursor + 8]) + 1;
					image = true;
				} else return false;
				while (cursor < bytes.length) {
					const length = bytes[cursor++];
					cursor += length;
					if (length === 0) break;
				}
				if (cursor > bytes.length) return false;
			}
			return false;
		}
		if (type === 'image/avif') {
			if (!tag(4, 'ftyp')) return false;
			const first = view.getUint32(0);
			if (first < 16 || first > bytes.length) return false;
			let brand = tag(8, 'avif') || tag(8, 'avis');
			for (let at = 16; at + 4 <= first; at += 4) brand ||= tag(at, 'avif') || tag(at, 'avis');
			if (!brand) return false;
			let cursor = 0,
				meta = false;
			while (cursor + 8 <= bytes.length) {
				let size = view.getUint32(cursor),
					header = 8;
				if (size === 1) {
					size = Number(view.getBigUint64(cursor + 8));
					header = 16;
				}
				if (size === 0) size = bytes.length - cursor;
				if (!Number.isSafeInteger(size) || size < header || cursor + size > bytes.length)
					return false;
				meta ||= tag(cursor + 4, 'meta');
				cursor += size;
			}
			return cursor === bytes.length && meta;
		}
		if (type === 'image/bmp') {
			if (!tag(0, 'BM') || bytes.length < 26) return false;
			const length = view.getUint32(2, true),
				pixels = view.getUint32(10, true);
			return length <= bytes.length && pixels >= 14 && pixels < length;
		}
		if (type === 'image/x-icon' || type === 'image/vnd.microsoft.icon') {
			if (bytes.length < 6 || view.getUint16(0, true) !== 0 || view.getUint16(2, true) !== 1)
				return false;
			const count = view.getUint16(4, true),
				end = 6 + count * 16;
			if (count === 0 || end > bytes.length) return false;
			for (let entry = 6; entry < end; entry += 16) {
				const length = view.getUint32(entry + 8, true),
					offset = view.getUint32(entry + 12, true);
				if (length === 0 || offset < end || offset + length > bytes.length) return false;
			}
			return true;
		}
	} catch {
		return false;
	}
	return false;
}

/** HEAD requests get headers only. */
function withMethod(res: Response, method: string): Response {
	if (method === 'HEAD') {
		return new Response(null, { status: res.status, headers: res.headers });
	}
	return res;
}

function errorResponse(status: number, message: string): Response {
	return new Response(message, {
		status,
		headers: {
			'Content-Type': 'text/plain; charset=utf-8',
			'Cache-Control': 'no-store',
			...CORS_HEADERS,
		},
	});
}

/**
 * True when the (possibly parameterized) Content-Type is a raster `image/*`
 * type. SVG is rejected: it is an active document (can carry `<script>` /
 * event handlers) and re-serving it under our permissive-CORS origin is a stored
 * XSS vector, so it must never be laundered through the proxy.
 */
export function isImageContentType(contentType: string): boolean {
	const type = contentType.split(';', 1)[0].trim().toLowerCase();
	return [
		'image/jpeg',
		'image/png',
		'image/webp',
		'image/gif',
		'image/avif',
		'image/bmp',
		'image/x-icon',
		'image/vnd.microsoft.icon',
	].includes(type);
}

/** Restrict source URLs to the public web service of an allowed host. */
export function safeUpstreamUrl(url: URL): boolean {
	return (
		(url.protocol === 'http:' || url.protocol === 'https:') &&
		url.username === '' &&
		url.password === '' &&
		url.port === ''
	);
}

/** Parse a comma-separated allowlist var into a trimmed, non-empty list. */
function parseList(raw: string | undefined): string[] {
	if (!raw) return [];
	return raw
		.split(',')
		.map((s) => s.trim())
		.filter(Boolean);
}

/**
 * Empty allowlist => deny all (fail closed) — an unconfigured proxy must never be
 * an open proxy (I1/CR5). Otherwise match host exactly or as a suffix.
 */
function hostAllowed(hostname: string, raw: string | undefined): boolean {
	const list = parseList(raw);
	if (list.length === 0) return false;
	const host = hostname.toLowerCase();
	return list.some((allowed) => {
		const a = allowed.toLowerCase();
		return host === a || host.endsWith(`.${a}`);
	});
}

/**
 * Hotlink protection. Requests with no Origin and no Referer (native apps,
 * curl, cache-fill jobs, direct hits) are always allowed. Browser requests are
 * checked against the allowlist; an empty allowlist disables the check.
 *
 * SECURITY: an empty `ALLOWED_ORIGINS` disables hotlink protection entirely and
 * is intended ONLY for local dev. In production `ALLOWED_ORIGINS` MUST be set to
 * a non-empty value (see wrangler.toml) so browser traffic from other origins is
 * rejected. This alone is not an abuse control — the host allowlist
 * (`ALLOWED_SOURCE_HOSTS`) is the primary open-proxy guard. Per-IP bandwidth
 * abuse is capped separately by the `IMG_RATE_LIMITER` binding (see the fetch
 * handler and wrangler.toml).
 */
function originAllowed(request: Request, raw: string | undefined): boolean {
	const list = parseList(raw);
	if (list.length === 0) return true;

	const origin = request.headers.get('Origin');
	const referer = request.headers.get('Referer');
	if (!origin && !referer) return true; // non-browser client

	const candidate = origin ?? originOf(referer);
	if (!candidate) return true;
	return list.includes(candidate);
}

function originOf(referer: string | null): string | null {
	if (!referer) return null;
	try {
		return new URL(referer).origin;
	} catch {
		return null;
	}
}
