<script lang="ts">
	import { onMount } from 'svelte';
	import type { AccessStatus } from '@komika/api';
	import { auth } from '$lib/auth.svelte';
	import { desktop } from '$lib/desktop';
	import { instance } from '$lib/instance.svelte';
	import Qr from '$lib/components/Qr.svelte';
	import { reachableFromOtherDevices, runsLocalServer, serverAddress } from '$lib/config';
	import { loadAccessStatus, loadLanAddress } from '$lib/data';
	import Button from '$lib/components/Button.svelte';
	import HttpsMove from '$lib/components/HttpsMove.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { copyText, finishSetup, labels, stepNumber, wizard } from '../wizard.svelte';

	let copied = $state(false);
	// How readers reach the server, as the server has it. Undefined while it is asked
	// (no address is shown on a guess), null when it cannot say.
	let access = $state<AccessStatus | null | undefined>(undefined);
	// A public mode: readers use the domain, or the server's public address, not the
	// address this console uses. Read from the server, not from what was typed here: a
	// Cloudflare setup that failed or was put off leaves the server without one.
	const publicAddress = $derived(
		access?.mode === 'internet' || access?.mode === 'direct' ? (access.address ?? '') : '',
	);
	// The owner chose a public way to reach the server, and the server has none yet.
	const notPublicYet = $derived(
		!!access &&
			!publicAddress &&
			(wizard.extra.access === 'internet' || wizard.extra.access === 'direct'),
	);
	// The address phones and tablets use, when the console's own is this computer's
	// loopback (the desktop app, or a browser on the server itself).
	let lanAddress = $state<string | null>(null);
	onMount(async () => {
		access = await loadAccessStatus().catch(() => null);
		if (publicAddress) return;
		// Home network on (desktop app): the app knows the addresses it opened.
		if (wizard.extra.lan && desktop.available) {
			try {
				const net = await desktop.network();
				if (net.enabled && net.addresses.length) lanAddress = net.addresses[0];
			} catch {
				/* ask the server below */
			}
		}
		if (!lanAddress && !reachableFromOtherDevices(serverAddress())) {
			lanAddress = await loadLanAddress();
		}
	});
	const address = $derived(publicAddress || (lanAddress ?? serverAddress()));
	// The server's effective setting: its environment can pin sign-up, whatever was chosen.
	const signupOpen = $derived(instance.info?.registrationOpen ?? wizard.signup === 'open');

	async function copy(): Promise<void> {
		if (await copyText(address)) {
			copied = true;
			setTimeout(() => (copied = false), 1600);
		}
	}
</script>

<Onboarding
	step={stepNumber('done')}
	steps={labels()}
	title="You are all set"
	lead="Your server is ready to use."
>
	<dl class="ob-rows">
		<div>
			<dt>Reader</dt>
			<dd>
				{#if access !== undefined}
					<span class="ob-truncate">{address}</span>
					<Button
						variant="ghost"
						size="sm"
						icon={copied ? 'check' : 'copy'}
						iconOnly
						aria-label="Copy reader address"
						onclick={copy}
					/>
				{:else}
					<span class="k-hint">Checking…</span>
				{/if}
			</dd>
		</div>
		<div>
			<dt>Owner</dt>
			<dd>{auth.user?.username ?? '—'}</dd>
		</div>
		<div>
			<dt>Sign-up</dt>
			<dd>{signupOpen ? 'Anyone can sign up' : 'Only people you add'}</dd>
		</div>
		{#if wizard.extra.mangadex !== undefined || wizard.extra.extensions}
			<div>
				<dt>Sources</dt>
				<dd>
					MangaDex {wizard.extra.mangadex ? 'on' : 'off'} · {Number(wizard.extra.extensions ?? 0)}
					{Number(wizard.extra.extensions ?? 0) === 1 ? 'extension' : 'extensions'}
				</dd>
			</div>
		{/if}
		<div>
			<dt>People added</dt>
			<dd class="k-num">{wizard.added.length}</dd>
		</div>
	</dl>
	{#if notPublicYet}
		<p class="k-hint">Internet access is not set up yet. You can finish it in Settings › Access.</p>
	{/if}
	{#if access !== undefined}
		{#if reachableFromOtherDevices(address)}
			<div class="ob-hint-row lan-qr">
				<Qr value={address} size={120} label="QR code for {address}" />
				<p class="k-hint">
					Scan with a phone{publicAddress ? '' : ' on the same Wi-Fi'} to open the reader. The komiq app
					scans it on its "Connect to a server" screen.
				</p>
			</div>
		{:else}
			<p class="k-hint">
				This address works on this computer only.{runsLocalServer
					? ''
					: ' On other devices, replace localhost with this computer’s network address.'}
			</p>
		{/if}
		<HttpsMove {address} />
	{/if}
	{#each wizard.notices as notice (notice)}<p class="k-hint">{notice}</p>{/each}
	{#snippet footer()}
		<Button variant="ghost" onclick={() => finishSetup('/')}>Open the catalog</Button>
		<Button variant="primary" onclick={() => finishSetup('/sources/extensions')}>
			Add your first source
		</Button>
	{/snippet}
</Onboarding>

<style>
	.lan-qr {
		gap: var(--k-space-4);
	}
</style>
