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
	}
	/* Clear the fixed mobile bottom-nav (56px + home-indicator safe area) so page
	   content / footer isn't hidden behind it. */
	@media (max-width: 640px) {
		main {
			padding-bottom: calc(56px + env(safe-area-inset-bottom));
		}
	}
</style>
