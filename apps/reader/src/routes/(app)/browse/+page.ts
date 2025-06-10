import type { PageLoad } from './$types';
import { getGenreFacets } from '$lib/data/source';
import { SSR_PUBLIC } from '$lib/ssr';

// Public browse — server-rendered at the edge on the web build. See $lib/ssr.
export const ssr = SSR_PUBLIC;

// Load the genre facet set for the filter multi-select (S4). Row fetching happens
// client-side in the page (reactive to query + filters + `?page=`); `getGenreFacets()`
// never rejects — it resolves to an empty list on error.
//
// DELIBERATELY does NOT destructure `url`. SvelteKit only re-runs a load that touched
// `url` when a search param changes, and the facet set is identical on every page of
// every query — so reading `url` here would refetch the whole genre facet payload on
// each pager click for no benefit. The page number is read in the component, straight
// off `page.url` (see `pageNum` in +page.svelte). Don't "fix" this.
export const load: PageLoad = ({ setHeaders }) => {
	// Facets change rarely; cache them longer than the feed pages. Rows are fetched
	// client-side, so only this facet payload is edge-cached.
	setHeaders({ 'cache-control': 'public, max-age=0, s-maxage=300' });
	return { facets: getGenreFacets() };
};
