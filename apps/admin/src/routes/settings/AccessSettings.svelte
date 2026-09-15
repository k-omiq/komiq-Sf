<script lang="ts">
	import { onMount } from 'svelte';
	import type { AccessStatus, CertificateStatus, CloudflareConnection } from '@komika/api';
	import { confirm } from '$lib/confirm.svelte';
	import { desktop } from '$lib/desktop';
	import {
		loadAccessStatus,
		loadCertificateStatus,
		loadLanAddress,
		removeCloudflareSetup,
		retryCloudflareSetup,
		setAccessMode,
	} from '$lib/data';
	import { copyText } from '$lib/passwords';
	import { addressFor, typedFromInternet } from '$lib/preflight';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Checklist from '$lib/components/Checklist.svelte';
	import CloudflareSetup, { type Phase } from '$lib/components/CloudflareSetup.svelte';
	import DirectAddress from '$lib/components/DirectAddress.svelte';
	import HomeNetwork from '$lib/components/HomeNetwork.svelte';
	import HttpsMove from '$lib/components/HttpsMove.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Qr from '$lib/components/Qr.svelte';
	import { reachableFromOtherDevices, runsLocalServer, serverAddress, target } from '$lib/config';
	import { moveTo } from '$lib/server-move.svelte';

	// Settings › Access: how readers reach this server, what Cloudflare setup made, and
	// the connector. Re-run and Remove ask for the API token again; it is not kept.
	// "Only my devices" and "Turn on" switch the public way in off and on without one:
	// what setup made at Cloudflare stays until Remove.

	let status = $state<AccessStatus | null>(null);
	let loadError = $state<string | null>(null);
	let panel = $state<'setup' | 'direct' | 'rerun' | 'remove' | null>(null);
	// The "without a domain" form.
	let directAddress = $state('');
	let directError = $state<string | null>(null);
	let directEncrypt = $state(true);
	// HTTPS on this server (direct mode, or Cloudflare's Public IP connection): the
	// certificate's end date, what is wrong with it, and why HTTPS is not running.
	let certificate = $state<CertificateStatus | null>(null);
	const directHttps = $derived(
		status?.mode === 'direct' && (status.address ?? '').startsWith('https://'),
	);
	let token = $state('');
	let tokenError = $state<string | null>(null);
	let acting = $state(false);
	// "Turn on" and "Only my devices" act at once, without a panel.
	let switching = $state<'on' | 'local' | null>(null);
	let switchError = $state<string | null>(null);
	let copied = $state(false);
	let alive = true;
	let pollId = 0;

	// The console is open on the server's own computer: the address phones and tablets use
	// (the console's own is this computer's loopback).
	let lanAddress = $state<string | null>(null);
	// Internet mode: the public address. Otherwise, on a self-hosted install, the
	// server's own address (it serves the reader there). The hosted console has no
	// such address: its API origin is not where anyone reads.
	const address = $derived(
		status?.address ?? lanAddress ?? (target === 'hosted' ? '' : serverAddress()),
	);

	// The "Set up Cloudflare" form.
	let setup = $state<ReturnType<typeof CloudflareSetup>>();
	let setupPhase = $state<Phase>('form');
	let setupBusy = $state(false);
	let setupDomain = $state('');
	let setupConnection = $state<CloudflareConnection>('TUNNEL');
	let setupError = $state<string | null>(null);

	onMount(() => {
		// Only for the server this app runs itself (its console address is loopback).
		if (desktop.available && !reachableFromOtherDevices(serverAddress())) {
			desktop.network().then(
				(net) => {
					if (alive && net.enabled && net.addresses.length) lanAddress = net.addresses[0];
				},
				() => {},
			);
		} else if (target !== 'hosted' && !reachableFromOtherDevices(serverAddress())) {
			// A browser on the server itself: the server knows its network address.
			void loadLanAddress().then((a) => {
				if (alive && a) lanAddress = a;
			});
		}
		void refresh().then(() => {
			if (status?.job?.running) void follow();
		});
		return () => {
			alive = false;
			pollId++;
		};
	});

	async function refresh(): Promise<void> {
		try {
			status = await loadAccessStatus();
			loadError = null;
			certificate =
				status.mode === 'direct' || status.cloudflare?.connection === 'PUBLIC_IP'
					? await loadCertificateStatus()
					: null;
		} catch (err) {
			loadError = err instanceof Error ? err.message : 'Access settings could not be loaded.';
		}
	}

	async function follow(): Promise<void> {
		const id = ++pollId;
		while (alive && id === pollId) {
			await refresh();
			if (!status?.job?.running) break;
			await new Promise((r) => setTimeout(r, 800));
		}
	}

	// A finished setup from the form below becomes the page's status.
	$effect(() => {
		if (panel === 'setup' && (setupPhase === 'done' || setupPhase === 'failed')) {
			void refresh();
		}
	});

	const cf = $derived(status?.cloudflare ?? null);
	const job = $derived(status?.job ?? null);
	// Set up, then turned off by "Only my devices" or by the direct mode.
	const paused = $derived(!!cf?.complete && status?.mode !== 'internet');
	// "Without a domain" is for a server on a public address, which the desktop app's own
	// server is not (see the Access step of setup). A setup that exists can still be
	// changed. Not offered next to a Cloudflare setup that is on or unfinished.
	const offersDirect = $derived(
		target !== 'hosted' && (status?.mode === 'direct' || (!runsLocalServer && (!cf || paused))),
	);
	const connectionName = (c: CloudflareConnection | null | undefined) =>
		c === 'PUBLIC_IP' ? 'Public IP' : 'Cloudflare Tunnel';

	const connector = $derived.by(() => {
		switch (status?.connector) {
			case 'running':
				return { text: 'Connected', tone: 'success' };
			case 'starting':
				return { text: 'Connecting…', tone: undefined };
			case 'failed':
				return { text: 'Restarting', tone: 'warning' };
			case 'missing':
				return { text: 'cloudflared not found', tone: 'danger' };
			default:
				return { text: 'Stopped', tone: undefined };
		}
	});

	function open(which: 'setup' | 'direct' | 'rerun' | 'remove'): void {
		panel = which;
		token = '';
		tokenError = null;
		switchError = null;
		if (which === 'direct') {
			directError = null;
			const stored = status?.mode === 'direct' ? (status.address ?? '') : '';
			// HTTPS has its own port: the field holds the bare host then.
			directAddress = directHttps ? addressFor(stored, true) : stored.replace(/^https?:\/\//, '');
			directEncrypt = status?.mode !== 'direct' || directHttps;
		}
		if (which === 'setup') {
			setupPhase = 'form';
			setupDomain = status?.domain ?? '';
			setupError = null;
		}
	}

	/** Cloudflare done by hand: record the domain, as the setup wizard does. */
	async function saveManual(): Promise<void> {
		if (acting) return;
		if (!setupDomain.trim().includes('.')) {
			setupError = 'Enter the domain readers will use.';
			return;
		}
		acting = true;
		setupError = null;
		try {
			status = await setAccessMode('internet', setupDomain.trim());
			await refresh();
			panel = null;
			toast.success('Readers now use this domain.');
		} catch (err) {
			setupError = err instanceof Error ? err.message : 'The domain could not be saved.';
		} finally {
			acting = false;
		}
	}

	/** A paused Cloudflare setup, on again. The server kept what it needs: no token. */
	async function turnOn(): Promise<void> {
		if (switching || !cf) return;
		// From "Public without a domain" this takes the place of the address readers use.
		if (status?.mode === 'direct') {
			const ok = await confirm({
				title: 'Turn Cloudflare on?',
				message: `Readers then use https://${cf.domain}. ${status.address ?? 'The address without a domain'} stops being the reader address${directHttps ? ', and its HTTPS is turned off' : ''}.`,
				confirmLabel: 'Turn on',
			});
			if (!ok) return;
		}
		switching = 'on';
		switchError = null;
		try {
			status = await setAccessMode('internet', cf.domain);
			await refresh();
			toast.success('Cloudflare is turned on.');
		} catch (err) {
			switchError = err instanceof Error ? err.message : 'Cloudflare could not be turned on.';
		} finally {
			switching = null;
		}
	}

	/** What "Only my devices" turns off, in plain words, for its confirm step. */
	function whatStops(s: AccessStatus): string {
		const at = s.address ?? 'the public address';
		const viaCloudflare = s.mode === 'internet' && !!s.cloudflare;
		// A console that is open at a public address loses its way in with the switch: in
		// this mode the server refuses plain HTTP from the internet, and the HTTPS that
		// komiq serves or set up is turned off. (HTTPS of the owner's own proxy stays.)
		const here = serverAddress();
		const lockedOut = (
			here.startsWith('http://')
				? target !== 'hosted' && typedFromInternet(here)
				: here === at && (viaCloudflare || directHttps)
		)
			? 'This console is open at a public address. After the switch it stops answering here. Open it on your home network, or run "komika-server setup" on the server to change the mode.'
			: '';
		if (!viaCloudflare && !directHttps) {
			// Nothing of komiq's carries that address, so there is nothing to turn off.
			return [
				s.mode === 'internet'
					? `komiq stops showing ${at} as the reader address. What you set up by hand at Cloudflare keeps running until you turn it off there.`
					: `komiq stops showing ${at} as the reader address. The HTTP port stops answering clients on the internet.`,
				lockedOut,
			]
				.filter(Boolean)
				.join(' ');
		}
		const off =
			viaCloudflare && s.cloudflare?.connection !== 'PUBLIC_IP'
				? 'The tunnel'
				: 'HTTPS on this server';
		return [
			`${off} is turned off. Readers on the internet can no longer open ${at}.`,
			viaCloudflare
				? 'The DNS record and the other things at Cloudflare stay until you choose "Remove".'
				: '',
			lockedOut,
		]
			.filter(Boolean)
			.join(' ');
	}

	/** Back to "Only my devices": the public way in is turned off, nothing is deleted. */
	async function goLocal(): Promise<void> {
		if (switching || !status) return;
		const ok = await confirm({
			title: 'Switch to "Only my devices"?',
			message: whatStops(status),
			confirmLabel: 'Switch',
		});
		if (!ok) return;
		switching = 'local';
		switchError = null;
		try {
			status = await setAccessMode('local');
			certificate = null;
			toast.success('Access is now "Only my devices".');
		} catch (err) {
			switchError = err instanceof Error ? err.message : 'The access choice could not be saved.';
		} finally {
			switching = null;
		}
	}

	/** Public at the server's own address: no domain, no Cloudflare. */
	async function saveDirect(): Promise<void> {
		if (acting) return;
		acting = true;
		directError = null;
		try {
			status = await setAccessMode('direct', directAddress.trim(), directEncrypt);
			certificate = await loadCertificateStatus();
			// The form stays busy while the desktop app moves to the new address: that can
			// take half a minute. When it gets no answer there, `HttpsMove` below says so.
			if (status.address) await moveTo(status.address, directAddress);
			panel = null;
			toast.success('Readers now use this address.');
		} catch (err) {
			directError = err instanceof Error ? err.message : 'The address could not be saved.';
		} finally {
			acting = false;
		}
	}

	async function act(): Promise<void> {
		if (acting || !panel || panel === 'setup' || panel === 'direct') return;
		if (!token.trim()) {
			tokenError = 'Paste the API token.';
			return;
		}
		if (panel === 'remove') {
			const ok = await confirm({
				title: 'Remove the Cloudflare setup?',
				message: `This deletes the DNS record for ${cf?.domain ?? 'the domain'}, ${
					cf?.connection === 'PUBLIC_IP' ? 'the origin certificate' : 'the tunnel'
				} and the cache rules that setup created, and puts the zone settings back. Readers can no longer reach this server at that address.`,
				confirmLabel: 'Remove',
				danger: true,
			});
			if (!ok) return;
		}
		acting = true;
		tokenError = null;
		try {
			status =
				panel === 'remove'
					? await removeCloudflareSetup(token.trim())
					: await retryCloudflareSetup(token.trim());
			token = '';
			panel = null;
		} catch (err) {
			tokenError = err instanceof Error ? err.message : 'That did not work.';
			return;
		} finally {
			acting = false;
		}
		await follow();
		if (status?.job && !status.job.failed) {
			toast.success(
				status.job.kind === 'remove' ? 'Cloudflare setup removed.' : 'Cloudflare setup is done.',
			);
		}
	}

	async function copy(text: string): Promise<void> {
		if (await copyText(text)) {
			copied = true;
			setTimeout(() => (copied = false), 1600);
		}
	}
</script>

{#if loadError}
	<div class="k-notice" data-tone="danger" role="alert">
		<Icon name="alert" />
		<div class="k-notice__body">{loadError}</div>
		<Button size="sm" variant="ghost" onclick={refresh}>Retry</Button>
	</div>
{:else if !status}
	<p class="k-hint">Loading…</p>
{:else}
	<div class="access">
		<dl class="facts">
			<dt>Mode</dt>
			<dd>
				{status.mode === 'internet'
					? 'Public on the internet'
					: status.mode === 'direct'
						? 'Public without a domain'
						: status.mode === 'local'
							? 'Only my devices'
							: 'Not chosen yet'}
			</dd>
			{#if address}
				<dt>Address</dt>
				<dd class="row">
					<a class="k-link" href={address} target="_blank" rel="noreferrer">{address}</a>
					<Button
						variant="ghost"
						size="sm"
						icon={copied ? 'check' : 'copy'}
						iconOnly
						aria-label="Copy address"
						onclick={() => copy(address)}
					/>
				</dd>
				{#if reachableFromOtherDevices(address)}
					<dt>QR code</dt>
					<dd class="stack">
						<Qr value={address} size={120} label="QR code for {address}" />
						<span class="k-hint"
							>Scan with a phone to open the reader, or in the komiq app on its "Connect to a
							server" screen.</span
						>
					</dd>
				{/if}
			{/if}
			{#if status.mode === 'direct'}
				<dt>Encryption</dt>
				<dd class="stack">
					{#if !directHttps}
						<span class="status" data-tone="warning">Off</span>
						<span class="k-hint"
							>People on the network between a reader and this server can see passwords and pages.
							Turn it on with "Change the address".</span
						>
					{:else}
						<span class="status" data-tone={status.https ? 'success' : 'warning'}
							>{status.https ? 'On' : 'Not running'}</span
						>
						{#if certificate?.httpsError}
							<span class="k-error">{certificate.httpsError} The server tries again by itself.</span
							>
						{/if}
						{#if certificate?.certificateError}
							<span class="k-error">{certificate.certificateError}</span>
						{:else if certificate?.certificateExpires}
							<span class="k-hint"
								>Let’s Encrypt certificate, valid until {new Date(
									certificate.certificateExpires,
								).toLocaleDateString()}. The server renews it by itself.</span
							>
						{/if}
						{#if certificate}
							<span class="k-hint"
								>{certificate.plainHttpClosed
									? 'The plain HTTP port no longer answers clients on the internet.'
									: 'The plain HTTP port stops answering clients on the internet after the first visit over HTTPS.'}</span
							>
						{/if}
					{/if}
				</dd>
			{/if}
			<dt>Cloudflare</dt>
			<dd>
				{#if cf}
					{connectionName(cf.connection)} for {cf.domain}{cf.zoneName && cf.zoneName !== cf.domain
						? ` (zone ${cf.zoneName})`
						: ''}
					{#if !cf.complete}
						<span class="status" data-tone="warning">Unfinished</span>
					{:else if paused}
						<span class="status">Turned off</span>
					{/if}
				{:else}
					Not set up
				{/if}
			</dd>
			<!-- Turned off, the connector and the HTTPS of setup are stopped on purpose. -->
			{#if cf?.connection === 'TUNNEL' && !paused}
				<dt>Connector</dt>
				<dd class="stack">
					<span class="status" data-tone={connector.tone}>{connector.text}</span>
					{#if status.connectorMessage && status.connector !== 'running'}
						<span class="k-hint">{status.connectorMessage}</span>
					{/if}
				</dd>
			{:else if cf?.connection === 'PUBLIC_IP' && !paused}
				<dt>HTTPS</dt>
				<dd class="stack">
					<span class="status" data-tone={status.https ? 'success' : 'warning'}
						>{status.https ? 'Serving' : 'Not running'}</span
					>
					{#if certificate?.httpsError}
						<span class="k-error">{certificate.httpsError} The server tries again by itself.</span>
					{/if}
					{#if certificate?.certificateError}
						<span class="k-error">{certificate.certificateError}</span>
					{:else if certificate?.certificateExpires}
						<span class="k-hint"
							>Cloudflare origin certificate, valid until {new Date(
								certificate.certificateExpires,
							).toLocaleDateString()}. Nothing renews it by itself: "Run setup again" gets a new one
							in its last 30 days.</span
						>
					{/if}
				</dd>
			{/if}
			<dt>Visitor addresses</dt>
			<dd>
				{#if status.trustedProxiesPinned}
					Set by TRUSTED_PROXY_CIDRS on the server
				{:else if status.trustedProxies.length}
					Read from Cloudflare
				{:else}
					Taken from each connection
				{/if}
			</dd>
		</dl>

		{#if directHttps && address}<HttpsMove {address} />{/if}

		{#if desktop.available && runsLocalServer}
			<!-- The app's own server: the home network switch of setup, to change it later. The
			     address shows above when readers use it (no public address); else here. -->
			<div class="home">
				<HomeNetwork
					addressView={status.address ? 'list' : 'none'}
					onchange={(net) => (lanAddress = net.enabled ? (net.addresses[0] ?? null) : null)}
				/>
			</div>
		{/if}

		{#if job && (job.running || job.failed)}
			<div class="run">
				<p class="run-title">
					{job.kind === 'remove' ? 'Removal' : 'Setup'}{job.running
						? ''
						: ' stopped at a failed step'}
				</p>
				<Checklist
					steps={job.steps}
					label={job.kind === 'remove' ? 'Cloudflare removal' : 'Cloudflare setup'}
					onretry={job.failed ? () => open(job?.kind === 'remove' ? 'remove' : 'rerun') : undefined}
				/>
			</div>
		{:else if job}
			<details class="run">
				<summary class="run-title">
					{job.kind === 'remove' ? 'Removal finished' : 'Setup finished'}. Show the steps
				</summary>
				<Checklist
					steps={job.steps}
					label={job.kind === 'remove' ? 'Cloudflare removal' : 'Cloudflare setup'}
				/>
			</details>
		{/if}

		{#if panel === 'setup'}
			<div class="panel">
				<CloudflareSetup
					bind:this={setup}
					bind:domain={setupDomain}
					bind:connection={setupConnection}
					bind:phase={setupPhase}
					bind:busy={setupBusy}
					resume={false}
				/>
				{#if setupError && setupPhase === 'manual'}
					<p class="k-error" role="alert">{setupError}</p>
				{/if}
				<div class="actions">
					<Button variant="ghost" disabled={setupBusy || acting} onclick={() => (panel = null)}>
						{setupPhase === 'done' ? 'Close' : 'Cancel'}
					</Button>
					{#if setupPhase === 'form'}
						<Button variant="primary" busy={setupBusy} onclick={() => setup?.start()}>Set up</Button
						>
					{:else if setupPhase === 'manual'}
						<Button variant="primary" busy={acting} onclick={saveManual}>Save</Button>
					{/if}
				</div>
			</div>
		{:else if panel === 'direct'}
			<div class="panel">
				<DirectAddress
					bind:address={directAddress}
					bind:error={directError}
					bind:encrypt={directEncrypt}
				/>
				{#if acting && directEncrypt}
					<p class="k-hint" role="status">Getting a certificate. This can take a minute.</p>
				{/if}
				<div class="actions">
					<Button variant="ghost" disabled={acting} onclick={() => (panel = null)}>Cancel</Button>
					<Button variant="primary" busy={acting} onclick={saveDirect}>Save</Button>
				</div>
			</div>
		{:else if panel}
			<div class="panel">
				<label class="k-field">
					<span class="k-label">Cloudflare API token</span>
					<input
						class="k-input k-mono"
						type="password"
						autocomplete="off"
						spellcheck={false}
						bind:value={token}
						aria-invalid={tokenError ? true : undefined}
					/>
					{#if tokenError}
						<span class="k-error">{tokenError}</span>
					{:else}
						<span class="k-hint"
							>The same permissions as for setup. It is used for this run and not kept.</span
						>
					{/if}
				</label>
				<div class="actions">
					<Button variant="ghost" disabled={acting} onclick={() => (panel = null)}>Cancel</Button>
					<Button variant="primary" busy={acting} onclick={act}>
						{panel === 'remove' ? 'Remove' : 'Run setup again'}
					</Button>
				</div>
			</div>
		{:else if !job?.running}
			{#if switchError}<p class="k-error" role="alert">{switchError}</p>{/if}
			<div class="actions start">
				{#if paused}
					<Button
						variant="ghost"
						icon="play"
						busy={switching === 'on'}
						disabled={!!switching}
						onclick={turnOn}>Turn on</Button
					>
				{:else if cf}
					<Button variant="ghost" icon="refresh" onclick={() => open('rerun')}
						>Run setup again</Button
					>
				{:else}
					<Button variant="ghost" icon="globe" onclick={() => open('setup')}>
						Set up Cloudflare
					</Button>
				{/if}
				{#if cf?.removable}
					<Button variant="ghost" icon="trash" onclick={() => open('remove')}>Remove</Button>
				{/if}
				{#if offersDirect}
					<Button variant="ghost" icon="server" onclick={() => open('direct')}>
						{status.mode === 'direct' ? 'Change the address' : 'Use without a domain'}
					</Button>
				{/if}
				{#if status.mode === 'internet' || status.mode === 'direct'}
					<Button
						variant="ghost"
						icon="monitor"
						busy={switching === 'local'}
						disabled={!!switching}
						onclick={goLocal}>Only my devices</Button
					>
				{/if}
			</div>
		{/if}
	</div>
{/if}

<style>
	.access {
		display: grid;
		gap: var(--k-space-5);
	}
	.facts {
		display: grid;
		grid-template-columns: 140px minmax(0, 1fr);
		gap: var(--k-space-3) var(--k-space-4);
		margin: 0;
		font-size: var(--k-text-sm);
	}
	.facts dt {
		color: var(--k-text-dim);
	}
	.facts dd {
		margin: 0;
		min-width: 0;
		color: var(--k-text-1);
		overflow-wrap: anywhere;
	}
	.facts .row {
		display: flex;
		align-items: center;
		gap: 4px;
		margin-block: -5px;
	}
	.facts .stack {
		display: grid;
		gap: 4px;
	}
	/* State as a dot and a word: flat, no outlined badge. */
	.status {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--k-text-1);
	}
	.facts dd > .status:not(:first-child) {
		margin-left: 8px;
	}
	.status::before {
		content: '';
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--k-text-dim);
	}
	.status[data-tone='success']::before {
		background: var(--k-success);
	}
	.status[data-tone='warning']::before {
		background: var(--k-warning);
	}
	.status[data-tone='danger']::before {
		background: var(--k-danger);
	}
	.run {
		display: grid;
		gap: var(--k-space-3);
	}
	details.run {
		gap: 0;
	}
	details.run[open] {
		gap: var(--k-space-3);
	}
	summary.run-title {
		cursor: pointer;
		color: var(--k-text-muted);
	}
	.run-title {
		margin: 0;
		font-size: var(--k-text-sm);
		font-weight: 600;
		color: var(--k-text-1);
	}
	.panel {
		display: grid;
		gap: var(--k-space-4);
		max-width: 520px;
	}
	.home {
		display: grid;
		gap: var(--k-space-3);
		max-width: 520px;
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		justify-content: flex-end;
		gap: var(--k-space-2);
	}
	.actions.start {
		justify-content: flex-start;
		margin-left: -10px;
	}
	@media (max-width: 520px) {
		.facts {
			grid-template-columns: minmax(0, 1fr);
			gap: 2px var(--k-space-4);
		}
		.facts dd,
		.facts dd.row {
			margin-bottom: var(--k-space-2);
		}
	}
</style>
