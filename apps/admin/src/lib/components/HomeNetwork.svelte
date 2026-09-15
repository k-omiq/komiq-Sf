<script lang="ts">
	import { onMount } from 'svelte';
	import { saveLocalServer, serverAddress } from '$lib/config';
	import { loadLanClientSeenAt } from '$lib/data';
	import { bridgeError, desktop, type FirewallInfo, type LocalNetwork } from '$lib/desktop';
	import { copyText } from '$lib/passwords';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';
	import Qr from './Qr.svelte';
	import Switch from './Switch.svelte';
	import '../onboarding.css';

	// The desktop app running its own server: let other devices at home open the reader.
	// The switch restarts the server on every interface (or back on loopback only) through
	// the bridge; nothing is reachable from outside until it is on. Used by the Home
	// network step of setup and by Settings › Access.
	//
	// While the switch is on, it asks the server whether a device on the network has
	// reached it, so the owner sees the phone get through. When it does not, the usual
	// two causes (another network, this computer's firewall) are one click away.

	let {
		// How the addresses show: with a QR code, as rows only, or not at all (the page
		// around it shows the address itself).
		addressView = 'qr',
		busy = $bindable(false),
		onchange,
	}: {
		addressView?: 'qr' | 'list' | 'none';
		busy?: boolean;
		onchange?: (net: LocalNetwork) => void;
	} = $props();

	let net = $state<LocalNetwork | null>(null);
	let error = $state<string | null>(null);
	let copied = $state<string | null>(null);

	// Null: the app cannot say which system this is or open its firewall (an older app).
	let firewall = $state<FirewallInfo | null>(null);
	let opening = $state(false);
	let opened = $state<string | null>(null);
	let openError = $state<string | null>(null);

	onMount(async () => {
		void desktop.firewallInfo().then((info) => (firewall = info));
		try {
			net = await desktop.network();
		} catch (err) {
			error = bridgeError(err, 'The network setting could not be read.');
		}
	});

	async function toggle(enabled: boolean): Promise<void> {
		if (busy) return;
		busy = true;
		error = null;
		try {
			const now = await desktop.setNetwork(enabled);
			net = now;
			onchange?.(now);
			// The port only moves when the old one was taken meanwhile. The console talks to
			// the server at its address, so follow it (the page comes back where it was).
			const own = now.port ? `http://127.0.0.1:${now.port}` : null;
			if (own && own !== serverAddress()) {
				saveLocalServer(own);
				location.reload();
			}
		} catch (err) {
			error = bridgeError(err, 'The server could not be restarted.');
		} finally {
			busy = false;
		}
	}

	async function copy(text: string): Promise<void> {
		if (await copyText(text)) {
			copied = text;
			setTimeout(() => (copied = copied === text ? null : copied), 1600);
		}
	}

	async function openFirewall(): Promise<void> {
		if (opening) return;
		opening = true;
		opened = null;
		openError = null;
		try {
			opened = (await desktop.openFirewall()) || 'The firewall now lets komiq through.';
		} catch (err) {
			openError = bridgeError(err, 'The firewall could not be opened.');
		} finally {
			opening = false;
		}
	}

	const enabled = $derived(net?.enabled ?? false);
	const addresses = $derived(net?.addresses ?? []);

	// Has a device on the home network reached the server? Asked every 2 seconds while
	// the addresses are on screen, until one has. `off`: nothing to say (the switch is
	// off, or the server cannot tell).
	let seen = $state<'off' | 'waiting' | 'seen'>('off');
	const watching = $derived(enabled && addresses.length > 0 && !busy);
	$effect(() => {
		if (!watching) {
			seen = 'off';
			return;
		}
		let live = true;
		let timer: ReturnType<typeof setTimeout> | undefined;
		const ask = async (): Promise<void> => {
			let again = true;
			try {
				const at = await loadLanClientSeenAt();
				if (!live) return;
				if (at === undefined) {
					seen = 'off'; // The server predates the question.
					again = false;
				} else if (at) {
					seen = 'seen';
					again = false;
				} else {
					seen = 'waiting';
				}
			} catch {
				// The server is restarting, or did not answer: ask again.
			}
			if (live && again) timer = setTimeout(ask, 2000);
		};
		void ask();
		// Leaving the page, or the switch going off, stops the asking.
		return () => {
			live = false;
			clearTimeout(timer);
		};
	});

	const firewallHelp = $derived.by(() => {
		const port = net?.port ? ` ${net.port}` : '';
		switch (firewall?.os) {
			case 'macos':
				return 'macOS asks if komiq may accept incoming connections. Choose Allow. To change it later: System Settings › Network › Firewall › Options.';
			case 'windows':
				return firewall.automatic
					? 'Windows asks the first time. If you chose Cancel, use the button below.'
					: 'Windows asks the first time. If you chose Cancel, open Windows Security › Firewall & network protection › Allow an app through firewall, and allow komiq.';
			case 'linux':
				return firewall.automatic
					? 'Linux does not ask. If this computer has a firewall, use the button below to let komiq through.'
					: `Linux does not ask. If this computer has a firewall, allow incoming connections on port${port} in it.`;
			default:
				return 'If this computer has a firewall, allow komiq through it.';
		}
	});
</script>

<div class="toggle">
	<div class="toggle-text">
		<p class="toggle-title">Allow other devices</p>
		<p class="toggle-detail">
			Devices on the same Wi-Fi can open the reader. Off keeps komiq on this computer only.
		</p>
	</div>
	<Switch checked={enabled} label="Allow other devices" disabled={busy || !net} onchange={toggle} />
</div>

{#if busy}
	<p class="k-hint ob-hint-row" role="status">
		<span class="k-spin"><Icon name="refresh" size={14} /></span>Restarting the server…
	</p>
{:else if error}
	<p class="k-error" role="alert">{error}</p>
{:else if enabled}
	{#if addresses.length}
		{#if addressView !== 'none'}
			<div class="lan">
				{#if addressView === 'qr'}
					<Qr value={addresses[0]} label="QR code for {addresses[0]}" />
				{/if}
				<div class="lan-text">
					<p class="lan-lead">
						{addressView === 'qr'
							? 'Scan with the phone’s camera, or type the address.'
							: 'On the same Wi-Fi, devices open this address.'}
					</p>
					<ul class="ob-rows">
						{#each addresses as address (address)}
							<li>
								<span class="ob-truncate k-num">{address}</span>
								<Button
									variant="ghost"
									size="sm"
									icon={copied === address ? 'check' : 'copy'}
									iconOnly
									aria-label="Copy {address}"
									onclick={() => copy(address)}
								/>
							</li>
						{/each}
					</ul>
				</div>
			</div>
		{/if}
		{#if seen !== 'off'}
			<p class="reach" class:seen={seen === 'seen'} role="status">
				{#if seen === 'seen'}
					<Icon name="check" size={14} />A device on your network reached komiq.
				{:else}
					<span class="pulse"></span>Waiting for a device to connect…
				{/if}
			</p>
		{/if}
		<details class="help">
			<summary>A device cannot connect?</summary>
			<ul class="ob-points">
				<li>
					<Icon name="globe" size={14} />Put the device on the same Wi-Fi as this computer. A guest
					network or mobile data does not reach it.
				</li>
				<li>
					<Icon name="shield" size={14} />
					<div class="firewall">
						<span>{firewallHelp}</span>
						{#if firewall?.automatic}
							<Button variant="ghost" icon="shield" busy={opening} onclick={openFirewall}>
								Open the firewall for komiq
							</Button>
							{#if opened}
								<span class="done" role="status"><Icon name="check" size={14} />{opened}</span>
							{:else if openError}
								<span class="k-error" role="alert">{openError}</span>
							{/if}
						{/if}
					</div>
				</li>
			</ul>
		</details>
	{:else}
		<p class="k-hint">
			This computer is not on a network right now. Connect it to your Wi-Fi, then come back here.
		</p>
	{/if}
{/if}

<style>
	.toggle {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-4);
		padding: 14px 16px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.toggle-text {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.toggle-text p {
		margin: 0;
	}
	.toggle-title {
		font-size: var(--k-text-md);
		font-weight: 600;
		color: var(--k-text-1);
	}
	.toggle-detail {
		font-size: var(--k-text-sm);
		line-height: 1.5;
		color: var(--k-text-dim);
	}
	.lan {
		display: flex;
		align-items: center;
		gap: var(--k-space-4);
	}
	.lan-text {
		display: grid;
		gap: var(--k-space-3);
		flex: 1 1 auto;
		min-width: 0;
	}
	.lan-lead {
		margin: 0;
		font-size: var(--k-text-sm);
		line-height: 1.5;
		color: var(--k-text-2);
	}
	/* One line that turns from a quiet "waiting" into the success. Not drawn until the
	   server has answered once, so it takes no room on a server that cannot tell. */
	.reach {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		margin: 0;
		font-size: var(--k-text-sm);
		line-height: 1.5;
		color: var(--k-text-dim);
	}
	.reach.seen {
		color: var(--k-success);
	}
	.pulse {
		flex: none;
		width: 7px;
		height: 7px;
		margin-inline: 3px 4px;
		border-radius: 50%;
		background: var(--k-text-dim);
		animation: pulse 1.6s var(--k-ease, ease-in-out) infinite;
	}
	@keyframes pulse {
		50% {
			opacity: 0.25;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.pulse {
			animation: none;
		}
	}
	.help {
		font-size: var(--k-text-sm);
		color: var(--k-text-2);
	}
	.help summary {
		cursor: pointer;
		color: var(--k-text-1);
		font-weight: 600;
	}
	.help .ob-points {
		margin-top: var(--k-space-3);
		gap: var(--k-space-2);
	}
	.help .ob-points li {
		font-size: var(--k-text-sm);
	}
	/* The firewall line: its text, then the button and what it did, on the same edge. */
	.firewall {
		display: grid;
		justify-items: start;
		gap: var(--k-space-1);
		min-width: 0;
	}
	.firewall :global(.k-btn) {
		margin-left: -10px;
	}
	/* `.ob-points` drops its icons 3px to sit on the text line; not the button's own. */
	.firewall :global(.k-btn svg) {
		margin-top: 0;
	}
	.done {
		display: flex;
		align-items: flex-start;
		gap: 6px;
		color: var(--k-text-2);
	}
	.done :global(svg) {
		margin-top: 3px;
	}
	@media (max-width: 420px) {
		.lan {
			flex-direction: column;
			align-items: stretch;
		}
		.lan :global(.qr) {
			align-self: center;
		}
	}
</style>
