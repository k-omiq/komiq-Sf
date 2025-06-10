<script lang="ts">
	import { FEED_PAGE_SIZE, lastPage } from '$lib/data/paging';

	/**
	 * Page-number pagination over a server-paginated feed.
	 *
	 * Built from real `<a href>` links, not buttons: the page lives in the URL, so
	 * a link is the honest control — it is shareable, middle/ctrl-clickable and
	 * crawlable, and SvelteKit still intercepts the click for a client-side
	 * navigation, so it behaves like the admin console's buttons. (The admin pager
	 * uses buttons precisely because it has NO URL state.)
	 *
	 * The consumer owns the URL shape via `href`, so this component works for any
	 * route that keeps other params in the URL alongside `page`.
	 */
	let {
		page,
		hasNext,
		total = null,
		pageSize = FEED_PAGE_SIZE,
		count,
		loading = false,
		href,
		label = 'Pagination',
		controls = undefined,
	}: {
		/** 1-based current page. */
		page: number;
		/** From the server envelope — never inferred from a short page. */
		hasNext: boolean;
		/** Total matching rows, or null when the backend can't tell us. */
		total?: number | null;
		pageSize?: number;
		/** Rows actually on this page, for the range text. */
		count: number;
		loading?: boolean;
		href: (p: number) => string;
		/** Accessible name of the <nav> — there is a second <nav> (BottomNav) on the page. */
		label?: string;
		/** id of the region this pager pages, for aria-controls. */
		controls?: string;
	} = $props();

	const pageCount = $derived(total != null ? lastPage(total, pageSize) : null);
	// BOTH ends collapse to 0 on an empty page. `to` used to be computed unconditionally,
	// so an over-range page (a shared `?page=5` link whose feed has since shrunk) rendered
	// "showing 0–80 of 63" for the one frame before the screen's repair effect navigates
	// to the last real page — a range that is both backwards and impossible.
	const from = $derived(count === 0 ? 0 : (page - 1) * pageSize + 1);
	const to = $derived(count === 0 ? 0 : (page - 1) * pageSize + count);

	// One string for both the visible range and the live region, so a sighted user
	// and a screen-reader user are told exactly the same thing. With `total`
	// unknown we never invent a page count — `N+` is the honest form.
	const rangeText = $derived(
		loading
			? `Loading page ${page}…`
			: pageCount != null && total != null
				? `Page ${page} of ${pageCount.toLocaleString()} · showing ${from.toLocaleString()}–${to.toLocaleString()} of ${total.toLocaleString()}`
				: `Page ${page} · showing ${from.toLocaleString()}–${to.toLocaleString()}${hasNext ? '+' : ''}`,
	);

	type Slot = { key: string; n: number; gap: boolean };

	// 1 … p-1 [p] p+1 … last, gaps collapsed, always including the first and last
	// page so a deep page can get home in one click. Empty when `total` is null:
	// we cannot know how many pages exist, and a fabricated "last" link is worse
	// than no numbers at all (the Prev/Next pair still works).
	const pageWindow = $derived.by<Slot[]>(() => {
		if (pageCount == null || pageCount <= 1) return [];
		const wanted = new Set<number>([1, pageCount, page]);
		if (page - 1 > 1) wanted.add(page - 1);
		if (page + 1 < pageCount) wanted.add(page + 1);
		const nums = [...wanted].sort((a, b) => a - b);
		const out: Slot[] = [];
		let prev = 0;
		for (const n of nums) {
			// A single skipped page renders as that page, not as an ellipsis hiding one link.
			if (prev && n - prev === 2) out.push({ key: `n${prev + 1}`, n: prev + 1, gap: false });
			else if (prev && n - prev > 2) out.push({ key: `g${prev}`, n: 0, gap: true });
			out.push({ key: `n${n}`, n, gap: false });
			prev = n;
		}
		return out;
	});

	const showPrev = $derived(page > 1 && !loading);
	const showNext = $derived(hasNext && !loading);
</script>

<!-- Hidden entirely when there is nothing to page: page 1 with no next page.
     Mirrors the admin pager's `{#if page > 1 || hasNext}`. -->
{#if page > 1 || hasNext}
	<nav class="pager" aria-label={label} aria-controls={controls}>
		<div class="row">
			<!-- Prev/Next are a real link when navigable and an INERT SPAN otherwise.
			     HTML has no disabled link; `aria-disabled` on a non-focusable span is
			     the correct encoding, and keeping the element (rather than dropping it)
			     holds the row's width steady so it doesn't jump between pages. -->
			{#if showPrev}
				<a class="pg" rel="prev" href={href(page - 1)} aria-label="Previous page, page {page - 1}">
					<span aria-hidden="true">←</span> Prev
				</a>
			{:else}
				<span class="pg is-disabled" aria-disabled="true"
					><span aria-hidden="true">←</span> Prev</span
				>
			{/if}

			{#if pageWindow.length}
				<ol class="pages">
					{#each pageWindow as p (p.key)}
						{#if p.gap}
							<li class="gap" aria-hidden="true">…</li>
						{:else if p.n === page}
							<!-- aria-current="page" is the only correct marker for the current page
							     in a pagination set; the `.on` class alone says nothing to AT. -->
							<li>
								<a class="pg num on" href={href(p.n)} aria-current="page" aria-label="Page {p.n}"
									>{p.n}</a
								>
							</li>
						{:else}
							<li><a class="pg num" href={href(p.n)} aria-label="Page {p.n}">{p.n}</a></li>
						{/if}
					{/each}
				</ol>
			{:else}
				<!-- Unknown total: no numbered links, just where we are. -->
				<span class="pg num on" aria-current="page">{page}</span>
			{/if}

			{#if showNext}
				<a class="pg" rel="next" href={href(page + 1)} aria-label="Next page, page {page + 1}">
					Next <span aria-hidden="true">→</span>
				</a>
			{:else}
				<span class="pg is-disabled" aria-disabled="true"
					>Next <span aria-hidden="true">→</span></span
				>
			{/if}
		</div>
		<p class="range" role="status" aria-live="polite">{rangeText}</p>
	</nav>
{/if}

<style>
	.pager {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 10px;
		padding: 8px 0 4px;
	}
	.row {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		justify-content: center;
		gap: 8px;
	}
	/* WRAPS. `.row` wraps, but the number list is one flex ITEM of it and could not
	   break internally, so it overflowed the viewport instead. Worst case is 5 numbers
	   + 2 ellipses + 6 gaps ≈ 308px against 280px of content width on a 320px phone
	   (--k-gutter is 20px a side at ≤640px) — a horizontally scrolling page body, which
	   the layout forbids. A second line of numbers is the correct overflow. */
	.pages {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: center;
		gap: 8px;
		list-style: none;
		margin: 0;
		padding: 0;
	}
	/* ≥44px hit targets, matching the load-more button this replaces. */
	.pg {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 6px;
		min-width: 44px;
		height: 44px;
		padding: 0 18px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		font-weight: 700;
		font-size: 13.5px;
		text-decoration: none;
		transition:
			border-color 0.15s,
			background 0.15s,
			color 0.15s;
	}
	.pg.num {
		padding: 0 12px;
	}
	a.pg:hover {
		border-color: var(--k-border-strong);
	}
	a.pg:focus-visible {
		outline: 2px solid var(--k-primary);
		outline-offset: 2px;
	}
	.pg.on {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.pg.is-disabled {
		opacity: 0.4;
		cursor: default;
	}
	.gap {
		min-width: 20px;
		text-align: center;
		font-size: 13.5px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	.range {
		margin: 0;
		font-size: 12.5px;
		color: var(--k-text-faint);
		text-align: center;
	}
	@media (max-width: 480px) {
		/* Tighten the horizontal padding only. The `min-width: 44px` is NOT narrowed
		   here: it is the touch target, and shrinking it to 38px to dodge a second line
		   traded the one guarantee this row makes for a cosmetic one — on the smallest
		   screen, where thumbs are the only pointer. `.pages` wraps instead. */
		.pg {
			padding: 0 12px;
		}
		.pg.num {
			padding: 0 8px;
		}
	}
</style>
