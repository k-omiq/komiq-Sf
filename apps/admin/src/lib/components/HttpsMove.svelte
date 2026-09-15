<script lang="ts">
	import { base } from '$app/paths';
	import { target } from '$lib/config';
	import Icon from './Icon.svelte';

	// "Public without a domain" with HTTPS: the server now answers at `address`, but a
	// console that was opened at the plain HTTP address is still on it. The plain
	// address stops answering clients on the internet after the first visit over HTTPS,
	// so the owner is sent to the new one. The desktop app follows by itself
	// (`followServer`); this is for the console the server serves.

	let { address }: { address: string } = $props();

	const moved = $derived(
		target === 'embedded' &&
			address.startsWith('https://') &&
			typeof location !== 'undefined' &&
			location.protocol === 'http:',
	);
	const consoleAddress = $derived(`${address}${base}`);
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
{/if}
