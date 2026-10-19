<script lang="ts">
	import { invalidateAll } from '$app/navigation';
	import { auth, changeOwnPassword, logout, needsCurrentPassword } from '$lib/auth.svelte';
	import AccountScreen from './AccountScreen.svelte';
	import TurnstileWidget from './TurnstileWidget.svelte';

	// Shown instead of the app while the signed-in account still has a password the
	// owner chose (a new account, or a reset). Picking their own is the way in.
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
		if (password.length < 8) errors.password = 'Use at least 8 characters.';
		else if (confirm !== password) errors.confirm = 'The passwords do not match.';
		if (Object.keys(errors).length) return;
		busy = true;
		try {
			await changeOwnPassword(password, askCurrent ? current : undefined, await check?.token());
			// Pages behind this screen loaded while the server still answered this
			// account as signed out (an empty library); load them again as the user.
			await invalidateAll();
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

<svelte:head><title>Choose a new password · komiq</title></svelte:head>

<AccountScreen
	title="Choose a new password"
	lead="You signed in with a temporary password. Pick your own to start reading."
>
	<form onsubmit={submit} novalidate aria-busy={busy}>
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
			<label>
				<span>Temporary password</span>
				<input
					type="password"
					name="current-password"
					autocomplete="current-password"
					bind:value={current}
					aria-invalid={errors.current ? true : undefined}
				/>
				{#if errors.current}<span class="field-error">{errors.current}</span>{/if}
			</label>
		{/if}
		<label>
			<span>New password</span>
			<input
				type="password"
				name="new-password"
				autocomplete="new-password"
				placeholder="At least 8 characters"
				bind:value={password}
				aria-invalid={errors.password ? true : undefined}
			/>
			{#if errors.password}<span class="field-error">{errors.password}</span>{/if}
		</label>
		<label>
			<span>Confirm new password</span>
			<input
				type="password"
				name="confirm-password"
				autocomplete="new-password"
				bind:value={confirm}
				aria-invalid={errors.confirm ? true : undefined}
			/>
			{#if errors.confirm}<span class="field-error">{errors.confirm}</span>{/if}
		</label>
		<TurnstileWidget bind:this={check} />
		{#if formError}<p class="error" role="alert">{formError}</p>{/if}
		<button class="submit" type="submit" disabled={busy}>
			{busy ? 'Saving…' : 'Save password'}
		</button>
	</form>
	{#snippet foot()}
		Not {auth.user?.username}?
		<button class="link" type="button" onclick={() => void logout()}>Sign out</button>
	{/snippet}
</AccountScreen>
