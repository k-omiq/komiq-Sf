<script lang="ts">
	let {
		checked,
		label,
		disabled = false,
		onchange,
	}: {
		checked: boolean;
		/** Accessible name; the visible label lives beside the switch. */
		label: string;
		disabled?: boolean;
		onchange: (checked: boolean) => void;
	} = $props();
</script>

<button
	type="button"
	class="switch"
	role="switch"
	aria-checked={checked}
	aria-label={label}
	{disabled}
	onclick={() => onchange(!checked)}
>
	<span class="thumb"></span>
</button>

<style>
	.switch {
		flex: none;
		position: relative;
		width: 40px;
		height: 24px;
		padding: 0;
		border: 1px solid var(--k-border-4);
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-5);
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			border-color var(--k-dur) var(--k-ease);
	}
	.thumb {
		position: absolute;
		top: 2px;
		left: 2px;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		background: var(--k-text-muted);
		transition:
			transform var(--k-dur) var(--k-ease),
			background var(--k-dur) var(--k-ease);
	}
	.switch[aria-checked='true'] {
		background: var(--k-accent);
		border-color: transparent;
	}
	.switch[aria-checked='true'] .thumb {
		background: var(--k-on-accent);
		transform: translateX(16px);
	}
	.switch:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
</style>
