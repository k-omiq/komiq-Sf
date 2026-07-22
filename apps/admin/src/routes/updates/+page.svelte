<script lang="ts">
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type { CanonicalUpdate } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { loadCanonicalUpdates } from '$lib/data';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';

	// Mirrors the server's PAGE_SIZE (apps/server/src/graphql/mod.rs) — the pager's
	// fullness threshold: a short page means there are no further pages.
	const PAGE_SIZE = 20;

	let updates = $state<CanonicalUpdate[]>([]);
	const page = $derived(queuePage(pageState.url));
	let loading = $state(false);
	let loadError = $state<string | null>(null);

	// Auth redirect is centralized in +layout.svelte.

	$effect(() => {
		if (!auth.user) return;
		void refresh(page);
	});

	let loadSeq = 0;
	async function refresh(p: number): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		try {
			const result = await loadCanonicalUpdates(p);
			if (seq !== loadSeq) return;
			updates = result;
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load catalogue updates.';
			updates = [];
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	function goTo(next: number): void {
		const target = Math.max(1, next);
		if (target === page) return;
		void queueNavigate(pageState.url, { page: target });
	}

	function fmtDate(iso: string | null): string {
		if (!iso) return 'Unknown';
		const t = Date.parse(iso);
		return Number.isNaN(t) ? 'Unknown' : new Date(t).toLocaleString();
	}

	// An empty page past the first is the end of the feed, not an empty feed.
	const pastEnd = $derived(page > 1);
</script>

<div class="k-page">
	<!-- The console reads this feed NSFW-inclusive regardless of the admin's own NSFW
	     preference; say so, so the console/reader difference isn't a mystery. -->
	<PageHeader
		title="Latest updates"
		description="Newest chapter releases first. Open a title to review it. NSFW titles are always included here, whatever your own NSFW setting is."
	/>

	<LoadState
		loading={loading && updates.length === 0}
		error={loadError}
		empty={updates.length === 0}
		emptyTitle={pastEnd ? 'No more updates' : 'No updates yet'}
		emptyHint={pastEnd
			? 'This page is past the end of the feed. Go back a page.'
			: 'New chapter releases from MangaDex show up here. The feed stays empty while catalogue sync is off on the server.'}
		emptyIcon="clock"
		onretry={() => refresh(page)}
	>
		{#snippet emptyActions()}
			{#if !pastEnd}
				<p class="env">
					To turn it on, set <span class="k-mono">CATALOGUE_SYNC=on</span> in the server environment.
				</p>
			{/if}
		{/snippet}

		<div class="k-table" class:is-dim={loading} role="table" aria-label="Catalogue updates">
			<div class="k-tr k-tr--head" role="row">
				<span role="columnheader">Work</span>
				<span role="columnheader">Latest chapter</span>
				<span role="columnheader">Updated</span>
			</div>
			{#each updates as u (u.workId)}
				<div class="k-tr" role="row">
					<span class="work" role="cell">
						<span class="title-line">
							<a class="title k-truncate" href="/series/{u.workId}">{u.title ?? 'Untitled'}</a>
							{#if u.isNsfw}<span class="k-badge" data-tone="warning">NSFW</span>{/if}
						</span>
						<span class="k-mono k-muted k-truncate">MangaDex {u.mangadexId}</span>
					</span>
					<span role="cell" data-label="Latest chapter">
						<span class="chapter">
							<span class="k-num">
								{u.latestChapter != null ? `Chapter ${u.latestChapter}` : 'Unknown'}
							</span>
							{#if u.latestChapterTitle}
								<span class="k-muted chapter-title">{u.latestChapterTitle}</span>
							{/if}
						</span>
					</span>
					<span class="k-muted k-num" role="cell" data-label="Updated">{fmtDate(u.latestAt)}</span>
				</div>
			{/each}
		</div>
	</LoadState>

	<!-- Outside LoadState so "Prev" stays reachable from an empty page past the end. -->
	{#if !loadError && (updates.length > 0 || page > 1)}
		<Pager {page} hasNext={updates.length >= PAGE_SIZE} busy={loading} onpage={goTo} />
	{/if}
</div>

<style>
	.k-table {
		--cols: minmax(0, 2.4fr) minmax(0, 2fr) minmax(0, 1.2fr);
	}
	.work {
		display: grid;
		gap: 2px;
	}
	.title-line {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		min-width: 0;
	}
	.title {
		color: var(--k-text-bright);
		font-weight: 700;
	}
	.title:hover {
		text-decoration: underline;
		text-underline-offset: 3px;
	}
	.chapter {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.chapter-title {
		font-size: var(--k-text-xs);
		overflow-wrap: anywhere;
	}
	.env {
		max-width: 52ch;
		font-size: var(--k-text-sm);
	}
	.env .k-mono {
		color: var(--k-text-1);
	}
</style>
