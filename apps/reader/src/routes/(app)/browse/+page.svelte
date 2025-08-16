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
		filterApplied: boolean;
		scrollY: number;
	};
	// Back-nav restore (B1): keep the last few browse result sets in MODULE memory —
	// it survives a component remount (series → back) but not a full reload — so
	// returning to Browse rehydrates instantly with scroll intact instead of
	// re-running the whole search. Keyed by the canonical query string (filters, sort
	// AND page) plus the viewer; small FIFO cap so it can't grow unbounded across a long
	// session. 12 × ≤30 view objects is trivial memory.
	const browseCache = new Map<string, BrowseCacheEntry>();
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
	import { tick, untrack } from 'svelte';
	import type { TagInfo } from '@komika/types';
	import type { BrowseSortField } from '@komika/api';
	import Icon from '$lib/components/Icon.svelte';
	import MangaCard from '$lib/components/MangaCard.svelte';
	import CardGridSkeleton from '$lib/components/CardGridSkeleton.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import { STATUS_META } from '$lib/data/types';
	import { BROWSE_PAGE_SIZE, lastPage, withPage } from '$lib/data/paging';
	import { getBrowseSearch, getFederatedSearch } from '$lib/data/source';
	import {
		activeChips,
		applicableState,
		applyDraft,
		browseHref,
		clearFilters,
		debounce,
		draftChanges,
		effectiveDir,
		effectiveSort,
		filtersOnlySig,
		legacyUnapplied,
		matchesFederated,
		parseBrowseUrl,
		sortOptions,
		toggleDir,
		toSearchParams,
		withPageNum,
		withQuery,
		withSort,
		type BrowseState,
	} from '$lib/data/browse-filters';
	import { auth } from '$lib/auth.svelte';
	import ActiveFilters from './ActiveFilters.svelte';
	import FilterPanel from './FilterPanel.svelte';
	import SortMenu from './SortMenu.svelte';

	let { data } = $props();

	// ---- tag vocabulary -----------------------------------------------------------
	// `data.tags` never rejects (see +page.ts). Until it lands, the source is assumed to
	// be the v2 vocabulary so a shared link's excluded tags aren't dropped by
	// `applicableState` during the load and then reinstated a moment later.
	let tagData = $state<{ tags: TagInfo[]; source: 'tags' | 'genres' }>({
		tags: [],
		source: 'tags',
	});
	let tagsLoading = $state(true);
	$effect(() => {
		let live = true;
		data.tags.then((t) => {
			if (!live) return;
			tagData = t;
			tagsLoading = false;
		});
		return () => {
			live = false;
		};
	});
	const tagIndex = $derived(new Map(tagData.tags.map((t) => [t.id, t])));
	const tagName = (id: string): string | undefined => tagIndex.get(id)?.name;

	// ---- state: THE URL -----------------------------------------------------------
	// Every filter, the sort and the page are read straight off the URL (see
	// $lib/data/browse-filters for the parse/canonicalise rules). There is no component
	// copy of them to keep in sync — the only local state is the search box's text while
	// it is being typed, and the filter panel's unapplied draft.
	const nsfw = $derived(auth.user?.showNsfw ?? false);
	const urlState = $derived(parseBrowseUrl(page.url.searchParams));
	// What is actually APPLIED: the URL minus what this viewer can't use (NSFW ratings
	// without the opt-in; exclusions against a server with no tag vocabulary). The
	// request, the chips and the panel are all built from this, so nothing is shown as
	// active that isn't being sent.
	const applied = $derived(
		applicableState(urlState, { nsfw, tagSource: tagsLoading ? 'tags' : tagData.source }),
	);
	const chips = $derived(activeChips(applied, tagName));

	/** Navigate to a state. Discrete changes PUSH (Back undoes them); typing REPLACES. */
	function navigate(next: BrowseState, replace = false) {
		const href = browseHref(page.url.pathname, next);
		if (href === `${page.url.pathname}${page.url.search}`) return;
		void goto(href, { replaceState: replace, noScroll: true, keepFocus: true });
	}

	// ---- search box ---------------------------------------------------------------
	// Typing rewrites `?q=` after a pause, with replaceState: a history entry per word
	// typed would make Back unusable. A discrete action taken mid-debounce (a chip, the
	// sort, Apply) cancels the pending write and carries the box's CURRENT text itself,
	// so it can't be overwritten a moment later by a stale write.
	let qInput = $state(page.url.searchParams.get('q') ?? '');
	const writeQuery = debounce((q: string) => navigate(withQuery(applied, q), true), 320);

	// URL → box, for navigations that weren't our own typing (Back/Forward, the header's
	// search overlay, home-page links). Our own write reads back as equal and is a no-op,
	// which also preserves a trailing space the viewer is mid-way through typing.
	//
	// `qInput` is read UNTRACKED: this must run on URL changes only. Tracking the box too
	// would re-run it on the box's own edits — and `clearSearch` empties the box a moment
	// BEFORE the URL catches up, so a tracked read would put the old query straight back.
	$effect(() => {
		const q = urlState.q;
		if (writeQuery.pending) return;
		if (untrack(() => qInput).trim() !== q) qInput = q;
	});

	// Canonicalise the address once it is ours to change: a legacy link
	// (`?genre=Action&sort=trending`) or a hand-edited one is rewritten in place to the
	// one spelling `toSearchParams` produces, so the edge cache, the back-nav cache and a
	// re-shared link all see a single URL per result set. replaceState — it is the same
	// page, not a step. A fixed point, so this can't loop.
	$effect(() => {
		const canonical = toSearchParams(urlState).toString();
		if (page.url.searchParams.toString() !== canonical) navigate(urlState, true);
	});

	/** The applied state under the box's current text (cancelling a pending write). */
	function withCurrentQuery(s: BrowseState): BrowseState {
		writeQuery.cancel();
		return withQuery(s, qInput);
	}
	/** Apply a discrete change (chip removal, Clear all): one pushed history entry. */
	function go(next: BrowseState) {
		navigate(withCurrentQuery(next));
	}
	/** A sort change, applied on top of the CURRENT query: the query decides what the
	 *  default sort is, so picking "Latest update" while a typed query is still waiting
	 *  out the debounce must not be normalised away as the empty-query default. */
	function sortBy(so: BrowseSortField) {
		navigate(withSort(withCurrentQuery(applied), so));
	}
	function flipDir() {
		navigate(toggleDir(withCurrentQuery(applied)));
	}

	function clearSearch() {
		qInput = '';
		writeQuery.cancel();
		navigate(withQuery(applied, ''));
	}

	// ---- sort -----------------------------------------------------------------------
	// Offered against the box's text, not the URL's, so Relevance appears as soon as
	// there is something to rank (choosing it carries the text — see `go`).
	const sortOpts = $derived(sortOptions(qInput));
	const sortValue = $derived(effectiveSort(applied));
	const dirValue = $derived(effectiveDir(applied));

	// ---- filter panel (draft) -------------------------------------------------------
	let filtersOpen = $state(false);
	// Seeded from the URL synchronously so the server-rendered panel matches the chips;
	// the effect below re-bases it on the APPLIED state once auth and tags are known.
	let draft = $state<BrowseState>(parseBrowseUrl(page.url.searchParams));
	let draftBase: string | null = null;
	// Re-base the draft only when the applied FILTERS change (a chip removed, Back, a
	// shared link) — not on a query or sort change, which would otherwise wipe edits the
	// viewer has pending in the rail every time they typed in the search box.
	$effect.pre(() => {
		const sig = filtersOnlySig(applied);
		if (sig === draftBase) return;
		draftBase = sig;
		draft = applied;
	});
	const pendingChanges = $derived(draftChanges(applied, draft));
	// Reset clears the DRAFT (Apply then commits it), so it is live exactly when the draft
	// has something to clear.
	const canReset = $derived(filtersOnlySig(draft) !== filtersOnlySig(clearFilters(draft)));

	function applyFilters() {
		const next = applyDraft(applied, draft, qInput);
		writeQuery.cancel();
		filtersOpen = false;
		navigate(next);
	}
	function resetDraft() {
		draft = clearFilters(draft);
	}
	/** Close the sheet WITHOUT applying: its edits are discarded, as a dismissed dialog's
	 *  should be — reopening shows what is actually applied. */
	function closeSheet() {
		filtersOpen = false;
		draft = applied;
	}

	// ---- rows -------------------------------------------------------------------------
	// Text and filters go to the public v2 search for everyone (filters now apply WITH a
	// query, so there is no longer a browse-only subset to disclose). Signed-in viewers can
	// additionally opt into the FEDERATED fan-out across every installed extension for one
	// query, to discover titles not yet in the catalogue; those rows are live extension
	// results with no filter arguments, narrowed client-side (`matchesFederated`).
	let rows = $state<FederatedResultView[]>([]);
	let rowsLoading = $state(true);
	let rowsError = $state(false);
	let rowsAreFederated = $state(false);
	/** False when the server predates `search(filter:)` — see `getBrowseSearch`. */
	let filterApplied = $state(true);
	let searchNotice = $state<string | null>(null);
	let reloadKey = $state(0);
	let hasNext = $state(false);
	let totalCount = $state<number | null>(null);

	// The federated opt-in is stored as THE QUERY IT WAS GRANTED FOR, not as a boolean, so
	// a new search term automatically returns to the fast local path — as a boolean it
	// once made every later keystroke-batch fan out to 24 sources and trip the rate limit.
	let federatedFor = $state<string | null>(null);
	const federatedRequested = $derived(federatedFor !== null && federatedFor === applied.q);
	const federatedPending = $derived(!!applied.q && !!auth.user && federatedRequested);
	const canSearchExternal = $derived(
		!!applied.q && !!auth.user && !rowsAreFederated && !federatedRequested,
	);

	// Signature of the currently-displayed result set, for the back-nav cache.
	let lastSig: string | null = null;
	// `reloadKey` at the time `lastSig` was set, so an explicit retry still refetches
	// even though its signature is unchanged.
	let lastReloadKey = -1;
	// Consult the cache on the FIRST effect run of a fresh mount (a back-nav into
	// Browse), never on user-driven changes — plus the popstate case below.
	let didInitFromCache = false;
	// A pager step or filter change is a SAME-ROUTE navigation, so browser-Back keeps the
	// component alive and leaves `didInitFromCache` true; `beforeNavigate` re-arms the
	// cache for exactly one run when the outgoing navigation is a popstate, so Back from
	// page 3 to page 2 restores rows AND scroll instead of flashing a skeleton.
	let restoreOnPop = false;

	$effect(() => {
		// Wait for `initAuth()` to settle: the first run would otherwise fetch as `anon`,
		// then refetch as the viewer a moment later — a skeleton flash on every cold load.
		if (!auth.ready) return;
		const s = applied;
		const q = s.q;
		const rk = reloadKey;
		const isFederated = !!q && !!auth.user && federatedRequested;
		// The key must capture EVERY input that changes the result set across a remount:
		// federated results are per-user, native ones are NSFW-gated per viewer, and both
		// survive login/logout in SPA memory. The native key is the canonical query string
		// (every filter, the sort and the page); the federated one is the query only — its
		// filters apply client-side and it has no pager.
		const idTag = `${auth.user?.id ?? 'anon'}:${auth.user?.showNsfw ? 1 : 0}`;
		const sig = isFederated ? `fed:${idTag}:${q}` : `nat:${idTag}:${toSearchParams(s)}`;

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
				filterApplied = cached.filterApplied;
				rowsError = false;
				searchNotice = null;
				rowsLoading = false;
				lastSig = sig;
				lastReloadKey = rk;
				const y = cached.scrollY;
				tick().then(() => requestAnimationFrame(() => window.scrollTo(0, y)));
				return;
			}
		}

		// Nothing about the result set changed (auth resolving to the same identity, the
		// tag vocabulary landing, the federated opt-in released after a rate limit) — keep
		// what is on screen rather than paying a round trip and a skeleton flash.
		if (sig === lastSig && rk === lastReloadKey && !rowsError) {
			rowsLoading = false;
			return;
		}

		rowsLoading = true;
		// The rate-limit branch KEEPS the previous rows on screen, so it must put the
		// previous pager envelope back with them.
		const prevPager = { hasNext, total: totalCount };
		hasNext = false;
		totalCount = null;
		let cancelled = false;
		const nameOf = (id: string) => tagName(id) ?? id;
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
							filterApplied = true;
							searchNotice = null;
							hasNext = false; // one deduped page, no server pager
							lastSig = sig;
							lastReloadKey = rk;
						} else if (outcome.kind === 'rateLimited') {
							searchNotice =
								outcome.retryAfter != null
									? `Too many searches — try again in ${outcome.retryAfter}s.`
									: 'Too many searches — try again in a moment.';
							hasNext = prevPager.hasNext;
							totalCount = prevPager.total;
							// Release the opt-in so the "Search external sources" button comes back
							// (it is the only way to retry). The re-run this triggers short-circuits on
							// the unchanged signature, so the prior rows and this notice survive.
							federatedFor = null;
						} else {
							// Not authenticated / error → the public search. Page 1 by construction:
							// the federated key has no page, so fetching another page here would cache
							// it under a page-free key.
							const r = await getBrowseSearch(withPageNum(s, 1), nameOf);
							if (cancelled) return;
							rows = r.items;
							rowsAreFederated = false;
							rowsError = r.error;
							filterApplied = r.filterApplied;
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
						const r = await getBrowseSearch(s, nameOf);
						if (cancelled) return;
						rows = r.items;
						rowsAreFederated = false;
						rowsError = r.error;
						filterApplied = r.filterApplied;
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
			// A short coalescing delay: a burst of navigations (Back mashed, a chip removed
			// right after Apply) collapses into one request.
			q ? 200 : 120,
		);
		return () => {
			cancelled = true;
			clearTimeout(t);
		};
	});

	// Snapshot the current result set + scroll into the module cache whenever we navigate
	// away (into a series, or onto another page / filter set), so a back-nav can restore it.
	beforeNavigate((nav) => {
		// Skip while a fetch is in flight: rows/lastSig still describe the PREVIOUS result
		// set while the pager envelope has already been reset, and the grid is a skeleton.
		if (lastSig != null && !rowsLoading && typeof window !== 'undefined') {
			putBrowseCache(lastSig, {
				rows,
				hasNext,
				totalCount,
				rowsAreFederated,
				filterApplied,
				scrollY: window.scrollY,
			});
		}
		restoreOnPop = nav.type === 'popstate';
	});

	// A shared, edited or stale link can name a page past the end (the server ECHOES the
	// requested page rather than clamping), which would render "No matches" over a
	// catalogue of thousands. Land on the last real page instead, with replaceState so
	// Back doesn't bounce onto the bad page again. BROWSE_PAGE_SIZE (30), not the feed's 20.
	$effect(() => {
		if (rowsLoading || rowsError || rowsAreFederated) return;
		if (rows.length > 0 || applied.page === 1) return;
		if (totalCount == null || totalCount === 0) return;
		const last = lastPage(totalCount, BROWSE_PAGE_SIZE);
		if (applied.page > last) navigate(withPageNum(applied, last), true);
	});

	function retryRows() {
		reloadKey++;
	}

	// Opt into the federated fan-out for the current query (signed-in only).
	function requestFederated() {
		if (!auth.user || federatedRequested) return;
		federatedFor = applied.q;
		// Federated results are one deduped page with no pager, so a lingering `?page=5`
		// would describe a pager that isn't rendered.
		if (applied.page !== 1) navigate(withPageNum(applied, 1), true);
	}

	// NATIVE rows are exactly what the server ordered and filtered. FEDERATED rows are
	// narrowed here by the facets they carry (tags by name, format, status).
	const results = $derived(
		rowsAreFederated ? rows.filter((r) => matchesFederated(r, applied, tagName)) : rows,
	);

	// The CATALOGUE-WIDE count (the per-page range lives in the Pager). Falls back to the
	// approximate "N+" form only when the total is genuinely unknown — a federated page,
	// an adapter that reports none, or a failed request.
	const countLabel = $derived.by(() => {
		if (rowsLoading)
			return applied.q ? (federatedPending ? 'Searching all sources…' : 'Searching…') : 'Loading…';
		// A failed request has no count — "0 results" over an error state reads as an
		// honest empty catalogue, which is exactly the confusion the error state prevents.
		if (rowsError) return '';
		if (totalCount != null)
			return `${totalCount.toLocaleString()} ${totalCount === 1 ? 'result' : 'results'}`;
		return `${results.length}${hasNext ? '+' : ''} ${results.length === 1 && !hasNext ? 'result' : 'results'}`;
	});

	// Against a server that predates the v2 filter only the legacy subset applied; name
	// the rest rather than present a wider result set under active chips.
	const unapplied = $derived(
		!rowsLoading && !rowsAreFederated && !filterApplied ? legacyUnapplied(applied) : [],
	);
	// External-source rows can only be narrowed by what they carry.
	const federatedIgnores = $derived(
		rowsAreFederated && chips.some((c) => !/^(tag[+-]|type|status):/.test(c.key)),
	);

	/**
	 * The card's second line. `m.ch` (how many chapters we hold) and `m.latestCh` (what
	 * the newest one is CALLED, pre-formatted, '' when unknown) are two different facts and
	 * independently absent — a ~67k-work tail has no chapters yet, and oneshots carry no
	 * number — so this builds a list instead of nesting ternaries, and only falls back to
	 * "No chapters yet" when BOTH are missing. Never "Ch. undefined", never a count under a
	 * "Ch." label.
	 */
	function cardSub(m: FederatedResultView): string {
		const parts = [m.ch > 0 ? `${m.ch} ch` : '', m.latestCh].filter(Boolean);
		const chapters = parts.length ? parts.join(' · ') : 'No chapters yet';
		return m.genre ? `${m.genre} · ${chapters}` : chapters;
	}

	const pageTitle = $derived(applied.q ? `${applied.q} · Browse · komiq` : 'Browse · komiq');
</script>

<svelte:head>
	<title>{pageTitle}</title>
</svelte:head>

<div class="head k-gutter">
	<h1>Browse</h1>
	<form
		class="searchbar"
		role="search"
		onsubmit={(e) => {
			e.preventDefault();
			writeQuery.flush();
		}}
	>
		<Icon name="search" size={20} />
		<input
			type="search"
			name="q"
			autocomplete="off"
			enterkeyhint="search"
			aria-label="Search series, authors and alternative titles"
			bind:value={qInput}
			oninput={() => writeQuery(qInput)}
			placeholder="Search series, authors…"
		/>
		{#if qInput}
			<button type="button" class="clear" aria-label="Clear search" onclick={clearSearch}
				><Icon name="x" size={18} /></button
			>
		{/if}
	</form>
</div>

<div class="body k-gutter">
	<FilterPanel
		open={filtersOpen}
		{draft}
		onchange={(next) => (draft = next)}
		tags={tagData.tags}
		tagSource={tagData.source}
		{tagsLoading}
		{nsfw}
		signedIn={!!auth.user}
		changes={pendingChanges}
		{canReset}
		onapply={applyFilters}
		onreset={resetDraft}
		onclose={closeSheet}
	/>

	<div class="results" id="browse-results" aria-busy={rowsLoading}>
		<div class="results-head">
			<div class="results-title">
				<h2 class="rt">{applied.q ? `“${applied.q}”` : 'All series'}</h2>
				<span class="rc" aria-live="polite">{countLabel}</span>
			</div>
			<div class="controls">
				<button
					type="button"
					class="filter-trigger"
					aria-haspopup="dialog"
					aria-expanded={filtersOpen}
					onclick={() => (filtersOpen = true)}
				>
					<Icon name="sliders-h" size={16} />Filters{#if chips.length}<span
							class="ft-badge"
							aria-hidden="true">{chips.length}</span
						><span class="sr-only">, {chips.length} active</span>{/if}
				</button>
				<SortMenu
					options={sortOpts}
					value={sortValue}
					dir={dirValue}
					onselect={sortBy}
					ontoggledir={flipDir}
				/>
			</div>
		</div>

		<ActiveFilters {chips} onapply={go} onclear={() => go(clearFilters(applied))} />

		{#if searchNotice}
			<div class="notice" role="status">
				<Icon name="alert" size={16} />
				<span>{searchNotice}</span>
			</div>
		{/if}
		{#if unapplied.length}
			<div class="notice" role="status">
				<Icon name="alert" size={16} />
				<span
					>This server doesn't support every filter yet, so these weren't applied: {unapplied.join(
						', ',
					)}.</span
				>
			</div>
		{/if}
		{#if federatedIgnores}
			<p class="scope-note">
				External-source results are matched live: only tags, format and status narrow them, and they
				keep the sources' own order.
			</p>
		{/if}

		{#if rowsLoading}
			<CardGridSkeleton count={12} />
		{:else if rowsError}
			<div class="empty">
				<div class="empty-icon error"><Icon name="alert" size={24} /></div>
				<h3 class="empty-title">Couldn't load results</h3>
				<p class="empty-desc">
					Something went wrong reaching the server. Check your connection and try again.
				</p>
				<button type="button" class="empty-btn" onclick={retryRows}>Retry</button>
			</div>
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
					<button type="button" class="ext-btn" onclick={requestFederated}>
						<Icon name="search" size={15} />Search external sources
					</button>
				</div>
			{/if}
		{:else if !searchNotice}
			<!-- Honest empties: with every filter resolved server-side, an empty page means the
			     filtered catalogue is genuinely empty — so say WHICH thing emptied it and offer
			     the one action that widens it. -->
			<div class="empty">
				<div class="empty-icon"><Icon name="search" size={24} /></div>
				{#if chips.length}
					<h3 class="empty-title">No series match these filters</h3>
					<p class="empty-desc">
						{applied.q
							? `Nothing matching “${applied.q}” passes every filter. Remove a filter or two, or clear them all.`
							: 'Remove a filter or two, or clear them all.'}
					</p>
					<div class="empty-actions">
						<button type="button" class="empty-btn" onclick={() => go(clearFilters(applied))}
							>Clear filters</button
						>
						{#if canSearchExternal}
							<button type="button" class="empty-btn secondary" onclick={requestFederated}
								>Search external sources</button
							>
						{/if}
					</div>
				{:else if applied.q}
					<h3 class="empty-title">No matches for “{applied.q}”</h3>
					<p class="empty-desc">
						{canSearchExternal
							? 'Nothing in the catalogue matches. Try searching external sources.'
							: 'Check the spelling, or try a shorter or alternative title.'}
					</p>
					{#if canSearchExternal}
						<button type="button" class="empty-btn" onclick={requestFederated}
							>Search external sources</button
						>
					{:else}
						<button type="button" class="empty-btn" onclick={clearSearch}>Clear search</button>
					{/if}
				{:else}
					<h3 class="empty-title">Nothing to browse yet</h3>
					<p class="empty-desc">The catalogue is empty right now. Check back soon.</p>
				{/if}
			</div>
		{/if}

		<!-- Native path only: the federated fan-out returns one deduped page with no server
		     pager. `Pager` hides itself on page 1 with no next page; past page 1 it stays
		     through a load (inert links) so the landmark and live region survive the click
		     that triggered it. `aria-controls` is deliberately not passed: the only candidate
		     region is `.results`, which CONTAINS this pager. -->
		{#if !rowsAreFederated}
			<Pager
				page={applied.page}
				{hasNext}
				total={totalCount}
				pageSize={BROWSE_PAGE_SIZE}
				count={results.length}
				loading={rowsLoading}
				href={(p) => withPage(page.url, p)}
				label="Browse results pages"
				scrollTarget="browse-results"
			/>
		{/if}
	</div>
</div>

<style>
	.head {
		padding-top: 40px;
	}
	h1 {
		font-size: 34px;
		margin: 0 0 18px;
		color: var(--k-text-bright);
	}
	.searchbar {
		display: flex;
		align-items: center;
		gap: 12px;
		max-width: 960px;
		height: 54px;
		padding: 0 8px 0 20px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-lg);
		color: var(--k-text-faint);
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
		width: 38px;
		height: 38px;
		flex: 0 0 auto;
		border: none;
		border-radius: var(--k-radius-pill);
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
	.body {
		display: grid;
		grid-template-columns: 300px minmax(0, 1fr);
		gap: 36px;
		padding-top: 28px;
		padding-bottom: 80px;
		align-items: start;
	}
	.results {
		/* Clears the sticky header when the pager aligns this column to the top of the
		   viewport on a page step. */
		scroll-margin-top: calc(var(--k-header-h) + 12px);
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 18px;
	}
	.results-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: 12px 20px;
		min-width: 0;
	}
	.results-title {
		display: flex;
		align-items: baseline;
		flex-wrap: wrap;
		gap: 4px 11px;
		min-width: 0;
	}
	.rt {
		margin: 0;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text);
		overflow-wrap: anywhere;
	}
	.rc {
		font-size: 13.5px;
		color: var(--k-text-faint);
		font-variant-numeric: tabular-nums;
	}
	.controls {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
	}
	.filter-trigger {
		display: none;
		align-items: center;
		gap: 7px;
		height: 36px;
		padding: 0 14px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		font-size: 13px;
		font-weight: 700;
		cursor: pointer;
		white-space: nowrap;
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
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		margin: -1px;
		padding: 0;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
		border: 0;
	}
	.notice {
		display: flex;
		align-items: flex-start;
		gap: 10px;
		padding: 12px 16px;
		border-radius: var(--k-radius-md);
		background: color-mix(in srgb, var(--k-hiatus) 12%, transparent);
		border: 1px solid color-mix(in srgb, var(--k-hiatus) 40%, transparent);
		color: var(--k-text-2);
		font-size: 13.5px;
		font-weight: 600;
		line-height: 1.45;
	}
	.notice :global(svg) {
		flex: 0 0 auto;
		margin-top: 1px;
		color: var(--k-hiatus);
	}
	.scope-note {
		margin: 0;
		font-size: 12.5px;
		line-height: 1.45;
		color: var(--k-text-faint);
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
		gap: 28px 20px;
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
		gap: 12px;
		padding: 72px 8px;
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
		border-color: color-mix(in srgb, var(--k-hiatus) 40%, transparent);
	}
	.empty-title {
		margin: 0;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text);
		overflow-wrap: anywhere;
	}
	.empty-desc {
		margin: 0;
		font-size: 14px;
		color: var(--k-text-dimmer);
		max-width: 360px;
		line-height: 1.5;
	}
	.empty-actions {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		gap: 10px;
	}
	.empty-btn {
		margin-top: 6px;
		height: 42px;
		padding: 0 22px;
		border-radius: var(--k-radius);
		background: var(--k-primary);
		border: 1px solid var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 13.5px;
		cursor: pointer;
	}
	.empty-btn.secondary {
		background: transparent;
		border-color: var(--k-border-3);
		color: var(--k-text);
	}

	/* Must match FilterPanel's SHEET_QUERY: below it the panel is a sheet, out of the
	   grid flow, and the toolbar grows a Filters button to open it. */
	@media (max-width: 959px) {
		.body {
			grid-template-columns: minmax(0, 1fr);
			gap: 0;
		}
		.filter-trigger {
			display: inline-flex;
		}
	}
	@media (max-width: 640px) {
		.head {
			padding-top: 24px;
		}
		h1 {
			font-size: 28px;
			margin-bottom: 14px;
		}
		.searchbar {
			height: 48px;
			padding-left: 14px;
		}
		.body {
			padding-top: 20px;
		}
		.results-head {
			/* Title + count on one line, controls on the next, both full width — the
			   controls never have to squeeze beside a long query title. */
			flex-direction: column;
			align-items: stretch;
		}
		.controls {
			justify-content: space-between;
		}
		.grid {
			grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
			gap: 22px 14px;
		}
	}
</style>
