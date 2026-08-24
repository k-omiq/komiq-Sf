<script lang="ts">
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type { AdminUser } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { createUser, loadUsers, resetUserPassword, setUserBanned, setUserAdmin } from '$lib/data';
	import { copyText, tempPassword } from '$lib/passwords';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import Switch from '$lib/components/Switch.svelte';

	let users = $state<AdminUser[]>([]);
	const page = $derived(queuePage(pageState.url));
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let busy = $state<string | null>(null); // id currently being mutated

	// Auth redirect is centralized in +layout.svelte.

	$effect(() => {
		if (!auth.user) return;
		void refresh(page);
	});

	let loadSeq = 0;
	async function refresh(p: number): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		try {
			const res = await loadUsers(p);
			if (seq !== loadSeq) return;
			users = res.items;
			hasNext = res.hasNextPage;
			total = res.total;
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load users.';
			users = [];
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	function fmtDate(iso: string): string {
		const t = Date.parse(iso);
		return Number.isNaN(t) ? 'Unknown' : new Date(t).toLocaleDateString();
	}

	async function toggleBan(u: AdminUser): Promise<void> {
		if (busy) return;
		const next = !u.isBanned;
		if (
			next &&
			!(await confirm({
				title: `Ban ${u.username}?`,
				message: 'They will not be able to sign in until you unban them.',
				confirmLabel: 'Ban user',
				danger: true,
			}))
		)
			return;
		busy = u.id;
		try {
			await setUserBanned(u.id, next);
			users = users.map((x) => (x.id === u.id ? { ...x, isBanned: next } : x));
			toast.success(next ? `${u.username} is banned.` : `${u.username} is unbanned.`);
		} catch (err) {
			toast.fromError(err, 'Action failed.');
		} finally {
			busy = null;
		}
	}

	async function toggleAdmin(u: AdminUser): Promise<void> {
		if (busy) return;
		const next = !u.isAdmin;
		const ok = await confirm(
			next
				? {
						title: `Make ${u.username} an admin?`,
						message: 'They will get full access to this console and every admin action.',
						confirmLabel: 'Make admin',
					}
				: {
						title: `Remove admin access from ${u.username}?`,
						message: 'They will be signed out of this console and keep a normal account.',
						confirmLabel: 'Remove admin',
						danger: true,
					},
		);
		if (!ok) return;
		busy = u.id;
		try {
			const updated = await setUserAdmin(u.id, next);
			users = users.map((x) => (x.id === u.id ? updated : x));
			toast.success(
				next ? `${u.username} is now an admin.` : `${u.username} is no longer an admin.`,
			);
		} catch (err) {
			toast.fromError(err, 'Action failed.');
		} finally {
			busy = null;
		}
	}

	// ---- add user / reset password ---------------------------------------------
	// Both hand the admin a temporary password to pass on ONCE: it is shown in the
	// dialog that follows and kept nowhere else. The person picks their own at first
	// sign-in (the server sets mustChangePassword).
	let adding = $state(false);
	let newName = $state('');
	let newEmail = $state('');
	let newAdmin = $state(false);
	let addBusy = $state(false);
	let addErrors = $state<Partial<Record<'username' | 'email', string>>>({});
	let addError = $state<string | null>(null);

	/** The one-time password to show, and whose it is. */
	let issued = $state<{ username: string; password: string; created: boolean } | null>(null);
	let copied = $state(false);

	function openAdd(): void {
		newName = '';
		newEmail = '';
		newAdmin = false;
		addErrors = {};
		addError = null;
		adding = true;
	}

	async function submitAdd(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (addBusy) return;
		addErrors = {};
		addError = null;
		const username = newName.trim();
		const email = newEmail.trim();
		if (username.length < 3) addErrors.username = 'At least 3 characters.';
		if (!email.includes('@')) addErrors.email = 'Enter an email address.';
		if (Object.keys(addErrors).length) return;
		addBusy = true;
		const password = tempPassword();
		try {
			await createUser({ username, email, password, isAdmin: newAdmin });
			adding = false;
			copied = false;
			issued = { username, password, created: true };
			void refresh(page);
		} catch (err) {
			const message = err instanceof Error ? err.message : 'The user could not be added.';
			const m = message.toLowerCase();
			if (m.includes('username')) addErrors.username = message;
			else if (m.includes('email')) addErrors.email = message;
			else addError = message;
		} finally {
			addBusy = false;
		}
	}

	async function resetPassword(u: AdminUser): Promise<void> {
		if (busy) return;
		const ok = await confirm({
			title: `Reset the password of ${u.username}?`,
			message:
				'They are signed out on every device and get a temporary password from you. They choose a new one when they next sign in.',
			confirmLabel: 'Reset password',
		});
		if (!ok) return;
		busy = u.id;
		const password = tempPassword();
		try {
			await resetUserPassword(u.id, password);
			copied = false;
			issued = { username: u.username, password, created: false };
		} catch (err) {
			toast.fromError(err, 'The password could not be reset.');
		} finally {
			busy = null;
		}
	}

	async function copyIssued(): Promise<void> {
		if (issued && (await copyText(issued.password))) copied = true;
		else toast.error('Copy failed. Select the password and copy it by hand.');
	}

	const count = $derived(total ?? users.length);
</script>

<div class="k-page">
	<PageHeader
		title="Users"
		description="{count} account{count === 1
			? ''
			: 's'}. Add people, reset passwords, give or remove admin access, and ban accounts."
	>
		{#snippet actions()}
			<Button variant="primary" icon="plus" onclick={openAdd}>Add user</Button>
		{/snippet}
	</PageHeader>

	<LoadState
		loading={loading && users.length === 0}
		error={loadError}
		empty={users.length === 0}
		emptyTitle="No users on this page"
		emptyIcon="users"
		onretry={() => refresh(page)}
	>
		{#snippet emptyActions()}
			<!-- An empty page past the first (a stale link, or the list shrank) has no table
			     and so no pager: give the way back here. -->
			{#if page > 1}
				<Button size="sm" onclick={() => queueNavigate(pageState.url, { page: null })}>
					Go to the first page
				</Button>
			{/if}
		{/snippet}
		<div class="k-table" class:is-dim={loading} role="table" aria-label="Users">
			<div class="k-tr k-tr--head" role="row">
				<span role="columnheader">User</span>
				<span role="columnheader">Joined</span>
				<span role="columnheader">Role</span>
				<span role="columnheader">Status</span>
				<span role="columnheader" class="k-td--end">Actions</span>
			</div>
			{#each users as u (u.id)}
				<div class="k-tr" role="row">
					<span class="user" role="cell">
						<span class="uname k-truncate" class:banned={u.isBanned}>{u.username}</span>
						<span class="k-muted k-truncate">{u.email}</span>
					</span>
					<span class="k-muted k-num" role="cell" data-label="Joined">{fmtDate(u.createdAt)}</span>
					<span role="cell" data-label="Role">
						{#if u.isAdmin}<span class="k-badge" data-tone="accent">Admin</span>{:else}<span
								class="k-badge">Member</span
							>{/if}
					</span>
					<span role="cell" data-label="Status">
						{#if u.isBanned}<span class="k-badge" data-tone="danger">Banned</span>{:else}<span
								class="k-badge"
								data-tone="success">Active</span
							>{/if}
					</span>
					<span class="k-td--end" role="cell">
						{#if u.id !== auth.user?.id}
							<Button
								size="sm"
								variant="ghost"
								disabled={busy === u.id}
								onclick={() => resetPassword(u)}
							>
								Reset password
							</Button>
						{/if}
						<Button
							size="sm"
							variant="ghost"
							disabled={busy === u.id}
							onclick={() => toggleAdmin(u)}
						>
							{u.isAdmin ? 'Revoke admin' : 'Make admin'}
						</Button>
						<Button
							size="sm"
							variant="ghost"
							class={u.isBanned ? '' : 'ban'}
							disabled={busy === u.id}
							onclick={() => toggleBan(u)}
						>
							{u.isBanned ? 'Unban' : 'Ban'}
						</Button>
					</span>
				</div>
			{/each}
		</div>

		<Pager
			{page}
			{hasNext}
			busy={loading}
			onpage={(n) => queueNavigate(pageState.url, { page: n })}
		/>
	</LoadState>
</div>

{#if adding}
	<Dialog
		title="Add user"
		description="They get a temporary password from you and choose their own at first sign-in."
		size="sm"
		onclose={() => (adding = false)}
	>
		<form id="add-user" class="add-form" onsubmit={submitAdd} novalidate aria-busy={addBusy}>
			<label class="k-field">
				<span class="k-label">Username</span>
				<input
					class="k-input"
					type="text"
					autocomplete="off"
					autocapitalize="none"
					spellcheck={false}
					bind:value={newName}
					aria-invalid={addErrors.username ? true : undefined}
				/>
				{#if addErrors.username}<span class="k-error">{addErrors.username}</span>{/if}
			</label>
			<label class="k-field">
				<span class="k-label">Email</span>
				<input
					class="k-input"
					type="email"
					autocomplete="off"
					bind:value={newEmail}
					aria-invalid={addErrors.email ? true : undefined}
				/>
				{#if addErrors.email}<span class="k-error">{addErrors.email}</span>{/if}
			</label>
			<div class="admin-row">
				<div>
					<p class="admin-name">Admin</p>
					<p class="k-hint">Full access to this console.</p>
				</div>
				<Switch label="Admin" checked={newAdmin} onchange={(v) => (newAdmin = v)} />
			</div>
			{#if addError}<p class="k-error" role="alert">{addError}</p>{/if}
		</form>
		{#snippet footer()}
			<Button variant="ghost" onclick={() => (adding = false)}>Cancel</Button>
			<Button variant="primary" type="submit" form="add-user" busy={addBusy}>
				{addBusy ? 'Adding…' : 'Add user'}
			</Button>
		{/snippet}
	</Dialog>
{/if}

{#if issued}
	<Dialog
		title={issued.created ? `${issued.username} was added` : 'Password reset'}
		description="Give {issued.username} this temporary password. It is shown only once."
		size="sm"
		onclose={() => (issued = null)}
	>
		<div class="issued">
			<code class="issued-code" aria-label="Temporary password">{issued.password}</code>
			<Button
				variant="ghost"
				size="sm"
				icon={copied ? 'check' : 'copy'}
				onclick={copyIssued}
				aria-live="polite"
			>
				{copied ? 'Copied' : 'Copy'}
			</Button>
		</div>
		<p class="k-hint issued-hint">They choose their own password when they first sign in.</p>
		{#snippet footer()}
			<Button variant="primary" onclick={() => (issued = null)}>Done</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
	.k-table {
		--cols: minmax(0, 2fr) minmax(0, 0.9fr) minmax(0, 0.8fr) minmax(0, 0.8fr) minmax(0, 2.4fr);
	}
	/* Row actions are flat text buttons; only the destructive one takes a colour. */
	.k-table :global(.ban) {
		color: var(--k-danger);
	}
	.k-table :global(.ban:hover:not(:disabled)) {
		background: var(--k-danger-soft);
		color: var(--k-danger);
	}
	.user {
		display: grid;
		gap: 2px;
	}
	.uname {
		color: var(--k-text-bright);
		font-weight: 700;
	}
	.add-form {
		display: grid;
		gap: var(--k-space-4);
	}
	.admin-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-4);
	}
	.admin-name {
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.issued {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-3);
		min-height: 52px;
		padding: 6px 8px 6px 16px;
		border-radius: var(--k-radius);
		background: var(--k-surface-4);
	}
	.issued-code {
		min-width: 0;
		overflow-wrap: anywhere;
		color: var(--k-text-bright);
		font-family: var(--k-font-mono);
		font-size: var(--k-text-lg);
		letter-spacing: 0.06em;
		user-select: all;
	}
	.issued-hint {
		margin-top: var(--k-space-3);
	}
	.uname.banned {
		color: var(--k-text-dim);
		text-decoration: line-through;
	}
</style>
