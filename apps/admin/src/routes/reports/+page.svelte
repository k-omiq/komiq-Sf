<script lang="ts">
	import type { Report, ReportKind, ReportStatus } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { deleteComment, loadReports, resolveReport } from '$lib/data';

	let reports = $state<Report[]>([]);
	let page = $state(1);
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let openCount = $state(0);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let busy = $state<string | null>(null); // report id currently acting

	let status = $state<ReportStatus | null>('OPEN');
	let kind = $state<ReportKind | null>(null);
	/** Per-row triage note, only sent for the row it was typed on. */
	let notes = $state<Record<string, string>>({});
	/** Which row's detail/note panel is expanded. */
	let openRow = $state<string | null>(null);

	const STATUS_TABS: { label: string; value: ReportStatus | null }[] = [
		{ label: 'Open', value: 'OPEN' },
		{ label: 'Resolved', value: 'RESOLVED' },
		{ label: 'Rejected', value: 'REJECTED' },
		{ label: 'All', value: null },
	];

	const KIND_LABEL: Record<ReportKind, string> = {
		WRONG_MERGE: 'Wrongly merged',
		NEEDS_MERGE: 'Needs merging',
		MISSING_WORK: 'Missing series',
		COMMENT_ABUSE: 'Comment abuse',
		OTHER: 'Other',
	};

	const KIND_FILTERS: { label: string; value: ReportKind | null }[] = [
		{ label: 'Every kind', value: null },
		...(Object.keys(KIND_LABEL) as ReportKind[]).map((k) => ({ label: KIND_LABEL[k], value: k })),
	];

	// Auth redirect is centralized in +layout.svelte. This effect is the SINGLE fetch
	// driver (the `users` page convention): the filters and the pager only assign state,
	// and the effect re-fires exactly once — `refresh` never writes `page`.
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
		status = v;
		page = 1;
	}

	function setKind(v: ReportKind | null): void {
		if (v === kind) return;
		kind = v;
		page = 1;
	}

	function go(delta: number): void {
		const next = Math.max(1, page + delta);
		if (next === page) return;
		page = next;
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
			else page = target;
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
		if (!confirm('Delete this comment and all replies to it? This cannot be undone.')) return;
		busy = r.id;
		actionError = null;
		try {
			await deleteComment(r.commentId);
			// Reassign, don't mutate. `$state` only deep-proxies plain objects and arrays
			// (svelte/internal/client/proxy.js bails on any other prototype), so this Set is
			// the RAW Set — `.add()` notifies nothing and the template's `deleted.has(...)`
			// would never re-run. Reassigning the variable is what publishes the change.
			// (`SvelteSet` from `svelte/reactivity` would also work; nothing else in the repo
			// uses it, and this set holds a handful of ids for one session.)
			deleted = new Set(deleted).add(r.commentId);
		} catch (err) {
			actionError = err instanceof Error ? err.message : 'Could not delete that comment.';
		} finally {
			busy = null;
		}
	}

	async function triage(r: Report, next: ReportStatus): Promise<void> {
		// One triage at a time (the `users`/`bugs` convention): the counter bookkeeping
		// below reads `r.status` captured before the await, so overlapping mutations on
		// two rows would each reconcile against a list the other has already changed.
		if (busy) return;
		busy = r.id;
		actionError = null;
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
			await settleAfterTriage();
		} catch (err) {
			actionError = err instanceof Error ? err.message : 'Could not update that report.';
		} finally {
			busy = null;
		}
	}

	function fmtDate(iso: string): string {
		const t = Date.parse(iso);
		return Number.isNaN(t) ? '—' : new Date(t).toLocaleString();
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
		return `/series/${encodeURIComponent(id)}`;
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

<div class="page">
	<div class="page-head">
		<div>
			<h1>Reader reports</h1>
			<p class="lede">
				Filed from the reader's Support page. {openCount} open{total != null
					? ` · ${total} in this view`
					: ''}. Merge two entries from <a href="/">Catalog</a> — NOT
				<a href="/review">Review</a>, which only triages what the matcher itself flagged, and a
				missed duplicate is by definition not in there. Bans live in <a href="/users">Users</a>.
			</p>
		</div>
		<button class="pg" disabled={loading} onclick={() => refresh(status, kind, page)}>
			Refresh
		</button>
	</div>

	<div class="filters">
		<!--
			Toggle buttons in a labelled group, NOT role="tablist"/role="tab". A `tab` must own
			a `tabpanel` via aria-controls and implement arrow-key roving focus; these do
			neither, so the tab roles only promised a widget that isn't there ("tab 1 of 4",
			arrow keys dead). They are filter chips, and aria-pressed says exactly that.
		-->
		<div class="tabs" role="group" aria-label="Filter by status">
			{#each STATUS_TABS as t (t.label)}
				<button
					aria-pressed={status === t.value}
					class="tab"
					class:on={status === t.value}
					onclick={() => setStatus(t.value)}
				>
					{t.label}
					{#if t.value === 'OPEN' && openCount > 0}<span class="count">{openCount}</span>{/if}
				</button>
			{/each}
		</div>
		<label class="kind-filter">
			<span>Kind</span>
			<select
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
		Always visible: an action can fail on the LAST row of a page, and when the list it
		used to sit inside is gone with it the only explanation would go with it too.
	-->
	{#if actionError}<div class="notice error">{actionError}</div>{/if}

	{#if loadError}
		<div class="notice error">{loadError}</div>
	{:else if loading && reports.length === 0}
		<div class="notice">Loading…</div>
	{:else}
		{#if reports.length === 0}
			<!--
				"The queue is clear" is a claim about the BACKLOG, so it is gated on `openCount`
				(global) rather than on this page being empty: page 3 of an open queue, or the
				COMMENT_ABUSE slice of a busy one, is empty without the queue being clear.
			-->
			<div class="notice">
				{status === 'OPEN' && openCount === 0
					? 'Nothing open — the queue is clear. 🎉'
					: 'No reports in this view.'}
			</div>
		{:else}
			<div class="table" role="table" aria-label="Reader reports">
				<div class="row head" role="row">
					<span role="columnheader">Kind</span>
					<span role="columnheader">Subject</span>
					<span role="columnheader">Reporter</span>
					<span role="columnheader">Filed</span>
					<span role="columnheader" class="right">Triage</span>
				</div>
				{#each reports as r (r.id)}
					<div class="row" role="row">
						<span role="cell">
							<span class="badge k-{r.kind.toLowerCase()}">{KIND_LABEL[r.kind] ?? r.kind}</span>
							{#if r.status !== 'OPEN'}
								<span class="badge sm {r.status.toLowerCase()}">{r.status.toLowerCase()}</span>
							{/if}
						</span>
						<span class="subject" role="cell">
							{#if r.subjectTitle}<span class="stitle">{r.subjectTitle}</span>{/if}
							{#if r.reportedUsername}
								<span class="stitle">@{r.reportedUsername}</span>
							{/if}
							<span class="ids">
								{#if r.subjectId}
									<a href={seriesHref(r.subjectId)} target="_blank" rel="noreferrer"
										>{r.subjectId}</a
									>
								{/if}
								{#if r.subjectIdSecondary}
									<span class="vs">↔</span>
									<a href={seriesHref(r.subjectIdSecondary)} target="_blank" rel="noreferrer">
										{r.subjectIdSecondary}
									</a>
								{/if}
								{#if !r.subjectId && !r.subjectIdSecondary && !r.subjectTitle && !r.reportedUsername}
									<span class="muted">—</span>
								{/if}
							</span>
						</span>
						<span class="muted" role="cell">{r.reporter ?? 'anonymous'}</span>
						<span class="muted" role="cell">{fmtDate(r.createdAt)}</span>
						<span class="actions" role="cell">
							<button class="act" onclick={() => (openRow = openRow === r.id ? null : r.id)}>
								{openRow === r.id ? 'Hide' : 'Read'}
							</button>
							{#if r.status === 'OPEN'}
								<button
									class="act ok"
									disabled={busy === r.id}
									onclick={() => triage(r, 'RESOLVED')}
								>
									{busy === r.id ? '…' : 'Fixed'}
								</button>
								<button class="act" disabled={busy === r.id} onclick={() => triage(r, 'REJECTED')}>
									Not a bug
								</button>
							{:else}
								<button class="act" disabled={busy === r.id} onclick={() => triage(r, 'OPEN')}>
									Reopen
								</button>
							{/if}
						</span>
					</div>
					{#if openRow === r.id}
						<!--
						A `row` inside a `table` may only own cells, so the detail panel cannot BE a
						row: as a sibling `role="row"` holding paragraphs, a link and a text input it
						was a row with no cells, and everything in it — including the note field and
						the delete button — sat outside the table's accessibility tree. One cell
						spanning the five columns is the valid shape for a full-width expansion.
					-->
						<div class="detail-row" role="row">
							<div class="detail" role="cell" aria-colspan="5">
								<p class="body">{r.detail}</p>
								{#if r.commentExcerpt}
									<div class="quote">
										<span class="quote-head">
											Reported comment{r.reportedUsername ? ` by @${r.reportedUsername}` : ''}
											{#if r.commentId}<code>{r.commentId}</code>{/if}
										</span>
										<p>{r.commentExcerpt}</p>
										<span class="quote-note">
											Snapshotted when filed — the comment itself may already be gone.
										</span>
										{#if r.commentId}
											<div class="quote-act">
												<button
													class="act danger"
													disabled={busy === r.id || deleted.has(r.commentId)}
													onclick={() => removeComment(r)}
												>
													{deleted.has(r.commentId) ? 'Deleted' : 'Delete this comment'}
												</button>
												<span class="quote-note">
													Deletes the comment and every reply under it. Cannot be undone; this
													report keeps the copy above either way.
												</span>
											</div>
										{/if}
									</div>
								{/if}
								{#if r.sourceUrl}
									{@const safe = safeHref(r.sourceUrl)}
									<p class="meta">
										Link given:
										{#if safe}
											<a href={safe} target="_blank" rel="noreferrer noopener">{r.sourceUrl}</a>
										{:else}
											<span class="unsafe" title="Not an http(s) link — shown as text, not linked">
												{r.sourceUrl}
											</span>
											<span class="quote-note">(not a http/https link — not clickable)</span>
										{/if}
									</p>
								{/if}
								{#if r.adminNote}
									<p class="meta">Note: {r.adminNote}</p>
								{/if}
								{#if r.resolvedAt}
									<p class="meta">Closed {fmtDate(r.resolvedAt)}</p>
								{/if}
								<label class="note">
									<span>Add a note (readers never see it)</span>
									<input
										value={notes[r.id] ?? ''}
										oninput={(e) => (notes[r.id] = e.currentTarget.value)}
										placeholder="What you did about it"
									/>
									<!--
									Both halves are server behaviour, not UI choices: `resolveReport` is the
									only writer of `admin_note`, so nothing is stored until a triage button
									runs; and it merges with `COALESCE(?, admin_note)`, so an empty box keeps
									whatever is there. There is deliberately no way to blank a note.
								-->
									<small class="note-hint">
										Saved when you press Fixed / Not a bug / Reopen. Typing here replaces the stored
										note; leaving it empty keeps it — a note can't be cleared from the console.
									</small>
								</label>
							</div>
						</div>
					{/if}
				{/each}
			</div>
		{/if}

		<!--
			Outside the empty check on purpose. Triaging the last row of page 1 (or landing on
			a page the queue has shrunk past) used to replace the whole block, pager included,
			with "no reports in this view" — leaving no way back to page 1 but a browser reload.
		-->
		{#if page > 1 || hasNext}
			<div class="pager">
				<button class="pg" disabled={page <= 1 || loading} onclick={() => go(-1)}>← Prev</button>
				<span class="muted">Page {page}</span>
				<button class="pg" disabled={!hasNext || loading} onclick={() => go(1)}>Next →</button>
			</div>
		{/if}
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
		max-width: 72ch;
	}
	.lede a {
		color: var(--k-text-2);
	}
	.filters {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-4);
	}
	.tabs {
		display: flex;
		gap: 6px;
	}
	.tab {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		padding: 8px 14px;
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		color: var(--k-text-dim);
		font-family: inherit;
		font-size: 13px;
		font-weight: 700;
		cursor: pointer;
	}
	.tab:hover {
		color: var(--k-text);
	}
	.tab.on {
		background: var(--k-surface-5);
		border-color: var(--k-border-4);
		color: var(--k-text-bright);
	}
	.count {
		padding: 1px 7px;
		border-radius: var(--k-radius-pill);
		background: rgba(224, 131, 105, 0.18);
		color: var(--k-accent);
		font-size: 11px;
	}
	.kind-filter {
		display: inline-flex;
		align-items: center;
		gap: 9px;
		font-size: 12px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.kind-filter select {
		padding: 8px 12px;
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-md);
		background: var(--k-surface-2);
		color: var(--k-text);
		font-family: inherit;
		font-size: 13px;
		font-weight: 600;
		letter-spacing: 0;
		text-transform: none;
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
		grid-template-columns: 1.5fr 2.2fr 1fr 1.2fr auto;
		align-items: center;
		gap: var(--k-space-4);
		padding: 12px 16px;
		border-bottom: 1px solid var(--k-border);
	}
	.row.head {
		background: var(--k-surface-2);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.right {
		text-align: right;
	}
	.badge {
		display: inline-block;
		font-size: 11px;
		font-weight: 700;
		padding: 3px 8px;
		border-radius: var(--k-radius-sm);
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
	.badge.sm {
		margin-left: 6px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}
	.badge.k-wrong_merge,
	.badge.k-needs_merge {
		background: rgba(198, 156, 240, 0.15);
		color: var(--k-accent-purple);
	}
	.badge.k-missing_work {
		background: rgba(143, 184, 255, 0.15);
		color: var(--k-completed);
	}
	.badge.k-comment_abuse {
		background: rgba(240, 128, 138, 0.15);
		color: #f0808a;
	}
	.badge.resolved {
		background: rgba(127, 211, 154, 0.15);
		color: var(--k-ongoing);
	}
	.badge.rejected {
		background: var(--k-surface-5);
		color: var(--k-text-faint);
	}
	.subject {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.stitle {
		font-size: 13.5px;
		font-weight: 600;
		color: var(--k-text-1);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ids {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 7px;
		min-width: 0;
		font-family: var(--k-font-mono);
		font-size: 11.5px;
		color: var(--k-text-faint);
		/* Ids are capped at 300 chars server-side, not at "looks like an id" — an over-long
		   one must wrap inside its own grid column instead of pushing the Triage buttons
		   off the row. */
		overflow-wrap: anywhere;
	}
	.ids a {
		color: var(--k-text-dim);
	}
	.vs {
		color: var(--k-text-ghost);
	}
	.muted {
		font-size: 13px;
		color: var(--k-text-faint);
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 6px;
	}
	.act,
	.pg {
		padding: 7px 12px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-md);
		background: var(--k-surface-2);
		color: var(--k-text-1);
		font-family: inherit;
		font-size: 12.5px;
		font-weight: 700;
		cursor: pointer;
	}
	.act:hover:not(:disabled),
	.pg:hover:not(:disabled) {
		border-color: var(--k-border-strong);
		color: var(--k-text-bright);
	}
	.act:disabled,
	.pg:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.act.ok {
		color: var(--k-ongoing);
		border-color: rgba(127, 211, 154, 0.35);
	}
	.act.danger {
		color: #f0808a;
		border-color: rgba(240, 128, 138, 0.35);
	}
	.detail-row {
		border-bottom: 1px solid var(--k-border);
		background: var(--k-surface-2);
	}
	.detail {
		display: flex;
		flex-direction: column;
		gap: 14px;
		padding: 18px 16px 22px;
		min-width: 0;
	}
	.body {
		margin: 0;
		font-size: 14px;
		line-height: 1.65;
		color: var(--k-text-1);
		white-space: pre-wrap;
		max-width: 90ch;
		/* `pre-wrap` only breaks at whitespace, and `detail` is 4000 reader-supplied chars
		   with no guarantee of any: one unbroken run overflowed the panel and was then
		   silently CLIPPED by `.table { overflow: hidden }` — unreadable, which for the
		   only field that says what the report is about is the whole page failing. */
		overflow-wrap: anywhere;
	}
	.quote {
		display: flex;
		flex-direction: column;
		gap: 7px;
		padding: 14px 16px;
		border-left: 3px solid var(--k-border-4);
		border-radius: var(--k-radius-sm);
		background: var(--k-surface-3);
	}
	.quote-head {
		font-size: 11.5px;
		font-weight: 700;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.quote-head code {
		font-family: var(--k-font-mono);
		text-transform: none;
		letter-spacing: 0;
		margin-left: 8px;
		color: var(--k-text-ghost);
	}
	.quote p {
		margin: 0;
		font-size: 13.5px;
		line-height: 1.6;
		color: var(--k-text-2);
		white-space: pre-wrap;
		overflow-wrap: anywhere; /* same reason as `.body` — a 500-char snapshot of a comment */
	}
	.quote-note {
		font-size: 11.5px;
		color: var(--k-text-ghost);
	}
	.quote-act {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 10px;
		margin-top: 4px;
	}
	.meta {
		margin: 0;
		font-size: 12.5px;
		color: var(--k-text-faint);
		word-break: break-all;
	}
	.meta a {
		color: var(--k-text-dim);
	}
	.unsafe {
		color: var(--k-text-2);
		text-decoration: underline dotted;
	}
	.note {
		display: flex;
		flex-direction: column;
		gap: 7px;
		max-width: 640px;
	}
	.note span {
		font-size: 11.5px;
		font-weight: 700;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.note input {
		padding: 10px 13px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-md);
		background: var(--k-surface);
		color: var(--k-text);
		font-family: inherit;
		font-size: 13.5px;
		outline: none;
	}
	.note input:focus {
		border-color: var(--k-border-strong);
	}
	.note-hint {
		font-size: 11.5px;
		line-height: 1.5;
		color: var(--k-text-ghost);
	}
	.pager {
		display: flex;
		align-items: center;
		gap: var(--k-space-4);
	}
	@media (max-width: 900px) {
		.row {
			grid-template-columns: 1fr;
			gap: 8px;
		}
		.row.head {
			display: none;
		}
		.actions {
			justify-content: flex-start;
			flex-wrap: wrap;
		}
	}
</style>
