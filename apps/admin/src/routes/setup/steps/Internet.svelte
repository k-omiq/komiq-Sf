<script lang="ts">
	import type { CloudflareConnection } from '@komika/api';
	import { setAccessMode } from '$lib/data';
	import Button from '$lib/components/Button.svelte';
	import CloudflareSetup, { type Phase } from '$lib/components/CloudflareSetup.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { back, labels, next, stepNumber, wizard } from '../wizard.svelte';

	// B3b: put the server on a domain behind Cloudflare. The domain and connection type
	// are saved with the wizard's progress; the API token never is.
	let setup = $state<ReturnType<typeof CloudflareSetup>>();
	let phase = $state<Phase>('form');
	let busy = $state(false);
	let saving = $state(false);
	let error = $state<string | null>(null);
	let domain = $state((wizard.extra.domain as string | undefined) ?? '');
	let connection = $state<CloudflareConnection>(
		(wizard.extra.connection as CloudflareConnection | undefined) ?? 'TUNNEL',
	);

	$effect(() => {
		const d = domain.trim();
		if (d !== wizard.extra.domain || connection !== wizard.extra.connection) {
			wizard.extra = { ...wizard.extra, domain: d, connection };
		}
	});

	/** Done by hand: remember the domain so Settings and the summary show it. */
	async function finishManual(): Promise<void> {
		if (saving) return;
		if (!domain.trim().includes('.')) {
			error = 'Enter the domain readers will use.';
			return;
		}
		saving = true;
		error = null;
		try {
			await setAccessMode('internet', domain.trim());
			next();
		} catch (err) {
			error = err instanceof Error ? err.message : 'The domain could not be saved.';
		} finally {
			saving = false;
		}
	}

	const title = $derived(
		phase === 'manual'
			? 'Set up Cloudflare by hand'
			: phase === 'done'
				? 'Your server is online'
				: 'Put your server online',
	);
	const lead = $derived(
		phase === 'manual'
			? 'Do these steps in the Cloudflare dashboard, then continue.'
			: 'Cloudflare sits in front of your server and caches covers and pages.',
	);
</script>

<Onboarding step={stepNumber('internet')} steps={labels()} {title} {lead}>
	<CloudflareSetup bind:this={setup} bind:domain bind:connection bind:phase bind:busy />
	{#if error}<p class="k-error" role="alert">{error}</p>{/if}
	{#snippet footer()}
		<Button variant="ghost" icon="arrow-left" onclick={back} disabled={busy}>Back</Button>
		{#if phase === 'form'}
			<!-- "Set up later" saves no access mode: the summary then says so. -->
			<span class="actions">
				<Button variant="ghost" onclick={next} disabled={busy}>Set up later</Button>
				<Button variant="primary" {busy} onclick={() => setup?.start()}>Set up</Button>
			</span>
		{:else if phase === 'running'}
			<Button variant="primary" busy>Setting up…</Button>
		{:else if phase === 'manual'}
			<Button variant="primary" busy={saving} onclick={finishManual}>Continue</Button>
		{:else if phase === 'paused'}
			<!-- Set up before, then turned off by another access choice. -->
			<span class="actions">
				<Button variant="ghost" onclick={next} disabled={busy}>Set up later</Button>
				<Button variant="primary" {busy} onclick={() => setup?.turnOn()}>Turn on</Button>
			</span>
		{:else if phase === 'failed'}
			<Button variant="ghost" onclick={next}>Skip for now</Button>
		{:else}
			<Button variant="primary" onclick={next}>Continue</Button>
		{/if}
	{/snippet}
</Onboarding>

<style>
	.actions {
		display: inline-flex;
		gap: var(--k-space-2);
	}
</style>
