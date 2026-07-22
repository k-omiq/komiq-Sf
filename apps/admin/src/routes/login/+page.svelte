<script lang="ts">
	import { goto } from '$app/navigation';
	import { auth, login } from '$lib/auth.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import Button from '$lib/components/Button.svelte';

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

<svelte:head><title>Sign in - Komiq admin</title></svelte:head>

<div class="wrap">
	<form class="form" onsubmit={submit} aria-busy={busy}>
		<h1 class="logo" aria-label="Komiq admin sign in"><Brand size={32} /></h1>

		<label class="k-field">
			<span class="k-label">Username</span>
			<input
				class="k-input"
				type="text"
				bind:value={username}
				name="username"
				autocomplete="username"
				autocapitalize="none"
				spellcheck={false}
				required
			/>
		</label>
		<label class="k-field">
			<span class="k-label">Password</span>
			<input
				class="k-input"
				type="password"
				bind:value={password}
				name="password"
				autocomplete="current-password"
				required
			/>
		</label>

		<Button type="submit" variant="primary" size="lg" class="submit" {busy}>
			{busy ? 'Signing in…' : 'Sign in'}
		</Button>

		{#if error}
			<p class="k-notice" data-tone="danger" role="alert">{error}</p>
		{/if}
	</form>
</div>

<style>
	.wrap {
		min-height: 100dvh;
		display: grid;
		place-items: center;
		padding: var(--k-space-6) var(--k-space-4);
	}
	.form {
		width: 100%;
		max-width: 360px;
		display: grid;
		gap: var(--k-space-4);
	}
	.logo {
		justify-self: center;
		margin-bottom: var(--k-space-4);
		line-height: 1;
	}
	.k-input {
		height: var(--k-control-lg);
		font-size: var(--k-text-md);
	}
	.form :global(.submit) {
		width: 100%;
		margin-top: var(--k-space-2);
	}
</style>
