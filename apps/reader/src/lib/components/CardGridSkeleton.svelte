<script lang="ts">
	// Responsive grid of placeholder cards, shown while a catalog/library resolves.
	let { count = 12, min = 158 }: { count?: number; min?: number } = $props();
</script>

<div class="grid" style="--min:{min}px" aria-hidden="true">
	{#each Array(count) as _, i (i)}
		<div class="card">
			<div class="k-skeleton cover"></div>
			<div class="k-skeleton line title"></div>
			<div class="k-skeleton line sub"></div>
		</div>
	{/each}
</div>

<style>
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(var(--min), 1fr));
		gap: 30px 22px;
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 9px;
	}
	.cover {
		width: 100%;
		aspect-ratio: 2 / 3;
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
