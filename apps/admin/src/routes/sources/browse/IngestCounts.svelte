<script lang="ts" module>
	export interface CountItem {
		label: string;
		value: number;
		/** Colour the number, e.g. a non-zero failure count. */
		tone?: 'danger';
	}
</script>

<script lang="ts">
	let { items }: { items: CountItem[] } = $props();
</script>

<!-- A compact definition grid: every count stays in place while a job updates,
     so the numbers do not jump around between polls. -->
<dl class="counts">
	{#each items as item (item.label)}
		<div class="count">
			<dt>{item.label}</dt>
			<dd class="k-num" data-tone={item.tone}>{item.value.toLocaleString()}</dd>
		</div>
	{/each}
</dl>

<style>
	.counts {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(116px, 1fr));
		gap: var(--k-space-3) var(--k-space-5);
		margin: 0;
		min-width: 0;
	}
	.count {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	dt {
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		font-weight: 600;
	}
	dd {
		margin: 0;
		color: var(--k-text-bright);
		font-size: var(--k-text-lg);
		font-weight: 700;
	}
	dd[data-tone='danger'] {
		color: var(--k-danger);
	}
</style>
