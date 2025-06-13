<script lang="ts">
	// Mobile primary navigation: a bottom tab bar (the common reader-app pattern)
	// that replaces the desktop inline nav on small screens. Hidden ≥641px, where
	// the header's inline nav takes over. Respects the home-indicator safe area.
	import { page } from '$app/state';

	const path = $derived(page.url.pathname);
	function isActive(href: string): boolean {
		return href === '/' ? path === '/' : path.startsWith(href);
	}

	const tabs = [
		{ href: '/', label: 'Home', icon: 'home' },
		{ href: '/browse', label: 'Browse', icon: 'browse' },
		{ href: '/updates', label: 'Updates', icon: 'updates' },
		{ href: '/library', label: 'Library', icon: 'library' },
	] as const;
</script>

<nav class="bottom-nav" aria-label="Primary">
	{#each tabs as t (t.href)}
		<a href={t.href} class="tab" class:active={isActive(t.href)} aria-current={isActive(t.href) ? 'page' : undefined}>
			<span class="ic" aria-hidden="true">
				{#if t.icon === 'home'}
					<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 10.5 12 3l9 7.5" /><path d="M5 9.5V21h5v-6h4v6h5V9.5" /></svg>
				{:else if t.icon === 'browse'}
					<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="7" /><path d="m21 21-4.3-4.3" /></svg>
				{:else if t.icon === 'updates'}
					<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 8a6 6 0 1 0-12 0c0 7-3 9-3 9h18s-3-2-3-9" /><path d="M13.7 21a2 2 0 0 1-3.4 0" /></svg>
				{:else}
					<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3h12a1 1 0 0 1 1 1v17l-7-4-7 4V4a1 1 0 0 1 1-1z" /></svg>
				{/if}
			</span>
			<span class="lbl">{t.label}</span>
		</a>
	{/each}
</nav>

<style>
	.bottom-nav {
		display: none;
	}
	@media (max-width: 640px) {
		.bottom-nav {
			position: fixed;
			left: 0;
			right: 0;
			bottom: 0;
			z-index: 40;
			display: grid;
			grid-template-columns: repeat(4, 1fr);
			background: var(--k-glass-strong, var(--k-glass));
			backdrop-filter: blur(16px);
			border-top: 1px solid var(--k-border);
			padding-bottom: env(safe-area-inset-bottom);
		}
		.tab {
			display: flex;
			flex-direction: column;
			align-items: center;
			justify-content: center;
			gap: 3px;
			min-height: 56px;
			padding: 7px 0 6px;
			text-decoration: none;
			color: var(--k-text-faint);
			transition: color 0.15s;
			-webkit-tap-highlight-color: transparent;
		}
		.tab.active {
			color: var(--k-text-bright);
		}
		.ic {
			width: 24px;
			height: 24px;
			display: inline-flex;
		}
		.ic svg {
			width: 100%;
			height: 100%;
		}
		.lbl {
			font-size: 10.5px;
			font-weight: 700;
			letter-spacing: 0.01em;
		}
	}
</style>
