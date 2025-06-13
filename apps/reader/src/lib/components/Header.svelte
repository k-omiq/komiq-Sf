<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import mark from '$lib/assets/mark.png';
	import { auth, logout } from '$lib/auth.svelte';
	import { config } from '$lib/config';
	import { theme, cycleTheme, resolvedTheme } from '$lib/theme.svelte';
	import Icon from './Icon.svelte';
	import Avatar from './Avatar.svelte';
	import SearchOverlay from './SearchOverlay.svelte';
	import NotificationBell from './NotificationBell.svelte';

	let { searchPlaceholder = 'Search series, authors, genres…' }: { searchPlaceholder?: string } =
		$props();

	let searchOpen = $state(false);
	let menuOpen = $state(false);

	const nav = [
		{ href: '/', label: 'Home' },
		{ href: '/browse', label: 'Browse' },
		{ href: '/updates', label: 'Updates' },
		{ href: '/library', label: 'Library' },
	];

	const path = $derived(page.url.pathname);
	function isActive(href: string): boolean {
		return href === '/' ? path === '/' : path.startsWith(href);
	}

	// Preserve the current page so the user lands back here after signing in.
	const loginHref = $derived(
		path === '/login' ? '/login' : `/login?redirect=${encodeURIComponent(path)}`,
	);
	const isDark = $derived(resolvedTheme(theme.pref) === 'dark');
	const themeTitle = $derived(
		theme.pref === 'system' ? 'System' : theme.pref === 'dark' ? 'Dark' : 'Light',
	);

	async function signOut(): Promise<void> {
		menuOpen = false;
		await logout();
		if (path === '/profile' || path === '/library') await goto('/');
	}
</script>

<header>
	<div class="left">
		<a class="brand" href="/" aria-label="komiq">
			<img class="brand-mark" src={mark} alt="" />
			<span>omiq</span>
		</a>
		<nav>
			{#each nav as item (item.href)}
				<a href={item.href} class="nav-link" class:active={isActive(item.href)}>{item.label}</a>
			{/each}
		</nav>
	</div>
	<div class="right">
		<button
			class="icon-btn"
			aria-label="Theme: {themeTitle}"
			title="Theme: {themeTitle} (click to change)"
			onclick={cycleTheme}
		>
			{#if isDark}
				<svg
					width="19"
					height="19"
					viewBox="0 0 24 24"
					fill="none"
					stroke="currentColor"
					stroke-width="2"
					stroke-linecap="round"
					stroke-linejoin="round"
					aria-hidden="true"
				>
					<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
				</svg>
			{:else}
				<svg
					width="19"
					height="19"
					viewBox="0 0 24 24"
					fill="none"
					stroke="currentColor"
					stroke-width="2"
					stroke-linecap="round"
					aria-hidden="true"
				>
					<circle cx="12" cy="12" r="4" />
					<path
						d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"
					/>
				</svg>
			{/if}
		</button>
		<button class="icon-btn" aria-label="Search" onclick={() => (searchOpen = true)}>
			<Icon name="search" size={21} />
		</button>
		{#if !auth.ready && !auth.user}
			<div class="avatar skeleton" aria-hidden="true"></div>
		{:else if auth.user}
			<NotificationBell />
			<div class="account">
				<button
					class="avatar"
					class:on={menuOpen || isActive('/profile')}
					aria-label="Account menu"
					aria-haspopup="menu"
					aria-expanded={menuOpen}
					onclick={() => (menuOpen = !menuOpen)}
				>
					<Avatar
						url={auth.user.avatarUrl}
						name={auth.user.username}
						colorKey={auth.user.id}
						size={34}
					/>
				</button>
				{#if menuOpen}
					<button class="backdrop" aria-label="Close menu" onclick={() => (menuOpen = false)}
					></button>
					<div class="menu" role="menu">
						<div class="menu-id">
							<span class="menu-name">{auth.user.username}</span>
							{#if auth.user.isAdmin}<span class="menu-badge">Admin</span>{/if}
						</div>
						<a class="menu-item" role="menuitem" href="/profile" onclick={() => (menuOpen = false)}
							>Profile</a
						>
						<a class="menu-item" role="menuitem" href="/library" onclick={() => (menuOpen = false)}
							>Library</a
						>
						<a class="menu-item" role="menuitem" href="/support" onclick={() => (menuOpen = false)}
							>Support</a
						>
						{#if config.donateUrl}
							<a
								class="menu-item sponsor"
								role="menuitem"
								href={config.donateUrl}
								target="_blank"
								rel="noopener noreferrer"
								onclick={() => (menuOpen = false)}>Donate</a
							>
						{/if}
						<a class="menu-item" role="menuitem" href="/about" onclick={() => (menuOpen = false)}
							>About</a
						>
						<button class="menu-item danger" role="menuitem" onclick={signOut}>Sign out</button>
					</div>
				{/if}
			</div>
		{:else}
			<!-- Donate/Support/About used to be header icons. Folding them into the account menu
			     would have stranded signed-out visitors — the account menu does not exist until you
			     log in, and there is no footer — so anonymous users get their own menu with the
			     same entries. Most people who need /support (and the /about takedown route) are
			     signed out, so these must never become URL-only. -->
			<div class="account">
				<button
					class="icon-btn"
					class:on={menuOpen}
					aria-label="More"
					aria-haspopup="menu"
					aria-expanded={menuOpen}
					onclick={() => (menuOpen = !menuOpen)}
				>
					<svg width="19" height="19" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
						<circle cx="5" cy="12" r="1.9" />
						<circle cx="12" cy="12" r="1.9" />
						<circle cx="19" cy="12" r="1.9" />
					</svg>
				</button>
				{#if menuOpen}
					<button class="backdrop" aria-label="Close menu" onclick={() => (menuOpen = false)}
					></button>
					<div class="menu" role="menu">
						<a class="menu-item" role="menuitem" href="/support" onclick={() => (menuOpen = false)}
							>Support</a
						>
						{#if config.donateUrl}
							<a
								class="menu-item sponsor"
								role="menuitem"
								href={config.donateUrl}
								target="_blank"
								rel="noopener noreferrer"
								onclick={() => (menuOpen = false)}>Donate</a
							>
						{/if}
						<a class="menu-item" role="menuitem" href="/about" onclick={() => (menuOpen = false)}
							>About</a
						>
					</div>
				{/if}
			</div>
			<a class="signin" href={loginHref}>Sign in</a>
		{/if}
	</div>
</header>

<SearchOverlay bind:open={searchOpen} placeholder={searchPlaceholder} />

<style>
	header {
		position: sticky;
		top: 0;
		z-index: 20;
		display: flex;
		align-items: center;
		justify-content: space-between;
		height: var(--k-header-h);
		padding: 0 var(--k-gutter);
		background: var(--k-glass);
		backdrop-filter: blur(14px);
		border-bottom: 1px solid var(--k-border);
	}
	.left {
		display: flex;
		align-items: center;
		gap: 48px;
	}
	.brand {
		display: inline-flex;
		align-items: baseline;
		gap: 0;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 20px;
		letter-spacing: -0.01em;
		color: var(--k-text);
		text-decoration: none;
	}
	.brand-mark {
		width: auto;
		height: 0.8em;
		margin-right: 1.5px;
		transform: translateY(0.07em);
	}
	nav {
		display: flex;
		gap: 32px;
	}
	.nav-link {
		color: var(--k-text-dimmer);
		font-size: 14px;
		font-weight: 600;
		text-decoration: none;
		transition: color 0.15s;
	}
	.nav-link:hover,
	.nav-link.active {
		color: var(--k-text);
	}
	.right {
		display: flex;
		align-items: center;
		gap: 14px;
	}
	.icon-btn {
		width: 40px;
		height: 40px;
		padding: 0;
		border-radius: 50%;
		background: transparent;
		border: 1px solid var(--k-border-4);
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--k-text-dim);
		cursor: pointer;
		text-decoration: none;
		transition: all 0.15s;
	}
	.icon-btn:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.icon-btn.on {
		border-color: var(--k-border-strong);
		color: var(--k-text);
		background: var(--k-hover-fill);
	}
	.avatar {
		width: 36px;
		height: 36px;
		border-radius: 50%;
		background: var(--k-avatar);
		border: 1px solid var(--k-border-2);
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 0;
		overflow: hidden;
		font-weight: 700;
		font-size: 13px;
		color: var(--k-text-3);
		text-decoration: none;
		cursor: pointer;
		transition: all 0.15s;
	}
	.avatar:hover,
	.avatar.on {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.avatar.skeleton {
		background: var(--k-surface-4);
		border-color: var(--k-border);
	}
	.signin {
		display: flex;
		align-items: center;
		flex: 0 0 auto;
		white-space: nowrap;
		height: 36px;
		padding: 0 18px;
		border-radius: var(--k-radius-pill);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-size: 13px;
		font-weight: 700;
		text-decoration: none;
		transition: background 0.15s;
	}
	.signin:hover {
		background: var(--k-primary-hover);
	}
	.account {
		position: relative;
	}
	.backdrop {
		position: fixed;
		inset: 0;
		z-index: 25;
		background: transparent;
		border: none;
		cursor: default;
	}
	.menu {
		position: absolute;
		top: calc(100% + 10px);
		right: 0;
		z-index: 26;
		min-width: 190px;
		padding: var(--k-space-2);
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-lg);
		box-shadow: 0 16px 40px rgba(0, 0, 0, 0.5);
	}
	.menu-id {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		padding: var(--k-space-2) var(--k-space-3) var(--k-space-3);
		border-bottom: 1px solid var(--k-border);
		margin-bottom: var(--k-space-2);
	}
	.menu-name {
		font-size: 13px;
		font-weight: 700;
		color: var(--k-text-bright);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.menu-badge {
		font-size: 10px;
		font-weight: 700;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-accent-teal);
		border: 1px solid rgba(95, 200, 207, 0.4);
		border-radius: var(--k-radius-sm);
		padding: 1px 5px;
	}
	.menu-item {
		display: block;
		width: 100%;
		text-align: left;
		padding: 9px var(--k-space-3);
		border: none;
		border-radius: var(--k-radius);
		background: transparent;
		color: var(--k-text-2);
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 600;
		text-decoration: none;
		cursor: pointer;
		transition: background 0.12s;
	}
	.menu-item:hover {
		background: var(--k-hover-fill);
		color: var(--k-text);
	}
	.menu-item.danger:hover {
		color: var(--k-accent);
	}
	.menu-item.sponsor:hover {
		color: var(--k-accent);
	}
	/* 641–900px is the one band where EVERYTHING renders at once — brand, the four nav
	   links, the icon buttons and Sign in — and stops fitting. `justify-content:
	   space-between` has no slack to give and nothing shrinks, so at 768px `Library` slid
	   underneath the theme button and `Sign in` wrapped onto two lines. Tightening the two
	   generous desktop gaps is enough; no control has to be dropped. */
	@media (min-width: 641px) and (max-width: 900px) {
		.left {
			gap: 24px;
		}
		nav {
			gap: 20px;
		}
		.right {
			gap: 10px;
		}
	}
	@media (max-width: 640px) {
		/* Primary nav moves to the bottom tab bar; keep only the brand on the left. */
		nav {
			display: none;
		}
		.left {
			gap: 0;
		}
		.right {
			gap: 8px;
		}
		/* Support/Donate now live in a menu that renders for signed-out visitors too, so
		   nothing here may hide that menu button: it is the only route to /support and the
		   /about takedown page for anonymous users. */
		.icon-btn {
			width: 38px;
			height: 38px;
		}
	}
</style>
