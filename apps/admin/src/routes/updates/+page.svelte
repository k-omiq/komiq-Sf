<script lang="ts">
	import type { CanonicalUpdate } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { loadCanonicalUpdates } from '$lib/data';

	// Mirrors the server's PAGE_SIZE (apps/server/src/graphql/mod.rs) — the pager's
	// fullness threshold: a short page means there are no further pages.
	const PAGE_SIZE = 20;

	let updates = $state<CanonicalUpdate[]>([]);
	let page = $state(1);
	let loading = $state(false);
	let loadError = $state<string | null>(null);

	// Auth redirect is centralized in +layout.svelte.

	$effect(() => {
		if (!auth.user) return;
		void refresh(page);
	});

	async function refresh(p: number): Promise<void> {
		loading = true;
		loadError = null;
		try {
			updates = await loadCanonicalUpdates(p);
		} catch (err) {
			loadError = err instanceof Error ? err.message : 'Failed to load catalogue updates.';
			updates = [];
		} finally {
			loading = false;
		}
	}

	function go(delta: number): void {
		const next = Math.max(1, page + delta);
		if (next === page) return;
		page = next;
	}

	function fmtDate(iso: string | null): string {
		if (!iso) return '—';
		const t = Date.parse(iso);
		return Number.isNaN(t) ? '—' : new Date(t).toLocaleString();
	}
</script>

<div class="page">
	<div class="page-head">
		<div>
			<h1>Catalogue updates</h1>
			<p class="lede">
				Recently-updated MangaDex works from the canonical mirror, newest chapter first. A
				monitoring feed — these works are not reader-openable yet. ·
				<span
					class="nsfw-note"
					title="This console always requests NSFW-inclusive results (an admin-only per-request override), independent of the NSFW on/off preference in the header. The reader still honours each viewer's own preference."
					>incl. NSFW-flagged</span
				>
			</p>
		</div>
		<div class="pager">
			<button class="pg" disabled={loading || page === 1} onclick={() => go(-1)}>Prev</button>
			<span class="page-num">Page {page}</span>
			<button class="pg" disabled={loading || updates.length < PAGE_SIZE} onclick={() => go(1)}
				>Next</button
			>
		</div>
	</div>

	{#if loadError}
		<div class="notice error">{loadError}</div>
	{:else if loading && updates.length === 0}
		<div class="notice">Loading…</div>
	{:else if updates.length === 0}
		<div class="notice">
			No catalogue updates yet — the MangaDex mirror is empty (enable <code>CATALOGUE_SYNC</code>).
		</div>
	{:else}
		<div class="table" role="table" aria-label="Catalogue updates">
			<div class="row head" role="row">
				<span role="columnheader">Work</span>
				<span role="columnheader">Latest chapter</span>
				<span role="columnheader">Updated</span>
			</div>
			{#each updates as u (u.workId)}
				<div class="row" role="row">
					<span class="work" role="cell">
						<span class="wtitle">
							{u.title ?? '(untitled)'}
							{#if u.isNsfw}<span class="badge nsfw">NSFW</span>{/if}
						</span>
						<span class="wid">md:{u.mangadexId}</span>
					</span>
					<span role="cell">
						<span class="ch">Ch. {u.latestChapter ?? '—'}</span>
						{#if u.latestChapterTitle}<span class="chtitle">{u.latestChapterTitle}</span>{/if}
					</span>
					<span class="muted" role="cell">{fmtDate(u.latestAt)}</span>
				</div>
			{/each}
		</div>
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-6);
	}
	.page-head {
		display: flex;
		align-items: flex-end;
		justify-content: space-between;
		gap: var(--k-space-4);
		flex-wrap: wrap;
	}
	h1 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 24px;
		margin: 0 0 6px;
		color: var(--k-text-bright);
	}
	.lede {
		margin: 0;
		max-width: 60ch;
		font-size: 13.5px;
		color: var(--k-text-dimmer);
	}
	/* The console reads this feed NSFW-inclusive regardless of the header's NSFW
	   preference pill; say so, so the console/reader difference isn't a mystery. */
	.nsfw-note {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-text-faint);
		border-bottom: 1px dotted var(--k-border-4);
		cursor: help;
	}
	.pager {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.page-num {
		font-size: 13px;
		color: var(--k-text-dimmer);
	}
	.pg {
		height: 34px;
		padding: 0 14px;
		border-radius: 8px;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		font-weight: 600;
		font-size: 13px;
		cursor: pointer;
	}
	.pg:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.notice {
		padding: 16px 18px;
		border-radius: 10px;
		border: 1px solid var(--k-border-1);
		background: var(--k-surface);
		color: var(--k-text-dimmer);
		font-size: 13.5px;
	}
	.notice.error {
		border-color: rgba(214, 92, 92, 0.4);
		color: #e08a8a;
	}
	.notice code {
		font-family: var(--k-font-mono, monospace);
		font-size: 12.5px;
		color: var(--k-text-2);
	}
	.table {
		display: flex;
		flex-direction: column;
		border: 1px solid var(--k-border-1);
		border-radius: 12px;
		overflow: hidden;
	}
	.row {
		display: grid;
		grid-template-columns: 2.4fr 2fr 1.2fr;
		gap: 16px;
		align-items: center;
		padding: 14px 18px;
		border-top: 1px solid var(--k-border-1);
	}
	.row.head {
		border-top: none;
		background: var(--k-surface);
		font-size: 11.5px;
		font-weight: 700;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-text-dimmer);
	}
	.work {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.wtitle {
		display: flex;
		align-items: center;
		gap: 8px;
		font-weight: 600;
		font-size: 14px;
		color: var(--k-text-1);
	}
	.wid {
		font-family: var(--k-font-mono, monospace);
		font-size: 11.5px;
		color: var(--k-text-faint);
	}
	.ch {
		font-weight: 600;
		font-size: 13.5px;
		color: var(--k-text-2);
	}
	.chtitle {
		display: block;
		font-size: 12px;
		color: var(--k-text-faint);
		margin-top: 2px;
	}
	.muted {
		font-size: 12.5px;
		color: var(--k-text-faint);
	}
	.badge {
		font-size: 10.5px;
		font-weight: 700;
		letter-spacing: 0.03em;
		padding: 2px 7px;
		border-radius: 5px;
	}
	.badge.nsfw {
		color: #e08a8a;
		background: rgba(214, 92, 92, 0.14);
		border: 1px solid rgba(214, 92, 92, 0.4);
	}
	@media (max-width: 720px) {
		.row {
			grid-template-columns: 1fr;
			gap: 6px;
		}
		.row.head {
			display: none;
		}
	}
</style>
