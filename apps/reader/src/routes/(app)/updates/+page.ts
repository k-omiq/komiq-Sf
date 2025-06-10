import type { PageLoad } from './$types';
import { getUpdates } from '$lib/data/source';
import { pageParam } from '$lib/data/paging';
import { SSR_PUBLIC } from '$lib/ssr';
import { shouldStream } from '$lib/stream';
import type { ComicType } from '$lib/data/types';

// Public updates feed — server-rendered at the edge on the web build. See $lib/ssr.
export const ssr = SSR_PUBLIC;

/** The three formats the screen's chips offer, and the only values `?type=` accepts. */
const TYPES: ComicType[] = ['Manga', 'Manhwa', 'Manhua'];

/** `?type=` → a format, or null for "All". An unknown value is "All" rather than an
 *  error: the param is user-editable and shareable, and an empty grid with a chip
 *  highlighted that the screen doesn't have is worse than ignoring it. */
function parseType(raw: string | null): ComicType | null {
	return raw && TYPES.includes(raw as ComicType) ? (raw as ComicType) : null;
}

// `getUpdates()` never rejects — it resolves to empty feeds on error.
//
// On the SERVER we AWAIT, for the same reason the home route does (see
// ../+page.ts): Svelte's SSR only ever renders the pending branch of `{#await}`,
// so a streamed promise leaves the edge HTML as permanent skeletons. This route
// streamed unconditionally and therefore server-rendered ZERO content — 12,574 B
// with 1 <img> and 55 skeleton nodes, versus the home route's 20,998 B with 12
// real cards. Worse, the s-maxage=30 edge cache was faithfully caching that empty
// shell, so every viewer got skeletons and then paid for the client fetch anyway.
//
// HYDRATION also awaits. Universal loads re-run in the browser on hydration, so
// `browser` alone can't tell it from a client navigation: the old test streamed on
// hydration, replacing the freshly server-rendered grid with skeletons and fetching
// the feed a second time. `shouldStream()` distinguishes them — see $lib/stream.
//
// On later CLIENT-SIDE navigations we return the promise UNawaited so the page
// shows skeletons via `{#await}`. The page renders both shapes — see +page.svelte.
//
// `url` IS DESTRUCTURED ON PURPOSE. SvelteKit only re-runs a universal load when
// something it actually read changed, so a load that never touches `url` is NOT re-run
// on a `?page=` / `?tab=` / `?type=` navigation — the pager would update the address bar
// and nothing else. Reading `url.searchParams` here is what registers the dependency.
// Consequence worth knowing: `s-maxage=30` now keys on the query string, so `?page=2` is
// its own edge-cache entry. That is correct (different content, different key) and
// bounded in practice — crawlers aside, real viewers see the first few pages.
export const load: PageLoad = async ({ url, setHeaders }) => {
	// Shortest TTL of the public pages — this is the "what's new" surface.
	setHeaders({ 'cache-control': 'public, max-age=0, s-maxage=30' });
	const sp = url.searchParams;
	// The facets live in the URL, not in component state, because a page NUMBER is
	// meaningless without them: "page 3" of which list, filtered how? Reload, share and
	// Back must all reproduce the exact grid.
	const tab = sp.get('tab') === 'hot' ? 'hot' : 'new';
	const type = parseType(sp.get('type'));
	// "Hot" is a curated top-N with no pagination in the stack, so it never carries a
	// page (the screen also strips `page` from the URL when switching to it).
	const page = tab === 'hot' ? 1 : pageParam(sp);
	const updates = getUpdates({ page, tab, type });
	return { page, tab, type, updates: shouldStream() ? updates : await updates };
};
