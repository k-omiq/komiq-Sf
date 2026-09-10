<script lang="ts">
	import { createUser, setRegistrationOpen } from '$lib/data';
	import { instance } from '$lib/instance.svelte';
	import { tempPassword } from '$lib/passwords';
	import Button from '$lib/components/Button.svelte';
	import Choice from '$lib/components/Choice.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { back, copyText, labels, next, stepNumber, wizard } from '../wizard.svelte';

	let newName = $state('');
	let newEmail = $state('');
	let addBusy = $state(false);
	let addError = $state<string | null>(null);
	let saveBusy = $state(false);
	let saveError = $state<string | null>(null);
	let copied = $state<string | null>(null);

	async function addUser(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (addBusy) return;
		addError = null;
		const name = newName.trim();
		if (name.length < 3) {
			addError = 'Usernames need at least 3 characters.';
			return;
		}
		if (!newEmail.includes('@')) {
			addError = 'Enter an email address.';
			return;
		}
		addBusy = true;
		const pw = tempPassword();
		try {
			await createUser({ username: name, email: newEmail.trim(), password: pw });
			wizard.added = [...wizard.added, { username: name, password: pw }];
			newName = '';
			newEmail = '';
		} catch (err) {
			addError = err instanceof Error ? err.message : 'The user could not be added.';
		} finally {
			addBusy = false;
		}
	}

	async function copy(text: string): Promise<void> {
		if (await copyText(text)) {
			copied = text;
			setTimeout(() => (copied = copied === text ? null : copied), 1600);
		}
	}

	// Back never returns to the owner form: that account exists now.
	const previous = $derived(wizard.order[stepNumber('accounts') - 2]?.id);
	const canGoBack = $derived(!!previous && previous !== 'owner');

	async function save(): Promise<void> {
		if (saveBusy) return;
		saveBusy = true;
		saveError = null;
		try {
			const open = await setRegistrationOpen(wizard.signup === 'open');
			if (instance.info) instance.info = { ...instance.info, registrationOpen: open };
			next();
		} catch (err) {
			const message = err instanceof Error ? err.message : '';
			// Pinned by the server's environment: nothing to change here, carry on.
			if (message.includes('KOMIKA_REGISTRATION_OPEN')) {
				wizard.notices = [
					...wizard.notices,
					'Sign-up is fixed by this server’s configuration, so that choice was not applied.',
				];
				next();
			} else {
				saveError = message || 'The setting could not be saved.';
			}
		} finally {
			saveBusy = false;
		}
	}
</script>

<Onboarding
	step={stepNumber('accounts')}
	steps={labels()}
	title="Who can sign in?"
	lead="Libraries stay separate for every account."
>
	<div class="ob-choices" role="radiogroup" aria-label="Sign-up">
		<Choice
			icon="users"
			title="Only people I add"
			detail="You create accounts. Nobody can sign up on their own."
			selected={wizard.signup === 'closed'}
			onselect={() => (wizard.signup = 'closed')}
		/>
		<Choice
			icon="globe"
			title="Anyone can sign up"
			detail="The reader shows a sign-up form to every visitor."
			selected={wizard.signup === 'open'}
			onselect={() => (wizard.signup = 'open')}
		/>
	</div>

	<div class="ob-section">
		<p class="ob-section-title">Add people now <span>Optional</span></p>
		<form class="ob-inline" onsubmit={addUser} novalidate>
			<input
				class="k-input"
				type="text"
				placeholder="Username"
				aria-label="Username"
				autocapitalize="none"
				spellcheck={false}
				bind:value={newName}
			/>
			<input
				class="k-input"
				type="email"
				placeholder="Email"
				aria-label="Email"
				bind:value={newEmail}
			/>
			<Button type="submit" variant="ghost" icon="plus" busy={addBusy}>Add</Button>
		</form>
		{#if addError}<p class="k-error" role="alert">{addError}</p>{/if}
		{#if wizard.added.length}
			<ul class="ob-rows">
				{#each wizard.added as person (person.username)}
					<li>
						<span class="ob-truncate">{person.username}</span>
						{#if person.password}
							<span class="ob-hint-row">
								<code class="ob-code">{person.password}</code>
								<Button
									variant="ghost"
									size="sm"
									icon={copied === person.password ? 'check' : 'copy'}
									iconOnly
									aria-label="Copy temporary password for {person.username}"
									onclick={() => person.password && copy(person.password)}
								/>
							</span>
						{:else}
							<span class="k-hint">Added</span>
						{/if}
					</li>
				{/each}
			</ul>
			<p class="k-hint">
				Share each temporary password once. They choose their own at first sign-in.
			</p>
		{/if}
	</div>
	{#if saveError}<p class="k-error" role="alert">{saveError}</p>{/if}
	{#snippet footer()}
		{#if canGoBack}
			<Button variant="ghost" icon="arrow-left" onclick={back}>Back</Button>
		{:else}
			<span></span>
		{/if}
		<Button variant="primary" busy={saveBusy} onclick={save}>Continue</Button>
	{/snippet}
</Onboarding>
