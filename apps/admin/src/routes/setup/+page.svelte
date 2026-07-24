<script lang="ts">
	import { goto } from '$app/navigation';
	import { href } from '$lib/paths';
	import { auth, adoptSession } from '$lib/auth.svelte';
	import { serverAddress, target, forgetServer } from '$lib/config';
	import { createOwner, createUser, setRegistrationOpen } from '$lib/data';
	import { takeSetupCode } from '$lib/desktop';
	import { instance } from '$lib/instance.svelte';
	import Button from '$lib/components/Button.svelte';
	import Choice from '$lib/components/Choice.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';

	// First-run wizard for a server with no owner yet. The step and the choices that
	// are safe to keep (never a password) are saved, so closing the app and coming
	// back resumes where it stopped. Every step keeps its own state on Back.
	const STEPS = ['Welcome', 'Owner account', 'Accounts', 'Done'];
	const PROGRESS_KEY = 'komiq-setup';

	type Signup = 'closed' | 'open';
	interface Saved {
		step: number;
		signup: Signup;
		added: string[];
	}

	function load(): Saved {
		try {
			const raw = localStorage.getItem(PROGRESS_KEY);
			if (raw) return { step: 1, signup: 'closed', added: [], ...JSON.parse(raw) };
		} catch {
			/* fall through */
		}
		return { step: 1, signup: 'closed', added: [] };
	}
	const saved = load();

	let step = $state(saved.step);
	let signup = $state<Signup>(saved.signup);

	// A finished server opens the catalog instead, unless this browser is mid-wizard
	// as the new owner (steps after the account still belong to setup).
	$effect(() => {
		if (instance.info && !instance.info.setupRequired && !(auth.user && step > 2)) {
			void goto(href('/'), { replaceState: true });
		}
	});

	// ---- owner -------------------------------------------------------------------

	let username = $state('');
	let email = $state('');
	let password = $state('');
	let confirm = $state('');
	const handedCode = takeSetupCode();
	let code = $state(handedCode);
	const codeFromApp = handedCode !== '';
	let busy = $state(false);
	let errors = $state<
		Partial<Record<'username' | 'email' | 'password' | 'confirm' | 'code', string>>
	>({});
	let formError = $state<string | null>(null);

	/** Route a server message to the field it is about. */
	function placeError(message: string): void {
		const m = message.toLowerCase();
		if (m.includes('setup code') || m.includes('too many setup')) errors.code = message;
		else if (m.includes('username')) errors.username = message;
		else if (m.includes('email')) errors.email = message;
		else if (m.includes('password')) errors.password = message;
		else formError = message;
	}

	async function submitOwner(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (busy) return;
		errors = {};
		formError = null;
		if (username.trim().length < 3) errors.username = 'At least 3 characters.';
		if (!email.includes('@')) errors.email = 'Enter an email address.';
		if (password.length < 8) errors.password = 'At least 8 characters.';
		else if (confirm !== password) errors.confirm = 'The passwords do not match.';
		if (!code.trim()) errors.code = 'Enter the setup code from the server log.';
		if (Object.keys(errors).length) return;
		busy = true;
		try {
			const session = await createOwner({
				setupCode: code.trim(),
				username: username.trim(),
				email: email.trim(),
				password,
			});
			adoptSession(session);
			password = '';
			confirm = '';
			code = '';
			step = 3;
		} catch (err) {
			placeError(err instanceof Error ? err.message : 'The account could not be created.');
		} finally {
			busy = false;
		}
	}

	// ---- accounts ----------------------------------------------------------------

	interface Added {
		username: string;
		/** Shown once, in this visit only. Never saved. */
		password: string | null;
	}
	let added = $state<Added[]>(saved.added.map((u) => ({ username: u, password: null })));
	let newName = $state('');
	let newEmail = $state('');
	let addBusy = $state(false);
	let addError = $state<string | null>(null);
	let saveBusy = $state(false);
	let saveNotice = $state<string | null>(null);
	let copied = $state<string | null>(null);

	/** A readable temporary password: 12 symbols, no look-alikes. */
	function tempPassword(): string {
		const alphabet = 'abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789';
		const bytes = crypto.getRandomValues(new Uint8Array(12));
		const raw = Array.from(bytes, (b) => alphabet[b % alphabet.length]).join('');
		return `${raw.slice(0, 4)}-${raw.slice(4, 8)}-${raw.slice(8)}`;
	}

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
			added = [...added, { username: name, password: pw }];
			newName = '';
			newEmail = '';
		} catch (err) {
			addError = err instanceof Error ? err.message : 'The user could not be added.';
		} finally {
			addBusy = false;
		}
	}

	async function copy(text: string): Promise<void> {
		try {
			await navigator.clipboard.writeText(text);
			copied = text;
			setTimeout(() => (copied = copied === text ? null : copied), 1600);
		} catch {
			/* clipboard blocked: the text stays selectable */
		}
	}

	async function saveAccounts(): Promise<void> {
		if (saveBusy) return;
		saveBusy = true;
		saveNotice = null;
		try {
			await setRegistrationOpen(signup === 'open');
			step = 4;
		} catch (err) {
			const message = err instanceof Error ? err.message : '';
			// Pinned by the server's environment: nothing to change here, carry on.
			if (message.includes('KOMIKA_REGISTRATION_OPEN')) {
				saveNotice =
					'Sign-up is fixed by this server’s configuration, so this choice was not applied.';
				step = 4;
			} else {
				saveNotice = message || 'The setting could not be saved.';
			}
		} finally {
			saveBusy = false;
		}
	}

	// Save progress (never a password) so a closed app resumes here.
	$effect(() => {
		try {
			localStorage.setItem(
				PROGRESS_KEY,
				JSON.stringify({ step, signup, added: added.map((u) => u.username) }),
			);
		} catch {
			/* private mode: progress lasts for this visit */
		}
	});

	// ---- done --------------------------------------------------------------------

	function finish(path: string): void {
		try {
			localStorage.removeItem(PROGRESS_KEY);
		} catch {
			/* ignore */
		}
		void goto(href(path), { replaceState: true });
	}

	function changeServer(): void {
		forgetServer();
		location.assign(href('/start'));
	}
</script>

<svelte:head><title>Set up - komiq</title></svelte:head>

{#snippet serverAside()}
	{#if target === 'desktop' && step <= 2}
		<button class="link" type="button" onclick={changeServer}>Use a different server</button>
	{/if}
{/snippet}

{#if step === 1}
	<Onboarding
		step={1}
		steps={STEPS}
		title="Set up your server"
		lead="A few steps and your library is ready."
		aside={serverAside}
	>
		<ul class="points">
			<li><Icon name="shield" size={16} />Create the owner account that runs this server.</li>
			<li><Icon name="users" size={16} />Decide who else can sign in.</li>
			<li>
				<Icon name="package" size={16} />No sources are included. You add the ones you want
				afterwards.
			</li>
		</ul>
		{#snippet footer()}
			<span></span>
			<Button variant="primary" onclick={() => (step = auth.user ? 3 : 2)}>Get started</Button>
		{/snippet}
	</Onboarding>
{:else if step === 2}
	<Onboarding
		step={2}
		steps={STEPS}
		title="Create the owner account"
		lead="The owner manages the server, its sources and its users."
		aside={serverAside}
	>
		<form id="owner" class="form" onsubmit={submitOwner} novalidate aria-busy={busy}>
			<label class="k-field">
				<span class="k-label">Username</span>
				<input
					class="k-input"
					type="text"
					autocomplete="username"
					autocapitalize="none"
					spellcheck={false}
					bind:value={username}
					aria-invalid={errors.username ? true : undefined}
				/>
				{#if errors.username}<span class="k-error">{errors.username}</span>{/if}
			</label>
			<label class="k-field">
				<span class="k-label">Email</span>
				<input
					class="k-input"
					type="email"
					autocomplete="email"
					bind:value={email}
					aria-invalid={errors.email ? true : undefined}
				/>
				{#if errors.email}<span class="k-error">{errors.email}</span>{/if}
			</label>
			<div class="pair">
				<label class="k-field">
					<span class="k-label">Password</span>
					<input
						class="k-input"
						type="password"
						autocomplete="new-password"
						bind:value={password}
						aria-invalid={errors.password ? true : undefined}
					/>
					{#if errors.password}<span class="k-error">{errors.password}</span>{/if}
				</label>
				<label class="k-field">
					<span class="k-label">Confirm</span>
					<input
						class="k-input"
						type="password"
						autocomplete="new-password"
						bind:value={confirm}
						aria-invalid={errors.confirm ? true : undefined}
					/>
					{#if errors.confirm}<span class="k-error">{errors.confirm}</span>{/if}
				</label>
			</div>
			{#if codeFromApp && !errors.code}
				<p class="k-hint code-ok">
					<Icon name="check" size={14} />Setup code filled in by the app.
				</p>
			{:else}
				<label class="k-field">
					<span class="k-label">Setup code</span>
					<input
						class="k-input mono"
						type="text"
						autocomplete="one-time-code"
						autocapitalize="characters"
						spellcheck={false}
						placeholder="XXXX-XXXX-XXXX"
						bind:value={code}
						aria-invalid={errors.code ? true : undefined}
					/>
					{#if errors.code}
						<span class="k-error">{errors.code}</span>
					{:else}
						<span class="k-hint"
							>Printed in the server log when it started. It proves you run this server.</span
						>
					{/if}
				</label>
			{/if}
			{#if formError}<p class="k-error" role="alert">{formError}</p>{/if}
		</form>
		{#snippet footer()}
			<Button variant="ghost" icon="arrow-left" onclick={() => (step = 1)}>Back</Button>
			<Button variant="primary" type="submit" form="owner" {busy}>
				{busy ? 'Creating…' : 'Create account'}
			</Button>
		{/snippet}
	</Onboarding>
{:else if step === 3}
	<Onboarding
		step={3}
		steps={STEPS}
		title="Who can sign in?"
		lead="Libraries stay separate for every account."
	>
		<div class="choices" role="radiogroup" aria-label="Sign-up">
			<Choice
				icon="users"
				title="Only people I add"
				detail="You create accounts. Nobody can sign up on their own."
				selected={signup === 'closed'}
				onselect={() => (signup = 'closed')}
			/>
			<Choice
				icon="globe"
				title="Anyone can sign up"
				detail="The reader shows a sign-up form to every visitor."
				selected={signup === 'open'}
				onselect={() => (signup = 'open')}
			/>
		</div>

		<div class="section">
			<p class="section-title">Add people now <span>Optional</span></p>
			<form class="add" onsubmit={addUser} novalidate>
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
			{#if added.length}
				<ul class="people">
					{#each added as person (person.username)}
						<li>
							<span class="who">{person.username}</span>
							{#if person.password}
								<code class="temp">{person.password}</code>
								<Button
									variant="ghost"
									size="sm"
									icon={copied === person.password ? 'check' : 'copy'}
									iconOnly
									aria-label="Copy temporary password for {person.username}"
									onclick={() => person.password && copy(person.password)}
								/>
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
		{#if saveNotice}<p class="k-error" role="alert">{saveNotice}</p>{/if}
		{#snippet footer()}
			<span></span>
			<Button variant="primary" busy={saveBusy} onclick={saveAccounts}>Continue</Button>
		{/snippet}
	</Onboarding>
{:else}
	<Onboarding step={4} steps={STEPS} title="You are all set" lead="Your server is ready to use.">
		<dl class="summary">
			<div>
				<dt>Reader</dt>
				<dd>
					<span class="addr">{serverAddress()}</span>
					<Button
						variant="ghost"
						size="sm"
						icon={copied === serverAddress() ? 'check' : 'copy'}
						iconOnly
						aria-label="Copy reader address"
						onclick={() => copy(serverAddress())}
					/>
				</dd>
			</div>
			<div>
				<dt>Owner</dt>
				<dd>{auth.user?.username ?? '—'}</dd>
			</div>
			<div>
				<dt>Sign-up</dt>
				<dd>{signup === 'open' ? 'Anyone can sign up' : 'Only people you add'}</dd>
			</div>
			<div>
				<dt>People added</dt>
				<dd class="k-num">{added.length}</dd>
			</div>
		</dl>
		{#if saveNotice}<p class="k-hint">{saveNotice}</p>{/if}
		{#snippet footer()}
			<Button variant="ghost" onclick={() => finish('/')}>Open the catalog</Button>
			<Button variant="primary" onclick={() => finish('/sources/extensions')}
				>Add your first source</Button
			>
		{/snippet}
	</Onboarding>
{/if}

<style>
	.form {
		display: grid;
		gap: var(--k-space-4);
	}
	.pair {
		display: grid;
		grid-template-columns: 1fr 1fr;
		align-items: start;
		gap: var(--k-space-3);
	}
	@media (max-width: 420px) {
		.pair {
			grid-template-columns: 1fr;
		}
	}
	.k-input {
		height: var(--k-control-lg);
		font-size: var(--k-text-md);
	}
	.mono {
		font-family: var(--k-font-mono, ui-monospace, monospace);
		letter-spacing: 0.06em;
	}
	.code-ok {
		display: flex;
		align-items: center;
		gap: 6px;
		margin: 0;
	}
	.points {
		display: grid;
		gap: var(--k-space-3);
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.points li {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: var(--k-space-3);
		align-items: start;
		font-size: var(--k-text-md);
		line-height: 1.55;
		color: var(--k-text-2);
	}
	.points :global(svg) {
		margin-top: 3px;
		color: var(--k-text-dim);
	}
	.choices {
		display: grid;
		gap: var(--k-space-2);
	}
	.section {
		display: grid;
		gap: var(--k-space-3);
		margin-top: var(--k-space-2);
	}
	.section-title {
		margin: 0;
		font-size: var(--k-text-sm);
		font-weight: 600;
		color: var(--k-text-1);
	}
	.section-title span {
		margin-left: 6px;
		font-weight: 400;
		color: var(--k-text-dim);
	}
	.add {
		display: grid;
		grid-template-columns: 1fr 1fr auto;
		gap: var(--k-space-2);
	}
	.add .k-input {
		height: var(--k-control);
		font-size: var(--k-text-sm);
	}
	@media (max-width: 420px) {
		.add {
			grid-template-columns: 1fr;
		}
	}
	.people {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.people li {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		padding: 8px 8px 8px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.who {
		flex: 1 1 auto;
		min-width: 0;
		font-size: var(--k-text-sm);
		color: var(--k-text-1);
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.temp {
		font-size: var(--k-text-sm);
		color: var(--k-text-2);
		letter-spacing: 0.04em;
		user-select: all;
	}
	.summary {
		display: grid;
		gap: 2px;
		margin: 0;
	}
	.summary > div {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-3);
		min-height: 44px;
		padding: 6px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.summary dt {
		font-size: var(--k-text-sm);
		color: var(--k-text-dim);
	}
	.summary dd {
		display: flex;
		align-items: center;
		gap: var(--k-space-1, 4px);
		margin: 0;
		min-width: 0;
		font-size: var(--k-text-sm);
		color: var(--k-text-1);
	}
	/* Icon buttons sit flush with the row's text edge. */
	.summary dd :global(.k-btn) {
		margin-right: -8px;
	}
	.addr {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.link {
		padding: 4px 6px;
		border: 0;
		background: none;
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		cursor: pointer;
		text-decoration: underline;
		text-underline-offset: 3px;
	}
	.link:hover {
		color: var(--k-text-1);
	}
</style>
