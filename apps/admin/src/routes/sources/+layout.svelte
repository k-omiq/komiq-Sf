<script lang="ts">
	import { href } from '$lib/paths';
	import { auth } from '$lib/auth.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';

	let { children } = $props();
</script>

<!-- With the account's NSFW preference off the server refuses source browsing,
     extension install and source ingest. Say so here, where those actions live,
     instead of letting each one fail on its own. -->
{#if auth.user && !auth.user.showNsfw}
	<div class="k-notice gate" data-tone="warning">
		<Icon name="eye-off" />
		<div class="k-notice__body">
			Source tools are blocked for your account while NSFW content is off. Browsing a source,
			installing an extension and adding series will fail until you turn it on.
		</div>
		<Button size="sm" href={href('/settings#content')}>Open Settings</Button>
	</div>
{/if}

{@render children()}

<style>
	.gate {
		align-items: center;
		margin-bottom: var(--k-space-5);
	}
</style>
