<script lang="ts">
	import Icon from '$lib/components/Icon.svelte';
	import type { BrowseState, FilterChip } from '$lib/data/browse-filters';

	/**
	 * The applied filters as one-tap-removable chips, plus "Clear all". Each chip removes
	 * exactly its own clause — the descriptor's `remove()` returns the next state and the
	 * page navigates to it (a history entry, so Back restores the chip).
	 *
	 * Excluded tags read as a negation: a minus and a strike-through, never colour alone.
	 */
	let {
		chips,
		onapply,
		onclear,
	}: {
		chips: FilterChip[];
		onapply: (next: BrowseState) => void;
		onclear: () => void;
	} = $props();
</script>

{#if chips.length}
	<ul class="chips" aria-label="Active filters">
		{#each chips as c (c.key)}
			<li>
				<button
					type="button"
					class="fchip {c.tone}"
					aria-label={c.ariaLabel}
					onclick={() => onapply(c.remove())}
				>
					{#if c.tone === 'exclude'}<span class="neg" aria-hidden="true">−</span>{/if}
					<span class="label">{c.label}</span>
					<Icon name="x" size={13} strokeWidth={2.4} />
				</button>
			</li>
		{/each}
		<li>
			<button type="button" class="clear" onclick={onclear}>Clear all</button>
		</li>
	</ul>
{/if}

<style>
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		margin: 0;
		padding: 0;
		list-style: none;
		min-width: 0;
	}
	.chips li {
		min-width: 0;
		max-width: 100%;
	}
	.fchip {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		max-width: 100%;
		min-height: 32px;
		padding: 5px 9px 5px 12px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-1);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		transition: border-color 0.15s;
	}
	.fchip:hover {
		border-color: var(--k-border-strong);
	}
	.fchip :global(svg) {
		flex: 0 0 auto;
		color: var(--k-text-dim);
	}
	.label {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.fchip.include {
		border-color: color-mix(in srgb, var(--k-ongoing) 55%, transparent);
	}
	.fchip.exclude {
		border-color: color-mix(in srgb, var(--k-cancelled) 55%, transparent);
	}
	.fchip.exclude .label {
		text-decoration: line-through;
		text-decoration-thickness: 1.5px;
	}
	.neg {
		font-weight: 800;
		color: var(--k-cancelled);
	}
	.clear {
		min-height: 32px;
		padding: 5px 10px;
		border: none;
		background: transparent;
		color: var(--k-text-dim);
		font-size: 12.5px;
		font-weight: 700;
		text-decoration: underline;
		text-underline-offset: 3px;
		cursor: pointer;
	}
	.clear:hover {
		color: var(--k-text);
	}
</style>
