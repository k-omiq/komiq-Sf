<script lang="ts">
	import type { PreflightLine } from '$lib/preflight';
	import Icon, { type IconName } from './Icon.svelte';

	// What the server found on its own machine before setup runs (see `$lib/preflight`):
	// one quiet line each, under the choice it belongs to. A line the owner must act on
	// takes the warning colour; nothing here is boxed. A command is in the code font, so
	// its two hyphens read as two.
	let { lines, label = 'This server' }: { lines: PreflightLine[]; label?: string } = $props();

	const icons: Record<PreflightLine['tone'], IconName> = {
		ok: 'check',
		info: 'info',
		warning: 'alert',
	};
</script>

{#if lines.length}
	<ul class="preflight" aria-label={label}>
		{#each lines as line (line.text)}
			<li data-tone={line.tone}>
				<Icon name={icons[line.tone]} size={14} /><span
					>{line.text}{#if line.command}{' '}<code class="k-mono"
							>{#each line.command.split(' ') as word, i (i)}{i ? ' ' : ''}<span>{word}</span
								>{/each}</code
						>{/if}</span
				>
			</li>
		{/each}
	</ul>
{/if}

<style>
	.preflight {
		display: grid;
		gap: 4px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	li {
		display: grid;
		grid-template-columns: 14px minmax(0, 1fr);
		gap: 6px;
		font-size: var(--k-text-xs);
		line-height: 1.55;
		color: var(--k-text-dim);
		overflow-wrap: anywhere;
	}
	li :global(svg) {
		margin-top: 2px;
	}
	li[data-tone='warning'] {
		color: var(--k-warning);
	}
	/* Kept whole on a line of its own where it fits, broken between its words (never
	   inside a flag) where it does not, and selected with one click. */
	code {
		display: inline-block;
		color: var(--k-text-2);
		user-select: all;
	}
	code span {
		white-space: nowrap;
	}
</style>
