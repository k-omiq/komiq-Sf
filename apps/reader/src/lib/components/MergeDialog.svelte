<script lang="ts">
	// Admin-only duplicate merger for the series page.
	//
	// DIRECTION IS FIXED AND ONE-WAY: the series whose page you opened is the TARGET and
	// survives; everything picked here is a SOURCE, gets its sources/library rows/progress/
	// reviews/comments re-pointed onto the target, and is then DELETED. That is what makes
	// multi-select coherent — many works can fold into one, not the reverse — and it is why
	// the confirm step names the survivor and counts the casualties instead of saying
	// "merge 3 series".
	//
	// There is NO UNDO. `mergeWorks` drops the source row; `work_redirect` keeps old links
	// resolving, but the work itself is gone.
	import Icon from '$lib/components/Icon.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import { backend } from '$lib/context';
	import type { MergeCandidateRow } from '@komika/types';

	let {
		open = $bindable(false),
		targetWorkId,
		targetTitle,
		onmerged,
	}: {
		open?: boolean;
		/** The SURVIVING work — the page you're on. Null while the detail query is still
		 *  resolving `workId`, which is what disables the trigger upstream. */
		targetWorkId: string | null;
		targetTitle: string;
		/** Fired after at least one merge succeeded, so the page can refetch itself: the
		 *  target has just gained sources/aliases and its chapter list has changed. */
		onmerged?: () => void;
	} = $props();

	type Picked = { workId: string; title: string };

	let query = $state('');
	let rows = $state<MergeCandidateRow[]>([]);
	let searching = $state(false);
	let searched = $state(false);
	let error = $state('');
	let picked = $state<Picked[]>([]);
	let confirming = $state(false);
	let merging = $state(false);
	let progress = $state('');
	let input = $state<HTMLInputElement | undefined>(undefined);

	const canSearch = $derived(query.trim().length >= 3);
	const pickedIds = $derived(new Set(picked.map((p) => p.workId)));

	function reset(): void {
		query = '';
		rows = [];
		searched = false;
		error = '';
		picked = [];
		confirming = false;
		progress = '';
	}

	function close(): void {
		if (merging) return; // never strand a half-finished batch behind a closed dialog
		open = false;
		reset();
	}

	// Autofocus the field and wire Escape, mirroring SearchOverlay. Escape backs OUT of the
	// confirm step first rather than closing the whole dialog — losing a multi-select to a
	// stray keypress is the kind of thing that makes an admin re-do the search from scratch.
	$effect(() => {
		if (!open) return;
		input?.focus();
		const onkey = (e: KeyboardEvent) => {
			if (e.key !== 'Escape') return;
			if (confirming) confirming = false;
			else close();
		};
		window.addEventListener('keydown', onkey);
		return () => window.removeEventListener('keydown', onkey);
	});

	async function search(): Promise<void> {
		const q = query.trim();
		// Same 3-character floor the server enforces (MIN_FTS_QUERY_CHARS): a shorter query
		// returns nothing, so surface that as a disabled button rather than an empty result.
		if (q.length < 3 || searching) return;
		searching = true;
		error = '';
		try {
			const page = await backend.searchForMerge!(q, 1);
			// Never offer the target as its own source — merging a work into itself either
			// no-ops or destroys it, depending on how the resolver unwinds.
			rows = page.items.filter((r) => r.workId && r.workId !== targetWorkId);
			searched = true;
		} catch (e) {
			error = e instanceof Error ? e.message : 'Search failed';
			rows = [];
		} finally {
			searching = false;
		}
	}

	function toggle(row: MergeCandidateRow): void {
		if (!row.workId) return;
		const id = row.workId;
		picked = pickedIds.has(id)
			? picked.filter((p) => p.workId !== id)
			: [...picked, { workId: id, title: row.title }];
	}

	// Merges run ONE AT A TIME, not Promise.all. Each is a multi-table write on the same
	// target row, and the server's pool is small enough that a parallel batch would contend
	// on the write lock; more importantly a partial failure has to leave a legible trail, so
	// `progress` names the work being folded and the loop stops at the first error with the
	// successes already committed and reported.
	async function run(): Promise<void> {
		if (!targetWorkId || merging || !picked.length) return;
		merging = true;
		error = '';
		let done = 0;
		try {
			for (const p of picked) {
				progress = `Merging "${p.title}"…`;
				await backend.mergeWorks!(p.workId, targetWorkId);
				done++;
			}
			open = false;
			reset();
			onmerged?.();
		} catch (e) {
			const why = e instanceof Error ? e.message : 'Merge failed';
			error = done
				? `${why} — ${done} of ${picked.length} merged. The rest were not touched.`
				: why;
			// Drop what already merged so a retry can't double-apply onto a deleted row.
			picked = picked.slice(done);
			confirming = false;
		} finally {
			merging = false;
			progress = '';
		}
	}
</script>

{#if open}
	<div class="scrim-wrap" role="dialog" aria-modal="true" aria-label="Merge series">
		<button class="scrim" aria-label="Close" onclick={close} disabled={merging}></button>
		<div class="panel">
			<header>
				<div class="head-text">
					<h2>Merge into this series</h2>
					<p class="sub">
						Everything you pick is folded into <strong>{targetTitle}</strong> and then deleted.
					</p>
				</div>
				<button class="icon-btn" onclick={close} disabled={merging} aria-label="Close">
					<Icon name="x" size={18} />
				</button>
			</header>

			{#if !confirming}
				<form
					class="search"
					onsubmit={(e) => {
						e.preventDefault();
						search();
					}}
				>
					<Icon name="search" size={16} />
					<input
						bind:this={input}
						bind:value={query}
						type="search"
						placeholder="Search for duplicates…"
						aria-label="Search series to merge"
					/>
					<button type="submit" class="go" disabled={!canSearch || searching}>
						{searching ? 'Searching…' : 'Search'}
					</button>
				</form>

				<div class="results">
					{#if error}
						<div class="msg error" role="alert"><Icon name="alert" size={15} />{error}</div>
					{:else if searching}
						<div class="msg">Searching…</div>
					{:else if searched && !rows.length}
						<div class="msg">No other series matched “{query.trim()}”.</div>
					{:else if !searched}
						<div class="msg">Search the catalogue to find duplicates of this series.</div>
					{/if}

					{#each rows as row (row.workId)}
						{@const on = pickedIds.has(row.workId ?? '')}
						<div class="row" class:on>
							<!-- The whole row toggles selection; the title is a SEPARATE link so an
							     admin can open the candidate's page to check it before committing to
							     a delete. Nested interactives would swallow one or the other, so the
							     link sits outside the toggle and stops propagation. -->
							<button
								class="pick"
								role="checkbox"
								aria-checked={on}
								aria-label={on ? `Deselect ${row.title}` : `Select ${row.title}`}
								onclick={() => toggle(row)}
							>
								<span class="box"
									>{#if on}<Icon name="check" size={13} strokeWidth={3} />{/if}</span
								>
								<span class="thumb"><Cover src={row.coverUrl ?? ''} alt="" loading="lazy" /></span>
							</button>
							<div class="meta">
								<a
									class="title"
									href="/series/{row.id}"
									target="_blank"
									rel="noopener"
									onclick={(e) => e.stopPropagation()}>{row.title}</a
								>
								<div class="sub-line">
									{#if row.author}<span>{row.author}</span>{/if}
									<span>{row.chapterCount} ch</span>
									{#if row.isNsfw}<span class="nsfw">NSFW</span>{/if}
								</div>
							</div>
						</div>
					{/each}
				</div>
			{:else}
				<!-- Confirm step. It lists every casualty BY NAME: a count alone ("merge 3
				     series") is not enough to catch a mis-click on an irreversible delete. -->
				<div class="confirm">
					<div class="warn" role="alert">
						<Icon name="alert" size={16} />
						<div>
							<strong>This cannot be undone.</strong>
							{picked.length}
							{picked.length === 1 ? 'series' : 'series'} will be permanently deleted. Their sources,
							chapters, library entries, progress, reviews and comments move to
							<strong>{targetTitle}</strong>.
						</div>
					</div>
					<ul class="casualties">
						{#each picked as p (p.workId)}
							<li><Icon name="arrow-right" size={14} />{p.title}</li>
						{/each}
					</ul>
					{#if error}
						<div class="msg error" role="alert"><Icon name="alert" size={15} />{error}</div>
					{/if}
					{#if progress}<div class="msg">{progress}</div>{/if}
				</div>
			{/if}

			<footer>
				<span class="count">
					{picked.length}
					selected
				</span>
				{#if confirming}
					<button class="ghost" onclick={() => (confirming = false)} disabled={merging}>Back</button
					>
					<button class="danger" onclick={run} disabled={merging}>
						{merging ? 'Merging…' : `Merge & delete ${picked.length}`}
					</button>
				{:else}
					<button class="ghost" onclick={close}>Cancel</button>
					<button class="primary" onclick={() => (confirming = true)} disabled={!picked.length}>
						Review merge
					</button>
				{/if}
			</footer>
		</div>
	</div>
{/if}

<style>
	.scrim-wrap {
		position: fixed;
		inset: 0;
		z-index: 220;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 24px;
	}
	.scrim {
		position: fixed;
		inset: 0;
		border: none;
		background: rgba(6, 6, 7, 0.82);
		backdrop-filter: blur(6px);
	}
	.panel {
		position: relative;
		display: flex;
		flex-direction: column;
		width: min(680px, 100%);
		max-height: min(760px, 100%);
		background: var(--k-surface);
		border: 1px solid var(--k-border-2);
		border-radius: 16px;
		overflow: hidden;
	}
	header {
		display: flex;
		align-items: flex-start;
		gap: 12px;
		padding: 18px 18px 14px;
		border-bottom: 1px solid var(--k-border-1);
	}
	.head-text {
		flex: 1;
		min-width: 0;
	}
	h2 {
		margin: 0;
		font-family: var(--k-font-display);
		font-size: 1.05rem;
		color: var(--k-text-bright);
	}
	.sub {
		margin: 4px 0 0;
		font-size: 0.82rem;
		color: var(--k-text-dim);
	}
	.sub strong {
		color: var(--k-text-1);
	}
	.icon-btn {
		display: grid;
		place-items: center;
		width: 30px;
		height: 30px;
		border: 1px solid var(--k-border-2);
		border-radius: 8px;
		background: transparent;
		color: var(--k-text-2);
		cursor: pointer;
	}
	.search {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 14px 18px;
		border-bottom: 1px solid var(--k-border-1);
		color: var(--k-text-dim);
	}
	.search input {
		flex: 1;
		min-width: 0;
		padding: 8px 10px;
		border: 1px solid var(--k-border-2);
		border-radius: 9px;
		background: var(--k-surface-2);
		color: var(--k-text);
		font: inherit;
	}
	.go,
	.ghost,
	.primary,
	.danger {
		padding: 8px 14px;
		border-radius: 9px;
		border: 1px solid var(--k-border-2);
		background: var(--k-surface-2);
		color: var(--k-text-1);
		font: inherit;
		font-weight: 600;
		cursor: pointer;
	}
	.primary,
	.go {
		background: var(--k-primary);
		border-color: transparent;
		color: var(--k-on-primary);
	}
	.danger {
		background: #b3261e;
		border-color: transparent;
		color: #fff;
	}
	.go:disabled,
	.primary:disabled,
	.danger:disabled,
	.ghost:disabled,
	.icon-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.results,
	.confirm {
		flex: 1;
		overflow-y: auto;
		padding: 10px 12px;
	}
	.msg {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 14px 6px;
		font-size: 0.85rem;
		color: var(--k-text-dim);
	}
	.msg.error {
		color: #ff8a80;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 6px;
		border-radius: 10px;
	}
	.row.on {
		background: var(--k-hover-fill);
	}
	.pick {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 0;
		border: none;
		background: transparent;
		cursor: pointer;
	}
	.box {
		display: grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border: 1.5px solid var(--k-border-strong);
		border-radius: 5px;
		color: var(--k-on-primary);
	}
	.row.on .box {
		background: var(--k-primary);
		border-color: transparent;
	}
	/* `position: relative` is LOAD-BEARING, not decoration. `Cover` renders its <img> as
	   `position: absolute; inset: 0`, so it sizes against the nearest POSITIONED ancestor.
	   Without this line that was `.panel` (the only positioned box in the chain — `.pick`,
	   `.row` and `.results` are all static, and `overflow` alone does not establish a
	   containing block), so every candidate's cover rendered at the dialog's full 680×760
	   instead of 38×54, stacked over the search field and the whole result list.

	   It was not merely ugly: absolutely-positioned boxes paint above static in-flow
	   siblings, so `elementFromPoint` at the dialog's centre returned the cover — which is
	   a child of that row's `.pick` button — and every click anywhere in the dialog
	   toggled the last candidate instead of hitting the control under the pointer. The
	   38×54 thumbs were still laid out correctly; they were just empty grey boxes with
	   their image escaped. */
	.thumb {
		position: relative;
		display: block;
		width: 38px;
		height: 54px;
		border-radius: 6px;
		overflow: hidden;
		background: var(--k-surface-3);
	}
	.meta {
		flex: 1;
		min-width: 0;
	}
	.title {
		display: block;
		font-size: 0.9rem;
		font-weight: 600;
		color: var(--k-text-1);
		text-decoration: none;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.title:hover {
		text-decoration: underline;
	}
	.sub-line {
		display: flex;
		gap: 10px;
		margin-top: 3px;
		font-size: 0.76rem;
		color: var(--k-text-dim);
	}
	.nsfw {
		color: var(--k-hiatus);
		font-weight: 600;
	}
	.warn {
		display: flex;
		gap: 10px;
		padding: 12px;
		border: 1px solid #b3261e;
		border-radius: 10px;
		background: rgba(179, 38, 30, 0.12);
		font-size: 0.85rem;
		color: var(--k-text-1);
		line-height: 1.45;
	}
	.casualties {
		margin: 12px 0 0;
		padding: 0;
		list-style: none;
	}
	.casualties li {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 7px 4px;
		font-size: 0.86rem;
		color: var(--k-text-2);
		border-bottom: 1px solid var(--k-border-1);
	}
	footer {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 14px 18px;
		border-top: 1px solid var(--k-border-1);
	}
	.count {
		flex: 1;
		font-size: 0.82rem;
		color: var(--k-text-dim);
	}
</style>
