<script lang="ts">
	import { confirmState, settleConfirm } from '$lib/confirm.svelte';
	import Button from './Button.svelte';
	import Dialog from './Dialog.svelte';
</script>

{#if confirmState.current}
	{@const c = confirmState.current}
	<Dialog title={c.title} size="sm" onclose={() => settleConfirm(false)}>
		<p class="message">{c.message}</p>
		{#snippet footer()}
			<Button variant="ghost" onclick={() => settleConfirm(false)}
				>{c.cancelLabel ?? 'Cancel'}</Button
			>
			<Button variant={c.danger ? 'danger-solid' : 'primary'} onclick={() => settleConfirm(true)}
				>{c.confirmLabel ?? 'Confirm'}</Button
			>
		{/snippet}
	</Dialog>
{/if}

<style>
	.message {
		color: var(--k-text-2);
		font-size: var(--k-text-md);
		line-height: 1.65;
		white-space: pre-line;
	}
</style>
