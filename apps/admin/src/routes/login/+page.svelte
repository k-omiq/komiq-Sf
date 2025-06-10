<script lang="ts">
	import { goto } from '$app/navigation';
	import { auth, login } from '$lib/auth.svelte';

	let username = $state('');
	let password = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);

	$effect(() => {
		if (auth.user) goto('/', { replaceState: true });
	});

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (busy) return;
		error = null;
		busy = true;
		try {
			await login(username.trim(), password);
			await goto('/', { replaceState: true });
		} catch (err) {
			error = err instanceof Error ? err.message : 'Sign in failed.';
		} finally {
			busy = false;
		}
	}
</script>

<div class="wrap">
	<form class="card" onsubmit={submit}>
		<div class="head">
			<div class="brand"><span class="mark">KOMIQ</span><span class="sub">manga DB</span></div>
			<h1>Admin sign in</h1>
			<p class="hint">Catalog management, scan overrides, and status flags.</p>
		</div>

		<label>
			<span>Username</span>
			<input type="text" bind:value={username} autocomplete="username" required />
		</label>
		<label>
			<span>Password</span>
			<input type="password" bind:value={password} autocomplete="current-password" required />
		</label>

		{#if error}<p class="error" role="alert">{error}</p>{/if}

		<button class="submit" type="submit" disabled={busy}>
			{busy ? 'Signing in…' : 'Sign in'}
		</button>
	</form>
</div>

<style>
	.wrap {
		min-height: 100vh;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: var(--k-space-6);
	}
	.card {
		width: 100%;
		max-width: 380px;
		display: flex;
		flex-direction: column;
		gap: var(--k-space-4);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-xl);
		padding: var(--k-space-8);
	}
	.head {
		margin-bottom: var(--k-space-2);
	}
	.brand {
		display: flex;
		align-items: baseline;
		gap: 9px;
		margin-bottom: var(--k-space-5);
	}
	.mark {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 17px;
		letter-spacing: 0.02em;
		color: var(--k-text-bright);
	}
	.sub {
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	h1 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 22px;
		letter-spacing: -0.02em;
		color: var(--k-text-bright);
	}
	.hint {
		margin-top: var(--k-space-2);
		font-size: 13px;
		color: var(--k-text-dim);
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
	}
	input {
		height: 42px;
		padding: 0 var(--k-space-4);
		background: var(--k-surface);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-md);
		color: var(--k-text);
		font-family: var(--k-font-sans);
		font-size: 14px;
	}
	input:focus {
		outline: none;
		border-color: var(--k-border-strong);
	}
	.error {
		margin: 0;
		padding: var(--k-space-3) var(--k-space-4);
		background: rgba(224, 131, 105, 0.1);
		border: 1px solid rgba(224, 131, 105, 0.3);
		border-radius: var(--k-radius);
		color: var(--k-accent);
		font-size: 13px;
	}
	.submit {
		height: 44px;
		margin-top: var(--k-space-2);
		border: none;
		border-radius: var(--k-radius-md);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-family: var(--k-font-sans);
		font-size: 14px;
		font-weight: 700;
		cursor: pointer;
	}
	.submit:hover:not(:disabled) {
		background: var(--k-primary-hover);
	}
	.submit:disabled {
		opacity: 0.6;
		cursor: default;
	}
</style>
