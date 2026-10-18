// The query string a series page is cached under.
//
// The hosted Worker caches series SSR per full URL, so every distinct query string is
// its own edge entry and its own render. The page reads exactly two parameters (see
// routes/(app)/series/[slug]/+page.svelte): `order` (only `oldest` differs from the
// default) and `cpage` (a chapter page; 1 is the default). Everything else, and every
// spelling that renders the same page, maps to one canonical form, in the order the
// page's own links write it.

/** The canonical `?…` (or '') for a series page with these parameters. */
export function seriesSearch(params: URLSearchParams): string {
	const canonical = new URLSearchParams();
	if (params.get('order') === 'oldest') canonical.set('order', 'oldest');
	const cpage = Math.floor(Number(params.get('cpage')));
	if (Number.isSafeInteger(cpage) && cpage > 1) canonical.set('cpage', String(cpage));
	const search = canonical.toString();
	return search ? `?${search}` : '';
}
