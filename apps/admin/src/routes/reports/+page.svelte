<script lang="ts">
	import { href } from '$lib/paths';
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type { Report, ReportKind, ReportStatus } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { beforeNavigate } from '$app/navigation';
	import { confirm } from '$lib/confirm.svelte';
	import { deleteComment, loadReports, resolveReport } from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import Segmented from '$lib/components/Segmented.svelte';

	let reports = $state<Report[]>([]);
	const page = $derived(queuePage(pageState.url));
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let openCount = $state(0);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let busy = $state<string | null>(null); // report id currently acting
	/** Which button on that row is in flight, so only that one shows the spinner. */
	let busyAction = $state<ReportStatus | 'DELETE' | null>(null);

	const status = $derived<ReportStatus | null>(
		pageState.url.searchParams.get('status') === 'all'
			? null
			: pageState.url.searchParams.get('status') === 'RESOLVED'
				? 'RESOLVED'
				: pageState.url.searchParams.get('status') === 'REJECTED'
					? 'REJECTED'
					: 'OPEN',
	);
	const kind = $derived<ReportKind | null>(
		['WRONG_MERGE', 'NEEDS_MERGE', 'MISSING_WORK', 'COMMENT_ABUSE', 'OTHER'].includes(
			pageState.url.searchParams.get('kind') ?? '',
		)
			? (pageState.url.searchParams.get('kind') as ReportKind)
			: null,
	);
	/** Per-row triage note, only sent for the row it was typed on. */
	let notes = $state<Record<string, string>>({});
	beforeNavigate(({ cancel, to, willUnload }) => {
		// Query-only queue navigation preserves note drafts in this component.
		if (!willUnload && to?.url.pathname === pageState.url.pathname) return;
		// Native and synchronous on purpose: `cancel()` must run before this callback
		// returns, so the promise-based `confirm()` dialog cannot be used here.
		if (
			Object.values(notes).some((note) => note.trim()) &&
			!window.confirm('Leave this page and discard your unsaved report notes?')
		)
			cancel();
	});
	/** Which row's detail/note panel is expanded. */
	let openRow = $state<string | null>(null);

	/** `Segmented` needs string values, so "every status" is `all` here and `null` everywhere else. */
	type StatusFilter = ReportStatus | 'all';
	const statusOptions = $derived<{ label: string; value: StatusFilter; count?: number | null }[]>([
		{ label: 'Open', value: 'OPEN', count: openCount > 0 ? openCount : null },
		{ label: 'Resolved', value: 'RESOLVED' },
		{ label: 'Rejected', value: 'REJECTED' },
		{ label: 'All', value: 'all' },
	]);

	const STATUS_LABEL: Record<ReportStatus, string> = {
		OPEN: 'Open',
		RESOLVED: 'Resolved',
		REJECTED: 'Rejected',
	};

	const KIND_LABEL: Record<ReportKind, string> = {
		WRONG_MERGE: 'Wrongly merged',
		NEEDS_MERGE: 'Needs merging',
		MISSING_WORK: 'Missing series',
		COMMENT_ABUSE: 'Comment abuse',
		OTHER: 'Other',
	};

	const KIND_TONE: Record<ReportKind, 'warning' | 'info' | 'danger' | undefined> = {
		WRONG_MERGE: 'warning',
		NEEDS_MERGE: 'warning',
		MISSING_WORK: 'info',
		COMMENT_ABUSE: 'danger',
		OTHER: undefined,
	};

	const KIND_FILTERS: { label: string; value: ReportKind | null }[] = [
		{ label: 'Every kind', value: null },
		...(Object.keys(KIND_LABEL) as ReportKind[]).map((k) => ({ label: KIND_LABEL[k], value: k })),
	];

	// Auth redirect is centralized in +layout.svelte. This effect is the SINGLE fetch
	// driver: filters and pager update the URL, then its derived values refetch the queue.
	$effect(() => {
		if (!auth.user) return;
		void refresh(status, kind, page);
	});

	/**
	 * Monotonic request token. The status tabs and the kind select are NOT disabled while
	 * a fetch is in flight (deliberately — a stuck request must not trap the admin on one
	 * filter), so two fetches can overlap and land out of order: a slow "Open" landing
	 * after a fast "All" would paint the wrong list under the wrong selected tab, with the
	 * wrong `total`, and `loading` would already have been cleared by the loser. Only the
	 * newest request is allowed to write state. Same guard as `series/[id]`'s `loadSeq`.
	 */
	let loadSeq = 0;

	async function refresh(s: ReportStatus | null, k: ReportKind | null, p: number): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		try {
			const res = await loadReports(s, k, p);
			if (seq !== loadSeq) return;
			reports = res.items;
			hasNext = res.hasNextPage;
			total = res.total ?? null;
			openCount = res.openCount;
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load reports.';
			reports = [];
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	function setStatus(v: ReportStatus | null): void {
		if (v === status) return;
		void queueNavigate(pageState.url, { status: v === 'OPEN' ? null : (v ?? 'all'), page: 1 });
	}

	function setKind(v: ReportKind | null): void {
		if (v === kind) return;
		void queueNavigate(pageState.url, { kind: v, page: 1 });
	}

	/**
	 * An optimistic removal can empty the CURRENT page while the queue still has rows —
	 * triage the 50 open reports on page 1 of 3 and the list renders "the queue is clear"
	 * over a backlog of 100. Reload instead: stay on this page when the server can backfill
	 * it (`hasNext`), else step back one. Mirrors `bugs`'s `settleAfterFix`.
	 */
	async function settleAfterTriage(): Promise<void> {
		if (reports.length === 0 && (hasNext || page > 1)) {
			const target = hasNext ? page : Math.max(1, page - 1);
			// Assigning `page` re-fires the driving effect; only refresh directly when the
			// page number does not change (otherwise both would fire).
			if (target === page) await refresh(status, kind, page);
			else await queueNavigate(pageState.url, { page: target }, true);
		}
	}

	/**
	 * Comment ids deleted during this session.
	 *
	 * Tracked client-side because there is nothing on the report to read it back from:
	 * the comment is hard-deleted, and the report's own `status` is about the REPORT, not
	 * about the comment. Without this the button stays live and a second click 500s on a
	 * row that is already gone.
	 */
	let deleted = $state(new Set<string>());

	async function removeComment(r: Report): Promise<void> {
		if (busy) return; // one write at a time, as in `triage`
		if (!r.commentId || deleted.has(r.commentId)) return;
		const ok = await confirm({
			title: 'Delete this comment?',
			message:
				'This deletes the comment and every reply under it. It cannot be undone. The report keeps its copy of the comment.',
			confirmLabel: 'Delete comment',
			danger: true,
		});
		// Re-check after the await: the guards above were read before the dialog opened.
		if (!ok || busy || deleted.has(r.commentId)) return;
		busy = r.id;
		busyAction = 'DELETE';
		try {
			await deleteComment(r.commentId);
			// Reassign, don't mutate. `$state` only deep-proxies plain objects and arrays
			// (svelte/internal/client/proxy.js bails on any other prototype), so this Set is
			// the RAW Set — `.add()` notifies nothing and the template's `deleted.has(...)`
			// would never re-run. Reassigning the variable is what publishes the change.
			// (`SvelteSet` from `svelte/reactivity` would also work; nothing else in the repo
			// uses it, and this set holds a handful of ids for one session.)
			deleted = new Set(deleted).add(r.commentId);
			toast.success('Comment deleted.');
		} catch (err) {
			toast.fromError(err, 'Could not delete that comment.');
		} finally {
			busy = null;
			busyAction = null;
		}
	}

	async function triage(r: Report, next: ReportStatus): Promise<void> {
		// One triage at a time (the `users`/`bugs` convention): the counter bookkeeping
		// below reads `r.status` captured before the await, so overlapping mutations on
		// two rows would each reconcile against a list the other has already changed.
		if (busy) return;
		busy = r.id;
		busyAction = next;
		try {
			const note = notes[r.id]?.trim() || null;
			const updated = await resolveReport(r.id, next, note);
			// The list is a filtered view, so a row that no longer matches the active status
			// filter is DROPPED rather than left showing a state the filter excludes.
			if (status !== null && updated.status !== status) {
				reports = reports.filter((x) => x.id !== r.id);
				if (total != null) total = Math.max(0, total - 1);
			} else {
				reports = reports.map((x) => (x.id === r.id ? updated : x));
			}
			// `openCount` is the GLOBAL backlog, so it moves on the transition itself, not on
			// whether the row survived the filter. `updated.status` (not `next`) is the
			// server's answer, and `r.status` is this client's last known value — the count is
			// exact for our own actions and only drifts if another admin triages concurrently,
			// which the next refresh corrects.
			openCount = Math.max(
				0,
				openCount + (r.status === 'OPEN' ? -1 : 0) + (updated.status === 'OPEN' ? 1 : 0),
			);
			delete notes[r.id];
			if (openRow === r.id) openRow = null;
			toast.success(
				updated.status === 'RESOLVED'
					? 'Report marked resolved.'
					: updated.status === 'REJECTED'
						? 'Report rejected.'
						: 'Report reopened.',
			);
			await settleAfterTriage();
		} catch (err) {
			toast.fromError(err, 'Could not update that report.');
		} finally {
			busy = null;
			busyAction = null;
		}
	}

	function fmtDate(iso: string): string {
		const t = Date.parse(iso);
		return Number.isNaN(t) ? 'Unknown' : new Date(t).toLocaleString();
	}

	/**
	 * Link a reported series id to THIS console's own `/series/[id]` editor — not to the
	 * reader. The admin app has no reader-origin config (`$lib/config` only knows the API
	 * endpoint), so a reader URL could not be built from here anyway, and the editor is
	 * where a wrong-merge report is actually acted on.
	 *
	 * The id space matches: `/` links the same `/series/{id}` with reader ids, and the
	 * server refuses a `subjectId` that `known_series_id` doesn't recognise, so a numeric
	 * Suwayomi id and a `w_` canonical id both resolve.
	 */
	function seriesHref(id: string): string {
		// `encodeURIComponent` escapes `:`, so an id can only ever be one path segment under
		// /series/ — it can never introduce a scheme. (The only other reader-controlled URL
		// on this page is `sourceUrl`; see `safeHref`. There are no `src`/`style`/`{@html}`
		// sinks here at all.)
		return href(`/series/${encodeURIComponent(id)}`);
	}

	/**
	 * The URL a reporter typed, if it is safe to make clickable — else null.
	 *
	 * `sourceUrl` is reader-supplied, arrives through an UNAUTHENTICATED mutation, and is
	 * rendered as a real `<a href>` in a signed-in admin's console. Svelte escapes text but
	 * does NOT sanitize href schemes, so `javascript:…` here is script execution on the
	 * console's origin with the admin's session, one click away.
	 *
	 * `submitReport` already allowlists http(s) at write time. This is the second layer,
	 * because that one is a single check on an endpoint anyone can post to, and it does not
	 * retro-validate rows filed before it existed — and this console is what renders those.
	 *
	 * Parsed with `URL` rather than a `startsWith` test on purpose: the parser strips the
	 * leading whitespace and the embedded tabs/newlines that defeat a string prefix check
	 * (`" javascript:"`, `"java\nscript:"` both normalise to the `javascript:` protocol),
	 * and a non-absolute string throws, which also fails closed.
	 *
	 * A rejected URL is still SHOWN, as plain text — hiding it would hide the evidence the
	 * report is about.
	 */
	function safeHref(url: string): string | null {
		let u: URL;
		try {
			u = new URL(url);
		} catch {
			return null;
		}
		return u.protocol === 'http:' || u.protocol === 'https:' ? url : null;
	}
</script>

<div class="k-page">
	<PageHeader title="Reports">
		Reports that readers file from the reader's Support page.
		<span class="k-num">{openCount}</span> open{total != null ? `, ${total} in this view` : ''}.
		Merge two entries from <a class="k-link" href={href('/')}>Catalog</a>, because
		<a class="k-link" href={href('/review')}>Duplicates</a> only lists pairs Komiq flagged itself.
		Ban accounts in <a class="k-link" href={href('/users')}>Users</a>.
		{#snippet actions()}
			<Button icon="refresh" busy={loading} onclick={() => refresh(status, kind, page)}>
				Refresh
			</Button>
		{/snippet}
	</PageHeader>

	<div class="k-toolbar filters">
		<!--
			`Segmented` renders toggle buttons in a labelled group, NOT role="tablist"/role="tab".
			A `tab` must own a `tabpanel` via aria-controls and implement arrow-key roving focus;
			these do neither, so tab roles would promise a widget that isn't there ("tab 1 of 4",
			arrow keys dead). They are filter chips, and aria-pressed says exactly that.
			Deliberately not disabled while a fetch is in flight: see `loadSeq`.
		-->
		<div class="k-field">
			<span class="k-label" aria-hidden="true">Status</span>
			<Segmented
				label="Filter by status"
				value={status ?? 'all'}
				options={statusOptions}
				onchange={(v) => setStatus(v === 'all' ? null : v)}
			/>
		</div>
		<label class="k-field kind-filter">
			<span class="k-label">Kind</span>
			<select
				class="k-select"
				value={kind ?? ''}
				onchange={(e) => setKind((e.currentTarget.value || null) as ReportKind | null)}
			>
				{#each KIND_FILTERS as f (f.label)}
					<option value={f.value ?? ''}>{f.label}</option>
				{/each}
			</select>
		</label>
	</div>

	<!--
		The empty title only claims "no open reports" when `openCount` (the GLOBAL backlog) is
		zero, not merely when this page is empty: page 3 of an open queue, or the Comment abuse
		slice of a busy one, is empty without the queue being clear.
	-->
	<LoadState
		loading={loading && reports.length === 0}
		error={loadError}
		empty={reports.length === 0}
		loadingLabel="Loading reports…"
		emptyIcon="flag"
		emptyTitle={status === 'OPEN' && openCount === 0
			? 'No open reports'
			: page > 1
				? 'No reports on this page'
				: 'No reports match this filter'}
		emptyHint={status === 'OPEN' && openCount === 0
			? 'Reports that readers file from the reader land here. None are open right now.'
			: page > 1
				? 'Reports that readers file from the reader land here. Go back a page to see the ones that match this filter.'
				: 'Reports that readers file from the reader land here. None match the current status and kind.'}
		onretry={() => refresh(status, kind, page)}
	>
		<div class="k-table" class:is-dim={loading} role="table" aria-label="Reader reports">
			<div class="k-tr k-tr--head" role="row">
				<span role="columnheader">Kind</span>
				<span role="columnheader">Subject</span>
				<span role="columnheader">Reporter</span>
				<span role="columnheader">Filed</span>
				<span role="columnheader" class="k-td--end">Triage</span>
			</div>
			{#each reports as r (r.id)}
				<div class="k-tr" role="row">
					<span class="kinds" role="cell">
						<span class="k-badge" data-tone={KIND_TONE[r.kind]}>{KIND_LABEL[r.kind] ?? r.kind}</span
						>
						{#if r.status !== 'OPEN'}
							<span class="k-badge" data-tone={r.status === 'RESOLVED' ? 'success' : undefined}>
								{STATUS_LABEL[r.status] ?? r.status}
							</span>
						{/if}
					</span>
					<span class="subject" role="cell">
						{#if r.subjectTitle}<span class="stitle k-truncate">{r.subjectTitle}</span>{/if}
						{#if r.reportedUsername}
							<span class="stitle k-truncate">@{r.reportedUsername}</span>
						{/if}
						{#if r.subjectId || r.subjectIdSecondary}
							<span class="ids k-mono">
								{#if r.subjectId}
									<a class="k-link" href={seriesHref(r.subjectId)} target="_blank" rel="noreferrer"
										>{r.subjectId}</a
									>
								{/if}
								{#if r.subjectIdSecondary}
									<span class="k-muted">and</span>
									<a
										class="k-link"
										href={seriesHref(r.subjectIdSecondary)}
										target="_blank"
										rel="noreferrer"
									>
										{r.subjectIdSecondary}
									</a>
								{/if}
							</span>
						{/if}
						{#if !r.subjectId && !r.subjectIdSecondary && !r.subjectTitle && !r.reportedUsername}
							<span class="k-muted">No subject given</span>
						{/if}
					</span>
					<span class="wrap k-muted" role="cell" data-label="Reporter"
						>{r.reporter ?? 'Anonymous'}</span
					>
					<span class="wrap k-muted k-num" role="cell" data-label="Filed"
						>{fmtDate(r.createdAt)}</span
					>
					<span class="k-td--end" role="cell">
						<Button
							size="sm"
							variant="ghost"
							aria-expanded={openRow === r.id}
							onclick={() => (openRow = openRow === r.id ? null : r.id)}
						>
							{openRow === r.id ? 'Hide' : 'Read'}
						</Button>
						{#if r.status === 'OPEN'}
							<Button
								size="sm"
								icon="check"
								disabled={busy === r.id}
								busy={busy === r.id && busyAction === 'RESOLVED'}
								onclick={() => triage(r, 'RESOLVED')}
							>
								Mark resolved
							</Button>
							<Button
								size="sm"
								disabled={busy === r.id}
								busy={busy === r.id && busyAction === 'REJECTED'}
								onclick={() => triage(r, 'REJECTED')}
							>
								Reject
							</Button>
						{:else}
							<Button
								size="sm"
								disabled={busy === r.id}
								busy={busy === r.id && busyAction === 'OPEN'}
								onclick={() => triage(r, 'OPEN')}
							>
								Reopen
							</Button>
						{/if}
					</span>
				</div>
				{#if openRow === r.id}
					<!--
						A `row` inside a `table` may only own cells, so the detail panel cannot BE a
						row: as a sibling `role="row"` holding paragraphs, a link and a text input it
						was a row with no cells, and everything in it, including the note field and
						the delete button, sat outside the table's accessibility tree. One cell
						spanning the five columns is the valid shape for a full-width expansion.
					-->
					<div class="detail-row" role="row">
						<div class="detail" role="cell" aria-colspan="5">
							<p class="body">{r.detail}</p>
							{#if r.commentExcerpt}
								<div class="quote">
									<p class="quote-head">
										<span class="k-label">
											Reported comment{r.reportedUsername ? ` by @${r.reportedUsername}` : ''}
										</span>
										{#if r.commentId}<span class="k-mono k-muted">{r.commentId}</span>{/if}
									</p>
									<p class="quote-text">{r.commentExcerpt}</p>
									<p class="k-hint">
										Copied when the report was filed. The comment itself may already be gone.
									</p>
									{#if r.commentId}
										<div class="quote-act">
											<Button
												size="sm"
												variant="danger"
												icon="trash"
												disabled={busy === r.id || deleted.has(r.commentId)}
												busy={busy === r.id && busyAction === 'DELETE'}
												onclick={() => removeComment(r)}
											>
												{deleted.has(r.commentId) ? 'Deleted' : 'Delete this comment'}
											</Button>
											<span class="k-hint">
												Deletes the comment and every reply under it. This cannot be undone. The
												report keeps the copy above either way.
											</span>
										</div>
									{/if}
								</div>
							{/if}
							{#if r.sourceUrl || r.adminNote || r.resolvedAt}
								<dl class="facts">
									{#if r.sourceUrl}
										{@const safe = safeHref(r.sourceUrl)}
										<div>
											<dt>Link given</dt>
											<dd>
												{#if safe}
													<a class="k-link" href={safe} target="_blank" rel="noreferrer noopener"
														>{r.sourceUrl}</a
													>
												{:else}
													<span
														class="unsafe"
														title="Not an http or https link. Shown as text, not linked."
													>
														{r.sourceUrl}
													</span>
													<span class="k-muted">(not an http or https link, so not clickable)</span>
												{/if}
											</dd>
										</div>
									{/if}
									{#if r.adminNote}
										<div>
											<dt>Stored note</dt>
											<dd>{r.adminNote}</dd>
										</div>
									{/if}
									{#if r.resolvedAt}
										<div>
											<dt>Closed</dt>
											<dd class="k-num">{fmtDate(r.resolvedAt)}</dd>
										</div>
									{/if}
								</dl>
							{/if}
							<div class="note">
								<label class="k-field">
									<span class="k-label">Add a note (readers never see it)</span>
									<input
										class="k-input"
										value={notes[r.id] ?? ''}
										oninput={(e) => (notes[r.id] = e.currentTarget.value)}
										placeholder="What you did about it"
										aria-describedby="note-hint-{r.id}"
									/>
								</label>
								<!--
									Both halves are server behaviour, not UI choices: `resolveReport` is the
									only writer of `admin_note`, so nothing is stored until a triage button
									runs; and it merges with `COALESCE(?, admin_note)`, so an empty box keeps
									whatever is there. There is deliberately no way to blank a note.
								-->
								<p class="k-hint" id="note-hint-{r.id}">
									Saved when you press Mark resolved, Reject or Reopen. Typing here replaces the
									stored note. Leaving it empty keeps the stored note. A note cannot be cleared from
									this console.
								</p>
							</div>
						</div>
					</div>
				{/if}
			{/each}
		</div>
	</LoadState>

	<!--
		Outside LoadState's empty branch on purpose. Triaging the last row of page 1 (or landing
		on a page the queue has shrunk past) would otherwise replace the whole block, pager
		included, with the empty state, leaving no way back to page 1 but a browser reload.
	-->
	{#if !loadError && !(loading && reports.length === 0) && (page > 1 || hasNext)}
		<Pager
			{page}
			{hasNext}
			busy={loading}
			onpage={(n) => queueNavigate(pageState.url, { page: Math.max(1, n) })}
		/>
	{/if}
</div>

<style>
	.k-table {
		--cols: minmax(0, 1.2fr) minmax(0, 2.2fr) minmax(0, 0.9fr) minmax(0, 1.1fr) minmax(0, 1.9fr);
	}
	.filters {
		align-items: flex-end;
		justify-content: space-between;
	}
	.kind-filter {
		flex: 0 1 220px;
	}
	.kinds {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.subject {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 3px;
	}
	.stitle {
		color: var(--k-text-bright);
		font-weight: 600;
	}
	.ids {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 6px;
	}
	/* Ids are capped at 300 chars server-side, not at "looks like an id", and a reporter
	   name has no word breaks either. An over-long one must wrap inside its own grid column
	   instead of pushing the Triage buttons off the row. */
	.ids,
	.wrap {
		overflow-wrap: anywhere;
	}
	.detail-row {
		border-bottom: 1px solid var(--k-border);
		background: var(--k-surface-2);
	}
	.detail-row:last-child {
		border-bottom: 0;
	}
	.detail {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--k-space-4);
		padding: var(--k-space-5) var(--k-space-4);
	}
	.body {
		max-width: 90ch;
		color: var(--k-text-1);
		font-size: var(--k-text-md);
		line-height: 1.65;
		white-space: pre-wrap;
		/* `pre-wrap` only breaks at whitespace, and `detail` is 4000 reader-supplied chars
		   with no guarantee of any: one unbroken run overflowed the panel and was then
		   silently CLIPPED by the table's `overflow: hidden`. Unreadable, which for the
		   only field that says what the report is about is the whole page failing. */
		overflow-wrap: anywhere;
	}
	.quote {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--k-space-2);
		max-width: 90ch;
		padding: var(--k-space-3) var(--k-space-4);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		background: var(--k-surface-3);
	}
	.quote-head {
		display: flex;
		align-items: baseline;
		flex-wrap: wrap;
		gap: var(--k-space-2);
		overflow-wrap: anywhere;
	}
	.quote-text {
		color: var(--k-text-2);
		font-size: var(--k-text-sm);
		line-height: 1.6;
		white-space: pre-wrap;
		overflow-wrap: anywhere; /* same reason as `.body`: a 500-char snapshot of a comment */
	}
	.quote-act {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-3);
		margin-top: var(--k-space-1);
	}
	.facts {
		display: grid;
		gap: var(--k-space-2);
		font-size: var(--k-text-sm);
	}
	.facts > div {
		display: grid;
		grid-template-columns: 96px minmax(0, 1fr);
		gap: var(--k-space-3);
	}
	.facts dt {
		color: var(--k-text-dim);
		font-weight: 600;
	}
	.facts dd {
		color: var(--k-text-2);
		overflow-wrap: anywhere;
	}
	.unsafe {
		text-decoration: underline dotted;
	}
	.note {
		display: grid;
		gap: 6px;
		max-width: 640px;
	}
	@media (max-width: 720px) {
		.kind-filter {
			flex-basis: 100%;
		}
		.facts > div {
			grid-template-columns: minmax(0, 1fr);
			gap: 2px;
		}
	}
</style>
