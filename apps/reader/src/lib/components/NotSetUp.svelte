<script lang="ts">
	import { apiOrigin, canChooseServer } from '$lib/config';
	import { connectScreen } from '$lib/server-connect.svelte';
	import AccountScreen from './AccountScreen.svelte';

	// The server answered that it has no owner yet. The admin console that runs setup
	// is served by the same server under /admin (same origin in the embedded build;
	// the API's origin when the reader is hosted elsewhere).
	const setupHref = `${apiOrigin}/admin/setup`;
	// The native app has no browser around it: following the link would replace the
	// app with the console. It shows the address to open in a browser instead.
	const setupAddress = setupHref.replace(/^https?:\/\//, '');
</script>

<svelte:head><title>Not set up yet · komiq</title></svelte:head>

{#if canChooseServer}
	<AccountScreen
		title="This server is not set up yet"
		lead="Its owner finishes setup in a browser. Reading opens here once that is done."
	>
		<p class="address">
			<span>Open in a browser</span>
			<code>{setupAddress}</code>
		</p>
		<button class="submit" type="button" onclick={() => location.reload()}>Check again</button>
		{#snippet foot()}
			<button class="link" type="button" onclick={() => (connectScreen.open = true)}
				>Change server</button
			>
		{/snippet}
	</AccountScreen>
{:else}
	<AccountScreen
		title="This server is not set up yet"
		lead="Its owner finishes setup in the admin console. Reading opens here once that is done."
	>
		<a class="submit" href={setupHref}>Open setup</a>
		{#snippet foot()}
			<button class="link" type="button" onclick={() => location.reload()}>Check again</button>
		{/snippet}
	</AccountScreen>
{/if}

<style>
	.address {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-2);
		margin-bottom: var(--k-space-4);
	}
	.address span {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-dim);
		letter-spacing: 0.01em;
	}
	.address code {
		padding: 11px var(--k-space-4);
		background: var(--k-surface);
		border-radius: var(--k-radius-md);
		color: var(--k-text);
		font-family: var(--k-font-mono);
		font-size: 13px;
		overflow-wrap: anywhere;
		user-select: all;
	}
</style>
