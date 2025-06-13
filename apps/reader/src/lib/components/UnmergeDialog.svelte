<script lang="ts">
	// Admin-only source SPLITTER for the series page — the counterpart to MergeDialog.
	//
	// IT IS NOT AN "UNDO MERGE", AND IT CANNOT BE. `mergeWorks` deletes the losing work
	// row and keeps no record of what it folded: `work_redirect` stores only
	// `(old_id, new_id, created_at)`, and the merge DELETEs colliding reviews/library
	// rows and SUMS view counters into the survivor. The pre-merge state is not on disk
	// any more, so nothing can restore it.
	//
	// What this does instead is the thing that actually fixes the reported problem — two
	// genuinely different series folded together because they share a title: DETACH the
	// sources that don't belong onto a NEW work. That is cheap and lossless in the one
	// direction that matters, because `chapter` is keyed by `source_series_id` and not by
	// work: moving the mapping carries its entire chapter run with it and writes no
	// chapter rows at all.
	//
	// What does NOT come along: reviews, library entries, reading progress and view
	// counts, which are keyed to the work and carry no per-source attribution. They stay
	// on the original. The confirm step says so in as many words rather than letting an
	// admin discover it afterwards.
	import Icon from '$lib/components/Icon.svelte';
	import { backend } from '$lib/context';
	import type { WorkSourceRow } from '@komika/types';

	let {
		open = $bindable(false),
		workId,
		workTitle,
		onsplit,
	}: {
		open?: boolean;
		/** The work being split. Null while the detail query is still resolving it,
		 *  which is what disables the trigger upstream. */
		workId: string | null;
		workTitle: string;
		/** Fired after a successful split so the page can refetch: this work just lost
		 *  sources and, with them, part of its chapter list. */
		onsplit?: (result: { newWorkId: string; newReaderId: string; title: string }) => void;
	} = $props();

	let rows = $state<WorkSourceRow[]>([]);
	let loading = $state(false);
	let loaded = $state(false);
	let error = $state('');
	let pickedIds = $state<Set<string>>(new Set());
	let confirming = $state(false);
	let splitting = $state(false);

	const picked = $derived(rows.filter((r) => pickedIds.has(r.id)));
	// The new work is titled from the source's own title — the first one picked, in the
	// order the server listed them, so the name is stable across re-picks rather than
	// depending on which checkbox was clicked first.
	const newTitle = $derived(picked[0]?.title ?? '');
	// A work with no `source_series` row is excluded from `browse_catalogue` AND from
	// `work_fts`, i.e. it disappears from Browse and from search while still existing in
	// the database. Detaching every source would do exactly that to the work you are
	// standing on, so the last one is not selectable. The server enforces this too; this
	// is here so the admin sees WHY rather than getting a rejection at the end.
	const wouldEmpty = $derived(rows.length > 0 && picked.length >= rows.length);
	const canSplit = $derived(picked.length > 0 && !wouldEmpty);

	function reset(): void {
		rows = [];
		loaded = false;
		error = '';
		pickedIds = new Set();
		confirming = false;
	}

	function close(): void {
		if (splitting) return; // never strand a half-finished split behind a closed dialog
		open = false;
		reset();
	}

	// Escape backs OUT of the confirm step first, then closes — same contract as
	// MergeDialog, for the same reason: losing a multi-select to a stray keypress means
	// re-picking from scratch.
	$effect(() => {
		if (!open) return;
		const onkey = (e: KeyboardEvent) => {
			if (e.key !== 'Escape') return;
			if (confirming) confirming = false;
			else close();
		};
		window.addEventListener('keydown', onkey);
		return () => window.removeEventListener('keydown', onkey);
	});

	// Load on OPEN, not on mount: the dialog is mounted for the whole page life of an
	// admin, and a work's source list is exactly the thing a merge (or another admin)
	// changes underneath you. Re-reading per open keeps the picker honest.
	$effect(() => {
		if (!open || !workId || loaded || loading) return;
		void load(workId);
	});

	async function load(id: string): Promise<void> {
		loading = true;
		error = '';
		try {
			rows = await backend.workSourceRows!(id);
			loaded = true;
		} catch (e) {
			error = e instanceof Error ? e.message : 'Could not load this series’ sources';
			rows = [];
		} finally {
			loading = false;
		}
	}

	function toggle(row: WorkSourceRow): void {
		// Reassigned, not mutated: a `Set` is not deeply reactive, so `$derived` would not
		// re-run on `.add()`/`.delete()` alone.
		const next = new Set(pickedIds);
		if (next.has(row.id)) next.delete(row.id);
		else next.add(row.id);
		pickedIds = next;
	}

	async function run(): Promise<void> {
		if (!workId || splitting || !canSplit) return;
		splitting = true;
		error = '';
		try {
			const r = await backend.splitSourceSeries!(
				workId,
				picked.map((p) => p.id),
			);
			open = false;
			reset();
			onsplit?.({ newWorkId: r.newWorkId, newReaderId: r.newReaderId, title: r.title });
		} catch (e) {
			error = e instanceof Error ? e.message : 'Split failed';
			confirming = false;
		} finally {
			splitting = false;
		}
	}
</script>

{#if open}
	<div class="scrim-wrap" role="dialog" aria-modal="true" aria-label="Split sources">
		<button class="scrim" onclick={close} tabindex="-1" aria-hidden="true"></button>
		<div class="panel">
			<header>
				<div class="head-text">
					<h2>Split sources off this series</h2>
					<p class="sub">
						Pick the sources that are a <strong>different series</strong>. They move to a new entry
						of their own, with their chapters.
					</p>
				</div>
				<button class="icon-btn" onclick={close} disabled={splitting} aria-label="Close">
					<Icon name="x" size={18} />
				</button>
			</header>

			{#if !confirming}
				<div class="results">
					{#if error}
						<div class="msg error" role="alert"><Icon name="alert" size={15} />{error}</div>
					{:else if loading}
						<div class="msg">Loading sources…</div>
					{:else if loaded && rows.length < 2}
						<!-- Nothing to split. One source IS the series; there is no impostor to
						     detach, and the guard below would disable every row anyway. -->
						<div class="msg">
							“{workTitle}” has only one source, so there is nothing to split off.
						</div>
					{/if}

					{#each rows as row (row.id)}
						{@const on = pickedIds.has(row.id)}
						{@const last = on && wouldEmpty}
						<div class="row" class:on>
							<button
								class="pick"
								role="checkbox"
								aria-checked={on}
								aria-label={on ? `Keep ${row.title}` : `Detach ${row.title}`}
								disabled={rows.length < 2}
								onclick={() => toggle(row)}
							>
								<span class="box">
									{#if on}<Icon name="check" size={13} strokeWidth={3} />{/if}
								</span>
								<span class="logo">
									{#if row.iconUrl}
										<img src={row.iconUrl} alt="" loading="lazy" />
									{:else}
										<span class="initial">{row.sourceName.charAt(0)}</span>
									{/if}
								</span>
							</button>
							<div class="meta">
								<span class="title" class:strike={last}>{row.title}</span>
								<div class="sub-line">
									<span class="src">{row.sourceName}</span>
									{#if row.lang}<span>{row.lang}</span>{/if}
									<span>{row.chapterCount} ch</span>
									{#if row.sourceUrl}
										<a href={row.sourceUrl} target="_blank" rel="noopener noreferrer">source ↗</a>
									{/if}
								</div>
							</div>
						</div>
					{/each}

					{#if wouldEmpty}
						<div class="msg warn-inline" role="alert">
							<Icon name="alert" size={15} />
							At least one source has to stay on “{workTitle}” — a series with none would vanish
							from Browse and search.
						</div>
					{/if}
				</div>
			{:else}
				<!-- Confirm step. It names the survivor, names the new entry, and spells out
				     what does NOT move, because none of that is recoverable by re-splitting. -->
				<div class="confirm">
					<div class="warn" role="alert">
						<Icon name="alert" size={16} />
						<div>
							{picked.length}
							{picked.length === 1 ? 'source moves' : 'sources move'} to a new series called
							<strong>{newTitle}</strong>, with
							{picked.reduce((n, p) => n + p.chapterCount, 0)} chapters.
							<strong
								>Reviews, library entries, reading progress and view counts stay with “{workTitle}”</strong
							> — they aren’t attributed per source, so they can’t follow.
						</div>
					</div>
					<ul class="casualties">
						{#each picked as p (p.id)}
							<li>
								<Icon name="arrow-right" size={14} />
								<span>{p.title}</span>
								<span class="dim">{p.sourceName} · {p.chapterCount} ch</span>
							</li>
						{/each}
					</ul>
					{#if error}
						<div class="msg error" role="alert"><Icon name="alert" size={15} />{error}</div>
					{/if}
				</div>
			{/if}

			<footer>
				<span class="count">{picked.length} selected</span>
				{#if confirming}
					<button class="ghost" onclick={() => (confirming = false)} disabled={splitting}>
						Back
					</button>
					<button class="danger" onclick={run} disabled={splitting || !canSplit}>
						{splitting ? 'Splitting…' : `Split off ${picked.length}`}
					</button>
				{:else}
					<button class="ghost" onclick={close}>Cancel</button>
					<button class="primary" onclick={() => (confirming = true)} disabled={!canSplit}>
						Review split
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
	.primary {
		background: var(--k-primary);
		border-color: transparent;
		color: var(--k-on-primary);
	}
	.danger {
		background: #b3261e;
		border-color: transparent;
		color: #fff;
	}
	.ghost:disabled,
	.primary:disabled,
	.danger:disabled,
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
	.msg.warn-inline {
		color: var(--k-text-2);
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
	.pick:disabled {
		cursor: not-allowed;
		opacity: 0.5;
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
	/* `position: relative` for the same reason MergeDialog's `.thumb` needs it — the
	   <img> below is absolutely positioned, so a static parent would size it against
	   `.panel` and paint a source logo over the whole dialog. */
	.logo {
		position: relative;
		display: grid;
		place-items: center;
		width: 30px;
		height: 30px;
		border-radius: 7px;
		overflow: hidden;
		background: var(--k-surface-3);
	}
	.logo img {
		position: absolute;
		inset: 0;
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.initial {
		font-size: 0.8rem;
		font-weight: 700;
		color: var(--k-text-2);
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
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.title.strike {
		text-decoration: line-through;
		color: var(--k-text-dim);
	}
	.sub-line {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		margin-top: 2px;
		font-size: 0.76rem;
		color: var(--k-text-dim);
	}
	.sub-line .src {
		color: var(--k-text-2);
	}
	.sub-line a {
		color: var(--k-text-dim);
	}
	.warn {
		display: flex;
		gap: 10px;
		padding: 12px;
		border: 1px solid rgba(179, 38, 30, 0.5);
		border-radius: 10px;
		background: rgba(179, 38, 30, 0.12);
		font-size: 0.85rem;
		line-height: 1.5;
		color: var(--k-text-1);
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
		color: var(--k-text-1);
		border-bottom: 1px solid var(--k-border-1);
	}
	.casualties .dim {
		margin-left: auto;
		font-size: 0.76rem;
		color: var(--k-text-dim);
	}
	footer {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 12px 16px;
		border-top: 1px solid var(--k-border-1);
	}
	.count {
		flex: 1;
		font-size: 0.8rem;
		color: var(--k-text-dim);
	}
</style>
