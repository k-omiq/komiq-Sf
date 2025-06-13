<script lang="ts">
	import '@komika/ui/reset.css';
	import '@komika/ui/tokens.css';
	import mark from '$lib/assets/mark.png';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { auth, initAuth, logout, toggleNsfwVisibility } from '$lib/auth.svelte';
	import { theme, cycleTheme, initTheme } from '$lib/theme.svelte';

	// ---- NSFW visibility (the admin's own show_nsfw flag) ---------------------
	// Not a viewing preference here: with it off the server refuses source browsing,
	// extension install and source ingest. Surfaced in the header so the state is
	// never invisible.
	//
	// It no longer decides what the console LISTS: catalogue search and canonical
	// updates are read with a per-request `includeNsfw: true` override (see
	// ADMIN_INCLUDE_NSFW in $lib/data), which the server honours for admins only. The
	// two are deliberately distinct — this pill is the ACCOUNT PREFERENCE (what the
	// reader shows you, and what the write-side source APIs gate on), whereas the
	// override is scoped to this console's read paths so the ~2.5k mis-flagged
	// mainstream works stay reachable for correction either way. Folding the override
	// into the pill would have made "NSFW off" silently mean two different things.
	let nsfwBusy = $state(false);
	let nsfwError = $state<string | null>(null);

	async function onToggleNsfw(): Promise<void> {
		if (!auth.user || nsfwBusy) return;
		nsfwBusy = true;
		nsfwError = null;
		try {
			await toggleNsfwVisibility(!auth.user.showNsfw);
		} catch (err) {
			nsfwError = err instanceof Error ? err.message : 'Failed to change NSFW visibility.';
		} finally {
			nsfwBusy = false;
		}
	}

	let { children } = $props();

	$effect(() => {
		void initAuth();
	});
	$effect(() => {
		initTheme();
	});

	const onLogin = $derived(page.url.pathname === '/login');

	// Centralized route guard: once auth has settled, bounce signed-out visitors to
	// /login from any protected route (but never off /login itself). Individual pages
	// no longer duplicate this, which also removes the per-page empty-content flash.
	$effect(() => {
		if (auth.ready && !auth.user && !onLogin) goto('/login');
	});
	const themeLabel = $derived(
		theme.pref === 'dark' ? 'Dark' : theme.pref === 'light' ? 'Light' : 'System',
	);
	const nav = [
		{ href: '/', label: 'Catalog' },
		{ href: '/sources', label: 'Sources' },
		{ href: '/review', label: 'Review' },
		{ href: '/updates', label: 'Updates' },
		{ href: '/users', label: 'Users' },
		{ href: '/bugs', label: 'Bugs' },
		// Reader-filed reports. Distinct from Bugs (our cover pipeline's own failures) and
		// from Review (the matcher's own uncertainty) — this is the queue a human filled in.
		{ href: '/reports', label: 'Reports' },
	];
</script>

<svelte:head>
	<title>Komiq · manga DB</title>
	<link rel="icon" href="/favicon.ico" sizes="any" />
	<link rel="icon" type="image/png" sizes="32x32" href="/favicon-32.png" />
	<link rel="icon" type="image/png" sizes="16x16" href="/favicon-16.png" />
	<link rel="apple-touch-icon" href="/apple-touch-icon.png" />
	<link rel="manifest" href="/manifest.webmanifest" />
	<meta name="theme-color" content="#0c0c0d" />
</svelte:head>

{#if !auth.ready}
	<div class="boot">Loading…</div>
{:else}
	{#if auth.user && !onLogin}
		<header class="admin-header">
			<div class="brand">
				<img class="brand-mark" src={mark} alt="" />
				<span class="mark">KOMIQ</span>
				<span class="sub">manga DB</span>
				<nav class="nav">
					{#each nav as item (item.href)}
						<a
							href={item.href}
							class="nav-link"
							aria-current={page.url.pathname === item.href ? 'page' : undefined}>{item.label}</a
						>
					{/each}
				</nav>
			</div>
			<div class="who">
				<button
					class="nsfw-toggle"
					class:off={!auth.user.showNsfw}
					disabled={nsfwBusy}
					onclick={onToggleNsfw}
					title={auth.user.showNsfw
						? 'Your ACCOUNT preference — NSFW visible: source browsing, extension install and source ingest are unblocked. Click to opt out. (Catalog and Updates list NSFW-flagged works in this console either way.)'
						: 'Your ACCOUNT preference — NSFW hidden: the server refuses source browsing, extension install and source ingest. Click to opt in. (Catalog and Updates still list NSFW-flagged works in this console, so mis-flagged titles stay fixable.)'}
					>NSFW {nsfwBusy ? '…' : auth.user.showNsfw ? 'on' : 'off'}</button
				>
				<button
					class="theme-toggle"
					onclick={cycleTheme}
					title="Theme: {themeLabel} (click to change)"
					aria-label="Theme: {themeLabel}">{themeLabel}</button
				>
				<span class="username">{auth.user.username}</span>
				<span class="admin-badge">Admin</span>
				<button class="signout" onclick={logout}>Sign out</button>
			</div>
		</header>
		{#if nsfwError}
			<p class="nsfw-error">{nsfwError}</p>
		{/if}
	{/if}
	<main class:full={!auth.user || onLogin}>
		{@render children()}
	</main>
{/if}

<style>
	.boot {
		min-height: 100vh;
		display: grid;
		place-items: center;
		color: var(--k-text-dim);
		font-family: var(--k-font-sans);
	}
	.admin-header {
		position: sticky;
		top: 0;
		z-index: 20;
		display: flex;
		align-items: center;
		justify-content: space-between;
		height: 60px;
		padding: 0 var(--k-space-6);
		background: var(--k-glass);
		backdrop-filter: blur(14px);
		border-bottom: 1px solid var(--k-border);
	}
	.brand {
		display: flex;
		align-items: center;
		gap: 16px;
	}
	.brand-mark {
		display: block;
		width: auto;
		height: 18px;
	}
	.nav {
		display: flex;
		align-items: center;
		gap: 4px;
	}
	.nav-link {
		padding: 6px 12px;
		border-radius: var(--k-radius-pill);
		font-size: 13px;
		font-weight: 600;
		color: var(--k-text-dim);
		text-decoration: none;
		transition: all 0.15s;
	}
	.nav-link:hover {
		color: var(--k-text);
		background: var(--k-hover-fill);
	}
	.nav-link[aria-current='page'] {
		color: var(--k-text-bright);
		background: var(--k-surface-4);
	}
	.theme-toggle {
		height: 32px;
		padding: 0 14px;
		border-radius: var(--k-radius-pill);
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		font-family: var(--k-font-sans);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		transition: all 0.15s;
	}
	.theme-toggle:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.nsfw-toggle {
		height: 32px;
		padding: 0 14px;
		border-radius: var(--k-radius-pill);
		background: transparent;
		border: 1px solid rgba(95, 200, 207, 0.4);
		color: var(--k-accent-teal);
		font-family: var(--k-font-sans);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		transition: all 0.15s;
	}
	.nsfw-toggle.off {
		border-color: rgba(224, 179, 84, 0.45);
		color: var(--k-hiatus);
	}
	.nsfw-toggle:hover:not(:disabled) {
		border-color: var(--k-border-strong);
	}
	.nsfw-toggle:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.nsfw-error {
		margin: 0 auto;
		padding: 10px var(--k-space-6) 0;
		max-width: var(--k-max-content);
		font-family: var(--k-font-sans);
		font-size: 13px;
		color: #f0808a;
	}
	.mark {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		letter-spacing: 0.02em;
		color: var(--k-text-bright);
	}
	.sub {
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.who {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.username {
		font-size: 13px;
		font-weight: 600;
		color: var(--k-text-2);
	}
	.admin-badge {
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		color: var(--k-accent-teal);
		border: 1px solid rgba(95, 200, 207, 0.4);
		border-radius: var(--k-radius-sm);
		padding: 2px 6px;
	}
	.signout {
		height: 32px;
		padding: 0 14px;
		border-radius: var(--k-radius-pill);
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		font-family: var(--k-font-sans);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		transition: all 0.15s;
	}
	.signout:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	main {
		max-width: var(--k-max-content);
		margin: 0 auto;
		padding: var(--k-space-8) var(--k-space-6);
	}
	main.full {
		max-width: none;
		padding: 0;
	}
</style>
