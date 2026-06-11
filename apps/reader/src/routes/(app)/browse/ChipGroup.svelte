<script lang="ts" generics="T extends string">
	import type { Snippet } from 'svelte';
	import Icon from '$lib/components/Icon.svelte';

	/** Native disclosures and checkboxes keep facets compact and keyboard accessible. */
	let {
		legend,
		options,
		selected,
		ontoggle,
		hint,
		initialOpen = false,
		segmented = false,
	}: {
		legend: string;
		options: readonly { value: T; label: string }[];
		selected: readonly T[];
		ontoggle: (value: T) => void;
		hint?: Snippet;
		initialOpen?: boolean;
		segmented?: boolean;
	} = $props();

	let expanded = $state(false);
	$effect(() => {
		if (initialOpen || selected.length) expanded = true;
	});
</script>

<details class="group" bind:open={expanded}>
	<summary>
		<span>{legend}</span>
		{#if selected.length}<span class="count">{selected.length} selected</span>{/if}
		<Icon name="chevron-down" size={14} />
	</summary>
	<fieldset>
		<legend class="sr-only">{legend}</legend>
		<div class="options" class:segmented>
			{#each options as option (option.value)}
				<label class="option">
					<input
						type="checkbox"
						checked={selected.includes(option.value)}
						onchange={() => ontoggle(option.value)}
					/>
					<span>{option.label}</span>
				</label>
			{/each}
		</div>
		{#if hint}<p class="hint">{@render hint()}</p>{/if}
	</fieldset>
</details>

<style>
	.group {
		min-width: 0;
		border-top: 1px solid var(--k-border-1);
	}
	summary {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 44px;
		list-style: none;
		color: var(--k-text-1);
		font-size: 13px;
		font-weight: 650;
		cursor: pointer;
	}
	summary::-webkit-details-marker {
		display: none;
	}
	summary :global(svg) {
		margin-left: auto;
		flex-shrink: 0;
		color: var(--k-text-dim);
	}
	.group[open] summary :global(svg) {
		transform: rotate(180deg);
	}
	summary:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
		border-radius: var(--k-radius-sm);
	}
	.count {
		margin-left: auto;
		color: var(--k-text-dim);
		font-size: 11px;
		font-weight: 500;
	}
	.count + :global(svg) {
		margin-left: 0;
	}
	fieldset {
		min-width: 0;
		margin: 0;
		padding: 0 0 14px;
		border: none;
	}
	.options {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 2px 12px;
	}
	.option {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 28px;
		min-width: 0;
		font-size: 12.5px;
		color: var(--k-text-3);
		cursor: pointer;
	}
	.option:hover {
		color: var(--k-text);
	}
	.option input {
		width: 15px;
		height: 15px;
		margin: 0;
		flex-shrink: 0;
		accent-color: var(--k-primary);
		cursor: pointer;
	}
	.segmented {
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 6px;
	}
	.segmented .option {
		position: relative;
		justify-content: center;
		min-height: 36px;
		padding: 5px 4px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius);
		font-size: 12px;
	}
	.segmented .option:has(input:checked) {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.segmented .option:has(input:focus-visible) {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.segmented input,
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
	.hint {
		margin: 12px 0 0;
		font-size: 11.5px;
		line-height: 1.55;
		color: var(--k-text-dim);
	}
	.hint :global(a) {
		color: var(--k-text-2);
		text-decoration: underline;
		text-underline-offset: 2px;
	}
	@media (max-width: 959px) {
		summary {
			min-height: 48px;
		}
		.option {
			min-height: 40px;
		}
		.segmented .option {
			min-height: 40px;
		}
	}
</style>
