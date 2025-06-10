<script lang="ts">
	import type { AdminUser } from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import { loadUsers, setUserBanned, setUserAdmin } from '$lib/data';

	let users = $state<AdminUser[]>([]);
	let page = $state(1);
	let hasNext = $state(false);
	let total = $state<number | null>(null);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let busy = $state<string | null>(null); // id currently being mutated

	// Auth redirect is centralized in +layout.svelte.

	$effect(() => {
		if (!auth.user) return;
		void refresh(page);
	});

	// The load effect above is the single fetch driver: pager buttons only move `page`
	// and the effect re-fires exactly once. `refresh` no longer reassigns `page` (which
	// previously caused a second fetch per navigation).
	function go(delta: number): void {
		const next = Math.max(1, page + delta);
		if (next === page) return;
		page = next;
	}

	async function refresh(p: number): Promise<void> {
		loading = true;
		loadError = null;
		try {
			const res = await loadUsers(p);
			users = res.items;
			hasNext = res.hasNextPage;
			total = res.total;
		} catch (err) {
			loadError = err instanceof Error ? err.message : 'Failed to load users.';
			users = [];
		} finally {
			loading = false;
		}
	}

	function fmtDate(iso: string): string {
		const t = Date.parse(iso);
		return Number.isNaN(t) ? '—' : new Date(t).toLocaleDateString();
	}

	async function toggleBan(u: AdminUser): Promise<void> {
		if (busy) return;
		const next = !u.isBanned;
		if (next && !confirm(`Ban ${u.username}? They won't be able to sign in.`)) return;
		busy = u.id;
		actionError = null;
		try {
			await setUserBanned(u.id, next);
			users = users.map((x) => (x.id === u.id ? { ...x, isBanned: next } : x));
		} catch (err) {
			actionError = err instanceof Error ? err.message : 'Action failed.';
		} finally {
			busy = null;
		}
	}

	async function toggleAdmin(u: AdminUser): Promise<void> {
		if (busy) return;
		const next = !u.isAdmin;
		busy = u.id;
		actionError = null;
		try {
			const updated = await setUserAdmin(u.id, next);
			users = users.map((x) => (x.id === u.id ? updated : x));
		} catch (err) {
			actionError = err instanceof Error ? err.message : 'Action failed.';
		} finally {
			busy = null;
		}
	}
</script>

<div class="page">
	<div class="page-head">
		<div>
			<h1>Users</h1>
			<p class="lede">
				{total ?? users.length} account{(total ?? users.length) === 1 ? '' : 's'} · manage admin access
				and suspensions
			</p>
		</div>
	</div>

	{#if loadError}
		<div class="notice error">{loadError}</div>
	{:else if loading && users.length === 0}
		<div class="notice">Loading…</div>
	{:else}
		{#if actionError}<div class="notice error">{actionError}</div>{/if}
		<div class="table" role="table" aria-label="Users">
			<div class="row head" role="row">
				<span role="columnheader">User</span>
				<span role="columnheader">Joined</span>
				<span role="columnheader">Role</span>
				<span role="columnheader">Status</span>
				<span role="columnheader" class="actions-col">Actions</span>
			</div>
			{#each users as u (u.id)}
				<div class="row" role="row" class:banned={u.isBanned}>
					<span class="user" role="cell">
						<span class="uname">{u.username}</span>
						<span class="email">{u.email}</span>
					</span>
					<span class="muted" role="cell">{fmtDate(u.createdAt)}</span>
					<span role="cell">
						{#if u.isAdmin}<span class="badge admin">Admin</span>{:else}<span class="badge"
								>Member</span
							>{/if}
					</span>
					<span role="cell">
						{#if u.isBanned}<span class="badge danger">Banned</span>{:else}<span class="badge ok"
								>Active</span
							>{/if}
					</span>
					<span class="actions" role="cell">
						<button class="act" disabled={busy === u.id} onclick={() => toggleAdmin(u)}>
							{u.isAdmin ? 'Revoke admin' : 'Make admin'}
						</button>
						<button
							class="act"
							class:danger={!u.isBanned}
							disabled={busy === u.id}
							onclick={() => toggleBan(u)}
						>
							{u.isBanned ? 'Unban' : 'Ban'}
						</button>
					</span>
				</div>
			{/each}
		</div>

		<div class="pager">
			<button class="pg" disabled={page <= 1 || loading} onclick={() => go(-1)}>← Prev</button>
			<span class="pg-num">Page {page}</span>
			<button class="pg" disabled={!hasNext || loading} onclick={() => go(1)}>Next →</button>
		</div>
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-6);
	}
	.page-head {
		display: flex;
		align-items: flex-end;
		justify-content: space-between;
	}
	h1 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 30px;
		letter-spacing: -0.02em;
		color: var(--k-text-bright);
	}
	.lede {
		margin-top: 6px;
		font-size: 13.5px;
		color: var(--k-text-dim);
	}
	.notice {
		padding: 14px 16px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		color: var(--k-text-dim);
		font-size: 13.5px;
	}
	.notice.error {
		color: #f0808a;
		border-color: rgba(240, 128, 138, 0.4);
	}
	.table {
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-lg);
		overflow: hidden;
	}
	.row {
		display: grid;
		grid-template-columns: 2fr 1fr 1fr 1fr auto;
		align-items: center;
		gap: var(--k-space-4);
		padding: 12px 16px;
		border-bottom: 1px solid var(--k-border);
	}
	.row:last-child {
		border-bottom: none;
	}
	.row.head {
		background: var(--k-surface-2);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.row.banned {
		background: rgba(240, 128, 138, 0.05);
	}
	.user {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.uname {
		font-size: 14px;
		font-weight: 600;
		color: var(--k-text-1);
	}
	.email {
		font-size: 12px;
		color: var(--k-text-faint);
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.muted {
		font-size: 13px;
		color: var(--k-text-dim);
	}
	.badge {
		display: inline-block;
		font-size: 11px;
		font-weight: 700;
		padding: 3px 8px;
		border-radius: var(--k-radius-sm);
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
	.badge.admin {
		background: rgba(95, 200, 207, 0.15);
		color: var(--k-accent-teal);
	}
	.badge.ok {
		background: rgba(127, 211, 154, 0.15);
		color: var(--k-ongoing);
	}
	.badge.danger {
		background: rgba(240, 128, 138, 0.15);
		color: #f0808a;
	}
	.actions-col {
		text-align: right;
	}
	.actions {
		display: flex;
		gap: 8px;
		justify-content: flex-end;
	}
	.act {
		height: 32px;
		padding: 0 12px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface);
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		font-family: var(--k-font-sans);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		white-space: nowrap;
		transition: all 0.15s;
	}
	.act:hover:not(:disabled) {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.act.danger:hover:not(:disabled) {
		border-color: rgba(240, 128, 138, 0.5);
		color: #f0808a;
	}
	.act:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.pager {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 16px;
	}
	.pg {
		height: 36px;
		padding: 0 16px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface);
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
	}
	.pg:disabled {
		opacity: 0.4;
		cursor: default;
	}
	.pg-num {
		font-size: 13px;
		color: var(--k-text-dim);
	}
</style>
