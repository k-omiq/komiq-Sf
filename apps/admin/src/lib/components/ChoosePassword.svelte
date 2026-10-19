<script lang="ts">
	import { auth, changeOwnPassword, logout, needsCurrentPassword } from '$lib/auth.svelte';
	import Button from './Button.svelte';
	import Onboarding from './Onboarding.svelte';
	import TurnstileWidget from './TurnstileWidget.svelte';

	// Shown instead of the console while the signed-in admin's account still has a
	// password someone else chose (a new account, or a reset). Nothing else in the
	// console is reachable until it is replaced.
	const askCurrent = needsCurrentPassword();
	let current = $state('');
	let password = $state('');
	let confirm = $state('');
	let busy = $state(false);
	let errors = $state<Partial<Record<'current' | 'password' | 'confirm', string>>>({});
	let formError = $state<string | null>(null);
	let check = $state<TurnstileWidget>();

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (busy) return;
		errors = {};
		formError = null;
		if (askCurrent && !current) errors.current = 'Enter the password you signed in with.';
		if (password.length < 8) errors.password = 'At least 8 characters.';
		else if (confirm !== password) errors.confirm = 'The passwords do not match.';
		if (Object.keys(errors).length) return;
		busy = true;
		try {
			await changeOwnPassword(password, askCurrent ? current : undefined, await check?.token());
		} catch (err) {
			const message = err instanceof Error ? err.message : 'The password could not be saved.';
			const m = message.toLowerCase();
			if (m.includes('current password')) errors.current = message;
			else if (m.includes('password must') || m.includes('different')) errors.password = message;
			else formError = message;
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>Choose a new password - Komiq admin</title></svelte:head>

<Onboarding
	title="Choose a new password"
	lead="You signed in with a temporary password. Pick your own to open the console."
>
	<form id="choose-password" class="ob-form" onsubmit={submit} novalidate aria-busy={busy}>
		<!-- Lets password managers file the new password under the right account. -->
		<input
			type="text"
			name="username"
			autocomplete="username"
			value={auth.user?.username ?? ''}
			hidden
			readonly
		/>
		{#if askCurrent}
			<label class="k-field">
				<span class="k-label">Temporary password</span>
				<input
					class="k-input ob-input"
					type="password"
					autocomplete="current-password"
					bind:value={current}
					aria-invalid={errors.current ? true : undefined}
				/>
				{#if errors.current}<span class="k-error">{errors.current}</span>{/if}
			</label>
		{/if}
		<label class="k-field">
			<span class="k-label">New password</span>
			<input
				class="k-input ob-input"
				type="password"
				autocomplete="new-password"
				bind:value={password}
				aria-invalid={errors.password ? true : undefined}
			/>
			{#if errors.password}
				<span class="k-error">{errors.password}</span>
			{:else}
				<span class="k-hint">At least 8 characters.</span>
			{/if}
		</label>
		<label class="k-field">
			<span class="k-label">Confirm new password</span>
			<input
				class="k-input ob-input"
				type="password"
				autocomplete="new-password"
				bind:value={confirm}
				aria-invalid={errors.confirm ? true : undefined}
			/>
			{#if errors.confirm}<span class="k-error">{errors.confirm}</span>{/if}
		</label>
		<TurnstileWidget bind:this={check} />
		{#if formError}<p class="k-error" role="alert">{formError}</p>{/if}
	</form>
	{#snippet aside()}
		Signed in as {auth.user?.username}
	{/snippet}
	{#snippet footer()}
		<Button variant="ghost" icon="logout" onclick={() => void logout()}>Sign out</Button>
		<Button variant="primary" type="submit" form="choose-password" {busy}>
			{busy ? 'Saving…' : 'Save password'}
		</Button>
	{/snippet}
</Onboarding>
