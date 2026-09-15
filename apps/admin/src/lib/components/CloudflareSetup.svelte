<script lang="ts" module>
	export type Phase = 'form' | 'running' | 'failed' | 'done' | 'paused' | 'manual';
</script>

<script lang="ts">
	import { onMount } from 'svelte';
	import type {
		AccessPreflight,
		AccessStatus,
		CloudflareConnection,
		CloudflareJob,
	} from '@komika/api';
	import { runsLocalServer, serverAddress } from '$lib/config';
	import {
		automaticSteps,
		CLOUDFLARE_DASHBOARD,
		localPort,
		manualSteps,
		plainDomain,
		tokenPermissions,
		tokenTemplateUrl,
	} from '$lib/cloudflare';
	import {
		checkCloudflareToken,
		loadAccessPreflight,
		loadAccessStatus,
		retryCloudflareSetup,
		setAccessMode,
		startCloudflareSetup,
	} from '$lib/data';
	import { cloudflareLines } from '$lib/preflight';
	import Button from './Button.svelte';
	import Checklist from './Checklist.svelte';
	import Choice from './Choice.svelte';
	import Icon from './Icon.svelte';
	import Preflight from './Preflight.svelte';
	import '../onboarding.css';

	// Cloudflare in front of this server (plan B3b / section D). Used by the setup
	// wizard and by Settings › Access. The parent owns the primary button: it reads
	// `phase` and calls `start()`. The API token lives in this component only; it is
	// sent with each run and never saved.
	//
	// The form says who does what: three things the owner does once at Cloudflare (an
	// account, the domain, a token), and the list of what setup then does by itself. Both
	// are one folded line each, so the fields stay in view on a short window.
	//
	// `paused`: setup is done, and "Only my devices" or the direct mode turned it off
	// since. What it made at Cloudflare is still there, so `turnOn()` needs no token.

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
	// Permission rows the checked token seems to lack. Advice: it never blocks "Set up".
	let missing = $state<string[]>([]);
	// What the server knows about its machine; null on a server that cannot say.
	let preflight = $state<AccessPreflight | null>(null);
	let job = $state<CloudflareJob | null>(null);
	// What readers type once setup is done, as the server stored it.
	let online = $state<string | null>(null);
	let retrying = $state(false);
	// A retry needs the token again when this visit has none (it is never saved). Once
	// asked for, the field stays for the whole failed phase, whatever is typed into it.
	let askToken = $state(false);
	$effect(() => {
		if (phase !== 'failed') askToken = false;
		else if (!token.trim()) askToken = true;
	});
	let alive = true;
	let pollId = 0;

	// The desktop app's own server answers on this computer only. It cannot take port 443
	// from the internet, so the tunnel is its one connection.
	const tunnelOnly = runsLocalServer;
	$effect(() => {
		if (tunnelOnly && connection !== 'TUNNEL') connection = 'TUNNEL';
	});

	const port = localPort(serverAddress());
	const permissions = $derived(tokenPermissions(connection));
	// The token page link fills these rows in; the owner adds the others there.
	const filledRows = $derived(permissions.filter((p) => p.templateKey));
	const handRows = $derived(permissions.filter((p) => !p.templateKey));
	const tokenPage = $derived(tokenTemplateUrl(connection));
	const steps = $derived(manualSteps(connection, plainDomain(domain), port));
	const httpsPort = $derived(preflight?.httpsPort ?? 443);
	const automatic = $derived(automaticSteps(connection, httpsPort, preflight?.cloudflared ?? null));
	const serverLines = $derived(cloudflareLines(preflight, connection, tunnelOnly));

	/** Ask the server again: the owner may have stopped the program that held a port. */
	async function readServer(): Promise<void> {
		const p = await loadAccessPreflight();
		if (alive && p) preflight = p;
	}

	onMount(() => {
		void readServer();
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
		// Set up, then turned off: readers cannot open the domain, whatever the run says.
		const off = !!s.cloudflare?.complete && s.mode !== 'internet';
		online = s.address;
		if (!s.job) {
			// A restarted server forgot the run, not what it made: a finished setup is
			// still done, with the domain and connection it was made for.
			if (!initial || !s.cloudflare?.complete) return;
			domain = s.cloudflare.domain;
			if (s.cloudflare.connection) connection = s.cloudflare.connection;
			phase = off ? 'paused' : 'done';
			return;
		}
		if (s.job.kind !== 'setup') return;
		if (initial && !s.job.running && !s.job.failed && !s.cloudflare?.complete) return;
		job = s.job;
		if (s.cloudflare?.domain && (off || !domain)) domain = s.cloudflare.domain;
		if (s.cloudflare?.connection) connection = s.cloudflare.connection;
		if (s.job.running) void follow();
		else phase = s.job.failed ? 'failed' : off ? 'paused' : 'done';
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
				online = s.address;
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

	/** A checked token's result is about that token, domain and connection only. */
	function forgetCheck(): void {
		found = null;
		missing = [];
	}

	function choose(value: CloudflareConnection): void {
		if (value === connection) return;
		forgetCheck();
		connection = value;
		if (preflight) void readServer();
	}

	async function check(): Promise<void> {
		if (checking) return;
		forgetCheck();
		if (!validate()) return;
		checking = true;
		const asked = connection;
		try {
			const r = await checkCloudflareToken(token.trim(), domain.trim(), asked);
			if (asked !== connection) return;
			found = `Found the zone ${r.zoneName} in ${r.accountName || 'your account'}.`;
			missing = r.missing;
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

	/** Turn a paused setup on again. Called by the parent's primary button. */
	export async function turnOn(): Promise<void> {
		if (busy) return;
		busy = true;
		errors = {};
		try {
			online = (await setAccessMode('internet', domain.trim())).address;
			phase = 'done';
		} catch (err) {
			errors = { form: err instanceof Error ? err.message : 'Cloudflare could not be turned on.' };
		} finally {
			busy = false;
		}
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
			oninput={forgetCheck}
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
				oninput={forgetCheck}
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
	<div class="ob-form form">
		<section class="who" aria-labelledby="cf-once">
			<details class="help">
				<summary
					><h2 id="cf-once" class="ob-section-title">You do this once</h2>
					<span class="count">account, domain, token</span></summary
				>
				<ol class="once">
					<li><span class="num k-num">1</span><span>A Cloudflare account. It is free.</span></li>
					<li>
						<span class="num k-num">2</span><span
							>Your domain, added to Cloudflare. You point the domain’s nameservers at Cloudflare,
							where you bought the domain. Cloudflare shows the steps.</span
						>
					</li>
					<li>
						<span class="num k-num">3</span><span
							>An API token, so komiq may set things up for you. You make it below.</span
						>
					</li>
				</ol>
			</details>
			<a class="k-link dashboard" href={CLOUDFLARE_DASHBOARD} target="_blank" rel="noreferrer"
				>Open Cloudflare</a
			>
			<details class="help">
				<summary
					>komiq does the rest <span class="count k-num">{automatic.length} steps</span></summary
				>
				<ul class="rest" aria-label="What komiq does">
					{#each automatic as text (text)}
						<li><Icon name="check" size={14} /><span>{text}</span></li>
					{/each}
				</ul>
			</details>
		</section>

		{@render domainField()}

		<div class="connection">
			{#if tunnelOnly}
				<p class="k-hint">
					The app uses a Cloudflare Tunnel. It needs no open ports. For a public IP, or without a
					domain, run komiq on a server.
				</p>
			{:else}
				<div class="ob-choices" role="radiogroup" aria-label="Connection">
					<Choice
						icon="shield"
						title="Cloudflare Tunnel (recommended)"
						detail="No open ports. This server connects out to Cloudflare."
						selected={connection === 'TUNNEL'}
						onselect={() => choose('TUNNEL')}
					/>
					<Choice
						icon="server"
						title="Public IP"
						detail="Cloudflare reaches this server on port {httpsPort} (HTTPS)."
						selected={connection === 'PUBLIC_IP'}
						onselect={() => choose('PUBLIC_IP')}
					/>
				</div>
			{/if}
			<Preflight lines={serverLines} />
		</div>

		{#if connection === 'PUBLIC_IP'}
			<label class="k-field">
				<span class="k-label">Public IP address <span class="opt">Optional</span></span>
				<input
					class="k-input ob-input k-num"
					type="text"
					inputmode="decimal"
					spellcheck={false}
					placeholder={preflight?.publicIp ?? 'Detected automatically'}
					bind:value={publicIp}
					aria-invalid={errors.ip ? true : undefined}
				/>
				{#if errors.ip}
					<span class="k-error">{errors.ip}</span>
				{:else if preflight?.publicIp}
					<span class="k-hint"
						>Leave it empty to use {preflight.publicIp}, the address komiq found for this server.</span
					>
				{/if}
			</label>
		{/if}

		<div class="token">
			{@render tokenField()}
			<div class="check">
				<Button variant="ghost" icon="external" href={tokenPage} target="_blank" rel="noreferrer"
					>Open the token page</Button
				>
				<Button variant="ghost" icon="check" busy={checking} onclick={check}>Check token</Button>
			</div>
			{#if found}
				<p class="found" role="status"><Icon name="check" size={14} />{found}</p>
			{/if}
			{#if missing.length}
				<div class="k-notice" data-tone="warning" role="status">
					<Icon name="alert" />
					<div class="k-notice__body">
						This token may lack: {missing.join(', ')}. You can still run setup. If a step fails, add
						the permission to the token and retry.
					</div>
				</div>
			{/if}
		</div>

		<details class="help">
			<summary>How to create the token</summary>
			<ol>
				<li>
					Choose “Open the token page”. Cloudflare opens a new token named komiq, with these
					permissions filled in:
					<ul class="perms" aria-label="Filled in by the link">
						{#each filledRows as p (p.scope + p.group)}
							<li>
								<span class="k-muted">{p.scope}</span><span>{p.group}</span><span class="k-muted"
									>{p.level}</span
								>
							</li>
						{/each}
					</ul>
				</li>
				<li>
					Add these permissions by hand, on the same page:
					<ul class="perms" aria-label="Add by hand">
						{#each handRows as p (p.scope + p.group)}
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
{:else if phase === 'paused'}
	<div class="ob-stack">
		<p class="k-hint ob-hint-row">
			<Icon name="pause" size={14} />Cloudflare is set up for {domain} and turned off. Readers cannot
			open it now.
		</p>
		{#if errors.form}<p class="k-error" role="alert">{errors.form}</p>{/if}
		<button class="ob-link left" type="button" onclick={showForm}>Set it up again</button>
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
		{#if phase === 'failed' && askToken}
			{@render tokenField()}
		{/if}
		{#if errors.form}<p class="k-error" role="alert">{errors.form}</p>{/if}
		{#if phase === 'done'}
			{#if online}
				<p class="k-hint ob-hint-row">
					<Icon name="globe" size={14} />Readers can now open {online}.
				</p>
			{/if}
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
		gap: var(--k-space-1) var(--k-space-2);
		margin-left: -10px;
	}
	/* Tighter than the other first-run forms: three fields and a choice share a short
	   window with the two lines on who does what. */
	.form {
		gap: var(--k-space-3);
	}
	/* Who does what: one folded line for the owner's part, one for komiq's. */
	.who {
		position: relative;
		display: grid;
		gap: var(--k-space-1);
	}
	/* On the first line, at its right edge. The line leaves it room. */
	.dashboard {
		position: absolute;
		top: 0;
		right: 0;
		font-size: var(--k-text-xs);
		line-height: 20px;
	}
	.who summary {
		line-height: 20px;
	}
	.who details:first-of-type summary {
		padding-right: 104px;
	}
	.who summary h2 {
		display: inline;
	}
	.who .once,
	.who .rest {
		display: grid;
		gap: 4px;
		margin: var(--k-space-2) 0;
		padding: 0;
		list-style: none;
		font-size: var(--k-text-sm);
		line-height: 1.5;
		color: var(--k-text-2);
	}
	.once li,
	.rest li {
		display: grid;
		grid-template-columns: 14px minmax(0, 1fr);
		gap: var(--k-space-2);
	}
	.who .rest {
		color: var(--k-text-dim);
	}
	.rest :global(svg) {
		margin-top: 3px;
	}
	.count {
		margin-left: 6px;
		font-weight: 400;
		color: var(--k-text-dim);
		white-space: nowrap;
	}
	.connection {
		display: grid;
		gap: var(--k-space-2);
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
