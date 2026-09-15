<script lang="ts">
	import { searchUi } from '$lib/search-ui.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Meta from '$lib/components/Meta.svelte';

	let { data } = $props();

	type Path = 'web' | 'native';
	let path = $state<Path>('web');
	// The trace plays when it scrolls into view. It waits hidden only if it starts
	// below the fold, and only once this script is running.
	let trace = $state<HTMLElement>();
	let armed = $state(false);
	let replay = $state(false);
	$effect(() => {
		if (!trace || matchMedia('(prefers-reduced-motion: reduce)').matches) return;
		const seen = new IntersectionObserver(
			([entry]) => {
				if (!entry.isIntersecting) return;
				armed = false;
				replay = true;
				seen.disconnect();
			},
			{ threshold: 0.3 },
		);
		armed = trace.getBoundingClientRect().top > innerHeight;
		seen.observe(trace);
		return () => seen.disconnect();
	});
	const paths: { id: Path; label: string }[] = [
		{ id: 'web', label: 'On the web' },
		{ id: 'native', label: 'In the apps' },
	];

	// The ways to host the admin app, and the kinds of server the reader app takes.
	// The example addresses are the ones the reader app itself shows.
	const hosting = [
		{ href: '/self-host/desktop-app', label: 'On your own computer, with the desktop app' },
		{ href: '/self-host/docker', label: 'On a home server or a VPS, with Docker' },
		{ href: '/self-host/internet-access', label: 'Over the internet, with a domain or Cloudflare' },
		{ href: '/start/choose-your-setup', label: 'Compare the setups' },
	];
	const servers = [
		{ label: 'The global server', address: 'komiq.cc', example: false },
		{ label: 'A server on your network', address: '192.168.1.20:8080', example: true },
		{ label: 'A server on the internet', address: 'komiq.example.com', example: true },
	];

	const has = (href: string) => data.handbooks.some((b) => b.chapters.some((c) => c.href === href));
</script>

<Meta
	description="Set up your own komiq server, get help with the apps, and see how fetching, the image proxy and each platform work inside."
/>

<div class="home">
	<header class="hero">
		<h1>komiq docs</h1>
		<p class="lede">
			komiq is self-hostable. Host the admin app, connect the reader app to it, and see how the
			whole thing works inside.
		</p>
		<button type="button" class="find" onclick={() => (searchUi.open = true)}>
			<Icon name="search" size={18} />
			<span class="wide">Search pages, sections and commands</span>
			<span class="narrow">Search the docs</span>
			<span class="k-kbd">/</span>
		</button>
	</header>

	<section class="apps" aria-labelledby="apps-title">
		<h2 id="apps-title">Two apps</h2>
		<div class="pair">
			<div class="app">
				<h3>The admin app</h3>
				<p>The server and its management portal in one. You host it, in the way that suits you.</p>
				<ul>
					{#each hosting as way (way.href)}
						{#if has(way.href)}
							<li>
								<a href={way.href}>
									<span>{way.label}</span>
									<Icon name="arrow-right" size={15} />
								</a>
							</li>
						{/if}
					{/each}
				</ul>
			</div>
			<div class="app">
				<h3>The reader app</h3>
				<p>What you read with. It connects to a komiq server, and you choose which one.</p>
				<ul>
					{#each servers as server (server.label)}
						<li>
							<span class="kind">
								<span>{server.label}</span>
								<span class="address">
									{#if server.example}<span class="eg">e.g.</span>{/if}
									<code>{server.address}</code>
								</span>
							</span>
						</li>
					{/each}
					{#if has('/guide/connect')}
						<li>
							<a href="/guide/connect">
								<span>Connect to a server</span>
								<Icon name="arrow-right" size={15} />
							</a>
						</li>
					{/if}
				</ul>
			</div>
		</div>
	</section>

	<section class="books" aria-labelledby="books-title">
		<h2 id="books-title">Handbooks</h2>
		<ul>
			{#each data.handbooks as book (book.id)}
				<li class="book">
					<span class="mark"><Icon name={book.icon} size={18} /></span>
					<div class="what">
						<h3><a class="cover" href={book.href}>{book.title}</a></h3>
						<p>{book.description}</p>
					</div>
					<ul class="starts" aria-label="First chapters in {book.title}">
						{#each book.chapters.slice(0, 3) as chapter (chapter.slug)}
							<li><a href={chapter.href}>{chapter.title}</a></li>
						{/each}
					</ul>
					<span class="count k-num">
						<span>{book.chapters.length}<span class="unit">&nbsp;chapters</span></span>
						<Icon name="arrow-right" size={15} />
					</span>
				</li>
			{/each}
		</ul>
	</section>

	<section class="trace" aria-labelledby="trace-title">
		<div class="trace-head">
			<div>
				<h2 id="trace-title">How a page reaches your screen</h2>
				<p>
					komiq is not built to keep an archive of page images. Each one is fetched when you open
					it.
				</p>
			</div>
			<div class="k-segmented" role="group" aria-label="Where you are reading">
				{#each paths as option (option.id)}
					<button type="button" aria-pressed={path === option.id} onclick={() => (path = option.id)}
						>{option.label}</button
					>
				{/each}
			</div>
		</div>
		<div class="prose trace-flow" class:armed class:replay bind:this={trace} aria-live="polite">
			{#key path}
				<!-- Rendered at build time by lib/server/content.ts from a fixed list. -->
				{@html data.traces[path]}
			{/key}
		</div>
		{#if has('/developers/fetching')}
			<a class="more" href="/developers/fetching">
				How fetching works, platform by platform
				<Icon name="arrow-right" size={15} />
			</a>
		{/if}
	</section>

	<footer class="foot">
		<p>
			komiq is open source under AGPL-3.0-only. It is reader software, not a publisher or a content
			service, and it is not affiliated with any publisher or source.
		</p>
		<ul>
			<li><a href="https://komiq.cc" rel="noopener noreferrer">komiq.cc</a></li>
			{#if has('/guide/privacy')}<li><a href="/guide/privacy">Privacy</a></li>{/if}
			{#if has('/guide/copyright')}<li><a href="/guide/copyright">Copyright</a></li>{/if}
		</ul>
	</footer>
</div>

<style>
	.home {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 88px;
		max-width: 980px;
	}
	h2 {
		font-size: var(--k-text-2xl);
		font-weight: 700;
		letter-spacing: -0.03em;
	}

	/* ---- opening ---- */
	.hero {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		justify-items: start;
		gap: var(--k-space-4);
		margin-bottom: -40px;
	}
	h1 {
		font-size: clamp(40px, 7vw, 68px);
		font-weight: 700;
		letter-spacing: -0.04em;
		line-height: 1;
	}
	.lede {
		max-width: 46ch;
		color: var(--k-text-muted);
		font-size: clamp(17px, 2vw, 20px);
		line-height: 1.5;
	}
	.find {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		width: min(100%, 560px);
		height: 52px;
		margin-top: var(--k-space-3);
		padding: 0 var(--k-space-3) 0 var(--k-space-4);
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-2);
		color: var(--k-text-dim);
		font-size: var(--k-text-lg);
		text-align: left;
		cursor: pointer;
		transition:
			border-color var(--k-dur) var(--k-ease),
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.find:hover {
		border-color: var(--k-border-strong);
		background: var(--k-surface-4);
		color: var(--k-text-1);
	}
	.find .narrow {
		display: none;
	}
	.find span:not(.k-kbd) {
		flex: 1 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* ---- the two apps ---- */
	.apps {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--k-space-5);
	}
	.pair {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		border-top: 1px solid var(--k-border-2);
	}
	.app {
		padding: var(--k-space-6) var(--k-space-8) var(--k-space-4) 0;
	}
	.app + .app {
		padding-left: var(--k-space-8);
		padding-right: 0;
		border-left: 1px solid var(--k-border-2);
	}
	.app h3 {
		font-size: 21px;
		font-weight: 700;
		letter-spacing: -0.025em;
	}
	.app p {
		max-width: 44ch;
		margin-top: 6px;
		color: var(--k-text-muted);
		font-size: var(--k-text-lg);
		line-height: 1.55;
	}
	.app ul {
		margin: var(--k-space-4) 0 0;
		padding: 0;
		list-style: none;
	}
	.app li {
		border-top: 1px solid var(--k-border);
	}
	.app li > * {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-3);
		min-height: 46px;
		padding: 8px 0;
		color: var(--k-text-2);
		font-size: var(--k-text-md);
		font-weight: 600;
	}
	/* A kind of server is a fact, not a link: quieter than the rows that lead somewhere. */
	.app .kind {
		color: var(--k-text-muted);
		font-weight: 500;
	}
	.address {
		display: inline-flex;
		align-items: center;
		gap: var(--k-space-2);
		white-space: nowrap;
	}
	.eg {
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
	}
	.app a {
		transition: color var(--k-dur) var(--k-ease);
	}
	.app a :global(svg) {
		color: var(--k-text-dim);
		transition:
			color var(--k-dur) var(--k-ease),
			transform var(--k-dur) var(--k-ease);
	}
	.app a:hover {
		color: var(--k-text-bright);
	}
	.app a:hover :global(svg) {
		color: var(--k-accent);
		transform: translateX(3px);
	}
	.app code {
		padding: 2px 6px;
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-sm);
		background: var(--k-surface-4);
		color: var(--k-text-1);
		font-family: var(--k-font-mono);
		font-size: var(--k-text-xs);
		font-weight: 500;
	}

	/* ---- the five handbooks, as an index ---- */
	.books {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--k-space-5);
	}
	.books > ul {
		margin: 0;
		padding: 0;
		border-top: 1px solid var(--k-border-2);
		list-style: none;
	}
	.book {
		position: relative;
		display: grid;
		grid-template-columns: 40px minmax(0, 5fr) minmax(0, 4fr) auto;
		align-items: center;
		gap: var(--k-space-5);
		padding: 22px var(--k-space-3);
		border-bottom: 1px solid var(--k-border-2);
		transition: background var(--k-dur) var(--k-ease);
	}
	.book:hover {
		background: var(--k-hover-fill);
	}
	.mark {
		display: grid;
		place-items: center;
		width: 40px;
		height: 40px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-4);
		color: var(--k-text-muted);
		transition:
			background var(--k-dur) var(--k-ease),
			border-color var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.book:hover .mark {
		border-color: var(--k-accent-border);
		background: var(--k-accent-soft);
		color: var(--k-accent);
	}
	.what h3 {
		font-size: 21px;
		font-weight: 700;
		letter-spacing: -0.025em;
	}
	/* The title link covers the whole row; the chapter links sit above it. */
	.cover::after {
		content: '';
		position: absolute;
		inset: 0;
	}
	.what p {
		margin-top: 3px;
		color: var(--k-text-dim);
		font-size: var(--k-text-md);
		line-height: 1.5;
	}
	.starts {
		position: relative;
		z-index: 1;
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.starts a {
		color: var(--k-text-muted);
		font-size: var(--k-text-sm);
		font-weight: 600;
		line-height: 1.7;
	}
	.starts a:hover {
		color: var(--k-text-bright);
		text-decoration: underline;
		text-decoration-color: var(--doc-link);
		text-underline-offset: 3px;
	}
	.count {
		display: inline-flex;
		align-items: center;
		gap: var(--k-space-2);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		font-weight: 600;
		white-space: nowrap;
	}
	.count :global(svg) {
		transition:
			color var(--k-dur) var(--k-ease),
			transform var(--k-dur) var(--k-ease);
	}
	.book:hover .count :global(svg) {
		color: var(--k-accent);
		transform: translateX(3px);
	}

	/* ---- the request trace ---- */
	.trace {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--k-space-5);
	}
	.trace-head {
		display: flex;
		align-items: flex-end;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-4);
	}
	.trace-head p {
		margin-top: 6px;
		color: var(--k-text-muted);
		font-size: var(--k-text-lg);
	}
	.trace-flow {
		max-width: none;
		/* Both traces keep the same footprint, so switching does not move the page. */
		min-height: 220px;
	}
	.more {
		justify-self: start;
		display: inline-flex;
		align-items: center;
		gap: var(--k-space-2);
		color: var(--k-text-1);
		font-weight: 600;
		text-decoration: underline;
		text-decoration-color: var(--doc-link);
		text-decoration-thickness: 1.5px;
		text-underline-offset: 4px;
	}
	.more:hover {
		color: var(--doc-link);
	}

	/* ---- small print ---- */
	.foot {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-4) var(--k-space-8);
		padding-top: var(--k-space-6);
		border-top: 1px solid var(--k-border-1);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		line-height: 1.6;
	}
	.foot p {
		flex: 1 1 320px;
		max-width: 62ch;
	}
	.foot ul {
		display: flex;
		gap: var(--k-space-5);
		margin: 0;
		padding: 0;
		list-style: none;
		font-weight: 600;
	}
	.foot a:hover {
		color: var(--k-text-bright);
	}

	@media (max-width: 1080px) {
		.book {
			grid-template-columns: 40px minmax(0, 1fr) auto;
		}
		.starts {
			display: none;
		}
	}
	@media (max-width: 760px) {
		.home {
			gap: 64px;
		}
		.pair {
			grid-template-columns: minmax(0, 1fr);
		}
		.app,
		.app + .app {
			padding-inline: 0;
		}
		.app + .app {
			border-top: 1px solid var(--k-border-2);
			border-left: 0;
		}
		.hero {
			margin-bottom: -24px;
		}
		.book {
			gap: var(--k-space-4);
			padding-inline: 0;
		}
		.count .unit {
			display: none;
		}
		.find .wide {
			display: none;
		}
		.find .narrow {
			display: block;
		}
	}
	/* A key hint is no use where there is no keyboard. */
	@media (pointer: coarse) {
		.find .k-kbd {
			display: none;
		}
	}
</style>
