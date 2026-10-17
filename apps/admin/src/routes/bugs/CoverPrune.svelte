<script lang="ts">
	import { onDestroy } from 'svelte';
	import type { CoverPruneStatus } from '@komika/api';
	import { auth } from '$lib/auth.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { coverAmount, formatBytes, formatCount } from '$lib/cover-rank';
	import { loadCoverPrune, setCoverPrune } from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Switch from '$lib/components/Switch.svelte';

	let status = $state<CoverPruneStatus | null>(null);
	let supported = $state(true);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let saving = $state(false);
	let result = $state('');
	let loadSeq = 0;

	const amount = $derived(status ? coverAmount(status.covers, status.bytes) : '');

	async function refresh(): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		error = null;
		try {
			const next = await loadCoverPrune();
			if (seq !== loadSeq) return;
			supported = next !== null;
			status = next;
			result = '';
		} catch (err) {
			if (seq !== loadSeq) return;
			error = err instanceof Error ? err.message : 'Failed to load the unpicked covers.';
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	$effect(() => {
		if (!auth.user) return;
		void refresh();
	});
	onDestroy(() => ++loadSeq);

	async function toggle(on: boolean): Promise<void> {
		if (saving || !status) return;
		if (on) {
			const ok = await confirm({
				title: 'Delete unpicked covers?',
				message: `This deletes ${amount}. MangaDex covers, covers a series shows, and covers an admin picked or uploaded are never deleted. It runs in the background. You can turn it off any time and it stops at the next cover.`,
				confirmLabel: 'Delete covers',
				danger: true,
			});
			if (!ok) return;
		}
		saving = true;
		result = '';
		// A load still in flight must not overwrite the answer to this change, and its
		// `finally` no longer clears the loading state, so clear it here.
		++loadSeq;
		loading = false;
		try {
			const next = await setCoverPrune(on);
			status = next;
			const left = coverAmount(next.covers, next.bytes);
			result = next.enabled
				? `On. Deleting ${left} in the background. Refresh to see what is left.`
				: `Off. Nothing more is deleted. ${left} could still be deleted.`;
		} catch (err) {
			toast.fromError(err, 'Failed to change the setting.');
			await refresh();
		} finally {
			saving = false;
		}
	}
</script>

<section class="section" aria-labelledby="cover-prune-h">
	<div class="section-head">
		<h2 id="cover-prune-h" class="section-title">Delete unpicked covers</h2>
		<p class="section-desc">
			Frees space by deleting covers no series uses. MangaDex covers and covers an admin picked or
			uploaded are kept.
		</p>
	</div>

	{#if error}
		<div class="k-notice" data-tone="danger" role="alert">
			<Icon name="alert" />
			<div class="k-notice__body">{error}</div>
			<Button size="sm" onclick={refresh}>Retry</Button>
		</div>
	{:else if loading && !status}
		<p class="k-hint" role="status">Loading unpicked covers…</p>
	{:else if !supported || !status}
		<p class="k-hint">This server does not support cover ranking</p>
	{:else}
		<div class="setting">
			<div class="setting-text">
				<p class="setting-name">Delete unpicked covers</p>
				<p class="k-hint">
					{#if status.covers === 0}
						Nothing to delete right now.
					{:else if status.enabled}
						Left to delete: {amount}.
					{:else}
						Would delete {amount}.
					{/if}
				</p>
			</div>
			<div class="controls">
				<Button size="sm" variant="ghost" icon="refresh" busy={loading} onclick={refresh}>
					Refresh
				</Button>
				<Switch
					label="Delete unpicked covers"
					checked={status.enabled}
					disabled={saving}
					onchange={toggle}
				/>
			</div>
		</div>

		{#if status.sources.length > 0}
			<div class="table-scroll">
				<table class:is-dim={loading} aria-label="Unpicked covers by source">
					<thead>
						<tr>
							<th scope="col">Source</th>
							<th scope="col">Covers</th>
							<th scope="col">Size</th>
						</tr>
					</thead>
					<tbody>
						{#each status.sources as source (source.sourceId)}
							<tr>
								<th scope="row">
									<span>{source.sourceName || source.sourceId}</span>
									<small class="k-mono">{source.sourceId}</small>
								</th>
								<td class="k-num">{formatCount(source.covers)}</td>
								<td class="k-num">{formatBytes(source.bytes)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}

		<p class="k-hint" role="status" aria-live="polite">{saving ? 'Saving…' : result}</p>
	{/if}
</section>

<style>
	.setting {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3) var(--k-space-6);
	}
	.setting-text {
		flex: 1 1 280px;
		min-width: 0;
		display: grid;
		gap: 4px;
	}
	.setting-name {
		color: var(--k-text-1);
		font-weight: 700;
	}
	.controls {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
	}
	.table-scroll {
		overflow-x: auto;
	}
	table {
		width: 100%;
		border-collapse: separate;
		border-spacing: 0 var(--k-space-1);
		font-size: var(--k-text-sm);
		transition: opacity var(--k-dur) var(--k-ease);
	}
	table.is-dim {
		opacity: 0.6;
	}
	th,
	td {
		padding: var(--k-space-2) var(--k-space-4);
		text-align: right;
	}
	thead th {
		color: var(--k-text-dim);
		font-weight: 600;
	}
	th:first-child {
		text-align: left;
	}
	tbody tr {
		background: var(--k-surface-2);
	}
	tbody th {
		min-width: 160px;
		border-radius: var(--k-radius) 0 0 var(--k-radius);
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	tbody td:last-child {
		border-radius: 0 var(--k-radius) var(--k-radius) 0;
	}
	small {
		display: block;
		margin-top: 2px;
		color: var(--k-text-dim);
		font-weight: 400;
	}
</style>
