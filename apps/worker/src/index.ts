/**
 * Komika image Worker.
 *
 * The web build can't cross-origin-fetch arbitrary source CDNs (CORS), so this
 * Worker fetches image bytes server-side and re-serves them with permissive
 * CORS + long cache headers. Popular images stay hot in Cloudflare's edge cache
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

const IMMUTABLE_CACHE = 'public, max-age=604800, immutable';
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
		if (upstream.protocol !== 'http:' && upstream.protocol !== 'https:') {
			return errorResponse(400, 'src must be http(s)');
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
			`${url.origin}/img?src=${encodeURIComponent(upstream.toString())}`,
			{ method: 'GET' },
		);
		const edge = caches.default;
		const cached = await edge.match(cacheKey);
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
				console.error(`img proxy: ${err.message} for ${upstream.toString()}`);
				return errorResponse(err.status, err.message);
			}
			console.error(`img proxy: upstream fetch failed for ${upstream.toString()}:`, err);
			return errorResponse(502, 'Upstream fetch failed');
		}
		if (!originResp.ok || !originResp.body) {
			console.error(`img proxy: upstream ${originResp.status} for ${upstream.toString()}`);
			return errorResponse(502, `Upstream ${originResp.status}`);
		}

		// Only ever re-serve actual images. Without this, an upstream `text/html`
		// (or any non-image) body would be laundered under our trusted origin with
		// permissive CORS + a 7-day immutable cache (content laundering / cache
		// poisoning) — see I3. Reject anything that isn't `image/*`.
		const contentType = originResp.headers.get('Content-Type') ?? '';
		if (!isImageContentType(contentType)) {
			console.error(
				`img proxy: non-image Content-Type "${contentType}" for ${upstream.toString()}`,
			);
			return errorResponse(502, 'Upstream is not an image');
		}

		// Fail fast on an oversized image before we stream a single byte. The body
		// is also capped while streaming (finalizeImage) in case this header is
		// absent or lies.
		const declaredLen = originResp.headers.get('Content-Length');
		if (declaredLen && Number(declaredLen) > MAX_IMAGE_BYTES) {
			console.error(
				`img proxy: upstream Content-Length ${declaredLen} exceeds cap for ${upstream.toString()}`,
			);
			return errorResponse(502, 'Upstream image too large');
		}

		const res = finalizeImage(originResp, contentType);
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
		if (next.protocol !== 'http:' && next.protocol !== 'https:') {
			throw new ProxyError(400, 'redirect target must be http(s)');
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
function finalizeImage(source: Response, contentType: string | null): Response {
	const headers = new Headers();
	headers.set('Content-Type', contentType || 'application/octet-stream');
	headers.set('Cache-Control', IMMUTABLE_CACHE);
	// Defense in depth against content-sniffing: never let a browser reinterpret
	// these bytes as anything other than the declared image type, and force an
	// inline disposition so a mislabeled body can't be treated as a download.
	headers.set('X-Content-Type-Options', 'nosniff');
	headers.set('Content-Disposition', 'inline');
	for (const [k, v] of Object.entries(CORS_HEADERS)) headers.set(k, v);
	// Do NOT forward the upstream Content-Length: a stale/incorrect length can truncate
	// or hang the response; let the runtime compute the length for the body we emit.
	//
	// Stream the body NATIVELY when the upstream declared a length within the cap. Piping
	// a large body through a JS TransformStream (`capStream`) runs JS for every chunk, and
	// that per-byte CPU on a multi-MB image can exceed the Worker's CPU limit and kill the
	// response mid-stream. Pass a trustworthy length-framed body through untouched; fall
	// back to the JS byte-cap whenever the length can't be trusted.
	//
	// Trust requires ALL of: a present, finite, POSITIVE length (`Number('')` is 0, a JS
	// footgun, so `> 0` keeps an empty/whitespace header on the capped path); within the
	// cap (re-checked here so this and the pre-fetch gate share one predicate); and NO
	// `Transfer-Encoding` — per RFC 7230 chunked framing wins over `Content-Length`, so a
	// response carrying both is chunked-and-unbounded and must stay capped.
	const declaredLen = source.headers.get('Content-Length');
	const len = Number(declaredLen);
	const trustedLen =
		declaredLen !== null &&
		Number.isFinite(len) &&
		len > 0 &&
		len <= MAX_IMAGE_BYTES &&
		source.headers.get('Transfer-Encoding') === null;
	const raw = source.body as ReadableStream<Uint8Array>;
	const body = trustedLen ? raw : capStream(raw, MAX_IMAGE_BYTES);
	return new Response(body, { status: 200, headers });
}

/**
 * Wrap a body stream so it errors out once it exceeds `max` bytes. Guards the
 * case where the upstream omits (or lies about) Content-Length — mirrors the
 * streamed byte accounting in the native `read_capped`.
 */
function capStream(body: ReadableStream<Uint8Array>, max: number): ReadableStream<Uint8Array> {
	let total = 0;
	return body.pipeThrough(
		new TransformStream<Uint8Array, Uint8Array>({
			transform(chunk, controller) {
				total += chunk.byteLength;
				if (total > max) {
					controller.error(new Error('image body exceeds size cap'));
					return;
				}
				controller.enqueue(chunk);
			},
		}),
	);
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
		headers: { 'Content-Type': 'text/plain; charset=utf-8', ...CORS_HEADERS },
	});
}

/**
 * True when the (possibly parameterized) Content-Type is a raster `image/*`
 * type. SVG is rejected: it is an active document (can carry `<script>` /
 * event handlers) and re-serving it under our permissive-CORS origin is a stored
 * XSS vector, so it must never be laundered through the proxy.
 */
function isImageContentType(contentType: string): boolean {
	const type = contentType.split(';', 1)[0].trim().toLowerCase();
	if (type === 'image/svg+xml' || type === 'image/svg') return false;
	return /^image\//i.test(type);
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
