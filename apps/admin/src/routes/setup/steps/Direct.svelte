<script lang="ts">
	import { followServer } from '$lib/config';
	import { setAccessMode } from '$lib/data';
	import Button from '$lib/components/Button.svelte';
	import DirectAddress from '$lib/components/DirectAddress.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { back, labels, next, stepNumber, wizard } from '../wizard.svelte';

	// Public without a domain: readers use the server's own address. No Cloudflare is
	// involved, so the server fetches everything itself, and gets its own certificate.
	// Both are saved with the wizard's progress, as the owner left them: coming back to
	// this step shows the address they typed and their choice about encryption.
	let address = $state((wizard.extra.address as string | undefined) ?? '');
	let encrypt = $state(wizard.extra.encrypt !== false);
	let error = $state<string | null>(null);
	let busy = $state(false);

	async function save(): Promise<void> {
		if (busy) return;
		busy = true;
		error = null;
		try {
			const status = await setAccessMode('direct', address.trim(), encrypt);
			// The summary reads the full origin from the server (`Done.svelte`).
			wizard.extra = { ...wizard.extra, address: address.trim(), encrypt };
			next();
			if (status.address) await followServer(status.address, address);
		} catch (err) {
			error = err instanceof Error ? err.message : 'The address could not be saved.';
		} finally {
			busy = false;
		}
	}
</script>

<Onboarding
	step={stepNumber('direct')}
	steps={labels()}
	title="Public without a domain"
	lead="Readers connect straight to this server. Nothing sits in front of it."
>
	<DirectAddress bind:address bind:error bind:encrypt />
	{#if busy && encrypt}
		<p class="k-hint" role="status">Getting a certificate. This can take a minute.</p>
	{/if}
	{#snippet footer()}
		<Button variant="ghost" icon="arrow-left" onclick={back} disabled={busy}>Back</Button>
		<!-- "Set up later" saves no access mode: the summary then says so. The way on when
		     a certificate cannot be issued yet (port 80 is still closed). -->
		<span class="actions">
			<Button variant="ghost" onclick={next} disabled={busy}>Set up later</Button>
			<Button variant="primary" {busy} onclick={save}>Continue</Button>
		</span>
	{/snippet}
</Onboarding>

<style>
	.actions {
		display: inline-flex;
		gap: var(--k-space-2);
	}
</style>
