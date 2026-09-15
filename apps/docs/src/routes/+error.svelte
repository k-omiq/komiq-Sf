<script lang="ts">
	import { page } from '$app/state';
	import { searchUi } from '$lib/search-ui.svelte';
	import Icon from '$lib/components/Icon.svelte';

	const missing = $derived(page.status === 404);
</script>

<svelte:head>
	<title>{missing ? 'Page not found' : 'Something went wrong'} · komiq docs</title>
	<meta name="robots" content="noindex" />
</svelte:head>

<div class="lost">
	<h1>{missing ? 'This page does not exist' : 'This page did not load'}</h1>
	<p class="lede">
		{#if missing}
			The address may be wrong, or the page may have moved. Search finds every page and section.
		{:else}
			The page failed with error {page.status}. Load the page again. If it keeps happening, start
			from the home page.
		{/if}
	</p>
	<div class="k-toolbar">
		<button type="button" class="k-btn k-btn--primary" onclick={() => (searchUi.open = true)}>
			<Icon name="search" />
			Search the docs
		</button>
		<a class="k-btn" href="/">Go to the home page</a>
	</div>
</div>

<style>
	.lost {
		display: grid;
		justify-items: start;
		gap: var(--k-space-3);
		max-width: 560px;
		padding-top: 8vh;
	}
	h1 {
		font-size: clamp(30px, 5vw, 40px);
		font-weight: 700;
		letter-spacing: -0.035em;
		line-height: 1.1;
	}
	.lede {
		margin-bottom: var(--k-space-3);
		color: var(--k-text-muted);
		font-size: var(--k-text-lg);
		line-height: 1.6;
	}
</style>
