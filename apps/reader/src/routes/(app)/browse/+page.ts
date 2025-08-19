import type { PageLoad } from './$types';
import { getBrowseTags } from '$lib/data/source';
import { SSR_PUBLIC } from '$lib/ssr';

// Public browse — server-rendered at the edge on the web build. See $lib/ssr.
export const ssr = SSR_PUBLIC;

// Load the tag vocabulary for the filter panel's tag picker (Browse v2). Row fetching
// happens client-side in the page, reactive to the URL — every filter, the sort and the
// page live in the query string (see $lib/data/browse-filters). `getBrowseTags()` never
// rejects: it falls back to the legacy genre facets against an older server, and to an
// empty list on error.
//
// Rows stay client-side on purpose: they are viewer-dependent (a signed-in NSFW viewer
// gets a different set from the anonymous edge render, and an authed request is a POST
// the edge can't replay), and the page keeps a module-level back-nav cache keyed on the
// full filter signature that an SSR'd grid would have to bypass anyway.
//
// DELIBERATELY does NOT destructure `url`. SvelteKit only re-runs a load that touched
// `url` when a search param changes, and the tag set is identical for every query,
// filter and page — reading `url` here would refetch the whole vocabulary on every
// filter tap and pager click for no benefit. The page reads its state straight off
// `page.url`. Don't "fix" this.
// `fetch` is threaded into the data layer so the SSR response is inlined in the HTML and
// the hydration re-run replays it instead of refetching — see $lib/data/load-fetch.
export const load: PageLoad = ({ fetch, setHeaders }) => {
	// The vocabulary changes rarely; cache it longer than the feed pages. Rows are fetched
	// client-side, so only this payload is edge-cached.
	setHeaders({ 'cache-control': 'public, max-age=0, s-maxage=300' });
	return { tags: getBrowseTags(fetch) };
};
