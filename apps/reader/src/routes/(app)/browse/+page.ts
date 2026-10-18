import type { PageLoad } from './$types';
import { getBrowseSources, getBrowseTags } from '$lib/data/source';
import { SSR_PUBLIC } from '$lib/ssr';

// Public browse — server-rendered on the web build. See $lib/ssr.
// The hosted Worker runs only on /series/* (apps/reader/wrangler.toml), so the hosted
// site serves this route as the static shell: no edge cache time is set here.
export const ssr = SSR_PUBLIC;

// Load the tag vocabulary for the filter panel's tag picker (Browse v2). Row fetching
// happens client-side in the page, reactive to the URL — every filter, the sort and the
// page live in the query string (see $lib/data/browse-filters). `getBrowseTags()` never
// rejects: it falls back to the legacy genre facets against an older server, and to an
// empty list on error.
//
// The source list for the panel's Source group rides along the same way.
// `getBrowseSources()` never rejects either: an older server, or an error, is an empty
// list, and the page then shows no Source group.
//
// Rows stay client-side on purpose: they are viewer-dependent (a signed-in NSFW viewer
// gets a different set from the anonymous edge render, and an authed request is a POST
// the edge can't replay), and the page keeps a module-level back-nav cache keyed on the
// full filter signature that an SSR'd grid would have to bypass anyway.
//
// DELIBERATELY does NOT destructure `url`. SvelteKit only re-runs a load that touched
// `url` when a search param changes, and the tag set and the source list are identical
// for every query, filter and page — reading `url` here would refetch both on every
// filter tap and pager click for no benefit. The page reads its state straight off
// `page.url`. Don't "fix" this.
// `fetch` is threaded into the data layer so the SSR response is inlined in the HTML and
// the hydration re-run replays it instead of refetching — see $lib/data/load-fetch.
export const load: PageLoad = ({ fetch }) => {
	// Two promises, neither awaited: the page resolves each on its own, so a slow source
	// list cannot hold back the tags (or the first paint).
	return { tags: getBrowseTags(fetch), sources: getBrowseSources(fetch) };
};
