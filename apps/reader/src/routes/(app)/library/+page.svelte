<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { invalidate, replaceState } from '$app/navigation';
	import { page } from '$app/state';
	import Icon from '$lib/components/Icon.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import CardGridSkeleton from '$lib/components/CardGridSkeleton.svelte';
	import EmptyCatalog from '$lib/components/EmptyCatalog.svelte';
	import Dropdown, { type DropdownOption } from '$lib/components/Dropdown.svelte';
	import FlagBadge from '$lib/components/FlagBadge.svelte';
	import Ratings from '$lib/components/Ratings.svelte';
	import StatusMenu from '$lib/components/StatusMenu.svelte';
	import {
		setLibraryStatus,
		setFavorite,
		type ContinueRowView,
		type LibraryRowView,
	} from '$lib/data/source';
	import { slug, type ComicType, type Shelf } from '$lib/data/types';
	import { sortLibraryRows } from '$lib/data/library-order';
	import { catalogIsEmpty, checksCatalogEmpty } from '$lib/data/catalog-empty';

	let { data } = $props();
	// Refresh releases and progress when returning to a tab left open during a scan.
	onMount(() => {
		let lastRefresh = Date.now();
		const refresh = () => {
			if (document.visibilityState !== 'visible' || Date.now() - lastRefresh < 30_000) return;
			lastRefresh = Date.now();
			void invalidate('komika:library');
		};
		window.addEventListener('focus', refresh);
		document.addEventListener('visibilitychange', refresh);
		return () => {
			window.removeEventListener('focus', refresh);
			document.removeEventListener('visibilitychange', refresh);
		};
	});
	// `data.library` is a PROMISE (see +page.ts): awaiting it in `load` blocked the
	// navigation with zero feedback. Copy the catalogue into local state once it
	// lands, so shelf/favourite edits reflect immediately (optimistic) and re-sync
	// whenever `load` re-runs.
	let rows = $state<LibraryRowView[]>([]);
	let continueRow = $state<ContinueRowView[]>([]);
	let loading = $state(true);
	$effect(() => {
		const p = data.library;
		loading = true;
		let cancelled = false;
		p.then((lib) => {
			if (cancelled) return;
			rows = lib.libraryCatalog.map((r) => ({ ...r }));
			continueRow = lib.continueRow;
			loading = false;
		});
		return () => {
			cancelled = true;
		};
	});

	type ShelfKey = 'all' | Shelf | 'favorites';
	type SortKey = 'updated' | 'read' | 'added' | 'alpha' | 'unread';
	type Dir = 'asc' | 'desc';
	type FormatKey = 'all' | ComicType;

	const shelfDefs: readonly { key: ShelfKey; label: string }[] = [
		{ key: 'all', label: 'All' },
		{ key: 'reading', label: 'Reading' },
		{ key: 'completed', label: 'Completed' },
		{ key: 'onhold', label: 'On Hold' },
		{ key: 'plan', label: 'Plan to Read' },
		{ key: 'favorites', label: 'Favorites' },
	];
	const sortOptions: readonly DropdownOption<SortKey>[] = [
		{ value: 'updated', label: 'Latest update' },
		{ value: 'read', label: 'Recently read' },
		{ value: 'added', label: 'Recently added' },
		{ value: 'alpha', label: 'Title' },
		{ value: 'unread', label: 'Unread chapters' },
	];
	const FORMATS: readonly ComicType[] = ['Manga', 'Manhwa', 'Manhua'];
	// Each sort has a "natural" direction — the one a reader expects on first pick —
	// and the order toggle names both ends in that sort's own words.
	const NATURAL_DIR: Record<SortKey, Dir> = {
		updated: 'desc',
		read: 'desc',
		added: 'desc',
		alpha: 'asc',
		unread: 'desc',
	};
	const DIR_LABEL: Record<SortKey, Record<Dir, string>> = {
		updated: { desc: 'Newest first', asc: 'Oldest first' },
		read: { desc: 'Newest first', asc: 'Oldest first' },
		added: { desc: 'Newest first', asc: 'Oldest first' },
		alpha: { asc: 'A–Z', desc: 'Z–A' },
		unread: { desc: 'Most first', asc: 'Fewest first' },
	};

	// Shelf, sort, order and format live in the query string so a refresh or a shared
	// link keeps the view. Defaults are written as ABSENCE (a bare /library), and the
	// title filter stays local — it changes on every keystroke.
	function param<K extends string>(name: string, allowed: readonly K[], fallback: K): K {
		const v = page.url.searchParams.get(name);
		return v && (allowed as readonly string[]).includes(v) ? (v as K) : fallback;
	}
	let shelf = $state<ShelfKey>(
		param(
			'shelf',
			shelfDefs.map((d) => d.key),
			'all',
		),
	);
	// Default is 'updated' — a library's most useful question is "what has new chapters?".
	let sort = $state<SortKey>(
		param(
			'sort',
			sortOptions.map((o) => o.value),
			'updated',
		),
	);
	let dir = $state<Dir>(param<Dir>('dir', ['asc', 'desc'], NATURAL_DIR[untrack(() => sort)]));
	let format = $state<FormatKey>(param<FormatKey>('format', ['all', ...FORMATS], 'all'));
	let query = $state('');

	$effect(() => {
		const url = new URL(location.href);
		const sp = url.searchParams;
		const put = (k: string, v: string | null) => (v === null ? sp.delete(k) : sp.set(k, v));
		put('shelf', shelf === 'all' ? null : shelf);
		put('sort', sort === 'updated' ? null : sort);
		put('dir', dir === NATURAL_DIR[sort] ? null : dir);
		put('format', format === 'all' ? null : format);
		if (url.search === location.search) return;
		// Shallow: `load` never reads the URL, so there is nothing to re-run. Only an
		// invalid param can make this differ on the very first run, before the router
		// may be ready — tidying that URL is cosmetic, so a throw there is ignored.
		untrack(() => {
			try {
				replaceState(url, page.state);
			} catch {
				/* router not initialised yet */
			}
		});
	});

	function setSort(next: SortKey): void {
		sort = next;
		dir = NATURAL_DIR[next];
	}

	// An empty library on a server that has no series at all (a new self-hosted
	// install) says so, instead of sending the reader to an empty Browse.
	// `null` = still asking (self-host builds only; see $lib/data/catalog-empty).
	let catalogEmpty = $state<boolean | null>(checksCatalogEmpty ? null : false);
	$effect(() => {
		if (loading || rows.length) return;
		let cancelled = false;
		void catalogIsEmpty().then((empty) => {
			if (!cancelled) catalogEmpty = empty;
		});
		return () => {
			cancelled = true;
		};
	});

	const counts = $derived({
		all: rows.length,
		reading: rows.filter((c) => c.shelf === 'reading').length,
		completed: rows.filter((c) => c.shelf === 'completed').length,
		onhold: rows.filter((c) => c.shelf === 'onhold').length,
		plan: rows.filter((c) => c.shelf === 'plan').length,
		favorites: rows.filter((c) => c.favorite).length,
	});

	const onShelf = $derived(
		shelf === 'all'
			? rows
			: shelf === 'favorites'
				? rows.filter((c) => c.favorite)
				: rows.filter((c) => c.shelf === shelf),
	);
	const formatOptions = $derived<DropdownOption<FormatKey>[]>([
		{ value: 'all', label: 'All formats', hint: onShelf.length },
		...FORMATS.map((f) => ({
			value: f,
			label: f,
			hint: onShelf.filter((c) => c.type === f).length,
		})),
	]);
	const needle = $derived(query.trim().toLowerCase());
	const filtersActive = $derived(format !== 'all' || needle !== '');

	const items = $derived.by(() => {
		let list = onShelf;
		if (format !== 'all') list = list.filter((c) => c.type === format);
		if (needle) list = list.filter((c) => c.title.toLowerCase().includes(needle));
		return sortLibraryRows(list, sort, dir).map((c) => {
			const progress = c.total ? Math.round((c.read / c.total) * 100) : 0;
			let sub: string;
			if (c.shelf === 'completed') sub = `Completed · ${c.total} ch`;
			else if (c.shelf === 'plan') sub = c.genre ? `${c.genre} · ${c.total} ch` : `${c.total} ch`;
			// Read every chapter of a series that is STILL PUBLISHING. The shelf stays
			// "Reading" (see `deriveShelf`) because the series isn't finished, so the
			// sub-line names the state that actually holds rather than leaving a
			// full-looking "Ch. 117 / 117" to read as an unlabelled "Completed".
			else if (c.total > 0 && c.read >= c.total) sub = `Caught up · ${c.total} ch`;
			else sub = `Ch. ${c.read} / ${c.total}`;
			return {
				...c,
				progress,
				sub,
				showBar: c.shelf === 'reading' || c.shelf === 'onhold',
			};
		});
	});

	function clearFilters(): void {
		format = 'all';
		query = '';
	}

	// Pick a shelf and keep its tab visible inside the horizontally scrolling strip.
	function selectShelf(key: ShelfKey, el: HTMLElement): void {
		shelf = key;
		el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
	}

	// Optimistically re-shelve / (un)favourite, persisting via the backend. A FAILED
	// write now rolls the card back and says why — the data layer used to resolve to
	// the optimistic value, so an expired session silently did nothing.
	//
	// `busy` is a per-series in-flight guard. Without it a double-click fired two
	// mutations whose completion order — not the reader's last click — decided the
	// stored shelf. (The series page already guarded this; the library didn't.)
	let busy = $state<Record<string, boolean>>({});
	let writeError = $state('');

	async function changeShelf(id: string | undefined, next: Shelf): Promise<void> {
		if (!id || busy[id]) return;
		const row = rows.find((r) => r.id === id);
		if (!row || row.shelf === next) return;
		const prev = row.shelf;
		row.shelf = next; // optimistic
		busy = { ...busy, [id]: true };
		writeError = '';
		try {
			const r = await setLibraryStatus(id, next, prev);
			row.shelf = r.value ?? prev;
			if (!r.ok) writeError = r.error ?? '';
		} finally {
			busy = { ...busy, [id]: false };
		}
	}

	async function toggleFavorite(id: string | undefined, current: boolean): Promise<void> {
		if (!id || busy[id]) return;
		const row = rows.find((r) => r.id === id);
		if (!row) return;
		row.favorite = !current; // optimistic
		busy = { ...busy, [id]: true };
		writeError = '';
		try {
			const r = await setFavorite(id, !current, current);
			row.favorite = r.value;
			if (!r.ok) writeError = r.error ?? '';
		} finally {
			busy = { ...busy, [id]: false };
		}
	}
</script>

<svelte:head>
	<title>Library · komiq</title>
</svelte:head>

<div class="head k-gutter">
	<h1>Your Library</h1>
	<div class="subtitle">
		{#if loading}Loading your shelves…{:else}{counts.all} series · {counts.reading} in progress{/if}
	</div>
</div>

{#if writeError}
	<div class="k-gutter">
		<div class="write-error" role="alert">
			<Icon name="alert" size={15} />
			<span>{writeError}</span>
		</div>
	</div>
{/if}

{#if continueRow.length}
	<section class="continue k-gutter">
		<h2 class="section-label">Continue Reading</h2>
		<div class="continue-row">
			{#each continueRow as item (item.id ?? item.title)}
				<a class="continue-card" href={`/series/${item.id ?? slug(item.title)}`}>
					<div class="landscape k-cover">
						<!-- Deliberately NOT sizing="card": `.landscape` is 300x168, twice the ~150px
						     box the card derivative is cut for, so a 1x viewer would get a 192px
						     image stretched to 300px. The master is the right source here. -->
						<Cover seriesId={item.id} src={item.cover} alt={item.title} loading="lazy" />
						<span class="resume"><Icon name="play" size={9} fill="currentColor" />Resume</span>
						<div class="bar"><div class="fill" style="width:{item.progress}%"></div></div>
					</div>
					<div class="continue-meta">
						<span class="ctitle">{item.title}</span>
						<span class="cch">{item.ch}</span>
					</div>
					<div class="csub">{item.progress}%{item.genre ? ` · ${item.genre}` : ''}</div>
				</a>
			{/each}
		</div>
	</section>
{/if}

<div class="shelfbar k-gutter">
	<div class="shelf-scroll">
		<div class="shelf-tabs" role="group" aria-label="Shelves">
			{#each shelfDefs as d (d.key)}
				<button
					type="button"
					class="shelf-tab k-hit"
					class:on={shelf === d.key}
					aria-pressed={shelf === d.key}
					onclick={(e) => selectShelf(d.key, e.currentTarget)}
				>
					<span>{d.label}</span>
					<span class="count">{loading ? '–' : counts[d.key]}</span>
				</button>
			{/each}
		</div>
	</div>

	<div class="toolbar">
		<label class="search">
			<span class="search-icon"><Icon name="search" size={16} /></span>
			<input
				type="search"
				placeholder="Filter by title"
				aria-label="Filter library by title"
				autocomplete="off"
				spellcheck="false"
				bind:value={query}
			/>
		</label>
		<div class="controls">
			<Dropdown
				class="format-dd"
				label="Format"
				options={formatOptions}
				value={format}
				onchange={(v) => (format = v)}
			/>
			<div class="sort-group">
				<Dropdown
					class="sort-dd"
					label="Sort by"
					prefix="Sort:"
					align="right"
					options={sortOptions}
					value={sort}
					onchange={setSort}
				/>
				<button
					type="button"
					class="dir-btn k-hit"
					aria-label={`Order: ${DIR_LABEL[sort][dir]}`}
					title={`Order: ${DIR_LABEL[sort][dir]} — switch to ${DIR_LABEL[sort][dir === 'asc' ? 'desc' : 'asc']}`}
					onclick={() => (dir = dir === 'asc' ? 'desc' : 'asc')}
				>
					<svg
						class="dir-icon"
						class:asc={dir === 'asc'}
						viewBox="0 0 24 24"
						width="16"
						height="16"
						fill="none"
						stroke="currentColor"
						stroke-width="2"
						stroke-linecap="round"
						stroke-linejoin="round"
						aria-hidden="true"><path d="M12 5v14M6 13l6 6 6-6" /></svg
					>
					<span class="dir-label">{DIR_LABEL[sort][dir]}</span>
				</button>
			</div>
		</div>
	</div>

	{#if !loading && counts.all > 0}
		<div class="resultline" aria-live="polite">
			<span>
				{#if filtersActive}
					{items.length} of {onShelf.length} series
				{:else}
					{items.length} series
				{/if}
			</span>
			{#if filtersActive}
				<button type="button" class="clear-btn k-hit" onclick={clearFilters}>
					<Icon name="x" size={13} strokeWidth={2.4} />Clear filters
				</button>
			{/if}
		</div>
	{/if}
</div>

<div class="grid-wrap k-gutter">
	{#if loading}
		<CardGridSkeleton count={12} />
	{:else if items.length}
		<div class="grid" class:large={items.length > 50}>
			<!-- The fallback key must NOT contain `shelf`: it's mutable, so re-shelving a
			     card changed its key, destroying and recreating the whole card — including
			     the <img> — which visibly re-flashed the cover on every shelf change. -->
			{#each items as item (item.id ?? item.title)}
				{@const href = `/series/${item.id ?? slug(item.title)}`}
				<div class="lib-card">
					<div class="cover k-cover">
						<Cover
							seriesId={item.id}
							src={item.cover}
							alt={item.title}
							loading="lazy"
							sizing="card"
						/>
						<a class="cover-link" {href} aria-label={item.title}></a>
						<span class="flag-svg"><FlagBadge type={item.type} /></span>
						<span class="rating"><Ratings own={item.rating} md={item.mdRating} chip stack /></span>
						<button
							class="fav-chip k-hit"
							class:on={item.favorite}
							aria-pressed={item.favorite}
							aria-label={item.favorite ? 'Remove from favourites' : 'Add to favourites'}
							title={item.favorite ? 'Favourited' : 'Add to favourites'}
							disabled={!!(item.id && busy[item.id])}
							onclick={() => toggleFavorite(item.id, item.favorite)}
						>
							<Icon name="heart" size={14} fill={item.favorite ? 'currentColor' : 'none'} />
						</button>
						<div class="status-slot">
							<StatusMenu
								status={item.shelf}
								disabled={!!(item.id && busy[item.id])}
								onchange={(s) => changeShelf(item.id, s)}
								canComplete={item.ended}
							/>
						</div>
						{#if item.showBar}
							<div class="progressbar"><div class="fill" style="width:{item.progress}%"></div></div>
						{/if}
					</div>
					<div class="lib-meta">
						<a class="lib-title" {href} title={item.title}>{item.title}</a>
						<div class="lib-sub">{item.sub}</div>
					</div>
				</div>
			{/each}
		</div>
	{:else if filtersActive && onShelf.length}
		<div class="empty">
			<div class="empty-icon"><Icon name="search" size={22} /></div>
			<div class="empty-title">No matches on this shelf</div>
			<div class="empty-desc">
				Nothing here matches{needle ? ` “${query.trim()}”` : ''}{format !== 'all'
					? ` in ${format}`
					: ''}. Try a different title or format.
			</div>
			<button type="button" class="empty-btn k-hit" onclick={clearFilters}>Clear filters</button>
		</div>
	{:else if counts.all === 0 && catalogEmpty !== false}
		{#if catalogEmpty}<EmptyCatalog heading="h2" />{/if}
	{:else}
		<div class="empty">
			<div class="empty-icon"><Icon name="bookmark" size={24} /></div>
			<div class="empty-title">
				{counts.all === 0 ? 'Your library is empty' : 'Nothing on this shelf yet'}
			</div>
			<div class="empty-desc">
				{counts.all === 0
					? 'Add series to your library and they’ll show up here, sorted by shelf.'
					: 'Move a series onto this shelf, or browse for something new to read.'}
			</div>
			<a class="empty-btn k-hit" href="/browse">Browse series</a>
		</div>
	{/if}
</div>

<style>
	.head {
		padding-top: 52px;
	}
	h1 {
		font-size: 40px;
		line-height: 1.1;
		margin: 0 0 6px;
		color: var(--k-text-bright);
	}
	.subtitle {
		font-size: 14px;
		color: var(--k-text-dimmer);
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

	/* ---- continue reading ---- */
	.continue {
		display: flex;
		flex-direction: column;
		gap: 18px;
		padding-top: 40px;
	}
	.continue-row {
		display: flex;
		gap: 20px;
		overflow-x: auto;
		padding-bottom: 4px;
		scroll-snap-type: x proximity;
		scrollbar-width: thin;
	}
	.continue-card {
		flex: 0 0 auto;
		width: 300px;
		text-decoration: none;
		scroll-snap-align: start;
	}
	.landscape {
		position: relative;
		width: 100%;
		aspect-ratio: 300 / 168;
		border-radius: 10px;
		transition: border-color 0.2s;
	}
	/* Round the cover art to match its container without `overflow:hidden`, which
	   would clip the StatusMenu dropdown that escapes below the grid cover box. */
	.landscape :global(.cover-img),
	.landscape :global(.cover-ph) {
		border-radius: 10px;
	}
	.cover :global(.cover-img),
	.cover :global(.cover-ph) {
		border-radius: 8px;
	}
	.continue-card:hover .landscape {
		border-color: var(--k-border-strong);
	}
	.resume {
		position: absolute;
		left: 12px;
		top: 11px;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		background: rgba(12, 12, 13, 0.68);
		padding: 4px 9px;
		border-radius: 6px;
		font-size: 11px;
		font-weight: 700;
		color: #f2f1ee;
	}
	.bar {
		position: absolute;
		left: 0;
		right: 0;
		bottom: 0;
		height: 4px;
		background: rgba(255, 255, 255, 0.12);
	}
	.bar .fill,
	.progressbar .fill {
		height: 100%;
		background: var(--k-primary);
	}
	.continue-meta {
		margin-top: 11px;
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 10px;
	}
	.ctitle {
		min-width: 0;
		font-weight: 600;
		font-size: 14.5px;
		color: var(--k-text-1);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.cch {
		flex: 0 0 auto;
		font-size: 12px;
		color: var(--k-text-faint);
	}
	.csub {
		font-size: 12px;
		color: var(--k-text-fainter);
		margin-top: 3px;
	}

	/* ---- shelves + toolbar ---- */
	.shelfbar {
		padding-top: 32px;
		margin-top: 48px;
		border-top: 1px solid var(--k-border);
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	/* The strip scrolls sideways instead of wrapping into a ragged second row.
	   Sideways only: `overflow-x: auto` alone makes the other axis `auto` too, and the
	   tabs' 44px `.k-hit` area is taller than the 40px tab, so the strip could be
	   dragged up and down. The padding (cancelled by the margin) keeps that hit area
	   and the focus ring inside the box instead of clipped. */
	.shelf-scroll {
		min-width: 0;
		margin: -4px;
		padding: 4px;
		overflow-x: auto;
		overflow-y: hidden;
		scrollbar-width: none;
		overscroll-behavior-x: contain;
	}
	.shelf-scroll::-webkit-scrollbar {
		display: none;
	}
	.shelf-tabs {
		display: flex;
		gap: 8px;
		width: max-content;
	}
	.shelf-tab {
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		gap: 8px;
		height: 40px;
		padding: 0 8px 0 16px;
		font: inherit;
		font-size: 13.5px;
		font-weight: 700;
		white-space: nowrap;
		border-radius: var(--k-radius-pill);
		cursor: pointer;
		background: transparent;
		color: var(--k-text-dimmer);
		border: 1px solid var(--k-border-4);
		transition:
			color 0.15s,
			background-color 0.15s,
			border-color 0.15s;
	}
	.shelf-tab:hover:not(.on) {
		color: var(--k-text);
		border-color: var(--k-border-strong);
	}
	.shelf-tab:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.shelf-tab.on {
		background: var(--k-primary);
		color: var(--k-on-primary);
		border-color: var(--k-primary);
	}
	.count {
		min-width: 24px;
		height: 24px;
		padding: 0 7px;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		color: var(--k-text-faint);
		font-size: 11.5px;
		font-weight: 700;
		font-variant-numeric: tabular-nums;
	}
	.shelf-tab.on .count {
		background: color-mix(in srgb, var(--k-on-primary) 12%, transparent);
		color: var(--k-on-primary);
	}

	.toolbar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
	}
	.search {
		position: relative;
		flex: 0 1 320px;
		min-width: 0;
		display: flex;
		align-items: center;
	}
	.search-icon {
		position: absolute;
		left: 14px;
		display: inline-flex;
		color: var(--k-text-faint);
		pointer-events: none;
	}
	.search input {
		width: 100%;
		height: 40px;
		padding: 0 14px 0 40px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-2);
		color: var(--k-text);
		font: inherit;
		font-size: 13.5px;
		font-weight: 600;
		outline: none;
		transition: border-color 0.15s;
	}
	.search input::placeholder {
		color: var(--k-text-faint);
		font-weight: 500;
	}
	.search input:hover {
		border-color: var(--k-border-4);
	}
	.search input:focus-visible {
		border-color: var(--k-border-strong);
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.controls {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
	}
	.sort-group {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
	}
	.dir-btn {
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 7px;
		height: 40px;
		min-width: 40px;
		padding: 0 13px 0 11px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-2);
		font: inherit;
		font-size: 13px;
		font-weight: 700;
		white-space: nowrap;
		cursor: pointer;
		transition:
			border-color 0.15s,
			color 0.15s;
	}
	.dir-btn:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.dir-btn:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.dir-icon {
		flex: 0 0 auto;
		color: var(--k-text-faint);
		transition: transform 0.18s;
	}
	.dir-icon.asc {
		transform: rotate(180deg);
	}
	.resultline {
		display: flex;
		align-items: center;
		gap: 12px;
		min-height: 24px;
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-faint);
		font-variant-numeric: tabular-nums;
	}
	.clear-btn {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		padding: 4px 2px;
		border: none;
		background: none;
		color: var(--k-text-2);
		font: inherit;
		font-weight: 700;
		cursor: pointer;
		text-decoration: underline;
		text-decoration-color: var(--k-border-strong);
		text-underline-offset: 3px;
	}
	.clear-btn:hover {
		color: var(--k-text-bright);
	}

	/* ---- grid ---- */
	.grid-wrap {
		padding-top: 20px;
		padding-bottom: 80px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
		gap: 28px 24px;
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
	.empty-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text);
	}
	.empty-desc {
		font-size: 14px;
		color: var(--k-text-dimmer);
		max-width: 340px;
		line-height: 1.5;
		overflow-wrap: anywhere;
	}
	.empty-btn {
		margin-top: 6px;
		height: 42px;
		padding: 0 22px;
		display: inline-flex;
		align-items: center;
		border-radius: 8px;
		background: var(--k-primary);
		border: none;
		color: var(--k-on-primary);
		font: inherit;
		font-weight: 700;
		font-size: 13.5px;
		text-decoration: none;
		cursor: pointer;
	}
	/* Covers have a definite aspect ratio, so skipping offscreen artwork never
	   guesses card height. Open shelf menus opt out of paint containment. */
	.grid.large .cover {
		content-visibility: auto;
	}
	.grid.large .cover:has(:global(.sm.open)) {
		content-visibility: visible;
	}
	.lib-card:has(:global(.sm.open)) {
		position: relative;
		z-index: 5;
	}
	.lib-card {
		display: flex;
		flex-direction: column;
		gap: 10px;
		min-width: 0;
	}
	.cover {
		position: relative;
		width: 100%;
		aspect-ratio: 2 / 3;
		border-radius: 8px;
		transition: opacity 0.15s;
	}
	/* The `.k-cover` utility sets `overflow: hidden`, which clips the StatusMenu
	   dropdown that opens outside the cover box. Re-allow overflow here — the cover
	   art is rounded on the <img> itself, so nothing else needs the clip. */
	.cover.k-cover {
		overflow: visible;
	}
	/* Stretched navigation target — fills the cover, sits under the controls. */
	.cover-link {
		position: absolute;
		inset: 0;
		z-index: 1;
		border-radius: 8px;
	}
	.lib-card:hover .cover-link {
		background: rgba(12, 12, 13, 0.12);
	}
	.fav-chip {
		position: absolute;
		z-index: 2;
		left: 9px;
		bottom: 9px;
		width: 28px;
		height: 28px;
		display: flex;
		align-items: center;
		justify-content: center;
		border-radius: 7px;
		cursor: pointer;
		color: #f7f6f3;
		background: rgba(12, 12, 13, 0.88);
		border: 1px solid var(--k-border-3);
		backdrop-filter: blur(3px);
		transition:
			color 0.12s,
			background-color 0.12s,
			border-color 0.12s;
	}
	.fav-chip:hover:not(:disabled) {
		color: #e96e6e;
		border-color: rgba(233, 110, 110, 0.5);
	}
	.fav-chip:disabled {
		opacity: 0.55;
		cursor: default;
	}
	.fav-chip.on {
		color: #e96e6e;
		background: rgba(233, 110, 110, 0.2);
		border-color: rgba(233, 110, 110, 0.55);
	}
	.write-error {
		display: flex;
		align-items: center;
		gap: 8px;
		margin-top: 20px;
		padding: 10px 14px;
		border-radius: 8px;
		background: rgba(246, 183, 60, 0.1);
		border: 1px solid rgba(246, 183, 60, 0.34);
		color: var(--k-text-2);
		font-size: 13px;
		font-weight: 600;
	}
	.status-slot {
		position: absolute;
		z-index: 2;
		right: 9px;
		bottom: 9px;
	}
	.flag-svg {
		position: absolute;
		z-index: 2;
		top: 9px;
		left: 9px;
		width: 30px;
		height: 20px;
	}
	.rating {
		position: absolute;
		z-index: 2;
		top: 9px;
		right: 9px;
		display: flex;
	}
	.progressbar {
		position: absolute;
		left: 0;
		right: 0;
		bottom: 0;
		height: 4px;
		background: rgba(12, 12, 13, 0.55);
		border-radius: 0 0 8px 8px;
		overflow: hidden;
	}
	.lib-meta {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.lib-title {
		display: -webkit-box;
		-webkit-box-orient: vertical;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		overflow: hidden;
		overflow-wrap: anywhere;
		font-weight: 600;
		font-size: 13.5px;
		line-height: 1.3;
		color: var(--k-text-1);
		text-decoration: none;
	}
	.lib-title:hover {
		color: var(--k-text-bright);
	}
	.lib-sub {
		font-size: 12px;
		color: var(--k-text-faint);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	/* ---- tablet ---- */
	@media (max-width: 900px) {
		.toolbar {
			flex-wrap: wrap;
		}
		.search {
			flex: 1 1 100%;
		}
		.controls {
			flex: 1 1 100%;
		}
		.controls :global(.format-dd) {
			flex: 1 1 0;
		}
		.sort-group {
			flex: 1.6 1 0;
		}
		.sort-group :global(.sort-dd) {
			flex: 1 1 auto;
		}
	}

	/* ---- phone ---- */
	@media (max-width: 640px) {
		.head {
			padding-top: 28px;
		}
		h1 {
			font-size: 30px;
		}
		.continue {
			padding-top: 28px;
			gap: 14px;
		}
		.continue-row {
			gap: 14px;
			/* Bleed to the screen edge so cards scroll out of view, not into the gutter. */
			margin-inline: calc(-1 * var(--k-gutter));
			padding-inline: var(--k-gutter);
			scroll-padding-inline: var(--k-gutter);
			scrollbar-width: none;
		}
		.continue-card {
			width: min(260px, 72vw);
		}
		.shelfbar {
			margin-top: 32px;
			padding-top: 24px;
			gap: 12px;
		}
		.shelf-scroll {
			margin-inline: calc(-1 * var(--k-gutter));
			padding-inline: var(--k-gutter);
			scroll-padding-inline: var(--k-gutter);
		}
		.shelf-tab {
			font-size: 13px;
			padding: 0 7px 0 14px;
		}
		.controls,
		.sort-group {
			gap: 6px;
		}
		/* Icon-only order toggle on phones; the label lives in aria-label/title. */
		.dir-label {
			display: none;
		}
		.dir-btn {
			padding: 0;
			width: 40px;
		}
		.grid-wrap {
			padding-top: 16px;
			padding-bottom: 40px;
		}
		.grid {
			grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
			gap: 22px 14px;
		}
		.empty {
			padding: 56px 0;
		}
	}
	@media (pointer: coarse) {
		.search input {
			height: 44px;
		}
	}
</style>
