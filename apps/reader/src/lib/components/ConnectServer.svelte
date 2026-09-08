<script lang="ts">
	import { chosenServer, needsServer } from '$lib/config';
	import {
		checkServer,
		connectScreen,
		toServerOrigin,
		useServer,
	} from '$lib/server-connect.svelte';
	import AccountScreen from './AccountScreen.svelte';

	// Native app only: point the reader at a komiq server. The address is checked
	// (`/health`, then the server's own facts) before it is saved on this device; the
	// app then restarts on the sign-in page of that server, or on its "not set up" page.
	//
	// No QR scanner: the WebViews the app runs in have no barcode API (only Android's
	// Chrome-based one might), and a native scanner plugin needs camera permissions in
	// the mobile projects. A link copied from the phone's own camera app pastes here.
	let address = $state(chosenServer ?? '');
	let busy = $state(false);
	let error = $state<string | null>(null);

	// First run has nothing to go back to; "Change server" from sign-in does.
	const canCancel = !needsServer;

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (busy) return;
		error = null;
		const origin = toServerOrigin(address);
		if (!origin) {
			error = 'Enter an address like 192.168.1.20:8080 or komiq.example.com.';
			return;
		}
		busy = true;
		try {
			await checkServer(origin);
			useServer(origin);
		} catch (err) {
			error = err instanceof Error ? err.message : 'The server could not be checked.';
			busy = false;
		}
	}
</script>

<svelte:head><title>Connect to a server · komiq</title></svelte:head>

<AccountScreen
	title="Connect to a server"
	lead="Enter the address of the komiq server you read on. You sign in next."
>
	<form onsubmit={submit} novalidate aria-busy={busy}>
		<label>
			<span>Server address</span>
			<input
				type="text"
				name="server"
				inputmode="url"
				autocapitalize="none"
				autocomplete="url"
				spellcheck="false"
				placeholder="192.168.1.20:8080"
				bind:value={address}
				oninput={() => (error = null)}
				aria-invalid={error ? true : undefined}
				aria-describedby="server-msg"
			/>
			{#if error}
				<span id="server-msg" class="field-error" role="alert">{error}</span>
			{:else}
				<small id="server-msg" class="hint"
					>The address you open komiq at in a browser. At home it is usually an IP address and a
					port.</small
				>
			{/if}
		</label>
		<button class="submit" type="submit" disabled={busy}>
			{busy ? 'Checking…' : 'Connect'}
		</button>
	</form>
	{#snippet foot()}
		{#if canCancel}
			<button class="link" type="button" onclick={() => (connectScreen.open = false)}>Cancel</button
			>
		{/if}
	{/snippet}
</AccountScreen>

<style>
	.hint {
		color: var(--k-text-dim);
		font-size: 12.5px;
		line-height: 1.45;
	}
</style>
