<script lang="ts">
	import { onDestroy } from 'svelte';
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type {
		BrowseSource,
		Scanlator,
		ScanlatorAi,
		ScanlatorAiFilter,
		ScanlatorSite,
	} from '@komika/api';
	import { auth } from '$lib/auth.svelte';
	import {
		loadBrowseSources,
		loadScanlatorSites,
		loadScanlators,
		setScanlatorAi,
		setSourceAi,
	} from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import Segmented from '$lib/components/Segmented.svelte';

	/** Rows per page. The server fixes it. */
	const PAGE_SIZE = 50;

	let rows = $state<Scanlator[]>([]);
	const page = $derived(queuePage(pageState.url));
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let loading = $state(false);
	let loadError = $state<string | null>(null);

	// The filters live in the URL, as on Reports: a control navigates, and the effect
	// further down is the single fetch driver.
	const search = $derived(pageState.url.searchParams.get('q')?.trim() ?? '');
	const tag = $derived.by<ScanlatorAiFilter | null>(() => {
		const v = pageState.url.searchParams.get('tag');
		return v === 'TAGGED' || v === 'AI' || v === 'PARTIAL' || v === 'UNTAGGED' ? v : null;
	});
	const source = $derived(pageState.url.searchParams.get('source') || null);

	/**
	 * Two lists share this page. A group is the name a chapter credits. A site is a source
	 * that translates its own series: its chapters credit no group, so the source is tagged.
	 */
	type View = 'groups' | 'sites';
	const VIEWS: { label: string; value: View }[] = [
		{ label: 'Groups', value: 'groups' },
		{ label: 'Sites', value: 'sites' },
	];
	const view = $derived<View>(
		pageState.url.searchParams.get('view') === 'sites' ? 'sites' : 'groups',
	);
	// A site is not filtered by source: it is one. A `source` left in the URL is kept for
	// the way back to Groups and ignored here.
	const filtered = $derived(
		search !== '' || tag !== null || (view === 'groups' && source !== null),
	);

	/** `Segmented` needs string values, so "every tag" is `all` here and `null` everywhere else. */
	type TagFilter = ScanlatorAiFilter | 'all';
	const TAG_FILTERS: { label: string; value: TagFilter }[] = [
		{ label: 'All', value: 'all' },
		{ label: 'Tagged', value: 'TAGGED' },
		{ label: 'AI', value: 'AI' },
		{ label: 'Partial AI', value: 'PARTIAL' },
		{ label: 'Not tagged', value: 'UNTAGGED' },
	];

	/** The same for a row's own tag: "no tag" is `NONE` here and `null` everywhere else. */
	type TagChoice = ScanlatorAi | 'NONE';
	const TAG_CHOICES: { label: string; value: TagChoice }[] = [
		{ label: 'None', value: 'NONE' },
		{ label: 'Partial AI', value: 'PARTIAL' },
		{ label: 'AI', value: 'AI' },
	];

	const TAG_LABEL: Record<ScanlatorAi, string> = {
		AI: 'AI',
		PARTIAL: 'Partial AI',
	};

	// The source filter is an extra. When its list is empty or does not load, the
	// control is left out and the rest of the page works without it.
	let sources = $state<BrowseSource[]>([]);
	let sourcesSettled = $state(false);

	async function loadSources(): Promise<void> {
		try {
			sources = await loadBrowseSources();
		} catch {
			sources = [];
		} finally {
			sourcesSettled = true;
		}
	}

	/** Option labels: the name, plus the language when two sources share a name. */
	const sourceOptions = $derived.by(() => {
		const uses = new Map<string, number>();
		for (const s of sources) uses.set(s.name, (uses.get(s.name) ?? 0) + 1);
		return sources.map((s) => ({
			id: s.id,
			label: (uses.get(s.name) ?? 0) > 1 && s.lang ? `${s.name} (${s.lang.toUpperCase()})` : s.name,
		}));
	});

	// Auth redirect is centralized in +layout.svelte.

	$effect(() => {
		if (!auth.user) return;
		void loadSources();
	});

	// A `source` in the URL that the control cannot show (the list did not load, or the
	// source is gone) would narrow the list with nothing on screen saying so. Drop it.
	$effect(() => {
		if (sourcesSettled && source && !sources.some((s) => s.id === source)) {
			void queueNavigate(pageState.url, { source: null, page: 1 }, true);
		}
	});

	// The site list is read whole and filtered here, so in that view the filters are not
	// part of the fetch.
	$effect(() => {
		if (!auth.user) return;
		if (view === 'sites') void refreshSites();
		else void refresh(search, tag, source, page);
	});

	/**
	 * Monotonic request token, as on Reports. The filters stay usable while a fetch is in
	 * flight, and the first load can take seconds, so two fetches can overlap and land out
	 * of order. Only the newest request is allowed to write state.
	 */
	let loadSeq = 0;

	/** `quiet` reloads behind the list on screen (after a save), without dimming it. */
	async function refresh(
		q: string,
		t: ScanlatorAiFilter | null,
		s: string | null,
		p: number,
		quiet = false,
	): Promise<void> {
		const seq = ++loadSeq;
		if (!quiet) loading = true;
		loadError = null;
		try {
			const res = await loadScanlators({
				search: q || undefined,
				ai: t ?? undefined,
				source: s ?? undefined,
				page: p,
			});
			if (seq !== loadSeq) return;
			rows = res.items;
			hasNext = res.hasNextPage;
			total = res.total;
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load scanlators.';
			rows = [];
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	// ---- Sites view ---------------------------------------------------------------
	// Its own rows, flags and request token: a fetch for one view never writes the state
	// of the other, whichever view is on screen when it lands.
	let sites = $state<ScanlatorSite[]>([]);
	let sitesLoading = $state(false);
	let sitesError = $state<string | null>(null);
	let sitesSeq = 0;

	async function refreshSites(quiet = false): Promise<void> {
		const seq = ++sitesSeq;
		if (!quiet) sitesLoading = true;
		sitesError = null;
		try {
			const res = await loadScanlatorSites();
			if (seq !== sitesSeq) return;
			sites = res;
		} catch (err) {
			if (seq !== sitesSeq) return;
			sitesError = err instanceof Error ? err.message : 'Failed to load sites.';
			sites = [];
		} finally {
			if (seq === sitesSeq) sitesLoading = false;
		}
	}

	/** The loaded sites the Name box and the Tag filter let through. */
	const shownSites = $derived.by(() => {
		const q = search.toLowerCase();
		return sites.filter(
			(s) =>
				(q === '' || s.name.toLowerCase().includes(q)) &&
				(tag === null ||
					(tag === 'TAGGED' ? s.ai !== null : tag === 'UNTAGGED' ? s.ai === null : s.ai === tag)),
		);
	});

	/** A source's language for a reader of the list: `en` is EN, `all` is every language. */
	function langLabel(lang: string): string {
		return lang === 'all' ? 'All languages' : lang.toUpperCase();
	}

	/** Two sites can share a name, so a control names its row with the language too. */
	function siteName(row: ScanlatorSite): string {
		return row.lang ? `${row.name} (${langLabel(row.lang)})` : row.name;
	}

	// The search box is a draft. Typing waits for a pause, then moves the search into the
	// URL, which is what the list is fetched from.
	let searchDraft = $state('');
	/** The search this page last put in the URL itself. */
	let committed = '';
	let searchTimer: ReturnType<typeof setTimeout> | undefined;

	$effect(() => {
		// Back, Forward or a link changed the search: show it in the box. Our own
		// navigation is skipped, so it cannot overwrite what was typed since.
		if (search === committed) return;
		committed = search;
		clearTimeout(searchTimer);
		searchDraft = search;
	});

	function onSearchInput(value: string): void {
		searchDraft = value;
		clearTimeout(searchTimer);
		searchTimer = setTimeout(() => {
			const q = value.trim();
			if (q === search) return;
			committed = q;
			void queueNavigate(pageState.url, { q: q || null, page: 1 }, true);
		}, 300);
	}

	onDestroy(() => clearTimeout(searchTimer));

	function setView(v: View): void {
		if (v === view) return;
		void queueNavigate(pageState.url, { view: v === 'sites' ? v : null, page: 1 });
	}

	function setTagFilter(v: ScanlatorAiFilter | null): void {
		if (v === tag) return;
		void queueNavigate(pageState.url, { tag: v, page: 1 });
	}

	function setSource(v: string | null): void {
		if (v === source) return;
		void queueNavigate(pageState.url, { source: v, page: 1 });
	}

	/**
	 * Tags being saved. A row in here shows its new tag at once and is disabled until the
	 * server answers.
	 *
	 * A list, not the single `busy` id of Users and Reports: tagging one row after another
	 * is the normal use of this page, and a click must not be dropped because the row
	 * before it is still saving. Each save is one write to one name, so they do not
	 * depend on each other.
	 */
	let saving = $state<{ name: string; ai: ScanlatorAi | null }[]>([]);

	async function setTag(row: Scanlator, next: ScanlatorAi | null): Promise<void> {
		if (next === row.ai || saving.some((s) => s.name === row.name)) return;
		saving = [...saving, { name: row.name, ai: next }];
		try {
			await setScanlatorAi(row.name, next);
			// This row is known now. The reload brings what the server works out for the
			// others: a joint credit with no tag of its own takes one from its members.
			rows = rows.map((x) => (x.name === row.name ? { ...x, ai: next, effectiveAi: next } : x));
			void refresh(search, tag, source, page, true);
		} catch (err) {
			toast.fromError(err, 'Could not save that tag.');
		} finally {
			saving = saving.filter((s) => s.name !== row.name);
		}
	}

	/** The same for sites, by source id. A second list, so a group name and a source id
	 * can never be taken for each other. */
	let savingSites = $state<{ sourceId: string; ai: ScanlatorAi | null }[]>([]);

	async function setSiteTag(row: ScanlatorSite, next: ScanlatorAi | null): Promise<void> {
		if (next === row.ai || savingSites.some((s) => s.sourceId === row.sourceId)) return;
		savingSites = [...savingSites, { sourceId: row.sourceId, ai: next }];
		try {
			await setSourceAi(row.sourceId, next);
			sites = sites.map((x) => (x.sourceId === row.sourceId ? { ...x, ai: next } : x));
			// A load that began before the save holds the old tag. This newer one outdates
			// it. It also drops a site that now has no tag and no series.
			void refreshSites(true);
		} catch (err) {
			toast.fromError(err, 'Could not save that tag.');
		} finally {
			savingSites = savingSites.filter((s) => s.sourceId !== row.sourceId);
		}
	}

	const rangeStart = $derived((page - 1) * PAGE_SIZE + 1);
	const pagerDetail = $derived(
		rows.length > 0 && total != null
			? `(${rangeStart.toLocaleString()} to ${(rangeStart + rows.length - 1).toLocaleString()} of ${total.toLocaleString()})`
			: undefined,
	);
</script>

<div class="k-page">
	<PageHeader
		title="Scanlators"
		description="Tag the scanlators that use AI for all of their work (AI) or part of it (Partial AI). A group is the name a chapter credits. A site is a source that translates its own series."
	/>

	<div class="view">
		<Segmented label="List" value={view} options={VIEWS} onchange={setView} />
	</div>

	<!-- Not disabled while a fetch is in flight, as on Reports: see `loadSeq`. -->
	<div class="k-toolbar filters">
		<label class="k-field k-grow">
			<span class="k-label">Name</span>
			<span class="k-search">
				<Icon name="search" />
				<input
					class="k-input"
					type="search"
					name="q"
					autocomplete="off"
					placeholder="Search by name"
					value={searchDraft}
					oninput={(e) => onSearchInput(e.currentTarget.value)}
				/>
			</span>
		</label>
		<div class="k-field">
			<span class="k-label" aria-hidden="true">Tag</span>
			<Segmented
				label="Filter by tag"
				value={tag ?? 'all'}
				options={TAG_FILTERS}
				onchange={(v) => setTagFilter(v === 'all' ? null : v)}
			/>
		</div>
		{#if view === 'groups' && sourceOptions.length > 0}
			<label class="k-field source-filter">
				<span class="k-label">Source</span>
				<select
					class="k-select"
					autocomplete="off"
					name="source"
					value={source ?? ''}
					onchange={(e) => setSource(e.currentTarget.value || null)}
				>
					<option value="">All sources</option>
					{#each sourceOptions as s (s.id)}
						<option value={s.id}>{s.label}</option>
					{/each}
				</select>
			</label>
		{/if}
	</div>

	{#if view === 'sites'}
		<p class="k-hint">
			A tag on a site applies to every chapter read from it. A chapter that also credits a tagged
			group takes the stronger tag.
		</p>

		<LoadState
			loading={sitesLoading && sites.length === 0}
			error={sitesError}
			empty={shownSites.length === 0}
			loadingLabel="Loading sites."
			emptyIcon={filtered ? 'search' : 'globe'}
			emptyTitle={filtered ? 'No sites match' : 'No sites yet'}
			emptyHint={filtered
				? 'Try another name or tag.'
				: 'A source is listed here once a series is added from it.'}
			onretry={() => refreshSites()}
		>
			<div class="k-table sites" class:is-dim={sitesLoading} role="table" aria-label="Sites">
				<div class="k-tr k-tr--head" role="row">
					<span role="columnheader">Name</span>
					<span role="columnheader">Series</span>
					<span role="columnheader" class="k-td--end">AI tag</span>
				</div>
				{#each shownSites as row (row.sourceId)}
					{@const pending = savingSites.find((s) => s.sourceId === row.sourceId)}
					<div class="k-tr" role="row">
						<span class="who" role="cell">
							<span class="name">{row.name}</span>
							{#if row.lang}<span class="k-hint">{langLabel(row.lang)}</span>{/if}
						</span>
						<span class="k-num" role="cell" data-label="Series">
							{row.seriesCount.toLocaleString()}
						</span>
						<span class="k-td--end" class:saving={!!pending} role="cell">
							<Segmented
								label="AI tag for {siteName(row)}"
								value={(pending ? pending.ai : row.ai) ?? 'NONE'}
								options={TAG_CHOICES}
								disabled={!!pending}
								onchange={(v) => setSiteTag(row, v === 'NONE' ? null : v)}
							/>
						</span>
					</div>
				{/each}
			</div>
		</LoadState>
	{:else}
		<LoadState
			loading={loading && rows.length === 0}
			error={loadError}
			empty={rows.length === 0}
			loadingLabel="Loading scanlators. The first load can take a few seconds."
			emptyIcon={filtered ? 'search' : 'globe'}
			emptyTitle={filtered
				? 'No scanlators match'
				: page > 1
					? 'No scanlators on this page'
					: 'No scanlators yet'}
			emptyHint={filtered
				? 'Try another name, tag or source.'
				: page > 1
					? 'Go back a page to see the list.'
					: 'A scanlator is listed here once a chapter credits it.'}
			onretry={() => refresh(search, tag, source, page)}
		>
			<div class="k-table" class:is-dim={loading} role="table" aria-label="Scanlators">
				<div class="k-tr k-tr--head" role="row">
					<span role="columnheader">Name</span>
					<span role="columnheader">Chapters</span>
					<span role="columnheader">Series</span>
					<span role="columnheader" class="k-td--end">AI tag</span>
				</div>
				{#each rows as row (row.name)}
					{@const pending = saving.find((s) => s.name === row.name)}
					<div class="k-tr" role="row">
						<span class="who" role="cell">
							<span class="name">{row.name}</span>
							<!-- No tag of its own, but one applies: a joint credit ("A & B") takes the
							     strongest tag of its members. The row can still get its own. -->
							{#if !pending && row.ai === null && row.effectiveAi}
								<span class="k-hint">{TAG_LABEL[row.effectiveAi]}, from a member group</span>
							{/if}
						</span>
						<span class="k-num" role="cell" data-label="Chapters">
							{row.chapterCount.toLocaleString()}
						</span>
						<span class="k-num" role="cell" data-label="Series">
							{row.seriesCount.toLocaleString()}
						</span>
						<span class="k-td--end" class:saving={!!pending} role="cell">
							<Segmented
								label="AI tag for {row.name}"
								value={(pending ? pending.ai : row.ai) ?? 'NONE'}
								options={TAG_CHOICES}
								disabled={!!pending}
								onchange={(v) => setTag(row, v === 'NONE' ? null : v)}
							/>
						</span>
					</div>
				{/each}
			</div>
		</LoadState>

		<!--
			Outside LoadState's empty branch on purpose, as on Reports: a page the list has
			shrunk past is empty, and the pager is the way back from it.
		-->
		{#if !loadError && !(loading && rows.length === 0) && (rows.length > 0 || page > 1)}
			<Pager
				{page}
				{hasNext}
				busy={loading}
				detail={pagerDetail}
				onpage={(n) => queueNavigate(pageState.url, { page: Math.max(1, n) })}
			/>
		{/if}
	{/if}
</div>

<style>
	.k-table {
		--cols: minmax(0, 2.4fr) minmax(0, 0.7fr) minmax(0, 0.7fr) minmax(200px, 1.5fr);
	}
	.k-table.sites {
		--cols: minmax(0, 2.4fr) minmax(0, 0.7fr) minmax(200px, 1.5fr);
	}
	/* A grid row would stretch the switch across the page. */
	.view {
		display: flex;
	}
	.filters {
		align-items: flex-end;
	}
	.source-filter {
		flex: 0 1 220px;
	}
	.who {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 2px;
	}
	/* A joint credit can be long, and a name has no sure word breaks. It wraps inside its
	   own column instead of pushing the tag control off the row. */
	.name {
		color: var(--k-text-bright);
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.saving {
		opacity: 0.55;
	}
	@media (max-width: 720px) {
		.source-filter {
			flex-basis: 100%;
		}
	}
</style>
