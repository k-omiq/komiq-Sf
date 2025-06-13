<script lang="ts">
	import MangaCard from '$lib/components/MangaCard.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import CardRowSkeleton from '$lib/components/CardRowSkeleton.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import { slug, cardSub, cardTimeTooltip as timeTooltip, FLAG } from '$lib/data/types';
	import type { FeaturedView } from '$lib/data/source';

	let { data } = $props();

	/** Genre chips shown on a hero slide — the rest would wrap past the synopsis. */
	const HERO_GENRES = 4;

	// The home feeds are a resolved object on the server/hydration (edge SSR renders
	// real cards) and a pending Promise on client navigations (skeletons via {#await}).
	// See +page.ts. `HomeData` is the resolved shape either way.
	type HomeData = Awaited<typeof data.home>;

	let heroIndex = $state(0);
	// Once the reader picks a slide, stop auto-rotating so their choice sticks.
	let heroPicked = $state(false);
	// …and hold the rotation while they are actually AT the hero (pointer over it, or
	// keyboard focus inside it). Without this the banner swaps the slide out from under
	// whatever they are doing: mid-synopsis it is merely rude, but with focus on the
	// title link it is a correctness bug — the link the reader tabbed to keeps its DOM
	// node and its focus ring while its href is rewritten to a different series, so
	// pressing Enter opens something they never chose. (It is also the WCAG 2.2.2
	// "pause, stop, hide" mechanism for a 5s auto-advancing region.)
	let heroHeld = $state(false);
	/** Auto-rotation is stopped — either pinned by a click or held by the reader. */
	const heroPaused = $derived(heroPicked || heroHeld);

	/** Step the carousel by ±1, wrapping, and pin it to the reader's choice. */
	function stepHero(delta: number): void {
		const n = featured.length;
		if (!n) return;
		// Step from the CLAMPED index, never the raw one. The two differ exactly when the
		// feed shrank under a stationary `heroIndex` (see `slide`), and stepping from the
		// raw value then moves relative to a slide nobody is looking at: `heroIndex` 7 in a
		// 3-slide feed shows slide 2, and "next" would compute (7 + 1) % 3 = 2 — the same
		// slide. The click would do nothing visible while still setting `heroPicked`, i.e.
		// permanently stopping the rotation, so the hero would look frozen for good.
		heroIndex = (slide + delta + n) % n;
		heroPicked = true;
	}

	function seriesHref(f: FeaturedView): string {
		return `/series/${f.id ?? slug(f.title)}`;
	}

	// The hero slides. This is a $derived, NOT an $effect: effects don't run during
	// SSR, so populating `featured` from one left the edge-rendered HTML with no
	// cover, no title and no CTA — the single most prominent thing on the page was
	// blank in the server response (and in the s-maxage cache behind it), even
	// though the data was right there in `data.home`.
	//
	// `data.home` is a resolved object on the server and on hydration, and a pending
	// Promise on client-side navigations (see +page.ts); the promise arm still needs
	// an effect, but only that arm.
	const settled = $derived(data.home instanceof Promise ? undefined : (data.home as HomeData));
	let streamed = $state<HomeData | undefined>(undefined);
	$effect(() => {
		const h = data.home;
		streamed = undefined;
		if (!(h instanceof Promise)) return;
		let cancelled = false;
		h.then((r) => {
			if (!cancelled) streamed = r;
		});
		return () => {
			cancelled = true;
		};
	});
	// `data.home` never rejects (empty feeds on error), so an empty `featured` is the
	// backend-down / nothing-live case rather than a loading state — the hero is dropped
	// entirely there, it does not fall back to the skeleton.
	const featured = $derived<FeaturedView[]>((settled ?? streamed)?.featured ?? []);

	// Auto-rotate the hero, unless the reader stopped it or prefers reduced motion.
	// Re-runs (and so re-arms a fresh interval) whenever `featured` or the paused flag
	// changes; the teardown clears the old one, so there is exactly one timer at a time
	// and none at all after unmount.
	$effect(() => {
		if (heroPaused) return;
		if (featured.length < 2) return;
		const reduce =
			typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches;
		if (reduce) return;
		const t = setInterval(() => {
			// From `slide`, not `heroIndex` — same reason as `stepHero`. Advancing the raw
			// index after the feed shrank would re-render the same slide and read as a
			// carousel that skipped a beat.
			heroIndex = (slide + 1) % featured.length;
		}, 5000);
		return () => clearInterval(t);
	});

	/**
	 * The active slide: `heroIndex` clamped into range, never indexed raw.
	 *
	 * `featured` is not a fixed length and this component does not own it — it is whatever
	 * the trending feed returned for the render in question, and it can change under a
	 * stationary `heroIndex`. A raw `featured[heroIndex]` past the end is `undefined`,
	 * which silently blanks the single most prominent element on the site (the `{#if
	 * current}` arm renders nothing at all) with no error anywhere to notice it by.
	 *
	 * Measured, so the next reader doesn't have to: today no in-app path re-lengths the
	 * feed under a LIVE instance. Navigating away and back destroys this component, so
	 * `heroIndex` returns to 0; clicking Home while already on `/` re-runs nothing at all,
	 * because `+page.ts`'s load reads neither `url` nor `params` and so has no dependency
	 * to invalidate. What makes the clamp worth its one `Math.min` anyway is that ALL of
	 * that is a property of the route's load, not of the hero: an `invalidateAll()` from a
	 * sign-in, a retry button like the one on /updates, or a `depends()` added later hands
	 * this component a shorter list while it is mounted, and the failure mode is silent.
	 *
	 * Clamping (rather than resetting to 0) is the right degradation for the reader who
	 * pinned a slide: they keep looking at the end of the feed they were reading, instead
	 * of being yanked back to the start. Its one hazard is that two `heroIndex` values then
	 * map to one slide, which is why both writers below step from `slide`, not `heroIndex`.
	 */
	const slide = $derived(Math.min(heroIndex, Math.max(0, featured.length - 1)));
	const current = $derived(featured[slide]);

	/**
	 * The slide's chapter line — "Ch. 151 · 12 chapters".
	 *
	 * TWO INDEPENDENT FACTS, both printed: `latestCh` is what the newest chapter is
	 * CALLED, `ch` is how many we hold. A partially-mirrored series makes them far
	 * apart, so the hero says both rather than picking one and being wrong about the
	 * other — and neither may ever stand in for the other (a count under a "Ch."
	 * label is F4, the bug the chapter-number contract exists to prevent).
	 *
	 * Both halves are independently absent, which is why this builds a list rather
	 * than nesting ternaries. `latestCh` is `''` for the catalogue rows with no dated
	 * chapter and for any response from a server predating the field; `ch === 0` is
	 * real and common (MangaDex strips chapters on a licensing takedown, and ~11k
	 * works count zero because their chapters carry a NULL number). With both gone
	 * the line is empty and the foot renders the arrows alone — "0 chapters" under
	 * the site's most prominent artwork is worse than no line at all.
	 */
	const chapterMeta = $derived.by(() => {
		const f = current;
		if (!f) return '';
		const count = f.ch > 0 ? `${f.ch} ${f.ch === 1 ? 'chapter' : 'chapters'}` : '';
		return [f.latestCh, count].filter(Boolean).join(' · ');
	});
</script>

<!-- Resolved object on the server (SSR renders cards) vs pending Promise on the
     client (skeletons via {#await}). Both feed the same `content` snippet. -->
{#if data.home instanceof Promise}
	{#await data.home}
		{@render loading()}
	{:then home}
		{@render content(home)}
	{/await}
{:else}
	{@render content(data.home)}
{/if}

{#snippet loading()}
	<!-- LOADING -->
	<!-- Reuses `.hero-slide` / `.hero-cover` / `.hero-text` / `.hero-foot` verbatim, so its
	     geometry is the real hero's by construction and the swap doesn't shift layout. -->
	<section class="hero">
		<div class="hero-slide">
			<div class="hero-cover k-skeleton"></div>
			<div class="hero-text">
				<div class="k-skeleton title-sk"></div>
				<div class="k-skeleton line-sk"></div>
				<div class="k-skeleton line-sk short"></div>
			</div>
			<div class="hero-foot">
				<div class="k-skeleton foot-sk"></div>
			</div>
		</div>
	</section>
	<div class="sections k-gutter">
		<section class="block">
			<div class="k-skeleton head-sk"></div>
			<CardRowSkeleton />
		</section>
		<section class="block">
			<div class="k-skeleton head-sk"></div>
			<CardRowSkeleton />
		</section>
		<section class="block">
			<div class="k-skeleton head-sk"></div>
			<CardRowSkeleton />
		</section>
	</div>
{/snippet}

{#snippet content(home: HomeData)}
	<!-- HERO. Rendered only when there IS a slide: an empty `featured` (backend down, or
	     every discovery feed empty) would otherwise leave a 120px band containing the word
	     "trending" and nothing else — a header for content that does not exist. The rows
	     below have the same all-or-nothing treatment. -->
	{#if current}
		<section
			class="hero"
			aria-roledescription="carousel"
			aria-label="Featured series"
			onmouseenter={() => (heroHeld = true)}
			onmouseleave={() => (heroHeld = false)}
			onfocusin={() => (heroHeld = true)}
			onfocusout={() => (heroHeld = false)}
		>
			<!-- `aria-live` is off while the carousel is advancing on its own (announcing a
			     slide every 5s would make the page unusable) and polite once it is paused —
			     which includes the moment an arrow is clicked, so a screen-reader user hears
			     the slide they just asked for. This is the APG carousel pattern. -->
			<div
				class="hero-slide"
				role="group"
				aria-roledescription="slide"
				aria-label={`Slide ${slide + 1} of ${featured.length}`}
				aria-live={heroPaused ? 'polite' : 'off'}
			>
				<a
					class="hero-cover"
					href={seriesHref(current)}
					aria-label={`Open ${current.title}`}
					tabindex="-1"
				>
					<Cover src={current.cover} alt={current.title} />
					<span class="hero-flag" title={current.type}>{FLAG[current.type]}</span>
				</a>
				<div class="hero-text">
					<h1><a href={seriesHref(current)}>{current.title}</a></h1>
					{#if current.genres.length}
						<div class="hero-tags">
							{#each current.genres.slice(0, HERO_GENRES) as g (g)}
								<a class="hero-tag" href={`/browse?genre=${encodeURIComponent(g)}`}>{g}</a>
							{/each}
						</div>
					{/if}
					{#if current.description}
						<p class="hero-desc">{current.description}</p>
					{/if}
				</div>
				<!-- The foot is a GRID CHILD, not a child of `.hero-text`: on a phone it moves
				     out from beside the cover and spans the full width, which is the only way
				     the credit, the chapter line and both arrows fit on one row at 390px. -->
				<div class="hero-foot">
					{#if current.author}
						<span class="hero-author">{current.author}</span>
					{/if}
					<div class="hero-nav">
						{#if chapterMeta}
							<span class="hero-chapters">{chapterMeta}</span>
						{/if}
						<!-- Disabled off `featured`, the same list `stepHero` steps through and
						     `slide` indexes. `home.featured` is the same array by every path that
						     can render this snippet, but it is a SECOND source of truth for a
						     control whose whole job is to move within the first one. -->
						<button
							class="hero-arrow"
							onclick={() => stepHero(-1)}
							disabled={featured.length < 2}
							aria-label="Previous featured series"
						>
							<Icon name="chevron-left" size={26} />
						</button>
						<button
							class="hero-arrow"
							onclick={() => stepHero(1)}
							disabled={featured.length < 2}
							aria-label="Next featured series"
						>
							<Icon name="chevron-right" size={26} />
						</button>
					</div>
				</div>
			</div>
		</section>
	{/if}

	<div class="sections k-gutter">
		<!-- LATEST UPDATES -->
		{#if home.latestUpdates.length}
			<section class="block">
				<div class="row-head">
					<h2>Latest Updates</h2>
					<a class="view-all" href="/updates">View all</a>
				</div>
				<div class="row">
					{#each home.latestUpdates as item (item.id ?? item.title + item.ch)}
						<MangaCard
							title={item.title}
							sub={cardSub(item)}
							subTitle={timeTooltip(item)}
							rating={item.rating}
							cover={item.cover}
							id={item.id}
							fixed
						/>
					{/each}
				</div>
			</section>
		{/if}

		<!-- TRENDING -->
		{#if home.trending.length}
			<section class="block">
				<h2>Trending</h2>
				<div class="row">
					{#each home.trending as item (item.id ?? item.title + item.ch)}
						<MangaCard
							title={item.title}
							sub={cardSub(item)}
							subTitle={timeTooltip(item)}
							rating={item.rating}
							cover={item.cover}
							id={item.id}
							fixed
						/>
					{/each}
				</div>
			</section>
		{/if}

		<!-- LATEST ADDED -->
		{#if home.latestAdded.length}
			<section class="block">
				<h2>Latest Added</h2>
				<div class="row">
					{#each home.latestAdded as item (item.id ?? item.title + item.ch)}
						<MangaCard
							title={item.title}
							sub={cardSub(item, 'Added ')}
							rating={item.rating}
							cover={item.cover}
							id={item.id}
							fixed
						/>
					{/each}
				</div>
			</section>
		{/if}

		<!-- GENRES -->
		{#if home.homeGenres.length}
			<section class="block">
				<h2>Genres</h2>
				<div class="genres">
					{#each home.homeGenres as g (g)}
						<a class="genre" href={`/browse?genre=${encodeURIComponent(g)}`}>{g}</a>
					{/each}
				</div>
			</section>
		{/if}
	</div>
{/snippet}

<style>
	/* SOLID, not the slide's cover blurred into a colour field. The wash was the only
	   thing tinting the banner, but it was also what every piece of hero text had its
	   contrast computed against — and the art behind it is arbitrary, so the dimmest
	   line on the slide sat a hair over the 4.5:1 floor in the worst case and needed a
	   0.88 scrim propping it up. On a flat theme background the text is back on the
	   normal ramp, at the ramp's designed contrast, in both themes. The cover still
	   supplies the banner's colour — it is just the real cover now, bled to the edge,
	   rather than a 48px-blurred copy of it behind glass. */
	.hero {
		position: relative;
		/* The cover bleeds to the left/top/bottom edges; clip it to the banner. */
		overflow: hidden;
		background: var(--k-bg);
		/* The page below is the same token, so the band needs an edge of its own. */
		border-bottom: 1px solid var(--k-border-1);
	}
	.head-sk {
		width: 180px;
		height: 22px;
		border-radius: 6px;
	}
	/*
	 * cover │ text
	 * cover │ foot     — and on a phone: cover │ text
	 *                                    foot  ╵ foot
	 *
	 * A grid, not the nested flex rows this used to be, precisely so that second layout
	 * is a `grid-template-areas` swap rather than a different DOM. The foot has to leave
	 * the text column on a phone (a ~190px column cannot hold a credit, a chapter line
	 * and two arrows), and it has to stay in it on a desktop (pinned to the bottom of
	 * the cover, per the design).
	 *
	 * The first row is `1fr` and the second `auto`, so `min-height` slack lands on the
	 * TEXT and the foot stays a fixed strip at the bottom of the banner. Without it the
	 * banner is only ever as tall as its content, and a short synopsis collapses the
	 * cover — the one element whose whole job here is to be tall.
	 */
	.hero-slide {
		display: grid;
		grid-template-columns: var(--hero-cover-w) minmax(0, 1fr);
		grid-template-areas:
			'cover text'
			'cover foot';
		grid-template-rows: 1fr auto;
		min-height: var(--hero-h);
		/* 2:3 against `--hero-h`, because the cover spans BOTH rows here — see the
		   breakpoints, where the phone layout drops it to row 1 and re-derives this. */
		--hero-cover-w: 320px;
		--hero-h: 480px;
		/* Gap between the cover and the text column — the text's own left padding,
		   kept as a variable because the foot has to match it exactly. */
		--hero-pad-l: 40px;
	}
	/* Full-bleed: flush to the banner's left/top/bottom edges, no radius, no shadow.
	   `object-fit: cover` (Cover's default) crops the art to the column; the column is
	   sized ~2:3 at every breakpoint so that crop stays small. */
	.hero-cover {
		grid-area: cover;
		position: relative;
		overflow: hidden;
		background: var(--k-cover);
		border-right: 1px solid var(--k-border-1);
	}
	.hero-flag {
		position: absolute;
		left: 14px;
		bottom: 14px;
		width: 26px;
		height: 26px;
		border-radius: var(--k-radius-sm);
		background: var(--k-glass-strong);
		display: flex;
		align-items: center;
		justify-content: center;
		font-size: 14px;
		line-height: 1;
	}
	.hero-text {
		grid-area: text;
		min-width: 0;
		display: flex;
		flex-direction: column;
		justify-content: center;
		gap: 16px;
		padding: 40px var(--k-gutter) 0 var(--hero-pad-l);
	}
	.hero-text h1 {
		font-size: 46px;
		line-height: 1.04;
		letter-spacing: -0.03em;
		margin: 0;
	}
	.hero-text h1 a {
		color: var(--k-text-bright);
		text-decoration: none;
	}
	.hero-text h1 a:hover {
		color: var(--k-accent);
	}
	.hero-tags {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.hero-tag {
		padding: 5px 10px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-sm);
		background: var(--k-hover-fill);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-muted);
		text-decoration: none;
	}
	.hero-tag:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.hero-desc {
		margin: 0;
		max-width: 78ch;
		font-size: 15.5px;
		line-height: 1.62;
		color: var(--k-text-2);
		/* Synopses run from one line to twenty; clamping keeps the banner's height
		   stable as the carousel rotates. */
		display: -webkit-box;
		-webkit-box-orient: vertical;
		-webkit-line-clamp: 5;
		line-clamp: 5;
		overflow: hidden;
	}
	.hero-foot {
		grid-area: foot;
		display: flex;
		align-items: center;
		gap: 20px;
		min-width: 0;
		padding: 16px var(--k-gutter) 36px var(--hero-pad-l);
	}
	.hero-author {
		font-size: 19px;
		font-style: italic;
		color: var(--k-text-muted);
		/* A credit is one unbreakable token as far as flex is concerned (its min-content
		   width is its longest word), so a long name pushed the arrows off a 320px screen.
		   Let it shrink and ellipsize instead — the arrows are the interactive half. */
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.hero-nav {
		display: flex;
		align-items: center;
		gap: 10px;
		flex: 0 0 auto;
		/* Stay hard right even when the credit is absent (not every source gives one) —
		   `justify-content: space-between` alone would slide the controls left there. */
		margin-left: auto;
	}
	.hero-chapters {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 14px;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-text-muted);
		margin-right: 6px;
		white-space: nowrap;
	}
	.foot-sk {
		width: 200px;
		height: 18px;
		border-radius: 6px;
	}
	.hero-arrow {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 34px;
		height: 34px;
		padding: 0;
		border: none;
		border-radius: var(--k-radius-sm);
		background: none;
		color: var(--k-text-dim);
		cursor: pointer;
	}
	.hero-arrow:hover:not(:disabled) {
		color: var(--k-text-bright);
		background: var(--k-hover-fill);
	}
	.hero-arrow:disabled {
		color: var(--k-text-disabled);
		cursor: default;
	}
	.title-sk {
		width: min(460px, 70%);
		height: 44px;
		border-radius: 8px;
	}
	.line-sk {
		width: min(620px, 90%);
		height: 14px;
		border-radius: 6px;
	}
	.line-sk.short {
		width: min(420px, 60%);
	}
	.sections {
		display: flex;
		flex-direction: column;
		gap: 60px;
		padding-top: 64px;
		padding-bottom: 80px;
	}
	.block {
		display: flex;
		flex-direction: column;
		gap: 20px;
	}
	.block h2 {
		font-size: 21px;
		margin: 0;
		color: var(--k-text);
	}
	.row-head {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.view-all {
		font-size: 13px;
		color: var(--k-text-dimmer);
		text-decoration: none;
	}
	.view-all:hover {
		color: var(--k-text);
	}
	.row {
		display: flex;
		gap: 22px;
		overflow-x: auto;
		padding-bottom: 4px;
	}
	.genres {
		display: flex;
		gap: 28px;
		flex-wrap: wrap;
	}
	.genre {
		font-size: 15px;
		color: var(--k-text-muted);
		text-decoration: none;
		padding-bottom: 2px;
		border-bottom: 1px solid transparent;
	}
	.genre:hover {
		color: var(--k-text);
		border-bottom-color: rgba(255, 255, 255, 0.3);
	}
	/* The cover column and the banner height shrink TOGETHER at every breakpoint, so the
	   column stays near 2:3 and `object-fit: cover` has almost nothing to crop. While the
	   cover spans both grid rows its height IS `--hero-h`, so each pair below satisfies
	   `--hero-cover-w ≈ --hero-h × 2/3`. (The phone block re-derives it — there the cover
	   spans row 1 only, so the foot's height comes off first.) */
	@media (max-width: 1100px) {
		.hero-slide {
			--hero-cover-w: 280px;
			--hero-h: 420px;
			--hero-pad-l: 32px;
		}
	}
	@media (max-width: 900px) {
		.hero-slide {
			--hero-cover-w: 240px;
			--hero-h: 360px;
			--hero-pad-l: 26px;
		}
		.hero-text {
			padding-top: 30px;
		}
		.hero-foot {
			padding-bottom: 28px;
		}
		.hero-text h1 {
			font-size: 32px;
		}
		.hero-desc {
			-webkit-line-clamp: 4;
			line-clamp: 4;
			font-size: 14.5px;
		}
	}
	@media (max-width: 640px) {
		/* PHONE. The foot leaves the text column and spans the banner's full width —
		   a ~190px column beside the cover cannot hold a credit, a chapter line and two
		   34px arrows without the arrows falling off the screen. The cover keeps its
		   full-bleed left edge and now ends where the foot begins.

		   `--hero-cover-w` is viewport-relative because the banner's height is fixed
		   while the screen's width is not: at a flat 150px the column would be 2:3 on
		   a 390px phone and near-square on a 600px tablet. The clamp holds it between
		   a 130px floor (below which the cover reads as a stripe) and 175px. */
		.hero-slide {
			grid-template-areas:
				'cover text'
				'foot foot';
			--hero-cover-w: clamp(130px, 44vw, 175px);
			--hero-h: 320px;
			--hero-pad-l: 16px;
		}
		.hero-text {
			gap: 10px;
			padding-top: 22px;
		}
		.hero-foot {
			/* Full width now, so it takes the page gutter on BOTH sides rather than the
			   cover-column offset on the left. */
			padding: 12px var(--k-gutter) 18px;
			gap: 12px;
		}
		/* Clamped, unlike the desktop title: the column beside the cover is ~180px, so an
		   unclamped 24px title of the length this catalogue actually carries ("I Became
		   the Villainess of an Otome Game…") runs seven lines and pushes the synopsis out
		   of the banner entirely. */
		.hero-text h1 {
			font-size: 24px;
			display: -webkit-box;
			-webkit-box-orient: vertical;
			-webkit-line-clamp: 3;
			line-clamp: 3;
			overflow: hidden;
		}
		/* Dropped, not shrunk. Genre chips are ~90px each and wrap one per line in that
		   column, so four of them cost three rows — the same space the synopsis needs and
		   a worse use of it. They are one tap away on /browse. */
		.hero-tags {
			display: none;
		}
		.hero-desc {
			font-size: 14px;
			-webkit-line-clamp: 3;
			line-clamp: 3;
		}
		.hero-flag {
			left: 10px;
			bottom: 10px;
		}
		.hero-author {
			font-size: 15px;
		}
		.hero-chapters {
			font-size: 11.5px;
			letter-spacing: 0.02em;
			margin-right: 2px;
		}
		.hero-arrow {
			width: 30px;
			height: 30px;
		}
		.sections {
			gap: 40px;
			padding-top: 40px;
			padding-bottom: 56px;
		}
		.block h2 {
			font-size: 19px;
		}
		/* Snappy, touch-friendly horizontal carousels. */
		.row {
			gap: 16px;
			scroll-snap-type: x proximity;
			-webkit-overflow-scrolling: touch;
		}
		.genres {
			gap: 18px 22px;
		}
	}
</style>
