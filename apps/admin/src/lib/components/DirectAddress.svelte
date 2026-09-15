<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import type { AccessPreflight } from '@komika/api';
	import { reachableFromOtherDevices, serverAddress } from '$lib/config';
	import { loadAccessPreflight, loadLanAddress } from '$lib/data';
	import {
		addressFor,
		directLines,
		isPrivateAddress,
		plainLines,
		suggestedAddress,
	} from '$lib/preflight';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';
	import Preflight from './Preflight.svelte';
	import Switch from './Switch.svelte';
	import '../onboarding.css';

	// Public without a domain: readers type the server's own address, and nothing sits
	// in front of it. Used by the setup wizard and by Settings › Access. The parent
	// saves the address (`setAccessMode('direct', address, encrypt)`); the server
	// checks and tidies it. With `encrypt` the server gets a Let's Encrypt certificate
	// for the address and serves HTTPS; without, readers use plain HTTP on its port.

	let {
		address = $bindable(''),
		error = $bindable<string | null>(null),
		encrypt = $bindable(true),
	}: {
		address?: string;
		/** A message about the address, from the parent's save or from the check here. */
		error?: string | null;
		/** Serve HTTPS with a Let's Encrypt certificate. */
		encrypt?: boolean;
	} = $props();

	let checking = $state(false);
	let reached = $state<boolean | null>(null);
	// What the server knows about its machine: the ports a certificate needs, a
	// container, its public address. Null on a server that cannot say.
	let preflight = $state<AccessPreflight | null>(null);
	const serverLines = $derived(encrypt ? directLines(preflight) : plainLines(preflight));
	const httpsPort = $derived(preflight?.httpsPort ?? 443);
	const acmePort = $derived(preflight?.acmePort ?? 80);
	// Let's Encrypt always connects to public port 80, and readers to the port in the
	// address. A server that listens elsewhere needs port 80 sent on to it.
	const portsHint = $derived(
		acmePort !== 80
			? `Forward public port 80 to port ${acmePort} of this server: Let’s Encrypt always connects to port 80. Open port ${httpsPort} for readers.`
			: httpsPort !== 443
				? `Open ports 80 and ${httpsPort} in your cloud firewall. Readers connect to port ${httpsPort}: it is part of the address.`
				: `Open ports ${acmePort} and ${httpsPort} in your cloud firewall.`,
	);
	let alive = true;
	onDestroy(() => (alive = false));
	// The first answer about the server's machine. The switch waits for it.
	let asked: Promise<AccessPreflight | null> = Promise.resolve(null);
	// The switch's last rewrite of the field, and the choice it was made for. Switching
	// back gives the text from before again, with a port the owner typed.
	let swapped: { from: string; to: string; on: boolean } | null = null;

	// Start from the address this console is open on (the owner reached the server
	// there), else from what the server sees on its own network card. Readers elsewhere
	// cannot use a home or office network address, so the public address the server
	// found replaces such a guess, unless the owner typed something meanwhile.
	onMount(async () => {
		asked = loadAccessPreflight().then((p) => (alive ? (preflight = p) : null));
		if (address) return;
		const own = serverAddress();
		const guess = reachableFromOtherDevices(own) ? own : await loadLanAddress();
		let filled = '';
		if (guess && !address) address = filled = guess.replace(/^http:\/\//, '');
		const found = await asked;
		if (found?.publicIp && address === filled && (!guess || isPrivateAddress(guess))) {
			address = suggestedAddress(found, encrypt);
		}
	});

	/**
	 * The switch. The field follows it: a bare host for HTTPS, the host and the server's
	 * HTTP port without. An address kept from the other choice would be saved as it is
	 * and lead nowhere (`203.0.113.5` without HTTPS is port 80).
	 */
	async function setEncrypt(on: boolean): Promise<void> {
		encrypt = on;
		reached = null;
		error = null;
		// The ports matter again: the owner may have freed one meanwhile.
		if (on && preflight) {
			void loadAccessPreflight().then((p) => {
				if (alive && p) preflight = p;
			});
		}
		const before = address;
		const port = (preflight ?? (await asked))?.httpPort;
		if (!alive || encrypt !== on || address !== before) return;
		const back = swapped?.to === before && swapped.on !== on ? swapped.from : null;
		const next = back ?? addressFor(before, on, port);
		if (next !== before) swapped = { from: before, to: next, on };
		address = next;
	}

	/** `203.0.113.5:8080` as an origin, or null when it is not an address. */
	function origin(): string | null {
		const typed = address.trim();
		try {
			const url = new URL(typed.includes('://') ? typed : `http://${typed}`);
			return typed && url.hostname ? url.origin : null;
		} catch {
			return null;
		}
	}

	/** Whether THIS browser gets an answer there. It says nothing about other networks. */
	async function check(): Promise<void> {
		if (checking) return;
		reached = null;
		error = null;
		const at = origin();
		if (!at) {
			error = 'Enter an address like 203.0.113.5:8080.';
			return;
		}
		checking = true;
		try {
			await fetch(`${at}/health`, {
				mode: 'no-cors',
				cache: 'no-store',
				signal: AbortSignal.timeout(6000),
			});
			reached = true;
		} catch {
			reached = false;
		} finally {
			checking = false;
		}
	}
</script>

<div class="ob-stack">
	<div class="k-field">
		<label class="k-label" for="direct-address">Server address</label>
		<input
			id="direct-address"
			aria-describedby="direct-address-msg"
			class="k-input ob-input ob-mono"
			type="text"
			inputmode="url"
			autocapitalize="none"
			spellcheck={false}
			placeholder={encrypt ? '203.0.113.5' : '203.0.113.5:8080'}
			bind:value={address}
			oninput={() => {
				reached = null;
				error = null;
			}}
			aria-invalid={error ? true : undefined}
		/>
		{#if error}
			<span id="direct-address-msg" class="k-error">{error}</span>
		{:else}
			<span id="direct-address-msg" class="k-hint"
				>{encrypt
					? `This server’s public IP address. ${portsHint}`
					: 'This server’s public IP address and its port. Open that port in your cloud firewall.'}</span
			>
		{/if}
	</div>
	<div class="ob-hint-row">
		<Switch checked={encrypt} label="Encrypt the connection" onchange={setEncrypt} />
		<span>Encrypt the connection (HTTPS)</span>
	</div>
	<Preflight lines={serverLines} />
	{#if encrypt}
		<div class="k-notice" data-tone="info" role="note">
			<Icon name="shield" />
			<div class="k-notice__body">
				komiq gets a free certificate for this address from Let’s Encrypt and renews it by itself.
				Saving can take a minute. By continuing you agree to the
				<a
					class="k-link"
					href="https://letsencrypt.org/repository/"
					target="_blank"
					rel="noreferrer">Let’s Encrypt Subscriber Agreement</a
				>.
			</div>
		</div>
	{:else}
		<div class="ob-hint-row">
			<Button variant="ghost" icon="check" busy={checking} onclick={check}>Check address</Button>
			{#if reached === true}
				<span class="k-hint" role="status">This browser reached the server there.</span>
			{:else if reached === false}
				<span class="k-error" role="status"
					>This browser got no answer there. Check the address, the port and the firewall.</span
				>
			{/if}
		</div>
		<div class="k-notice" data-tone="warning" role="note">
			<Icon name="alert" />
			<div class="k-notice__body">
				This connection is not encrypted. People on the network between a reader and this server can
				see passwords and pages.
			</div>
		</div>
	{/if}
</div>
