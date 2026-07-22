<script lang="ts">
	import { dismissToast, toasts } from '$lib/toast.svelte';
	import Icon from './Icon.svelte';
</script>

<!-- `aria-live` rather than role="status": pages own their own status/alert
     regions and this one must not be mistaken for them. -->
<!-- `data-modal-backdrop` exempts this region from the `inert` an open dialog
     puts on everything else, so a toast fired from inside a dialog is still
     announced and can be dismissed. -->
<div class="toaster" aria-live="polite" aria-relevant="additions" data-modal-backdrop>
	{#each toasts.items as t (t.id)}
		<div class="toast" data-tone={t.tone}>
			<Icon name={t.tone === 'success' ? 'check' : t.tone === 'danger' ? 'alert' : 'info'} />
			<p>{t.message}</p>
			<button type="button" aria-label="Dismiss" onclick={() => dismissToast(t.id)}>
				<Icon name="x" size={14} />
			</button>
		</div>
	{/each}
</div>

<style>
	.toaster {
		position: fixed;
		right: var(--k-space-4);
		bottom: max(var(--k-space-4), env(safe-area-inset-bottom));
		z-index: 400;
		display: grid;
		gap: var(--k-space-2);
		width: min(calc(100vw - 32px), 380px);
		pointer-events: none;
	}
	.toast {
		display: flex;
		align-items: flex-start;
		gap: var(--k-space-3);
		padding: 12px 12px 12px 14px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-md);
		background: var(--k-surface-5);
		box-shadow: var(--k-shadow-2);
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		line-height: 1.5;
		pointer-events: auto;
		animation: rise var(--k-dur) var(--k-ease);
	}
	.toast > :global(svg) {
		margin-top: 2px;
	}
	.toast[data-tone='success'] > :global(svg) {
		color: var(--k-success);
	}
	.toast[data-tone='danger'] > :global(svg) {
		color: var(--k-danger);
	}
	.toast[data-tone='info'] > :global(svg) {
		color: var(--k-info);
	}
	p {
		flex: 1 1 auto;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	button {
		flex: none;
		display: grid;
		place-items: center;
		width: 24px;
		height: 24px;
		border: 0;
		border-radius: 6px;
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
	}
	button:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
	}
	@keyframes rise {
		from {
			opacity: 0;
			transform: translateY(8px);
		}
	}
</style>
