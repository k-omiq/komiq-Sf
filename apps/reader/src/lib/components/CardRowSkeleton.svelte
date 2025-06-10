<script lang="ts">
	// Horizontal scrolling row of placeholder cards, shown while a feed resolves.
	let { count = 6, width = 150 }: { count?: number; width?: number } = $props();
	const coverH = $derived(Math.round((width * 212) / 150));
</script>

<div class="row" aria-hidden="true">
	{#each Array(count) as _, i (i)}
		<div class="card" style="width:{width}px">
			<div class="k-skeleton cover" style="height:{coverH}px"></div>
			<div class="k-skeleton line title"></div>
			<div class="k-skeleton line sub"></div>
		</div>
	{/each}
</div>

<style>
	.row {
		display: flex;
		gap: 22px;
		overflow: hidden;
		padding-bottom: 4px;
	}
	.card {
		flex: 0 0 auto;
		display: flex;
		flex-direction: column;
		gap: 9px;
	}
	.cover {
		width: 100%;
		border-radius: 8px;
	}
	.line {
		height: 11px;
		border-radius: 4px;
	}
	.line.title {
		width: 85%;
	}
	.line.sub {
		width: 55%;
		height: 9px;
	}
</style>
