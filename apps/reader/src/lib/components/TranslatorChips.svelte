<script lang="ts">
	// Small extension-logo chips marking which sources ("translators") carry a
	// work. Each chip shows the store-hosted logo (falling back to a coloured
	// initial when it's missing or fails to load) with the source name + language
	// as its accessible label / tooltip.
	interface Chip {
		name: string;
		lang: string | null;
		iconUrl: string | null;
	}

	let {
		chips,
		max = 4,
		size = 22,
		labelled = false,
	}: { chips: Chip[]; max?: number; size?: number; labelled?: boolean } = $props();

	const shown = $derived(chips.slice(0, max));
	const overflow = $derived(Math.max(0, chips.length - max));

	// Track which icons failed (→ initial fallback) by a STABLE chip id, not array
	// index: when this instance is reused for a different chip set (new search
	// results), an index-keyed flag would wrongly hide a valid icon now at that
	// index. name+lang identifies a source (and its icon URL) stably.
	function chipId(c: Chip): string {
		return `${c.name}|${c.lang ?? ''}`;
	}
	let broken = $state<Record<string, boolean>>({});
	function fail(id: string) {
		broken = { ...broken, [id]: true };
	}
	function label(c: Chip): string {
		return c.lang ? `${c.name} · ${c.lang.toUpperCase()}` : c.name;
	}
</script>

<div class="chips" class:labelled>
	{#each shown as c (chipId(c))}
		<span class="chip" title={label(c)} style="--sz:{size}px">
			{#if c.iconUrl && !broken[chipId(c)]}
				<img
					class="logo"
					src={c.iconUrl}
					alt={label(c)}
					loading="lazy"
					decoding="async"
					referrerpolicy="no-referrer"
					onerror={() => fail(chipId(c))}
				/>
			{:else}
				<span class="initial" aria-label={label(c)}>{c.name.charAt(0).toUpperCase()}</span>
			{/if}
			{#if labelled}<span class="name">{label(c)}</span>{/if}
		</span>
	{/each}
	{#if overflow > 0}
		<span class="chip more" title={`${overflow} more source${overflow === 1 ? '' : 's'}`}
			>+{overflow}</span
		>
	{/if}
</div>

<style>
	.chips {
		display: flex;
		align-items: center;
		gap: 5px;
		flex-wrap: wrap;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		flex: 0 0 auto;
	}
	.logo,
	.initial {
		width: var(--sz);
		height: var(--sz);
		border-radius: 6px;
		flex: 0 0 auto;
		object-fit: cover;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-2);
	}
	.initial {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		font-size: calc(var(--sz) * 0.5);
		font-weight: 800;
		color: var(--k-text-2);
	}
	.labelled .chip {
		padding: 3px 10px 3px 3px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-pill);
		background: var(--k-hover-fill);
	}
	.name {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-2);
		white-space: nowrap;
	}
	.more {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		height: var(--sz, 22px);
		min-width: var(--sz, 22px);
		padding: 0 6px;
		border-radius: 6px;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-2);
		font-size: 11px;
		font-weight: 800;
		color: var(--k-text-3);
	}
</style>
