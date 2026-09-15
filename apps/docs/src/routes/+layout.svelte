<script lang="ts">
	import '@fontsource-variable/bricolage-grotesque';
	import '@fontsource-variable/manrope';
	import '@komika/ui/reset.css';
	import '@komika/ui/tokens.css';
	import '@komika/ui/console.css';
	import '$lib/docs.css';
	import { afterNavigate } from '$app/navigation';
	import { page } from '$app/state';
	import { searchUi } from '$lib/search-ui.svelte';
	import { SITE_NAME } from '$lib/site';
	import { theme, cycleTheme, initTheme } from '$lib/theme.svelte';
	import Brand from '$lib/components/Brand.svelte';
	import Icon, { type IconName } from '$lib/components/Icon.svelte';
	import Search from '$lib/components/Search.svelte';

	let { data, children } = $props();

	$effect(() => {
		initTheme();
	});

	const path = $derived(page.url.pathname.replace(/\/+$/, '') || '/');
	// The handbook the reader is in. Its chapters are the only ones the menu lists.
	const current = $derived(
		data.handbooks.find((b) => path === b.href || path.startsWith(b.href + '/')),
	);

	const themeLabel = $derived(
		theme.pref === 'dark' ? 'Dark' : theme.pref === 'light' ? 'Light' : 'System',
	);
	const themeIcon = $derived<IconName>(
		theme.pref === 'dark' ? 'moon' : theme.pref === 'light' ? 'sun' : 'monitor',
	);

	// Off-canvas menu on narrow screens; closed again after every navigation.
	let menuOpen = $state(false);
	let sidebar = $state<HTMLElement>();
	afterNavigate(() => {
		menuOpen = false;
		// A long handbook can push the current chapter below the sidebar's fold.
		sidebar?.querySelector('.chapters [aria-current="page"]')?.scrollIntoView({ block: 'nearest' });
	});

	// One listener for every "Copy" button in rendered Markdown, on any page.
	async function onMainClick(e: MouseEvent): Promise<void> {
		const button = (e.target as HTMLElement).closest<HTMLButtonElement>('[data-copy]');
		const code = button?.parentElement?.querySelector('pre')?.innerText;
		if (!button || code === undefined) return;
		const label = button.querySelector('span');
		try {
			await navigator.clipboard.writeText(code);
			button.dataset.copied = '';
			if (label) label.textContent = 'Copied';
		} catch {
			if (label) label.textContent = 'Copy failed';
		}
		setTimeout(() => {
			delete button.dataset.copied;
			if (label) label.textContent = 'Copy';
		}, 1600);
	}

	// Shown in the key hint. Read after mount, so the prerendered HTML is the same
	// for everyone and only the hint text changes.
	let isMac = $state(false);
	$effect(() => {
		isMac = /Mac|iPhone|iPad/.test(navigator.platform);
	});

	function onWindowKeydown(e: KeyboardEvent): void {
		if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 'k') {
			e.preventDefault();
			searchUi.open = !searchUi.open;
		} else if (e.key === '/' && !searchUi.open && !isTyping(e.target)) {
			e.preventDefault();
			searchUi.open = true;
		} else if (e.key === 'Escape' && menuOpen) {
			menuOpen = false;
		}
	}
	function isTyping(target: EventTarget | null): boolean {
		return (
			target instanceof HTMLElement &&
			(target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))
		);
	}
</script>

<svelte:head>
	<link rel="icon" href="/favicon.ico" sizes="any" />
	<link rel="icon" type="image/png" sizes="32x32" href="/favicon-32.png" />
	<link rel="icon" type="image/png" sizes="16x16" href="/favicon-16.png" />
	<link rel="apple-touch-icon" href="/apple-touch-icon.png" />
	<meta name="theme-color" content="#0c0c0d" />
	<meta property="og:site_name" content={SITE_NAME} />
	<meta property="og:type" content="website" />
</svelte:head>

<svelte:window onkeydown={onWindowKeydown} />

<a class="skip-link" href="#main-content">Skip to main content</a>

<div class="shell">
	<header class="topbar">
		<button
			type="button"
			class="k-btn k-btn--ghost k-btn--icon"
			aria-label="Open menu"
			aria-expanded={menuOpen}
			aria-controls="docs-sidebar"
			onclick={() => (menuOpen = true)}
		>
			<Icon name="menu" />
		</button>
		<a href="/" class="brand" aria-label="komiq docs home">
			<Brand />
			<span class="brand-tag">Docs</span>
		</a>
		<button
			type="button"
			class="k-btn k-btn--ghost k-btn--icon topbar-search"
			aria-label="Search the docs"
			onclick={() => (searchUi.open = true)}
		>
			<Icon name="search" />
		</button>
	</header>

	{#if menuOpen}
		<div class="scrim" aria-hidden="true" onclick={() => (menuOpen = false)}></div>
	{/if}

	<aside id="docs-sidebar" class="sidebar" class:open={menuOpen} bind:this={sidebar}>
		<a href="/" class="brand" aria-label="komiq docs home">
			<Brand />
			<span class="brand-tag">Docs</span>
		</a>

		<button type="button" class="search" onclick={() => (searchUi.open = true)}>
			<Icon name="search" size={15} />
			<span>Search</span>
			<span class="k-kbd">{isMac ? 'Cmd' : 'Ctrl'} K</span>
		</button>

		<nav class="nav" aria-label="Handbooks">
			<ul class="books">
				{#each data.handbooks as book (book.id)}
					{@const open = current?.id === book.id}
					<li>
						<a
							href={book.href}
							class="nav-link"
							class:open
							aria-current={path === book.href ? 'page' : undefined}
						>
							<Icon name={book.icon} size={17} />
							<span>{book.title}</span>
							<span class="count k-num" aria-label="{book.chapters.length} chapters"
								>{book.chapters.length}</span
							>
						</a>
						{#if open}
							<ol class="chapters">
								{#each book.chapters as chapter (chapter.slug)}
									<li>
										<a href={chapter.href} aria-current={path === chapter.href ? 'page' : undefined}
											>{chapter.title}</a
										>
									</li>
								{/each}
							</ol>
						{/if}
					</li>
				{/each}
			</ul>
		</nav>

		<div class="foot">
			<a class="site-link" href="https://komiq.cc" rel="noopener noreferrer">
				<span>komiq.cc</span>
				<Icon name="external" size={13} />
			</a>
			<button
				type="button"
				class="k-btn k-btn--ghost k-btn--sm k-btn--icon"
				title="Theme: {themeLabel}"
				aria-label="Theme: {themeLabel}. Change theme"
				onclick={cycleTheme}
			>
				<Icon name={themeIcon} size={14} />
			</button>
		</div>
	</aside>

	<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
	<main id="main-content" class="main" tabindex="-1" onclick={onMainClick}>
		{@render children()}
	</main>
</div>

<Search bind:open={searchUi.open} handbooks={data.handbooks} />

<style>
	.shell {
		--sidebar-w: 264px;
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
		color: var(--doc-link);
		font-size: 10.5px;
		font-weight: 700;
		letter-spacing: 0.04em;
		text-transform: uppercase;
	}
	.search {
		flex: none;
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
	.nav {
		flex: 1 1 auto;
	}
	.books,
	.chapters {
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.books {
		display: grid;
		gap: 2px;
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
	.nav-link.open {
		color: var(--k-text-bright);
	}
	.nav-link.open :global(svg) {
		color: var(--k-accent);
	}
	.nav-link[aria-current='page'] {
		background: var(--k-surface-5);
	}
	.count {
		margin-left: auto;
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		font-weight: 600;
	}
	/* The open handbook's chapters hang off a hairline under its icon. */
	.chapters {
		display: grid;
		gap: 1px;
		margin: 4px 0 10px 20px;
		padding-left: 11px;
		border-left: 1px solid var(--k-border-2);
	}
	.chapters a {
		position: relative;
		display: flex;
		align-items: center;
		min-height: 32px;
		padding: 4px var(--k-space-3);
		border-radius: var(--k-radius);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		font-weight: 500;
		line-height: 1.35;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.chapters a:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
	}
	.chapters a[aria-current='page'] {
		background: var(--k-surface-5);
		color: var(--k-text-bright);
		font-weight: 600;
	}
	/* The current chapter marks its place on the hairline. */
	.chapters a[aria-current='page']::before {
		content: '';
		position: absolute;
		left: -15px;
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--k-accent);
		box-shadow: 0 0 0 3px var(--k-surface);
	}
	.foot {
		flex: none;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-2);
		padding: var(--k-space-3) var(--k-space-2) 0 var(--k-space-3);
		border-top: 1px solid var(--k-border);
	}
	.site-link {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.site-link:hover {
		color: var(--k-text-bright);
	}

	@media (pointer: coarse) {
		.search .k-kbd {
			display: none;
		}
	}

	/* ---- main ---- */
	.main {
		min-width: 0;
		width: 100%;
		max-width: 1180px;
		margin: 0 auto;
		padding: 56px var(--k-space-10) 96px;
	}
	.main:focus {
		outline: none;
	}

	/* ---- narrow: the sidebar becomes an off-canvas drawer ---- */
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
		.topbar-search {
			margin-left: auto;
		}
		.sidebar {
			position: fixed;
			inset: 0 auto 0 0;
			z-index: 120;
			width: min(86vw, 300px);
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
		.main {
			padding: var(--k-space-8) var(--k-space-4) 72px;
		}
		.nav-link {
			min-height: 44px;
		}
		.chapters a {
			min-height: 40px;
		}
	}
</style>
