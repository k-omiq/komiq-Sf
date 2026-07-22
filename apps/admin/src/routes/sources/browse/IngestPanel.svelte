<script lang="ts">
	import type { Snippet } from 'svelte';
	import IngestCounts, { type CountItem } from './IngestCounts.svelte';

	let {
		heading,
		detail,
		stateLabel,
		tone,
		running,
		counts,
		error = null,
		note = null,
		review = false,
		actions,
		children,
	}: {
		/** What the job is doing, e.g. "Add all from source 42". */
		heading: string;
		/** A short line under the heading, e.g. "2 of 5 sources done". */
		detail?: string;
		stateLabel: string;
		tone?: 'info' | 'success' | 'danger';
		/** Still working: shows the progress bar. */
		running: boolean;
		counts: CountItem[];
		/** Why the job failed, when it did. */
		error?: string | null;
		note?: string | null;
		/** Some series were sent to Duplicates: point the admin there. */
		review?: boolean;
		actions: Snippet;
		/** Extra rows under the counts, e.g. one line per source. */
		children?: Snippet;
	} = $props();
</script>

<section class="k-card panel">
	<div class="top">
		<div class="titles">
			<div class="line">
				<h2 class="heading">{heading}</h2>
				<span class="k-badge" data-tone={tone}>{stateLabel}</span>
			</div>
			{#if detail}<p class="k-hint k-num wrap">{detail}</p>{/if}
		</div>
		<div class="actions">{@render actions()}</div>
	</div>

	{#if error}<p class="error" role="alert">{error}</p>{/if}
	{#if note}<p class="k-hint wrap">{note}</p>{/if}

	<IngestCounts items={counts} />

	{#if running}
		<!-- The server reports no total, so the bar cannot show how far along it is. -->
		<div class="progress" role="progressbar" aria-label="Adding series, in progress">
			<span class="bar"></span>
		</div>
	{/if}

	{#if children}{@render children()}{/if}

	{#if review}
		<p class="k-hint">
			Some series look like works already in the catalogue. Decide on them on the
			<a class="k-link" href="/review">Duplicates</a> page.
		</p>
	{/if}
</section>

<style>
	.panel {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--k-space-4);
		padding: var(--k-space-5);
	}
	.top {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3);
		min-width: 0;
	}
	.titles {
		display: grid;
		gap: 4px;
		flex: 1 1 240px;
		min-width: 0;
	}
	.line {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-2) var(--k-space-3);
		min-width: 0;
	}
	.heading {
		min-width: 0;
		font-size: var(--k-text-lg);
		font-weight: 700;
		overflow-wrap: anywhere;
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}
	/* Source and extension names come from the engine and can be one long word. */
	.wrap {
		overflow-wrap: anywhere;
	}
	.error {
		color: var(--k-danger);
		font-size: var(--k-text-sm);
		line-height: 1.6;
		overflow-wrap: anywhere;
	}
	.progress {
		height: 4px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-5);
		overflow: hidden;
	}
	.bar {
		display: block;
		width: 35%;
		height: 100%;
		border-radius: inherit;
		background: var(--k-accent);
		animation: travel 1.4s ease-in-out infinite;
	}
	@keyframes travel {
		from {
			transform: translateX(-100%);
		}
		to {
			transform: translateX(290%);
		}
	}
	/* No travelling bar for reduced motion: a still, dimmed bar still says "working". */
	@media (prefers-reduced-motion: reduce) {
		.bar {
			width: 100%;
			opacity: 0.45;
			animation: none;
		}
	}
</style>
