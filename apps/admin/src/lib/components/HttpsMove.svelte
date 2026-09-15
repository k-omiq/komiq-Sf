<script lang="ts">
	import { base } from '$app/paths';
	import { serverAddress, target } from '$lib/config';
	import { move, moveTo } from '$lib/server-move.svelte';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';

	// "Public without a domain" with HTTPS: the server now answers at `address`, but a
	// console that was opened at the plain HTTP address is still on it. The plain
	// address stops answering clients on the internet after the first visit over HTTPS,
	// so the owner is sent to the new one. The console the server serves cannot move by
	// itself: it shows a link. The desktop app moves by itself (`moveTo`); when the new
	// address did not answer, it says so here and offers another try.

	let { address }: { address: string } = $props();

	const moved = $derived(
		target === 'embedded' &&
			address.startsWith('https://') &&
			typeof location !== 'undefined' &&
			location.protocol === 'http:',
	);
	const consoleAddress = $derived(`${address}${base}`);
	// The desktop app's move that got no answer, while the server still names that
	// address. Another try uses what the first one had, never `address`: that one came
	// over plain HTTP again.
	const stuck = $derived(move.stuck?.address === address ? move.stuck : null);
	// The usual case: HTTPS was just turned on, and the app is on the plain address. Not
	// so when the owner changed an HTTPS address to another one.
	const plain = serverAddress().startsWith('http://');
</script>

{#if moved}
	<div class="k-notice" data-tone="warning" role="note">
		<Icon name="shield" />
		<div class="k-notice__body">
			This page still uses the old address, without encryption. Open the console at
			<a class="k-link" href={consoleAddress}>{consoleAddress}</a> and sign in there. After the first
			visit over HTTPS, the old address stops answering from the internet.
		</div>
	</div>
{:else if stuck}
	<div class="k-notice" data-tone="warning" role="alert">
		<Icon name="shield" />
		<div class="k-notice__body">
			This app still uses the old address{plain ? ', without encryption' : ''}. It got no answer at
			{stuck.address}. Check that port {new URL(stuck.address).port || '443'} is open in your cloud firewall,
			then try again.
			{plain
				? 'After the first visit over HTTPS, the old address stops answering from the internet.'
				: ''}
		</div>
		<Button size="sm" busy={move.trying} onclick={() => moveTo(stuck.address, stuck.typed)}>
			Try again
		</Button>
	</div>
{/if}
