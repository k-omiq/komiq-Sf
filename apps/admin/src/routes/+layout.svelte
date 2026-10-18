<script lang="ts">
	import { href, routePath } from '$lib/paths';
	import '@fontsource-variable/bricolage-grotesque';
	import '@fontsource-variable/manrope';
	import '@komika/ui/reset.css';
	import '@komika/ui/tokens.css';
	import '@komika/ui/console.css';
	import { afterNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import { auth, initAuth, logout, watchSession } from '$lib/auth.svelte';
	import { needsServer, target } from '$lib/config';
	import { instance, loadInstance } from '$lib/instance.svelte';
	import { settleConfirm } from '$lib/confirm.svelte';
	import { theme, cycleTheme, initTheme } from '$lib/theme.svelte';
	import { refreshSourcesDown, sourceHealth } from '$lib/source-health.svelte';
	import { loadEngineGenerations } from '$lib/data';
	import Brand from '$lib/components/Brand.svelte';
	import Button from '$lib/components/Button.svelte';
	import ChoosePassword from '$lib/components/ChoosePassword.svelte';
	import CommandPalette from '$lib/components/CommandPalette.svelte';
	import ConfirmHost from '$lib/components/ConfirmHost.svelte';
	import Icon, { type IconName } from '$lib/components/Icon.svelte';
	import Toaster from '$lib/components/Toaster.svelte';

	// The route guard bounces the signed-out admin to /login immediately, so this
	// warning is rendered OUTSIDE the shell if-block to survive that redirect.
	let signoutWarning = $state(false);
	// It describes the sign-out that just happened; signing in again answers it.
	$effect(() => {
		if (auth.user) signoutWarning = false;
	});

	async function onSignOut(): Promise<void> {
		const r = await logout();
		if (!r.revoked) signoutWarning = true;
	}

	let { children } = $props();

	$effect(() => {
		void initAuth();
	});
	// Keep the session in step with the server: retries, focus, refused requests.
	$effect(() => watchSession());
	$effect(() => {
		initTheme();
	});

	const path = $derived(routePath(page.url.pathname));
	// An account with a password someone else chose (new or reset) picks its own
	// before anything else in the console opens.
	const mustChangePassword = $derived(!!auth.user?.mustChangePassword);
	const onLogin = $derived(path === '/login');
	// First-run screens: full page, no sidebar, reachable signed out.
	const onSetup = $derived(path === '/setup' || path === '/start');

	// Reuse the shell's warning notice. Health has the full recovery section itself.
	// The check does not track `path`, so navigating does not ask the server again.
	let engineQuarantined = $state(false);
	const onHealth = $derived(path === '/sources/health');
	$effect(() => {
		engineQuarantined = false;
		if (!auth.user || mustChangePassword || onLogin || onSetup) return;
		let cancelled = false;
		async function refreshEngineWarning(): Promise<void> {
			try {
				const rows = await loadEngineGenerations();
				if (!cancelled)
					engineQuarantined = rows?.some((row) => row.state === 'quarantined') ?? false;
			} catch {
				// Best effort; the Health page presents query failures with a retry.
			}
		}
		void refreshEngineWarning();
		const timer = setInterval(() => void refreshEngineWarning(), 5 * 60_000);
		return () => {
			cancelled = true;
			clearInterval(timer);
		};
	});

	// Self-host builds ask the server whether it still needs its owner. The hosted
	// console never does (its server always has admins), except on /setup itself,
	// which must know to send a visitor away.
	const checksSetup = target !== 'hosted';
	$effect(() => {
		if ((checksSetup || onSetup) && !needsServer && !instance.ready) void loadInstance();
	});
	const setupKnown = $derived(!checksSetup || needsServer || instance.ready);

	// Centralized route guard: once auth has settled, bounce signed-out visitors to
	// /login from any protected route (but never off /login itself). Individual pages
	// no longer duplicate this, which also removes the per-page empty-content flash.
	// Before that, the desktop app picks a server and a fresh server runs setup.
	$effect(() => {
		if (!auth.ready || !setupKnown) return;
		if (needsServer) {
			if (path !== '/start') goto(href('/start'), { replaceState: true });
			return;
		}
		if (checksSetup && instance.info?.setupRequired && !onSetup) {
			goto(href('/setup'), { replaceState: true });
			return;
		}
		if (!auth.user && !onLogin && !onSetup) goto(href('/login'));
	});

	const themeLabel = $derived(
		theme.pref === 'dark' ? 'Dark' : theme.pref === 'light' ? 'Light' : 'System',
	);
	const themeIcon = $derived<IconName>(
		theme.pref === 'dark' ? 'moon' : theme.pref === 'light' ? 'sun' : 'monitor',
	);

	interface NavItem {
		href: string;
		label: string;
		icon: IconName;
		/** Extra path prefixes that belong to this item (detail pages). */
		also?: string[];
		/** A count that needs attention, shown as a badge. */
		alarm?: number;
		alarmLabel?: string;
	}
	// Labels match each page's heading. Grouped by what the admin is looking after:
	// the catalogue itself, where it comes from, and the people using it.
	const groups = $derived<{ label: string; items: NavItem[] }[]>([
		{
			label: 'Library',
			items: [
				{ href: '/', label: 'Catalog', icon: 'book', also: ['/series'] },
				{ href: '/updates', label: 'Latest updates', icon: 'clock' },
				// The matcher's own uncertainty: pairs it could not decide on.
				{ href: '/review', label: 'Duplicates', icon: 'layers' },
				// Cover ranking, unpicked-cover cleanup and the cover pipeline's own failures.
				{ href: '/bugs', label: 'Covers', icon: 'image' },
			],
		},
		{
			label: 'Sources',
			items: [
				{ href: '/sources/extensions', label: 'Extensions', icon: 'package' },
				{ href: '/sources/browse', label: 'Browse & add', icon: 'compass' },
				// One title across every installed source at once.
				{ href: '/sources/search', label: 'Search all', icon: 'search' },
				{
					href: '/sources/health',
					label: 'Health',
					icon: 'activity',
					alarm: sourceHealth.down,
					alarmLabel: 'sources down',
				},
				// Which sources may supply each kind of series detail.
				{ href: '/sources/metadata', label: 'Metadata', icon: 'shield' },
				// The groups chapters are credited to, and which of them use AI.
				{ href: '/scanlators', label: 'Scanlators', icon: 'globe' },
			],
		},
		{
			label: 'Community',
			items: [
				{ href: '/users', label: 'Users', icon: 'users' },
				// Reader-filed reports: the queue a human filled in.
				{ href: '/reports', label: 'Reports', icon: 'flag' },
			],
		},
	]);
	const settingsItem: NavItem = { href: '/settings', label: 'Settings', icon: 'gear' };

	function isActive(item: NavItem, pathname: string): boolean {
		if (item.href === '/') {
			return pathname === '/' || (item.also ?? []).some((p) => pathname.startsWith(p));
		}
		return pathname === item.href || pathname.startsWith(item.href + '/');
	}

	// Off-canvas sidebar on narrow screens; always closed again after a navigation.
	let menuOpen = $state(false);
	afterNavigate((navigation) => {
		menuOpen = false;
		// A confirmation belongs to the page that asked. Left open across a page change it
		// would sit over the new page and, if accepted, act for a page that is gone.
		// (`from.url` is null for the entry that follows a reload.)
		const from = navigation.from?.url?.pathname;
		if (from !== undefined && from !== navigation.to?.url?.pathname) settleConfirm(false);
	});

	// The sidebar alarm: checked at sign-in and then every few minutes.
	$effect(() => {
		if (!auth.user || mustChangePassword) return;
		void refreshSourcesDown();
		const timer = setInterval(() => void refreshSourcesDown(), 5 * 60_000);
		return () => clearInterval(timer);
	});

	// ---- quick search (Ctrl/Cmd+K) --------------------------------------------
	let paletteOpen = $state(false);
	const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform);
	const palettePages = $derived([
		...groups.flatMap((g) => g.items.map((i) => ({ href: i.href, label: i.label, icon: i.icon }))),
		{ href: settingsItem.href, label: settingsItem.label, icon: settingsItem.icon },
		...(
			[
				['appearance', 'Theme'],
				['content', 'NSFW content'],
				['blocked', 'Blocked content'],
				['repos', 'Extension repos'],
				['updates', 'Extension updates'],
				['maintenance', 'Maintenance jobs'],
			] as const
		).map(([id, label]) => ({
			href: `/settings#${id}`,
			label,
			icon: 'gear' as IconName,
			hint: 'Settings',
		})),
	]);

	function onWindowKeydown(e: KeyboardEvent): void {
		if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 'k') {
			if (!auth.user || onLogin || mustChangePassword) return;
			e.preventDefault();
			// Never over another dialog: jumping away from an open confirmation or an
			// unsaved drawer would skip the question it is asking.
			if (!paletteOpen && document.querySelector('[aria-modal="true"]')) return;
			paletteOpen = !paletteOpen;
		} else if (e.key === 'Escape' && menuOpen) {
			menuOpen = false;
		}
	}
</script>

<svelte:head>
	<title>Komiq admin</title>
	<link rel="icon" href={href('/favicon.ico')} sizes="any" />
	<link rel="icon" type="image/png" sizes="32x32" href={href('/favicon-32.png')} />
	<link rel="icon" type="image/png" sizes="16x16" href={href('/favicon-16.png')} />
	<link rel="apple-touch-icon" href={href('/apple-touch-icon.png')} />
	<link rel="manifest" href={href('/manifest.webmanifest')} />
	<meta name="theme-color" content="#0c0c0d" />
</svelte:head>

<svelte:window onkeydown={onWindowKeydown} />

{#snippet navLink(item: NavItem)}
	<!-- Closes the phone drawer itself: a link to the page already open does not navigate,
	     so `afterNavigate` never runs for it. -->
	<a
		href={href(item.href)}
		class="nav-link"
		aria-current={isActive(item, routePath(page.url.pathname)) ? 'page' : undefined}
		onclick={() => (menuOpen = false)}
	>
		<Icon name={item.icon} size={17} />
		<span>{item.label}</span>
		{#if item.alarm}
			<span class="alarm k-num" aria-label="{item.alarm} {item.alarmLabel}">{item.alarm}</span>
		{/if}
	</a>
{/snippet}

{#if !auth.ready || !setupKnown}
	<div class="boot" role="status">Loading admin console…</div>
{:else}
	<a class="skip-link" href="#main-content">Skip to main content</a>
	{#if signoutWarning}
		<p class="signout-warning" role="status">
			You are signed out on this device. The server could not be reached, so the session may still
			be valid there.
		</p>
	{/if}
	{#if onSetup}
		<main id="main-content" class="admin-main full" tabindex="-1">
			{@render children()}
		</main>
	{:else if auth.user && mustChangePassword}
		<main id="main-content" class="admin-main full" tabindex="-1">
			<ChoosePassword />
		</main>
	{:else if auth.user && !onLogin}
		<div class="shell">
			<header class="topbar">
				<Button
					variant="ghost"
					icon="menu"
					iconOnly
					aria-label="Open menu"
					aria-expanded={menuOpen}
					aria-controls="admin-sidebar"
					onclick={() => (menuOpen = true)}
				/>
				<a href={href('/')} class="brand" aria-label="Komiq admin home">
					<Brand />
				</a>
			</header>

			{#if menuOpen}
				<div class="scrim" aria-hidden="true" onclick={() => (menuOpen = false)}></div>
			{/if}

			<aside id="admin-sidebar" class="sidebar" class:open={menuOpen}>
				<a href={href('/')} class="brand" aria-label="Komiq admin home">
					<Brand />
					<span class="brand-tag">Admin</span>
				</a>

				<button type="button" class="search" onclick={() => (paletteOpen = true)}>
					<Icon name="search" size={15} />
					<span>Search</span>
					<span class="k-kbd">{isMac ? 'Cmd' : 'Ctrl'} K</span>
				</button>

				<nav class="nav" aria-label="Admin navigation">
					{#each groups as group (group.label)}
						<div class="group">
							<p class="group-label">{group.label}</p>
							{#each group.items as item (item.href)}
								{@render navLink(item)}
							{/each}
						</div>
					{/each}
					<div class="group bottom">
						{@render navLink(settingsItem)}
					</div>
				</nav>

				{#if !auth.user.showNsfw}
					<a class="nsfw-flag" href={href('/settings#content')} onclick={() => (menuOpen = false)}>
						<Icon name="eye-off" size={14} />
						<span>NSFW off: source tools are blocked</span>
					</a>
				{/if}

				<div class="account">
					<span class="avatar" aria-hidden="true">{auth.user.username.slice(0, 1)}</span>
					<div class="who">
						<span class="username k-truncate">{auth.user.username}</span>
						<span class="role">Admin</span>
					</div>
					<Button
						variant="ghost"
						size="sm"
						icon={themeIcon}
						iconOnly
						title="Theme: {themeLabel}"
						aria-label="Theme: {themeLabel}. Change theme"
						onclick={cycleTheme}
					/>
					<Button
						variant="ghost"
						size="sm"
						icon="logout"
						iconOnly
						title="Sign out"
						aria-label="Sign out"
						onclick={onSignOut}
					/>
				</div>
			</aside>

			<main id="main-content" class="admin-main" tabindex="-1">
				{#if engineQuarantined && !onHealth}
					<p class="signout-warning engine-warning" role="status">
						Source engine data is quarantined. Extension-source chapters, pages and thumbnails are
						unavailable.
						<a class="k-link" href={href('/sources/health#engine-generation')}
							>Review engine state</a
						>
					</p>
				{/if}
				{@render children()}
			</main>
		</div>
	{:else if onLogin}
		<main id="main-content" class="admin-main full" tabindex="-1">
			{@render children()}
		</main>
	{:else}
		<!-- Signed out on a protected route: the guard above is redirecting. Render
		     nothing of the page, so it neither flashes nor fires its queries. -->
		<div class="boot" role="status">Redirecting to sign in…</div>
	{/if}
	{#if paletteOpen}
		<CommandPalette pages={palettePages} onclose={() => (paletteOpen = false)} />
	{/if}
	<ConfirmHost />
	<Toaster />
{/if}

<style>
	.boot {
		display: grid;
		place-items: center;
		min-height: 100dvh;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
	}
	.signout-warning {
		margin: var(--k-space-3) var(--k-space-4) 0;
		padding: 10px 14px;
		border: 1px solid var(--k-warning-border);
		border-radius: var(--k-radius);
		background: var(--k-warning-soft);
		color: var(--k-warning);
		font-size: var(--k-text-sm);
	}
	.engine-warning {
		margin: 0 0 var(--k-space-5);
		border: 0;
		line-height: 1.6;
	}

	.shell {
		--sidebar-w: 244px;
		display: grid;
		grid-template-columns: var(--sidebar-w) minmax(0, 1fr);
		min-height: 100dvh;
	}
	.topbar {
		display: none;
	}

	/* ---- sidebar ---- */
	.sidebar {
		position: sticky;
		top: 0;
		display: flex;
		flex-direction: column;
		gap: var(--k-space-4);
		height: 100dvh;
		padding: var(--k-space-5) var(--k-space-3) var(--k-space-3);
		background: var(--k-surface);
		border-right: 1px solid var(--k-border-1);
		overflow-y: auto;
		overscroll-behavior: contain;
	}
	.brand {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 0 var(--k-space-3);
	}
	.brand-tag {
		padding: 2px 7px;
		border: 1px solid var(--k-accent-border);
		border-radius: var(--k-radius-pill);
		background: var(--k-accent-soft);
		color: var(--k-accent);
		font-family: var(--k-font-sans);
		font-size: 10.5px;
		font-weight: 700;
		letter-spacing: 0.04em;
		text-transform: uppercase;
	}
	.search {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		height: var(--k-control);
		padding: 0 var(--k-space-2) 0 var(--k-space-3);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		cursor: pointer;
		transition:
			border-color var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.search:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text-1);
	}
	.search span:first-of-type {
		flex: 1 1 auto;
		text-align: left;
	}
	.alarm {
		margin-left: auto;
		min-width: 20px;
		padding: 1px 6px;
		border-radius: var(--k-radius-pill);
		background: var(--k-danger-soft);
		border: 1px solid var(--k-danger-border);
		color: var(--k-danger);
		font-size: 11px;
		font-weight: 700;
		text-align: center;
	}
	.nav {
		flex: 1 1 auto;
		display: flex;
		flex-direction: column;
		gap: var(--k-space-5);
	}
	.group {
		display: grid;
		gap: 2px;
	}
	.group.bottom {
		margin-top: auto;
	}
	.group-label {
		padding: 0 var(--k-space-3) 6px;
		color: var(--k-text-dim);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.07em;
		text-transform: uppercase;
	}
	.nav-link {
		display: flex;
		align-items: center;
		gap: 11px;
		min-height: 36px;
		padding: 0 var(--k-space-3);
		border-radius: var(--k-radius);
		color: var(--k-text-muted);
		font-size: var(--k-text-md);
		font-weight: 600;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.nav-link:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
	}
	.nav-link[aria-current='page'] {
		background: var(--k-surface-5);
		color: var(--k-text-bright);
	}
	.nav-link[aria-current='page'] :global(svg) {
		color: var(--k-accent);
	}
	.nsfw-flag {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		padding: 9px var(--k-space-3);
		border: 1px solid var(--k-warning-border);
		border-radius: var(--k-radius);
		background: var(--k-warning-soft);
		color: var(--k-warning);
		font-size: var(--k-text-xs);
		font-weight: 600;
		line-height: 1.4;
	}
	.account {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		padding: var(--k-space-3) var(--k-space-2) 0 var(--k-space-3);
		border-top: 1px solid var(--k-border);
	}
	.avatar {
		flex: none;
		display: grid;
		place-items: center;
		width: 30px;
		height: 30px;
		border-radius: 50%;
		background: var(--k-surface-5);
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		font-weight: 700;
		text-transform: uppercase;
	}
	.who {
		flex: 1 1 auto;
		display: grid;
		min-width: 0;
		line-height: 1.25;
	}
	.username {
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		font-weight: 700;
	}
	.role {
		color: var(--k-text-dim);
		font-size: 11.5px;
	}

	/* ---- main ---- */
	.admin-main {
		min-width: 0;
		width: 100%;
		max-width: 1360px;
		margin: 0 auto;
		padding: var(--k-space-8) var(--k-space-10) 72px;
		scroll-margin-top: 16px;
	}
	.admin-main:focus {
		outline: none;
	}
	.admin-main.full {
		max-width: none;
		padding: 0;
	}

	/* ---- narrow: sidebar becomes an off-canvas drawer ---- */
	@media (max-width: 900px) {
		.shell {
			grid-template-columns: minmax(0, 1fr);
			grid-template-rows: auto 1fr;
		}
		.topbar {
			position: sticky;
			top: 0;
			z-index: 50;
			display: flex;
			align-items: center;
			gap: var(--k-space-2);
			height: 56px;
			padding: 0 var(--k-space-3);
			background: var(--k-glass-strong);
			backdrop-filter: blur(12px);
			border-bottom: 1px solid var(--k-border-1);
		}
		.topbar .brand {
			padding: 0;
		}
		.sidebar {
			position: fixed;
			inset: 0 auto 0 0;
			z-index: 120;
			width: min(86vw, 290px);
			box-shadow: var(--k-shadow-3);
			transform: translateX(-100%);
			visibility: hidden;
			transition:
				transform 220ms var(--k-ease),
				visibility 220ms;
		}
		.sidebar.open {
			transform: none;
			visibility: visible;
		}
		.scrim {
			position: fixed;
			inset: 0;
			z-index: 110;
			background: var(--k-scrim);
		}
		.admin-main {
			padding: var(--k-space-5) var(--k-space-4) 56px;
		}
		.nav-link {
			min-height: 44px;
		}
	}
</style>
