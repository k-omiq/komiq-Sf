<script lang="ts">
	import Button from './Button.svelte';

	let {
		page,
		hasNext,
		busy = false,
		detail,
		onpage,
	}: {
		page: number;
		hasNext: boolean;
		/** A page load is in flight: both directions are disabled. */
		busy?: boolean;
		/** Extra context after the page number, e.g. "of 12, showing 51 to 100". */
		detail?: string;
		onpage: (page: number) => void;
	} = $props();
</script>

<nav class="pager" aria-label="Pagination">
	<Button
		size="sm"
		icon="chevron-left"
		disabled={page <= 1 || busy}
		onclick={() => onpage(page - 1)}>Prev</Button
	>
	<span class="where k-num">Page {page}{detail ? ` ${detail}` : ''}</span>
	<Button size="sm" disabled={!hasNext || busy} onclick={() => onpage(page + 1)}>Next</Button>
</nav>

<style>
	.pager {
		display: flex;
		align-items: center;
		justify-content: center;
		flex-wrap: wrap;
		gap: var(--k-space-3);
	}
	.where {
		color: var(--k-text-muted);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
</style>
