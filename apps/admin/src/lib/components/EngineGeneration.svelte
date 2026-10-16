<script lang="ts">
	import { generationToReplace, type EngineGenerationView } from '$lib/engine-generation';
	import { confirm } from '$lib/confirm.svelte';
	import { toast } from '$lib/toast.svelte';
	import Button from './Button.svelte';
	import LoadState from './LoadState.svelte';

	let {
		view,
		loading,
		error,
		onrefresh,
		onstart,
	}: {
		view: EngineGenerationView;
		loading: boolean;
		error: string | null;
		onrefresh: () => Promise<void>;
		/** Gets the quarantined generation shown, so a changed state is refused. */
		onstart: (generationId: string) => Promise<void>;
	} = $props();

	let starting = $state(false);

	async function startGeneration(): Promise<void> {
		const generationId = generationToReplace(view);
		if (starting || loading || !generationId) return;
		starting = true;
		try {
			const accepted = await confirm({
				title: 'Start a new engine generation?',
				message: [
					'Reading progress on extension-source chapters is set aside: kept on the server, no longer shown.',
					'Comment threads on extension-source series and chapters are set aside with their replies.',
					'The extension-source series and chapters are removed from the catalogue; scan or add them again from the new engine.',
					'Works, libraries and MangaDex progress stay.',
					'Reader apps that saved progress offline before the change cannot save that progress; it is dropped, never put on a different chapter.',
				].join('\n\n'),
				confirmLabel: 'Start a new generation',
				danger: true,
			});
			if (!accepted) return;
			await onstart(generationId);
			toast.success('Started a new engine generation.');
			await onrefresh();
		} catch (err) {
			toast.fromError(err, 'Failed to start a new engine generation.');
		} finally {
			starting = false;
		}
	}
</script>

<section
	id="engine-generation"
	class="engine-generation"
	class:quarantined={view.kind === 'quarantined'}
	aria-label="Source engine"
	aria-busy={loading || starting}
>
	{#if view.kind !== 'unsupported'}
		<header class="heading">
			<h2>Source engine</h2>
			<Button
				size="sm"
				variant="ghost"
				icon="refresh"
				busy={loading}
				disabled={starting}
				onclick={() => onrefresh()}>Refresh engine state</Button
			>
		</header>
	{/if}

	<LoadState
		{loading}
		{error}
		loadingLabel="Loading source engine state…"
		skeletonRows={1}
		onretry={() => onrefresh()}
	>
		{#if view.kind === 'unsupported' || view.kind === 'none'}
			<p class="k-muted">{view.message}</p>
		{:else}
			<div class="current">
				<p class="state-line">
					<strong class="state" data-state={view.current.state}>{view.current.stateLabel}</strong>
					<span class="k-muted"
						>since <time title={view.current.since}>{view.current.sinceLabel}</time> (local time)</span
					>
				</p>
				{#if view.current.reason}<p>{view.current.reason}</p>{/if}
				{#if view.kind === 'active' && view.note}<p class="k-muted">{view.note}</p>{/if}
			</div>

			{#if view.kind === 'quarantined'}
				<p>Chapters, pages and thumbnails from extension sources are unavailable.</p>
				<div class="recovery">
					<div class="option">
						<h3>Re-attach the old engine data</h3>
						<p>
							Stop the server. Put back the copy of the engine's data folder (the engine volume)
							from before the change. Start the server. The server checks again and leaves
							quarantine by itself.
						</p>
						<p>
							If only the marker was lost and the data is the same, run
							<code>komika-server engine reattach</code>. It checks every stored source identity and
							refuses on a mismatch.
						</p>
					</div>
					<div class="option">
						<h3>Start a new generation</h3>
						<p>Keep the engine you have now. Review what happens before you confirm.</p>
						<Button variant="danger-solid" busy={starting} onclick={startGeneration}
							>Start a new generation</Button
						>
					</div>
				</div>
			{/if}
		{/if}

		{#if view.detached.length > 0}
			<div class="history">
				<h3>Previous generations</h3>
				<ul>
					{#each view.detached as row (row.id)}
						<li>
							<p class="state-line">
								<strong>{row.stateLabel}</strong>
								<span class="k-muted"
									>since <time title={row.since}>{row.sinceLabel}</time> (local time)</span
								>
							</p>
							{#if row.reason}<p class="k-muted">{row.reason}</p>{/if}
						</li>
					{/each}
				</ul>
			</div>
		{/if}
	</LoadState>
</section>

<style>
	.engine-generation {
		display: grid;
		gap: var(--k-space-4);
		padding: var(--k-space-5);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface);
		font-size: var(--k-text-sm);
		line-height: 1.6;
	}
	.quarantined {
		background: var(--k-warning-soft);
	}
	.heading,
	.state-line {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-2) var(--k-space-3);
	}
	.heading {
		justify-content: space-between;
	}
	h2 {
		font-size: var(--k-text-lg);
		font-weight: 700;
	}
	h3 {
		font-size: var(--k-text-sm);
		font-weight: 700;
	}
	.current,
	.option,
	.history {
		display: grid;
		gap: var(--k-space-2);
	}
	.state {
		padding: 2px 9px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
	}
	.state[data-state='active'] {
		background: var(--k-success-soft);
		color: var(--k-success);
	}
	.state[data-state='quarantined'] {
		background: var(--k-warning-soft);
		color: var(--k-warning);
	}
	.recovery {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 320px), 1fr));
		gap: var(--k-space-5);
	}
	.option {
		align-content: start;
		justify-items: start;
		padding: var(--k-space-4);
		border-radius: var(--k-radius);
		background: var(--k-surface);
	}
	code {
		overflow-wrap: anywhere;
	}
	.history ul {
		display: grid;
		gap: var(--k-space-2);
		padding: 0;
		list-style: none;
	}
	.history li {
		padding: var(--k-space-3) var(--k-space-4);
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	/* Keep the shared loading/error components as soft fills within this section. */
	.engine-generation :global(.loading),
	.engine-generation :global(.label),
	.engine-generation :global(.row),
	.engine-generation :global(.k-notice) {
		border: 0;
	}
</style>
