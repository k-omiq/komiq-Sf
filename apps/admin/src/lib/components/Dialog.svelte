<script lang="ts">
	import type { Snippet } from 'svelte';
	import { modal } from '$lib/modal';
	import Button from './Button.svelte';

	let {
		title,
		description,
		variant = 'modal',
		size = 'md',
		onclose,
		children,
		footer,
	}: {
		title: string;
		description?: string;
		/** `modal` is centred; `drawer` slides in from the right for longer forms. */
		variant?: 'modal' | 'drawer';
		size?: 'sm' | 'md' | 'lg';
		/** Called on Escape, scrim click and the close button. */
		onclose: () => void;
		children: Snippet;
		footer?: Snippet;
	} = $props();

	const uid = $props.id();
</script>

<div class="scrim" data-modal-backdrop onclick={onclose} aria-hidden="true"></div>
<div
	class="dialog {variant} {size}"
	role="dialog"
	aria-modal="true"
	aria-labelledby="{uid}-title"
	aria-describedby={description ? `${uid}-desc` : undefined}
	use:modal={onclose}
>
	<header class="top">
		<div class="titles">
			<h2 id="{uid}-title">{title}</h2>
			{#if description}<p id="{uid}-desc">{description}</p>{/if}
		</div>
		<Button variant="ghost" size="sm" icon="x" iconOnly aria-label="Close" onclick={onclose} />
	</header>
	<div class="body">{@render children()}</div>
	{#if footer}
		<footer class="foot">{@render footer()}</footer>
	{/if}
</div>

<style>
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 200;
		background: var(--k-scrim);
		animation: fade var(--k-dur) var(--k-ease);
	}
	.dialog {
		position: fixed;
		z-index: 201;
		display: flex;
		flex-direction: column;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-2);
		box-shadow: var(--k-shadow-3);
		overscroll-behavior: contain;
	}
	.dialog:focus {
		outline: none;
	}
	.modal {
		top: 50%;
		left: 50%;
		width: min(calc(100vw - 32px), var(--w));
		max-height: calc(100dvh - 32px);
		border-radius: var(--k-radius-xl);
		transform: translate(-50%, -50%);
		animation: pop var(--k-dur) var(--k-ease);
	}
	.drawer {
		top: 0;
		right: 0;
		bottom: 0;
		width: min(100vw, var(--w));
		border-width: 0 0 0 1px;
		animation: slide var(--k-dur) var(--k-ease);
	}
	.sm {
		--w: 420px;
	}
	.md {
		--w: 560px;
	}
	.lg {
		--w: 820px;
	}
	.top {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: var(--k-space-4);
		padding: var(--k-space-5) var(--k-space-5) var(--k-space-3);
	}
	.titles {
		min-width: 0;
	}
	h2 {
		font-size: var(--k-text-xl);
		font-weight: 700;
		overflow-wrap: anywhere;
	}
	.titles p {
		margin-top: 6px;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		line-height: 1.6;
	}
	.body {
		flex: 1 1 auto;
		min-height: 0;
		overflow-y: auto;
		padding: var(--k-space-2) var(--k-space-5) var(--k-space-5);
	}
	.foot {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		flex-wrap: wrap;
		gap: var(--k-space-2);
		padding: var(--k-space-4) var(--k-space-5);
		padding-bottom: max(var(--k-space-4), env(safe-area-inset-bottom));
		border-top: 1px solid var(--k-border);
	}
	@keyframes fade {
		from {
			opacity: 0;
		}
	}
	@keyframes pop {
		from {
			opacity: 0;
			transform: translate(-50%, -48%) scale(0.98);
		}
	}
	@keyframes slide {
		from {
			transform: translateX(24px);
			opacity: 0;
		}
	}
</style>
