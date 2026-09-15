<script lang="ts">
	import Icon from '$lib/components/Icon.svelte';
	import { REPORT_TOPICS } from '$lib/data/content';
</script>

<svelte:head>
	<title>Support · komiq</title>
	<meta
		name="description"
		content="Report a problem with the catalogue, or read how the site works."
	/>
</svelte:head>

<!-- One centred column at the same width as /about, so the two pages read as the same
     site. The old layout set a max-width without `margin: 0 auto` and used a different
     one per section, which left every block hugging the left edge at a ragged width. -->
<div class="wrap k-gutter">
	<header class="head">
		<h1>Support</h1>
		<p class="lede">
			komiq is free to read, carries no ads, and takes no money from anyone. What keeps the
			catalogue honest is readers telling us when it's wrong.
		</p>
	</header>

	<section id="report">
		<h2>Report a problem</h2>
		<p>
			The catalogue is stitched together from a lot of sources, and some mistakes are only visible
			from where you're sitting. Pick whichever is closest — it goes to a queue a person reads.
		</p>

		<div class="rows links">
			{#each REPORT_TOPICS as t (t.kind)}
				<a class="row" href={`/support/report?kind=${t.kind}`}>
					<span class="row-title">{t.title}<Icon name="chevron-right" size={15} /></span>
					<span class="row-desc">{t.desc}</span>
				</a>
			{/each}
		</div>

		<!-- No response-time promise and no "we'll get back to you": there is no reply
		     channel at all — reports land in an admin queue and nothing emails the reader.
		     Saying so here is better than a reader waiting for an answer that was never
		     coming. -->
		<p class="note">
			You don't need an account, and having one changes nothing except that we know who sent it.
			Either way we can't write back, which is why nothing here asks for your email.
		</p>
	</section>

	<section>
		<h2>How komiq works</h2>
		<p>
			What komiq does with comics, what it keeps, and what it doesn't. If you're a rights holder,
			the same page explains how to reach us.
		</p>
		<a class="link k-hit" href="/about">Read about komiq <Icon name="arrow-right" size={15} /></a>
	</section>
</div>

<style>
	.wrap {
		max-width: 820px;
		margin: 0 auto;
		padding-top: 64px;
		padding-bottom: 100px;
	}
	.head h1 {
		font-size: 44px;
		line-height: 1.05;
		letter-spacing: -0.03em;
		margin: 0 0 16px;
		color: var(--k-text-bright);
	}
	.lede {
		margin: 0;
		font-size: 18px;
		line-height: 1.6;
		color: var(--k-text-2);
	}
	section {
		padding-top: 52px;
		/* So the #report anchor doesn't land under the fixed header. */
		scroll-margin-top: calc(var(--k-header-h) + 16px);
	}
	h2 {
		font-size: 24px;
		margin: 0 0 14px;
		color: var(--k-text-bright);
	}
	section p {
		margin: 0;
		font-size: 16px;
		line-height: 1.7;
		color: var(--k-text-2);
	}

	/* Hairline rows rather than a card grid: five report topics never divide evenly into
	   an auto-fit grid, so the old version always ended on a half-empty orphan row. A
	   stack has no such width to disagree with. */
	.rows {
		margin: 8px 0 0;
		border-top: 1px solid var(--k-border-1);
	}
	.row {
		display: block;
		padding: 16px 0;
		border-bottom: 1px solid var(--k-border-1);
	}
	.row-title {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 15.5px;
		color: var(--k-text-bright);
	}
	.row-desc {
		display: block;
		margin: 5px 0 0;
		font-size: 14.5px;
		line-height: 1.6;
		color: var(--k-text-dim);
		max-width: 62ch;
	}
	.links .row {
		text-decoration: none;
	}
	.links .row :global(svg) {
		flex: 0 0 auto;
		color: var(--k-text-ghost);
		transition: transform 0.15s ease;
	}
	.links .row:hover .row-title {
		color: var(--k-accent);
	}
	.links .row:hover :global(svg) {
		color: var(--k-accent);
		transform: translateX(3px);
	}

	.link {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		margin-top: 18px;
		font-weight: 700;
		font-size: 15px;
		color: var(--k-accent);
		text-decoration: none;
	}
	.link:hover {
		text-decoration: underline;
	}
	.note {
		margin-top: 14px;
		font-size: 13.5px;
		line-height: 1.65;
		color: var(--k-text-faint);
		max-width: 68ch;
	}
	@media (max-width: 640px) {
		.wrap {
			padding-top: 40px;
		}
		.head h1 {
			font-size: 32px;
		}
		.lede {
			font-size: 16px;
		}
		section {
			padding-top: 40px;
		}
		h2 {
			font-size: 21px;
		}
		section p {
			font-size: 15.5px;
		}
	}
</style>
