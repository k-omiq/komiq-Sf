<script lang="ts">
	import { confirmState, settleConfirm } from '$lib/confirm.svelte';
	import Button from './Button.svelte';
	import Dialog from './Dialog.svelte';
	import DontAskAgain from './DontAskAgain.svelte';

	let dontAskAgain = $state(false);
	// Each new confirm starts unticked.
	$effect.pre(() => {
		void confirmState.current;
		dontAskAgain = false;
	});
</script>

{#if confirmState.current}
	{@const c = confirmState.current}
	<Dialog title={c.title} size="sm" onclose={() => settleConfirm(false)}>
		<p class="message">{c.message}</p>
		{#snippet footer()}
			{#if c.skipKey}<DontAskAgain bind:checked={dontAskAgain} />{/if}
			<Button variant="ghost" onclick={() => settleConfirm(false)}
				>{c.cancelLabel ?? 'Cancel'}</Button
			>
			<Button variant={c.danger ? 'danger-solid' : 'primary'} onclick={() => settleConfirm(true, dontAskAgain)}
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
