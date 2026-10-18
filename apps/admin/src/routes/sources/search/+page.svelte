<script lang="ts">
	import { href } from '$lib/paths';
	import type { FederatedSearchPage, SeriesStatus } from '@komika/types';
	import { onDestroy } from 'svelte';
	import { goto } from '$app/navigation';
	import { page as pageState } from '$app/state';
	import { auth } from '$lib/auth.svelte';
	import { assetSrc } from '$lib/config';
	import { searchAllSources } from '$lib/data';
	import Button from '$lib/components/Button.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';

	// Searches every installed source at once and files each hit under its catalogue
	// work. The reader used to offer this from its Browse page; it is admin-only now
	// because every search writes source mappings. The query and page live in the URL so
	// a reload or Back keeps them.

	const MAX_PAGE = 1_000;
	const appliedQuery = $derived(pageState.url.searchParams.get('q')?.trim() ?? '');
	const page = $derived.by(() => {
		const n = Number(pageState.url.searchParams.get('page') ?? '1');
		return Number.isInteger(n) && n >= 1 && n <= MAX_PAGE ? n : 1;
	});

	let searchDraft = $state('');
	$effect(() => {
		searchDraft = appliedQuery;
	});

	let result = $state<FederatedSearchPage | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let reloadKey = $state(0);
	let loadSeq = 0;
	onDestroy(() => ++loadSeq);

	$effect(() => {
		const q = appliedQuery;
		const p = page;
		void reloadKey;
		if (!auth.user) return;
		const seq = ++loadSeq;
		if (!q) {
			result = null;
			error = null;
			loading = false;
			return;
		}
		loading = true;
		error = null;
		searchAllSources(q, p)
			.then((r) => {
				if (seq === loadSeq) result = r;
			})
			.catch((err) => {
				if (seq !== loadSeq) return;
				result = null;
				error = err instanceof Error ? err.message : 'The search failed.';
			})
			.finally(() => {
				if (seq === loadSeq) loading = false;
			});
	});

	function searchHref(q: string, p: number): string {
		const sp = new URLSearchParams();
		if (q) sp.set('q', q);
		if (p > 1) sp.set('page', String(p));
		const s = sp.toString();
		return href(s ? `/sources/search?${s}` : '/sources/search');
	}

	function onSearch(e: SubmitEvent): void {
		e.preventDefault();
		const q = searchDraft.trim();
		// Submitting the same search again is the retry.
		if (q === appliedQuery && page === 1) reloadKey++;
		else goto(searchHref(q, 1), { keepFocus: true });
	}

	const STATUS_TONE: Record<SeriesStatus, string | undefined> = {
		ONGOING: 'success',
		COMPLETED: 'info',
		HIATUS: 'warning',
		CANCELLED: 'danger',
		UNKNOWN: undefined,
	};
	function sentence(word: string): string {
		return word.charAt(0).toUpperCase() + word.slice(1).toLowerCase().replaceAll('_', ' ');
	}

	const items = $derived(result?.items ?? []);
</script>

<div class="k-page k-stack-lg">
	<PageHeader
		title="Search all sources"
		description="Search every installed source at once. Each result is filed under its catalogue work. The top results that are not in the catalogue yet are added to it."
	/>

	<div class="find">
		<form class="k-toolbar" onsubmit={onSearch}>
			<div class="k-search k-grow">
				<Icon name="search" />
				<input
					class="k-input"
					type="search"
					name="q"
					aria-label="Search all sources"
					autocomplete="off"
					placeholder="Search by title"
					bind:value={searchDraft}
				/>
			</div>
			<Button type="submit" variant="primary" disabled={!searchDraft.trim()}>Search</Button>
		</form>
		<p class="k-muted">
			{#if result && !loading}
				Searched <span class="k-num">{result.sourcesQueried}</span>
				{result.sourcesQueried === 1 ? 'source' : 'sources'} for “{appliedQuery}”.
			{:else}
				A search asks every source, so it can take a while. Each account can run about 20 a minute.
			{/if}
		</p>
	</div>

	{#if appliedQuery}
		<LoadState
			loading={loading && items.length === 0}
			{error}
			empty={items.length === 0}
			skeletonRows={6}
			emptyIcon="search"
			emptyTitle={page > 1 ? 'Nothing on this page' : 'No source has this title'}
			emptyHint={page > 1
				? 'Go back to the previous page.'
				: 'Check the spelling or try a shorter or alternative title.'}
			onretry={() => reloadKey++}
		>
			<div
				class="k-table"
				class:is-dim={loading}
				role="table"
				aria-label="Results from all sources"
				aria-busy={loading}
			>
				<div class="k-tr k-tr--head" role="row">
					<span role="columnheader">Series</span>
					<span role="columnheader">Type</span>
					<span role="columnheader">Status</span>
					<span role="columnheader">Chapters</span>
					<span role="columnheader">Sources</span>
				</div>
				{#each items as { series: s, translators } (s.id)}
					<div class="k-tr" role="row">
						<div role="cell" class="series">
							<Cover src={s.coverUrl ? assetSrc(s.coverUrl) : null} />
							<div class="titles">
								<a class="title" href={href(`/series/${s.id}`)}>{s.title}</a>
								<span class="detail k-truncate">
									<span class="k-mono">#{s.id}</span>{s.author ? ` · ${s.author}` : ''}
								</span>
							</div>
						</div>
						<span role="cell" data-label="Type">{sentence(s.type)}</span>
						<span role="cell" data-label="Status">
							<span class="k-badge" data-tone={STATUS_TONE[s.status]}>{sentence(s.status)}</span>
						</span>
						<span role="cell" class="k-num" data-label="Chapters">{s.chapterCount}</span>
						<span role="cell" class="sources" data-label="Sources">
							{#each translators as t (`${t.sourceType}:${t.sourceId}`)}
								<span class="k-badge">{t.sourceName ?? t.sourceId}</span>
							{/each}
						</span>
					</div>
				{/each}
			</div>
		</LoadState>

		{#if result && (page > 1 || result.hasNextPage)}
			<Pager
				{page}
				hasNext={result.hasNextPage}
				busy={loading}
				onpage={(p) => goto(searchHref(appliedQuery, p))}
			/>
		{/if}
	{/if}
</div>

<style>
	.find {
		display: grid;
		gap: var(--k-space-2);
	}
	.k-table {
		--cols: minmax(0, 2.4fr) 80px 100px 80px minmax(0, 1.6fr);
	}
	.series {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
	}
	.titles {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.title {
		overflow-wrap: anywhere;
		font-weight: 600;
	}
	.detail {
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
	}
	.sources {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-1);
	}
</style>
