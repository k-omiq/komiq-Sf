/**
 * Server-side renders ask the API under `/graphql/ssr`: the same endpoint under a second
 * path, which every server has (`GRAPHQL_SSR_PATH` in `apps/server/src/main.rs`). All
 * renders leave from the same Workers address, so the second path lets an edge rule, such
 * as a per-address rate limit on `/graphql`, tell renders from browsers.
 *
 * SvelteKit keeps a load's response in the page under the URL the load asked for, so
 * hydration in the browser still finds it under `/graphql`.
 *
 * A server without the path answers 404, or with its web app's HTML. The render then asks
 * `/graphql`, and goes there directly for {@link MISSING_ALIAS_MS}.
 */
const MISSING_ALIAS_MS = 5 * 60_000;

export type ServerFetch = (request: Request, fetch: typeof globalThis.fetch) => Promise<Response>;

/** A `handleFetch` body that sends the reads of `endpoint` to its `/ssr` path. */
export function ssrGraphqlFetch(endpoint: string): ServerFetch {
	let missingUntil = 0;
	return async (request, fetch) => {
		const url = new URL(request.url);
		let api: URL;
		try {
			api = new URL(endpoint, url);
		} catch {
			return fetch(request);
		}
		if (
			request.method !== 'GET' ||
			url.origin !== api.origin ||
			url.pathname !== api.pathname ||
			Date.now() < missingUntil
		) {
			return fetch(request);
		}
		const alias = new URL(url);
		alias.pathname = `${url.pathname}/ssr`;
		const res = await fetch(new Request(alias, request));
		const html = (res.headers.get('content-type') ?? '').includes('text/html');
		if (res.status !== 404 && !(res.ok && html)) return res;
		missingUntil = Date.now() + MISSING_ALIAS_MS;
		return fetch(request);
	};
}
