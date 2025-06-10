<script module lang="ts">
	import type { FederatedResultView } from '$lib/data/source';

	type BrowseCacheEntry = {
		rows: FederatedResultView[];
		// There is deliberately no page number here: the page is part of the cache KEY
		// (see the rows effect's `sig`) and it lives in the URL, which SvelteKit restores
		// on a back-nav. So "Back must return to the page I was on" costs the cache
		// nothing — an entry only has to restore the rows and scroll FOR that page.
		hasNext: boolean;
		totalCount: number | null;
		rowsAreFederated: boolean;
		scrollY: number;
	};
	// Back-nav restore (B1): keep the last few browse result sets in MODULE memory —
	// it survives a component remount (series → back) but not a full reload — so
	// returning to Browse rehydrates instantly with scroll intact instead of
	// re-running the whole search. Keyed by a query+filters+PAGE signature; small FIFO
	// cap so it can't grow unbounded across a long session.
	const browseCache = new Map<string, BrowseCacheEntry>();
	// Raised from 8 when the page number joined the key: at 8, paging one query eight
	// deep evicted every other query's entry, so "page a bit, open a series, go back,
	// page again" always missed. 12 × ≤30 view objects is trivial memory.
	const BROWSE_CACHE_MAX = 12;
	function putBrowseCache(sig: string, entry: BrowseCacheEntry) {
		browseCache.delete(sig); // re-insert so it becomes most-recent
		browseCache.set(sig, entry);
		while (browseCache.size > BROWSE_CACHE_MAX) {
			const oldest = browseCache.keys().next().value as string;
			browseCache.delete(oldest);
		}
	}
</script>

<script lang="ts">
	import { page } from '$app/state';
	import { beforeNavigate, goto } from '$app/navigation';
	import { tick } from 'svelte';
	import Icon from '$lib/components/Icon.svelte';
	import MangaCard from '$lib/components/MangaCard.svelte';
	import CardGridSkeleton from '$lib/components/CardGridSkeleton.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import { FLAG, STATUS_META, type ComicType, type Status } from '$lib/data/types';
	import { BROWSE_PAGE_SIZE, lastPage, pageParam, withPage } from '$lib/data/paging';
	import { getFederatedSearch, getNativeSearch, type CatalogFilters } from '$lib/data/source';
	import type { BrowseSort, ContentRatingFilter } from '@komika/api';
	import { auth } from '$lib/auth.svelte';

	let { data } = $props();

	// Genre facets (S4): the full genre set across the persisted catalogue, most
	// common first, driving the genre multi-select. `data.facets` never rejects.
	let facets = $state<{ genre: string; count: number }[]>([]);
	$effect(() => {
		data.facets.then((f) => (facets = f));
	});

	// The rail's vocabulary, and the whitelist every URL param is validated against —
	// declared BEFORE the state it initialises, because that initialisation runs at
	// component-setup time and a `const` referenced above its declaration is a TDZ throw,
	// not a hoist.
	const TYPES: ComicType[] = ['Manga', 'Manhwa', 'Manhua'];
	const VALID_STATUS = ['ongoing', 'completed', 'hiatus', 'cancelled'];
	const VALID_SORT = ['trending', 'rating', 'newest', 'chapters'];
	/**
	 * The content-rating tiers this rail offers, a deliberate SUBSET of the server's
	 * `ContentRatingFilter`. The intermediate cumulative tiers (SUGGESTIVE, EROTICA,
	 * PORNOGRAPHIC) are omitted: they are ceilings, not categories, so "Erotica" would
	 * read as "erotica only" while actually returning everything up to and including it.
	 * The three below are the ones whose plain-English label matches what comes back.
	 */
	const CONTENT_RATINGS = ['ALL', 'SAFE', 'NSFW_ONLY'] as const;
	// `Extract`, not a bare `(typeof CONTENT_RATINGS)[number]`: it asserts the subset
	// relationship against the wire enum, so a typo'd tier above collapses this to `never`
	// and fails the build here instead of shipping a value the server rejects at runtime.
	type RailContentRating = Extract<ContentRatingFilter, (typeof CONTENT_RATINGS)[number]>;

	const params = page.url.searchParams;
	let query = $state(params.get('q') ?? '');
	// `type` and `genre` are REPEATABLE params — the rail multi-selects both, so a single
	// `get()` would silently drop every choice after the first on reload/share.
	let types = $state<ComicType[]>(params.getAll('type').filter(isViewType));
	let selectedGenres = $state<string[]>(params.getAll('genre'));
	let status = $state<Status | 'any'>('any');
	// Rating is a dual-handle range on the 0–10 scale (10 = no upper bound).
	let minRating = $state(0);
	let maxRating = $state(10);

	// TEMPORARILY DISABLED — re-enable once MangaDex statistics are ingested into
	// `work_stats` (see docs/plans/2026-07-23-architecture-decisions.md, AD-6).
	//
	// The server filters rating on AVG(score) FROM reviews — LOCAL user reviews —
	// and the entire database contains 3 of them. Measured against the live API:
	// no filter → 10,574 results; minRating 0.5 (one notch) → 3. The max handle is
	// inert because unrated series COALESCE to 0. So the control silently deleted
	// 99.97% of the catalogue on first touch, which is strictly worse than having
	// no control at all. Hiding it is the honest interim state; the facet returns
	// on real data (rating_bayesian), it is not being dropped.
	const RATING_FILTER_ENABLED = false;
	let sort = $state<'trending' | 'rating' | 'newest' | 'chapters'>('trending');
	// The wire enum is what the SERVER orders by; the lowercase word is what the URL has
	// always carried and what the chips key off, so shared `?sort=` links keep working.
	// One map, one direction — the reverse never needs to exist.
	const SORT_TO_WIRE: Record<typeof sort, BrowseSort> = {
		trending: 'TRENDING',
		rating: 'RATING',
		newest: 'NEWEST',
		chapters: 'CHAPTERS',
	};
	let contentRating = $state<RailContentRating>('ALL');
	/**
	 * "Has chapters" — the one filter that is OFF by default and still worth having.
	 *
	 * Browse pages the whole catalogue, which includes ~67k works with no chapter yet.
	 * They are not the obscure tail: MangaDex removes chapters when a series is licensed or
	 * claimed, so the set is full of things people search for (Boku no Hero Academia,
	 * Nausicaä, Ghost in the Shell), and many already have another source lined up. Showing
	 * them is the default; this chip is for the reader who only wants what they can open
	 * right now. Sent as `undefined` when off — `false` is a real wire value meaning "only
	 * series with NO chapters", which no control here offers.
	 */
	let hasChapters = $state(false);
	let genreQuery = $state(''); // filters the (long) facet list

	function isViewType(t: string): t is ComicType {
		return TYPES.includes(t as ComicType);
	}

	/**
	 * The signature of the FILTER params in a URL — the gate on the URL→state sync below,
	 * and what {@link syncFilterUrl} pre-arms so a write of our own doesn't read back.
	 *
	 * `page` is deliberately excluded: the pager writes `?page=` and nothing else, and
	 * without that exclusion every page step re-hydrated the whole filter block from the
	 * URL (and reset `federatedFor`). `q` IS included even though the search box never
	 * writes it, because the search overlay and home links do.
	 */
	function filterUrlSig(sp: URLSearchParams): string {
		return JSON.stringify([
			sp.get('q') ?? '',
			sp.getAll('type'),
			sp.getAll('genre'),
			sp.get('status') ?? '',
			sp.get('sort') ?? '',
			sp.get('content') ?? '',
			sp.get('chapters') ?? '',
			sp.get('minRating') ?? '',
		]);
	}

	// Re-sync inputs/filters from the URL on same-route client navigation (home genre
	// links, the search overlay's advanced filters, Back/Forward) — and NOT on our own
	// writes: the rail now publishes itself to the URL (see `syncFilterUrl`), so this
	// effect would otherwise re-hydrate every field on every chip click. That is not
	// merely wasteful, it is lossy: `query` and `genreQuery` live only in component state
	// while a filter write is in flight, so a round-trip through here would wipe a typed
	// search term and clear the federated opt-in on a mere sort change. `syncFilterUrl`
	// pre-arms `lastUrlSig` with the signature it is about to produce, which turns this
	// effect into a no-op for self-writes while leaving genuine navigations untouched.
	let lastUrlSig: string | null = null;
	$effect(() => {
		const sp = page.url.searchParams;
		const urlSig = filterUrlSig(sp);
		// Starts null, so the first run still hydrates — mount behaviour is unchanged.
		if (urlSig === lastUrlSig) return;
		lastUrlSig = urlSig;
		query = sp.get('q') ?? '';
		// A navigation (new search term, home genre link) always restarts on the fast
		// local path; the viewer re-opts into the federated fan-out per query.
		federatedFor = null;
		types = sp.getAll('type').filter(isViewType);
		selectedGenres = sp.getAll('genre');
		const st = sp.get('status');
		status = st && VALID_STATUS.includes(st) ? (st as Status) : 'any';
		const so = sp.get('sort');
		sort = so && VALID_SORT.includes(so) ? (so as typeof sort) : 'trending';
		// Written lowercase for a readable URL, matched case-insensitively so a
		// hand-typed `?content=SAFE` still lands. Anything else — including a tier this
		// rail deliberately doesn't offer — falls back to ALL rather than applying a
		// filter with no control to clear it from.
		const cr = (sp.get('content') ?? '').toUpperCase();
		contentRating = (CONTENT_RATINGS as readonly string[]).includes(cr)
			? (cr as RailContentRating)
			: 'ALL';
		// Written as the bare presence `?chapters=1`, so anything else (including a
		// hand-typed `?chapters=0`) reads as OFF — the default, and the wider result set.
		hasChapters = sp.get('chapters') === '1';
		// Only hydrate the rating from the URL while the facet is enabled. With it
		// disabled the control is hidden, so a stale/shared `?minRating=` link must not
		// still narrow the (signed-in) federated results or render a phantom "N.N
		// rating" pill the user can't clear from the hidden slider. See
		// RATING_FILTER_ENABLED.
		if (RATING_FILTER_ENABLED) {
			const mr = sp.get('minRating');
			const mrn = mr ? parseFloat(mr) : NaN;
			minRating = Number.isNaN(mrn) ? 0 : Math.min(10, Math.max(0, mrn));
		}
	});

	/**
	 * Publish the rail's server-side filters into the URL, and drop `?page=`.
	 *
	 * Every one of these is now a SERVER argument, so a filter change is a different
	 * result set of a different length — page 7 of the old one is meaningless. The reset
	 * happens HERE, in the same navigation that writes the filters, rather than in a
	 * second `replacePage(1)`: two `goto`s racing off one state change both compute their
	 * href from the pre-navigation `page.url`, so whichever landed second would erase the
	 * other's params and the URL→state sync would then hydrate the loser back out of the
	 * rail.
	 *
	 * Unknown params are carried through untouched (notably `q` and `minRating`) — this
	 * owns the filter params, not the query string.
	 */
	function syncFilterUrl() {
		const sp = new URLSearchParams(page.url.searchParams);
		setAll(sp, 'type', types);
		setAll(sp, 'genre', selectedGenres);
		// Defaults are written as ABSENCE, not as `?status=any&sort=trending`: `/browse`
		// is edge-SSR'd and shareable, so the unfiltered grid must have exactly one URL
		// or it gets two cache keys and two crawlable addresses for identical content.
		setOne(sp, 'status', status === 'any' ? null : status);
		setOne(sp, 'sort', sort === 'trending' ? null : sort);
		setOne(sp, 'content', contentRating === 'ALL' ? null : contentRating.toLowerCase());
		setOne(sp, 'chapters', hasChapters ? '1' : null);
		sp.delete('page');
		const qs = sp.toString();
		const href = qs ? `${page.url.pathname}?${qs}` : page.url.pathname;
		if (href === `${page.url.pathname}${page.url.search}`) return;
		lastUrlSig = filterUrlSig(sp); // this write is ours; don't read it back
		void goto(href, { replaceState: true, noScroll: true, keepFocus: true });
	}

	/** Replace a repeatable param with `values` (empty → remove it entirely). */
	function setAll(sp: URLSearchParams, key: string, values: string[]) {
		sp.delete(key);
		for (const v of values) sp.append(key, v);
	}
	/** Set a single-valued param, or remove it when `value` is null. */
	function setOne(sp: URLSearchParams, key: string, value: string | null) {
		if (value == null) sp.delete(key);
		else sp.set(key, value);
	}

	function toggle<T>(arr: T[], v: T): T[] {
		return arr.includes(v) ? arr.filter((x) => x !== v) : [...arr, v];
	}

	// The facet list filtered by the genre search box (case-insensitive), always
	// keeping already-selected genres visible.
	const shownFacets = $derived.by(() => {
		const q = genreQuery.trim().toLowerCase();
		if (!q) return facets;
		return facets.filter(
			(f) => f.genre.toLowerCase().includes(q) || selectedGenres.includes(f.genre),
		);
	});

	// ---- rows: server-filtered native / client-filtered federated -------------
	// Signed-in viewers get FEDERATED search across every installed extension
	// (deduped works tagged with translators); genre/rating filtering there is
	// client-side (searchAllSources takes no filter args) so a filter change does
	// NOT re-fan-out (protects the rate limit). Everything else — an empty query
	// (whole-catalogue browse) or a signed-out text search — goes to the public
	// NATIVE search with genre/rating applied SERVER-side, re-fetching on change.
	let rows = $state<FederatedResultView[]>([]);
	let rowsLoading = $state(true);
	let rowsError = $state(false);
	let rowsAreFederated = $state(false);
	let searchNotice = $state<string | null>(null);
	let reloadKey = $state(0);
	const queryActive = $derived(query.trim().length > 0);

	// Text search hits the local catalogue (FTS over the canonical `work` table,
	// AD-5) by DEFAULT — fast, deterministic, and returning `w_` works that carry the
	// source picker. The federated "search every installed extension" fan-out is slow
	// (up to 24 live source calls) and runs ONLY when the signed-in viewer asks for it
	// explicitly, to discover titles not yet in the catalogue.
	//
	// The opt-in is stored as THE QUERY IT WAS GRANTED FOR, not as a boolean. As a
	// boolean it was only ever cleared by the URL-sync effect above, whose sole
	// dependency is `page.url` — and typing in the search box never touches the URL.
	// So one click on "Search external sources" made every subsequent keystroke-batch
	// fan out to 24 sources until the next navigation: the server rate limit tripped,
	// and the rateLimited branch deliberately KEEPS the previous query's rows, so the
	// grid showed "naruto" results under a header reading "one piece".
	let federatedFor = $state<string | null>(null);
	const federatedRequested = $derived(federatedFor !== null && federatedFor === query.trim());
	// Drives the in-flight label + the opt-in button visibility.
	const federatedPending = $derived(queryActive && !!auth.user && federatedRequested);
	// Offer the external-source fan-out only for a signed-in viewer on a text query
	// whose local results aren't already the federated set.
	const canSearchExternal = $derived(
		queryActive && !!auth.user && !rowsAreFederated && !federatedRequested,
	);

	// Whole-catalogue pagination (native path only): the server pages the ENTIRE
	// filtered catalogue and each page REPLACES the grid. THE page number is the URL's
	// — not component state — so a reload, a shared link and the browser's Back button
	// all agree, and the rows effect re-runs simply by reading it. EVERY facet in the
	// rail is now applied server-side across the whole catalogue, so `totalCount` counts
	// the filtered set and the pager walks it.
	const pageNum = $derived(pageParam(page.url.searchParams));
	let hasNext = $state(false);
	let totalCount = $state<number | null>(null);

	// The content-rating control exists only for a viewer who has opted into NSFW: the
	// server clamps the filter to the viewer's own posture, so for everyone else "Safe
	// only" is what they already have and "NSFW only" returns a guaranteed-empty page —
	// a control whose two interesting settings are a no-op and a dead end.
	const nsfwOptedIn = $derived(auth.user?.showNsfw ?? false);

	function serverFilters(): CatalogFilters {
		return {
			genres: [...selectedGenres],
			// Gated on RATING_FILTER_ENABLED so a stale/shared ?minRating= URL can't
			// still collapse the catalogue to 3 rows while the control is hidden.
			minRating: RATING_FILTER_ENABLED && minRating > 0 ? minRating : undefined,
			maxRating: RATING_FILTER_ENABLED && maxRating < 10 ? maxRating : undefined,
			types: [...types],
			status: status === 'any' ? undefined : status,
			sort: SORT_TO_WIRE[sort],
			// Same reasoning as the rating gate: the chip is hidden for an opted-out
			// viewer, so a stale/shared `?content=nsfw_only` link must not silently pin
			// them to an empty grid they have no control to clear.
			contentRating: nsfwOptedIn ? contentRating : undefined,
			// `undefined` when off, NEVER `false`: `false` asks the server for only the works
			// with no chapters, which would invert the filter instead of clearing it.
			hasChapters: hasChapters ? true : undefined,
		};
	}

	// Signature of the currently-displayed result set, for the back-nav cache. Native
	// results depend on query + EVERY server filter; federated results depend only on the
	// query (filters are applied client-side), so its signature deliberately omits the
	// filters — mirroring the effect's reactive-dependency split so a federated filter
	// change neither re-fetches nor invalidates the cache.
	let lastSig: string | null = null;
	// `reloadKey` at the time `lastSig` was set, so an explicit retry still refetches
	// even though its signature is unchanged.
	let lastReloadKey = -1;
	// Consult the cache on the FIRST effect run of a fresh mount (a back-nav into
	// Browse), never on user-driven filter/query changes — plus the one extra case
	// below.
	let didInitFromCache = false;
	// `didInitFromCache` alone only covers a REMOUNT (series → back). A pager step is a
	// SAME-ROUTE navigation, so browser-Back from page 3 to page 2 keeps the component
	// alive, leaves that flag true, and would refetch — showing a skeleton and losing
	// the scroll position (SvelteKit restores scroll before the rows land).
	// `beforeNavigate` tells us the outgoing navigation was a popstate, which re-arms
	// the cache for exactly one effect run. Clicking a Pager link is a `'link'`
	// navigation (they are real `<a href>`s), so it correctly does NOT restore: it
	// fetches fresh and scrolls to the top, which is what a forward step should do.
	let restoreOnPop = false;

	$effect(() => {
		const q = query.trim();
		// Wait for `initAuth()` (a root-layout effect) to settle before fetching.
		// Without this the first run read `auth.user === null`, fetched the whole
		// catalogue as `nat:anon:0`, then `auth.user` resolved, the signature changed
		// and it fetched the WHOLE catalogue again — a skeleton flash over results
		// that were already on screen, on every cold load for a signed-in viewer.
		if (!auth.ready) return;
		const loggedIn = !!auth.user;
		const rk = reloadKey; // manual retry re-runs the fetch
		// Federated only when the viewer has explicitly opted in for THIS query
		// (federatedRequested); otherwise a text query uses the fast local FTS path.
		const isFederated = !!q && loggedIn && federatedRequested;
		// Reading the filter state HERE (synchronously) only for the native path makes
		// the effect depend on it → native re-fetches on filter change; the federated
		// branch skips these reads so it re-fetches only on query/auth change. The page
		// number rides the same trick: the native path depends on it (a page step must
		// re-fetch), the federated path must not (it has no server pager at all).
		const nativeFilters = isFederated ? null : serverFilters();
		const nativePage = isFederated ? 1 : pageNum;
		// The cache key must capture EVERY input that changes the result set across a
		// remount, or a back-nav could serve one identity's results to another. Federated
		// results are per-authenticated-user; native results are server-filtered by the
		// viewer's NSFW preference. Both the cache and auth state survive login/logout
		// (SPA state, no reload), so fold the user id + NSFW posture into the signature.
		// (Filters still stay out of the FEDERATED sig — they're applied client-side there
		// — so a federated filter change neither re-fetches nor invalidates the cache.)
		// The native sig also carries the PAGE (`p`): each page REPLACES the grid, so two
		// pages of one query are two different result sets. Without it, back-nav from a
		// series opened on page 3 would restore page 1's 30 rows under a URL still
		// reading `?page=3`. The federated key stays page-free — federated search
		// returns a single deduped page (`hasNext = false`).
		//
		// Format/status/sort/content-rating are IN the native key. They used to be absent
		// because they were applied client-side over whatever page was loaded; now the
		// server resolves them, so two sorts of one query are two different result sets.
		// Without them here the module cache would happily serve "Trending page 1" as
		// "Top rated page 1" on any back-nav.
		const idTag = `${auth.user?.id ?? 'anon'}:${auth.user?.showNsfw ? 1 : 0}`;
		const sig = isFederated
			? `fed:${idTag}:${q}`
			: `nat:${idTag}:${JSON.stringify({
					q,
					g: [...nativeFilters!.genres!].sort(),
					mn: nativeFilters!.minRating ?? 0,
					mx: nativeFilters!.maxRating ?? 10,
					t: [...nativeFilters!.types!].sort(),
					st: nativeFilters!.status ?? '',
					so: nativeFilters!.sort ?? '',
					cr: nativeFilters!.contentRating ?? '',
					// In the key because it is a SERVER argument: "has chapters, page 1" and
					// "everything, page 1" are two different 30-row result sets of two
					// different lengths, and the module cache would otherwise serve one as
					// the other on any back-nav.
					hc: nativeFilters!.hasChapters ?? '',
					p: nativePage,
				})}`;

		// B1 back-nav restore: on a remount OR a same-route Back (see `restoreOnPop`), if
		// we cached this exact result set, hydrate it and skip the fetch entirely, then
		// restore the scroll position.
		const mayRestore = !didInitFromCache || restoreOnPop;
		didInitFromCache = true;
		restoreOnPop = false; // one-shot
		if (mayRestore) {
			const cached = browseCache.get(sig);
			if (cached) {
				rows = cached.rows;
				hasNext = cached.hasNext;
				totalCount = cached.totalCount;
				rowsAreFederated = cached.rowsAreFederated;
				rowsError = false;
				searchNotice = null;
				rowsLoading = false;
				lastSig = sig;
				lastReloadKey = rk;
				const y = cached.scrollY;
				// Wait for the grid to paint, then jump back to where we were.
				tick().then(() => requestAnimationFrame(() => window.scrollTo(0, y)));
				return;
			}
		}

		// Nothing about the result set changed — don't refetch. The effect re-runs for
		// reasons that don't affect the query (auth resolving to the same identity, the
		// federated opt-in being cleared after a rate limit), and each of those used to
		// cost a full catalogue round trip plus a skeleton flash over live results.
		if (sig === lastSig && rk === lastReloadKey && !rowsError) {
			rowsLoading = false;
			return;
		}

		rowsLoading = true;
		// Snapshot the pager envelope: the federated rate-limit branch below deliberately
		// KEEPS the previous result set on screen, so it has to put the previous pager
		// back with it — otherwise clicking "Search external sources" and getting
		// rate-limited left the catalogue rows visible but with hasNext=false (no pager)
		// and totalCount=null (the headline dropping from "10,574 series" to "20+
		// series"), because the short-circuit above returns before any refetch can
		// restore them. The page NUMBER is not snapshotted — it lives in the URL, which
		// that branch never touches.
		const prevPager = { hasNext, total: totalCount };
		hasNext = false;
		totalCount = null;
		let cancelled = false;
		const t = setTimeout(
			async () => {
				try {
					if (isFederated) {
						const outcome = await getFederatedSearch(q);
						if (cancelled) return;
						if (outcome.kind === 'ok') {
							rows = outcome.rows;
							rowsAreFederated = true;
							rowsError = false;
							searchNotice = null;
							// Federated live search returns a single deduped page (no server pager).
							hasNext = false;
							lastSig = sig;
							lastReloadKey = rk;
						} else if (outcome.kind === 'rateLimited') {
							// Keep prior results; show a transient message, not "0 results".
							searchNotice =
								outcome.retryAfter != null
									? `Too many searches — try again in ${outcome.retryAfter}s.`
									: 'Too many searches — try again in a moment.';
							// Nothing was replaced, so put the pager back where it was — the rows
							// still on screen are the previous (native) result set and they are
							// still paged. See `prevPager`.
							hasNext = prevPager.hasNext;
							totalCount = prevPager.total;
							// Release the opt-in so the "Search external sources" button comes
							// BACK — clicking it was the only way to retry, and leaving the flag
							// set made `canSearchExternal` permanently false until a navigation.
							// The effect re-run this triggers short-circuits on the unchanged
							// signature, so the prior rows and this notice both survive.
							federatedFor = null;
						} else {
							// Not authenticated / error → public native fallback (server-filtered).
							// `nativePage` is 1 on this branch by construction, and it must be:
							// the federated signature carries no page, so fetching `pageNum` here
							// would cache page N's rows under a page-free key. `requestFederated`
							// already drops the URL back to page 1 before we can get here.
							const r = await getNativeSearch(q, serverFilters(), nativePage);
							if (cancelled) return;
							rows = r.items;
							rowsAreFederated = false;
							rowsError = r.error;
							hasNext = r.hasNext;
							totalCount = r.total;
							searchNotice =
								outcome.kind === 'error'
									? 'Live search had a problem — showing catalogue results.'
									: null;
							lastSig = sig;
							lastReloadKey = rk;
						}
					} else {
						const r = await getNativeSearch(q, nativeFilters!, nativePage);
						if (cancelled) return;
						rows = r.items;
						rowsAreFederated = false;
						rowsError = r.error;
						hasNext = r.hasNext;
						totalCount = r.total;
						searchNotice = null;
						lastSig = sig;
						lastReloadKey = rk;
					}
				} finally {
					if (!cancelled) rowsLoading = false;
				}
			},
			q ? 280 : 160,
		);
		return () => {
			cancelled = true;
			clearTimeout(t);
		};
	});

	// Snapshot the current result set + scroll into the module cache whenever we
	// navigate away (into a series, or onto another page of results), so a back-nav can
	// restore it. Keyed by the signature of what's actually on screen — which now
	// includes the page number, so each page gets its own entry.
	beforeNavigate((nav) => {
		// Skip while a fetch is in flight: a new run resets hasNext/totalCount
		// synchronously but leaves lastSig + rows on the PREVIOUS result set until the
		// fetch lands, so snapshotting now would cache a no-pager view of a multi-page
		// result. During loading the grid shows a skeleton anyway, so there's nothing on
		// screen worth preserving — a back-nav simply re-runs the search.
		if (lastSig != null && !rowsLoading && typeof window !== 'undefined') {
			putBrowseCache(lastSig, {
				rows,
				hasNext,
				totalCount,
				rowsAreFederated,
				scrollY: window.scrollY,
			});
		}
		// Re-arm the cache for the next effect run when (and only when) this navigation
		// is the browser's Back/Forward. See `restoreOnPop`.
		restoreOnPop = nav.type === 'popstate';
	});

	/** Rewrite `?page=` in place, without adding a history entry. */
	function replacePage(p: number) {
		if (p === pageNum) return;
		void goto(withPage(page.url, p), { replaceState: true, noScroll: true, keepFocus: true });
	}

	// A shared, edited or stale link can name a page past the end (the server ECHOES the
	// requested page rather than clamping — same behaviour the admin review queue relies
	// on), which would render an empty grid reading "No matches found" over a catalogue
	// of 10,000+. Land on the last real page instead. replaceState, so Back doesn't
	// bounce the viewer straight onto the bad page again.
	//
	// BROWSE_PAGE_SIZE, not FEED_PAGE_SIZE: browse pages at 30 while the rest of the API
	// pages at 20, and this is the one place the constant is load-bearing rather than
	// cosmetic — computing `lastPage` at 20 over-reports the page count by half and
	// "repairs" the viewer onto a page that is itself past the end.
	$effect(() => {
		if (rowsLoading || rowsError || rowsAreFederated) return;
		if (rows.length > 0 || pageNum === 1) return;
		if (totalCount == null || totalCount === 0) return;
		const last = lastPage(totalCount, BROWSE_PAGE_SIZE);
		if (pageNum > last) replacePage(last);
	});

	// A new query or a new filter is a new result set — page 7 of the old one is
	// meaningless — so publish the rail to the URL and drop back to page 1, in ONE
	// replaceState navigation (see `syncFilterUrl` for why it cannot be two, and why
	// replaceState: filter fiddling must not fill the history stack).
	//
	// `types`/`status`/`sort`/`contentRating` are IN here now. They used to be excluded
	// on the grounds that they were applied client-side over the loaded page, so changing
	// them shouldn't discard the viewer's page. They are server arguments as of the
	// browse API, and the old comment said this is where they'd move. Leaving them out
	// now would be actively broken twice over: the page wouldn't reset (page 7 of
	// "Trending" is not page 7 of "Top rated") and, worse, the rows effect's
	// unchanged-signature short-circuit would swallow the change entirely — the chip
	// would light up and nothing would refetch.
	//
	// `reloadKey` is still absent — an explicit Retry must re-fetch the page the viewer
	// is ON, not page 1.
	//
	// The viewer identity + NSFW posture ARE included: the server filters by the
	// persisted `show_nsfw` preference, so logging in (or flipping the profile setting)
	// yields a different result set of a different length. Gated on `auth.ready`
	// because `initAuth()` resolves the user asynchronously — recording the baseline
	// before then would see anon→user as a filter change and blow away the page number
	// from a shared `?page=3` link on every cold load for a signed-in viewer.
	let lastFilterSig: string | null = null;
	$effect(() => {
		if (!auth.ready) return;
		const fsig = JSON.stringify([
			query.trim(),
			[...types].sort(),
			[...selectedGenres].sort(),
			status,
			sort,
			contentRating,
			hasChapters,
			minRating,
			maxRating,
			auth.user?.id ?? 'anon',
			auth.user?.showNsfw ? 1 : 0,
		]);
		// Don't fight the initial URL: the first run only records the baseline.
		if (lastFilterSig === null) {
			lastFilterSig = fsig;
			return;
		}
		if (fsig === lastFilterSig) return;
		lastFilterSig = fsig;
		syncFilterUrl();
	});

	function retryRows() {
		reloadKey++;
	}

	// Opt into the federated multi-extension fan-out for the current query (signed-in
	// only). Flips the dispatch flag; the rows effect re-runs and swaps in federated
	// results (which carry per-source translator chips).
	function requestFederated() {
		if (!auth.user || federatedRequested) return;
		federatedFor = query.trim();
		// Federated results are one deduped page with no server pager, so a lingering
		// `?page=5` would describe a pager that isn't rendered — and would still be in
		// the URL when the viewer goes back to the native path.
		replacePage(1);
	}

	// The displayed rows.
	//
	// NATIVE rows are returned exactly as the server ordered and filtered them — every
	// facet in the rail (genre, format, status, sort, content rating, rating range) is a
	// server argument now, so there is nothing left to narrow here. Re-filtering or
	// re-sorting them locally is precisely the bug this replaced: it operated on the one
	// page that happened to be loaded while presenting itself as catalogue-wide, so
	// "Top rated" ranked 30 arbitrary rows and a Format chip could empty a grid sitting
	// on 48,000 matches.
	//
	// FEDERATED rows stay client-filtered BY DESIGN: `searchAllSources` takes no filter
	// arguments (it fans out to live extensions), so genre/rating are the only facets it
	// can honour and they are honoured here. Format/status/sort are simply not available
	// on that path — see `facetsIgnored`, which says so rather than pretending.
	const results = $derived.by(() => {
		if (!rowsAreFederated) return rows;
		const gsel = selectedGenres.map((g) => g.toLowerCase());
		return rows.filter((m) => {
			if (gsel.length && !m.genres.some((mg) => gsel.includes(mg.toLowerCase()))) return false;
			if (minRating > 0 && m.rating < minRating) return false;
			if (maxRating < 10 && m.rating > maxRating) return false;
			return true;
		});
	});

	// The CATALOGUE-WIDE match count, deliberately not the page's — the per-page range
	// lives in the Pager. It is honest unconditionally now that the server applies every
	// facet: `totalCount` describes the same filtered set the grid is a window onto.
	// (It previously had to be suppressed whenever a client-side chip narrowed the page,
	// because the two numbers then described different sets.)
	//
	// Falls back to the approximate form only when the total is genuinely unknown. That
	// is NOT the text-search path — both branches of the server's `search` resolver
	// return `total: Some(..)` (the FTS branch counts in SQL, same as the browse branch)
	// — so a null total means the backend is off, the request failed, an adapter that
	// doesn't report one is in use, or these are federated rows.
	const countLabel = $derived(
		totalCount != null
			? `${totalCount.toLocaleString()} series`
			: `${results.length}${hasNext ? '+' : ''} series`,
	);

	const resultsLoading = $derived(rowsLoading);
	const catalogError = $derived(rowsError);

	// Format / Status / Sort / Content are BROWSE arguments: the server applies them to
	// an empty query and ignores them for a text query, which is ordered by full-text
	// relevance instead (and the federated fan-out can't take them at all). Rather than
	// leave chips that look active and do nothing, say so — the same disclosure the old
	// page-scope note existed for, reduced to the case that is still true.
	const facetsIgnored = $derived(
		queryActive &&
			!resultsLoading &&
			(types.length > 0 ||
				status !== 'any' ||
				sort !== 'trending' ||
				hasChapters ||
				// Gated: for an opted-out viewer the Content group isn't rendered and the
				// filter isn't sent, so naming it here would point at a control that
				// doesn't exist on their screen.
				(nsfwOptedIn && contentRating !== 'ALL')),
	);

	const anyFilter = $derived(
		types.length > 0 ||
			selectedGenres.length > 0 ||
			status !== 'any' ||
			(nsfwOptedIn && contentRating !== 'ALL') ||
			hasChapters ||
			minRating > 0 ||
			maxRating < 10,
	);
	const ratingLabel = $derived(
		minRating === 0 && maxRating >= 10
			? 'Any'
			: `${minRating.toFixed(1)} – ${maxRating >= 10 ? '10' : maxRating.toFixed(1)}`,
	);

	// `kind` exists purely to key the {#each}: the labels come from different facets
	// and are NOT mutually unique — a genre literally named "Ongoing"/"Completed"/
	// "Hiatus"/"Cancelled" would collide with the status pill, and Svelte 5 throws
	// `each_key_duplicate` in production, killing the page.
	const activePills = $derived.by(() => {
		const pills: { kind: string; label: string; remove: () => void }[] = [];
		types.forEach((t) =>
			pills.push({
				kind: 'type',
				label: `${FLAG[t]}  ${t}`,
				remove: () => (types = toggle(types, t)),
			}),
		);
		selectedGenres.forEach((g) =>
			pills.push({
				kind: 'genre',
				label: g,
				remove: () => (selectedGenres = toggle(selectedGenres, g)),
			}),
		);
		if (status !== 'any')
			pills.push({
				kind: 'status',
				label: STATUS_META[status].label,
				remove: () => (status = 'any'),
			});
		// Gated on `nsfwOptedIn` for the same reason `serverFilters` gates it: with the
		// chip group hidden, a pill from a shared `?content=` link would be the only
		// evidence of a filter that isn't even being sent.
		if (nsfwOptedIn && contentRating !== 'ALL')
			pills.push({
				kind: 'content',
				label: CONTENT_LABEL[contentRating],
				remove: () => (contentRating = 'ALL'),
			});
		if (hasChapters)
			pills.push({
				kind: 'chapters',
				label: 'Has chapters',
				remove: () => (hasChapters = false),
			});
		if (minRating > 0 || maxRating < 10)
			pills.push({
				kind: 'rating',
				label: `${ratingLabel} rating`,
				remove: () => {
					minRating = 0;
					maxRating = 10;
				},
			});
		return pills;
	});

	// Dual-handle rating slider: keep min ≤ max as either thumb is dragged.
	function onMinRating() {
		if (minRating > maxRating) minRating = maxRating;
	}
	function onMaxRating() {
		if (maxRating < minRating) maxRating = minRating;
	}

	const sortChips = [
		{ key: 'trending', label: 'Trending' },
		{ key: 'rating', label: 'Top rated' },
		// NOT "Newest". The server's NEWEST orders by newest upstream CHAPTER release,
		// not by when a work entered the catalogue, so a decade-old series that shipped a
		// chapter this morning sits at the top. Labelled "Newest" the chip and its own
		// ordering disagreed on screen, which reads as a bug in the sort.
		{ key: 'newest', label: 'Recently updated' },
		{ key: 'chapters', label: 'Most chapters' },
	] as const;

	/** Chip labels for the content-rating tiers this rail offers (see CONTENT_RATINGS).
	 *  "only" is load-bearing on both: SAFE is a cumulative ceiling that happens to admit
	 *  just `safe`, and NSFW_ONLY is an exclusion — neither is "include these too". */
	const CONTENT_LABEL: Record<RailContentRating, string> = {
		ALL: 'All',
		SAFE: 'Safe only',
		NSFW_ONLY: 'NSFW only',
	};

	/**
	 * The card's second line.
	 *
	 * `m.ch` is legitimately 0 now that Browse pages the whole catalogue — ~67k works have no
	 * chapter yet — and the old `` `${m.genre} · ${m.ch} ch` `` rendered that as "Action · 0
	 * ch", or as a bare "· 0 ch" for a work with no genre. Both read as a broken card rather
	 * than as the true statement "we have no chapters for this yet, try another source".
	 * Neither half is guaranteed present, so the separator is only emitted between two real
	 * parts.
	 */
	function cardSub(m: FederatedResultView): string {
		const chapters = m.ch > 0 ? `${m.ch} ch` : 'No chapters yet';
		return m.genre ? `${m.genre} · ${chapters}` : chapters;
	}

	// Mobile: the filter rail collapses into a bottom sheet toggled by this flag.
	let filtersOpen = $state(false);

	function resetFilters() {
		types = [];
		selectedGenres = [];
		status = 'any';
		contentRating = 'ALL';
		hasChapters = false;
		minRating = 0;
		maxRating = 10;
	}
	function resetAll() {
		query = '';
		resetFilters();
	}
</script>

<div class="head k-gutter">
	<div class="head-inner">
		<h1>Browse</h1>
		<div class="searchbar">
			<Icon name="search" size={20} stroke="#87857f" />
			<input bind:value={query} placeholder="Search series, authors, genres…" />
			{#if query}
				<button class="clear" aria-label="Clear" onclick={() => (query = '')}
					><Icon name="x" size={18} /></button
				>
			{/if}
		</div>
	</div>
</div>

<div class="body k-gutter">
	{#if filtersOpen}
		<button class="filter-scrim" aria-label="Close filters" onclick={() => (filtersOpen = false)}
		></button>
	{/if}
	<aside class="rail" class:open={filtersOpen}>
		<div class="rail-head">
			<span class="rail-title">Filters</span>
			<div class="rail-head-right">
				{#if anyFilter}<button class="reset" onclick={resetFilters}>Reset</button>{/if}
				<button class="sheet-done" onclick={() => (filtersOpen = false)}>Done</button>
			</div>
		</div>

		<div class="group">
			<span class="glabel">Format</span>
			<div class="chips">
				{#each TYPES as t (t)}
					<button
						class="chip"
						class:on={types.includes(t)}
						onclick={() => (types = toggle(types, t))}
					>
						<span class="flag">{FLAG[t]}</span>{t}
					</button>
				{/each}
			</div>
		</div>

		<!-- Hidden entirely while there are no facets. `genreFacets` is empty until the
		     MangaDex tags are backfilled into `work_tag`, and that backfill has not run:
		     rendering the group would put a labelled, permanently-empty box in the rail
		     that reads as a broken control rather than an absent one. It reappears on its
		     own the moment the backfill lands — nothing here needs changing. Selected
		     genres keep the group open even if the facet list is momentarily empty, so a
		     shared `?genre=` link never strands a filter with no way to clear it. -->
		{#if facets.length > 0 || selectedGenres.length > 0}
			<div class="group">
				<div class="glabel-row">
					<span class="glabel">Genre</span>
					{#if selectedGenres.length}<span class="glabel-count"
							>{selectedGenres.length} selected</span
						>{/if}
				</div>
				<div class="genre-search">
					<Icon name="search" size={15} stroke="#87857f" />
					<input bind:value={genreQuery} placeholder="Filter genres…" aria-label="Filter genres" />
					{#if genreQuery}
						<button class="gs-clear" aria-label="Clear" onclick={() => (genreQuery = '')}
							><Icon name="x" size={14} /></button
						>
					{/if}
				</div>
				<div class="genre-list">
					{#if facets.length === 0}
						<span class="genre-empty">Genres load with the catalogue…</span>
					{:else}
						{#each shownFacets as f (f.genre)}
							<button
								class="genre-opt"
								class:on={selectedGenres.includes(f.genre)}
								onclick={() => (selectedGenres = toggle(selectedGenres, f.genre))}
							>
								<span class="go-check" aria-hidden="true">
									{#if selectedGenres.includes(f.genre)}<Icon
											name="check"
											size={12}
											strokeWidth={2.6}
										/>{/if}
								</span>
								<span class="go-name">{f.genre}</span>
								<span class="go-count">{f.count}</span>
							</button>
						{:else}
							<span class="genre-empty">No genres match “{genreQuery}”.</span>
						{/each}
					{/if}
				</div>
			</div>
		{/if}

		<div class="group">
			<span class="glabel">Status</span>
			<div class="chips">
				{#each ['any', 'ongoing', 'completed', 'hiatus', 'cancelled'] as s (s)}
					<button
						class="chip"
						class:on={status === s}
						onclick={() => (status = s as Status | 'any')}
					>
						{s === 'any' ? 'Any' : STATUS_META[s as Status].label}
					</button>
				{/each}
			</div>
		</div>

		<!-- "Has chapters": a single toggle, not a two-chip Any/Yes pair, because the wire
		     argument has three states and only two of them are worth offering. Absent (off) is
		     the whole catalogue and `true` is the readable subset; `false` — "only series with
		     NO chapters" — is a debugging view, not a reader's request. A checkbox-shaped chip
		     therefore maps 1:1 onto what the server can usefully do. -->
		<div class="group">
			<span class="glabel">Availability</span>
			<div class="chips">
				<button class="chip" class:on={hasChapters} onclick={() => (hasChapters = !hasChapters)}>
					Has chapters
				</button>
			</div>
			<span class="ghint">
				Off shows everything, including series whose chapters were pulled upstream after a licensing
				deal.
			</span>
		</div>

		<!-- Content rating, and ONLY for a viewer who has already opted into NSFW. This
		     replaced a "Show NSFW" switch that lived here and wrote the account-wide
		     `show_nsfw` preference — a permanent profile setting disguised as a browse
		     filter, one click away from every anonymous-adjacent viewer. Opting in now
		     happens in one place, on the profile screen; this narrows WITHIN that posture
		     and can never widen past it (the server clamps it), so showing it to an
		     opted-out viewer would offer three chips of which two are a no-op and a
		     guaranteed-empty grid. See `nsfwOptedIn`. -->
		{#if nsfwOptedIn}
			<div class="group">
				<span class="glabel">Content</span>
				<div class="chips">
					{#each CONTENT_RATINGS as cr (cr)}
						<button
							class="chip"
							class:on={contentRating === cr}
							onclick={() => (contentRating = cr)}
						>
							{CONTENT_LABEL[cr]}
						</button>
					{/each}
				</div>
			</div>
		{/if}

		<!-- Hidden until ratings come from MangaDex statistics rather than the 3-row
		     local `reviews` table — see RATING_FILTER_ENABLED above. -->
		{#if RATING_FILTER_ENABLED}
			<div class="group">
				<div class="rating-head">
					<span class="glabel">Rating</span>
					<span class="rating-val">
						{#if minRating > 0 || maxRating < 10}<Icon
								name="star"
								size={12}
								fill="var(--k-star)"
							/>{/if}{ratingLabel}
					</span>
				</div>
				<div class="range" style="--min:{(minRating / 10) * 100}%;--max:{(maxRating / 10) * 100}%">
					<div class="range-rail"></div>
					<div class="range-fill"></div>
					<input
						class="range-input"
						type="range"
						min="0"
						max="10"
						step="0.5"
						bind:value={minRating}
						oninput={onMinRating}
						aria-label="Minimum rating"
					/>
					<input
						class="range-input"
						type="range"
						min="0"
						max="10"
						step="0.5"
						bind:value={maxRating}
						oninput={onMaxRating}
						aria-label="Maximum rating"
					/>
				</div>
				<div class="rating-scale"><span>0</span><span>10</span></div>
			</div>
		{/if}
	</aside>

	<div class="results" id="browse-results" aria-busy={resultsLoading}>
		<button class="filter-trigger" onclick={() => (filtersOpen = true)}>
			<Icon name="sliders-h" size={16} />Filters{#if activePills.length}<span class="ft-badge"
					>{activePills.length}</span
				>{/if}
		</button>
		<div class="results-head">
			<div class="results-title">
				<span class="rt">{query.trim() ? `"${query.trim()}"` : 'All series'}</span>
				<span class="rc"
					>{resultsLoading
						? queryActive
							? federatedPending
								? 'Searching all sources…'
								: 'Searching…'
							: 'Loading…'
						: countLabel}</span
				>
			</div>
			<!-- Plain "Sort" again: it is the server's ORDER BY over the whole filtered
			     catalogue now, not a re-sort of the loaded page, so it no longer needs the
			     "Sort this page" qualifier that used to sit here. -->
			<div class="sort">
				<span class="sort-label">Sort</span>
				<div class="chips">
					{#each sortChips as so (so.key)}
						<button class="sortchip" class:on={sort === so.key} onclick={() => (sort = so.key)}
							>{so.label}</button
						>
					{/each}
				</div>
			</div>
		</div>

		<!-- The one scope disclosure that survives: Format/Status/Sort/Content are browse
		     arguments and a text query ignores them server-side. See `facetsIgnored`. -->
		{#if facetsIgnored}
			<p class="scope-note">
				Format, Status, Sort, Content and Has chapters apply to browsing — a text search is ranked
				by relevance and ignores them. Clear the search to use them.
			</p>
		{/if}

		{#if activePills.length}
			<div class="pills">
				{#each activePills as p (`${p.kind}:${p.label}`)}
					<button class="pill" onclick={p.remove}
						>{p.label}<Icon name="x" size={13} stroke="#9c9a94" strokeWidth={2.4} /></button
					>
				{/each}
			</div>
		{/if}

		{#if searchNotice}
			<div class="notice" role="status">
				<Icon name="alert" size={16} />
				<span>{searchNotice}</span>
			</div>
		{/if}

		{#if resultsLoading}
			<CardGridSkeleton count={12} />
		{:else if results.length}
			<div class="grid">
				{#each results as m (m.id ?? m.title)}
					<MangaCard
						title={m.title}
						sub={cardSub(m)}
						rating={m.rating.toFixed(1)}
						cover={m.cover}
						id={m.id}
						flagType={m.type}
						status={{ label: STATUS_META[m.status].label, color: STATUS_META[m.status].color }}
						translators={m.translators ?? []}
					/>
				{/each}
			</div>
			{#if canSearchExternal}
				<div class="ext-search">
					<span class="ext-hint">Not finding it in the catalogue?</span>
					<button class="ext-btn" onclick={requestFederated}>
						<Icon name="search" size={15} />Search external sources
					</button>
				</div>
			{/if}
		{:else if !queryActive && catalogError}
			<div class="empty">
				<div class="empty-icon error"><Icon name="alert" size={24} /></div>
				<div class="empty-title">Couldn't load the catalogue</div>
				<div class="empty-desc">
					Something went wrong reaching the server. Check your connection and try again.
				</div>
				<button class="empty-btn" onclick={retryRows}>Retry</button>
			</div>
		{:else if !searchNotice}
			<!-- Generic and now CORRECT: with every facet resolved server-side, an empty
			     result set means the filtered catalogue is genuinely empty. There used to
			     be a "Nothing on this page matches" branch above this one, because the
			     client-side Format/Status chips could empty a page of a catalogue that had
			     thousands of matches; that can no longer happen. -->
			<div class="empty">
				<div class="empty-icon"><Icon name="search" size={24} /></div>
				<div class="empty-title">No matches found</div>
				<div class="empty-desc">
					{canSearchExternal
						? 'Nothing in the catalogue matches. Try searching external sources.'
						: 'Try a different search term or loosen your filters.'}
				</div>
				{#if canSearchExternal}
					<button class="empty-btn" onclick={requestFederated}>Search external sources</button>
				{:else}
					<button class="empty-btn" onclick={resetAll}>Clear all</button>
				{/if}
			</div>
		{/if}

		<!-- Native path only: the federated fan-out returns a single deduped page with no
		     server pager. `Pager` hides itself on page 1 with no next page, so a result
		     set that fits in one page renders no pager at all. Past page 1 it stays on
		     screen through a load (its links become inert aria-disabled spans) so the
		     landmark and live region survive, rather than vanishing under the click that
		     triggered the load. `aria-controls` is deliberately not passed: the only
		     candidate region is `.results`, which CONTAINS this pager. -->
		{#if !rowsAreFederated}
			<Pager
				page={pageNum}
				{hasNext}
				total={totalCount}
				pageSize={BROWSE_PAGE_SIZE}
				count={results.length}
				loading={resultsLoading}
				href={(p) => withPage(page.url, p)}
				label="Browse results pages"
			/>
		{/if}
	</div>
</div>

<style>
	.head {
		padding-top: 44px;
	}
	.head-inner {
		max-width: 960px;
	}
	h1 {
		font-size: 34px;
		margin: 0 0 20px;
		color: var(--k-text-bright);
	}
	.searchbar {
		display: flex;
		align-items: center;
		gap: 14px;
		height: 56px;
		padding: 0 10px 0 22px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		border-radius: 12px;
		transition: border-color 0.15s;
	}
	.searchbar:focus-within {
		border-color: var(--k-border-strong);
	}
	.searchbar input {
		flex: 1;
		min-width: 0;
		background: transparent;
		border: none;
		outline: none;
		color: var(--k-text);
		font-size: 16px;
	}
	.clear {
		width: 36px;
		height: 36px;
		border: none;
		background: transparent;
		color: var(--k-text-faint);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.clear:hover {
		color: var(--k-text);
	}
	.flag {
		font-size: 17px;
		line-height: 1;
	}
	.body {
		display: grid;
		grid-template-columns: 236px 1fr;
		gap: 44px;
		padding-top: 32px;
		padding-bottom: 80px;
		align-items: start;
	}
	.rail {
		display: flex;
		flex-direction: column;
		gap: 30px;
		position: sticky;
		top: 88px;
	}
	.rail-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	.rail-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 16px;
		color: var(--k-text);
	}
	.reset {
		background: none;
		border: none;
		font-size: 12.5px;
		font-weight: 700;
		color: var(--k-text-dimmer);
		cursor: pointer;
	}
	.reset:hover {
		color: var(--k-text);
	}
	.group {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.glabel {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.09em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		font-size: 12.5px;
		font-weight: 600;
		padding: 6px 13px;
		border-radius: var(--k-radius-pill);
		cursor: pointer;
		background: var(--k-hover-fill);
		border: 1px solid var(--k-border-3);
		color: var(--k-text-3);
		transition: all 0.15s;
	}
	.chip.on {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.rating-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	.rating-val {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		font-size: 13px;
		font-weight: 700;
		color: var(--k-text);
	}
	/* genre multi-select (S4) */
	/* One-line explanation under a control whose OFF state is the surprising one. Matches
	   `.genre-empty`'s size and colour, so the rail keeps one voice for secondary text. */
	.ghint {
		font-size: 12.5px;
		line-height: 1.4;
		color: var(--k-text-faint);
	}
	.glabel-row {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
	}
	.glabel-count {
		font-size: 11px;
		font-weight: 700;
		color: var(--k-primary);
	}
	.genre-search {
		display: flex;
		align-items: center;
		gap: 8px;
		height: 36px;
		padding: 0 8px 0 12px;
		border-radius: 9px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
	}
	.genre-search:focus-within {
		border-color: var(--k-border-strong);
	}
	.genre-search input {
		flex: 1;
		min-width: 0;
		background: transparent;
		border: none;
		outline: none;
		color: var(--k-text);
		font-size: 13px;
	}
	.gs-clear {
		width: 26px;
		height: 26px;
		border: none;
		background: transparent;
		color: var(--k-text-faint);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.genre-list {
		display: flex;
		flex-direction: column;
		gap: 2px;
		max-height: 232px;
		overflow-y: auto;
		padding-right: 2px;
	}
	.genre-opt {
		display: flex;
		align-items: center;
		gap: 9px;
		padding: 7px 9px;
		border-radius: 8px;
		border: 1px solid transparent;
		background: transparent;
		color: var(--k-text-3);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
		text-align: left;
		transition: background 0.12s;
	}
	.genre-opt:hover {
		background: var(--k-hover-fill);
	}
	.genre-opt.on {
		background: rgba(224, 131, 105, 0.12);
		border-color: rgba(224, 131, 105, 0.4);
		color: var(--k-text-bright);
	}
	.go-check {
		width: 18px;
		height: 18px;
		flex: 0 0 auto;
		border-radius: 5px;
		border: 1px solid var(--k-border-3);
		display: inline-flex;
		align-items: center;
		justify-content: center;
		color: var(--k-on-primary);
	}
	.genre-opt.on .go-check {
		background: var(--k-primary);
		border-color: var(--k-primary);
	}
	.go-name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.go-count {
		flex: 0 0 auto;
		font-size: 11.5px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	.genre-empty {
		font-size: 12.5px;
		color: var(--k-text-faint);
		padding: 6px 2px;
	}
	/* dual-handle rating range slider */
	.range {
		position: relative;
		height: 24px;
		--min: 0%;
		--max: 100%;
	}
	.range-rail,
	.range-fill {
		position: absolute;
		top: 50%;
		height: 4px;
		border-radius: 4px;
		transform: translateY(-50%);
		pointer-events: none;
	}
	.range-rail {
		left: 0;
		right: 0;
		background: var(--k-border-4);
	}
	.range-fill {
		left: var(--min);
		right: calc(100% - var(--max));
		background: var(--k-star);
	}
	/* Two native range inputs stacked; only the thumbs receive pointer events so
	   both handles stay grabbable. */
	.range-input {
		position: absolute;
		inset: 0;
		width: 100%;
		margin: 0;
		background: transparent;
		-webkit-appearance: none;
		appearance: none;
		pointer-events: none;
	}
	.range-input::-webkit-slider-thumb {
		-webkit-appearance: none;
		appearance: none;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		background: var(--k-text-bright);
		border: 2px solid var(--k-star);
		cursor: pointer;
		pointer-events: auto;
	}
	.range-input::-moz-range-thumb {
		width: 18px;
		height: 18px;
		border-radius: 50%;
		background: var(--k-text-bright);
		border: 2px solid var(--k-star);
		cursor: pointer;
		pointer-events: auto;
	}
	.rating-scale {
		display: flex;
		justify-content: space-between;
		font-size: 10.5px;
		color: var(--k-text-disabled);
	}
	.results {
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 22px;
	}
	.results-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 20px;
		flex-wrap: wrap;
	}
	.results-title {
		display: flex;
		align-items: baseline;
		gap: 11px;
	}
	.rt {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text);
	}
	.rc {
		font-size: 13.5px;
		color: var(--k-text-faint);
	}
	.sort {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.sort-label {
		font-size: 12.5px;
		color: var(--k-text-faint);
	}
	.sortchip {
		font-size: 12.5px;
		font-weight: 700;
		padding: 7px 13px;
		border-radius: var(--k-radius-pill);
		cursor: pointer;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-dimmer);
		transition: all 0.15s;
	}
	.sortchip.on {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.pills {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.pill {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		font-size: 12px;
		font-weight: 600;
		padding: 6px 8px 6px 12px;
		border-radius: var(--k-radius-pill);
		background: rgba(255, 255, 255, 0.07);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-1);
		cursor: pointer;
	}
	.notice {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 12px 16px;
		border-radius: 10px;
		background: rgba(246, 183, 60, 0.1);
		border: 1px solid rgba(246, 183, 60, 0.34);
		color: var(--k-text-2);
		font-size: 13.5px;
		font-weight: 600;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(158px, 1fr));
		gap: 30px 22px;
	}
	/* Scope disclosure for the browse-only facets under a text query. Sits directly
	   under the results head, above the grid, so it is read before the cards rather
	   than discovered after them. */
	.scope-note {
		margin: -10px 0 0;
		font-size: 12.5px;
		line-height: 1.45;
		color: var(--k-text-faint);
	}
	.ext-search {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 8px;
		padding: 22px 0 4px;
	}
	.ext-hint {
		font-size: 12.5px;
		color: var(--k-text-faint);
	}
	.ext-btn {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		height: 40px;
		padding: 0 20px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		font-weight: 700;
		font-size: 13px;
		cursor: pointer;
		transition: border-color 0.15s;
	}
	.ext-btn:hover {
		border-color: var(--k-border-strong);
	}
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		padding: 80px 20px;
		text-align: center;
	}
	.empty-icon {
		width: 56px;
		height: 56px;
		border-radius: 50%;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--k-text-disabled);
	}
	.empty-icon.error {
		color: var(--k-hiatus);
		border-color: rgba(246, 183, 60, 0.4);
	}
	.empty-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text);
	}
	.empty-desc {
		font-size: 14px;
		color: var(--k-text-dimmer);
		max-width: 320px;
		line-height: 1.5;
	}
	.empty-btn {
		margin-top: 6px;
		height: 42px;
		padding: 0 22px;
		border-radius: 8px;
		background: var(--k-primary);
		border: none;
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 13.5px;
		cursor: pointer;
	}
	/* Mobile filter sheet controls — hidden on desktop/tablet. */
	.rail-head-right {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.sheet-done {
		display: none;
	}
	.filter-trigger {
		display: none;
	}
	.filter-scrim {
		display: none;
	}
	@media (max-width: 820px) {
		.body {
			grid-template-columns: 1fr;
			gap: 28px;
		}
		.rail {
			position: static;
		}
	}
	@media (max-width: 640px) {
		/* Rail becomes a bottom sheet; results take the full width with a Filters
		   trigger. Declared after the 820 block so it wins the shared props. */
		.filter-trigger {
			display: inline-flex;
			align-items: center;
			gap: 8px;
			align-self: flex-start;
			height: 40px;
			padding: 0 16px;
			margin-bottom: 4px;
			border-radius: var(--k-radius-pill);
			background: var(--k-surface-2);
			border: 1px solid var(--k-border-3);
			color: var(--k-text);
			font-size: 13.5px;
			font-weight: 700;
			cursor: pointer;
		}
		.ft-badge {
			display: inline-flex;
			align-items: center;
			justify-content: center;
			min-width: 18px;
			height: 18px;
			padding: 0 5px;
			border-radius: 9px;
			background: var(--k-primary);
			color: var(--k-on-primary);
			font-size: 11px;
			font-weight: 800;
		}
		.filter-scrim {
			display: block;
			position: fixed;
			inset: 0;
			z-index: 59;
			border: none;
			background: rgba(8, 8, 9, 0.55);
			backdrop-filter: blur(2px);
		}
		.rail {
			position: fixed;
			left: 0;
			right: 0;
			bottom: 0;
			top: auto;
			z-index: 60;
			max-height: 84vh;
			overflow-y: auto;
			gap: 22px;
			padding: 18px 20px calc(24px + env(safe-area-inset-bottom));
			background: var(--k-surface-3);
			border: 1px solid var(--k-border-2);
			border-radius: 18px 18px 0 0;
			box-shadow: 0 -20px 60px rgba(0, 0, 0, 0.5);
			transform: translateY(110%);
			transition: transform 0.28s cubic-bezier(0.4, 0, 0.2, 1);
		}
		.rail.open {
			transform: translateY(0);
		}
		.sheet-done {
			display: inline-flex;
			align-items: center;
			height: 32px;
			padding: 0 16px;
			border-radius: var(--k-radius-pill);
			background: var(--k-primary);
			border: none;
			color: var(--k-on-primary);
			font-size: 13px;
			font-weight: 700;
			cursor: pointer;
		}
		.grid {
			grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
			gap: 22px 14px;
		}
	}
</style>
