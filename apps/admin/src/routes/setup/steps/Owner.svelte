<script lang="ts">
	import { onMount } from 'svelte';
	import { adoptSession } from '$lib/auth.svelte';
	import {
		forgetServer,
		runsLocalServer,
		saveLocalServer,
		serverAddress,
		target,
	} from '$lib/config';
	import { createOwner, loadSetupNeedsTerminal } from '$lib/data';
	import { desktop, forgetSetupCode, handedSetupCode, handOffSetupCode } from '$lib/desktop';
	import { instance } from '$lib/instance.svelte';
	import { href } from '$lib/paths';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { back, labels, next, stepNumber } from '../wizard.svelte';

	let username = $state('');
	let email = $state('');
	let password = $state('');
	let confirm = $state('');
	const handedCode = handedSetupCode();
	let code = $state(handedCode);
	let codeFromApp = $state(handedCode !== '');
	let busy = $state(false);
	let errors = $state<
		Partial<Record<'username' | 'email' | 'password' | 'confirm' | 'code', string>>
	>({});
	let formError = $state<string | null>(null);
	// Plain HTTP from the internet: the server refuses to create the owner there, so
	// the form stays hidden and no password is sent. The form shows only on a definite
	// `false`: `undefined` is "still asking", `null` is "the server did not answer".
	let needsTerminal = $state<boolean | null | undefined>(undefined);

	async function checkConnection(): Promise<void> {
		needsTerminal = undefined;
		needsTerminal = await loadSetupNeedsTerminal();
	}

	/**
	 * The app's own server restarted since `/start` handed its setup code over: that code
	 * is spent, and the server may answer on another port. The app gives the new code out
	 * once, on the first status read after a restart. True when a new code arrived.
	 */
	async function refreshFromApp(): Promise<boolean> {
		if (!desktop.available || !runsLocalServer) return false;
		try {
			const status = await desktop.status();
			handOffSetupCode(status.setupCode);
			// The console talks to the server at its address, so follow it (the handed
			// code survives the reload).
			if (status.address && status.address !== serverAddress()) {
				saveLocalServer(status.address);
				location.reload();
				return false;
			}
			if (!status.setupCode) return false;
			code = status.setupCode;
			codeFromApp = true;
			return true;
		} catch {
			return false; // an older app: keep the code from `/start`
		}
	}

	onMount(() => {
		void checkConnection();
		void refreshFromApp();
	});

	/** Route a server message to the field it is about. */
	function placeError(message: string): void {
		const m = message.toLowerCase();
		if (m.includes('setup code') || m.includes('too many setup')) errors.code = message;
		else if (m.includes('username')) errors.username = message;
		else if (m.includes('email')) errors.email = message;
		else if (m.includes('password')) errors.password = message;
		else formError = message;
	}

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (busy) return;
		errors = {};
		formError = null;
		if (username.trim().length < 3) errors.username = 'At least 3 characters.';
		if (!email.includes('@')) errors.email = 'Enter an email address.';
		if (password.length < 8) errors.password = 'At least 8 characters.';
		else if (confirm !== password) errors.confirm = 'The passwords do not match.';
		if (!code.trim()) {
			errors.code = runsLocalServer
				? 'Close komiq and open it again to get a new setup code.'
				: 'Enter the setup code from the server log.';
		}
		if (Object.keys(errors).length) return;
		busy = true;
		const create = () =>
			createOwner({
				setupCode: code.trim(),
				username: username.trim(),
				email: email.trim(),
				password,
			});
		try {
			const session = await create().catch(async (err: unknown) => {
				// A refused code may be one from before a restart: take the app's new code
				// and try once more.
				const refused = err instanceof Error && err.message.toLowerCase().includes('setup code');
				if (!refused || !(await refreshFromApp())) throw err;
				return create();
			});
			adoptSession(session);
			forgetSetupCode();
			// The server has its owner now. The layout's route guard reads this cached
			// answer, so without it leaving the wizard would bounce straight back here.
			if (instance.info) instance.info = { ...instance.info, setupRequired: false };
			password = '';
			confirm = '';
			code = '';
			next();
		} catch (err) {
			placeError(err instanceof Error ? err.message : 'The account could not be created.');
		} finally {
			busy = false;
		}
	}

	function changeServer(): void {
		forgetServer();
		location.assign(href('/start'));
	}
</script>

{#snippet aside()}
	{#if target === 'desktop'}
		<button class="ob-link" type="button" onclick={changeServer}>Use a different server</button>
	{/if}
{/snippet}

<Onboarding
	step={stepNumber('owner')}
	steps={labels()}
	title="Create the owner account"
	lead="The owner manages the server, its sources and its users."
	{aside}
>
	{#if needsTerminal === undefined}
		<p class="k-hint" role="status">Checking the connection…</p>
	{:else if needsTerminal === null}
		<div class="ob-hint-row">
			<span class="k-error" role="alert">komiq could not check this connection.</span>
			<Button variant="ghost" icon="refresh" onclick={checkConnection}>Try again</Button>
		</div>
	{:else if needsTerminal}
		<div class="ob-stack">
			<div class="k-notice" data-tone="warning" role="note">
				<Icon name="alert" />
				<div class="k-notice__body">
					This connection is not encrypted. People on the network between you and the server could
					see the owner password, so the server does not accept it here.
				</div>
			</div>
			<p>Create the owner in a terminal on the server instead. With Docker:</p>
			<p><code class="ob-code">docker compose exec server komika-server setup</code></p>
			<p>Without Docker:</p>
			<p><code class="ob-code">komika-server setup</code></p>
			<p class="k-hint">
				It asks the same questions as these screens. It can also turn on encryption. Then come back
				and sign in.
			</p>
		</div>
	{:else}
		<form id="owner" class="ob-form" onsubmit={submit} novalidate aria-busy={busy}>
			<div class="k-field">
				<label class="k-label" for="owner-username">Username</label>
				<input
					id="owner-username"
					aria-describedby="owner-username-msg"
					class="k-input ob-input"
					type="text"
					autocomplete="username"
					autocapitalize="none"
					spellcheck={false}
					bind:value={username}
					aria-invalid={errors.username ? true : undefined}
				/>
				{#if errors.username}<span id="owner-username-msg" class="k-error">{errors.username}</span
					>{/if}
			</div>
			<div class="k-field">
				<label class="k-label" for="owner-email">Email</label>
				<input
					id="owner-email"
					aria-describedby="owner-email-msg"
					class="k-input ob-input"
					type="email"
					autocomplete="email"
					bind:value={email}
					aria-invalid={errors.email ? true : undefined}
				/>
				{#if errors.email}<span id="owner-email-msg" class="k-error">{errors.email}</span>{/if}
			</div>
			<div class="ob-pair">
				<div class="k-field">
					<label class="k-label" for="owner-password">Password</label>
					<input
						id="owner-password"
						aria-describedby="owner-password-msg"
						class="k-input ob-input"
						type="password"
						autocomplete="new-password"
						bind:value={password}
						aria-invalid={errors.password ? true : undefined}
					/>
					{#if errors.password}<span id="owner-password-msg" class="k-error">{errors.password}</span
						>{/if}
				</div>
				<div class="k-field">
					<label class="k-label" for="owner-confirm">Confirm</label>
					<input
						id="owner-confirm"
						aria-describedby="owner-confirm-msg"
						class="k-input ob-input"
						type="password"
						autocomplete="new-password"
						bind:value={confirm}
						aria-invalid={errors.confirm ? true : undefined}
					/>
					{#if errors.confirm}<span id="owner-confirm-msg" class="k-error">{errors.confirm}</span
						>{/if}
				</div>
			</div>
			{#if codeFromApp && !errors.code}
				<p class="k-hint ob-hint-row">
					<Icon name="check" size={14} />Setup code filled in by the app.
				</p>
			{:else}
				<div class="k-field">
					<label class="k-label" for="owner-code">Setup code</label>
					<input
						id="owner-code"
						aria-describedby="owner-code-msg"
						class="k-input ob-input ob-mono"
						type="text"
						autocomplete="one-time-code"
						autocapitalize="characters"
						spellcheck={false}
						placeholder="XXXX-XXXX-XXXX"
						bind:value={code}
						aria-invalid={errors.code ? true : undefined}
					/>
					{#if errors.code}
						<span id="owner-code-msg" class="k-error">{errors.code}</span>
					{:else}
						<span id="owner-code-msg" class="k-hint">
							{#if runsLocalServer}
								The app fills this in. Close komiq and open it again to get a new code.
							{:else}
								Printed in the server log when it started. It proves you run this server.
							{/if}
						</span>
					{/if}
				</div>
			{/if}
			{#if formError}<p class="k-error" role="alert">{formError}</p>{/if}
		</form>
	{/if}
	{#snippet footer()}
		<Button variant="ghost" icon="arrow-left" onclick={back}>Back</Button>
		{#if needsTerminal}
			<Button variant="primary" onclick={() => location.reload()}>I ran it</Button>
		{:else if needsTerminal === false}
			<Button variant="primary" type="submit" form="owner" {busy}>
				{busy ? 'Creating…' : 'Create account'}
			</Button>
		{/if}
	{/snippet}
</Onboarding>
