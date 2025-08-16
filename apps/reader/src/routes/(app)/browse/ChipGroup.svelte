<script lang="ts" generics="T extends string">
	import type { Snippet } from 'svelte';

	/**
	 * One multi-select facet (Status, Format, Demographic, …): a labelled group of toggle
	 * chips. A <fieldset>/<legend> so assistive tech announces the facet name with each
	 * chip, and `aria-pressed` toggle buttons rather than checkboxes because the visual
	 * is a chip, not a box — same semantics, honest to what is on screen.
	 */
	let {
		legend,
		options,
		selected,
		ontoggle,
		hint,
	}: {
		legend: string;
		options: readonly { value: T; label: string; prefix?: string }[];
		selected: readonly T[];
		ontoggle: (value: T) => void;
		hint?: Snippet;
	} = $props();
</script>

<fieldset class="group">
	<legend class="glabel">
		{legend}
		{#if selected.length}<span class="gcount">{selected.length}</span>{/if}
	</legend>
	<div class="chips">
		{#each options as o (o.value)}
			<button
				type="button"
				class="chip"
				aria-pressed={selected.includes(o.value)}
				onclick={() => ontoggle(o.value)}
			>
				{#if o.prefix}<span class="prefix" aria-hidden="true">{o.prefix}</span>{/if}{o.label}
			</button>
		{/each}
	</div>
	{#if hint}<p class="ghint">{@render hint()}</p>{/if}
</fieldset>

<style>
	.group {
		min-width: 0;
		margin: 0;
		padding: 0;
		border: none;
	}
	/* A floated full-width legend is the one reliable way to make a <legend> lay out as
	   an ordinary block (margins included) across engines. */
	.glabel {
		float: left;
		width: 100%;
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 0;
		margin-bottom: 10px;
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.09em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.gcount {
		min-width: 18px;
		height: 18px;
		padding: 0 5px;
		border-radius: 9px;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-size: 10.5px;
		letter-spacing: 0;
	}
	.chips {
		clear: both;
		display: flex;
		flex-wrap: wrap;
		gap: 7px;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		min-height: 32px;
		padding: 5px 12px;
		border-radius: var(--k-radius-pill);
		background: var(--k-hover-fill);
		border: 1px solid var(--k-border-3);
		color: var(--k-text-3);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		transition:
			color 0.15s,
			background-color 0.15s,
			border-color 0.15s;
	}
	.chip:hover {
		border-color: var(--k-border-strong);
	}
	.chip[aria-pressed='true'] {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.prefix {
		font-size: 15px;
		line-height: 1;
	}
	.ghint {
		margin: 10px 0 0;
		font-size: 12px;
		line-height: 1.45;
		color: var(--k-text-faint);
	}
	.ghint :global(a) {
		color: var(--k-text-2);
		text-decoration: underline;
		text-underline-offset: 2px;
	}
</style>
