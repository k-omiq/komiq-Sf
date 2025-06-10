<script lang="ts">
	import { auth } from '$lib/auth.svelte';
	import Icon from './Icon.svelte';
	import Avatar from './Avatar.svelte';
	import {
		getNotifications,
		getUnreadCount,
		markAllRead,
		type NotificationView,
	} from '$lib/data/notifications';

	let open = $state(false);
	let items = $state<NotificationView[]>([]);
	let unread = $state(0);
	let loading = $state(false);

	// Refresh the unread badge on first mount AND whenever the signed-in user changes —
	// login/logout are reactive here (no full reload), so an onMount-only fetch would
	// leave the badge stuck. Signing out closes the panel and clears state.
	$effect(() => {
		const uid = auth.user?.id;
		if (!uid) {
			open = false;
			items = [];
			unread = 0;
			return;
		}
		void refreshCount();
	});

	async function refreshCount() {
		unread = await getUnreadCount();
	}

	async function toggle() {
		open = !open;
		if (!open) return;
		// Opening the panel loads the list and marks everything read (clears the badge).
		loading = true;
		items = await getNotifications();
		loading = false;
		if (unread > 0) {
			const ok = await markAllRead();
			if (ok) {
				unread = 0;
				items = items.map((n) => ({ ...n, read: true }));
			}
		}
	}

	function close() {
		open = false;
	}
</script>

{#snippet body(n: NotificationView)}
	<span class="n-avatar">
		{#if n.actorName}
			<Avatar url={n.avatarUrl} name={n.actorName} colorKey={n.actorName} size={30} />
		{:else}
			<span class="n-emoji"><Icon name="thumbs-up" size={15} /></span>
		{/if}
	</span>
	<span class="n-text">
		<span class="n-msg">{n.text}</span>
		{#if n.excerpt}<span class="n-excerpt">“{n.excerpt}”</span>{/if}
		<span class="n-time">{n.time}</span>
	</span>
	{#if !n.read}<span class="n-dot" aria-hidden="true"></span>{/if}
{/snippet}

<div class="bell-wrap">
	<button
		class="icon-btn"
		class:on={open}
		aria-label={unread > 0 ? `Notifications, ${unread} unread` : 'Notifications'}
		aria-haspopup="menu"
		aria-expanded={open}
		title="Notifications"
		onclick={toggle}
	>
		<Icon name="bell" size={19} />
		{#if unread > 0}
			<span class="badge" aria-label={`${unread} unread`}>{unread > 99 ? '99+' : unread}</span>
		{/if}
	</button>

	{#if open}
		<button class="backdrop" aria-label="Close notifications" onclick={close}></button>
		<div class="panel" role="menu">
			<div class="panel-head">Notifications</div>
			{#if loading}
				<div class="empty">Loading…</div>
			{:else if items.length === 0}
				<div class="empty">
					<div class="empty-icon"><Icon name="bell" size={20} /></div>
					<span>No notifications yet</span>
					<span class="empty-sub">Replies and likes on your comments show up here.</span>
				</div>
			{:else}
				<ul class="list">
					{#each items as n (n.id)}
						<li>
							{#if n.href}
								<a
									class="n-item"
									class:unread={!n.read}
									role="menuitem"
									href={n.href}
									onclick={close}
								>
									{@render body(n)}
								</a>
							{:else}
								<div class="n-item static" class:unread={!n.read} role="menuitem">
									{@render body(n)}
								</div>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	{/if}
</div>

<style>
	.bell-wrap {
		position: relative;
		display: inline-flex;
	}
	.icon-btn {
		position: relative;
		width: 38px;
		height: 38px;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border-radius: 50%;
		border: none;
		background: transparent;
		color: var(--k-text-2);
		cursor: pointer;
		transition:
			background 0.15s,
			color 0.15s;
	}
	.icon-btn:hover,
	.icon-btn.on {
		background: var(--k-surface-2);
		color: var(--k-text-bright);
	}
	.badge {
		position: absolute;
		top: 2px;
		right: 2px;
		min-width: 16px;
		height: 16px;
		padding: 0 4px;
		border-radius: 999px;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-size: 10px;
		font-weight: 800;
		line-height: 16px;
		text-align: center;
		box-shadow: 0 0 0 2px var(--k-bg, #0c0c0d);
	}
	.backdrop {
		position: fixed;
		inset: 0;
		z-index: 60;
		border: none;
		background: transparent;
		cursor: default;
	}
	.panel {
		position: absolute;
		z-index: 61;
		top: calc(100% + 10px);
		right: 0;
		width: min(360px, calc(100vw - 32px));
		max-height: 460px;
		overflow-y: auto;
		background: var(--k-surface-3, var(--k-surface));
		border: 1px solid var(--k-border-3);
		border-radius: 14px;
		box-shadow: 0 20px 60px rgba(0, 0, 0, 0.5);
		padding: 6px;
	}
	.panel-head {
		padding: 10px 12px 8px;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 14px;
		color: var(--k-text-bright);
	}
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 6px;
		padding: 30px 20px;
		text-align: center;
		color: var(--k-text-dimmer);
		font-size: 13.5px;
	}
	.empty-icon {
		width: 40px;
		height: 40px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--k-surface-2);
		color: var(--k-text-disabled);
		margin-bottom: 4px;
	}
	.empty-sub {
		font-size: 12px;
		color: var(--k-text-faint);
		max-width: 220px;
	}
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.n-item {
		display: flex;
		align-items: flex-start;
		gap: 10px;
		padding: 10px;
		border-radius: 10px;
		text-decoration: none;
		color: inherit;
		cursor: pointer;
	}
	.n-item:hover {
		background: var(--k-hover-fill, rgba(255, 255, 255, 0.06));
	}
	.n-item.unread {
		background: var(--k-surface-2);
	}
	.n-avatar {
		flex: 0 0 auto;
	}
	.n-emoji {
		width: 30px;
		height: 30px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		background: rgba(95, 191, 126, 0.16);
		color: var(--k-primary);
	}
	.n-text {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
		flex: 1;
	}
	.n-msg {
		font-size: 13px;
		color: var(--k-text-1);
		line-height: 1.35;
	}
	.n-excerpt {
		font-size: 12px;
		color: var(--k-text-faint);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.n-time {
		font-size: 11.5px;
		color: var(--k-text-fainter);
	}
	.n-dot {
		flex: 0 0 auto;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--k-primary);
		margin-top: 6px;
	}
</style>
