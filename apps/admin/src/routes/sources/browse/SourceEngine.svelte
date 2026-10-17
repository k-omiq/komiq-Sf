<script lang="ts">
	import type { EngineStatus, SourceEngineRoute } from '@komika/api';
	import { loadEngines, loadSourceEngineRoute, setSourceEngine } from '$lib/data';
	import Button from '$lib/components/Button.svelte';

	let { sourceId }: { sourceId: string } = $props();
	let engines = $state<EngineStatus[]>([]);
	let route = $state<SourceEngineRoute | null>(null);
	let engineId = $state('owner');
	let strict = $state(false);
	let loading = $state(true);
	let saving = $state(false);
	let error = $state<string | null>(null);
	let saved = $state(false);
	const hasRegional = $derived(engines.some((e) => e.id !== 'owner'));

	$effect(() => {
		let current = true;
		loading = true;
		Promise.all([loadEngines(), loadSourceEngineRoute(sourceId)])
			.then(([rows, stored]) => {
				if (!current) return;
				engines = rows;
				route = stored;
				engineId = stored?.engineId ?? 'owner';
				strict = stored?.strict ?? false;
			})
			.catch((err) => {
				if (current) error = err instanceof Error ? err.message : 'Failed to load engine routing.';
			})
			.finally(() => {
				if (current) loading = false;
			});
		return () => {
			current = false;
		};
	});

	async function save(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		if (saving || loading) return;
		saving = true;
		error = null;
		saved = false;
		try {
			route = await setSourceEngine(
				sourceId,
				engineId === 'owner' ? null : engineId,
				engineId !== 'owner' && strict,
			);
			engineId = route?.engineId ?? 'owner';
			strict = route?.strict ?? false;
			saved = true;
		} catch (err) {
			error = err instanceof Error ? err.message : 'Failed to save engine routing.';
		} finally {
			saving = false;
		}
	}
</script>

{#if hasRegional || route}
	<section class="source-engine" aria-label="Source engine">
		<form
			onsubmit={save}
			oninput={() => {
				saved = false;
			}}
		>
			<label class="k-field engine-field">
				<span class="k-label">Engine</span>
				<select class="k-select" bind:value={engineId} disabled={loading || saving}>
					<option value="owner">Owner</option>
					{#if route && !engines.some((e) => e.id === route?.engineId)}
						<option value={route.engineId}>{route.engineId} (not configured)</option>
					{/if}
					{#each engines.filter((e) => e.id !== 'owner') as engine (engine.id)}
						<option value={engine.id}>{engine.id}{engine.reachable ? '' : ' (unreachable)'}</option>
					{/each}
				</select>
			</label>
			<label class="strict">
				<input
					type="checkbox"
					bind:checked={strict}
					disabled={loading || saving || engineId === 'owner'}
				/>
				Strict
			</label>
			<Button type="submit" busy={saving} disabled={loading}>Save engine</Button>
		</form>
		<p class="k-hint">
			Fetch pages and covers through this engine. Strict routes show an error when the regional
			engine cannot serve a request.
		</p>
		{#if route}
			<p class="k-hint">
				{route.active ? 'Active' : 'Inactive'} · {route.versionCheck.replaceAll('_', ' ')}
			</p>
		{/if}
		{#if error}<p class="error" role="alert">{error}</p>{/if}
		{#if saved}<p class="k-hint" role="status">Engine saved.</p>{/if}
	</section>
{/if}

<style>
	.source-engine {
		display: grid;
		gap: var(--k-space-3);
		padding: var(--k-space-5);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface);
	}
	form {
		display: flex;
		flex-wrap: wrap;
		align-items: end;
		gap: var(--k-space-4);
	}
	.engine-field {
		flex: 1 1 180px;
		max-width: 360px;
	}
	.strict {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		align-self: center;
	}
	.error {
		color: var(--k-danger);
		overflow-wrap: anywhere;
	}
	.source-engine :global(.k-select),
	.source-engine :global(.k-btn) {
		border: 0;
	}
</style>
