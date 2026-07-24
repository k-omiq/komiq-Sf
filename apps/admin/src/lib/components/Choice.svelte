<script lang="ts">
	import Icon, { type IconName } from './Icon.svelte';

	// One option in a first-run choice list: a flat row with an icon, a title and a
	// line of detail. Selected rows lift to a slightly brighter surface and show a
	// check; there is no outline, so the list reads as one quiet block.
	let {
		icon,
		title,
		detail,
		selected = false,
		disabled = false,
		note,
		onselect,
		role = 'radio',
	}: {
		icon: IconName;
		title: string;
		detail: string;
		selected?: boolean;
		disabled?: boolean;
		/** Shown instead of the check when disabled (e.g. "Desktop app only"). */
		note?: string;
		onselect: () => void;
		/** `radio` inside a radiogroup; `button` for a plain next-screen choice. */
		role?: 'radio' | 'button';
	} = $props();
</script>

<button
	type="button"
	class="choice"
	class:selected
	{role}
	aria-checked={role === 'radio' ? selected : undefined}
	{disabled}
	onclick={onselect}
>
	<span class="icon"><Icon name={icon} size={18} /></span>
	<span class="text">
		<span class="title">{title}</span>
		<span class="detail">{detail}</span>
	</span>
	{#if disabled && note}
		<span class="note">{note}</span>
	{:else if role === 'radio'}
		<span class="mark" aria-hidden="true"
			>{#if selected}<Icon name="check" size={16} />{/if}</span
		>
	{:else}
		<span class="mark" aria-hidden="true"><Icon name="chevron-right" size={16} /></span>
	{/if}
</button>

<style>
	.choice {
		display: grid;
		grid-template-columns: auto 1fr auto;
		align-items: center;
		gap: var(--k-space-3);
		width: 100%;
		padding: 14px 16px;
		border: 0;
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-2);
		color: var(--k-text-1);
		text-align: left;
		cursor: pointer;
		transition: background var(--k-dur) var(--k-ease);
	}
	.choice:hover:not(:disabled) {
		background: var(--k-surface-4);
	}
	.choice.selected {
		background: var(--k-surface-5);
	}
	.choice:disabled {
		cursor: not-allowed;
		opacity: 0.6;
	}
	.icon {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border-radius: var(--k-radius);
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
	.choice.selected .icon {
		background: var(--k-surface);
		color: var(--k-text-bright);
	}
	.text {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.title {
		font-size: var(--k-text-md);
		font-weight: 600;
		color: var(--k-text-bright);
	}
	.detail {
		font-size: var(--k-text-sm);
		line-height: 1.5;
		color: var(--k-text-dim);
	}
	.mark {
		display: grid;
		place-items: center;
		width: 20px;
		color: var(--k-text-2);
	}
	.note {
		font-size: var(--k-text-xs);
		color: var(--k-text-dim);
		white-space: nowrap;
	}
</style>
