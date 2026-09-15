<script lang="ts">
	import Icon from '$lib/components/Icon.svelte';
	import Meta from '$lib/components/Meta.svelte';

	let { data } = $props();
	const first = $derived(data.book.chapters[0]);
</script>

<Meta title={data.book.title} description={data.book.description} />

<div class="book">
	<header class="head">
		<span class="mark"><Icon name={data.book.icon} size={22} /></span>
		<h1>{data.book.title}</h1>
		<p class="lede">{data.book.description}</p>
		{#if first}
			<a class="k-btn k-btn--primary k-btn--lg" href={first.href}>
				Start reading
				<Icon name="arrow-right" />
			</a>
		{/if}
	</header>

	<!-- The order is the reading order, so the list keeps its numbers. -->
	<ol class="chapters">
		{#each data.book.chapters as chapter (chapter.slug)}
			<li>
				<a href={chapter.href}>
					<span class="text">
						<span class="title">{chapter.title}</span>
						<span class="desc">{chapter.description}</span>
					</span>
					<Icon name="arrow-right" />
				</a>
			</li>
		{/each}
	</ol>
</div>

<style>
	.book {
		max-width: 980px;
	}
	.head {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		justify-items: start;
		gap: var(--k-space-3);
		margin-bottom: 48px;
	}
	.mark {
		display: grid;
		place-items: center;
		width: 44px;
		height: 44px;
		margin-bottom: var(--k-space-2);
		border: 1px solid var(--k-accent-border);
		border-radius: var(--k-radius-lg);
		background: var(--k-accent-soft);
		color: var(--k-accent);
	}
	h1 {
		font-size: clamp(34px, 5.5vw, 52px);
		font-weight: 700;
		letter-spacing: -0.035em;
		line-height: 1.05;
	}
	.lede {
		max-width: 58ch;
		margin-bottom: var(--k-space-3);
		color: var(--k-text-muted);
		font-size: 18px;
		line-height: 1.55;
	}

	.chapters {
		margin: 0;
		padding: 0;
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface);
		list-style: none;
		counter-reset: chapter;
		overflow: hidden;
	}
	.chapters li {
		counter-increment: chapter;
	}
	.chapters li + li {
		border-top: 1px solid var(--k-border);
	}
	.chapters a {
		display: grid;
		grid-template-columns: 2.2em minmax(0, 1fr) auto;
		align-items: center;
		gap: var(--k-space-3);
		padding: 18px var(--k-space-5);
		color: var(--k-text-dim);
		transition: background var(--k-dur) var(--k-ease);
	}
	.chapters a::before {
		content: counter(chapter);
		font-family: var(--k-font-display);
		font-size: var(--k-text-lg);
		font-variant-numeric: tabular-nums;
		font-weight: 700;
	}
	.chapters a:hover {
		background: var(--k-hover-fill);
	}
	.chapters a :global(svg) {
		transition:
			color var(--k-dur) var(--k-ease),
			transform var(--k-dur) var(--k-ease);
	}
	.chapters a:hover :global(svg) {
		color: var(--k-accent);
		transform: translateX(3px);
	}
	.text {
		display: grid;
		gap: 3px;
		min-width: 0;
	}
	.title {
		color: var(--k-text-bright);
		font-family: var(--k-font-display);
		font-size: 17px;
		font-weight: 700;
		letter-spacing: -0.015em;
	}
	.desc {
		font-size: var(--k-text-md);
		line-height: 1.55;
	}

	@media (max-width: 560px) {
		.chapters a {
			grid-template-columns: 1.6em minmax(0, 1fr);
			padding: var(--k-space-4);
		}
		.chapters a :global(svg) {
			display: none;
		}
	}
</style>
