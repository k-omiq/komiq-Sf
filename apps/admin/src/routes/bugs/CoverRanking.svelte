<script lang="ts">
	import { onDestroy, tick } from 'svelte';
	import { auth } from '$lib/auth.svelte';
	import { href } from '$lib/paths';
	import { confirm } from '$lib/confirm.svelte';
	import {
		defaultSourceOrder,
		moveItem,
		rankToSave,
		rankedOrder,
		sameOrder,
		type RankSource,
	} from '$lib/cover-rank';
	import { loadCoverSourceRank, loadSources, setCoverSourceRank } from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';

	let defaults = $state<RankSource[]>([]);
	let rows = $state<RankSource[]>([]);
	/** The order the server holds now, as shown (ranked ids, then the rest). */
	let savedIds = $state<string[]>([]);
	let stored = $state<string[]>([]);
	let supported = $state(true);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let saving = $state(false);
	let result = $state('');
	let announce = $state('');
	let dragId = $state<string | null>(null);
	let listEl = $state<HTMLOListElement | null>(null);
	let loadSeq = 0;

	const ids = $derived(rows.map((r) => r.id));
	const dirty = $derived(!sameOrder(ids, savedIds));
	/** How many rows from the top a save stores; the rest keep the default order. */
	const rankedCount = $derived(rankToSave(ids, defaults, stored).length);

	async function refresh(): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		error = null;
		try {
			const rank = await loadCoverSourceRank();
			if (seq !== loadSeq) return;
			if (rank === null) {
				supported = false;
				return;
			}
			supported = true;
			// Without the source list the ranking still shows, MangaDex and ranked ids only.
			const installed = await loadSources().catch(() => []);
			if (seq !== loadSeq) return;
			defaults = defaultSourceOrder(installed);
			apply(rank);
		} catch (err) {
			if (seq !== loadSeq) return;
			error = err instanceof Error ? err.message : 'Failed to load the cover ranking.';
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	function apply(rank: string[]): void {
		stored = rank;
		rows = rankedOrder(rank, defaults);
		savedIds = rows.map((r) => r.id);
	}

	$effect(() => {
		if (!auth.user) return;
		void refresh();
	});
	onDestroy(() => {
		++loadSeq;
		endDrag();
	});

	async function move(id: string, dir: -1 | 1): Promise<void> {
		const from = ids.indexOf(id);
		const to = from + dir;
		if (from < 0 || to < 0 || to >= rows.length) return;
		rows = moveItem(rows, from, to);
		result = '';
		announce = `${rows[to].name} moved to position ${to + 1}.`;
		// At the top or bottom the pressed arrow turns off; keep focus on the row.
		await tick();
		const row = listEl?.querySelector<HTMLElement>(`[data-rank-id="${CSS.escape(id)}"]`);
		const pressed = row?.querySelector<HTMLButtonElement>(`[data-dir="${dir}"]`);
		if (pressed && pressed.disabled)
			row?.querySelector<HTMLButtonElement>(`[data-dir="${-dir}"]`)?.focus();
	}

	function remove(id: string): void {
		const row = rows.find((r) => r.id === id);
		rows = rows.filter((r) => r.id !== id);
		result = '';
		if (row) announce = `${row.name} removed.`;
	}

	// Pointer drag on the handle: works with mouse, pen and touch, and in the desktop app.
	// The listeners sit on the window: reordering moves the row nodes, which can drop a
	// pointer capture.
	function onGripDown(e: PointerEvent, id: string): void {
		if (saving || dragId || e.button !== 0) return;
		e.preventDefault();
		dragId = id;
		window.addEventListener('pointermove', onDragMove);
		window.addEventListener('pointerup', endDrag);
		window.addEventListener('pointercancel', endDrag);
	}

	function onDragMove(e: PointerEvent): void {
		if (!dragId || !listEl) return;
		const items = [...listEl.querySelectorAll<HTMLElement>('[data-rank-id]')];
		if (items.length === 0) return;
		let to = items.findIndex((el) => e.clientY < el.getBoundingClientRect().bottom);
		if (to === -1) to = items.length - 1;
		const from = ids.indexOf(dragId);
		if (from < 0 || to === from) return;
		rows = moveItem(rows, from, to);
		result = '';
	}

	function endDrag(): void {
		window.removeEventListener('pointermove', onDragMove);
		window.removeEventListener('pointerup', endDrag);
		window.removeEventListener('pointercancel', endDrag);
		if (!dragId) return;
		const to = ids.indexOf(dragId);
		if (to >= 0) announce = `${rows[to].name} moved to position ${to + 1}.`;
		dragId = null;
	}

	async function save(next: string[]): Promise<void> {
		if (saving) return;
		saving = true;
		result = '';
		try {
			apply(await setCoverSourceRank(next));
			result =
				next.length > 0
					? 'Saved. Covers move to their ranked source in the background.'
					: 'Ranking cleared.';
		} catch (err) {
			toast.fromError(err, 'Failed to save the cover ranking.');
		} finally {
			saving = false;
		}
	}

	async function reset(): Promise<void> {
		const ok = await confirm({
			title: 'Reset the cover ranking?',
			message:
				'New cover choices use the default order again: MangaDex first, then the source each series was added from first. Covers that already moved stay until they change.',
			confirmLabel: 'Reset ranking',
		});
		if (ok) await save([]);
	}
</script>

<section class="section" aria-labelledby="cover-rank-h">
	<div class="section-head">
		<h2 id="cover-rank-h" class="section-title">Cover ranking</h2>
		<p class="section-desc">
			A series uses the cover from the highest-ranked source that has one. A cover an admin picked
			or uploaded always wins. Source bars on the
			<a class="k-link" href={href('/sources/metadata')}>Metadata</a> page still apply. Sources below
			the ones you move keep the default order.
		</p>
	</div>

	{#if error}
		<div class="k-notice" data-tone="danger" role="alert">
			<Icon name="alert" />
			<div class="k-notice__body">{error}</div>
			<Button size="sm" onclick={refresh}>Retry</Button>
		</div>
	{:else if loading}
		<p class="k-hint" role="status">Loading the cover ranking…</p>
	{:else if !supported}
		<p class="k-hint">This server does not support cover ranking</p>
	{:else}
		<ol class="rank" bind:this={listEl} aria-label="Cover ranking, best first" aria-busy={saving}>
			{#each rows as row, i (row.id)}
				<li
					class="row"
					class:is-dragging={dragId === row.id}
					class:is-default={i >= rankedCount}
					data-rank-id={row.id}
				>
					<span
						class="grip"
						aria-hidden="true"
						title="Drag to reorder"
						onpointerdown={(e) => onGripDown(e, row.id)}
					>
						<Icon name="grip" strokeWidth={3} />
					</span>
					<span class="pos k-num">{i + 1}</span>
					<span class="name">
						<span class="title">{row.name}</span>
						<small class="k-mono k-muted"
							>{row.id}{#if !row.installed}&nbsp;· not installed{/if}{#if i >= rankedCount}&nbsp;·
								default order{/if}</small
						>
					</span>
					<span class="moves">
						{#if !row.installed}
							<Button
								size="sm"
								variant="ghost"
								icon="x"
								iconOnly
								aria-label={`Remove ${row.name}`}
								disabled={saving}
								onclick={() => remove(row.id)}
							/>
						{/if}
						<Button
							size="sm"
							variant="ghost"
							icon="chevron-up"
							iconOnly
							data-dir="-1"
							aria-label={`Move ${row.name} up`}
							disabled={saving || i === 0}
							onclick={() => move(row.id, -1)}
						/>
						<Button
							size="sm"
							variant="ghost"
							icon="chevron-down"
							iconOnly
							data-dir="1"
							aria-label={`Move ${row.name} down`}
							disabled={saving || i === rows.length - 1}
							onclick={() => move(row.id, 1)}
						/>
					</span>
				</li>
			{/each}
		</ol>
		<p class="k-sr-only" aria-live="polite">{announce}</p>

		<div class="actions">
			<Button
				variant="primary"
				icon="check"
				busy={saving}
				disabled={!dirty}
				onclick={() => save(rankToSave(ids, defaults, stored))}
			>
				Save ranking
			</Button>
			<Button variant="ghost" disabled={saving || (stored.length === 0 && !dirty)} onclick={reset}>
				Reset to default
			</Button>
			<span class="k-hint" role="status" aria-live="polite">
				{#if saving}Saving…{:else if dirty}Unsaved changes{:else}{result}{/if}
			</span>
		</div>
	{/if}
</section>

<style>
	.rank {
		display: grid;
		gap: var(--k-space-1);
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.row {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		min-width: 0;
		padding: var(--k-space-2) var(--k-space-2) var(--k-space-2) var(--k-space-1);
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		transition: background var(--k-dur) var(--k-ease);
	}
	.row:hover {
		background: var(--k-surface-3);
	}
	.row.is-dragging {
		background: var(--k-accent-soft);
	}
	.row.is-default .pos {
		color: var(--k-text-dim);
	}
	.grip {
		flex: none;
		display: grid;
		place-items: center;
		width: 28px;
		height: 32px;
		border-radius: var(--k-radius-sm);
		color: var(--k-text-dim);
		cursor: grab;
		touch-action: none;
		user-select: none;
	}
	.row.is-dragging .grip {
		cursor: grabbing;
		color: var(--k-text-1);
	}
	.pos {
		flex: none;
		min-width: 2ch;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		text-align: right;
	}
	.name {
		flex: 1 1 auto;
		min-width: 0;
		display: grid;
		gap: 2px;
		overflow-wrap: anywhere;
	}
	.title {
		color: var(--k-text-bright);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.moves {
		flex: none;
		display: flex;
		gap: 2px;
	}
	.actions {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-2) var(--k-space-3);
	}
</style>
