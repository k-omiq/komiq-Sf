<script lang="ts">
	import type { Snippet } from 'svelte';
	import Brand from './Brand.svelte';

	// The frame every first-run screen shares: brand, an optional step bar, a title,
	// one line of context, the step's body, and a footer row for its actions. Flat
	// surfaces only; the primary action is the one filled control on the screen.
	let {
		title,
		lead,
		step,
		steps,
		children,
		footer,
		aside,
	}: {
		title: string;
		lead?: string;
		/** 1-based step number; with `steps`, draws the step bar. */
		step?: number;
		steps?: string[];
		children: Snippet;
		footer?: Snippet;
		/** Small text under the card (e.g. "Change server"). */
		aside?: Snippet;
	} = $props();
</script>

<div class="page">
	<div class="column">
		<header class="head">
			<Brand size={22} />
			{#if step && steps}
				<p class="count k-num">Step {step} of {steps.length}</p>
			{/if}
		</header>

		{#if step && steps}
			<ol class="bar" aria-label="Setup progress">
				{#each steps as name, i (name)}
					<li
						class:done={i + 1 < step}
						class:current={i + 1 === step}
						aria-current={i + 1 === step ? 'step' : undefined}
					>
						<span class="sr">{name}</span>
					</li>
				{/each}
			</ol>
		{/if}

		<section class="card" aria-labelledby="onboarding-title">
			<h1 id="onboarding-title" class="title">{title}</h1>
			{#if lead}<p class="lead">{lead}</p>{/if}
			<div class="body">{@render children()}</div>
			{#if footer}<div class="footer">{@render footer()}</div>{/if}
		</section>

		{#if aside}<div class="aside">{@render aside()}</div>{/if}
	</div>
</div>

<style>
	.page {
		min-height: 100dvh;
		display: grid;
		place-items: start center;
		padding: clamp(32px, 12vh, 120px) var(--k-space-4) var(--k-space-8);
		background: var(--k-bg);
	}
	.column {
		width: 100%;
		max-width: 460px;
		display: grid;
		gap: var(--k-space-5);
	}
	.head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		line-height: 1;
	}
	.count {
		margin: 0;
		font-size: var(--k-text-xs);
		color: var(--k-text-dim);
	}
	.bar {
		display: grid;
		grid-auto-flow: column;
		grid-auto-columns: 1fr;
		gap: 6px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.bar li {
		height: 3px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-5);
		transition: background var(--k-dur) var(--k-ease);
	}
	.bar li.done,
	.bar li.current {
		background: var(--k-text-2);
	}
	.bar li.current {
		opacity: 0.55;
	}
	.card {
		display: grid;
		gap: var(--k-space-2);
	}
	.title {
		margin: 0;
		font-size: var(--k-text-2xl);
		font-weight: 700;
		letter-spacing: -0.01em;
		line-height: 1.2;
		color: var(--k-text-bright);
	}
	.lead {
		margin: 0;
		font-size: var(--k-text-md);
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	.body {
		display: grid;
		gap: var(--k-space-4);
		margin-top: var(--k-space-5);
	}
	.footer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-3);
		margin-top: var(--k-space-6);
	}
	.aside {
		font-size: var(--k-text-xs);
		color: var(--k-text-dim);
		text-align: center;
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
