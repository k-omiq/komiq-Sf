<script lang="ts">
	import { AI_LABEL, AI_TITLE, copyAi } from '$lib/data/scanlator-ai';
	import { loadScanlatorAi, scanlatorAi } from '$lib/scanlator-ai.svelte';

	// One chapter copy: its scanlation group and the source it is read from. Renders
	// nothing unless an admin tagged one of the two.
	let {
		group,
		sourceId,
	}: { group: string | null | undefined; sourceId: string | null | undefined } = $props();

	void loadScanlatorAi();
	const mark = $derived(copyAi(group, sourceId, scanlatorAi.index));
</script>

{#if mark}<span class="ai" title={AI_TITLE[mark.by][mark.level]}>{AI_LABEL[mark.level]}</span>{/if}

<style>
	.ai {
		flex: 0 0 auto;
		font-size: 9.5px;
		font-weight: 800;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		white-space: nowrap;
		padding: 2px 6px;
		border-radius: 4px;
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
</style>
