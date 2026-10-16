<script lang="ts">
	import { modalFocus } from '$lib/modal-focus';
	import { allowPageZoom, setStatusBarHidden } from '$lib/native';
	import { goto, beforeNavigate, preloadData } from '$app/navigation';
	import { navigating } from '$app/state';
	import { untrack, onDestroy, tick } from 'svelte';
	import {
		saveProgress,
		recordView,
		getChapterSourceLinks,
		readerViewForCurrentScope,
		readerViewIsCurrent,
		releaseStaleReaderView,
		type ReaderView,
	} from '$lib/data/source';
	import { refreshViewerLoads } from '$lib/viewer-cache.svelte';
	import { externalChapterHost, externalChapterHref } from '$lib/data/chapter-label';
	import { chapterAttribution } from '$lib/data/chapter-versions';
	import { stripProgress } from '$lib/data/reader-format';
	import { setPreferredTranslator } from '$lib/data/translator-pref.svelte';
	import AiBadge from '$lib/components/AiBadge.svelte';
	import { aiSpoken } from '$lib/scanlator-ai.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import {
		BARS,
		DIRS,
		FITS,
		HEADERS,
		MODES,
		WIDTH_MAX,
		WIDTH_MIN,
		WIDTH_STEP,
		loadReaderPrefs,
		saveReaderPrefs,
		type Bar,
		type Dir,
		type Fit,
		type HeaderMode,
		type Mode,
		type Opt,
	} from '$lib/reader-prefs';
	import CommentThread from '$lib/components/CommentThread.svelte';
	import { ChapterMediaRecovery } from '@komika/api';
	import { currentMediaRouter, freshRoutedPages } from '$lib/media';

	let { data: loadedData } = $props();
	// Kit can deliver a completed preload without calling load again. Guard the
	// complete view, including metadata/progress, before it reaches any UI or write.
	const data = $derived(readerViewForCurrentScope(loadedData));
	$effect(() => {
		if (readerViewIsCurrent(loadedData)) return;
		releaseStaleReaderView(loadedData);
		void refreshViewerLoads();
	});

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
	// "Read on source": the open copy on its original site ("This chapter on mangadex.org"),
	// else — when the chapter's own URL is unknown, as for every Suwayomi copy — the reading
	// source's series page. The data layer already validated both; re-checked here because
	// the string becomes an `href` and the check is the whole XSS defence. Null when neither
	// is known: the menu then explains instead of offering a dead link.
	//
	// A Suwayomi copy (any extension that is not MangaDex) has no link in `data`: only the
	// engine knows it. `engineLinks` holds the answer for ONE chapter id, looked up when the
	// menu that shows the link is on screen — see the effect below `drawerShown`.
	let engineLinks = $state<{
		chapterId: string;
		chapter: string | null;
		series: string | null;
	} | null>(null);
	const readOnSource = $derived.by(() => {
		const engine = engineLinks?.chapterId === data.chapterId ? engineLinks : null;
		const chapterHref = externalChapterHref(data.sourceUrl) ?? externalChapterHref(engine?.chapter);
		const href =
			chapterHref ??
			externalChapterHref(engine?.series) ??
			externalChapterHref(data.seriesSourceUrl);
		return href ? { href, host: externalChapterHost(href), chapter: !!chapterHref } : null;
	});

	// Translators (sources) for this work. `selectedTranslatorKey` is the series' DEFAULT
	// source: chapters open from it, and a chapter it lacks opens from another source for
	// that chapter only. The source of the OPEN chapter is `readingSrc`.
	const translators = $derived(data.translators ?? []);
	const activeTranslator = $derived(
		translators.find((t) => t.suwayomiMangaId === data.readingSrc) ?? null,
	);

	// Copies of the OPEN chapter number — one per (source, scanlation group) upload, so
	// a chapter two groups translated can be swapped without leaving the reader. Unlike
	// the default-source switch below this is a one-off pick: it opens that exact copy
	// and does not change the series' default source.
	const versions = $derived(data.versions ?? []);
	const currentVersion = $derived(versions.find((v) => v.current) ?? null);
	// The open copy's group: the view's own field first, else the current version row.
	const openGroup = $derived(data.group ?? currentVersion?.group ?? null);
	// Its source id, the same way. A source can carry an AI tag of its own.
	const openSourceId = $derived(data.sourceId ?? currentVersion?.sourceId ?? null);
	// "No Group" is a real MangaDex value but says nothing in a one-line header.
	const headerGroup = $derived(openGroup && openGroup !== 'No Group' ? openGroup : null);
	function versionHref(v: { id: string; src: string | null }): string {
		const src = v.src ? `&src=${encodeURIComponent(v.src)}` : '';
		return `/read/${data.seriesId}?ch=${encodeURIComponent(v.id)}${src}`;
	}
	async function openVersion(v: { id: string; src: string | null; current: boolean }) {
		if (v.current) return;
		if (!docked) closeMenu();
		await goto(versionHref(v));
		resetWindowScroll();
	}

	// Picking a default source persists it and stays on the open chapter NUMBER: the new
	// default's copy of it when it has one (chapter ids differ per source), else the copy
	// that is open. `invalidateAll` re-runs the load even when the URL does not change, so
	// the chapter list and prev/next follow the new default.
	async function switchTranslator(t: { key: string; suwayomiMangaId: string | null }) {
		if (!data.workId || t.key === data.selectedTranslatorKey) return;
		setPreferredTranslator(data.workId, t.key);
		// Progress for the chapter being left is saved by `beforeNavigate`.
		closeMenu();
		const copy = versions.find((v) => v.src === t.suwayomiMangaId);
		const stay = data.chapterId ? { id: data.chapterId, src: data.readingSrc } : null;
		const open = copy ?? stay;
		await goto(open ? versionHref(open) : `/read/${data.workId}`, { invalidateAll: true });
		if (copy && !copy.current) resetWindowScroll();
	}

	// ---- reader preferences --------------------------------------------------
	// Every display choice lives in one persisted object. Each control in the menu is a
	// cycle button (tap to step to the next value), so the option lists below are also
	// the cycle order and the source of each button's label + icon.
	// Keep manga preferences intact when a webtoon enforces continuous reading.
	const longStrip = $derived(data.longStrip === true);
	let preferredMode = $state<Mode>('strip');
	const mode = $derived(longStrip ? 'strip' : preferredMode);
	let fit = $state<Fit>('both');
	let dir = $state<Dir>('ltr');
	let headerMode = $state<HeaderMode>('auto');
	let preferredBar = $state<Bar>('segments');
	const bar = $derived(longStrip ? 'continuous' : preferredBar);
	let width = $state(720);
	let gaps = $state(false);
	let pinned = $state(false);

	function optOf<T>(list: Opt<T>[], v: T): Opt<T> {
		return list.find((o) => o.value === v) ?? list[0];
	}
	function nextOf<T>(list: Opt<T>[], v: T): T {
		const i = list.findIndex((o) => o.value === v);
		return list[(i + 1) % list.length].value;
	}

	function persist() {
		saveReaderPrefs({
			mode: preferredMode,
			fit,
			dir,
			header: headerMode,
			bar: preferredBar,
			width,
			gaps,
			pinned,
		});
	}

	let pageIdx = $state(0);
	const hasPages = $derived(total > 0);
	let chromeVisible = $state(true);
	let menuOpen = $state(false);
	let scrolledPage = $state(1);
	let scrollFraction = $state(0);
	let maxScrolledPage = $state(1);
	const warmedUrls = new Set<string>();

	// The menu can dock beside the pages on wide screens (pin). On narrow screens a pin
	// is ignored and the menu is always an overlay sheet.
	let wide = $state(false);
	$effect(() => {
		const mq = window.matchMedia('(min-width: 1024px)');
		const sync = () => (wide = mq.matches);
		sync();
		mq.addEventListener('change', sync);
		return () => mq.removeEventListener('change', sync);
	});
	const docked = $derived(pinned && wide);
	const drawerShown = $derived(menuOpen || docked);

	// Ask the engine where the open chapter lives on its source's website, once per
	// chapter, and only while the menu is showing — that is the only place the link is
	// drawn. Skipped when `data` already has the chapter's own link (MangaDex, off-site).
	// A late answer for a chapter the reader has left is dropped by the cleanup, and
	// `readOnSource` ignores an answer whose id is not the open one.
	$effect(() => {
		const id = data.chapterId;
		if (!drawerShown || !id || data.sourceUrl) return;
		if (untrack(() => engineLinks?.chapterId) === id) return;
		let cancelled = false;
		void getChapterSourceLinks(id).then((links) => {
			if (!cancelled && links) engineLinks = { chapterId: id, ...links };
		});
		return () => {
			cancelled = true;
		};
	});

	function openMenu() {
		menuOpen = true;
	}
	function closeMenu() {
		menuOpen = false;
	}
	function togglePin() {
		pinned = !pinned;
		if (!pinned) menuOpen = false;
		persist();
	}

	const chKey = $derived(`${data.seriesId}:${data.chNum}`);

	// True while a navigation to a DIFFERENT chapter of this reader is in flight.
	// SvelteKit blocks a same-route navigation on its load, so without this the old
	// chapter's pages stay fully painted and the reader looks frozen. Scoped to
	// `/read/` targets so leaving for the series page or a comment link doesn't fade.
	//
	// Keyed on the navigation's own `complete` promise, NOT on `navigating.to` alone.
	// SvelteKit does not clear `navigating` when a navigation is ABORTED (another
	// navigation or an `invalidate()` takes its token — kit 2.70 `navigate()` returns
	// without resetting it), so an overlay driven by `navigating.to` alone dimmed the old
	// chapter, blocked its pointer events, and never lifted: the production "page turns
	// grey but the change doesn't happen". `complete` settles on every outcome (fulfilled,
	// aborted, cancelled), and a deadline lifts the overlay even if it somehow never does,
	// so the reader can always try again.
	const NAV_STUCK_MS = 15_000;
	const pendingNav = $derived(
		navigating.to?.url.pathname.startsWith('/read/') ? navigating.complete : null,
	);
	let settledNav = $state<Promise<void> | null>(null);
	$effect(() => {
		const nav = pendingNav;
		if (!nav) return;
		const settle = () => (settledNav = nav);
		nav.then(settle, settle);
		const timer = setTimeout(settle, NAV_STUCK_MS);
		return () => clearTimeout(timer);
	});
	const chapterLoading = $derived(!!pendingNav && settledNav !== pendingNav);

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

	// Jump the window to y=0 and assert it again after layout. A single
	// fire-and-forget `scrollTo` loses the race against strip cells collapsing
	// onto DEFAULT_RATIO then growing as images load, and against the browser's
	// overflow-anchor keeping the end-of-chapter block in view when the strip
	// above it is replaced.
	let scrollResetting = false;
	function resetWindowScroll() {
		scrollResetting = true;
		lastY = 0;
		window.scrollTo({ top: 0, behavior: 'auto' });
		void tick().then(() =>
			requestAnimationFrame(() => {
				window.scrollTo({ top: 0, behavior: 'auto' });
				requestAnimationFrame(() => {
					window.scrollTo({ top: 0, behavior: 'auto' });
					scrollResetting = false;
				});
			}),
		);
	}

	// Reset page + scroll position whenever the loaded chapter changes.
	$effect(() => {
		data.chapterId;
		untrack(() => {
			pageIdx = 0;
			scrolledPage = 1;
			scrollFraction = 0;
			maxScrolledPage = 1;
			readMarked = data.chapterRead ?? false;
			pageRatio = {};
			pageError = {};
			warmedUrls.clear();
			settledPages.clear();
			queueUntil = STARTER_PAGES;
			resetWindowScroll();
		});
	});

	// The native image provider hands back `blob:` object URLs for pages; those pin
	// memory until explicitly revoked, so a long reading session (chapter after
	// chapter) leaks. Track the current chapter's page URLs and revoke the blob
	// ones when the chapter changes (this effect's cleanup runs before it re-runs
	// with the next chapter's pages) and on teardown. Web/proxy pages are plain
	// http(s) strings — the `blob:` guard leaves those untouched.
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

	const isStrip = $derived(mode === 'strip');
	const chromeHidden = $derived(
		headerMode === 'hidden' || (headerMode === 'auto' && !chromeVisible),
	);

	// ---- page geometry ---------------------------------------------------------
	// Real width/height ratios, learned as pages load (or as paged-mode prefetches
	// decode). Strip cells reserve their height from it; double-page mode uses it to
	// show a wide spread on its own instead of squeezing it next to another page.
	let pageRatio = $state<Record<number, number>>({});
	// A neutral manga default (~2:3) used only until a page's real ratio is known.
	const DEFAULT_RATIO = 2 / 3;
	function isWide(i: number): boolean {
		return (pageRatio[i] ?? 0) > 1;
	}
	// How many pages the double-page view shows starting at `i`: two, unless this is
	// the last page or either page is a wide spread (spreads always stand alone).
	function spreadSize(i: number): number {
		if (mode !== 'double') return 1;
		if (i >= total - 1 || isWide(i) || isWide(i + 1)) return 1;
		return 2;
	}
	const pagedIdx = $derived(Math.min(pageIdx, Math.max(0, total - 1)));
	const visiblePages = $derived(pages.slice(pagedIdx, pagedIdx + spreadSize(pagedIdx)));

	// The current 0-indexed page, however the chapter is being read.
	function currentPageIndex(): number {
		return isStrip ? Math.max(0, scrolledPage - 1) : pagedIdx;
	}
	const currentIdx = $derived(isStrip ? Math.max(0, scrolledPage - 1) : pagedIdx);
	// Last page index currently on screen (a double spread covers two).
	const currentEnd = $derived(isStrip ? currentIdx : pagedIdx + visiblePages.length - 1);
	const pageReadout = $derived(
		currentEnd > currentIdx
			? `${currentIdx + 1}–${currentEnd + 1} / ${total}`
			: `${currentIdx + 1} / ${total}`,
	);
	const readPct = $derived(
		longStrip ? scrollFraction * 100 : total ? ((currentEnd + 1) / total) * 100 : 0,
	);

	// Mark the chapter read once the reader reaches the end (idempotent per chapter).
	// Strip mode: the bottom of the final page is on screen — NOT a document %, since
	// the end-of-chapter block and the comments sit below the pages.
	function maybeMarkRead(): void {
		if (readMarked || !data.chapterId || !hasPages) return;
		const atEnd = isStrip ? atStripEnd() : currentEnd >= total - 1;
		if (atEnd) {
			readMarked = true;
			void saveProgress(data.chapterId, Math.max(0, total - 1), true);
		}
	}

	// Persist the current reading position. Reading otherwise only saves on in-app
	// chapter changes and at the end, so a browser-back or header-link exit would drop
	// a mid-chapter position. Save on navigation away, on tab-hide, and on unmount.
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

	// The native app locks zoom everywhere else; chapter pages may be zoomed in on.
	$effect(() => allowPageZoom());

	// The native app hides the system status bar (clock, battery) together with the
	// reader's own top bar, and shows it again with the bar and on the way out.
	$effect(() => setStatusBarHidden(chromeHidden));
	$effect(() => () => setStatusBarHidden(false));

	function chapterHref(id: string) {
		// Chapter ids are per-source, and each row names its own (S2): a number the default
		// source lacks opens from another one.
		const row = data.chapters.find((c) => c.id === id);
		return versionHref({ id, src: row ? (row.src ?? null) : data.readingSrc });
	}

	async function openChapter(id?: string) {
		if (!docked) menuOpen = false;
		if (!id) return;
		const href = chapterHref(id);
		// A second tap on the same control while its chapter is still loading is a no-op,
		// not a restarted navigation.
		const to = navigating.to?.url;
		if (to && `${to.pathname}${to.search}` === href) return;
		// Progress for the chapter being left is saved by `beforeNavigate` — NOT here as
		// well: the duplicate write only doubled the requests racing the navigation.
		await goto(href);
		// Immediate jump so a same-chapter re-open (chapterId unchanged, the
		// reset effect does not re-run) still lands on the first image. The
		// chapter-change effect re-asserts after layout when the id DID change.
		resetWindowScroll();
	}

	// ---- next-chapter prefetch ---------------------------------------------
	// Opening a chapter is a client-side waterfall (chapter list → page list → image
	// bytes); with nothing warmed, all of it starts on the click, which is the pause
	// readers feel between chapters. Once the reader is far enough in to probably
	// continue, run the very load the navigation would run — SvelteKit reuses a
	// preloaded result for the matching `goto`, so the click resolves from memory —
	// then warm the first few page images off that result.
	const PREFETCH_AT_PCT = 40;
	const PREFETCH_IMAGES = 3;
	let prefetchedFor = '';
	$effect(() => {
		const id = data.nextChapterId;
		const deepEnough = readPct >= PREFETCH_AT_PCT;
		if (!id || !deepEnough || prefetchedFor === id) return;
		// Set before awaiting so a burst of scroll frames can't launch it twice.
		prefetchedFor = id;
		warmChapter(id);
	});

	/**
	 * Run the load a chapter navigation would run, ahead of the click, and warm its first
	 * page images. Called by the 40% rule above and on INTENT — pointer over, focus or
	 * touch on any previous/next control — so even a reader who jumps straight to "next"
	 * from the top of a chapter usually clicks into a load that is already under way.
	 *
	 * SvelteKit keeps ONE preloaded result, and reuses it only for a navigation to the
	 * identical URL; a hover on "previous" therefore replaces a warmed "next", which is
	 * why intent re-warms instead of trusting an earlier preload. Same-URL calls share the
	 * in-flight load, so repeated hovers cost nothing. Off-site chapters
	 * (MangaPlus/Comikey/…) load no pages by design, so there is nothing to warm.
	 */
	function warmChapter(id: string | undefined): void {
		if (!id || id === data.chapterId) return;
		if (data.chapters.find((c) => c.id === id)?.externalUrl) return;
		void preloadData(chapterHref(id))
			.then((res) => {
				// `redirect` results carry no data; only a `loaded` one has pages.
				if (res?.type !== 'loaded') return;
				const view = res.data as ReaderView;
				if (!readerViewIsCurrent(view)) return releaseStaleReaderView(view);
				const next = view.pages;
				for (const pg of next.slice(0, PREFETCH_IMAGES)) {
					if (!pg?.url || warmedUrls.has(pg.url)) continue;
					warmedUrls.add(pg.url);
					const warm = new Image();
					warm.decoding = 'async';
					warm.src = pg.url;
				}
			})
			// Best-effort only: a failed prefetch must never surface as an unhandled
			// rejection, and the real navigation will fetch (and report) for itself.
			.catch(() => {});
	}

	/** Intent handlers for a control that opens chapter `id` (pointer, keyboard, touch). */
	function warmOn(id: string | undefined) {
		const warm = () => warmChapter(id);
		return { onpointerenter: warm, onfocus: warm, ontouchstart: warm };
	}

	// ---- strip position --------------------------------------------------------
	let stripEl = $state<HTMLElement | null>(null);
	function updateStripProgress() {
		if (!longStrip || !stripEl) return;
		const rect = stripEl.getBoundingClientRect();
		scrollFraction = stripProgress(rect.top, rect.height, window.innerHeight);
	}
	$effect(() => {
		const el = stripEl;
		if (!longStrip || !el) return;
		// Images refine their reserved height as they load; progress follows that geometry.
		const observer = new ResizeObserver(updateStripProgress);
		observer.observe(el);
		window.addEventListener('resize', updateStripProgress);
		untrack(updateStripProgress);
		return () => {
			observer.disconnect();
			window.removeEventListener('resize', updateStripProgress);
		};
	});
	// The reading line: a page is "current" once its top passes 40% of the viewport.
	function stripPageAtLine(): number {
		const cells = stripEl?.children;
		if (!cells || cells.length === 0) return 1;
		const line = window.innerHeight * 0.4;
		let lo = 0;
		let hi = cells.length - 1;
		let found = 0;
		while (lo <= hi) {
			const mid = (lo + hi) >> 1;
			if ((cells[mid] as HTMLElement).getBoundingClientRect().top <= line) {
				found = mid;
				lo = mid + 1;
			} else hi = mid - 1;
		}
		return found + 1;
	}
	function atDocTop(): boolean {
		return window.scrollY <= 2;
	}
	function atStripEnd(): boolean {
		return !!stripEl && stripEl.getBoundingClientRect().bottom <= window.innerHeight + 2;
	}

	$effect(() => {
		if (!restored) {
			restored = true;
			const saved = loadReaderPrefs();
			preferredMode = saved.mode;
			fit = saved.fit;
			dir = saved.dir;
			headerMode = saved.header;
			preferredBar = saved.bar;
			width = saved.width;
			gaps = saved.gaps;
			pinned = saved.pinned;
		}
		// Coalesce scroll work into one rAF per frame — reading layout + writing
		// reactive state on every scroll event is a jank source on fast scrolls.
		let ticking = false;
		const update = () => {
			ticking = false;
			if (scrollResetting) {
				lastY = 0;
				return;
			}
			const y = window.scrollY;
			if (mode === 'strip') {
				scrolledPage = Math.min(total, stripPageAtLine());
				if (scrolledPage > maxScrolledPage) maxScrolledPage = scrolledPage;
				updateStripProgress();
			}
			maybeMarkRead();
			const dy = y - lastY;
			if (y < 80) chromeVisible = true;
			else if (dy > 6) chromeVisible = false;
			else if (dy < -6) chromeVisible = true;
			lastY = y;
		};
		const onScroll = () => {
			if (ticking) return;
			ticking = true;
			requestAnimationFrame(update);
		};
		window.addEventListener('scroll', onScroll, { passive: true });
		// The priming call reads reactive state; untrack keeps those reads from
		// becoming deps that would tear down and re-add the listener.
		untrack(() => update());
		return () => window.removeEventListener('scroll', onScroll);
	});

	// ---- lookahead prefetch & long-strip rendering -------------------------
	// Every page stays in the DOM (no IntersectionObserver mount/unmount — that
	// thrashed the DOM and caused scroll bumps). `loading` is computed per image and
	// the browser starts fetching the moment it flips from 'lazy' to 'eager':
	//
	// 1. STARTERS — the first STARTER_PAGES pages fetch at once with
	//    `fetchpriority="high"`.
	// 2. BACKGROUND QUEUE — every remaining page auto-loads in order, only
	//    QUEUE_CONCURRENCY at a time and at low priority, advancing as pages settle,
	//    so it never competes with the starters.
	// 3. SCROLL LOOKAHEAD — EAGER_AHEAD pages ahead of the furthest page reached
	//    stay in flight, so a reader who outruns the queue (or jumps from the page
	//    bar) never waits on it. The high-water mark never moves back.
	const STARTER_PAGES = 3;
	const QUEUE_CONCURRENCY = 2;
	const EAGER_AHEAD = 6;
	let queueUntil = $state(STARTER_PAGES);
	const settledPages = new Set<number>();
	const eagerUntil = $derived(Math.max(queueUntil, maxScrolledPage + EAGER_AHEAD));

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

	// impeccable-disable-next-line broken-image: literal image element in explanatory prose.
	// Per-page load failures — a failed <img> otherwise leaves a permanent
	// full-height gap with the browser's broken-image glyph and no way to retry.
	let pageError = $state<Record<number, boolean>>({});

	function learnRatio(index: number, w: number, h: number): void {
		if (w > 0 && h > 0 && pageRatio[index] !== w / h) {
			pageRatio = { ...pageRatio, [index]: w / h };
		}
	}

	// A page that finishes loading ABOVE the viewport changes height from the 2:3
	// reservation to its real one. Main opts out of browser scroll anchoring (it
	// fought the chapter-swap reset), so compensate by hand: shift the scroll by the
	// growth so the page being read stays put. Matters most after a page-bar jump,
	// where the background queue keeps finishing pages behind the reader.
	function onImgLoad(e: Event, index: number): void {
		const img = e.currentTarget as HTMLImageElement;
		const cell = img.parentElement;
		const rect = isStrip && !scrollResetting ? cell?.getBoundingClientRect() : undefined;
		learnRatio(index, img.naturalWidth, img.naturalHeight);
		if (rect && rect.bottom <= 0 && img.naturalWidth > 0) {
			const grown = (rect.width * img.naturalHeight) / img.naturalWidth - rect.height;
			if (Math.abs(grown) > 1) void tick().then(() => window.scrollBy(0, grown));
		}
		settlePage(index);
	}
	// Exact media objects (typed media routes): a failed regional object is retried once on
	// the owner; when the owner fails too, the chapter's pages are fetched again ONCE with
	// no HTTP cache and pages whose URL changed load again. Then the retry button.
	let pageUrl = $state<Record<number, string>>({});
	const recovery = $derived.by(() => {
		const router = currentMediaRouter();
		const chapterId = data.chapterId;
		if (!router || !chapterId || !pages.some((p) => p.media)) return null;
		return new ChapterMediaRecovery(
			router,
			pages.map((p) => ({ url: p.url, media: p.media ?? null })),
			() => freshRoutedPages(chapterId),
		);
	});
	$effect(() => {
		recovery;
		untrack(() => {
			pageUrl = {};
		});
	});

	function onImgError(index: number): void {
		const rec = recovery;
		const failed = pageUrl[index] ?? pages[index]?.url;
		if (!rec || !failed) return failPage(index);
		void rec.recover(index, failed).then((changes) => {
			if (rec !== recovery) return;
			let urls = pageUrl;
			let errors = pageError;
			// Every changed page takes its new URL, mounted or not: a loaded image keeps
			// showing its bytes until the new ones arrive.
			for (const [i, next] of changes) {
				urls = { ...urls, [i]: next.url };
				if (errors[i]) {
					errors = { ...errors };
					delete errors[i];
				}
			}
			pageUrl = urls;
			pageError = errors;
			if (!changes.has(index)) failPage(index);
		});
	}
	function failPage(index: number): void {
		pageError = { ...pageError, [index]: true };
		settlePage(index);
	}
	// impeccable-disable-next-line broken-image: literal image element in explanatory prose.
	// Clearing the flag remounts a fresh <img> for that page, re-fetching the URL.
	function retryImage(index: number): void {
		const next = { ...pageError };
		delete next[index];
		pageError = next;
	}

	// ---- paged mode prefetch -----------------------------------------------
	// Paged modes render only the visible page(s). Warm the next few into the HTTP
	// cache with detached Image() objects so page turns feel instant, and learn their
	// ratios on the way so double-page mode knows about spreads before it reaches them.
	const PAGED_PREFETCH = 4;
	$effect(() => {
		if (mode === 'strip') return;
		const from = pagedIdx + visiblePages.length;
		for (let i = from; i < Math.min(total, from + PAGED_PREFETCH); i++) {
			const pg = pages[i];
			if (!pg?.url || warmedUrls.has(pg.url)) continue;
			warmedUrls.add(pg.url);
			const warm = new Image();
			warm.decoding = 'async';
			warm.onload = () => learnRatio(i, warm.naturalWidth, warm.naturalHeight);
			warm.src = pg.url;
		}
	});

	function chapterLabel(c: { n: number; title: string }): string {
		const t = c.title?.trim();
		return t && t !== `Chapter ${c.n}` ? `Ch. ${c.n} · ${t}` : `Chapter ${c.n}`;
	}

	// ---- navigation ------------------------------------------------------------
	// Page turns hop chapters at the edges: forward on the LAST page opens the next
	// chapter when one exists, backward on the FIRST page opens the previous one.
	function nextPage() {
		const step = spreadSize(pagedIdx);
		if (pagedIdx + step <= total - 1) {
			pageIdx = pagedIdx + step;
			resetWindowScroll();
			void tick().then(maybeMarkRead);
		} else if (data.nextChapterId) {
			void openChapter(data.nextChapterId);
		}
	}
	function prevPage() {
		if (pagedIdx > 0) {
			// Step back over a full pair unless either page is a lone spread.
			const back =
				mode === 'double' && pagedIdx >= 2 && !isWide(pagedIdx - 1) && !isWide(pagedIdx - 2)
					? 2
					: 1;
			pageIdx = pagedIdx - back;
			resetWindowScroll();
		} else if (data.prevChapterId) {
			void openChapter(data.prevChapterId);
		}
	}
	// Visual left/right (tap zones + arrow keys) map onto prev/next by direction.
	function goLeft() {
		if (dir === 'rtl') nextPage();
		else prevPage();
	}
	function goRight() {
		if (dir === 'rtl') prevPage();
		else nextPage();
	}

	function jumpTo(i: number) {
		if (!hasPages) return;
		const target = Math.max(0, Math.min(total - 1, i));
		if (!isStrip) {
			pageIdx = target;
			resetWindowScroll();
			void tick().then(maybeMarkRead);
			return;
		}
		const cell = stripEl?.children[target] as HTMLElement | undefined;
		if (!cell) return;
		const offset = chromeHidden ? 0 : 56;
		const top = target === 0 ? 0 : cell.getBoundingClientRect().top + window.scrollY - offset;
		window.scrollTo({ top: Math.max(0, top), behavior: 'auto' });
		scrolledPage = target + 1;
		if (scrolledPage > maxScrolledPage) maxScrolledPage = scrolledPage;
	}

	function setMode(m: Mode) {
		if (longStrip) return;
		const at = currentPageIndex();
		preferredMode = m;
		persist();
		// Keep the reader on the same page across a mode switch.
		void tick().then(() => {
			if (m === 'strip') {
				jumpTo(at);
			} else {
				pageIdx = at;
				resetWindowScroll();
			}
		});
	}

	// Paged mode: a sideways swipe turns the page, like a tap on a screen edge. The page
	// follows the finger, so a swipe to the left shows what is on the right. Not while
	// the page is zoomed in or wider than the screen (the swipe moves the page then), and
	// not with two fingers (that is a pinch).
	let swipe: { x: number; y: number; at: number } | null = null;
	function onSwipeStart(e: TouchEvent) {
		swipe = null;
		if (isStrip || e.touches.length !== 1) return;
		const paged = (e.target as Element).closest?.('main')?.querySelector('.paged');
		if (!paged || !(e.target as Element).closest('.paged, .zone')) return;
		if ((window.visualViewport?.scale ?? 1) > 1.01) return;
		if (paged.scrollWidth > paged.clientWidth + 1) return;
		const t = e.touches[0];
		swipe = { x: t.clientX, y: t.clientY, at: e.timeStamp };
	}
	function onSwipeEnd(e: TouchEvent) {
		const start = swipe;
		swipe = null;
		if (!start || e.touches.length > 0) return;
		const t = e.changedTouches[0];
		const dx = t.clientX - start.x;
		const dy = t.clientY - start.y;
		if (Math.abs(dx) < 56 || Math.abs(dx) < Math.abs(dy) * 1.5) return;
		if (e.timeStamp - start.at > 700) return;
		if (dx < 0) goRight();
		else goLeft();
	}

	// Tap on the reading surface (not a page-turn zone).
	function stageClick() {
		if (headerMode === 'auto') chromeVisible = !chromeVisible;
		// With no header to reveal, the tap is the way to the menu. In an always-shown
		// header, strip taps stay inert so reading isn't interrupted.
		else if (headerMode === 'hidden' || !isStrip) openMenu();
	}

	// ---- page bar ---------------------------------------------------------------
	function jumpToProgress(fraction: number) {
		if (!stripEl) return;
		const rect = stripEl.getBoundingClientRect();
		const target = Math.max(0, Math.min(1, fraction));
		window.scrollTo({
			top: Math.max(
				0,
				rect.top + window.scrollY + target * Math.max(0, rect.height - window.innerHeight),
			),
			behavior: 'auto',
		});
		scrollFraction = target;
	}
	let barHover = $state<{ i: number; x: number } | null>(null);
	function barIndexAt(e: PointerEvent | MouseEvent): number {
		const el = e.currentTarget as HTMLElement;
		const r = el.getBoundingClientRect();
		let f = (e.clientX - r.left) / r.width;
		if (dir === 'rtl' && !isStrip) f = 1 - f;
		return Math.max(0, Math.min(total - 1, Math.floor(f * total)));
	}
	function onBarMove(e: PointerEvent) {
		const el = e.currentTarget as HTMLElement;
		barHover = { i: barIndexAt(e), x: e.clientX - el.getBoundingClientRect().left };
	}
	function onBarKey(e: KeyboardEvent) {
		const fwd = dir === 'rtl' && !isStrip ? 'ArrowLeft' : 'ArrowRight';
		const back = dir === 'rtl' && !isStrip ? 'ArrowRight' : 'ArrowLeft';
		if (e.key === fwd) jumpTo(currentEnd + 1);
		else if (e.key === back) jumpTo(currentIdx - 1);
		else if (e.key === 'Home') jumpTo(0);
		else if (e.key === 'End') jumpTo(total - 1);
		else return;
		e.preventDefault();
		e.stopPropagation();
	}

	// Keyboard: M toggles the menu; Escape closes it. Paged modes turn pages with
	// arrows / PageUp / PageDown / space (arrows follow the reading direction) and hop
	// chapters at the edges. Long strip keeps native scrolling, except forward keys at
	// the end of the strip open the next chapter and backward keys at the very top
	// open the previous one. Ignored while typing.
	function onKeydown(e: KeyboardEvent) {
		const el = e.target as HTMLElement | null;
		if (
			el &&
			(el.tagName === 'TEXTAREA' ||
				el.tagName === 'INPUT' ||
				el.tagName === 'SELECT' ||
				el.isContentEditable)
		)
			return;
		if (e.metaKey || e.ctrlKey || e.altKey) return;
		if (e.key === 'Escape') {
			if (menuOpen) {
				e.preventDefault();
				menuOpen = false;
			}
			return;
		}
		if (e.key === 'm' || e.key === 'M') {
			e.preventDefault();
			if (docked) togglePin();
			else menuOpen = !menuOpen;
			return;
		}
		if (menuOpen && !docked) return;
		if (!isStrip) {
			if (e.key === 'ArrowRight') {
				e.preventDefault();
				goRight();
			} else if (e.key === 'ArrowLeft') {
				e.preventDefault();
				goLeft();
			} else if (e.key === 'PageDown' || e.key === ' ') {
				e.preventDefault();
				nextPage();
			} else if (e.key === 'PageUp') {
				e.preventDefault();
				prevPage();
			}
			return;
		}
		const forward = e.key === 'ArrowRight' || e.key === 'PageDown';
		const backward = e.key === 'ArrowLeft' || e.key === 'PageUp';
		if (forward && atStripEnd() && data.nextChapterId) {
			e.preventDefault();
			void openChapter(data.nextChapterId);
		} else if (backward && atDocTop() && data.prevChapterId) {
			e.preventDefault();
			void openChapter(data.prevChapterId);
		} else if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
			// Mid-strip, left/right step a whole page (up/down still scroll freely).
			e.preventDefault();
			jumpTo(e.key === 'ArrowRight' ? currentIdx + 1 : currentIdx - 1);
		}
	}

	const reportHref = $derived(
		`/support/report?kind=OTHER&series=${encodeURIComponent(
			`${seriesTitle || data.seriesId} — Chapter ${chNum}`,
		)}`,
	);
</script>

<svelte:head>
	<title>{seriesTitle ? `Ch. ${chNum} · ${seriesTitle} · komiq` : `Ch. ${chNum} · komiq`}</title>
</svelte:head>

<svelte:window onkeydown={onKeydown} />

{#snippet cycle<T>(list: Opt<T>[], value: T, onpick: (v: T) => void, caption: string)}
	{@const o = optOf(list, value)}
	<button
		class="cycle"
		aria-label={`${caption}: ${o.label}. Change`}
		onclick={() => onpick(nextOf(list, value))}
	>
		<Icon name={o.icon} size={16} />
		<span class="cycle-text">
			<span class="cycle-cap">{caption}</span>
			<span class="cycle-val">{o.label}</span>
		</span>
		<Icon name="chevron-right" size={14} />
	</button>
{/snippet}

{#snippet srcLogo(iconUrl: string | null, name: string)}
	{#if iconUrl}
		<img
			class="tr-logo"
			src={iconUrl}
			alt=""
			loading="lazy"
			decoding="async"
			referrerpolicy="no-referrer"
		/>
	{:else}
		<span class="tr-logo tr-initial">{name.charAt(0).toUpperCase()}</span>
	{/if}
{/snippet}

{#snippet menu()}
	<div class="dr-head">
		<button
			class="icon-btn k-hit"
			aria-label="Close menu"
			onclick={() => (docked ? togglePin() : closeMenu())}><Icon name="x" size={18} /></button
		>
		<span class="dr-title">Reader</span>
		{#if wide}
			<button
				class="icon-btn"
				class:on={pinned}
				aria-pressed={pinned}
				aria-label={pinned ? 'Unpin menu' : 'Pin menu beside the pages'}
				title={pinned ? 'Unpin' : 'Pin beside the pages'}
				onclick={togglePin}><Icon name="pin" size={17} /></button
			>
		{/if}
	</div>

	<div class="dr-body">
		<a class="dr-series k-hit" href={seriesHref}>
			<Icon name="book-open" size={16} />
			<span>{seriesTitle || 'Back to series'}</span>
		</a>
		{#if data.chapterId}
			<div class="dr-chapter">
				<Icon name="file" size={16} /><span>{chapterLabel({ n: chNum, title: chTitle })}</span>
			</div>
			{#if openGroup}
				<div class="dr-uploader">
					<Icon name="users" size={15} /><span
						><span class="dr-up-cap">Uploaded by</span> {openGroup}</span
					>
				</div>
			{/if}
			<!-- "Read on source": the open copy on its original site, for the source being read.
			     New tab, no opener and no referrer: the destination is a third-party site and
			     gets neither a handle on this window nor the reader URL. Always rendered, so a
			     copy with no known link says so instead of the action silently vanishing. -->
			{#if readOnSource}
				<a
					class="dr-source"
					href={readOnSource.href}
					target="_blank"
					rel="noopener noreferrer"
					title={readOnSource.href}
				>
					<Icon name="globe" size={16} />
					<span class="dr-source-text">
						<span class="dr-source-cap">Read on source</span>
						<span class="dr-source-sub"
							>{readOnSource.chapter ? 'This chapter on' : 'Series page on'}
							{readOnSource.host}</span
						>
					</span>
					<Icon name="arrow-right" size={14} />
				</a>
			{:else}
				<div class="dr-source off" role="note">
					<Icon name="globe" size={16} />
					<span class="dr-source-text">
						<span class="dr-source-cap">Read on source</span>
						<span class="dr-source-sub">No link to this copy on its source site is known</span>
					</span>
				</div>
			{/if}
		{/if}

		{#if hasPages}
			<div class="stepper">
				<button
					class="step k-hit"
					aria-label="Previous page"
					disabled={currentIdx === 0 && !data.prevChapterId}
					onclick={() => (isStrip ? jumpTo(currentIdx - 1) : prevPage())}
					><Icon name="chevron-left" size={16} /></button
				>
				<label class="sel">
					<span class="sel-cap">Page</span>
					<select
						aria-label="Jump to page"
						value={currentIdx}
						onchange={(e) => jumpTo(Number((e.currentTarget as HTMLSelectElement).value))}
					>
						{#each pages as p (p.index)}
							<option value={p.index}>{p.index + 1} of {total}</option>
						{/each}
					</select>
					<Icon name="chevron-down" size={14} />
				</label>
				<button
					class="step k-hit"
					aria-label="Next page"
					disabled={currentEnd >= total - 1 && !data.nextChapterId}
					onclick={() => (isStrip ? jumpTo(currentIdx + 1) : nextPage())}
					><Icon name="chevron-right" size={16} /></button
				>
			</div>
		{/if}

		{#if data.chapters.length}
			<div class="stepper">
				<button
					class="step k-hit"
					aria-label="Previous chapter"
					disabled={!data.prevChapterId}
					{...warmOn(data.prevChapterId)}
					onclick={() => openChapter(data.prevChapterId)}
					><Icon name="chevron-left" size={16} /></button
				>
				<label class="sel">
					<span class="sel-cap">Chapter</span>
					<select
						aria-label="Jump to chapter"
						value={data.chapterId ?? ''}
						onchange={(e) => openChapter((e.currentTarget as HTMLSelectElement).value || undefined)}
					>
						{#each data.chapters as c (c.id ?? c.n)}
							<option value={c.id ?? ''} disabled={!c.id}
								>{chapterLabel(c)}{c.externalUrl ? ' (off-site)' : ''}</option
							>
						{/each}
					</select>
					<Icon name="chevron-down" size={14} />
				</label>
				<button
					class="step k-hit"
					aria-label="Next chapter"
					disabled={!data.nextChapterId}
					{...warmOn(data.nextChapterId)}
					onclick={() => openChapter(data.nextChapterId)}
					><Icon name="chevron-right" size={16} /></button
				>
			</div>
		{/if}

		<!-- Every copy of this chapter number, one row per (source, group) upload. Always
		     present when a chapter is open, so the reader can see WHO translated what
		     they are reading even when there is nothing to switch to. -->
		{#if data.chapterId}
			<div class="dr-sec">
				<span class="dr-label">Group &amp; source</span>
				<div class="tr-opts">
					{#if versions.length}
						{#each versions as v (`${v.src ?? 'spine'}:${v.id}`)}
							{@const label = chapterAttribution(v.source, v.group)}
							{@const ai = aiSpoken(v.group, v.sourceId)}
							<button
								class="tr-opt ver"
								class:on={v.current}
								aria-pressed={v.current}
								aria-label={v.current
									? `${label} on ${v.source}${ai} (reading now)`
									: `Read this chapter from ${label} on ${v.source}${ai}`}
								onclick={() => openVersion(v)}
							>
								{@render srcLogo(v.iconUrl, v.source)}
								<span class="ver-text">
									<span class="tr-name">{label}</span>
								</span>
								<AiBadge group={v.group} sourceId={v.sourceId} />
								{#if v.current}<Icon name="check" size={14} strokeWidth={2.6} />{/if}
							</button>
						{/each}
					{:else}
						<!-- No per-copy data (older API / not backfilled): describe the open
						     copy only, without pretending there is anything to switch to. -->
						{@const srcName = activeTranslator?.name ?? null}
						<div class="tr-opt ver on static">
							{@render srcLogo(activeTranslator?.iconUrl ?? null, srcName ?? openGroup ?? '?')}
							<span class="ver-text">
								<span class="tr-name"
									>{chapterAttribution(srcName || 'Unknown source', openGroup)}</span
								>
							</span>
							<AiBadge group={openGroup} sourceId={openSourceId} />
							<Icon name="check" size={14} strokeWidth={2.6} />
						</div>
					{/if}
				</div>
			</div>
		{/if}

		<!-- The series-wide preference: which source chapters open from by default (and
		     where prev/next go whenever it carries the number). Distinct from the one-off
		     copy pick above. -->
		{#if translators.length > 1}
			<div class="dr-sec">
				<span class="dr-label">Default source for this series</span>
				<div class="tr-opts">
					{#each translators as t (t.key)}
						<button
							class="tr-opt"
							class:on={t.key === data.selectedTranslatorKey}
							aria-pressed={t.key === data.selectedTranslatorKey}
							onclick={() => switchTranslator(t)}
							title={t.lang ? `${t.name} · ${t.lang.toUpperCase()}` : t.name}
						>
							{@render srcLogo(t.iconUrl, t.name)}
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

		<div class="dr-sec">
			<span class="dr-label">Display</span>
			{#if longStrip}
				<div class="cycle">
					<Icon name="list" size={16} />
					<span class="cycle-text"
						><span class="cycle-cap">Mode</span><span class="cycle-val">Long strip</span></span
					>
				</div>
			{:else}
				{@render cycle(MODES, mode, setMode, 'Mode')}
			{/if}
			{#if isStrip}
				<div class="width-row">
					<div class="width-head">
						<span>Page width</span><span class="width-val">{width}px</span>
					</div>
					<input
						type="range"
						name="pageWidth"
						aria-label="Page width"
						min={WIDTH_MIN}
						max={WIDTH_MAX}
						step={WIDTH_STEP}
						bind:value={width}
						oninput={persist}
					/>
				</div>
				<button
					class="cycle"
					aria-pressed={gaps}
					onclick={() => {
						gaps = !gaps;
						persist();
					}}
				>
					<Icon name="sliders-h" size={16} />
					<span class="cycle-text">
						<span class="cycle-cap">Page gaps</span>
						<span class="cycle-val">{gaps ? 'Space between pages' : 'Seamless'}</span>
					</span>
					<span class="switch" class:on={gaps}><span class="knob"></span></span>
				</button>
			{:else}
				{@render cycle(
					FITS,
					fit,
					(v) => {
						fit = v;
						persist();
					},
					'Fit',
				)}
				{@render cycle(
					DIRS,
					dir,
					(v) => {
						dir = v;
						persist();
					},
					'Direction',
				)}
			{/if}
			{@render cycle(
				HEADERS,
				headerMode,
				(v) => {
					headerMode = v;
					chromeVisible = true;
					persist();
				},
				'Header',
			)}
			{#if !longStrip}
				{@render cycle(
					BARS,
					preferredBar,
					(v) => {
						preferredBar = v;
						persist();
					},
					'Progress',
				)}
			{/if}
		</div>

		<a class="dr-report k-hit" href={reportHref}><Icon name="flag" size={14} />Report a problem</a>
		<p class="dr-keys">
			<kbd>M</kbd> menu · <kbd>←</kbd><kbd>→</kbd> turn pages · <kbd>Esc</kbd> close
		</p>
	</div>
{/snippet}

<!-- Links in here preload on TAP, not hover (the app default): SvelteKit keeps a single
     preloaded result, and a pointer drifting over a comment author or the series link on
     the way to "Next chapter" would otherwise throw away the warmed chapter. -->
<div
	class="reader"
	class:docked
	class:gaps
	class:header-off={headerMode === 'hidden'}
	style="--w:{width}px"
	data-sveltekit-preload-data="tap"
>
	<!-- top header -->
	<header class="chrome" class:hidden={chromeHidden}>
		<a class="icon-btn k-hit" href={seriesHref} aria-label="Back to series"
			><Icon name="chevron-left" size={18} /></a
		>
		<div class="titlebox">
			<a class="series k-hit" href={seriesHref}>{seriesTitle || 'Back to series'}</a>
			<!-- The group shrinks (and ellipsises) before the chapter label does. -->
			{#if data.chapterId}<span class="chapter"
					><span class="ch-label">{chapterLabel({ n: chNum, title: chTitle })}</span
					>{#if headerGroup}<span class="ch-group" title={headerGroup}>· {headerGroup}</span
						>{/if}</span
				>{/if}
		</div>
		<div class="head-right">
			{#if hasPages}<span class="pg-chip" aria-live={longStrip ? 'off' : 'polite'}
					>{longStrip ? `${Math.round(readPct)}%` : `Pg. ${pageReadout}`}</span
				>{/if}
			<!-- Prev/next chapter stay on phones (icon-only): going BACK a chapter must
			     never depend on finding the chapter picker inside the menu. -->
			<button
				class="icon-btn k-hit"
				aria-label="Previous chapter"
				title="Previous chapter"
				disabled={!data.prevChapterId}
				{...warmOn(data.prevChapterId)}
				onclick={() => openChapter(data.prevChapterId)}
				><Icon name="chevrons-left" size={16} /></button
			>
			<button
				class="icon-btn k-hit"
				aria-label="Next chapter"
				title="Next chapter"
				disabled={!data.nextChapterId}
				{...warmOn(data.nextChapterId)}
				onclick={() => openChapter(data.nextChapterId)}
				><Icon name="chevrons-right" size={16} /></button
			>
			{#if !docked}
				<button
					class="menu-btn k-hit"
					aria-haspopup="dialog"
					aria-expanded={menuOpen}
					onclick={openMenu}
					><Icon name="sliders" size={16} /><span class="hide-sm">Menu</span></button
				>
			{/if}
		</div>
	</header>

	<!-- menu: docked beside the pages (pinned, wide screens) or an overlay sheet -->
	{#if docked}
		<aside class="drawer" aria-label="Reader menu">{@render menu()}</aside>
	{:else if menuOpen}
		<div use:modalFocus role="dialog" aria-modal="true" aria-label="Reader menu">
			<button class="scrim" aria-label="Close menu" onclick={closeMenu}></button>
			<aside class="drawer floating">{@render menu()}</aside>
		</div>
	{/if}

	<!-- Tap/keyboard target behind the pages: toggles the header (or opens the menu
	     when there is no header). A real <button> keeps it operable by keyboard and
	     screen readers; links and page-turn zones above it still take their clicks. -->
	<button class="chrome-toggle" aria-label="Toggle reader controls" onclick={stageClick}></button>

	<!-- reading area -->
	<main
		class:loading={chapterLoading}
		ontouchstart={onSwipeStart}
		ontouchend={onSwipeEnd}
		ontouchcancel={() => (swipe = null)}
	>
		{#if externalUrl}
			<!-- An off-site chapter. It has no pages BY DESIGN, so it must not fall
			     through to the no-pages state below. `rel="noopener noreferrer"` because
			     the destination is a third party. -->
			<div class="no-pages">
				<div class="no-pages-icon"><Icon name="globe" size={26} /></div>
				<h2>This chapter is read on {externalHost}</h2>
				<p>
					The publisher hosts it themselves, so there are no pages for us to show. Open it on
					{externalHost} to keep reading.
				</p>
				<div class="no-pages-btns">
					<a class="next-ch" href={externalUrl} target="_blank" rel="noopener noreferrer">
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
					{#if data.prevChapterId}
						<button
							class="prev-ch"
							{...warmOn(data.prevChapterId)}
							onclick={() => openChapter(data.prevChapterId)}
							><Icon name="chevron-left" size={16} strokeWidth={2.4} />Previous chapter</button
						>
					{/if}
					{#if data.nextChapterId}
						<button
							class="next-ch"
							{...warmOn(data.nextChapterId)}
							onclick={() => openChapter(data.nextChapterId)}
							>Next chapter<Icon name="arrow-right" size={16} strokeWidth={2.4} /></button
						>
					{/if}
					<a class="all-ch" href={seriesHref}><Icon name="list" size={16} />Back to series</a>
				</div>
			</div>
		{:else if isStrip}
			<div class="strip" bind:this={stripEl}>
				<!-- Keyed on the CHAPTER as well as the page index: `p.index` alone repeats
				     across chapters, so Svelte reused each <img> and only patched `src`,
				     leaving the previous chapter's decoded frame on screen. -->
				{#each pages as p (`${data.chapterId}:${p.index}`)}
					<div class="strip-cell" style="aspect-ratio:{pageRatio[p.index] ?? DEFAULT_RATIO}">
						{#if p.url && !pageError[p.index]}
							<img
								class="strip-img"
								src={pageUrl[p.index] ?? p.url}
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
							<button class="page-ph page-err" onclick={() => retryImage(p.index)}>
								<span class="err-msg">
									<Icon name="alert" size={18} />
									<span>Page {p.label} failed — tap to retry</span>
								</span>
							</button>
						{:else}
							<div class="page-ph"><span class="page-tag">PAGE {p.label}</span></div>
						{/if}
					</div>
				{/each}
			</div>
		{:else}
			<div class="paged fit-{fit}" class:double={visiblePages.length > 1} class:rtl={dir === 'rtl'}>
				<!-- Keyed on the chapter so a chapter change REMOUNTS each <img> instead of
				     patching `src`, which left the previous chapter's frame on screen. -->
				{#each visiblePages as p (`${data.chapterId}:${p.index}`)}
					{#if p.url && !pageError[p.index]}
						<img
							class="paged-img"
							src={pageUrl[p.index] ?? p.url}
							alt={`Page ${p.label}`}
							decoding="async"
							onload={(e) => onImgLoad(e, p.index)}
							onerror={() => onImgError(p.index)}
						/>
					{:else if p.url && pageError[p.index]}
						<button class="page-ph paged-ph page-err" onclick={() => retryImage(p.index)}>
							<span class="err-msg">
								<Icon name="alert" size={18} />
								<span>Page {p.label} failed — tap to retry</span>
							</span>
						</button>
					{:else}
						<div class="page-ph paged-ph"><span class="page-tag">PAGE {p.label}</span></div>
					{/if}
				{/each}
			</div>
			<!-- Turn zones span the whole screen edge (not just the image), 35% each side;
			     the middle stays with the chrome toggle underneath. -->
			<button
				class="zone left"
				aria-label={dir === 'rtl' ? 'Next page' : 'Previous page'}
				onclick={goLeft}
			></button>
			<button
				class="zone right"
				aria-label={dir === 'rtl' ? 'Previous page' : 'Next page'}
				onclick={goRight}
			></button>
		{/if}

		<!-- end of chapter -->
		{#if hasPages}
			<div class="eoc">
				<div class="eoc-kicker">End of chapter {chNum}</div>
				<h2>{chTitle}</h2>
				<!-- On the LAST chapter there is no "Next Chapter" button at all: "All
				     chapters" becomes the primary action instead of a dead control. -->
				<div class="eoc-btns">
					{#if data.prevChapterId}
						<button
							class="prev-ch"
							{...warmOn(data.prevChapterId)}
							onclick={() => openChapter(data.prevChapterId)}
							><Icon name="chevron-left" size={16} strokeWidth={2.4} />Previous chapter</button
						>
					{/if}
					{#if data.nextChapterId}
						<button
							class="next-ch"
							{...warmOn(data.nextChapterId)}
							onclick={() => openChapter(data.nextChapterId)}
							>Next chapter<Icon name="arrow-right" size={16} strokeWidth={2.4} /></button
						>
					{/if}
					<a class="all-ch" class:solo={!data.nextChapterId} href={seriesHref}
						><Icon name="list" size={16} />All chapters</a
					>
				</div>
				{#if !data.nextChapterId}
					<p class="eoc-caught-up">You're all caught up — this is the latest chapter we have.</p>
				{:else if nextChapter}
					<p class="eoc-next">Up next: {chapterLabel(nextChapter)}</p>
				{/if}
			</div>
		{/if}

		<!-- comments (shared thread component; series discussion uses the same one) -->
		<section class="comments" id="comments">
			<CommentThread
				targetType="chapter"
				targetId={data.chapterId ?? ''}
				storageKey={chKey}
				prompt="Share your thoughts on this chapter…"
			/>
		</section>
	</main>

	<!-- Long strips track scroll distance; manga retains the configured page bar. -->
	{#if hasPages && bar !== 'hidden'}
		<div class="pbar-wrap" class:thin={bar === 'thin'} class:rtl={dir === 'rtl' && !isStrip}>
			{#if bar === 'continuous'}
				<div class="continuous-progress">
					<div class="pthin">
						<div class="pthin-fill" style="transform:scaleX({scrollFraction})"></div>
					</div>
					<input
						type="range"
						min="0"
						max="100"
						step="0.1"
						value={readPct}
						aria-label="Chapter progress"
						aria-valuetext={`${Math.round(readPct)}% through chapter`}
						oninput={(e) => jumpToProgress(Number(e.currentTarget.value) / 100)}
					/>
				</div>
			{:else if bar === 'thin'}
				<div class="pthin">
					<div class="pthin-fill" style="transform:scaleX({readPct / 100})"></div>
				</div>
			{:else}
				<div
					class="pbar"
					role="slider"
					tabindex="0"
					aria-label="Page"
					aria-valuemin={1}
					aria-valuemax={total}
					aria-valuenow={currentIdx + 1}
					aria-valuetext={`Page ${pageReadout}`}
					onclick={(e) => jumpTo(barIndexAt(e))}
					onpointermove={onBarMove}
					onpointerleave={() => (barHover = null)}
					onkeydown={onBarKey}
				>
					{#each pages as p (p.index)}
						<span
							class="seg"
							class:done={p.index < currentIdx}
							class:cur={p.index >= currentIdx && p.index <= currentEnd}
						></span>
					{/each}
					{#if barHover}
						<span class="ptip" style="left:{barHover.x}px">{barHover.i + 1}</span>
					{/if}
				</div>
			{/if}
		</div>
	{/if}
</div>

<style>
	/* --head and --bar are the space the top bar and the page bar hold, INCLUDING the
	   safe areas they sit in (status bar / notch above, home indicator below), so the
	   page area, the turn zones and "fit to screen" all stay clear of both. */
	.reader {
		--dock: 0px;
		--head: calc(56px + var(--k-safe-top));
		--bar: calc(22px + var(--k-safe-bottom));
		min-height: 100vh;
		min-height: 100dvh;
	}
	/* Status-bar scrim. When the top bar slides away (or is switched off) the pages
	   scroll up to the screen edge; this keeps them from running under the clock.
	   It has no height on a device without a top inset. */
	.reader::before {
		content: '';
		position: fixed;
		top: 0;
		left: 0;
		right: var(--dock);
		z-index: 49;
		height: var(--k-safe-top);
		background: var(--k-glass-strong);
		-webkit-backdrop-filter: blur(16px);
		backdrop-filter: blur(16px);
		pointer-events: none;
	}
	@media (min-width: 1024px) {
		.reader.docked {
			--dock: 320px;
		}
	}
	.reader.header-off {
		--head: var(--k-safe-top);
	}

	/* ---- header ---- */
	.chrome {
		position: fixed;
		top: 0;
		left: 0;
		right: var(--dock);
		z-index: 50;
		--pad: 10px;
		height: calc(56px + var(--k-safe-top));
		display: flex;
		align-items: center;
		gap: 8px;
		/* Controls sit below the status bar / notch and inside a landscape notch. */
		padding: var(--k-safe-top) max(var(--pad), var(--k-safe-right)) 0
			max(var(--pad), var(--k-safe-left));
		background: var(--k-glass-strong);
		-webkit-backdrop-filter: blur(16px);
		backdrop-filter: blur(16px);
		border-bottom: 1px solid var(--k-border);
		transition: transform 0.26s cubic-bezier(0.4, 0, 0.2, 1);
	}
	.chrome.hidden {
		transform: translateY(-100%);
	}
	.titlebox {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		line-height: 1.25;
	}
	.series {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-dim);
		text-decoration: none;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.series:hover {
		color: var(--k-accent);
	}
	.chapter {
		display: flex;
		gap: 5px;
		min-width: 0;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 15px;
		color: var(--k-text-bright);
		white-space: nowrap;
	}
	.ch-label,
	.ch-group {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.ch-label {
		flex: 0 1 auto;
	}
	/* The group only takes the room the label leaves (basis 0), so a long group name
	   gives way first and the chapter number is never the part that is cut off. */
	.ch-group {
		flex: 1 1 0;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.head-right {
		display: flex;
		align-items: center;
		gap: 6px;
		flex: 0 0 auto;
	}
	.pg-chip {
		font-size: 12px;
		font-weight: 700;
		font-variant-numeric: tabular-nums;
		color: var(--k-text-3);
		padding: 6px 10px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		white-space: nowrap;
	}
	.icon-btn {
		width: 36px;
		height: 36px;
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border-radius: var(--k-radius-md);
		background: transparent;
		border: 1px solid var(--k-border-3);
		color: var(--k-text-3);
		cursor: pointer;
		text-decoration: none;
		padding: 0;
	}
	.icon-btn:hover:not(:disabled) {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.icon-btn:disabled {
		opacity: 0.35;
		cursor: default;
	}
	.icon-btn.on {
		background: var(--k-accent);
		border-color: var(--k-accent);
		color: #fff;
	}
	.menu-btn {
		height: 36px;
		display: inline-flex;
		align-items: center;
		gap: 8px;
		padding: 0 14px;
		border-radius: var(--k-radius-md);
		background: var(--k-primary);
		color: var(--k-on-primary);
		border: none;
		font-weight: 700;
		font-size: 13px;
		cursor: pointer;
	}
	.menu-btn:hover {
		background: var(--k-primary-hover);
	}

	/* ---- drawer ---- */
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 70;
		border: none;
		background: rgba(8, 8, 9, 0.5);
		cursor: default;
		animation: fade 0.18s ease-out;
	}
	.drawer {
		position: fixed;
		top: 0;
		right: 0;
		bottom: 0;
		width: 320px;
		z-index: 71;
		display: flex;
		flex-direction: column;
		background: var(--k-surface-3);
		border-left: 1px solid var(--k-border-3);
		/* Landscape notch on the right: the panel reaches the edge, its content does not. */
		padding-right: var(--k-safe-right);
	}
	.drawer.floating {
		box-shadow: -24px 0 70px rgba(0, 0, 0, 0.5);
		animation: slide-in 0.22s cubic-bezier(0.2, 0.8, 0.2, 1);
	}
	@keyframes slide-in {
		from {
			transform: translateX(100%);
		}
	}
	@keyframes fade {
		from {
			opacity: 0;
		}
	}
	.dr-head {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 12px 14px;
		border-bottom: 1px solid var(--k-border);
	}
	.dr-title {
		flex: 1;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 15px;
		color: var(--k-text);
	}
	.dr-body {
		flex: 1;
		overflow-y: auto;
		overscroll-behavior: contain;
		padding: 16px 14px calc(20px + env(safe-area-inset-bottom));
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.dr-series,
	.dr-chapter {
		display: flex;
		align-items: flex-start;
		gap: 10px;
		font-size: 14px;
		line-height: 1.35;
	}
	.dr-series {
		font-weight: 700;
		color: var(--k-accent);
		text-decoration: none;
	}
	.dr-series:hover span {
		text-decoration: underline;
	}
	.dr-chapter {
		color: var(--k-text-2);
		font-weight: 600;
		margin-bottom: 4px;
	}
	.dr-uploader {
		display: flex;
		align-items: flex-start;
		gap: 10px;
		margin: -6px 0 4px;
		font-size: 13px;
		font-weight: 600;
		line-height: 1.35;
		color: var(--k-text-2);
	}
	.dr-up-cap {
		color: var(--k-text-faint);
	}
	.dr-series :global(svg),
	.dr-chapter :global(svg),
	.dr-uploader :global(svg) {
		flex: 0 0 auto;
		margin-top: 1px;
	}
	.stepper {
		display: flex;
		gap: 6px;
		align-items: stretch;
	}
	.step {
		width: 34px;
		flex: 0 0 auto;
		display: flex;
		align-items: center;
		justify-content: center;
		border-radius: var(--k-radius);
		background: var(--k-surface-5);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-2);
		cursor: pointer;
		padding: 0;
	}
	.step:hover:not(:disabled) {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.step:disabled {
		opacity: 0.35;
		cursor: default;
	}
	.sel {
		position: relative;
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		padding: 6px 30px 6px 12px;
		border-radius: var(--k-radius);
		background: var(--k-surface-5);
		border: 1px solid var(--k-border-2);
		cursor: pointer;
	}
	.sel:focus-within {
		border-color: var(--k-accent);
	}
	.sel-cap {
		font-size: 10.5px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	.sel select {
		appearance: none;
		background: transparent;
		border: none;
		color: var(--k-text);
		font: inherit;
		font-size: 13.5px;
		font-weight: 700;
		padding: 0;
		width: 100%;
		cursor: pointer;
		outline: none;
		text-overflow: ellipsis;
	}
	.sel select option {
		background: var(--k-surface-3);
		color: var(--k-text);
	}
	.sel :global(svg) {
		position: absolute;
		right: 10px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--k-text-dim);
		pointer-events: none;
	}
	.dr-sec {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding-top: 12px;
		border-top: 1px solid var(--k-border);
	}
	.dr-label {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--k-text-faint);
		margin-bottom: 2px;
	}
	.cycle {
		display: flex;
		align-items: center;
		gap: 12px;
		width: 100%;
		padding: 9px 12px;
		border-radius: var(--k-radius);
		background: var(--k-surface-5);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-2);
		cursor: pointer;
		text-align: left;
	}
	.cycle:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.cycle-text {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
	}
	.cycle-cap {
		font-size: 10.5px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	.cycle-val {
		font-size: 13.5px;
		font-weight: 700;
		color: var(--k-text);
	}
	.switch {
		width: 34px;
		height: 20px;
		border-radius: 999px;
		padding: 2px;
		display: flex;
		background: var(--k-border-4);
		transition: background 0.18s;
	}
	.switch.on {
		justify-content: flex-end;
		background: var(--k-accent);
	}
	.knob {
		width: 16px;
		height: 16px;
		border-radius: 50%;
		background: #fff;
	}
	/* The slider starts at 480px, so on a narrower screen it cannot change anything:
	   the strip is already as wide as the screen. Shown from where it has an effect. */
	.width-row {
		display: none;
		padding: 8px 12px 10px;
		border-radius: var(--k-radius);
		background: var(--k-surface-5);
		border: 1px solid var(--k-border-2);
	}
	.width-head {
		display: flex;
		justify-content: space-between;
		font-size: 12px;
		font-weight: 700;
		color: var(--k-text-faint);
		margin-bottom: 4px;
	}
	.width-val {
		color: var(--k-text-2);
		font-variant-numeric: tabular-nums;
	}
	.width-row input {
		width: 100%;
		cursor: pointer;
	}
	.tr-opts {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.tr-opt {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 7px 10px;
		border-radius: var(--k-radius);
		background: var(--k-surface-5);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-2);
		cursor: pointer;
		text-align: left;
	}
	.tr-opt:hover {
		border-color: var(--k-border-strong);
	}
	.tr-opt.on {
		border-color: var(--k-accent);
		background: rgba(224, 131, 105, 0.12);
		color: var(--k-text-bright);
	}
	.tr-logo {
		width: 24px;
		height: 24px;
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
	/* Group & source rows: group on top, source beneath. */
	.ver-text {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.ver-text .tr-name {
		flex: none;
	}
	.tr-opt.ver.on,
	.tr-opt.static {
		cursor: default;
	}
	.tr-opt.static:hover {
		border-color: var(--k-accent);
	}
	.tr-count {
		font-size: 11px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	.dr-source {
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 0;
		margin: 2px 0 4px;
		padding: 10px 12px;
		border-radius: 12px;
		border: 1px solid var(--k-border-2);
		background: var(--k-surface-4);
		color: var(--k-text-2);
		text-decoration: none;
	}
	.dr-source :global(svg) {
		flex: 0 0 auto;
	}
	.dr-source-text {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		line-height: 1.3;
	}
	.dr-source-cap {
		font-size: 13px;
		font-weight: 700;
		color: var(--k-text-bright);
	}
	.dr-source-sub {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-dim);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	a.dr-source:hover,
	a.dr-source:focus-visible {
		border-color: var(--k-accent);
		color: var(--k-accent);
	}
	.dr-source.off {
		background: transparent;
		border-style: dashed;
		color: var(--k-text-faint);
	}
	.dr-source.off .dr-source-cap {
		color: var(--k-text-3);
	}
	.dr-source.off .dr-source-sub {
		white-space: normal;
	}
	.dr-report {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		margin-top: 6px;
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-dim);
		text-decoration: none;
	}
	.dr-report:hover {
		color: var(--k-cancelled);
	}
	.dr-keys {
		margin: 0;
		font-size: 11.5px;
		color: var(--k-text-faint);
	}
	kbd {
		display: inline-block;
		min-width: 18px;
		padding: 1px 5px;
		margin: 0 1px;
		border-radius: 4px;
		border: 1px solid var(--k-border-4);
		font-family: var(--k-font-mono);
		font-size: 10.5px;
		text-align: center;
		color: var(--k-text-2);
	}

	/* ---- reading surface ---- */
	main {
		position: relative;
		z-index: 1;
		min-height: 100vh;
		min-height: 100dvh;
		margin-right: var(--dock);
		padding: var(--head) 0 var(--bar);
		display: flex;
		flex-direction: column;
		align-items: center;
		pointer-events: none;
		overflow-anchor: none;
	}
	/* Re-enable pointer events on interactive controls and readable text, so the
	   chrome-toggle button underneath only receives taps on the pages, gaps and
	   margins while links work and comment text stays selectable. */
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
	/* Chapter swap in progress — see `chapterLoading`. `:global(*)` because the rule
	   above re-enables pointer events on every link/button and would out-specify a
	   bare `main.loading`. */
	main.loading {
		opacity: 0.32;
		/* Held back a beat: a preloaded chapter swaps in well under this, and dimming for
		   one frame first reads as a flicker rather than as speed. */
		transition: opacity 0.15s ease-out 0.15s;
	}
	main.loading :global(*) {
		pointer-events: none;
	}
	.chrome-toggle {
		position: fixed;
		top: 0;
		left: 0;
		bottom: 0;
		right: var(--dock);
		z-index: 0;
		border: none;
		background: transparent;
		cursor: default;
		padding: 0;
	}
	.chrome-toggle:focus-visible {
		outline-offset: -4px;
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

	/* long strip */
	.strip {
		display: flex;
		flex-direction: column;
		align-items: center;
		width: 100%;
	}
	.gaps .strip {
		gap: 14px;
	}
	/* Each cell reserves its page's space from the real ratio once known (2:3
	   before), so scroll geometry stays stable. Seamless by default. */
	.strip-cell {
		position: relative;
		display: block;
		width: 100%;
		max-width: var(--w);
		background: var(--k-bg);
		/* The pre-load 2:3 reservation can be shorter than the natural image for the
		   frame before the measured ratio applies — clip so it never overlaps. */
		overflow: hidden;
	}
	.strip-img {
		display: block;
		width: 100%;
		height: auto;
	}
	.page-ph {
		position: absolute;
		inset: 0;
		border: none;
		background: var(--k-cover);
		background-image: var(--k-hatch);
	}
	.page-err {
		display: flex;
		align-items: center;
		justify-content: center;
		cursor: pointer;
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
	.page-err:hover .err-msg {
		color: var(--k-text);
	}
	.page-tag {
		position: absolute;
		top: 14px;
		left: 14px;
		font-family: var(--k-font-mono);
		font-size: 10.5px;
		letter-spacing: 0.16em;
		color: var(--k-text-ghost);
	}

	/* paged (single + double). The view height excludes the header (when it holds
	   space) and the page bar, so "fit to screen" really fits. */
	.paged {
		--view-h: calc(100dvh - var(--head) - var(--bar));
		width: 100%;
		min-height: var(--view-h);
		display: flex;
		align-items: center;
		justify-content: center;
		overflow-x: auto;
	}
	.paged.rtl {
		flex-direction: row-reverse;
	}
	.paged-img {
		display: block;
		flex: 0 1 auto;
		min-width: 0;
	}
	.fit-both .paged-img {
		max-height: var(--view-h);
		max-width: 100%;
		object-fit: contain;
	}
	.fit-both.double .paged-img {
		max-width: 50%;
	}
	.fit-width .paged-img {
		width: 100%;
		height: auto;
	}
	.fit-width.double .paged-img {
		width: 50%;
	}
	.fit-height .paged-img {
		height: var(--view-h);
		width: auto;
		max-width: none;
		flex-shrink: 0;
	}
	.fit-none .paged-img {
		max-width: none;
		flex-shrink: 0;
	}
	/* Overflowing pages (fit height / original) start at the reading edge. */
	.fit-height,
	.fit-none {
		justify-content: safe center;
	}
	.paged-ph {
		position: relative;
		height: var(--view-h);
		aspect-ratio: 2 / 3;
		max-width: 100%;
	}
	.zone {
		position: fixed;
		top: var(--head);
		bottom: var(--bar);
		width: calc((100% - var(--dock)) * 0.35);
		z-index: 2;
		background: transparent;
		border: none;
		padding: 0;
	}
	.zone.left {
		left: 0;
		cursor: w-resize;
	}
	.zone.right {
		right: var(--dock);
		cursor: e-resize;
	}
	.zone:focus-visible {
		outline-offset: -4px;
	}

	/* end of chapter */
	.eoc {
		position: relative;
		z-index: 3;
		width: 100%;
		max-width: 640px;
		padding: 56px 20px 64px;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		text-align: center;
	}
	.eoc-kicker {
		font-size: 12px;
		font-weight: 700;
		letter-spacing: 0.12em;
		text-transform: uppercase;
		color: var(--k-accent);
	}
	.eoc h2 {
		font-size: 26px;
		margin: 0;
		color: var(--k-text-bright);
	}
	.eoc-btns {
		display: flex;
		gap: 12px;
		flex-wrap: wrap;
		justify-content: center;
		margin-top: 10px;
	}
	.next-ch,
	.prev-ch,
	.all-ch {
		height: 48px;
		justify-content: center;
		padding: 0 24px;
		border-radius: var(--k-radius-md);
		font-weight: 700;
		font-size: 14.5px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		gap: 9px;
		text-decoration: none;
	}
	.next-ch {
		border: none;
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.next-ch:hover {
		background: var(--k-primary-hover);
	}
	.prev-ch,
	.all-ch {
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text);
	}
	.prev-ch:hover,
	.all-ch:hover {
		border-color: var(--k-border-strong);
	}
	.all-ch.solo {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.eoc-caught-up,
	.eoc-next {
		margin: 6px 0 0;
		font-size: 13.5px;
		color: var(--k-text-dimmer);
	}
	.comments {
		position: relative;
		z-index: 3;
		width: 100%;
		max-width: 720px;
		padding: 12px 20px 96px;
		display: flex;
		flex-direction: column;
		gap: 22px;
		scroll-margin-top: calc(var(--head) + 12px);
	}

	/* ---- page bar ---- */
	.pbar-wrap {
		position: fixed;
		left: 0;
		right: var(--dock);
		bottom: 0;
		z-index: 45;
		--pad: 8px;
		padding: 0 max(var(--pad), var(--k-safe-right)) calc(6px + var(--k-safe-bottom))
			max(var(--pad), var(--k-safe-left));
		pointer-events: none;
	}
	.pbar-wrap.thin {
		padding: 0;
	}
	.pbar {
		position: relative;
		height: 16px;
		display: flex;
		align-items: flex-end;
		gap: 3px;
		cursor: pointer;
		pointer-events: auto;
		touch-action: none;
	}
	.pbar-wrap.rtl .pbar {
		flex-direction: row-reverse;
	}
	/* A fixed 10px box shown at 40% height (4px) and grown on hover with a transform:
	   animating `height` on 100+ segments re-ran layout every frame. Bottom origin
	   keeps the bar anchored to the screen edge. The radius is 2px across / 5px down
	   so that, squashed to 0.4, the resting corners still read as the old 2px. */
	.seg {
		flex: 1;
		height: 10px;
		border-radius: 2px / 5px;
		background: var(--k-border-4);
		transform: scaleY(0.4);
		transform-origin: bottom;
		transition:
			transform 0.15s ease,
			background-color 0.15s;
	}
	.pbar:hover .seg,
	.pbar:focus-visible .seg {
		transform: scaleY(1);
	}
	.seg.done {
		background: color-mix(in srgb, var(--k-accent) 55%, transparent);
	}
	.seg.cur {
		background: var(--k-accent);
	}
	.ptip {
		position: absolute;
		bottom: 20px;
		transform: translateX(-50%);
		padding: 3px 8px;
		border-radius: 6px;
		background: var(--k-surface-5);
		border: 1px solid var(--k-border-3);
		font-size: 12px;
		font-weight: 700;
		color: var(--k-text);
		pointer-events: none;
		white-space: nowrap;
	}
	.pthin {
		height: 3px;
		background: var(--k-border);
	}
	.continuous-progress {
		position: relative;
		height: 22px;
		display: flex;
		align-items: flex-end;
		pointer-events: auto;
	}
	.continuous-progress .pthin {
		width: 100%;
		height: 5px;
		border-radius: 3px;
		overflow: hidden;
	}
	.continuous-progress input {
		position: absolute;
		inset: 0;
		width: 100%;
		margin: 0;
		opacity: 0;
		cursor: pointer;
	}
	.continuous-progress:focus-within {
		outline: 2px solid var(--k-accent);
		outline-offset: 3px;
	}
	/* Full-width fill scaled to the read fraction: a transform, so moving it never
	   re-runs layout. */
	.pthin-fill {
		height: 100%;
		background: var(--k-accent);
		transform-origin: left;
		transition: transform 0.12s linear;
	}
	.pbar-wrap.rtl .pthin-fill {
		transform-origin: right;
	}

	/* Wider than a phone: the bars get their roomier spacing back, the side panel
	   (full height here, a bottom sheet on phones) clears the status bar, and the
	   page-width slider has something to do. */
	@media (min-width: 520px) {
		.width-row {
			display: block;
		}
	}
	@media (min-width: 641px) {
		.chrome {
			--pad: 16px;
			gap: 12px;
		}
		.head-right {
			gap: 8px;
		}
		.drawer {
			padding-top: var(--k-safe-top);
		}
		.pbar-wrap {
			--pad: 12px;
		}
	}
	@media (max-width: 640px) {
		.chapter {
			font-size: 14px;
		}
		.hide-sm {
			display: none;
		}
		.menu-btn {
			width: 36px;
			padding: 0;
			justify-content: center;
		}
		.drawer.floating {
			width: 100%;
			top: auto;
			max-height: 86dvh;
			border-left: none;
			border-top: 1px solid var(--k-border-3);
			border-radius: 18px 18px 0 0;
			animation-name: slide-up;
		}
		.dr-keys {
			display: none;
		}
		.eoc h2 {
			font-size: 22px;
		}
		/* Phone: the primary "Next chapter" takes its own full-width row on top;
		   "Previous chapter" and "All chapters" share the row beneath. */
		.eoc-btns .next-ch {
			order: -1;
			flex: 1 1 100%;
		}
		.eoc-btns .prev-ch,
		.eoc-btns .all-ch {
			flex: 1 1 0;
			min-width: 0;
			padding: 0 12px;
			font-size: 14px;
		}
		.pbar {
			gap: 2px;
		}
	}
	/* Very narrow phones: the page chip goes first so the title keeps room — the page
	   bar at the bottom still shows progress, and the menu has the page picker. */
	@media (max-width: 480px) {
		.pg-chip {
			display: none;
		}
	}
	@keyframes slide-up {
		from {
			transform: translateY(100%);
		}
	}
	/* Touch: the page bar and the strip scrubber are thin lines; give the finger a
	   44px band around them (it reaches down over the home-indicator padding). */
	@media (pointer: coarse) {
		.pbar::after {
			content: '';
			position: absolute;
			left: 0;
			right: 0;
			top: -20px;
			bottom: -8px;
		}
		.continuous-progress input {
			inset: -14px 0 -8px;
			height: auto;
		}
		.tr-opt {
			min-height: 44px;
		}
	}
</style>
