<script lang="ts">
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type { CoverIssue } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { assetSrc } from '$lib/config';
	import { loadCoverIssues, retryCover, uploadCover } from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import CoverPrune from './CoverPrune.svelte';
	import CoverRanking from './CoverRanking.svelte';

	let issues = $state<CoverIssue[]>([]);
	const page = $derived(queuePage(pageState.url));
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let busy = $state<string | null>(null); // workId currently acting
	// Which of the row's two actions is in flight, so only that button spins.
	let busyAction = $state<'retry' | 'upload' | null>(null);
	// Hidden file inputs keyed by workId, so each row's "Upload" button opens its own.
	let fileInputs = $state<Record<string, HTMLInputElement>>({});

	const REASON_LABEL: Record<string, string> = {
		too_large: 'Image too large',
		unsupported: 'Unsupported or damaged file',
		empty: 'Image is empty',
		encode: 'Could not convert',
		store: 'Could not save',
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
		return Number.isNaN(t) ? 'Unknown' : new Date(t).toLocaleString();
	}

	function nameOf(w: CoverIssue): string {
		return w.title ?? w.workId;
	}

	// After an optimistic removal empties the current page while issues still exist
	// elsewhere, reload so we don't show the "No cover issues" empty state (and hide the
	// pager) on a page that only looks empty. Stay on the page if more are queued ahead
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
		busyAction = 'retry';
		try {
			const stored = await retryCover(w.workId);
			if (stored) {
				// Fixed — drop it from the list.
				issues = issues.filter((x) => x.workId !== w.workId);
				if (total != null) total -= 1;
				toast.success(`Cover fixed for ${nameOf(w)}.`);
				await settleAfterFix();
			} else {
				toast.error(
					`No source image could be fetched for ${nameOf(w)}. Try again later or upload one.`,
				);
			}
		} catch (err) {
			// A deterministic re-failure refreshes the reason server-side; reload so the
			// row shows the latest attempt count / reason.
			toast.fromError(err, 'Retry failed. Try again or upload a cover.');
			await refresh();
		} finally {
			busy = null;
			busyAction = null;
		}
	}

	async function onFilePicked(w: CoverIssue, ev: Event): Promise<void> {
		const input = ev.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		input.value = ''; // allow re-picking the same file later
		if (!file || busy) return;
		busy = w.workId;
		busyAction = 'upload';
		try {
			await uploadCover(w.workId, file);
			issues = issues.filter((x) => x.workId !== w.workId);
			if (total != null) total -= 1;
			toast.success(`Cover uploaded for ${nameOf(w)}.`);
			await settleAfterFix();
		} catch (err) {
			toast.fromError(err, 'Upload failed. Check the file is an image and try again.');
		} finally {
			busy = null;
			busyAction = null;
		}
	}

	const count = $derived(total ?? issues.length);
</script>

<div class="k-page covers">
	<PageHeader
		title="Covers"
		description="Choose which source covers come from, clear out covers no series uses, and fix covers that failed."
	/>

	<CoverRanking />

	<CoverPrune />

	<section class="section" aria-labelledby="cover-issues-h">
		<div class="section-head section-head--row">
			<div>
				<h2 id="cover-issues-h" class="section-title">Cover issues</h2>
				<p class="section-desc">
					{count} cover{count === 1 ? '' : 's'} could not be processed. Retry fetches and converts the
					image again. Upload replaces it with a file from your device.
				</p>
			</div>
			<Button icon="refresh" busy={loading} onclick={() => refresh()}>Refresh</Button>
		</div>

		<LoadState
			loading={loading && issues.length === 0}
			error={loadError}
			empty={issues.length === 0}
			emptyTitle={page > 1 ? 'No cover issues on this page' : 'No cover issues'}
			emptyHint={page > 1
				? 'This page is past the end of the list.'
				: 'Every cover was processed without a problem. Covers that fail to download or convert are listed here.'}
			emptyIcon="check"
			onretry={() => refresh()}
		>
			{#snippet emptyActions()}
				<!-- An empty page past the first (a stale link, or the list shrank) has no table
			     and so no pager: give the way back here. -->
				{#if page > 1}
					<Button size="sm" onclick={() => queueNavigate(pageState.url, { page: null })}>
						Go to the first page
					</Button>
				{/if}
			{/snippet}
			<div class="k-table" class:is-dim={loading} role="table" aria-label="Cover issues">
				<div class="k-tr k-tr--head" role="row">
					<span role="columnheader">Cover</span>
					<span role="columnheader">Work</span>
					<span role="columnheader">Reason</span>
					<span role="columnheader">Tries</span>
					<span role="columnheader">Last attempt</span>
					<span role="columnheader" class="k-td--end">Fix</span>
				</div>
				{#each issues as w (w.workId)}
					<div class="k-tr" role="row">
						<span class="thumb" role="cell">
							<Cover src={assetSrc(w.coverUrl)} width={40} />
						</span>
						<span class="work" role="cell">
							<span class="title k-truncate">{w.title ?? 'Untitled'}</span>
							<span class="k-mono k-muted k-truncate">{w.workId}</span>
						</span>
						<span role="cell" data-label="Reason">
							<span class="reason">
								<span class="k-badge" data-tone="danger">{REASON_LABEL[w.reason] ?? w.reason}</span>
								{#if w.detail}<span class="detail k-muted" title={w.detail}>{w.detail}</span>{/if}
							</span>
						</span>
						<span class="k-muted k-num" role="cell" data-label="Tries">{w.attempts}</span>
						<span class="k-muted k-num" role="cell" data-label="Last attempt"
							>{fmtDate(w.lastSeen)}</span
						>
						<span class="k-td--end" role="cell">
							<Button
								size="sm"
								icon="refresh"
								busy={busy === w.workId && busyAction === 'retry'}
								disabled={busy === w.workId}
								onclick={() => retry(w)}
							>
								Retry
							</Button>
							<Button
								size="sm"
								icon="upload"
								busy={busy === w.workId && busyAction === 'upload'}
								disabled={busy === w.workId}
								onclick={() => fileInputs[w.workId]?.click()}
							>
								Upload
							</Button>
							<input
								class="file"
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

			<Pager
				{page}
				{hasNext}
				busy={loading}
				onpage={(n) => queueNavigate(pageState.url, { page: Math.max(1, n) })}
			/>
		</LoadState>
	</section>
</div>

<style>
	/* Section headings shared with CoverRanking and CoverPrune: spacing, no rules. */
	.covers {
		gap: var(--k-space-10);
	}
	.covers :global(.section) {
		display: grid;
		gap: var(--k-space-4);
		min-width: 0;
	}
	.covers :global(.section-head) {
		display: grid;
		gap: 6px;
	}
	.section-head--row {
		grid-template-columns: minmax(0, 1fr) auto;
		align-items: start;
		column-gap: var(--k-space-4);
	}
	.covers :global(.section-title) {
		font-size: var(--k-text-xl);
		font-weight: 700;
	}
	.covers :global(.section-desc) {
		max-width: 66ch;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		line-height: 1.6;
	}
	.k-table {
		/* The last track is fixed, not auto: each row is its own grid, so an auto track
		   would size differently in the head row and the columns would not line up. */
		--cols: 40px minmax(0, 2fr) minmax(0, 1.8fr) minmax(0, 0.5fr) minmax(0, 1.2fr) 176px;
	}
	.thumb {
		display: flex;
	}
	.work {
		display: grid;
		gap: 2px;
	}
	.title {
		color: var(--k-text-bright);
		font-weight: 700;
	}
	.reason {
		display: grid;
		justify-items: start;
		gap: 4px;
		min-width: 0;
	}
	/* The server's own failure text: useful, but it can be a long unbroken string. */
	.detail {
		display: -webkit-box;
		-webkit-box-orient: vertical;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		overflow: hidden;
		overflow-wrap: anywhere;
		font-size: var(--k-text-xs);
		line-height: 1.45;
	}
	.file {
		display: none;
	}
	@media (max-width: 720px) {
		.covers {
			gap: var(--k-space-8);
		}
		/* Keep the thumbnail beside the title; every other cell takes the full row. */
		.k-tr {
			grid-template-columns: 40px minmax(0, 1fr);
			column-gap: var(--k-space-3);
		}
		.k-tr > :not(.thumb, .work) {
			grid-column: 1 / -1;
		}
		/* Badge sits opposite the caption; the failure text takes its own full line. */
		.k-tr > [data-label='Reason'] {
			flex-wrap: wrap;
			row-gap: 6px;
		}
		.reason {
			display: contents;
		}
		.detail {
			flex: 1 1 100%;
			text-align: left;
		}
	}
</style>
