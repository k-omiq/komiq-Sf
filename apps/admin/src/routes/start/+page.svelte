<script lang="ts">
	import { onDestroy } from 'svelte';
	import { href } from '$lib/paths';
	import { goto } from '$app/navigation';
	import { runsLocalServer, saveLocalServer, saveServer, target } from '$lib/config';
	import { bridgeError, desktop, handOffSetupCode, type LocalStage } from '$lib/desktop';
	import Button from '$lib/components/Button.svelte';
	import Choice from '$lib/components/Choice.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';

	// Desktop first run: run the server here, or point the app at one that already
	// runs. Either way the choice is saved and the app reloads into setup (a fresh
	// server) or sign-in. An app that runs its own server comes back here on every
	// launch and starts it straight away.
	// Only the desktop app chooses its server; other builds talk to a fixed one.
	if (target !== 'desktop') void goto(href('/'), { replaceState: true });

	type View = 'choose' | 'local' | 'connect';
	let view = $state<View>('choose');

	function finish(origin: string, local: boolean, setupCode?: string | null): void {
		if (local) saveLocalServer(origin);
		else saveServer(origin);
		handOffSetupCode(setupCode);
		// The backend is built once per load; reload so it targets the new server.
		location.assign(href('/'));
	}

	// ---- run on this computer ----------------------------------------------------

	type StepState = 'waiting' | 'running' | 'done' | 'failed';
	const localSteps: { stage: Exclude<LocalStage, 'ready'>; label: string }[] = [
		{ stage: 'database', label: 'Database' },
		{ stage: 'engine', label: 'Source engine' },
		{ stage: 'server', label: 'Server' },
	];
	let stage = $state<LocalStage>('database');
	let localError = $state<string | null>(null);
	let log = $state<string | null>(null);
	let warnings = $state<string[]>([]);
	// A start that finished with warnings waits here until the owner has read them.
	let started = $state<{ address: string; setupCode?: string | null } | null>(null);
	let poll: ReturnType<typeof setTimeout> | null = null;
	// One status loop at a time: Back and then Run starts a new one and ends the old.
	let watch = 0;

	function stepState(s: Exclude<LocalStage, 'ready'>): StepState {
		const order = ['database', 'engine', 'server', 'ready'];
		const at = order.indexOf(stage);
		const mine = order.indexOf(s);
		if (mine < at) return 'done';
		if (mine > at) return 'waiting';
		return localError ? 'failed' : 'running';
	}

	async function startLocal(): Promise<void> {
		const mine = ++watch;
		if (poll) clearTimeout(poll);
		view = 'local';
		stage = 'database';
		localError = null;
		log = null;
		warnings = [];
		started = null;
		try {
			await desktop.start();
			if (mine === watch) void tick(mine);
		} catch (err) {
			if (mine === watch) localError = bridgeError(err, 'Could not start.');
		}
	}

	if (desktop.available && runsLocalServer) void startLocal();

	async function tick(mine: number): Promise<void> {
		try {
			const status = await desktop.status();
			if (mine !== watch) return;
			stage = status.stage;
			warnings = status.warnings ?? [];
			if (status.error) {
				localError = status.error;
				return;
			}
			if (status.stage === 'ready' && status.address) {
				// The setup code is handed out once: keep it while the warnings are read.
				if (warnings.length > 0) {
					started = { address: status.address, setupCode: status.setupCode };
					return;
				}
				finish(status.address, true, status.setupCode);
				return;
			}
		} catch (err) {
			if (mine === watch) localError = bridgeError(err, 'Lost contact with the server process.');
			return;
		}
		poll = setTimeout(() => void tick(mine), 700);
	}

	async function showLog(): Promise<void> {
		try {
			log = (await desktop.log()) || 'The log is empty.';
		} catch {
			log = 'The log could not be read.';
		}
	}

	onDestroy(() => {
		watch++;
		if (poll) clearTimeout(poll);
	});

	// ---- connect to a server -----------------------------------------------------

	let address = $state('');
	let addressError = $state<string | null>(null);
	let checking = $state(false);
	let found = $state<{
		origin: string;
		name: string;
		version: string;
		setupRequired: boolean;
	} | null>(null);

	/** "192.168.1.20:8080", "https://komiq.example.com/graphql" → a bare origin. */
	function normalize(raw: string): string | null {
		let v = raw.trim();
		if (!v) return null;
		if (!/^https?:\/\//i.test(v)) {
			// A home-network address (an IP, `localhost`, a `.local` name, or anything with
			// a port) gets `http://`. A plain domain gets `https://`: a password must not
			// go to a public name in the clear because nobody typed the scheme.
			const host = v.split(/[/?#]/)[0];
			const home =
				/:\d+$/.test(host) ||
				/^(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-f:.]+\]|localhost)$/i.test(host) ||
				/\.local$/i.test(host);
			v = `${home ? 'http' : 'https'}://${v}`;
		}
		try {
			return new URL(v).origin;
		} catch {
			return null;
		}
	}

	async function check(e?: SubmitEvent): Promise<void> {
		e?.preventDefault();
		if (checking) return;
		found = null;
		addressError = null;
		const origin = normalize(address);
		if (!origin) {
			addressError = 'Enter an address like 192.168.1.20:8080 or https://komiq.example.com.';
			return;
		}
		checking = true;
		try {
			const health = await fetch(`${origin}/health`);
			if (!health.ok) throw new Error();
			const res = await fetch(`${origin}/graphql`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					query: '{ instance { setupRequired name version } }',
				}),
			});
			const json = (await res.json()) as {
				data?: { instance?: { setupRequired: boolean; name: string; version: string } };
			};
			const info = json.data?.instance;
			if (!info) {
				addressError = 'This server is too old for the desktop app. Update it and try again.';
				return;
			}
			found = { origin, ...info };
		} catch {
			addressError = 'No komiq server answered at this address. Check it is running and reachable.';
		} finally {
			checking = false;
		}
	}
</script>

<svelte:head><title>Welcome - komiq</title></svelte:head>

{#if view === 'choose'}
	<Onboarding
		title="Welcome to komiq"
		lead="Choose where your library lives. You can change this later."
	>
		<div class="choices">
			<Choice
				role="button"
				icon="monitor"
				title="Run komiq on this computer"
				detail="Starts the server and the source engine here. Other devices at home can connect to it."
				disabled={!desktop.available}
				note="Desktop app only"
				onselect={startLocal}
			/>
			<Choice
				role="button"
				icon="server"
				title="Connect to a server I already run"
				detail="Use komiq on a home server or a VPS you set up before."
				onselect={() => (view = 'connect')}
			/>
		</div>
	</Onboarding>
{:else if view === 'local'}
	<Onboarding
		title={localError
			? 'komiq could not start'
			: started
				? 'komiq started, with a note'
				: 'Starting komiq'}
		lead={localError
			? 'One step failed. Retry, or read the log for the cause.'
			: started
				? 'komiq is running. Read this before you continue.'
				: 'This takes a few seconds the first time.'}
	>
		<ul class="steps">
			{#each localSteps as s (s.stage)}
				{@const state = stepState(s.stage)}
				<li data-state={state}>
					<span class="dot" aria-hidden="true">
						{#if state === 'done'}
							<Icon name="check" size={14} />
						{:else if state === 'running'}
							<span class="k-spin"><Icon name="refresh" size={14} /></span>
						{:else if state === 'failed'}
							<Icon name="x" size={14} />
						{/if}
					</span>
					<span class="label">{s.label}</span>
					<span class="state">
						{state === 'done'
							? 'Running'
							: state === 'running'
								? 'Starting…'
								: state === 'failed'
									? 'Failed'
									: 'Waiting'}
					</span>
				</li>
			{/each}
		</ul>
		{#if warnings.length > 0}
			<ul class="notes" role="status">
				{#each warnings as warning (warning)}
					<li>{warning}</li>
				{/each}
			</ul>
		{/if}
		{#if localError}
			<p class="k-error cause" role="alert">{localError}</p>
		{/if}
		{#if log}<pre class="log">{log}</pre>{/if}
		{#snippet footer()}
			<Button variant="ghost" icon="arrow-left" onclick={() => (view = 'choose')}>Back</Button>
			{#if localError}
				<div class="row">
					{#if !log}<Button variant="ghost" onclick={showLog}>Show log</Button>{/if}
					<Button variant="primary" icon="refresh" onclick={startLocal}>Retry</Button>
				</div>
			{:else if started}
				<div class="row">
					{#if !log}<Button variant="ghost" onclick={showLog}>Show log</Button>{/if}
					<Button
						variant="primary"
						onclick={() => started && finish(started.address, true, started.setupCode)}
						>Continue</Button
					>
				</div>
			{/if}
		{/snippet}
	</Onboarding>
{:else}
	<Onboarding title="Connect to your server" lead="Enter the address you use to reach it.">
		<form id="connect" class="form" onsubmit={check} novalidate>
			<label class="k-field">
				<span class="k-label">Server address</span>
				<input
					class="k-input"
					type="text"
					inputmode="url"
					autocapitalize="none"
					autocomplete="url"
					spellcheck={false}
					placeholder="192.168.1.20:8080"
					bind:value={address}
					oninput={() => {
						found = null;
						addressError = null;
					}}
					aria-invalid={addressError ? true : undefined}
					aria-describedby="address-msg"
				/>
				{#if addressError}
					<span id="address-msg" class="k-error" role="alert">{addressError}</span>
				{:else}
					<span id="address-msg" class="k-hint"
						>On a home network this is usually an IP and port.</span
					>
				{/if}
			</label>
		</form>
		{#if found}
			<div class="found" role="status">
				<Icon name="check" size={16} />
				<div>
					<p class="found-name">{found.name} <span class="k-num">{found.version}</span></p>
					<p class="found-detail">
						{found.setupRequired ? 'New server. Setup comes next.' : 'Ready. Sign in next.'}
					</p>
				</div>
			</div>
		{/if}
		{#snippet footer()}
			<Button variant="ghost" icon="arrow-left" onclick={() => (view = 'choose')}>Back</Button>
			{#if found}
				<Button variant="primary" onclick={() => found && finish(found.origin, false)}
					>Continue</Button
				>
			{:else}
				<Button variant="primary" type="submit" form="connect" busy={checking}>
					{checking ? 'Checking…' : 'Check'}
				</Button>
			{/if}
		{/snippet}
	</Onboarding>
{/if}

<style>
	.choices {
		display: grid;
		gap: var(--k-space-2);
	}
	.form {
		display: grid;
		gap: var(--k-space-4);
	}
	.k-input {
		height: var(--k-control-lg);
		font-size: var(--k-text-md);
	}
	.steps {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.steps li {
		display: grid;
		grid-template-columns: auto 1fr auto;
		align-items: center;
		gap: var(--k-space-3);
		padding: 12px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.dot {
		display: grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
	[data-state='done'] .dot {
		color: var(--k-success);
	}
	[data-state='failed'] .dot {
		color: var(--k-danger);
	}
	.label {
		font-size: var(--k-text-md);
		color: var(--k-text-1);
	}
	.state {
		font-size: var(--k-text-xs);
		color: var(--k-text-dim);
	}
	[data-state='failed'] .state {
		color: var(--k-danger);
	}
	.log {
		max-height: 220px;
		overflow: auto;
		margin: 0;
		padding: 12px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-text-2);
		font-size: var(--k-text-xs);
		line-height: 1.6;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	/* Warnings of a start that went on: soft fill, no outline, like the steps. */
	.notes {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.notes li {
		padding: 12px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-text-2);
		font-size: var(--k-text-sm);
		line-height: 1.5;
		overflow-wrap: anywhere;
	}
	.cause {
		white-space: pre-line;
		overflow-wrap: anywhere;
	}
	.row {
		display: flex;
		gap: var(--k-space-2);
	}
	.found {
		display: flex;
		align-items: flex-start;
		gap: var(--k-space-3);
		padding: 12px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-success);
	}
	.found p {
		margin: 0;
	}
	.found-name {
		font-size: var(--k-text-md);
		font-weight: 600;
		color: var(--k-text-bright);
	}
	.found-name .k-num {
		font-weight: 400;
		color: var(--k-text-dim);
	}
	.found-detail {
		font-size: var(--k-text-sm);
		color: var(--k-text-dim);
	}
</style>
