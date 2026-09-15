<script lang="ts">
	import Icon from '$lib/components/Icon.svelte';
	import Meta from '$lib/components/Meta.svelte';

	let { data } = $props();

	// Which heading the reader is under, for the "On this page" rail.
	let activeId = $state('');
	let prose = $state<HTMLElement>();
	$effect(() => {
		// Re-run for each chapter: the headings are new elements.
		void data.chapter.html;
		const headings = [...(prose?.querySelectorAll<HTMLElement>('h2[id], h3[id]') ?? [])];
		activeId = '';
		if (!headings.length) return;
		const onScroll = () => {
			let current = '';
			for (const heading of headings) {
				if (heading.getBoundingClientRect().top > 120) break;
				current = heading.id;
			}
			activeId = current;
		};
		onScroll();
		addEventListener('scroll', onScroll, { passive: true });
		return () => removeEventListener('scroll', onScroll);
	});
</script>

<Meta title={data.chapter.title} description={data.chapter.description} />

<div class="page" class:has-toc={data.chapter.toc.length > 1}>
	<article>
		<header class="head">
			<nav class="crumbs" aria-label="Breadcrumb">
				<a href={data.book.href}>{data.book.title}</a>
				<Icon name="chevron-right" size={12} />
				<span class="k-num">Chapter {data.number} of {data.total}</span>
			</nav>
			<h1>{data.chapter.title}</h1>
			<p class="lede">{data.chapter.description}</p>
		</header>

		{#if data.chapter.toc.length > 1}
			<!-- Narrow screens have no room for the rail, so the same list folds in here. -->
			<details class="contents">
				<summary>On this page</summary>
				<ul>
					{#each data.chapter.toc as item (item.id)}
						<li><a href="#{item.id}" class:sub={item.depth === 3}>{item.text}</a></li>
					{/each}
				</ul>
			</details>
		{/if}

		<div class="prose" bind:this={prose}>
			<!-- Built from this repository's own Markdown at build time; raw HTML in a
			     page is escaped by the renderer (lib/server/content.ts). -->
			{@html data.chapter.html}
		</div>

		<nav class="pager" aria-label="Chapters">
			{#if data.prev}
				<a class="step" href={data.prev.href}>
					<span class="dir"><Icon name="arrow-left" size={14} /> Previous</span>
					<span class="name">{data.prev.title}</span>
				</a>
			{:else}
				<a class="step" href={data.book.href}>
					<span class="dir"><Icon name="arrow-left" size={14} /> Handbook</span>
					<span class="name">{data.book.title}</span>
				</a>
			{/if}
			{#if data.next}
				<a class="step next" href={data.next.href}>
					<span class="dir">Next <Icon name="arrow-right" size={14} /></span>
					<span class="name">{data.next.title}</span>
				</a>
			{/if}
		</nav>
	</article>

	{#if data.chapter.toc.length > 1}
		<aside class="toc" aria-label="On this page">
			<p class="toc-title">On this page</p>
			<ul>
				{#each data.chapter.toc as item (item.id)}
					<li>
						<a
							href="#{item.id}"
							class:sub={item.depth === 3}
							aria-current={activeId === item.id ? 'location' : undefined}>{item.text}</a
						>
					</li>
				{/each}
			</ul>
		</aside>
	{/if}
</div>

<style>
	.page {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 64px;
	}
	article {
		min-width: 0;
		max-width: 700px;
	}
	article .prose {
		max-width: none;
	}

	.head {
		margin-bottom: 40px;
		padding-bottom: 32px;
		border-bottom: 1px solid var(--k-border-1);
	}
	.crumbs {
		display: flex;
		align-items: center;
		gap: 6px;
		margin-bottom: var(--k-space-3);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.crumbs a:hover {
		color: var(--k-text-bright);
	}
	h1 {
		font-size: clamp(30px, 4.4vw, 40px);
		font-weight: 700;
		letter-spacing: -0.035em;
		line-height: 1.1;
	}
	.lede {
		margin-top: var(--k-space-3);
		max-width: 62ch;
		color: var(--k-text-muted);
		font-size: 18px;
		line-height: 1.55;
	}

	/* ---- previous / next ---- */
	.pager {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: var(--k-space-3);
		margin-top: 72px;
	}
	.step {
		display: grid;
		gap: 4px;
		padding: 14px var(--k-space-4);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-lg);
		transition:
			background var(--k-dur) var(--k-ease),
			border-color var(--k-dur) var(--k-ease);
	}
	.step:hover {
		background: var(--k-hover-fill);
		border-color: var(--k-border-strong);
	}
	.step.next {
		grid-column: 2;
		justify-items: end;
		text-align: right;
	}
	.dir {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		font-weight: 700;
	}
	.name {
		color: var(--k-text-bright);
		font-family: var(--k-font-display);
		font-size: var(--k-text-lg);
		font-weight: 700;
		letter-spacing: -0.015em;
	}
	.step:hover .dir :global(svg) {
		color: var(--k-accent);
	}

	/* ---- on this page ---- */
	.contents {
		margin: -16px 0 32px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.contents summary {
		padding: 10px var(--k-space-4);
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		font-weight: 700;
		cursor: pointer;
	}
	.contents ul {
		margin: 0;
		padding: 0 var(--k-space-4) var(--k-space-3);
		list-style: none;
	}
	.contents a {
		display: block;
		padding: 6px 0;
		color: var(--k-text-muted);
		font-size: var(--k-text-md);
		line-height: 1.4;
	}
	.contents a.sub {
		padding-left: var(--k-space-4);
	}
	.contents a:hover {
		color: var(--k-text-bright);
	}
	.toc {
		display: none;
	}
	@media (min-width: 1240px) {
		.page.has-toc .contents {
			display: none;
		}
		.page.has-toc {
			grid-template-columns: minmax(0, 700px) 200px;
			justify-content: space-between;
		}
		.toc {
			display: block;
			position: sticky;
			top: 56px;
			align-self: start;
			max-height: calc(100dvh - 96px);
			overflow-y: auto;
		}
	}
	.toc-title {
		margin-bottom: var(--k-space-2);
		color: var(--k-text-dim);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.07em;
		text-transform: uppercase;
	}
	.toc ul {
		margin: 0;
		padding: 0;
		list-style: none;
		border-left: 1px solid var(--k-border-2);
	}
	.toc a {
		display: block;
		margin-left: -1px;
		padding: 5px 0 5px var(--k-space-3);
		border-left: 1px solid transparent;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		line-height: 1.4;
		transition: color var(--k-dur) var(--k-ease);
	}
	.toc a.sub {
		padding-left: var(--k-space-6);
	}
	.toc a:hover {
		color: var(--k-text-bright);
	}
	.toc a[aria-current='location'] {
		border-left-color: var(--k-accent);
		color: var(--k-text-bright);
		font-weight: 600;
	}

	@media (max-width: 560px) {
		.pager {
			grid-template-columns: minmax(0, 1fr);
		}
		.step.next {
			grid-column: 1;
		}
	}
</style>
