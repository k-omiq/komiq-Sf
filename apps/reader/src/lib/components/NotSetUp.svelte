<script lang="ts">
	import { apiOrigin } from '$lib/config';
	import AccountScreen from './AccountScreen.svelte';

	// The server answered that it has no owner yet. The admin console that runs setup
	// is served by the same server under /admin (same origin in the embedded build;
	// the API's origin when the reader is hosted elsewhere).
	const setupHref = `${apiOrigin}/admin/setup`;
</script>

<svelte:head><title>Not set up yet · komiq</title></svelte:head>

<AccountScreen
	title="This server is not set up yet"
	lead="Its owner finishes setup in the admin console. Reading opens here once that is done."
>
	<a class="submit" href={setupHref}>Open setup</a>
	{#snippet foot()}
		<button class="link" type="button" onclick={() => location.reload()}>Check again</button>
	{/snippet}
</AccountScreen>
