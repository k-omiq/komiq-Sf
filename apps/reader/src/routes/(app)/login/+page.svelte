<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { auth, login, register } from '$lib/auth.svelte';
	import { loadInstance, registrationOpen } from '$lib/instance.svelte';
	import { apiOrigin, canChooseServer } from '$lib/config';
	import { connectScreen } from '$lib/server-connect.svelte';
	import TurnstileWidget from '$lib/components/TurnstileWidget.svelte';

	type Mode = 'login' | 'register';
	let mode = $state<Mode>('login');

	// The server can close sign-up (self-host). Until it says so the page looks as it
	// always has; a server that cannot tell us keeps sign-up visible.
	$effect(() => {
		void loadInstance();
	});
	const canRegister = $derived(registrationOpen());
	$effect(() => {
		if (!canRegister && mode === 'register') mode = 'login';
	});

	let username = $state('');
	let email = $state('');
	let password = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);
	let check = $state<TurnstileWidget>();

	// Native app: which server this signs in to, with a way to pick another.
	const serverHost = apiOrigin.replace(/^https?:\/\//, '');

	const redirectTo = $derived(page.url.searchParams.get('redirect') || '/');

	// Already signed in → bounce out.
	$effect(() => {
		if (auth.user) goto(redirectTo, { replaceState: true });
	});

	function switchMode(next: Mode): void {
		mode = next;
		error = null;
	}

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (busy) return;
		error = null;
		busy = true;
		try {
			const turnstileToken = await check?.token();
			if (mode === 'login') {
				await login(username.trim(), password, turnstileToken);
			} else {
				await register({
					username: username.trim(),
					email: email.trim(),
					password,
					turnstileToken,
				});
			}
			await goto(redirectTo, { replaceState: true });
		} catch (err) {
			error = err instanceof Error ? err.message : 'Something went wrong. Please try again.';
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head>
	<title>{mode === 'login' ? 'Sign in' : 'Create account'} · komiq</title>
</svelte:head>

<div class="wrap">
	<div class="card">
		<div class="head">
			<h1>{mode === 'login' ? 'Welcome back' : 'Join komiq'}</h1>
			<p class="sub">
				{mode === 'login'
					? 'Sign in to rate series and join the discussion.'
					: 'Create an account to rate, review, and comment.'}
			</p>
		</div>

		{#if canRegister}
			<div class="tabs" role="group" aria-label="Account access">
				<button
					class="k-hit"
					aria-pressed={mode === 'login'}
					class:active={mode === 'login'}
					onclick={() => switchMode('login')}>Sign in</button
				>
				<button
					class="k-hit"
					aria-pressed={mode === 'register'}
					class:active={mode === 'register'}
					onclick={() => switchMode('register')}>Create account</button
				>
			</div>
		{/if}

		<form onsubmit={submit}>
			<label>
				<span>Username</span>
				<input
					type="text"
					name="username"
					spellcheck="false"
					autocapitalize="none"
					bind:value={username}
					autocomplete="username"
					placeholder="your_handle…"
					required
				/>
			</label>

			{#if mode === 'register'}
				<label>
					<span>Email</span>
					<input
						type="email"
						name="email"
						spellcheck="false"
						bind:value={email}
						autocomplete="email"
						placeholder="you@example.com…"
						required
					/>
				</label>
			{/if}

			<label>
				<span>Password</span>
				<input
					type="password"
					name="password"
					bind:value={password}
					autocomplete={mode === 'login' ? 'current-password' : 'new-password'}
					placeholder={mode === 'register' ? 'At least 8 characters' : '••••••••'}
					minlength={mode === 'register' ? 8 : undefined}
					required
				/>
			</label>

			<TurnstileWidget bind:this={check} />

			{#if error}
				<p class="error" role="alert">{error}</p>
			{/if}

			<button class="submit" type="submit" disabled={busy}>
				{#if busy}
					{mode === 'login' ? 'Signing in…' : 'Creating account…'}
				{:else}
					{mode === 'login' ? 'Sign in' : 'Create account'}
				{/if}
			</button>
		</form>

		<p class="foot">
			{#if !canRegister}
				New here? Ask the owner for an account.
			{:else if mode === 'login'}
				New to komiq?
				<button class="link k-hit" onclick={() => switchMode('register')}>Create an account</button>
			{:else}
				Already have an account?
				<button class="link k-hit" onclick={() => switchMode('login')}>Sign in</button>
			{/if}
		</p>

		{#if canChooseServer}
			<p class="server">
				<span class="server-host" title={serverHost}>{serverHost}</span>
				<button class="link k-hit" type="button" onclick={() => (connectScreen.open = true)}
					>Change server</button
				>
			</p>
		{/if}
	</div>
</div>

<style>
	/* Short phones (568px tall) get a tight top gap so the submit button is on screen
	   above the tab bar; taller screens open it up. */
	.wrap {
		min-height: calc(100vh - var(--k-header-h));
		min-height: calc(100dvh - var(--k-header-h));
		display: flex;
		align-items: flex-start;
		justify-content: center;
		padding: var(--k-space-4) var(--k-space-4) var(--k-space-10);
	}
	@media (min-height: 700px) {
		.wrap {
			padding-top: var(--k-space-14);
		}
	}
	.card {
		width: 100%;
		max-width: 400px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-xl);
		padding: var(--k-space-8);
	}
	.head {
		margin-bottom: var(--k-space-6);
	}
	h1 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 26px;
		letter-spacing: -0.02em;
		color: var(--k-text-bright);
	}
	.sub {
		margin-top: var(--k-space-2);
		color: var(--k-text-dim);
		font-size: 14px;
		line-height: 1.5;
	}
	.tabs {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 4px;
		padding: 4px;
		background: var(--k-surface);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-pill);
		margin-bottom: var(--k-space-6);
	}
	.tabs button {
		padding: 9px 0;
		border: none;
		border-radius: var(--k-radius-pill);
		background: transparent;
		color: var(--k-text-dim);
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
		transition:
			color 0.15s,
			background-color 0.15s,
			border-color 0.15s,
			opacity 0.15s;
	}
	.tabs button.active {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.tabs button:not(.active):hover {
		color: var(--k-text);
	}
	form {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-4);
	}
	label {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-2);
	}
	label span {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-dim);
		letter-spacing: 0.01em;
	}
	input {
		width: 100%;
		height: 44px;
		padding: 0 var(--k-space-4);
		background: var(--k-surface);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-md);
		color: var(--k-text);
		font-family: var(--k-font-sans);
		font-size: 14px;
		transition: border-color 0.15s;
	}
	input::placeholder {
		color: var(--k-text-disabled);
	}
	input:focus {
		outline: none;
		border-color: var(--k-border-strong);
	}
	.error {
		margin: calc(var(--k-space-1) * -1) 0 0;
		padding: var(--k-space-3) var(--k-space-4);
		background: rgba(224, 131, 105, 0.1);
		border: 1px solid rgba(224, 131, 105, 0.3);
		border-radius: var(--k-radius);
		color: var(--k-accent);
		font-size: 13px;
		line-height: 1.4;
	}
	.submit {
		height: 46px;
		margin-top: var(--k-space-1);
		border: none;
		border-radius: var(--k-radius-md);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-family: var(--k-font-sans);
		font-size: 14px;
		font-weight: 700;
		cursor: pointer;
		transition: background 0.15s;
	}
	.submit:hover:not(:disabled) {
		background: var(--k-primary-hover);
	}
	.submit:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.foot {
		margin-top: var(--k-space-6);
		text-align: center;
		font-size: 13px;
		color: var(--k-text-dim);
	}
	.server {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-3);
		margin-top: var(--k-space-5);
		padding-top: var(--k-space-4);
		border-top: 1px solid var(--k-border-1);
		font-size: 13px;
		color: var(--k-text-dim);
	}
	.server-host {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.server .link {
		flex: none;
	}
	.link {
		background: none;
		border: none;
		padding: 0;
		color: var(--k-text);
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
		text-decoration: underline;
		text-underline-offset: 2px;
	}
	.link:hover {
		color: var(--k-accent);
	}
</style>
