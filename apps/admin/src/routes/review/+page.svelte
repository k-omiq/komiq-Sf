<script lang="ts">
	import { href } from '$lib/paths';
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type { MergeCandidate, WorkReviewDetail } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { assetSrc } from '$lib/config';
	import { confirm } from '$lib/confirm.svelte';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import {
		loadMergeQueue,
		loadWorkReviewDetail,
		MERGE_QUEUE_PAGE_SIZE,
		resolveMergeCandidate,
	} from '$lib/data';

	// ---- server-side paging ----------------------------------------------------
	// `mergeQueue` is paginated on the server (page/limit, `total` = the pending
	// count). The backlog is ~10k rows, so only the current page is ever held here.
	let queue = $state<MergeCandidate[]>([]);
	const page = $derived(queuePage(pageState.url));
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let busy = $state<string | null>(null); // candidate id currently being resolved
	let busyAccept = $state(false); // which of that row's two buttons is in flight
	/**
	 * Rows resolved since this page was fetched — the OFFSET DRIFT.
	 *
	 * `mergeQueue` is offset-paged (`OFFSET (page-1)*limit`, `ORDER BY score DESC`) over
	 * a list that resolving a row REMOVES from. Resolve k rows on page p and every row
	 * after them shifts k slots earlier, so k rows that were the head of page p+1 fall
	 * back into page p's slot — and asking the server for p+1 steps straight over them.
	 * With a ~10k backlog those candidates are then never seen again.
	 *
	 * The page/limit API can't express "offset 50p − k", so Next re-reads the CURRENT
	 * page while this is non-zero: same 50-slot window, now holding the rows that
	 * shifted in. It converges (the rows we resolved are gone from it) and it never
	 * skips. A full drain is the special case where the shift is exactly one page, and
	 * {@link settleAfterResolve} already re-reads the same page number for it.
	 */
	let resolvedOnPage = $state(0);

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
			let res = await loadMergeQueue(p);
			// Landed past the end of a backlog that shrank under us (another admin, or a
			// consolidation run). The server echoes the requested page rather than
			// clamping, and the pager isn't rendered for an empty table — so without this
			// the admin is stranded on "the queue is clear" beside a non-zero count, and
			// Refresh just re-reads the same empty page. Jump to the real last page; one
			// extra round-trip, bounded (the second read is not re-clamped).
			if (res.items.length === 0 && res.page > 1 && (res.total ?? 0) > 0) {
				const last = Math.max(1, Math.ceil((res.total ?? 1) / MERGE_QUEUE_PAGE_SIZE));
				if (last < res.page) res = await loadMergeQueue(last);
			}
			if (seq !== loadSeq) return;
			queue = res.items;
			if (res.page !== p) {
				void queueNavigate(pageState.url, { page: res.page }, true);
				return;
			}
			hasNext = res.hasNextPage;
			total = res.total ?? null;
			resolvedOnPage = 0;
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load the review queue.';
			queue = [];
			hasNext = false;
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	/**
	 * Step forward without stepping OVER anything — see {@link resolvedOnPage}. Once
	 * this page has drifted, `page + 1` is the wrong window and `page` is the right one.
	 */
	function nextPage(): void {
		if (resolvedOnPage > 0) void refresh(page);
		else void queueNavigate(pageState.url, { page: page + 1 });
	}

	/** 1-based index of the first row on this page, within the whole backlog. */
	const rangeStart = $derived(queue.length === 0 ? 0 : (page - 1) * MERGE_QUEUE_PAGE_SIZE + 1);
	const rangeEnd = $derived((page - 1) * MERGE_QUEUE_PAGE_SIZE + queue.length);

	/**
	 * Rows are removed optimistically as they resolve, so a page can drain while the
	 * backlog is still large. Re-pull rather than leave an empty table that reads as
	 * "queue clear": the SAME page number is the right window, because a full drain
	 * shifts the backlog by exactly one page and the server backfills this slot from
	 * what used to be the next one. Resolving the last row on the LAST page reads as
	 * empty, and `refresh()`'s past-the-end clamp walks it back.
	 */
	async function settleAfterResolve(): Promise<void> {
		if (queue.length === 0 && total != null && total > 0) await refresh(page);
	}

	// ---- detail panel ----------------------------------------------------------
	// Clicking either side's cover or title expands that WORK — deciding a merge means
	// looking at two entries, and title-plus-id is not enough to tell "Blue Lock" from
	// "Blue Lock: Episode Nagi". Opened as an overlay rather than a navigation so the
	// reviewer keeps their page position in a ~10k-row offset-paged backlog.
	let detailFor = $state<{ workId: string; label: string; side: 'source' | 'candidate' } | null>(
		null,
	);
	let detail = $state<WorkReviewDetail | null>(null);
	let detailLoading = $state(false);
	let detailError = $state<string | null>(null);

	async function openDetail(
		workId: string,
		label: string,
		side: 'source' | 'candidate',
	): Promise<void> {
		detailFor = { workId, label, side };
		detail = null;
		detailError = null;
		detailLoading = true;
		try {
			const res = await loadWorkReviewDetail(workId);
			// Clicking a second row before the first resolves must not paint the stale
			// work into the open panel — only accept the response still being asked for.
			if (detailFor?.workId !== workId) return;
			detail = res;
		} catch (err) {
			if (detailFor?.workId !== workId) return;
			detailError = err instanceof Error ? err.message : 'Failed to load this work.';
		} finally {
			if (detailFor?.workId === workId) detailLoading = false;
		}
	}

	function closeDetail(): void {
		detailFor = null;
		detail = null;
		detailError = null;
		detailLoading = false;
	}

	/**
	 * `/series/{id}` loads through `canonicalSeries`, which REFUSES a work with no
	 * MangaDex anchor — and Suwayomi-only entries are exactly what fills this queue. Only
	 * offer the link when the work has a MangaDex mapping, rather than sending the
	 * reviewer to a "No such work" page.
	 */
	const editorHref = $derived(
		detail?.sources.some((s) => s.sourceType === 'mangadex')
			? href(`/series/${detail.workId}`)
			: null,
	);

	function fmtDate(iso: string): string {
		const t = Date.parse(iso);
		return Number.isNaN(t) ? 'Unknown' : new Date(t).toLocaleDateString();
	}

	/** `title_corroborated` reads as "Title corroborated"; the raw value stays in a tooltip. */
	function methodLabel(method: string): string {
		const words = method.replace(/_/g, ' ').trim();
		return words ? words.charAt(0).toUpperCase() + words.slice(1) : 'Unknown';
	}

	function pct(score: number): string {
		return `${Math.round(score * 100)}%`;
	}

	async function resolve(c: MergeCandidate, accept: boolean): Promise<void> {
		if (busy) return;
		const source = c.sourceTitle ?? 'Untitled';
		const candidate = c.candidateTitle ?? 'Untitled';
		if (
			accept &&
			!(await confirm({
				title: 'Merge these two entries?',
				message:
					`“${source}” will be merged into “${candidate}” and deleted as a separate entry. ` +
					`This cannot be undone.`,
				confirmLabel: 'Merge',
				danger: true,
				skipKey: 'merge-candidate',
			}))
		)
			return;
		// The confirm is async now: another row may have started resolving meanwhile.
		if (busy) return;
		busy = c.id;
		busyAccept = accept;
		try {
			// The resolver has no "already resolved" SUCCESS value — every exit path is
			// either an error or a true. So a concurrent resolve arrives as a throw, and
			// the stale-row cleanup has to live in the catch below.
			await resolveMergeCandidate(c.id, accept);
			queue = queue.filter((x) => x.id !== c.id);
			resolvedOnPage += 1;
			if (total != null && total > 0) total -= 1;
			toast.success(
				accept ? `Merged “${source}” into “${candidate}”.` : `Kept “${source}” separate.`,
			);
			await settleAfterResolve();
		} catch (err) {
			const why = err instanceof Error ? err.message : 'Action failed.';
			// "already resolved" / "no such candidate" => the row is a phantom another
			// admin (or a consolidation run) already closed. Drop it and re-pull the
			// authoritative queue instead of leaving it there to be clicked again.
			const stale = /already resolved|no such merge candidate|not found/i.test(why);
			toast.error(stale ? `${why} Reloading the list.` : why);
			if (stale) {
				await refresh();
				await settleAfterResolve();
			}
		} finally {
			busy = null;
		}
	}

	const count = $derived(total ?? queue.length);
	const lead = $derived(
		loading && total == null && queue.length === 0
			? 'Pairs'
			: `${total == null && hasNext ? `More than ${count}` : count} pair${count === 1 ? '' : 's'}`,
	);
</script>

<div class="k-page">
	<PageHeader
		title="Duplicates"
		description="{lead} the matcher was not sure about. Merge a pair if both are the same series, or keep them separate."
	>
		{#snippet actions()}
			<Button icon="refresh" busy={loading} onclick={() => refresh()}>Refresh</Button>
		{/snippet}
		<a class="k-link bulk" href={href('/settings#maintenance')}
			>Merge many duplicates at once in Settings</a
		>
	</PageHeader>

	<LoadState
		loading={loading && queue.length === 0}
		error={loadError}
		empty={queue.length === 0}
		emptyTitle="No duplicates to review"
		emptyHint="A pair shows up here when the matcher finds two series that might be the same and cannot decide. Nothing is waiting right now."
		emptyIcon="layers"
		onretry={() => refresh()}
	>
		<div class="k-table" class:is-dim={loading} role="table" aria-label="Dedup review queue">
			<div class="k-tr k-tr--head" role="row">
				<span role="columnheader">Source series</span>
				<span role="columnheader">Candidate work</span>
				<span role="columnheader">Confidence</span>
				<span role="columnheader">Matched by</span>
				<span role="columnheader">Queued</span>
				<span role="columnheader" class="k-td--end">Decision</span>
			</div>
			{#each queue as c (c.id)}
				<div class="k-tr" role="row">
					<span class="work-cell" role="cell" data-caption="Source series">
						<button
							type="button"
							class="work"
							title="Source series {c.sourceSeriesId}. Open details."
							onclick={() => openDetail(c.sourceWorkId, c.sourceTitle ?? 'Untitled', 'source')}
						>
							<Cover src={c.sourceCoverUrl ? assetSrc(c.sourceCoverUrl) : null} />
							<span class="wtext">
								<span class="wtitle k-truncate">{c.sourceTitle ?? 'Untitled'}</span>
								<span class="k-mono k-muted k-truncate">{c.sourceWorkId}</span>
							</span>
						</button>
					</span>
					<span class="work-cell" role="cell" data-caption="Candidate work">
						<button
							type="button"
							class="work"
							title="Candidate work {c.candidateWorkId}. Open details."
							onclick={() =>
								openDetail(c.candidateWorkId, c.candidateTitle ?? 'Untitled', 'candidate')}
						>
							<Cover src={c.candidateCoverUrl ? assetSrc(c.candidateCoverUrl) : null} />
							<span class="wtext">
								<span class="wtitle k-truncate">{c.candidateTitle ?? 'Untitled'}</span>
								<span class="k-mono k-muted k-truncate">{c.candidateWorkId}</span>
							</span>
						</button>
					</span>
					<span role="cell" data-label="Confidence">
						<span class="k-badge k-num" data-tone={c.score >= 0.75 ? 'info' : undefined}
							>{pct(c.score)}</span
						>
					</span>
					<span class="k-muted" role="cell" data-label="Matched by" title={c.method}
						>{methodLabel(c.method)}</span
					>
					<span class="k-muted k-num" role="cell" data-label="Queued">{fmtDate(c.createdAt)}</span>
					<span class="k-td--end" role="cell">
						<Button
							size="sm"
							icon="merge"
							busy={busy === c.id && busyAccept}
							disabled={busy === c.id}
							onclick={() => resolve(c, true)}
						>
							Merge
						</Button>
						<Button
							size="sm"
							busy={busy === c.id && !busyAccept}
							disabled={busy === c.id}
							onclick={() => resolve(c, false)}
						>
							Keep separate
						</Button>
					</span>
				</div>
			{/each}
		</div>

		{#if page > 1 || hasNext}
			<div class="paging">
				<Pager
					{page}
					{hasNext}
					busy={loading}
					detail="({rangeStart} to {rangeEnd}{total != null ? ` of ${total}` : ''})"
					onpage={(n) => {
						if (n > page) nextPage();
						else void queueNavigate(pageState.url, { page: Math.max(1, n) });
					}}
				/>
				{#if resolvedOnPage > 0}
					<p class="drift">
						You decided {resolvedOnPage} pair{resolvedOnPage === 1 ? '' : 's'} on this page, so the pairs
						behind {resolvedOnPage === 1 ? 'it' : 'them'} moved up. Next reloads this page so none are
						skipped.
					</p>
				{/if}
			</div>
		{/if}
	</LoadState>
</div>

{#if detailFor}
	{@const target = detailFor}
	<Dialog
		size="lg"
		title={detail?.title ?? target.label}
		description={target.side === 'source' ? 'Source series' : 'Candidate work'}
		onclose={closeDetail}
	>
		<LoadState
			loading={detailLoading || (!detail && !detailError)}
			error={detailError}
			loadingLabel="Loading details…"
			skeletonRows={3}
			onretry={() => openDetail(target.workId, target.label, target.side)}
		>
			{#if detail}
				<div class="detail">
					<Cover src={detail.coverUrl ? assetSrc(detail.coverUrl) : null} width={132} />
					<div class="facts">
						<span class="k-mono k-muted workid">{detail.workId}</span>
						<div class="chips">
							{#if detail.isNsfw}<span class="k-badge" data-tone="danger">NSFW</span>{/if}
							{#if detail.status}<span class="k-badge">{detail.status}</span>{/if}
							{#if detail.year}<span class="k-badge k-num">{detail.year}</span>{/if}
							{#if detail.contentRating}<span class="k-badge">{detail.contentRating}</span>{/if}
							{#if detail.originalLanguage}<span class="k-badge">{detail.originalLanguage}</span
								>{/if}
							<span class="k-badge k-num"
								>{detail.chapterCount} chapter{detail.chapterCount === 1 ? '' : 's'}</span
							>
						</div>
						{#if detail.author || detail.artist}
							<p class="by">
								{detail.author ?? 'Unknown author'}{detail.artist && detail.artist !== detail.author
									? `, art by ${detail.artist}`
									: ''}
							</p>
						{/if}
						{#if detail.description}
							<p class="desc">{detail.description}</p>
						{/if}
						{#if detail.altTitles.length > 0}
							<div class="facet">
								<span class="k-label">Also known as</span>
								<div class="chips wrap">
									{#each detail.altTitles as t (t)}
										<span class="k-badge">{t}</span>
									{/each}
								</div>
							</div>
						{/if}
						<div class="facet">
							<span class="k-label">Sources ({detail.sources.length})</span>
							{#if detail.sources.length === 0}
								<span class="k-muted note">No sources are linked to this work.</span>
							{:else}
								<div class="chips wrap">
									{#each detail.sources as s (`${s.sourceType}:${s.sourceId}:${s.sourceKey}`)}
										<span class="k-badge" title="{s.sourceType}: {s.sourceKey}">
											{s.extension?.pkgName?.split('.').pop() ?? s.sourceType}{s.lang
												? ` (${s.lang})`
												: ''}
										</span>
									{/each}
								</div>
							{/if}
						</div>
						{#if editorHref}
							<div>
								<Button
									size="sm"
									icon="external"
									href={editorHref}
									target="_blank"
									rel="noreferrer"
								>
									Open in the catalogue editor
								</Button>
							</div>
						{:else}
							<span class="k-muted note">
								This work has no MangaDex mapping, so the catalogue editor cannot open it.
							</span>
						{/if}
					</div>
				</div>
			{/if}
		</LoadState>
	</Dialog>
{/if}

<style>
	.k-table {
		--cols: minmax(0, 2fr) minmax(0, 2fr) minmax(0, 0.7fr) minmax(0, 1.1fr) minmax(0, 0.7fr) 216px;
	}
	.bulk {
		font-size: var(--k-text-sm);
	}
	.work {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		width: 100%;
		min-width: 0;
		padding: 0;
		background: none;
		border: none;
		border-radius: var(--k-radius-sm);
		text-align: left;
		cursor: pointer;
	}
	.wtext {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.wtitle {
		color: var(--k-text-bright);
		font-weight: 700;
	}
	.work:hover .wtitle,
	.work:focus-visible .wtitle {
		text-decoration: underline;
		text-underline-offset: 3px;
	}
	.paging {
		display: grid;
		gap: var(--k-space-2);
		justify-items: center;
	}
	.drift {
		max-width: 72ch;
		color: var(--k-warning);
		font-size: var(--k-text-xs);
		line-height: 1.55;
		text-align: center;
	}

	/* ---- detail dialog ---- */
	.detail {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		gap: var(--k-space-5);
		align-items: start;
	}
	.facts {
		display: grid;
		gap: var(--k-space-3);
		min-width: 0;
	}
	.workid {
		overflow-wrap: anywhere;
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	/* Alternative titles and source names can be long: let them wrap inside the
	   dialog instead of forcing it to scroll sideways. */
	.chips.wrap .k-badge {
		height: auto;
		min-height: 22px;
		padding: 4px 8px;
		line-height: 1.35;
		font-weight: 600;
		white-space: normal;
		overflow-wrap: anywhere;
	}
	.by {
		font-size: var(--k-text-sm);
		color: var(--k-text-1);
	}
	.desc {
		font-size: var(--k-text-sm);
		line-height: 1.6;
		color: var(--k-text-dim);
		/* A synopsis is context, not the point — cap it so the sources and alt titles
		   below stay visible without scrolling past a wall of text. */
		max-height: 9.5em;
		overflow-y: auto;
		white-space: pre-wrap;
	}
	.facet {
		display: grid;
		gap: 6px;
	}
	.note {
		font-size: var(--k-text-xs);
		line-height: 1.55;
	}

	@media (max-width: 720px) {
		/* Both works get their column name back as a caption above the title, so the
		   pair still reads as "source" and "candidate" once the row stacks. */
		.work-cell::before {
			content: attr(data-caption);
			display: block;
			margin-bottom: 6px;
			color: var(--k-text-dim);
			font-size: 11px;
			font-weight: 700;
			letter-spacing: 0.05em;
			text-transform: uppercase;
		}
		.detail {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
