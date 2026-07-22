<script lang="ts">
	import { page as pageState } from '$app/state';
	import { queuePage, queueNavigate } from '$lib/url-state';
	import type { AdminUser } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { loadUsers, setUserBanned, setUserAdmin } from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';

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

	const count = $derived(total ?? users.length);
</script>

<div class="k-page">
	<PageHeader
		title="Users"
		description="{count} account{count === 1
			? ''
			: 's'}. Give or remove admin access, and ban accounts."
	/>

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
						<Button size="sm" disabled={busy === u.id} onclick={() => toggleAdmin(u)}>
							{u.isAdmin ? 'Revoke admin' : 'Make admin'}
						</Button>
						<Button
							size="sm"
							variant={u.isBanned ? 'secondary' : 'danger'}
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

<style>
	.k-table {
		--cols: minmax(0, 2.2fr) minmax(0, 1fr) minmax(0, 0.8fr) minmax(0, 0.8fr) minmax(0, 1.6fr);
	}
	.user {
		display: grid;
		gap: 2px;
	}
	.uname {
		color: var(--k-text-bright);
		font-weight: 700;
	}
	.uname.banned {
		color: var(--k-text-dim);
		text-decoration: line-through;
	}
</style>
