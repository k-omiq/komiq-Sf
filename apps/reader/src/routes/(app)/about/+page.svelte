<script lang="ts">
	import Icon from '$lib/components/Icon.svelte';
	import { config } from '$lib/config';
</script>

<svelte:head>
	<title>About · komiq</title>
	<meta
		name="description"
		content="How komiq works: a catalogue, not a library. komiq indexes where comics are; chapter pages are fetched from their source when you open them, and there is no archive here to download."
	/>
</svelte:head>

<div class="wrap k-gutter">
	<header class="head">
		<h1>About komiq</h1>
		<p class="lede">
			komiq is a catalogue and a reader — not a library. We keep track of series; we don't keep the
			comics.
		</p>
	</header>

	<section class="sec">
		<h2>How it works</h2>
		<p>
			Comics live on the sites and scanlation groups that publish them. komiq indexes what exists
			and where: titles, authors, genres, which chapters are out, and which sources carry them. That
			index is the whole product.
		</p>
		<!-- Do not restore the old ending here ("close the tab and there is nothing left
		     behind"). The fetch engine keeps a working cache on a persistent volume, so that
		     sentence was false, and it contradicted the cache this page now admits to two
		     sections down. What is true is the part that matters: the thing we keep is the
		     index entry, not the chapter. -->
		<p>
			When you open a chapter, nothing is pulled out of an archive of ours, because there isn't one.
			Your reader asks for a page, our workers fetch that page from its original source, and it goes
			straight to your screen. What komiq keeps afterwards is the index entry — where that chapter
			is — not the chapter.
		</p>
		<p>
			This is also why a source going down takes its chapters with it, and why new chapters appear
			here on a delay — we re-check each series every few hours rather than the moment something is
			published. We are pointing at the internet, not copying it.
		</p>
	</section>

	<section class="sec">
		<h2>What we store, and what we don't</h2>
		<div class="cols">
			<div class="col">
				<!-- "Never stored on our servers" was the old heading and it was not true: the
				     fetch engine's page cache lives on a disk we own. The honest line is the one
				     that actually distinguishes komiq from a piracy host — there is no archive
				     and nothing to download — so that is what this column claims. -->
				<div class="col-head no">
					<Icon name="x" size={17} />
					<span>No archive, and nothing to download</span>
				</div>
				<ul>
					<li>
						<strong>An archive of chapters.</strong> There isn't one. komiq has no mirror of any source,
						no bulk download, no export, and nowhere to upload a file — a page is fetched from its source
						when you ask for it and passed straight to you.
					</li>
					<li>
						<strong>Anything a page cache would let you keep.</strong> The fetch engine holds recently-read
						pages for a while, the way a browser does, so re-reading doesn't hammer the source. It is
						scratch space: nothing indexes it, nothing lists it, and there is no way to ask komiq for
						its contents.
					</li>
				</ul>
			</div>
			<div class="col">
				<div class="col-head yes">
					<Icon name="check" size={17} />
					<span>Kept, because the site can't work otherwise</span>
				</div>
				<ul>
					<li>
						<strong>Series information.</strong> Titles, authors, genres, chapter numbers, and which source
						has what. This is the catalogue.
					</li>
					<li>
						<strong>Cover thumbnails.</strong> Small, downscaled cover images, cached so a grid of a hundred
						covers doesn't hammer a hundred upstream sites. Covers only — never pages.
					</li>
					<li>
						<strong>Your account, if you make one.</strong> Your library, reading progress, ratings and
						comments. No account is needed to read.
					</li>
					<li>
						<strong>Anonymous view counts.</strong> Per series, so "trending" means something. There are
						no third-party trackers and no advertising on komiq.
					</li>
				</ul>
			</div>
		</div>
	</section>

	<section class="sec">
		<h2>Rights and takedowns</h2>
		<p>
			Every series, chapter, cover and page belongs to its creators and publishers. komiq claims no
			ownership of any of it, sells none of it, and puts no advertising against it.
		</p>
		<p>
			If you are a creator, publisher, or represent one and you want a series removed from the
			index, tell us and we will remove it. We would rather act on the request than argue with it.
			We can only change what we control — the catalogue entry and the links — because we host no
			copies; the files themselves live with whoever published them.
		</p>
		<a class="btn" href="/support/report?kind=OTHER">
			<Icon name="flag" size={16} /> Send us a request
		</a>
	</section>

	<!-- The whole section is gated on `donateUrl`: with it unset there is no Donate block on
	     /support either, so "See where donations go" would scroll to an anchor that isn't
	     rendered — dumping the reader at the top of a page that never mentions donations —
	     and "it runs on donations" would be a claim about someone else's server. -->
	{#if config.donateUrl}
		<section class="sec">
			<h2>Who pays for it</h2>
			<p>
				Fetching and serving images costs money even when nothing is stored — that is bandwidth and
				worker time, and it grows with every reader. komiq is free and ad-free, so it runs on
				donations.
			</p>
			<div class="btns">
				<a class="btn" href="/support#donate">See where donations go</a>
				<a class="btn ghost" href={config.donateUrl} target="_blank" rel="noopener noreferrer">
					<Icon name="heart" size={16} /> Ko-fi
				</a>
			</div>
		</section>
	{/if}
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
	.sec {
		padding-top: 52px;
	}
	.sec h2 {
		font-size: 24px;
		margin: 0 0 16px;
		color: var(--k-text-bright);
	}
	.sec p {
		margin: 0 0 14px;
		font-size: 16px;
		line-height: 1.7;
		color: var(--k-text-2);
	}
	.sec p:last-of-type {
		margin-bottom: 0;
	}
	.cols {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
		gap: 18px;
		/* The two lists are deliberately lopsided (there is far more to say about what we
		   keep). Without this they stretch to equal height and the short one is mostly
		   empty box. */
		align-items: start;
	}
	.col {
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface);
		padding: 22px 24px;
	}
	.col-head {
		display: flex;
		align-items: center;
		gap: 9px;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 14px;
		margin-bottom: 14px;
	}
	.col-head.no {
		color: var(--k-cancelled);
	}
	.col-head.yes {
		color: var(--k-ongoing);
	}
	.col ul {
		margin: 0;
		padding-left: 18px;
		display: flex;
		flex-direction: column;
		gap: 11px;
	}
	.col li {
		font-size: 14.5px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	.col li strong {
		color: var(--k-text-1);
		font-weight: 700;
	}
	.btns {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
		margin-top: 22px;
	}
	.btn {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		margin-top: 22px;
		height: 46px;
		padding: 0 22px;
		border-radius: var(--k-radius);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 14.5px;
		text-decoration: none;
	}
	.btn:hover {
		background: var(--k-primary-hover);
	}
	.btns .btn {
		margin-top: 0;
	}
	.btn.ghost {
		background: transparent;
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
	}
	.btn.ghost:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text-bright);
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
		.sec {
			padding-top: 40px;
		}
		.sec h2 {
			font-size: 21px;
		}
		.sec p {
			font-size: 15.5px;
		}
	}
</style>
