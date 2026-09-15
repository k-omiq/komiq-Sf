<script lang="ts">
	import Button from '$lib/components/Button.svelte';
	import HomeNetwork from '$lib/components/HomeNetwork.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { back, labels, next, stepNumber, wizard } from '../wizard.svelte';

	// B3a, desktop app running its own server: let other devices at home open the
	// reader. The switch and its help are `HomeNetwork`, which Settings › Access shows too.
	// With a domain coming on the next step, this is the way in that needs no internet.
	const lead = $derived(
		wizard.extra.access === 'internet'
			? 'Let devices on your Wi-Fi connect straight to this computer. Your domain is the next step.'
			: 'Read on your phone or tablet while komiq runs on this computer.',
	);

	let busy = $state(false);

	// Back never returns to the owner form: that account exists now.
	const previous = $derived(wizard.order[stepNumber('lan') - 2]?.id);
	const canGoBack = $derived(!!previous && previous !== 'owner');
</script>

<Onboarding step={stepNumber('lan')} steps={labels()} title="Home network" {lead}>
	<HomeNetwork
		bind:busy
		onchange={(now) => (wizard.extra = { ...wizard.extra, lan: now.enabled })}
	/>

	{#snippet footer()}
		{#if canGoBack}
			<Button variant="ghost" icon="arrow-left" onclick={back} disabled={busy}>Back</Button>
		{:else}
			<span></span>
		{/if}
		<Button variant="primary" onclick={next} disabled={busy}>Continue</Button>
	{/snippet}
</Onboarding>
