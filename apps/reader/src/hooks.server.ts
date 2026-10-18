import type { Handle } from '@sveltejs/kit';
import { seriesSearch } from '$lib/series-url';

const SERIES_ROUTE = '/(app)/series/[slug]';

// A series URL with parameters the page does not use (or a different spelling of one
// it does) moves permanently to its canonical form, so junk query strings cannot
// multiply edge cache entries or SSR runs. The browser keeps the redirect for an hour;
// the edge for a minute, like the page, so a parameter a later release starts using
// is not stripped for long.
function canonicalRedirect(url: URL): Response | null {
	const search = seriesSearch(url.searchParams);
	if (search === url.search) return null;
	return new Response(null, {
		status: 301,
		headers: {
			location: url.pathname + search,
			'cache-control': 'public, max-age=3600, s-maxage=60',
		},
	});
}

// Workers Assets headers do not cover dynamically rendered HTML. Apply these
// to SvelteKit responses as well; SvelteKit supplies the bootstrap-safe CSP.
export const handle: Handle = async ({ event, resolve }) => {
	const redirect =
		event.route.id === SERIES_ROUTE &&
		!event.isDataRequest &&
		(event.request.method === 'GET' || event.request.method === 'HEAD')
			? canonicalRedirect(event.url)
			: null;
	const response = redirect ?? (await resolve(event));
	response.headers.set('X-Content-Type-Options', 'nosniff');
	response.headers.set('X-Frame-Options', 'DENY');
	response.headers.set('Referrer-Policy', 'strict-origin-when-cross-origin');
	response.headers.set(
		'Permissions-Policy',
		'camera=(), microphone=(), geolocation=(), payment=(), usb=()',
	);
	return response;
};
