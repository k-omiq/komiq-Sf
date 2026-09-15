<script lang="ts">
	import type { AccessMode } from '@komika/api';
	import { runsLocalServer } from '$lib/config';
	import { setAccessMode } from '$lib/data';
	import Button from '$lib/components/Button.svelte';
	import Choice from '$lib/components/Choice.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { labels, next, stepNumber, wizard } from '../wizard.svelte';

	// B3: who reaches this server. "Internet" adds the Cloudflare step and "without a
	// domain" the address step; on the desktop app's own server, the home-network step
	// follows either choice. The app's own server is not offered "without a domain":
	// that is for a server on a public address (a cloud host), not a home computer.
	let busy = $state(false);
	let error = $state<string | null>(null);

	const access = $derived(wizard.extra.access as AccessMode | undefined);

	function choose(value: AccessMode): void {
		wizard.extra = { ...wizard.extra, access: value };
		error = null;
	}

	async function proceed(): Promise<void> {
		if (busy) return;
		if (!access) {
			error = 'Choose one to continue.';
			return;
		}
		// The public modes are recorded by their own step, with the domain or address.
		if (access === 'local') {
			busy = true;
			try {
				await setAccessMode('local');
			} catch {
				// An older server without the setting: the choice only shapes this wizard.
			} finally {
				busy = false;
			}
		}
		next();
	}
</script>

<Onboarding
	step={stepNumber('access')}
	steps={labels()}
	title="Who should reach this server?"
	lead="You can change this later in Settings."
>
	<div class="ob-choices" role="radiogroup" aria-label="Access">
		<Choice
			icon="monitor"
			title="Only my devices"
			detail="This computer and devices on your home network."
			selected={access === 'local'}
			onselect={() => choose('local')}
		/>
		<Choice
			icon="globe"
			title="Public on the internet"
			detail="A domain of yours, with Cloudflare in front for speed and caching."
			selected={access === 'internet'}
			onselect={() => choose('internet')}
		/>
		{#if !runsLocalServer}
			<Choice
				icon="server"
				title="Public without a domain"
				detail="Readers type this server’s address. No Cloudflare. Encrypted with a free certificate."
				selected={access === 'direct'}
				onselect={() => choose('direct')}
			/>
		{/if}
	</div>
	{#if error}<p class="k-error" role="alert">{error}</p>{/if}
	{#snippet footer()}
		<span></span>
		<Button variant="primary" {busy} onclick={proceed}>Continue</Button>
	{/snippet}
</Onboarding>
