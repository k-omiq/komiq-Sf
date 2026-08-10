<script lang="ts">
	import { onMount } from 'svelte';
	import { saveLocalServer, serverAddress } from '$lib/config';
	import { bridgeError, desktop, type LocalNetwork } from '$lib/desktop';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import Qr from '$lib/components/Qr.svelte';
	import Switch from '$lib/components/Switch.svelte';
	import { back, copyText, labels, next, stepNumber, wizard } from '../wizard.svelte';

	// B3a, desktop app running its own server: let other devices at home open the
	// reader. The switch restarts the server on every interface (or back on loopback
	// only) through the bridge; nothing is reachable from outside until it is on.

	let net = $state<LocalNetwork | null>(null);
	let busy = $state(false);
	let error = $state<string | null>(null);
	let copied = $state<string | null>(null);

	onMount(async () => {
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
			wizard.extra = { ...wizard.extra, lan: now.enabled };
			// The port only moves when the old one was taken meanwhile. The console talks to
			// the server at its address, so follow it (progress is saved; this resumes here).
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

	// Back never returns to the owner form: that account exists now.
	const previous = $derived(wizard.order[stepNumber('lan') - 2]?.id);
	const canGoBack = $derived(!!previous && previous !== 'owner');

	const enabled = $derived(net?.enabled ?? false);
	const addresses = $derived(net?.addresses ?? []);
</script>

<Onboarding
	step={stepNumber('lan')}
	steps={labels()}
	title="Home network"
	lead="Read on your phone or tablet while komiq runs on this computer."
>
	<div class="toggle">
		<div class="toggle-text">
			<p class="toggle-title">Allow other devices</p>
			<p class="toggle-detail">
				Devices on the same Wi-Fi can open the reader. Off keeps komiq on this computer only.
			</p>
		</div>
		<Switch
			checked={enabled}
			label="Allow other devices"
			disabled={busy || !net}
			onchange={toggle}
		/>
	</div>

	{#if busy}
		<p class="k-hint ob-hint-row" role="status">
			<span class="k-spin"><Icon name="refresh" size={14} /></span>Restarting the server…
		</p>
	{:else if error}
		<p class="k-error" role="alert">{error}</p>
	{:else if enabled}
		{#if addresses.length}
			<div class="lan">
				<Qr value={addresses[0]} label="QR code for {addresses[0]}" />
				<div class="lan-text">
					<p class="lan-lead">Scan with the phone’s camera, or type the address.</p>
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
			<p class="k-hint ob-hint-row">
				<Icon name="shield" size={14} />If a device cannot connect, allow komiq through this
				computer’s firewall.
			</p>
		{:else}
			<p class="k-hint">
				This computer is not on a network right now. Connect it to your Wi-Fi, then come back to
				this step.
			</p>
		{/if}
	{/if}

	{#snippet footer()}
		{#if canGoBack}
			<Button variant="ghost" icon="arrow-left" onclick={back} disabled={busy}>Back</Button>
		{:else}
			<span></span>
		{/if}
		<Button variant="primary" onclick={next} disabled={busy}>Continue</Button>
	{/snippet}
</Onboarding>

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
