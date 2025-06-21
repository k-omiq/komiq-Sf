import type { Backend } from '@komika/api';
import { backend } from '$lib/context';

/**
 * SvelteKit hands every `load` its own `fetch`. Requests made with it during edge SSR are
 * serialized into the HTML as `<script data-sveltekit-fetched>`, and the hydration re-run
 * of that same load resolves them from there instead of going back over the network.
 *
 * That is worth threading through the data layer because the reader's public pages
 * server-render at the edge and then re-run their universal load in the browser — so
 * every cold, anonymous visit used to pay for the SAME GraphQL queries twice, the second
 * time from the browser, on the critical path, after the whole JS chunk chain had
 * downloaded. Lighthouse measured the home page's pair landing at 706 ms and 963 ms.
 *
 * WHAT STILL REFETCHES, ON PURPOSE. SvelteKit matches a serialized response by URL plus a
 * hash of the request's headers/body. Edge SSR is always anonymous, so it issues the
 * cacheable `GET /graphql` with no headers; a SIGNED-IN browser sends the same query as an
 * authenticated POST, which matches nothing and goes to the network exactly as before.
 * The personalized re-run the reader depends on (NSFW rows, library marks) is therefore
 * untouched — only the anonymous duplicate disappears.
 */
export type LoadFetch = typeof globalThis.fetch;

/**
 * The backend a `load` should use: bound to SvelteKit's `fetch` when one is supplied,
 * the plain singleton otherwise (client-side navigations, the Tauri build, and any
 * backend that does not implement `withFetch`).
 *
 * The returned view shares the singleton's bearer token — see `Backend.withFetch`.
 */
export function api(fetch?: LoadFetch): Backend {
	if (!fetch) return backend;
	return backend.withFetch?.(guarded(fetch)) ?? backend;
}

/**
 * SvelteKit's server-side `fetch` SIMULATES CORS "for consistency with client-side
 * behaviour": a cross-origin response without a matching `Access-Control-Allow-Origin`
 * makes it throw, even though nothing on the server enforces same-origin policy. Our API
 * answers that header from an allowlist keyed on the request's `Origin`, and SvelteKit's
 * fetch sends none — so the API grew an explicit originless allowance for exactly this
 * caller (`add_originless_cors`, apps/server/src/main.rs).
 *
 * This retries such a failure on the global `fetch`, which has no simulation and no
 * opinion about CORS. It exists for the deploy window: the reader and the API ship
 * separately, and against an API that predates that allowance EVERY public page would
 * otherwise SSR to empty feeds — and the edge would cache the empty page. Losing the
 * response inlining is a performance regression; losing the content is an outage.
 *
 * Narrow on purpose: only SvelteKit's own CORS-simulation error is retried. A real
 * network failure, a 5xx or an abort propagates untouched rather than being sent twice.
 */
function guarded(fetch: LoadFetch): LoadFetch {
	return async (input, init) => {
		try {
			return await fetch(input, init);
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			if (!message.startsWith('CORS error')) throw err;
			console.warn('[komika] SSR fetch CORS simulation failed; retrying unserialized:', message);
			return globalThis.fetch(input, init);
		}
	};
}
