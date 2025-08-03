<script lang="ts">
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type { CoverIssue } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { assetSrc } from '$lib/config';
	import { loadCoverIssues, retryCover, uploadCover } from '$lib/data';

	let issues = $state<CoverIssue[]>([]);
	const page = $derived(queuePage(pageState.url));
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let busy = $state<string | null>(null); // workId currently acting
	// Hidden file inputs keyed by workId, so each row's "Upload" button opens its own.
	let fileInputs = $state<Record<string, HTMLInputElement>>({});

	const REASON_LABEL: Record<string, string> = {
		too_large: 'Source too large',
		unsupported: 'Unsupported / corrupt',
		empty: 'Empty source',
		encode: 'Encode failed',
		store: 'Store failed',
	};

	// Auth redirect is centralized in +layout.svelte.
	$effect(() => {
		if (!auth.user) return;
		void refresh(page);
	});

	let loadSeq = 0;
	async function refresh(p = page): Promise<void> {
		if (p !== page) {
			await queueNavigate(pageState.url, { page: p });
			return;
		}
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		try {
			const res = await loadCoverIssues(p);
			if (seq !== loadSeq) return;
			issues = res.items;
			if (res.page !== p) {
				void queueNavigate(pageState.url, { page: res.page }, true);
				return;
			}
			hasNext = res.hasNextPage;
			total = res.total ?? null;
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load cover issues.';
			issues = [];
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	function fmtDate(iso: string): string {
		const t = Date.parse(iso);
		return Number.isNaN(t) ? '—' : new Date(t).toLocaleString();
	}

	// After an optimistic removal empties the current page while issues still exist
	// elsewhere, reload so we don't show the "all clean 🎉" message (and hide the pager)
	// on a page that only looks empty. Stay on the page if more are queued ahead
	// (server backfills from the next page), else step back a page.
	async function settleAfterFix(): Promise<void> {
		if (issues.length === 0 && total != null && total > 0) {
			const next = hasNext ? page : Math.max(1, page - 1);
			if (next === page) await refresh(page);
			else await queueNavigate(pageState.url, { page: next }, true);
		}
	}

	async function retry(w: CoverIssue): Promise<void> {
		if (busy) return;
		busy = w.workId;
		actionError = null;
		try {
			const stored = await retryCover(w.workId);
			if (stored) {
				// Fixed — drop it from the list.
				issues = issues.filter((x) => x.workId !== w.workId);
				if (total != null) total -= 1;
				await settleAfterFix();
			} else {
				actionError = `Couldn't fetch a source image for “${w.title ?? w.workId}” — try again later or upload one.`;
			}
		} catch (err) {
			// A deterministic re-failure refreshes the reason server-side; reload so the
			// row shows the latest attempt count / reason.
			actionError = err instanceof Error ? err.message : 'Retry failed.';
			await refresh();
		} finally {
			busy = null;
		}
	}

	async function onFilePicked(w: CoverIssue, ev: Event): Promise<void> {
		const input = ev.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		input.value = ''; // allow re-picking the same file later
		if (!file || busy) return;
		busy = w.workId;
		actionError = null;
		try {
			await uploadCover(w.workId, file);
			issues = issues.filter((x) => x.workId !== w.workId);
			if (total != null) total -= 1;
			await settleAfterFix();
		} catch (err) {
			actionError = err instanceof Error ? err.message : 'Upload failed.';
		} finally {
			busy = null;
		}
	}
</script>

<div class="page">
	<div class="page-head">
		<div>
			<h1>Cover issues</h1>
			<p class="lede">
				{total ?? issues.length} cover{(total ?? issues.length) === 1 ? '' : 's'} the crawl couldn't process.
				Retry (re-fetch + re-encode) or upload a replacement.
			</p>
		</div>
		<button class="pg" disabled={loading} onclick={() => refresh()}>Refresh</button>
	</div>

	{#if loadError}
		<div class="notice error" role="alert">{loadError}</div>
	{:else if loading && issues.length === 0}
		<div class="notice" role="status">Loading…</div>
	{:else if issues.length === 0}
		<div class="notice">No cover issues — every cover processed cleanly. 🎉</div>
	{:else}
		{#if actionError}<div class="notice error" role="alert">{actionError}</div>{/if}
		<div class="table" role="table" aria-label="Cover issues">
			<div class="row head" role="row">
				<span role="columnheader">Cover</span>
				<span role="columnheader">Work</span>
				<span role="columnheader">Reason</span>
				<span role="columnheader">Tries</span>
				<span role="columnheader">Last attempt</span>
				<span role="columnheader" class="actions-col">Fix</span>
			</div>
			{#each issues as w (w.workId)}
				<div class="row" role="row">
					<span role="cell">
						{#if assetSrc(w.coverUrl)}
							<img
								width="80"
								height="112"
								class="cover"
								src={assetSrc(w.coverUrl)}
								alt=""
								loading="lazy"
							/>
						{:else}
							<span class="cover placeholder" aria-hidden="true">?</span>
						{/if}
					</span>
					<span class="work" role="cell">
						<span class="wtitle">{w.title ?? '(untitled)'}</span>
						<span class="wid">{w.workId}</span>
					</span>
					<span role="cell">
						<span class="badge" title={w.detail ?? ''}>{REASON_LABEL[w.reason] ?? w.reason}</span>
					</span>
					<span class="muted" role="cell">{w.attempts}</span>
					<span class="muted" role="cell">{fmtDate(w.lastSeen)}</span>
					<span class="actions" role="cell">
						<button class="act" disabled={busy === w.workId} onclick={() => retry(w)}>
							{busy === w.workId ? '…' : 'Retry'}
						</button>
						<button
							class="act upload"
							disabled={busy === w.workId}
							onclick={() => fileInputs[w.workId]?.click()}
						>
							Upload
						</button>
						<input
							class="hidden-file"
							aria-label={`Upload cover for ${w.title ?? w.workId}`}
							type="file"
							accept="image/*"
							bind:this={fileInputs[w.workId]}
							onchange={(e) => onFilePicked(w, e)}
						/>
					</span>
				</div>
			{/each}
		</div>

		<div class="pager">
			<button
				class="pg"
				disabled={page <= 1 || loading}
				onclick={() => queueNavigate(pageState.url, { page: Math.max(1, page - 1) })}
			>
				← Prev
			</button>
			<span class="muted">Page {page}</span>
			<button
				class="pg"
				disabled={!hasNext || loading}
				onclick={() => queueNavigate(pageState.url, { page: page + 1 })}
			>
				Next →
			</button>
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
	}
	h1 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 30px;
		letter-spacing: -0.02em;
		color: var(--k-text-bright);
	}
	.lede {
		margin-top: 6px;
		font-size: 13.5px;
		color: var(--k-text-dim);
		max-width: 60ch;
	}
	.notice {
		padding: 14px 16px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		color: var(--k-text-dim);
		font-size: 13.5px;
	}
	.notice.error {
		color: #f0808a;
		border-color: rgba(240, 128, 138, 0.4);
	}
	.table {
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-lg);
		overflow: hidden;
	}
	.row {
		display: grid;
		grid-template-columns: 56px 2.4fr 1.4fr 0.6fr 1.4fr auto;
		align-items: center;
		gap: var(--k-space-4);
		padding: 12px 16px;
		border-bottom: 1px solid var(--k-border);
	}
	.row:last-child {
		border-bottom: none;
	}
	.row.head {
		background: var(--k-surface-2);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.cover {
		width: 40px;
		height: 56px;
		object-fit: cover;
		border-radius: var(--k-radius-sm);
		background: var(--k-surface-4);
		display: inline-flex;
		align-items: center;
		justify-content: center;
	}
	.cover.placeholder {
		color: var(--k-text-faint);
		font-size: 18px;
		font-weight: 700;
	}
	.work {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.wtitle {
		font-size: 14px;
		font-weight: 600;
		color: var(--k-text-1);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.wid {
		font-size: 11px;
		color: var(--k-text-faint);
		font-family: var(--k-font-mono, monospace);
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.muted {
		font-size: 13px;
		color: var(--k-text-dim);
	}
	.badge {
		display: inline-block;
		font-size: 11px;
		font-weight: 700;
		padding: 3px 8px;
		border-radius: var(--k-radius-sm);
		background: rgba(240, 128, 138, 0.14);
		color: #f0808a;
		cursor: help;
	}
	.actions-col {
		text-align: right;
	}
	.actions {
		display: flex;
		gap: 8px;
		justify-content: flex-end;
	}
	.act {
		height: 32px;
		padding: 0 12px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface);
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		font-family: var(--k-font-sans);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		white-space: nowrap;
		transition:
			color 0.15s,
			background-color 0.15s,
			border-color 0.15s;
	}
	.act:hover:not(:disabled) {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.act.upload:hover:not(:disabled) {
		border-color: rgba(95, 200, 207, 0.5);
		color: var(--k-accent-teal);
	}
	.act:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.hidden-file {
		display: none;
	}
	.pager {
		display: flex;
		align-items: center;
		gap: var(--k-space-4);
		justify-content: flex-end;
	}
	.pg {
		height: 36px;
		padding: 0 16px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface);
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
	}
	.pg:disabled {
		opacity: 0.4;
		cursor: default;
	}
	@media (max-width: 720px) {
		.page-head {
			flex-wrap: wrap;
		}
		.row {
			grid-template-columns: 1fr;
			gap: 6px;
		}
		.row.head {
			display: none;
		}
		.row > [role='cell']:nth-child(4)::before,
		.row > [role='cell']:nth-child(5)::before {
			font-weight: 700;
			color: var(--k-text-faint);
		}
		.row > [role='cell']:nth-child(4)::before {
			content: 'Tries · ';
		}
		.row > [role='cell']:nth-child(5)::before {
			content: 'Last attempt · ';
		}
		.actions {
			justify-content: flex-start;
			flex-wrap: wrap;
		}
		.act,
		.pg {
			height: 40px;
		}
		.pager {
			justify-content: flex-start;
		}
	}
</style>
