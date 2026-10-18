import { error } from '@sveltejs/kit';
import type { PageLoad } from './$types';
import { getSeries } from '$lib/data/source';
import { SSR_PUBLIC } from '$lib/ssr';
import { shouldStream } from '$lib/stream';

// Public series detail — server-rendered at the edge on the web build (SEO +
// shareable link previews). Per-viewer bits (library mark, progress) hydrate
// client-side. See $lib/ssr.
export const ssr = SSR_PUBLIC;

// `getSeries()` never rejects — it resolves to `{ view: null, error }` and the page
// renders its not-found / error state.
//
// This route USED to return the promise unawaited unconditionally, with the page
// filling itself from an `$effect`. Effects don't run during SSR, so the edge
// emitted the hero/chapter SKELETON — and then cached that empty shell for the
// whole s-maxage window. Every shared series link previewed as a generic page, the
// content was invisible to crawlers, and the SSR run still paid for the full
// backend fan-out whose result was thrown away and refetched on hydration.
//
// So: await on the server AND on hydration (see $lib/stream — `browser` alone
// can't tell hydration from a client-side navigation), and stream only on later
// client-side navigations, where a skeleton is the right thing to show.
// `fetch` is threaded into the data layer so the SSR responses are inlined in the HTML and
// the hydration re-run replays them instead of refetching — see $lib/data/load-fetch.
//
// The cache header is decided AFTER the data: the edge (the adapter's `caches.default`)
// keeps any response under 400 that is not `no-store` for its s-maxage, so a header set
// up front cached outage pages and unknown ids as normal pages for a minute. Only a
// found series is cacheable. An unknown id is a real 404; an outage keeps this page's
// own retry state. `setHeaders` does nothing in the browser.
export const load: PageLoad = async ({ params, fetch, setHeaders }) => {
	const pending = getSeries(params.slug, fetch);
	// Tells the root layout to skip its generic <title>/og:*/twitter:* defaults —
	// this page emits its own, and two sets in one <head> means a crawler reads
	// whichever it happens to hit first. See routes/+layout.svelte.
	if (shouldStream()) return { slug: params.slug, series: pending, ownsMeta: true };
	const series = await pending;
	setHeaders({ 'cache-control': series.view ? 'public, max-age=0, s-maxage=60' : 'no-store' });
	if (!series.view && !series.error) error(404, 'Series not found');
	return { slug: params.slug, series, ownsMeta: true };
};
