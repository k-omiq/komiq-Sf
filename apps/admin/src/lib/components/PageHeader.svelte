<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from './Icon.svelte';

	let {
		title,
		description,
		crumbs = [],
		actions,
		children,
	}: {
		title: string;
		/** One or two plain sentences under the title. Use `children` for rich content. */
		description?: string;
		/** Parent pages, outermost first. The current page is not listed. */
		crumbs?: { label: string; href: string }[];
		actions?: Snippet;
		children?: Snippet;
	} = $props();
</script>

<svelte:head>
	<title>{title} - Komiq admin</title>
</svelte:head>

<header class="head">
	<div class="text">
		{#if crumbs.length}
			<nav class="crumbs" aria-label="Breadcrumb">
				{#each crumbs as crumb (crumb.href)}
					<a href={crumb.href}>{crumb.label}</a>
					<Icon name="chevron-right" size={12} />
				{/each}
			</nav>
		{/if}
		<h1>{title}</h1>
		{#if description}<p class="desc">{description}</p>{/if}
		{#if children}<div class="desc">{@render children()}</div>{/if}
	</div>
	{#if actions}
		<div class="actions">{@render actions()}</div>
	{/if}
</header>

<style>
	.head {
		display: flex;
		align-items: flex-end;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-4) var(--k-space-6);
		min-width: 0;
	}
	.text {
		flex: 1 1 320px;
		min-width: 0;
	}
	.crumbs {
		display: flex;
		align-items: center;
		gap: 6px;
		margin-bottom: var(--k-space-2);
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		font-weight: 600;
	}
	.crumbs a:hover {
		color: var(--k-text-bright);
	}
	h1 {
		font-size: var(--k-text-2xl);
		font-weight: 700;
		letter-spacing: -0.03em;
		overflow-wrap: anywhere;
	}
	.desc {
		margin-top: 6px;
		max-width: 72ch;
		color: var(--k-text-dim);
		font-size: var(--k-text-md);
		line-height: 1.6;
	}
	.actions {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-2);
		min-width: 0;
	}
</style>
