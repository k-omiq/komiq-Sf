<script lang="ts">
	import { auth } from '$lib/auth.svelte';
	import { forgetServer, target } from '$lib/config';
	import { href } from '$lib/paths';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { goToStep, labels, next, stepNumber } from '../wizard.svelte';

	// The owner already exists (signed in mid-wizard): skip past that step.
	function start(): void {
		if (auth.user) goToStep('owner');
		next();
	}

	function changeServer(): void {
		forgetServer();
		location.assign(href('/start'));
	}
</script>

{#snippet aside()}
	{#if target === 'desktop'}
		<button class="ob-link" type="button" onclick={changeServer}>Use a different server</button>
	{/if}
{/snippet}

<Onboarding
	step={stepNumber('welcome')}
	steps={labels()}
	title="Set up your server"
	lead="A few steps and your library is ready."
	{aside}
>
	<ul class="ob-points">
		<li><Icon name="shield" size={16} />Create the owner account that runs this server.</li>
		<li><Icon name="users" size={16} />Decide who else can sign in.</li>
		<li>
			<Icon name="package" size={16} />No sources are included. You add the ones you want.
		</li>
	</ul>
	{#snippet footer()}
		<span></span>
		<Button variant="primary" onclick={start}>Get started</Button>
	{/snippet}
</Onboarding>
