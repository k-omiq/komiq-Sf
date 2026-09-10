<script lang="ts" module>
	export type Phase = 'form' | 'running' | 'failed' | 'done' | 'manual';
</script>

<script lang="ts">
	import { onMount } from 'svelte';
	import type { AccessStatus, CloudflareConnection, CloudflareJob } from '@komika/api';
	import { serverAddress } from '$lib/config';
	import { localPort, manualSteps, tokenPermissions } from '$lib/cloudflare';
	import {
		checkCloudflareToken,
		loadAccessStatus,
		retryCloudflareSetup,
		startCloudflareSetup,
	} from '$lib/data';
	import Button from './Button.svelte';
	import Checklist from './Checklist.svelte';
	import Choice from './Choice.svelte';
	import Icon from './Icon.svelte';
	import '../onboarding.css';

	// Cloudflare in front of this server (plan B3b / section D). Used by the setup
	// wizard and by Settings › Access. The parent owns the primary button: it reads
	// `phase` and calls `start()`. The API token lives in this component only; it is
	// sent with each run and never saved.

	let {
		domain = $bindable(''),
		connection = $bindable<CloudflareConnection>('TUNNEL'),
		phase = $bindable<Phase>('form'),
		busy = $bindable(false),
		/** Show the last run (and follow a running one) when the component opens. */
		resume = true,
	}: {
		domain?: string;
		connection?: CloudflareConnection;
		phase?: Phase;
		busy?: boolean;
		resume?: boolean;
	} = $props();

	let token = $state('');
	let publicIp = $state('');
	let showToken = $state(false);
	let errors = $state<Partial<Record<'domain' | 'token' | 'ip' | 'form', string>>>({});
	let checking = $state(false);
	let found = $state<string | null>(null);
	let job = $state<CloudflareJob | null>(null);
	let retrying = $state(false);
	let alive = true;
	let pollId = 0;

	const port = localPort(serverAddress());
	const permissions = $derived(tokenPermissions(connection));
	const steps = $derived(manualSteps(connection, domain.trim(), port));

	onMount(() => {
		if (resume) {
			void loadAccessStatus()
				.then((s) => adopt(s, true))
				.catch(() => {});
		}
		return () => {
			alive = false;
			pollId++;
		};
	});

	/** Take over a run the server reports (e.g. after a reload). */
	function adopt(s: AccessStatus, initial = false): void {
		if (!s.job || s.job.kind !== 'setup') return;
		if (initial && !s.job.running && !s.job.failed && !s.cloudflare?.complete) return;
		job = s.job;
		if (s.cloudflare?.domain && !domain) domain = s.cloudflare.domain;
		if (s.cloudflare?.connection) connection = s.cloudflare.connection;
		if (s.job.running) void follow();
		else phase = s.job.failed ? 'failed' : 'done';
	}

	async function follow(): Promise<void> {
		const id = ++pollId;
		phase = 'running';
		busy = true;
		try {
			while (alive && id === pollId) {
				const s = await loadAccessStatus();
				if (id !== pollId) return;
				job = s.job;
				if (!s.job?.running) break;
				await new Promise((r) => setTimeout(r, 800));
			}
			if (id === pollId) phase = job?.failed ? 'failed' : 'done';
		} catch (err) {
			errors = { form: err instanceof Error ? err.message : 'Lost contact with the server.' };
			phase = 'failed';
		} finally {
			if (id === pollId) busy = false;
		}
	}

	function validate(): boolean {
		errors = {};
		const d = domain.trim();
		if (!d) errors.domain = 'Enter the domain readers will use.';
		else if (!d.includes('.') || /\s/.test(d))
			errors.domain = 'Enter a domain like read.example.com.';
		if (!token.trim()) errors.token = 'Paste the API token.';
		return Object.keys(errors).length === 0;
	}

	async function check(): Promise<void> {
		if (checking) return;
		found = null;
		if (!validate()) return;
		checking = true;
		try {
			const r = await checkCloudflareToken(token.trim(), domain.trim());
			found = `Found the zone ${r.zoneName} in ${r.accountName || 'your account'}.`;
		} catch (err) {
			placeError(err instanceof Error ? err.message : 'The token could not be checked.');
		} finally {
			checking = false;
		}
	}

	function placeError(message: string): void {
		const m = message.toLowerCase();
		if (m.includes('domain') || m.includes('zone')) errors = { domain: message };
		else if (m.includes('token')) errors = { token: message };
		else if (m.includes('address')) errors = { ip: message };
		else errors = { form: message };
	}

	/** Run the checklist. Called by the parent's primary button. */
	export async function start(): Promise<void> {
		if (busy || !validate()) return;
		busy = true;
		try {
			await startCloudflareSetup({
				token: token.trim(),
				domain: domain.trim(),
				connection,
				publicIp: connection === 'PUBLIC_IP' ? publicIp.trim() || null : null,
			});
		} catch (err) {
			busy = false;
			placeError(err instanceof Error ? err.message : 'Setup could not start.');
			return;
		}
		await follow();
	}

	async function retry(): Promise<void> {
		if (retrying) return;
		if (!token.trim()) {
			errors = { token: 'Paste the API token again to retry.' };
			return;
		}
		retrying = true;
		errors = {};
		try {
			await retryCloudflareSetup(token.trim());
		} catch (err) {
			retrying = false;
			errors = { form: err instanceof Error ? err.message : 'The retry could not start.' };
			return;
		}
		retrying = false;
		await follow();
	}

	export function showManual(): void {
		phase = 'manual';
	}

	export function showForm(): void {
		phase = 'form';
		job = null;
	}
</script>

{#snippet domainField()}
	<label class="k-field">
		<span class="k-label">Domain</span>
		<input
			class="k-input ob-input"
			type="text"
			inputmode="url"
			autocapitalize="none"
			spellcheck={false}
			placeholder="read.example.com"
			bind:value={domain}
			oninput={() => (found = null)}
			aria-invalid={errors.domain ? true : undefined}
		/>
		{#if errors.domain}
			<span class="k-error">{errors.domain}</span>
		{:else}
			<span class="k-hint">The domain, or its parent, must already be on Cloudflare.</span>
		{/if}
	</label>
{/snippet}

{#snippet tokenField()}
	<label class="k-field">
		<span class="k-label">Cloudflare API token</span>
		<span class="secret">
			<input
				class="k-input ob-input ob-mono"
				type={showToken ? 'text' : 'password'}
				autocomplete="off"
				spellcheck={false}
				bind:value={token}
				oninput={() => (found = null)}
				aria-invalid={errors.token ? true : undefined}
			/>
			<Button
				variant="ghost"
				size="sm"
				icon={showToken ? 'eye-off' : 'eye'}
				iconOnly
				aria-label={showToken ? 'Hide token' : 'Show token'}
				onclick={() => (showToken = !showToken)}
			/>
		</span>
		{#if errors.token}
			<span class="k-error">{errors.token}</span>
		{:else}
			<span class="k-hint">Used for this setup only. The server does not keep it.</span>
		{/if}
	</label>
{/snippet}

{#if phase === 'manual'}
	<div class="ob-stack">
		{@render domainField()}
		<ol class="manual">
			{#each steps as text, i (i)}
				<li><span class="num k-num">{i + 1}</span><span>{text}</span></li>
			{/each}
		</ol>
		<button class="ob-link left" type="button" onclick={showForm}>Set it up for me instead</button>
	</div>
{:else if phase === 'form'}
	<div class="ob-form">
		{@render domainField()}

		<div class="ob-choices" role="radiogroup" aria-label="Connection">
			<Choice
				icon="shield"
				title="Cloudflare Tunnel"
				detail="No open ports. This server connects out to Cloudflare. Recommended."
				selected={connection === 'TUNNEL'}
				onselect={() => (connection = 'TUNNEL')}
			/>
			<Choice
				icon="server"
				title="Public IP"
				detail="Cloudflare connects to this server on port 443 over HTTPS."
				selected={connection === 'PUBLIC_IP'}
				onselect={() => (connection = 'PUBLIC_IP')}
			/>
		</div>

		{#if connection === 'PUBLIC_IP'}
			<label class="k-field">
				<span class="k-label">Public IP address <span class="opt">Optional</span></span>
				<input
					class="k-input ob-input k-num"
					type="text"
					inputmode="decimal"
					spellcheck={false}
					placeholder="Detected automatically"
					bind:value={publicIp}
					aria-invalid={errors.ip ? true : undefined}
				/>
				{#if errors.ip}<span class="k-error">{errors.ip}</span>{/if}
			</label>
		{/if}

		<div class="token">
			{@render tokenField()}
			<div class="check">
				<Button variant="ghost" icon="check" busy={checking} onclick={check}>Check token</Button>
				{#if found}
					<p class="found" role="status"><Icon name="check" size={14} />{found}</p>
				{/if}
			</div>
		</div>

		<details class="help">
			<summary>How to create the token</summary>
			<ol>
				<li>
					In the Cloudflare dashboard, open My Profile › API Tokens and choose Create Token, then
					Create Custom Token.
				</li>
				<li>
					Add these permissions:
					<ul class="perms">
						{#each permissions as p (p.scope + p.group)}
							<li>
								<span class="k-muted">{p.scope}</span><span>{p.group}</span><span class="k-muted"
									>{p.level}</span
								>
							</li>
						{/each}
					</ul>
				</li>
				<li>
					Zone Resources: Include, Specific zone, your domain. Account Resources: your account.
				</li>
				<li>Create the token and paste it above.</li>
			</ol>
		</details>

		{#if errors.form}<p class="k-error" role="alert">{errors.form}</p>{/if}
		<button class="ob-link left" type="button" onclick={showManual}>I will do it by hand</button>
	</div>
{:else}
	<div class="ob-stack">
		{#if job}
			<Checklist
				steps={job.steps}
				label="Cloudflare setup"
				onretry={phase === 'failed' ? retry : undefined}
				{retrying}
			/>
		{/if}
		{#if phase === 'failed' && !token.trim()}
			{@render tokenField()}
		{/if}
		{#if errors.form}<p class="k-error" role="alert">{errors.form}</p>{/if}
		{#if phase === 'done'}
			<p class="k-hint ob-hint-row">
				<Icon name="globe" size={14} />Readers can now open https://{domain.trim()}.
			</p>
		{:else if phase === 'failed'}
			<button class="ob-link left" type="button" onclick={showForm}>Change the details</button>
		{/if}
	</div>
{/if}

<style>
	.secret {
		position: relative;
		display: block;
	}
	.secret input {
		width: 100%;
		padding-right: 44px;
	}
	.secret :global(.k-btn) {
		position: absolute;
		top: 50%;
		right: 6px;
		transform: translateY(-50%);
	}
	.opt {
		margin-left: 6px;
		font-weight: 400;
		color: var(--k-text-dim);
	}
	.token {
		display: grid;
		gap: var(--k-space-2);
	}
	.check {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-2) var(--k-space-3);
		margin-left: -10px;
	}
	.found {
		display: flex;
		align-items: center;
		gap: 6px;
		margin: 0;
		font-size: var(--k-text-sm);
		color: var(--k-text-2);
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
	.help ol,
	.manual {
		display: grid;
		gap: var(--k-space-2);
		margin: var(--k-space-3) 0 0;
		padding-left: 20px;
		line-height: 1.55;
	}
	.perms {
		display: grid;
		gap: 2px;
		margin: 6px 0 2px;
		padding: 0;
		list-style: none;
	}
	.perms li {
		display: grid;
		grid-template-columns: 72px minmax(0, 1fr) auto;
		gap: var(--k-space-3);
		padding: 6px 12px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.manual {
		margin: 0;
		padding: 0;
		list-style: none;
		font-size: var(--k-text-sm);
		color: var(--k-text-2);
	}
	.manual li {
		display: grid;
		grid-template-columns: 24px minmax(0, 1fr);
		gap: var(--k-space-2);
		padding: 10px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		overflow-wrap: anywhere;
	}
	.num {
		color: var(--k-text-dim);
	}
	.left {
		justify-self: start;
		padding-left: 0;
	}
</style>
