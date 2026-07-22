<script lang="ts">
	import type { Snippet } from 'svelte';
	import Button from './Button.svelte';
	import Icon, { type IconName } from './Icon.svelte';

	let {
		loading = false,
		error = null,
		empty = false,
		loadingLabel = 'Loading…',
		skeletonRows = 6,
		emptyTitle = 'Nothing here',
		emptyHint,
		emptyIcon = 'check',
		onretry,
		emptyActions,
		children,
	}: {
		/** First load in flight and nothing to show yet. */
		loading?: boolean;
		/** The load failed and nothing is on screen. */
		error?: string | null;
		/** Loaded fine, zero rows. */
		empty?: boolean;
		loadingLabel?: string;
		/** How many placeholder rows to draw while loading. */
		skeletonRows?: number;
		emptyTitle?: string;
		emptyHint?: string;
		emptyIcon?: IconName;
		onretry?: () => void;
		emptyActions?: Snippet;
		/** The loaded content. */
		children: Snippet;
	} = $props();
</script>

{#if error}
	<div class="k-notice" data-tone="danger" role="alert">
		<Icon name="alert" />
		<div class="k-notice__body">{error}</div>
		{#if onretry}<Button size="sm" onclick={onretry}>Retry</Button>{/if}
	</div>
{:else if loading}
	<!-- Skeleton rows in the shape of the table that replaces them. -->
	<div class="loading" role="status">
		<span class="label">{loadingLabel}</span>
		{#each { length: skeletonRows } as _, i (i)}
			<div class="row">
				<span class="k-skeleton" style:width="{38 - (i % 3) * 7}%"></span>
				<span class="k-skeleton" style:width="{14 + (i % 2) * 5}%"></span>
				<span class="k-skeleton" style:width="12%"></span>
			</div>
		{/each}
	</div>
{:else if empty}
	<div class="k-empty">
		<Icon name={emptyIcon} size={22} />
		<p class="k-empty__title">{emptyTitle}</p>
		{#if emptyHint}<p>{emptyHint}</p>{/if}
		{#if emptyActions}{@render emptyActions()}{/if}
	</div>
{:else}
	{@render children()}
{/if}

<style>
	.loading {
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface);
		overflow: hidden;
	}
	.label {
		display: block;
		padding: 11px var(--k-space-4);
		background: var(--k-surface-2);
		border-bottom: 1px solid var(--k-border);
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		font-weight: 600;
	}
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-4);
		height: 54px;
		padding: 0 var(--k-space-4);
		border-bottom: 1px solid var(--k-border);
	}
	.row:last-child {
		border-bottom: 0;
	}
	.row .k-skeleton {
		height: 12px;
	}
</style>
