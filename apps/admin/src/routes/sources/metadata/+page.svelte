<script lang="ts">
	import { onDestroy } from 'svelte';
	import type { MetadataFieldGroup } from '@komika/api';
	import { auth } from '$lib/auth.svelte';
	import {
		loadSourceMetadataBars,
		loadSources,
		metadataBarsSupported,
		setSourceMetadataBar,
	} from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Switch from '$lib/components/Switch.svelte';

	const groups: { field: MetadataFieldGroup; label: string }[] = [
		{ field: 'TITLE', label: 'Title' },
		{ field: 'DESCRIPTION', label: 'Description' },
		{ field: 'CREDITS', label: 'Authors' },
		{ field: 'TAGS', label: 'Tags' },
		{ field: 'COVER', label: 'Cover' },
		{ field: 'STATUS', label: 'Status' },
	];
	let sources = $state<{ id: string; name: string }[]>([]);
	let bars = $state<string[]>([]);
	let supported = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let saving = $state<string | null>(null);
	let result = $state('');
	let loadSeq = 0;
	const key = (sourceId: string, field: MetadataFieldGroup) => `${sourceId}:${field}`;

	async function refresh(): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		error = null;
		try {
			const rows = await loadSourceMetadataBars();
			if (seq !== loadSeq) return;
			supported = metadataBarsSupported();
			if (!supported) return;
			const installed = await loadSources();
			if (seq !== loadSeq) return;
			const byId = new Map(
				installed.map((s) => [String(s.id), { id: String(s.id), name: s.name }]),
			);
			byId.delete('mangadex');
			sources = [
				{ id: 'mangadex', name: 'MangaDex' },
				...[...byId.values()].sort(
					(a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id),
				),
			];
			bars = rows.map((row) => key(row.sourceId, row.fieldGroup));
		} catch (err) {
			if (seq !== loadSeq) return;
			error = err instanceof Error ? err.message : 'Failed to load metadata bars.';
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	$effect(() => {
		if (!auth.user) return;
		void refresh();
	});
	onDestroy(() => ++loadSeq);

	async function setAllowed(
		sourceId: string,
		field: MetadataFieldGroup,
		allowed: boolean,
	): Promise<void> {
		if (saving || !supported) return;
		saving = key(sourceId, field);
		result = '';
		try {
			const saved = await setSourceMetadataBar(sourceId, field, !allowed);
			const savedKey = key(saved.sourceId, saved.fieldGroup);
			bars = bars.filter((bar) => bar !== savedKey);
			if (saved.barred) bars = [...bars, savedKey];
			result = `Re-checked ${saved.works} works`;
		} catch (err) {
			supported = metadataBarsSupported();
			if (supported) toast.fromError(err, 'Failed to save metadata bar.');
			await refresh();
		} finally {
			saving = null;
		}
	}
</script>

<div class="k-page k-stack-lg">
	<PageHeader
		title="Metadata"
		description="Choose which sources may supply each kind of series detail. A switch on means allowed. Changes save right away."
	/>
	{#if error}
		<div class="k-notice" data-tone="danger" role="alert">
			<Icon name="alert" />
			<div class="k-notice__body">{error}</div>
			<Button size="sm" onclick={refresh}>Retry</Button>
		</div>
	{:else if loading}
		<p class="k-hint" role="status">Loading metadata bars…</p>
	{:else if !supported}
		<p class="k-hint">This server does not support metadata bars</p>
	{:else}
		<div class="table-scroll">
			<table aria-label="Allowed metadata by source" aria-busy={saving !== null}>
				<thead>
					<tr>
						<th scope="col">Source</th>
						{#each groups as group (group.field)}<th scope="col">{group.label}</th>{/each}
					</tr>
				</thead>
				<tbody>
					{#each sources as source (source.id)}
						<tr>
							<th scope="row"><span>{source.name}</span><small>{source.id}</small></th>
							{#each groups as group (group.field)}
								<td>
									<Switch
										checked={!bars.includes(key(source.id, group.field))}
										label={`Allow ${group.label.toLowerCase()} from ${source.name} (${source.id})`}
										disabled={saving !== null}
										onchange={(allowed) => setAllowed(source.id, group.field, allowed)}
									/>
								</td>
							{/each}
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		<p class="k-hint" role="status" aria-live="polite">{saving ? 'Saving…' : result}</p>
	{/if}
</div>

<style>
	.table-scroll {
		overflow-x: auto;
	}
	table {
		width: 100%;
		border-collapse: separate;
		border-spacing: 0 var(--k-space-2);
		font-size: var(--k-text-sm);
	}
	th,
	td {
		padding: var(--k-space-3) var(--k-space-4);
		text-align: center;
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
		min-width: 180px;
		border-radius: var(--k-radius) 0 0 var(--k-radius);
		overflow-wrap: anywhere;
	}
	tbody td:last-child {
		border-radius: 0 var(--k-radius) var(--k-radius) 0;
	}
	small {
		display: block;
		margin-top: var(--k-space-1);
		color: var(--k-text-dim);
		font-weight: 400;
	}
</style>
