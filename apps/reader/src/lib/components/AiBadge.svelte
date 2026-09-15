<script lang="ts">
	import { AI_LABEL, AI_TITLE, aiLevel } from '$lib/data/scanlator-ai';
	import { loadScanlatorAi, scanlatorAi } from '$lib/scanlator-ai.svelte';

	// The scanlation group of one chapter copy. Renders nothing unless an admin tagged it.
	let { group }: { group: string | null | undefined } = $props();

	void loadScanlatorAi();
	const level = $derived(aiLevel(group, scanlatorAi.index));
</script>

{#if level}<span class="ai" title={AI_TITLE[level]}>{AI_LABEL[level]}</span>{/if}

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
