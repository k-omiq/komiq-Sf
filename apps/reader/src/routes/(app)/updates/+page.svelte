<script lang="ts">
	import { page as pageState } from '$app/state';
	import { goto, invalidateAll } from '$app/navigation';
	import Icon from '$lib/components/Icon.svelte';
	import MangaCard from '$lib/components/MangaCard.svelte';
	import CardRowSkeleton from '$lib/components/CardRowSkeleton.svelte';
	import CardGridSkeleton from '$lib/components/CardGridSkeleton.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import { FEED_PAGE_SIZE, lastPage, withPage } from '$lib/data/paging';
	// The card's visible label is the CHAPTER RELEASE time; our detection time (when
	// the backend reports it) goes in the hover title — see cardTimeTooltip.
	import { FLAG, cardSub, cardTimeTooltip as timeTooltip, type ComicType } from '$lib/data/types';

	type UpdatesData = Awaited<ReturnType<typeof import('$lib/data/source').getUpdates>>;

	let { data } = $props();

	// `data.updates` is a RESOLVED object on the server (and on hydration) and a
	// pending Promise on client-side navigations — see +page.ts.
	//
	// The resolved case is a $derived, NOT an $effect: effects do not run during
	// SSR, so anything awaiting one renders as skeletons in the edge HTML and then
	// gets cached that way for the whole s-maxage window. Deriveds do evaluate on
	// the server, so this is what makes the edge serve real cards.
	const settled = $derived(
		data.updates instanceof Promise ? undefined : (data.updates as UpdatesData),
	);
	// Filled by the effect on client-side navigations only.
	//
	// The teardown CANCELS the pending promise, and it has to. Every pager step and facet
	// chip is a same-route navigation, so two of them in quick succession leave two
	// in-flight `getUpdates` calls; whichever RESOLVES last wins, not whichever was
	// requested last. Without this guard, paging 1 → 2 → 3 while page 2 is slow lands
	// page 2's twenty cards under a URL and a pager both reading page 3 — and the
	// over-range repair effect below then reasons about the wrong list. The home route's
	// identical block already guards this way (`(app)/+page.svelte`); this one didn't.
	let streamed = $state<UpdatesData | undefined>(undefined);
	$effect(() => {
		const u = data.updates;
		if (!(u instanceof Promise)) return;
		streamed = undefined;
		let alive = true;
		u.then((r) => {
			if (alive) streamed = r;
		});
		return () => {
			alive = false;
		};
	});

	const feeds = $derived(settled ?? streamed);
	const trendingGroups = $derived<UpdatesData['trendingGroups']>(feeds?.trendingGroups ?? []);
	const newUpdates = $derived<UpdatesData['newUpdates']>(feeds?.newUpdates ?? []);
	const hotUpdates = $derived<UpdatesData['hotUpdates']>(feeds?.hotUpdates ?? []);
	const loading = $derived(!feeds);

	// The facets and the page number are URL state, resolved once in `+page.ts` and read
	// back off `data` — NOT component `$state`. They have to be: the "New" list is paged
	// server-side, and a page number the server doesn't know about ("page 3 of what?")
	// can't be reproduced by a reload, a shared link or the Back button. Every control
	// below is therefore a LINK that writes the URL, not a button that mutates state.
	const tab = $derived(data.tab);
	const typeFilter = $derived(data.type);
	// Echoed from the server via `getUpdates` (the admin pager's rule: never assume the
	// page from the click), falling back to the requested page while the feed is in
	// flight so the pager doesn't flicker back to 1.
	const pageNum = $derived(feeds?.page ?? data.page);
	const hasNext = $derived(feeds?.hasNext ?? false);
	const total = $derived(feeds?.total ?? null);
	// A genuine backend failure, as distinct from an honest empty page — see
	// `UpdatesView.error`. Only ever true on the "New" tab.
	const feedError = $derived(feeds?.error ?? false);

	const TYPES: ComicType[] = ['Manga', 'Manhwa', 'Manhua'];

	const updates = $derived.by(() => {
		// "New" arrives already filtered, deduped, ordered and PAGED by the server, so
		// there is nothing left to do to it here — filtering a 20-row page client-side is
		// exactly the dishonesty the server `type` argument exists to avoid.
		if (tab !== 'hot') return newUpdates;
		// "Hot" is different on purpose: it is a curated top-N slice of the discovery
		// TRENDING feed with no server pagination anywhere in the stack, so the WHOLE
		// list is present and filtering it client-side is honest (and it gets no pager —
		// paging a top-N ranking is meaningless). Cards with no format only show under
		// "All", as before.
		return typeFilter ? hotUpdates.filter((u) => u.type === typeFilter) : hotUpdates;
	});

	/**
	 * A URL with one facet changed and `page` DROPPED.
	 *
	 * Dropping it is the point: page 4 of "New / All" is not page 4 of "New / Manhwa" or
	 * of "Hot", so carrying the number across a facet change would land the viewer on an
	 * arbitrary — often empty — page of a different list. Everything else in the query
	 * string is carried through untouched.
	 */
	function facetHref(next: { tab?: 'new' | 'hot'; type?: ComicType | null }): string {
		const sp = new URLSearchParams(pageState.url.searchParams);
		const t = next.tab ?? tab;
		const ty = next.type !== undefined ? next.type : typeFilter;
		// Defaults are omitted rather than written, so `/updates` stays the canonical URL
		// for the default view instead of being one of several spellings of it.
		if (t === 'hot') sp.set('tab', 'hot');
		else sp.delete('tab');
		if (ty) sp.set('type', ty);
		else sp.delete('type');
		sp.delete('page');
		const qs = sp.toString();
		return qs ? `${pageState.url.pathname}?${qs}` : pageState.url.pathname;
	}

	// A shared, edited or stale link can name a page past the end — the server ECHOES the
	// page it was asked for rather than clamping it — which would render the "No updates
	// here" empty state over a feed of tens of thousands of rows. Land on the last real
	// page instead. `replaceState`, so Back doesn't bounce the viewer onto the bad page.
	//
	// Deliberately NOT a page-reset effect: every facet control is a link that already
	// drops `page` itself, so there is no effect racing auth readiness to strip `?page=`
	// off a shared link before the viewer's own feed has even loaded (the failure Browse
	// hit). This repair only ever fires on a page that came back genuinely EMPTY with a
	// known non-zero total.
	$effect(() => {
		if (loading || pageNum === 1 || newUpdates.length > 0) return;
		if (tab === 'hot' || total == null || total === 0) return;
		const last = lastPage(total, FEED_PAGE_SIZE);
		if (pageNum > last) {
			void goto(withPage(pageState.url, last), {
				replaceState: true,
				noScroll: true,
				keepFocus: true,
			});
		}
	});
</script>

<div class="trending k-gutter">
	{#if loading}
		<section class="tgroup">
			<div class="k-skeleton head-sk"></div>
			<CardRowSkeleton />
		</section>
	{:else}
		{#each trendingGroups as tg (tg.label)}
			<section class="tgroup">
				<h2 class="section-label">{tg.label}</h2>
				<div class="marquee">
					{#each tg.items as item (item.id ?? item.title)}
						<MangaCard
							title={item.title}
							sub={cardSub(item)}
							subTitle={timeTooltip(item)}
							rating={item.rating}
							cover={item.cover}
							id={item.id}
							fixed
						/>
					{/each}
				</div>
			</section>
		{/each}
	{/if}
</div>

<div class="updates k-gutter">
	<div class="updates-head">
		<h2 class="section-label">Updates</h2>
		<!-- Tabs and format chips are LINKS, not buttons: the facet is URL state now, so a
		     link is the honest control — shareable, middle/ctrl-clickable, crawlable — and
		     SvelteKit still intercepts the click for a client-side navigation. Each href
		     drops `page` (see facetHref). -->
		<nav class="tabs" aria-label="Updates list">
			<a
				class="tab"
				class:on={tab === 'new'}
				href={facetHref({ tab: 'new' })}
				aria-current={tab === 'new' ? 'page' : undefined}>New</a
			>
			<a
				class="tab"
				class:on={tab === 'hot'}
				href={facetHref({ tab: 'hot' })}
				aria-current={tab === 'hot' ? 'page' : undefined}
			>
				<Icon name="flame" size={13} />Hot
			</a>
		</nav>
	</div>

	<nav class="type-tabs" aria-label="Format">
		<a
			class="ttab"
			class:on={typeFilter === null}
			href={facetHref({ type: null })}
			aria-current={typeFilter === null ? 'page' : undefined}>All</a
		>
		{#each TYPES as t (t)}
			<a
				class="ttab"
				class:on={typeFilter === t}
				href={facetHref({ type: t })}
				title={t}
				aria-label={t}
				aria-current={typeFilter === t ? 'page' : undefined}
			>
				<span class="flag" aria-hidden="true">{FLAG[t]}</span>
			</a>
		{/each}
	</nav>

	{#if loading}
		<!-- One skeleton per row of a full page, not 12: on a Back navigation this route
		     STREAMS (see +page.ts), so SvelteKit restores the scroll position against
		     whatever is on screen at that moment. A skeleton grid the same height as the
		     real page keeps that restoration close instead of clamping it to a short page. -->
		<CardGridSkeleton count={FEED_PAGE_SIZE} min={150} />
	{:else if updates.length}
		<div class="grid">
			<!-- Key on the id, falling back to the title. The "New" list's ids are unique
			     by construction now — the server materializes ONE row per canonical work,
			     so the union can no longer carry a series under both its numeric Suwayomi
			     and its `w_` mirror identity. That matters: a duplicate key is not
			     cosmetic, Svelte 5 throws `each_key_duplicate` in production and the page
			     dies in the error boundary. "Hot" still leans on dedupeCardsByTitle. -->
			{#each updates as item (item.id ?? item.title)}
				<MangaCard
					title={item.title}
					sub={cardSub(item)}
					subTitle={timeTooltip(item)}
					rating={item.rating}
					cover={item.cover}
					id={item.id}
					flagEmoji={item.type ? FLAG[item.type] : ''}
				/>
			{/each}
		</div>
	{:else if feedError}
		<!-- The feed request FAILED. Must come before the empty state: an outage and a
		     filter that matches nothing both leave `updates` empty, and the empty state's
		     advice ("try another format or tab") cannot work while the backend is down. -->
		<div class="empty">
			<div class="empty-icon error"><Icon name="alert" size={22} /></div>
			<div class="empty-title">Couldn't load updates</div>
			<div class="empty-desc">
				Something went wrong reaching the server. Check your connection and try again.
			</div>
			<button class="empty-btn" onclick={() => void invalidateAll()}>Retry</button>
		</div>
	{:else}
		<div class="empty">
			<div class="empty-icon"><Icon name="flame" size={22} /></div>
			<div class="empty-title">No updates here</div>
			<div class="empty-desc">
				Nothing matches this filter right now — try another format or tab.
			</div>
			<!-- A link to the bare route, so it clears tab, format AND page in one step and
			     lands on the canonical URL. -->
			<a class="empty-btn" href="/updates">Reset filters</a>
		</div>
	{/if}

	<!-- "New" only. "Hot" is a curated top-N ranking of the discovery TRENDING feed with
	     no server pagination anywhere in the stack — there is no page 2 to link to, and a
	     pager over a top-N would be a lie. Please don't "fix" the missing pager here.
	     `Pager` renders nothing at all on page 1 with no next page, so a feed that fits
	     in one page shows no pager. -->
	{#if tab === 'new'}
		<Pager
			page={pageNum}
			{hasNext}
			{total}
			pageSize={FEED_PAGE_SIZE}
			count={updates.length}
			{loading}
			href={(p) => withPage(pageState.url, p)}
			label="Updates pages"
		/>
	{/if}
</div>

<style>
	.trending {
		display: flex;
		flex-direction: column;
		gap: 48px;
		padding-top: 56px;
	}
	.section-label {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 16px;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		margin: 0;
		color: var(--k-text-dimmer);
	}
	.tgroup {
		display: flex;
		flex-direction: column;
		gap: 20px;
	}
	.marquee {
		display: flex;
		gap: 22px;
		overflow-x: auto;
		padding-bottom: 4px;
	}
	.updates {
		display: flex;
		flex-direction: column;
		gap: 20px;
		padding-top: 48px;
		padding-bottom: 80px;
		margin-top: 48px;
		border-top: 1px solid var(--k-border);
	}
	.updates-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 20px;
	}
	.tabs {
		display: flex;
		gap: 8px;
	}
	/* `.tab` / `.ttab` / `.empty-btn` are anchors now (the facet is URL state), so they
	   need the type reset a <button> gave for free: no underline, inherited font. */
	.tab {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 13px;
		font-weight: 700;
		padding: 7px 15px;
		border-radius: var(--k-radius-pill);
		cursor: pointer;
		background: transparent;
		color: var(--k-text-dimmer);
		border: 1px solid var(--k-border-4);
		text-decoration: none;
		transition: all 0.15s;
	}
	.tab:focus-visible,
	.ttab:focus-visible,
	.empty-btn:focus-visible {
		outline: 2px solid var(--k-primary);
		outline-offset: 2px;
	}
	.tab.on {
		background: var(--k-primary);
		color: var(--k-on-primary);
		border-color: var(--k-primary);
	}
	.type-tabs {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.ttab {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		font-size: 12.5px;
		font-weight: 700;
		padding: 7px 14px;
		border-radius: var(--k-radius-pill);
		cursor: pointer;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-dimmer);
		text-decoration: none;
		transition: all 0.15s;
	}
	.ttab.on {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.flag {
		font-size: 17px;
		line-height: 1;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
		gap: 24px;
	}
	.head-sk {
		width: 180px;
		height: 20px;
		border-radius: 6px;
	}
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		padding: 72px 20px;
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
	/* Outage variant, matching Browse's `.empty-icon.error` so the two screens read the
	   same way when the backend is down. */
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
		display: inline-flex;
		align-items: center;
		justify-content: center;
		height: 42px;
		padding: 0 22px;
		border-radius: 8px;
		background: var(--k-primary);
		border: none;
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 13.5px;
		text-decoration: none;
		cursor: pointer;
	}
</style>
