<script lang="ts">
	let {
		src,
		width = 36,
		alt = '',
	}: {
		/** Cover URL, or null/undefined for the placeholder. */
		src?: string | null;
		/** Rendered width in px; height follows the 2:3 cover ratio. */
		width?: number;
		/** Leave empty when the title is printed beside the cover. */
		alt?: string;
	} = $props();

	let failed = $state(false);
	$effect(() => {
		void src;
		failed = false;
	});
</script>

<span class="cover" style:width="{width}px">
	{#if src && !failed}
		<!-- no-referrer: source hosts with hotlink protection reject a foreign referrer. -->
		<img
			{src}
			{alt}
			loading="lazy"
			decoding="async"
			referrerpolicy="no-referrer"
			onerror={() => (failed = true)}
		/>
	{/if}
</span>

<style>
	.cover {
		flex: none;
		display: block;
		aspect-ratio: 2 / 3;
		border-radius: var(--k-radius-sm);
		background: var(--k-hatch), var(--k-cover);
		border: 1px solid var(--k-border-1);
		overflow: hidden;
	}
	img {
		display: block;
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
</style>
