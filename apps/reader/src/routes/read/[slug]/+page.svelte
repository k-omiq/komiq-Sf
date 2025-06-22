<script lang="ts">
	import { goto, beforeNavigate, preloadData } from '$app/navigation';
	import { navigating } from '$app/state';
	import { untrack, onDestroy } from 'svelte';
	import { saveProgress, recordView } from '$lib/data/source';
	import { externalChapterHost } from '$lib/data/chapter-label';
	import { setPreferredTranslator } from '$lib/data/translator-pref.svelte';
	import { auth } from '$lib/auth.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import CommentThread from '$lib/components/CommentThread.svelte';

	let { data } = $props();

	const seriesTitle = $derived(data.seriesTitle);
	const seriesHref = $derived(`/series/${data.seriesId}`);
	const pages = $derived(data.pages);
	const total = $derived(data.pages.length);
	const chNum = $derived(data.chNum);
	const chTitle = $derived(data.chTitle);
	// Guarded on `nextChapterId` FIRST: `ReaderChapterRef.id` is optional, so a bare
	// `find(c => c.id === undefined)` matches the first id-less chapter and makes
	// "Up next" truthy on the LAST chapter — where the end-of-chapter block is already
	// rendering "You're all caught up", so the reader got both at once.
	const nextChapter = $derived(
		data.nextChapterId ? data.chapters.find((c) => c.id === data.nextChapterId) : undefined,
	);

	// Off-site chapter: the publisher hosts it (MangaPlus, Comikey, NamiComi, BiliBili —
	// ~35,000 chapters, 4% of the mirror), so we have no pages and never will. This is the
	// ONE state that must be checked before `!hasPages`: both render zero pages, but only
	// this one has somewhere to send the reader, and the no-pages copy ("often a licensing
	// gap at the source") is actively wrong about it. Already validated to http(s) by
	// `externalChapterHref` in source.ts — never raw upstream text in an href.
	const externalUrl = $derived(data.externalUrl ?? null);
	const externalHost = $derived(externalChapterHost(data.externalUrl));

	// Translators (sources) for this work — switchable from reader settings. Picking
	// a different source persists the preference and reopens the work under it
	// (chapter ids differ per source, so we land on the new source's first/unread
	// chapter rather than a stale ch param).
	const translators = $derived(data.translators ?? []);
	function switchTranslator(key: string): void {
		if (!data.workId || key === data.selectedTranslatorKey) return;
		setPreferredTranslator(data.workId, key);
		void saveProgress(data.chapterId, currentPageIndex(), readMarked);
		settingsOpen = false;
		// Drop any `?ch=` (chapter ids differ per source) and force the load to re-run
		// even when we're already at the bare `/read/<workId>` URL — otherwise
		// SvelteKit treats it as the same URL and the source wouldn't actually swap.
		goto(`/read/${data.workId}`, { invalidateAll: true });
	}

	let mode = $state<'strip' | 'page'>('strip');
	let pageIdx = $state(0);
	let width = $state(720);
	const hasPages = $derived(total > 0);
	let chromeVisible = $state(true);
	let lockChrome = $state(false);
	let settingsOpen = $state(false);
	let chapterListOpen = $state(false);
	let progressPct = $state(0);
	let scrolledPage = $state(1);
	let maxScrolledPage = $state(1);
	const warmedUrls = new Set<string>();

	const chKey = $derived(`${data.seriesId}:${data.chNum}`);

	// True while a navigation to a DIFFERENT chapter of this reader is in flight.
	// SvelteKit blocks a same-route navigation on its load, so without this the old
	// chapter's pages stay fully painted and the reader looks frozen. Scoped to
	// `/read/` targets so leaving for the series page or a comment link doesn't fade.
	const chapterLoading = $derived(navigating.to?.url.pathname.startsWith('/read/') ?? false);

	// Popularity: count ONE view per chapter open — for every reader, signed-in or
	// anonymous (the product decision is to count everyone). Keyed on the chapter so
	// opening a new chapter counts again but re-renders of the same one don't. A page
	// reload legitimately re-counts (no per-session dedup, by design).
	let lastViewedChapter = '';
	$effect(() => {
		const key = chKey;
		// Only count a view when the chapter actually rendered pages — an empty/broken
		// ("No pages available") reader state must not inflate the popularity signal.
		if (!data.seriesId || data.pages.length === 0 || key === lastViewedChapter) return;
		lastViewedChapter = key;
		void recordView(data.seriesId);
	});

	let lastY = 0;
	let restored = false;
	let readMarked = $state(false);

	// Reset page + scroll position whenever the loaded chapter changes.
	$effect(() => {
		data.chapterId;
		untrack(() => {
			pageIdx = 0;
			scrolledPage = 1;
			maxScrolledPage = 1;
			readMarked = false;
			pageAspect = {};
			pageError = {};
			warmedUrls.clear();
			settledPages.clear();
			queueUntil = STARTER_PAGES;
		});
	});

	// The native image provider hands back `blob:` object URLs for pages; those pin
	// memory until explicitly revoked, so a long reading session (chapter after
	// chapter) leaks. Track the current chapter's page URLs and revoke the blob
	// ones when the chapter changes (this effect's cleanup runs before it re-runs
	// with the next chapter's pages) and on teardown. Web/proxy pages are plain
	// http(s) strings — the `blob:` guard leaves those untouched. Because `pages`
	// is derived from the loaded chapter, the URLs captured here are only revoked
	// once the DOM has moved on to a different chapter, so nothing in view breaks.
	$effect(() => {
		const current = pages;
		return () => {
			for (const p of current) {
				if (typeof p.url === 'string' && p.url.startsWith('blob:')) {
					URL.revokeObjectURL(p.url);
				}
			}
		};
	});

	// The current 0-indexed page, however the chapter is being read.
	function currentPageIndex(): number {
		return mode === 'strip' ? Math.max(0, scrolledPage - 1) : pageIdx;
	}

	// Mark the chapter read once the reader reaches the end (idempotent per chapter).
	function maybeMarkRead(): void {
		if (readMarked || !data.chapterId) return;
		const atEnd = mode === 'strip' ? progressPct >= 98 : pageIdx >= total - 1;
		if (atEnd) {
			readMarked = true;
			void saveProgress(data.chapterId, Math.max(0, total - 1), true);
		}
	}

	// Persist the current reading position. Reading otherwise only saves on in-app
	// Prev/Next and at ≥98%, so a browser-back or header-link exit would drop a
	// mid-chapter position. Save on navigation away, on tab-hide, and on unmount.
	// saveProgress is idempotent latest-wins, so overlapping triggers are harmless.
	function saveCurrentProgress(): void {
		if (!data.chapterId) return;
		void saveProgress(data.chapterId, currentPageIndex(), readMarked);
	}
	beforeNavigate(() => saveCurrentProgress());
	$effect(() => {
		const onVis = () => {
			if (document.visibilityState === 'hidden') saveCurrentProgress();
		};
		document.addEventListener('visibilitychange', onVis);
		return () => document.removeEventListener('visibilitychange', onVis);
	});
	onDestroy(() => saveCurrentProgress());

	const isStrip = $derived(mode === 'strip');
	const chromeHidden = $derived(!chromeVisible && !lockChrome);
	const currentPage = $derived(pages[Math.min(pageIdx, total - 1)]);
	const pageLabel = $derived(`${Math.min(pageIdx, total - 1) + 1} / ${total}`);
	const readout = $derived(
		isStrip
			? `Page ${scrolledPage} of ${total} · ${Math.round(progressPct)}%`
			: `Page ${pageLabel}`,
	);

	function persist() {
		try {
			localStorage.setItem('komiq-reader', JSON.stringify({ mode, width }));
		} catch {
			/* ignore */
		}
	}

	async function openChapter(id?: string) {
		chapterListOpen = false;
		if (!id) return;
		// Persist how far we got in the chapter we're leaving.
		void saveProgress(data.chapterId, currentPageIndex(), readMarked);
		// Keep prev/next on the same source (S2) — chapter ids are per-source.
		const src = data.readingSrc ? `&src=${data.readingSrc}` : '';
		await goto(`/read/${data.seriesId}?ch=${id}${src}`);
		// Jump AFTER the navigation resolves, and instantly. The old code fired a
		// SMOOTH scroll straight after an un-awaited `goto`, so the animation ran down
		// the outgoing chapter while its replacement loaded — motion that belonged to
		// content the reader was already leaving.
		window.scrollTo({ top: 0, behavior: 'auto' });
	}

	// ---- next-chapter prefetch ---------------------------------------------
	// Opening a chapter is a client-side waterfall (chapter list → page list → image
	// bytes); with nothing warmed, all of it starts on the click, which is the pause
	// readers feel between chapters. Once the reader is far enough in to probably
	// continue, run the very load the navigation would run — SvelteKit reuses a
	// preloaded result for the matching `goto`, so the click resolves from memory —
	// then warm the first few page images off that result, since decoded bytes are
	// the part that actually keeps the next chapter blank.
	const PREFETCH_AT_PCT = 40;
	const PREFETCH_IMAGES = 3;
	let prefetchedFor = '';
	$effect(() => {
		const id = data.nextChapterId;
		const deepEnough = progressPct >= PREFETCH_AT_PCT;
		// Off-site next chapter (MangaPlus/Comikey/…): its load deliberately returns no
		// pages, so prefetching it can only ever warm an empty result. Skip the request
		// rather than spend it — the reader is leaving for the publisher anyway.
		if (!id || !deepEnough || nextChapter?.externalUrl || prefetchedFor === id) return;
		// Set before awaiting so a burst of scroll frames can't launch it twice.
		prefetchedFor = id;
		const src = data.readingSrc ? `&src=${data.readingSrc}` : '';
		void preloadData(`/read/${data.seriesId}?ch=${id}${src}`)
			.then((res) => {
				// `redirect` results carry no data; only a `loaded` one has pages.
				if (res?.type !== 'loaded') return;
				const next = (res.data as { pages?: { url?: string }[] } | undefined)?.pages ?? [];
				for (const pg of next.slice(0, PREFETCH_IMAGES)) {
					if (!pg?.url) continue;
					// A detached Image() populates the HTTP cache; the <img> the next
					// chapter renders then hits that cache instead of the network.
					const warm = new Image();
					warm.decoding = 'async';
					warm.src = pg.url;
				}
			})
			// Best-effort only: a failed prefetch must never surface as an unhandled
			// rejection, and the real navigation will fetch (and report) for itself.
			.catch(() => {});
	});

	$effect(() => {
		if (!restored) {
			restored = true;
			try {
				const saved = JSON.parse(localStorage.getItem('komiq-reader') || '{}');
				if (saved) {
					mode = saved.mode || 'strip';
					width = saved.width || 720;
				}
			} catch {
				/* ignore */
			}
		}
		// Coalesce scroll work into one rAF per frame — reading layout + writing
		// reactive state on every scroll event is a jank source on fast scrolls.
		let ticking = false;
		const update = () => {
			ticking = false;
			const doc = document.documentElement;
			const max = doc.scrollHeight - window.innerHeight;
			const y = window.scrollY;
			const pct = max > 0 ? Math.min(100, Math.max(0, (y / max) * 100)) : 0;
			progressPct = pct;
			scrolledPage = Math.min(total, Math.max(1, Math.round((pct / 100) * (total - 1)) + 1));
			if (scrolledPage > maxScrolledPage) {
				maxScrolledPage = scrolledPage;
			}
			maybeMarkRead();
			if (!lockChrome) {
				const dy = y - lastY;
				if (y < 80) chromeVisible = true;
				else if (dy > 6) chromeVisible = false;
				else if (dy < -6) chromeVisible = true;
			} else {
				chromeVisible = true;
			}
			lastY = y;
		};
		const onScroll = () => {
			if (ticking) return;
			ticking = true;
			requestAnimationFrame(update);
		};
		window.addEventListener('scroll', onScroll, { passive: true });
		// The priming call reads `total`/`lockChrome` (and, via maybeMarkRead, more
		// reactive state). Without untrack those become deps of THIS effect, so every
		// chrome-lock toggle / chapter change would tear down and re-add the scroll
		// listener. The listener only needs binding once for the component's life;
		// scroll-time invocations of `update` run outside any tracking scope already.
		untrack(() => update());
		return () => window.removeEventListener('scroll', onScroll);
	});

	// ---- lookahead prefetch & long-strip rendering -------------------------
	// A chapter can be 37+ full-size images. We DON'T hand-virtualize with an
	// IntersectionObserver — mounting/unmounting <img> on scroll thrashes the DOM
	// and is what caused the visible scroll bumps. Instead every page stays in the
	// DOM. But native `loading="lazy"` uses the browser's fixed distance threshold
	// (Chrome is ~1250px), which is too small for tall manga pages: the fetch only
	// starts when the page is nearly on screen, so scrolling down forces readers to
	// wait seconds for images to decode.
	//
	// We compute `loading` reactively per image and the browser starts fetching
	// the moment `loading` flips from 'lazy' to 'eager', so no detached Image
	// allocations are needed in strip mode. Three things can flip a page eager:
	//
	// 1. STARTERS — the first STARTER_PAGES pages fetch at once from first paint
	//    with `fetchpriority="high"`, so the reader sees something as fast as the
	//    connection allows. Nothing else is in flight while they download.
	// 2. BACKGROUND QUEUE — every remaining page is auto-loaded in order WITHOUT
	//    waiting for a scroll trigger, but only QUEUE_CONCURRENCY at a time and
	//    with `fetchpriority="low"`. Firing the whole chapter at once made the
	//    starters share bandwidth with 30+ pages the reader hadn't reached yet,
	//    which on slow wifi meant a long blank first screen. The queue only
	//    advances as pages settle (load or error), so it cannot start until the
	//    starters are nearly done and it never fans out.
	// 3. SCROLL LOOKAHEAD — when the reader outruns the queue, the window keeps
	//    EAGER_AHEAD pages in flight ahead of the scroll position so they never
	//    wait on the queue, at normal priority. A monotonically non-decreasing
	//    high-water mark ensures scrolling back up never flips an image back to lazy.
	const STARTER_PAGES = 3;
	const QUEUE_CONCURRENCY = 2;
	const EAGER_AHEAD = 6;
	let queueUntil = $state(STARTER_PAGES);
	const settledPages = new Set<number>();
	const eagerUntil = $derived(Math.max(queueUntil, maxScrolledPage + EAGER_AHEAD));

	// Pages the queue has released that have not settled yet count as in flight;
	// pages the lookahead already loaded are skipped over rather than re-counted.
	function advanceQueue(): void {
		let inFlight = 0;
		for (let i = 0; i < queueUntil; i++) if (!settledPages.has(i)) inFlight++;
		let next = queueUntil;
		while (next < total && inFlight < QUEUE_CONCURRENCY) {
			if (!settledPages.has(next)) inFlight++;
			next++;
		}
		if (next !== queueUntil) queueUntil = next;
	}
	function settlePage(index: number): void {
		if (settledPages.has(index)) return;
		settledPages.add(index);
		advanceQueue();
	}

	let pageAspect = $state<Record<number, string>>({});
	// A neutral manga default (~2:3) used only until a page's real ratio is known.
	const DEFAULT_ASPECT = '800 / 1200';
	// Per-page load failures — a failed <img> otherwise leaves a permanent
	// full-height gap with the browser's broken-image glyph and no way to retry.
	let pageError = $state<Record<number, boolean>>({});

	function onImgLoad(e: Event, index: number): void {
		const img = e.currentTarget as HTMLImageElement;
		if (img.naturalWidth > 0 && img.naturalHeight > 0) {
			pageAspect = { ...pageAspect, [index]: `${img.naturalWidth} / ${img.naturalHeight}` };
		}
		settlePage(index);
	}
	function onImgError(index: number): void {
		pageError = { ...pageError, [index]: true };
		settlePage(index);
	}
	// Clearing the flag remounts a fresh <img> for that page, re-fetching the URL.
	function retryImage(index: number): void {
		const next = { ...pageError };
		delete next[index];
		pageError = next;
	}

	// ---- paged mode prefetch -----------------------------------------------
	// Single-page mode renders only the active page in the DOM. When `pageIdx`
	// advances, warm the next 3 pages into the HTTP cache with detached Image()
	// objects (matching the next-chapter prefetch pattern) so page turns feel
	// instantaneous. `warmedUrls` keeps each URL to one warm per chapter; the
	// chapter-change effect above clears it.
	const PAGED_PREFETCH = 3;
	$effect(() => {
		if (mode !== 'page') return;
		const next = pages.slice(pageIdx + 1, pageIdx + 1 + PAGED_PREFETCH);
		for (const pg of next) {
			if (!pg?.url || warmedUrls.has(pg.url)) continue;
			warmedUrls.add(pg.url);
			const warm = new Image();
			warm.decoding = 'async';
			warm.src = pg.url;
		}
	});

	const chapterMenu = $derived(
		data.chapters.map((c) => ({
			id: c.id,
			n: c.n,
			title: c.title,
			current: c.id === data.chapterId,
			// Badged in the list, not discovered on arrival: ~35,000 chapters (4% of the
			// mirror) are hosted by MangaPlus/Comikey/NamiComi/BiliBili, and clicking one
			// leaves the app. Knowing that beforehand is the difference between a link and
			// a dead end.
			external: !!c.externalUrl,
		})),
	);

	function stageClick() {
		if (!lockChrome) chromeVisible = !chromeVisible;
	}
	function setMode(m: 'strip' | 'page') {
		mode = m;
		pageIdx = 0;
		persist();
	}
	function nextPage() {
		pageIdx = Math.min(total - 1, pageIdx + 1);
		maybeMarkRead();
		persist();
	}
	function prevPage() {
		pageIdx = Math.max(0, pageIdx - 1);
		persist();
	}

	// Keyboard operability: arrows/space page through single-page mode, Escape
	// closes any open overlay. Ignore when typing in the comment box.
	function onKeydown(e: KeyboardEvent) {
		const el = e.target as HTMLElement | null;
		if (el && (el.tagName === 'TEXTAREA' || el.tagName === 'INPUT')) return;
		if (e.key === 'Escape') {
			if (settingsOpen || chapterListOpen) {
				e.preventDefault();
				settingsOpen = false;
				chapterListOpen = false;
			}
			return;
		}
		if (mode !== 'page') return;
		if (e.key === 'ArrowRight' || e.key === 'PageDown' || e.key === ' ') {
			e.preventDefault();
			nextPage();
		} else if (e.key === 'ArrowLeft' || e.key === 'PageUp') {
			e.preventDefault();
			prevPage();
		}
	}
</script>

<svelte:head>
	<title>{seriesTitle ? `Ch. ${chNum} · ${seriesTitle} · komiq` : `Ch. ${chNum} · komiq`}</title>
</svelte:head>

<svelte:window onkeydown={onKeydown} />

<!-- progress bar -->
<div class="progress"><div class="progress-fill" style="width:{progressPct}%"></div></div>

<!-- top chrome -->
<header class="chrome top" class:hidden={chromeHidden}>
	<div class="top-left">
		<a class="back" href={seriesHref} aria-label="Back to series"
			><Icon name="chevron-left" size={18} /></a
		>
		<div class="titlebox">
			<a class="series" href={seriesHref}>{seriesTitle || 'Back to series'}</a>
			<!-- Series title only. The chapter is already named twice on this screen —
			     the picker at top-right and the chapter heading below — so repeating it
			     under the series title was pure noise. -->
		</div>
	</div>
	<div class="top-right">
		{#if data.chapters.length}
			<div class="ch-select">
				<button
					class="ch-btn"
					aria-haspopup="menu"
					aria-expanded={chapterListOpen}
					onclick={() => {
						chapterListOpen = !chapterListOpen;
						settingsOpen = false;
					}}
				>
					Chapter {chNum}<Icon name="chevron-down" size={14} />
				</button>
				{#if chapterListOpen}
					<div class="ch-menu">
						{#each chapterMenu as c (c.id ?? c.n)}
							<button class="ch-item" class:current={c.current} onclick={() => openChapter(c.id)}>
								<div class="ch-item-info">
									<div class="ch-item-title">Ch. {c.n} · {c.title}</div>
								</div>
								{#if c.external}<span class="ext-tag" title="Hosted on another site"
										><Icon name="globe" size={11} />OFF-SITE</span
									>{/if}
								{#if c.current}<span class="reading-tag">READING</span>{/if}
							</button>
						{/each}
					</div>
				{/if}
			</div>
		{/if}
		<button
			class="settings-btn"
			class:on={settingsOpen}
			aria-label="Reader settings"
			aria-haspopup="dialog"
			aria-expanded={settingsOpen}
			onclick={() => {
				settingsOpen = !settingsOpen;
				chapterListOpen = false;
			}}
		>
			<Icon name="sliders" size={18} />
		</button>
	</div>
</header>

<!-- settings panel -->
{#if settingsOpen}
	<button class="settings-scrim" aria-label="Close settings" onclick={() => (settingsOpen = false)}
	></button>
	<div class="settings">
		<div class="settings-title">Reader settings</div>
		{#if translators.length > 1}
			<div class="setting">
				<span class="setting-label">Translation source</span>
				<div class="tr-opts">
					{#each translators as t (t.key)}
						<button
							class="tr-opt"
							class:on={t.key === data.selectedTranslatorKey}
							onclick={() => switchTranslator(t.key)}
							title={t.lang ? `${t.name} · ${t.lang.toUpperCase()}` : t.name}
						>
							{#if t.iconUrl}
								<img
									class="tr-logo"
									src={t.iconUrl}
									alt=""
									loading="lazy"
									decoding="async"
									referrerpolicy="no-referrer"
								/>
							{:else}
								<span class="tr-logo tr-initial">{t.name.charAt(0).toUpperCase()}</span>
							{/if}
							<span class="tr-name">{t.name}</span>
							<span class="tr-count">{t.chapterCount}</span>
							{#if t.key === data.selectedTranslatorKey}
								<Icon name="check" size={14} strokeWidth={2.6} />
							{/if}
						</button>
					{/each}
				</div>
			</div>
		{/if}
		<div class="setting">
			<span class="setting-label">Reading mode</span>
			<div class="mode-btns">
				<button class="mode" class:on={mode === 'strip'} onclick={() => setMode('strip')}
					>Long strip</button
				>
				<button class="mode" class:on={mode === 'page'} onclick={() => setMode('page')}
					>Single page</button
				>
			</div>
		</div>
		<div class="setting">
			<div class="setting-row">
				<span class="setting-label">Page width</span>
				<span class="setting-val">{width}px</span>
			</div>
			<input type="range" min="480" max="1000" step="20" bind:value={width} oninput={persist} />
		</div>
		<div class="setting toggle-row">
			<span class="toggle-label">Keep chrome visible</span>
			<button
				class="toggle"
				class:on={lockChrome}
				aria-label="Toggle chrome lock"
				onclick={() => {
					lockChrome = !lockChrome;
					chromeVisible = true;
				}}
			>
				<span class="knob"></span>
			</button>
		</div>
	</div>
{/if}

<!-- Tap/keyboard target that toggles the reader chrome. A real <button> keeps
     this operable by keyboard and screen readers (the reading <main> stays a
     landmark, not an interactive element). It sits behind the page content so
     links/paging zones still take clicks. -->
<button class="chrome-toggle" aria-label="Toggle reader controls" onclick={stageClick}></button>

<!-- reading area -->
<main style="--w:{width}px" class:loading={chapterLoading}>
	{#if data.chapterId}
		<!-- Heading only: the kicker above it restated the same chapter number the
		     heading already carries, one line apart. -->
		<div class="chapter-header">
			<h1>{chTitle}</h1>
		</div>
	{/if}

	{#if externalUrl}
		<!-- An off-site chapter (~35,000 of them: MangaPlus, Comikey, NamiComi, BiliBili).
		     It has no pages BY DESIGN, so it must not fall through to the no-pages state
		     below — that message blames "a licensing gap" for a chapter that is licensed
		     and readable, one hop away. Send the reader there, the way MangaDex does.
		     `rel="noopener noreferrer"` because the destination is a third party. -->
		<div class="no-pages">
			<div class="no-pages-icon"><Icon name="globe" size={26} /></div>
			<h2>This chapter is read on {externalHost}</h2>
			<p>
				The publisher hosts it themselves, so there are no pages for us to show. Open it on
				{externalHost} to keep reading.
			</p>
			<div class="no-pages-btns">
				<a class="next-ch ext-go" href={externalUrl} target="_blank" rel="noopener noreferrer">
					Read on {externalHost}<Icon name="arrow-right" size={16} strokeWidth={2.4} />
				</a>
				<a class="all-ch" href={seriesHref}><Icon name="list" size={16} />Back to series</a>
			</div>
		</div>
	{:else if !hasPages}
		<div class="no-pages">
			<div class="no-pages-icon"><Icon name="alert" size={26} /></div>
			<h2>No pages available for this chapter</h2>
			<p>
				This chapter has no readable pages yet — often a licensing gap at the source. Try another
				chapter or head back to the series.
			</p>
			<div class="no-pages-btns">
				{#if data.nextChapterId}
					<button class="next-ch" onclick={() => openChapter(data.nextChapterId)}
						>Next Chapter<Icon name="arrow-right" size={16} strokeWidth={2.4} /></button
					>
				{/if}
				<a class="all-ch" href={seriesHref}><Icon name="list" size={16} />Back to series</a>
			</div>
		</div>
	{:else if isStrip}
		<div class="strip">
			<!-- Keyed on the CHAPTER as well as the page index: `p.index` alone repeats
			     across chapters, so Svelte reused each <img> and only patched `src`,
			     leaving the previous chapter's decoded frame on screen until the new
			     bytes arrived. `data.chapterId` is fixed for the whole block, so the
			     composite key stays as unique as `p.index` already was. -->
			{#each pages as p (`${data.chapterId}:${p.index}`)}
				<div
					class="strip-cell"
					style="aspect-ratio:{pageAspect[p.index] ?? DEFAULT_ASPECT};max-width:{width}px"
				>
					{#if p.url && !pageError[p.index]}
						<img
							class="strip-img"
							src={p.url}
							alt={`Page ${p.label}`}
							loading={p.index < eagerUntil ? 'eager' : 'lazy'}
							fetchpriority={p.index < STARTER_PAGES
								? 'high'
								: p.index < maxScrolledPage + EAGER_AHEAD
									? 'auto'
									: 'low'}
							decoding="async"
							onload={(e) => onImgLoad(e, p.index)}
							onerror={() => onImgError(p.index)}
						/>
					{:else if p.url && pageError[p.index]}
						<button class="strip-ph strip-err k-cover" onclick={() => retryImage(p.index)}>
							<span class="err-msg">
								<Icon name="alert" size={18} />
								<span>Page {p.label} failed — tap to retry</span>
							</span>
						</button>
					{:else}
						<div class="strip-ph k-cover">
							<span class="page-tag">PAGE {p.label} · {p.dim}</span>
							<span class="panel-tag">MANGA PANEL</span>
						</div>
					{/if}
				</div>
			{/each}
		</div>
	{:else}
		<div class="paged">
			<div
				class="paged-view"
				style={currentPage?.url ? 'height:74vh' : `height:74vh;aspect-ratio:${currentPage?.ratio}`}
			>
				{#if currentPage?.url && !pageError[currentPage.index]}
					<!-- Keyed on the chapter so a chapter change REMOUNTS the <img> instead
					     of patching `src` on the existing one, which left the previous
					     chapter's decoded frame on screen until the new page downloaded. -->
					{#key data.chapterId}
						<img
							class="paged-img"
							src={currentPage.url}
							alt={`Page ${currentPage.label}`}
							decoding="async"
							onerror={() => onImgError(currentPage.index)}
						/>
					{/key}
				{:else if currentPage?.url && pageError[currentPage.index]}
					<button class="paged-ph strip-err k-cover" onclick={() => retryImage(currentPage.index)}>
						<span class="err-msg">
							<Icon name="alert" size={18} />
							<span>Page {currentPage.label} failed — tap to retry</span>
						</span>
					</button>
				{:else}
					<div class="paged-ph k-cover">
						<span class="page-tag">PAGE {currentPage?.label} · {currentPage?.dim}</span>
						<span class="panel-tag">MANGA PANEL</span>
					</div>
				{/if}
				<button
					class="zone left"
					aria-label="Previous page"
					onclick={(e) => {
						e.stopPropagation();
						prevPage();
					}}
				></button>
				<button
					class="zone right"
					aria-label="Next page"
					onclick={(e) => {
						e.stopPropagation();
						nextPage();
					}}
				></button>
			</div>
			<div class="paged-nav">
				<button class="pg-btn" onclick={prevPage}>← Prev</button>
				<span class="pg-label">{pageLabel}</span>
				<button class="pg-btn" onclick={nextPage}>Next →</button>
			</div>
		</div>
	{/if}

	<!-- end of chapter -->
	{#if hasPages}
		<div class="eoc">
			<div class="eoc-check"><Icon name="check" size={24} strokeWidth={2.4} /></div>
			<div>
				<div class="eoc-kicker">END OF CHAPTER {chNum}</div>
				<h2>{chTitle}</h2>
			</div>
			<!-- On the LAST chapter there is no "Next Chapter" button at all. It used to
			     render always and merely carry `disabled`, so the end of a series still
			     said "Next Chapter" — a dead control promising a chapter that doesn't
			     exist. "All chapters" becomes the primary action instead. -->
			<div class="eoc-btns">
				{#if data.nextChapterId}
					<button class="next-ch" onclick={() => openChapter(data.nextChapterId)}
						>Next Chapter<Icon name="arrow-right" size={16} strokeWidth={2.4} /></button
					>
				{/if}
				<a class="all-ch" class:solo={!data.nextChapterId} href={seriesHref}
					><Icon name="list" size={16} />All chapters</a
				>
			</div>
			{#if !data.nextChapterId}
				<p class="eoc-caught-up">You're all caught up — this is the latest chapter we have.</p>
			{/if}
			{#if nextChapter}
				<div class="upnext">
					<span class="upnext-label">Up next</span>
					<button class="upnext-card" onclick={() => openChapter(nextChapter.id)}>
						<div class="upnext-info">
							<div class="upnext-title">Ch. {nextChapter.n} · {nextChapter.title}</div>
							<div class="upnext-sub">{seriesTitle}</div>
						</div>
						<Icon name="chevron-right" size={18} stroke="#57554f" />
					</button>
				</div>
			{/if}
		</div>
	{/if}

	<!-- comments (shared thread component; series discussion uses the same one) -->
	<section class="comments">
		<CommentThread
			targetType="chapter"
			targetId={data.chapterId ?? ''}
			storageKey={chKey}
			prompt="Share your thoughts on this chapter…"
		/>
	</section>
</main>

<!-- bottom chrome -->
<div class="chrome bottom" class:hidden={chromeHidden}>
	<!-- Rendered only when the target chapter exists. `disabled` was invisible here:
	     `.nav-ch` has no styling of its own and nothing styles `button:disabled`, so a
	     dead Prev/Next looked exactly like a live one. The readout below is flex-centred
	     so dropping either button leaves it in place instead of sliding to an edge. -->
	{#if data.prevChapterId}
		<button class="nav-ch" onclick={() => openChapter(data.prevChapterId)}
			><Icon name="chevron-left" size={15} />Prev</button
		>
	{/if}
	<span class="readout">{readout}</span>
	{#if data.nextChapterId}
		<button class="nav-ch" onclick={() => openChapter(data.nextChapterId)}
			>Next<Icon name="chevron-right" size={15} /></button
		>
	{/if}
</div>

<style>
	.progress {
		position: fixed;
		top: 0;
		left: 0;
		right: 0;
		height: 3px;
		z-index: 60;
		background: var(--k-border);
	}
	.progress-fill {
		height: 100%;
		background: var(--k-primary);
		transition: width 0.12s linear;
	}
	.chrome {
		position: fixed;
		left: 0;
		right: 0;
		z-index: 50;
		height: 60px;
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0 24px;
		background: var(--k-glass-strong);
		backdrop-filter: blur(16px);
		transition: transform 0.28s cubic-bezier(0.4, 0, 0.2, 1);
	}
	.chrome.top {
		top: 0;
		border-bottom: 1px solid var(--k-border);
	}
	.chrome.bottom {
		bottom: 0;
		border-top: 1px solid var(--k-border);
	}
	.chrome.top.hidden {
		transform: translateY(-100%);
	}
	.chrome.bottom.hidden {
		transform: translateY(100%);
	}
	/* Takes the free space so the page readout stays centred whether or not the
	   Prev/Next buttons flanking it are rendered (they aren't on the first/last
	   chapter). Without this, `justify-content: space-between` would shove it to
	   whichever edge lost its button. */
	.readout {
		flex: 1;
		text-align: center;
	}
	.top-left {
		display: flex;
		align-items: center;
		gap: 18px;
		min-width: 0;
	}
	.back {
		flex: 0 0 auto;
		width: 38px;
		height: 38px;
		border-radius: 9px;
		background: transparent;
		border: 1px solid var(--k-border-3);
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--k-text-3);
		text-decoration: none;
	}
	.back:hover {
		border-color: rgba(255, 255, 255, 0.34);
		color: var(--k-text);
	}
	.titlebox {
		min-width: 0;
	}
	.series {
		display: block;
		font-weight: 700;
		font-size: 14.5px;
		color: var(--k-text);
		text-decoration: none;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.top-right {
		display: flex;
		align-items: center;
		gap: 10px;
		flex: 0 0 auto;
	}
	.ch-select {
		position: relative;
	}
	.ch-btn {
		display: inline-flex;
		align-items: center;
		gap: 9px;
		height: 38px;
		padding: 0 14px;
		border-radius: 9px;
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-1);
		font-weight: 700;
		font-size: 13px;
		cursor: pointer;
	}
	.ch-btn:hover {
		border-color: var(--k-border-strong);
	}
	.ch-menu {
		position: absolute;
		top: 46px;
		right: 0;
		width: 280px;
		max-height: 360px;
		overflow-y: auto;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		border-radius: 12px;
		box-shadow: 0 26px 70px rgba(0, 0, 0, 0.6);
		padding: 6px;
		z-index: 70;
	}
	.ch-item {
		width: 100%;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
		padding: 10px 12px;
		border-radius: 8px;
		cursor: pointer;
		background: transparent;
		border: none;
		text-align: left;
	}
	.ch-item:hover {
		background: rgba(255, 255, 255, 0.05);
	}
	.ch-item.current {
		background: rgba(255, 255, 255, 0.05);
	}
	.ch-item-title {
		font-weight: 700;
		font-size: 13px;
		color: var(--k-text-2);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.ch-item.current .ch-item-title {
		color: var(--k-text-bright);
	}
	.reading-tag {
		flex: 0 0 auto;
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.05em;
		color: var(--k-on-primary);
		background: var(--k-primary);
		padding: 2px 7px;
		border-radius: 5px;
	}
	/* Off-site marker in the chapter list. Deliberately NOT the primary colour of
	   `.reading-tag`: it flags a departure from the app, not a state to seek out. */
	.ext-tag {
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.05em;
		color: var(--k-text-faint);
		border: 1px solid var(--k-border-4);
		padding: 2px 6px;
		border-radius: 5px;
	}
	/* `.next-ch` is a <button> rule; the off-site CTA is a real <a> (it leaves the app,
	   so it must be openable in a new tab and readable as a link by assistive tech). */
	.ext-go {
		text-decoration: none;
	}
	.settings-btn {
		width: 38px;
		height: 38px;
		flex: 0 0 auto;
		border-radius: 9px;
		background: transparent;
		border: 1px solid var(--k-border-3);
		color: var(--k-text-3);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.settings-btn.on {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.settings-scrim {
		position: fixed;
		inset: 0;
		z-index: 55;
		border: none;
		background: rgba(8, 8, 9, 0.4);
		cursor: default;
	}
	.settings {
		position: fixed;
		z-index: 56;
		top: 68px;
		right: 24px;
		width: 300px;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		border-radius: 16px;
		box-shadow: 0 26px 70px rgba(0, 0, 0, 0.6);
		padding: 22px;
		display: flex;
		flex-direction: column;
		gap: 22px;
	}
	.settings-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 15px;
		color: var(--k-text);
	}
	.setting {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.setting-label {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.09em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.setting-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	.setting-val {
		font-size: 12px;
		font-weight: 700;
		color: var(--k-text-3);
	}
	.mode-btns {
		display: flex;
		gap: 8px;
	}
	.mode {
		flex: 1;
		height: 40px;
		border-radius: 9px;
		font-weight: 700;
		font-size: 13px;
		cursor: pointer;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-3);
		transition: all 0.15s;
	}
	.mode.on {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.tr-opts {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.tr-opt {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 8px 12px;
		border-radius: 10px;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-2);
		cursor: pointer;
		transition: all 0.15s;
		text-align: left;
	}
	.tr-opt:hover {
		border-color: var(--k-border-strong);
	}
	.tr-opt.on {
		border-color: var(--k-primary);
		background: rgba(224, 131, 105, 0.12);
		color: var(--k-text-bright);
	}
	.tr-logo {
		width: 26px;
		height: 26px;
		flex: 0 0 auto;
		border-radius: 6px;
		object-fit: cover;
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
	}
	.tr-initial {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		font-size: 12px;
		font-weight: 800;
	}
	.tr-name {
		flex: 1;
		min-width: 0;
		font-size: 13px;
		font-weight: 700;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.tr-count {
		font-size: 11px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	.setting input[type='range'] {
		width: 100%;
		cursor: pointer;
	}
	.toggle-row {
		flex-direction: row;
		align-items: center;
		justify-content: space-between;
		padding-top: 4px;
		border-top: 1px solid var(--k-border);
	}
	.toggle-label {
		font-size: 13px;
		color: var(--k-text-2);
		font-weight: 600;
	}
	.toggle {
		width: 44px;
		height: 26px;
		border-radius: 999px;
		border: none;
		cursor: pointer;
		padding: 3px;
		display: flex;
		justify-content: flex-start;
		background: var(--k-border-4);
		transition: background 0.18s;
	}
	.toggle.on {
		justify-content: flex-end;
		background: var(--k-ongoing);
	}
	.knob {
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--k-bg);
	}
	main {
		position: relative;
		z-index: 1;
		min-height: 100vh;
		padding: 60px 0 0;
		display: flex;
		flex-direction: column;
		align-items: center;
		pointer-events: none;
	}
	/* Re-enable pointer events on interactive controls and readable text, so the
	   chrome-toggle button underneath only receives taps on the page images,
	   gaps and margins (preserving tap-anywhere-to-toggle in strip mode) while
	   links work and comment/synopsis text stays selectable. */
	main :global(a),
	main :global(button),
	main :global(input),
	main :global(textarea),
	main :global(label),
	main :global(p),
	main :global(h1),
	main :global(h2),
	main :global(h3),
	main :global(.c-text),
	main :global(.c-name),
	main :global(.c-time) {
		pointer-events: auto;
	}
	/* Chapter swap in progress — see `chapterLoading`. Dimming the outgoing chapter is
	   the whole feedback: the pages themselves cannot change until the load resolves.
	   The blocker has to be `:global(*)` rather than the plain `main.loading` of a
	   simpler reader: the rule right above re-enables pointer events on every link and
	   button inside `main`, and would out-specify a bare `main.loading`. Targeting the
	   descendants directly takes the clicks back, so a second click can't land on
	   content that is already on its way out. */
	main.loading {
		opacity: 0.32;
		transition: opacity 0.12s ease-out;
	}
	main.loading :global(*) {
		pointer-events: none;
	}
	.chrome-toggle {
		position: fixed;
		inset: 0;
		z-index: 0;
		width: 100%;
		height: 100%;
		border: none;
		background: transparent;
		cursor: pointer;
		padding: 0;
	}
	.chapter-header {
		width: 100%;
		max-width: var(--w);
		padding: 32px 20px 22px;
		text-align: center;
	}
	.chapter-header h1 {
		font-size: 30px;
		letter-spacing: -0.02em;
		margin: 0;
		color: var(--k-text-bright);
	}
	.no-pages {
		width: 100%;
		max-width: 480px;
		margin: 40px 0;
		padding: 40px 24px;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		text-align: center;
	}
	.no-pages-icon {
		width: 60px;
		height: 60px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-2);
		color: var(--k-hiatus);
	}
	.no-pages h2 {
		font-size: 22px;
		letter-spacing: -0.02em;
		margin: 0;
		color: var(--k-text-bright);
	}
	.no-pages p {
		margin: 0;
		max-width: 360px;
		font-size: 14px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	.no-pages-btns {
		display: flex;
		gap: 12px;
		flex-wrap: wrap;
		justify-content: center;
		margin-top: 10px;
	}
	.strip {
		display: flex;
		flex-direction: column;
		align-items: center;
		width: 100%;
	}
	/* Each cell reserves its page's space via the image's real aspect-ratio, so
	   scroll geometry stays stable while offscreen images are torn down. Pages
	   stack seamlessly — no borders/margins between them. */
	.strip-cell {
		position: relative;
		display: block;
		width: 100%;
		background: var(--k-bg);
		/* The cell's aspect-ratio is only a pre-load *reservation* (DEFAULT until the
		   real ratio is measured on load). A loaded image renders at its natural
		   height (see .strip-img), which for the ~1 frame before the measured ratio is
		   applied can be taller than the DEFAULT 2:3 box — clip that overflow so it
		   never overlaps the next page. Once the real ratio lands the box matches the
		   image exactly and nothing is clipped. */
		overflow: hidden;
	}
	.strip-ph {
		position: absolute;
		inset: 0;
		background-image: repeating-linear-gradient(
			135deg,
			rgba(255, 255, 255, 0.028) 0 2px,
			transparent 2px 12px
		);
	}
	.strip-img {
		display: block;
		width: 100%;
		/* Fill the cell width and let intrinsic height govern. The previous
		   `height:100%; object-fit:contain` fitted every page into the cell's
		   *reserved* 2:3 box — so a taller-than-2:3 page (most manga/webtoon pages)
		   was shrunk to a narrow, letterboxed column until the real ratio was measured
		   and the box reshaped ("zoomed out for a couple seconds"). Sizing by width
		   with natural height renders the page correctly the instant it paints. */
		height: auto;
	}
	/* Failed-page retry target (strip + paged share the look). */
	.strip-err {
		display: flex;
		align-items: center;
		justify-content: center;
		cursor: pointer;
		color: var(--k-text-dim);
	}
	.err-msg {
		display: inline-flex;
		align-items: center;
		gap: 10px;
		font-size: 13px;
		font-weight: 600;
		color: var(--k-hiatus);
		padding: 0 16px;
		text-align: center;
	}
	.strip-err:hover .err-msg {
		color: var(--k-text);
	}
	.page-tag {
		position: absolute;
		top: 14px;
		left: 14px;
		font-family: var(--k-font-mono);
		font-size: 10.5px;
		letter-spacing: 0.16em;
		color: #54545c;
	}
	.panel-tag {
		position: absolute;
		inset: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		font-family: var(--k-font-mono);
		font-size: 11px;
		letter-spacing: 0.24em;
		color: #33333a;
	}
	.paged {
		width: 100%;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 16px;
		padding: 4px 0 20px;
	}
	.paged-view {
		position: relative;
		width: 100%;
		height: 74vh;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.paged-img {
		height: 100%;
		width: auto;
		max-width: 100%;
		object-fit: contain;
		border-radius: 6px;
	}
	.paged-ph {
		position: relative;
		height: 100%;
		aspect-ratio: 800 / 1200;
		border: 1px solid var(--k-border);
		border-radius: 6px;
		background-image: repeating-linear-gradient(
			135deg,
			rgba(255, 255, 255, 0.028) 0 2px,
			transparent 2px 12px
		);
	}
	.zone {
		position: absolute;
		top: 0;
		bottom: 0;
		width: 38%;
		background: transparent;
		border: none;
		cursor: pointer;
	}
	.zone.left {
		left: 0;
		cursor: w-resize;
	}
	.zone.right {
		right: 0;
		cursor: e-resize;
	}
	.paged-nav {
		display: flex;
		align-items: center;
		gap: 20px;
	}
	.pg-btn {
		height: 36px;
		padding: 0 16px;
		border-radius: 8px;
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-1);
		font-weight: 700;
		font-size: 13px;
		cursor: pointer;
	}
	.pg-btn:hover {
		border-color: var(--k-border-strong);
	}
	.pg-label {
		font-family: var(--k-font-display);
		font-weight: 700;
		color: var(--k-text);
	}
	.eoc {
		width: 100%;
		max-width: 640px;
		padding: 64px 20px 96px;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 26px;
		text-align: center;
	}
	.eoc-check {
		width: 52px;
		height: 52px;
		border-radius: 50%;
		background: rgba(95, 191, 126, 0.14);
		border: 1px solid rgba(95, 191, 126, 0.4);
		display: flex;
		align-items: center;
		justify-content: center;
		color: #7fd39a;
	}
	.eoc-kicker {
		font-size: 13px;
		letter-spacing: 0.14em;
		color: var(--k-text-disabled);
		margin-bottom: 8px;
	}
	.eoc h2 {
		font-size: 26px;
		letter-spacing: -0.02em;
		margin: 0;
		color: var(--k-text-bright);
	}
	.eoc-btns {
		display: flex;
		gap: 12px;
		flex-wrap: wrap;
		justify-content: center;
		margin-top: 4px;
	}
	.next-ch {
		height: 50px;
		padding: 0 28px;
		border: none;
		border-radius: 9px;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 15px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		gap: 9px;
	}
	.all-ch {
		height: 50px;
		padding: 0 24px;
		border-radius: 9px;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text);
		font-weight: 700;
		font-size: 14px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		gap: 9px;
		text-decoration: none;
	}
	.all-ch:hover {
		border-color: rgba(255, 255, 255, 0.34);
	}
	/* Last chapter: with no "Next Chapter" beside it, this is the only action left,
	   so it takes the primary treatment rather than sitting alone as a lone outline. */
	.all-ch.solo {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.all-ch.solo:hover {
		border-color: var(--k-primary);
		filter: brightness(1.08);
	}
	.eoc-caught-up {
		margin: 14px 0 0;
		font-size: 13.5px;
		color: var(--k-text-dimmer);
	}
	.upnext {
		width: 100%;
		margin-top: 22px;
		padding-top: 26px;
		border-top: 1px solid var(--k-border);
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.upnext-label {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.09em;
		text-transform: uppercase;
		color: var(--k-text-faint);
		text-align: left;
	}
	.upnext-card {
		display: flex;
		align-items: center;
		gap: 16px;
		padding: 14px;
		border-radius: 12px;
		border: 1px solid var(--k-border-1);
		background: var(--k-surface);
		cursor: pointer;
		text-align: left;
		transition: all 0.15s;
	}
	.upnext-card:hover {
		border-color: rgba(255, 255, 255, 0.22);
		background: var(--k-surface-2);
	}
	.upnext-info {
		flex: 1;
		min-width: 0;
	}
	.upnext-title {
		font-weight: 700;
		font-size: 14.5px;
		color: var(--k-text-1);
	}
	.upnext-sub {
		font-size: 12px;
		color: var(--k-text-fainter);
		margin-top: 3px;
	}
	.comments {
		width: 100%;
		max-width: 720px;
		padding: 12px 20px 120px;
		display: flex;
		flex-direction: column;
		gap: 22px;
	}
	@media (max-width: 640px) {
		.chapter-header h1 {
			font-size: 24px;
		}
		/* Tighten top/bottom chrome padding for narrow screens. */
		.chrome {
			padding: 0 14px;
		}
		.top-left {
			gap: 12px;
		}
		/* The chapter dropdown must never exceed the viewport width. */
		.ch-menu {
			width: min(280px, calc(100vw - 28px));
		}
		/* Reader settings become a full-width bottom sheet (thumb-reachable), with
		   the translator switcher, reading mode, width + chrome-lock all inside. */
		.settings {
			top: auto;
			bottom: 0;
			left: 0;
			right: 0;
			width: auto;
			max-height: 82vh;
			overflow-y: auto;
			border-radius: 18px 18px 0 0;
			padding: 22px 20px calc(24px + env(safe-area-inset-bottom));
			box-shadow: 0 -20px 60px rgba(0, 0, 0, 0.6);
		}
		/* Larger, one-handed tap targets for the reading-mode + page-width controls. */
		.mode {
			height: 46px;
		}
		.tr-opt {
			padding: 10px 12px;
		}
	}
</style>
