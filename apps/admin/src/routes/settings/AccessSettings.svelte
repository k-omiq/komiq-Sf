<script lang="ts">
	import { onMount } from 'svelte';
	import type { AccessStatus, CloudflareConnection } from '@komika/api';
	import { confirm } from '$lib/confirm.svelte';
	import { loadAccessStatus, removeCloudflareSetup, retryCloudflareSetup } from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Checklist from '$lib/components/Checklist.svelte';
	import CloudflareSetup, { type Phase } from '$lib/components/CloudflareSetup.svelte';
	import Icon from '$lib/components/Icon.svelte';

	// Settings › Access: how readers reach this server, what Cloudflare setup made, and
	// the connector. Re-run and Remove ask for the API token again; it is not kept.

	let status = $state<AccessStatus | null>(null);
	let loadError = $state<string | null>(null);
	let panel = $state<'setup' | 'rerun' | 'remove' | null>(null);
	let token = $state('');
	let tokenError = $state<string | null>(null);
	let acting = $state(false);
	let copied = $state(false);
	let alive = true;
	let pollId = 0;

	// The "Set up Cloudflare" form.
	let setup = $state<ReturnType<typeof CloudflareSetup>>();
	let setupPhase = $state<Phase>('form');
	let setupBusy = $state(false);
	let setupDomain = $state('');
	let setupConnection = $state<CloudflareConnection>('TUNNEL');

	onMount(() => {
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

	function open(which: 'setup' | 'rerun' | 'remove'): void {
		panel = which;
		token = '';
		tokenError = null;
		if (which === 'setup') {
			setupPhase = 'form';
			setupDomain = status?.domain ?? '';
		}
	}

	async function act(): Promise<void> {
		if (acting || !panel || panel === 'setup') return;
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
		try {
			await navigator.clipboard.writeText(text);
			copied = true;
			setTimeout(() => (copied = false), 1600);
		} catch {
			/* no clipboard */
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
					: status.mode === 'local'
						? 'Only my devices'
						: 'Not chosen yet'}
			</dd>
			{#if status.address}
				<dt>Address</dt>
				<dd class="row">
					<a class="k-link" href={status.address} target="_blank" rel="noreferrer"
						>{status.address}</a
					>
					<Button
						variant="ghost"
						size="sm"
						icon={copied ? 'check' : 'copy'}
						iconOnly
						aria-label="Copy address"
						onclick={() => status?.address && copy(status.address)}
					/>
				</dd>
			{/if}
			<dt>Cloudflare</dt>
			<dd>
				{#if cf}
					{connectionName(cf.connection)} for {cf.domain}{cf.zoneName && cf.zoneName !== cf.domain
						? ` (zone ${cf.zoneName})`
						: ''}
					{#if !cf.complete}<span class="status" data-tone="warning">Unfinished</span>{/if}
				{:else}
					Not set up
				{/if}
			</dd>
			{#if cf?.connection === 'TUNNEL'}
				<dt>Connector</dt>
				<dd class="stack">
					<span class="status" data-tone={connector.tone}>{connector.text}</span>
					{#if status.connectorMessage && status.connector !== 'running'}
						<span class="k-hint">{status.connectorMessage}</span>
					{/if}
				</dd>
			{:else if cf?.connection === 'PUBLIC_IP'}
				<dt>HTTPS</dt>
				<dd>
					<span class="status" data-tone={status.https ? 'success' : 'warning'}
						>{status.https ? 'Serving' : 'Not running'}</span
					>
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
				<div class="actions">
					<Button variant="ghost" disabled={setupBusy} onclick={() => (panel = null)}>
						{setupPhase === 'done' ? 'Close' : 'Cancel'}
					</Button>
					{#if setupPhase === 'form'}
						<Button variant="primary" busy={setupBusy} onclick={() => setup?.start()}>Set up</Button
						>
					{/if}
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
			<div class="actions start">
				{#if cf}
					<Button variant="ghost" icon="refresh" onclick={() => open('rerun')}
						>Run setup again</Button
					>
					{#if cf.removable}
						<Button variant="ghost" icon="trash" onclick={() => open('remove')}>Remove</Button>
					{/if}
				{:else}
					<Button variant="ghost" icon="globe" onclick={() => open('setup')}>
						Set up Cloudflare
					</Button>
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
