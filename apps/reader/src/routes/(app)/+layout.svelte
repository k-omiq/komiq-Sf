<script lang="ts">
	import Header from '$lib/components/Header.svelte';
	import BottomNav from '$lib/components/BottomNav.svelte';

	let { children } = $props();
</script>

<a class="k-skip-link" href="#main-content">Skip to content</a>
<Header />
<main id="main-content" tabindex="-1">
	{@render children()}
</main>
<BottomNav />

<style>
	main {
		min-height: calc(100vh - var(--k-header-h));
		min-height: calc(100dvh - var(--k-header-h));
	}
	/* Clear the fixed bottom tab bar, where there is one (`--k-tabbar-h`, app.css), so
	   page content / footer isn't hidden behind it. */
	main {
		padding-bottom: var(--k-tabbar-h);
	}
</style>
