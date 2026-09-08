<script lang="ts">
	import type { CloudflareStep } from '@komika/api';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';

	// A server-run checklist: one flat row per step with its state (waiting, running,
	// done, failed) and the server's message. A failed row offers Retry.
	let {
		steps,
		onretry,
		retrying = false,
		label = 'Progress',
	}: {
		steps: CloudflareStep[];
		onretry?: () => void;
		retrying?: boolean;
		label?: string;
	} = $props();

	const stateText: Record<CloudflareStep['state'], string> = {
		waiting: 'Waiting',
		running: 'Running',
		done: 'Done',
		failed: 'Failed',
	};
</script>

<ol class="checklist" aria-label={label} aria-live="polite">
	{#each steps as step (step.id)}
		<li class="row" data-state={step.state}>
			<span class="mark" title={stateText[step.state]}>
				{#if step.state === 'done'}
					<Icon name="check" size={15} />
				{:else if step.state === 'running'}
					<span class="k-spin"><Icon name="refresh" size={15} /></span>
				{:else if step.state === 'failed'}
					<Icon name="alert" size={15} />
				{:else}
					<span class="dot"></span>
				{/if}
				<span class="sr">{stateText[step.state]}:</span>
			</span>
			<span class="text">
				<span class="label">{step.label}</span>
				{#if step.message && step.state !== 'waiting'}
					<span class="message">{step.message}</span>
				{/if}
			</span>
			{#if step.state === 'failed' && onretry}
				<Button variant="ghost" size="sm" icon="refresh" busy={retrying} onclick={onretry}>
					Retry
				</Button>
			{/if}
		</li>
	{/each}
</ol>

<style>
	.checklist {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.row {
		display: grid;
		grid-template-columns: 20px minmax(0, 1fr) auto;
		align-items: start;
		gap: var(--k-space-3);
		padding: 10px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.mark {
		display: grid;
		place-items: center;
		height: 20px;
		color: var(--k-text-dim);
	}
	.row[data-state='done'] .mark {
		color: var(--k-text-2);
	}
	.row[data-state='failed'] .mark {
		color: var(--k-danger);
	}
	.dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--k-surface-5);
	}
	.text {
		display: grid;
		gap: 2px;
		min-width: 0;
		padding-top: 1px;
	}
	.label {
		font-size: var(--k-text-sm);
		color: var(--k-text-1);
		overflow-wrap: anywhere;
	}
	.row[data-state='waiting'] .label {
		color: var(--k-text-dim);
	}
	.message {
		font-size: var(--k-text-xs);
		line-height: 1.5;
		color: var(--k-text-dim);
		overflow-wrap: anywhere;
	}
	.row[data-state='failed'] .message {
		color: var(--k-danger);
	}
	.row :global(.k-btn) {
		margin: -4px -8px -4px 0;
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
</style>
