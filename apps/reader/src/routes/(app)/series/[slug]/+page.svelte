<script lang="ts">
	import { modalFocus } from '$lib/modal-focus';
	import { page } from '$app/state';
	import { goto, replaceState } from '$app/navigation';
	import { tick, untrack } from 'svelte';
	import AiBadge from '$lib/components/AiBadge.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Stars from '$lib/components/Stars.svelte';
	import Ratings from '$lib/components/Ratings.svelte';
	import MangaCard from '$lib/components/MangaCard.svelte';
	import SeriesMetadata from '$lib/components/SeriesMetadata.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import CommentThread from '$lib/components/CommentThread.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import StatusMenu from '$lib/components/StatusMenu.svelte';
	import MergeDialog from '$lib/components/MergeDialog.svelte';
	import UnmergeDialog from '$lib/components/UnmergeDialog.svelte';
	import { deriveShelf, FLAG, type Shelf } from '$lib/data/types';
	import {
		setLibraryMark,
		setFavorite,
		setLibraryStatus,
		getSeries,
		getSeriesNsfwOverride,
		setSeriesNsfw,
		setChapterRead,
	} from '$lib/data/source';
	import { backend, images } from '$lib/context';
	import { apiOrigin, config } from '$lib/config';
	import { retryCoverUrl } from '$lib/data/cover-retry';
	import { auth } from '$lib/auth.svelte';
	import {
		socialLive,
		loadSeriesSocial,
		saveSeriesRating,
		clearSeriesRating,
	} from '$lib/data/social-repo';

	import type { SeriesResult } from '$lib/data/source';
	import { volumeHeaders } from '$lib/data/chapter-volumes';
	import { chapterAttribution } from '$lib/data/chapter-versions';
	import { setPreferredTranslator } from '$lib/data/translator-pref.svelte';
	import { aiSpoken, hasAiBadge, loadScanlatorAi } from '$lib/scanlator-ai.svelte';

	let { data } = $props();

	// The AI tags, asked for here and not only by a badge: a chapter row with no group
	// has no badge to ask, and it needs the answer to know whether to show one.
	void loadScanlatorAi();

	// `data.series` is a RESOLVED result on the server and on hydration, and a
	// pending Promise on client-side navigations — see +page.ts. It never rejects:
	// it resolves to { view, error }, where a null view with error=false is a genuine
	// not-found and error=true is a backend outage (honest error state + retry).
	//
	// The resolved case MUST be a $derived, not an $effect: effects don't run during
	// SSR, so filling `view` from one is exactly what made the edge serve — and cache
	// — an empty hero/chapter skeleton for every shareable series link.
	const settled = $derived(
		data.series instanceof Promise ? undefined : (data.series as SeriesResult),
	);
	// Filled by the effect on client-side navigations only.
	let streamed = $state<SeriesResult | undefined>(undefined);
	$effect(() => {
		const s = data.series;
		streamed = undefined;
		if (!(s instanceof Promise)) return;
		// Guard against a slow A→B navigation letting A's response overwrite B
		// (mirrors the social-load effect below).
		let cancelled = false;
		s.then((r) => {
			if (!cancelled) streamed = r;
		});
		return () => {
			cancelled = true;
		};
	});

	// A locally-refetched result (retry, or a translator switch) that supersedes
	// `load`'s payload until the next navigation replaces it.
	let override = $state<SeriesResult | undefined>(undefined);
	$effect(() => {
		data.series; // a new payload (navigation) discards any local override
		untrack(() => (override = undefined));
	});

	let retrying = $state(false);
	const result = $derived(override ?? settled ?? streamed);
	const view = $derived(result?.view ?? null);
	const loading = $derived(retrying || !result);
	const loadError = $derived(result?.error ?? false);

	async function retrySeries(): Promise<void> {
		if (retrying) return;
		retrying = true;
		override = undefined;
		// Pin the slug we're retrying so a mid-flight navigation can't be clobbered.
		const slug = data.slug;
		try {
			const r = await getSeries(slug);
			if (data.slug !== slug) return;
			override = r;
		} finally {
			retrying = false;
		}
	}

	const detail = $derived(view?.detail);
	const seriesDetail = $derived(view?.detail); // alias: template reads seriesDetail.author/artist/votes/…
	const title = $derived(detail?.title ?? '');
	const type = $derived(detail?.type ?? 'Manga');
	const rating = $derived(detail?.rating ?? '');
	const totalCh = $derived(detail?.totalCh ?? 0);
	const statusLabel = $derived(detail?.statusLabel ?? '');
	// Publication ended (completed/cancelled) — NOT the viewer's shelf. Gates both the
	// derived `completed` shelf and whether the picker offers it at all. Defaults to
	// false while `detail` is still loading, so the option can only ever appear once
	// we actually know the series has ended.
	const ended = $derived(detail?.ended ?? false);
	const continueCh = $derived(detail?.continueCh ?? 1);
	// All-time views, compactly formatted (1.2K / 3.4M). Only shown once a series has
	// any recorded reads, so a brand-new/untracked series doesn't display "0 views".
	const viewsTotal = $derived(detail?.viewsTotal ?? 0);
	const viewsLabel = $derived(
		new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(
			viewsTotal,
		),
	);
	const relatedSeries = $derived(view?.related ?? []);

	// `src` (a Suwayomi manga id) pins which source a chapter is read from when it
	// comes from a non-preferred source in the aggregated list (S2 per-chapter
	// fallback); omitted for the preferred/mirror source.
	function chapterHref(chId?: string, src?: string | null) {
		const base = `/read/${detail?.id ?? data.slug}`;
		if (!chId) return base;
		return `${base}?ch=${chId}${src ? `&src=${src}` : ''}`;
	}
	const readHref = $derived(chapterHref(detail?.startChapterId, detail?.startChapterSrc));

	let inLibrary = $state(false);
	let marking = $state(false);
	// Favourite + explicit shelf, reflected from the backend on load then toggled
	// optimistically. Setting either implies library membership (server adds it).
	let isFavorite = $state(false);
	let favBusy = $state(false);
	let libStatus = $state<Shelf | null>(null);
	// The star rating (community score) is kept; the old bespoke review-comment list
	// was retired in favour of the unified CommentThread discussion below. `myBody`
	// is preserved so a rating upsert doesn't wipe any review body already on record.
	let userRating = $state(0);
	let myBody = $state('');
	const sort = $derived<'newest' | 'oldest'>(
		page.url.searchParams.get('order') === 'oldest' ? 'oldest' : 'newest',
	);
	const orderHref = (order: string) => {
		const url = new URL(page.url);
		url.searchParams.set('order', order);
		// A new order starts from its first page of chapters.
		url.searchParams.delete('cpage');
		return url.pathname + url.search + '#chapters';
	};
	const SORTS = [
		{ value: 'newest', label: 'Newest first' },
		{ value: 'oldest', label: 'Oldest first' },
	] as const;

	// EVERY id-keyed action on this page (library mark, favourite, shelf, ratings,
	// reviews, chapter links) goes through `detail.id` — which is the id the BACKEND
	// resolved, not the one in the URL. Those differ for a work that has been merged
	// away: the server follows `work_redirect` and answers with the survivor, so
	// writes land on the live work instead of a retired row nothing will ever read.
	const seriesId = $derived(detail?.id ?? '');
	// Offline/mock storage key only — the live social path keys off `seriesId`
	// above. Left on the URL slug so a locally-stored rating doesn't move out from
	// under an offline reader; there is no server to merge works in that mode.
	const socialKey = $derived(data.slug ?? page.params.slug ?? '');
	const needsAuth = $derived(socialLive() && !auth.user);

	// Per-chapter read/unread from the list. Read state lives on the server per viewer, so
	// the toggle exists only for a signed-in reader; signed out, the mark stays a static
	// indicator. Optimistic: the override paints at once and is dropped again if the write
	// fails, so the mark never claims a state the server did not take. Keyed by chapter id
	// and cleared when the series changes — `view.chapters` is the truth after a reload.
	let readOverride = $state<Record<string, boolean>>({});
	let readPending = $state<Record<string, boolean>>({});
	$effect(() => {
		void seriesId;
		readOverride = {};
		readPending = {};
	});
	const canMarkRead = $derived(auth.ready && !!auth.user);
	function isChapterRead(c: { id?: string; read: boolean }): boolean {
		return c.id && c.id in readOverride ? readOverride[c.id] : c.read;
	}
	async function toggleChapterRead(c: { id?: string; read: boolean }) {
		const id = c.id;
		if (!id || readPending[id]) return;
		// The state BEFORE this click, overrides included: after one successful toggle
		// `c.read` is the stale page-load value, so a failed second toggle must restore THIS,
		// not fall back to it.
		const previous = isChapterRead(c);
		const next = !previous;
		readOverride = { ...readOverride, [id]: next };
		readPending = { ...readPending, [id]: true };
		const ok = await setChapterRead(id, next);
		const { [id]: _done, ...rest } = readPending;
		readPending = rest;
		if (!ok) readOverride = { ...readOverride, [id]: previous };
	}

	// Heal the address bar when the id we asked for is not the id we got back.
	//
	// A shallow `replaceState`, deliberately:
	//  • REPLACE, not push — Back must still return to wherever the reader came
	//    from, not step through the retired URL they never chose to visit.
	//  • SHALLOW (no `goto`) — `load` must NOT re-run. The rendered page is already
	//    the right one; re-running it would repeat the whole multi-round-trip
	//    resolution and, on a client-side navigation, flash the streaming skeleton
	//    for what is a purely cosmetic URL correction.
	// Nothing on the page depends on this landing — it only decides what the reader
	// bookmarks or copies next. `healedId` (deliberately not `$state`) makes the
	// effect idempotent: `replaceState` publishes a new `page` object, so a version
	// that re-read `page.url` reactively would loop.
	let healedId = '';
	$effect(() => {
		const id = detail?.id ?? '';
		if (!id.startsWith('w_') || id === healedId) return;
		healedId = id;
		let cancelled = false;
		// Deferred by one microtask: on a first (hydrating) load this effect flushes
		// while SvelteKit is still finishing `_hydrate`, and shallow routing refuses
		// to run before the router reports itself started — which it does one awaited
		// microtask after mounting the root. Reading `location` rather than
		// `page.url` also keeps the callback out of the effect's dependency graph, so
		// the new `page` object `replaceState` publishes can't re-trigger it.
		queueMicrotask(() => {
			if (cancelled) return;
			const want = `/series/${id}`;
			if (location.pathname === want) return;
			try {
				replaceState(want + location.search + location.hash, page.state);
			} catch (err) {
				// Purely cosmetic — never break the page over it.
				console.warn('[komiq] could not rewrite the URL to the resolved work id:', err);
			}
		});
		return () => {
			cancelled = true;
		};
	});

	// Reflect the backend's library state on load (resets when navigating series).
	$effect(() => {
		const marked = detail?.isMarked ?? false;
		const fav = detail?.isFavorite ?? false;
		const status = detail?.libraryStatus ?? null;
		untrack(() => {
			inLibrary = marked;
			isFavorite = fav;
			libStatus = status;
		});
	});

	// The shelf the picker shows: the explicit choice, else derived from progress via
	// the SHARED `deriveShelf` — this page used to carry its own inline copy of the
	// rule, which is how the same series could read "Completed" here and "Reading" on
	// the library. Catching up on an ongoing series no longer derives to `completed`.
	// Through `isChapterRead`, not `c.read`: a chapter toggled from the list must move the
	// shelf and the hero's Continue/Read state too, not only its own mark.
	const readCount = $derived((view?.chapters ?? []).filter((c) => isChapterRead(c)).length);
	const effectiveShelf = $derived<Shelf>(libStatus ?? deriveShelf(readCount, totalCh, ended));

	// Library writes are optimistic, but a FAILED write now rolls back and says so.
	// Previously the data layer returned the optimistic argument on error, so an
	// expired token left the button reading "In Library" forever and the reader only
	// discovered nothing had saved by reloading. `WriteResult.ok` makes the two
	// distinguishable and `.value` is always the state to display.
	let writeError = $state('');

	async function toggleLibrary(): Promise<void> {
		if (marking) return;
		const prev = inLibrary;
		const next = !prev;
		inLibrary = next; // optimistic
		marking = true;
		writeError = '';
		try {
			const r = await setLibraryMark(seriesId, next, prev);
			inLibrary = r.value;
			if (!r.ok) {
				writeError = r.error ?? '';
				return;
			}
			// Removing from the library also drops its shelf/favourite server-side (the
			// row is deleted) — reflect that so the controls don't show stale state.
			if (!inLibrary) {
				libStatus = null;
				isFavorite = false;
			}
		} finally {
			marking = false;
		}
	}

	async function toggleFavorite(): Promise<void> {
		if (favBusy) return;
		const prevFav = isFavorite;
		const prevInLibrary = inLibrary;
		const next = !prevFav;
		isFavorite = next; // optimistic
		if (next) inLibrary = true; // favouriting implies membership
		favBusy = true;
		writeError = '';
		try {
			const r = await setFavorite(seriesId, next, prevFav);
			isFavorite = r.value;
			if (!r.ok) {
				inLibrary = prevInLibrary; // the implied membership didn't happen either
				writeError = r.error ?? '';
			}
		} finally {
			favBusy = false;
		}
	}

	// Guarded like the two above: without a busy flag a double-click fired two
	// mutations whose completion order decided the stored shelf.
	let statusBusy = $state(false);
	async function chooseStatus(s: Shelf): Promise<void> {
		if (statusBusy) return;
		const prev = libStatus;
		const prevInLibrary = inLibrary;
		libStatus = s; // optimistic
		inLibrary = true; // filing a shelf implies membership
		statusBusy = true;
		writeError = '';
		try {
			const r = await setLibraryStatus(seriesId, s, prev);
			libStatus = r.value;
			if (!r.ok) {
				inLibrary = prevInLibrary;
				writeError = r.error ?? '';
			}
		} finally {
			statusBusy = false;
		}
	}

	// Load ratings + reviews for this series (re-runs on navigation and sign-in).
	$effect(() => {
		const id = seriesId;
		const key = socialKey;
		void auth.user?.id; // reload after auth changes so "mine" resolves
		if (!id) return; // series still streaming in
		let cancelled = false;
		loadSeriesSocial(id, key)
			.then((s) => {
				if (cancelled) return;
				userRating = s.myScore;
				myBody = s.myBody;
			})
			.catch(() => {});
		return () => {
			cancelled = true;
		};
	});

	// Share the current series via the Web Share API, falling back to copying the
	// link to the clipboard when the native sheet isn't available.
	let shareCopied = $state(false);
	async function shareSeries(): Promise<void> {
		if (typeof window === 'undefined') return;
		const url = window.location.href;
		const shareData = {
			title: title || 'komiq',
			text: title ? `Check out ${title} on komiq` : 'Check out this series on komiq',
			url,
		};
		if (navigator.share) {
			try {
				await navigator.share(shareData);
				return;
			} catch {
				return; // user dismissed the share sheet
			}
		}
		try {
			await navigator.clipboard?.writeText(url);
			shareCopied = true;
			setTimeout(() => (shareCopied = false), 2000);
		} catch {
			/* clipboard unavailable — nothing more we can do */
		}
	}

	// Rating is the one action on this page whose RESULT is rendered from the series
	// payload rather than from local state: the stars reflect `userRating` immediately,
	// but "N community ratings" and the average come from `detail`, which `load` fetched
	// before the vote existed. Without a refetch the page kept saying "0 community
	// ratings" straight after a successful vote — indistinguishable from the write having
	// failed, and the exact thing that made people rate the same work twice.
	//
	// Refetch AFTER the write resolves (not optimistically) so the number shown is the
	// server's own aggregate. A failed write leaves the page untouched rather than
	// inventing a count.
	//
	// GUARDED BY A GENERATION COUNTER, not just by the slug. Rating is a row of ten stars:
	// tapping 7 then 9 is one gesture, and those two refetches race each other on the SAME
	// slug — the slug pin (which only defends against an A→B navigation) would happily let
	// the slower 7 response land last and print the count from before the 9. Each tap takes
	// the next generation and only the LATEST one is allowed to write `override`; the
	// stale ones resolve into nothing. `getSeries` is the whole series payload, so this
	// also means a burst of taps repaints once, at the end, instead of flickering through
	// every intermediate answer. (There is no rating-only query to ask instead:
	// `canonicalSeries` is not narrower in kind and 404s for the federation-only works
	// this fix matters most for, and inventing a resolver for one label is not worth it.)
	//
	// Shared by rating and CLEARING, which race each other just as readily as two taps do
	// (rate 9, change your mind, hit Clear) and must not be able to land out of order.
	// Returns whether the write itself succeeded, so a caller that moved local state
	// optimistically can put it back.
	let rateGen = 0;
	async function applyRatingChange(write: () => Promise<unknown>): Promise<boolean> {
		const slug = data.slug;
		const gen = ++rateGen;
		try {
			await write();
		} catch {
			return false;
		}
		const r = await getSeries(slug).catch(() => null);
		// A newer tap (or a navigation) now owns what this label says.
		if (gen !== rateGen || data.slug !== slug) return true;
		if (r?.view) override = r;
		return true;
	}
	async function onRate(v: number): Promise<void> {
		if (needsAuth) return;
		await applyRatingChange(() => saveSeriesRating(seriesId, socialKey, v, myBody));
	}
	// Clearing withdraws the vote for real (`deleteReview`), so the community count drops
	// with it. The stars are local state and move immediately — the count they sit next to
	// cannot, since only the server knows the new average — and they go back if the write
	// fails, rather than showing an unrated widget over a rating that is still counted.
	//
	// `deleteReview` DELETES THE ROW (a review with no score can't exist — `score` is NOT
	// NULL), so the button is hidden when the viewer's review carries text; see the markup.
	async function clearRating(): Promise<void> {
		if (needsAuth) return;
		const previous = userRating;
		userRating = 0;
		const ok = await applyRatingChange(() => clearSeriesRating(seriesId, socialKey));
		if (!ok) userRating = previous;
	}

	const chapters = $derived.by(() => {
		const chs = view?.chapters ?? [];
		// The data layer delivers chapters ascending (oldest → newest), but sort
		// explicitly by chapter number so display order never depends on a source's
		// arrival order (some sources return newest-first). "Newest" = descending.
		const asc = [...chs].sort((a, b) => a.n - b.n);
		return sort === 'newest' ? asc.reverse() : asc;
	});

	// The sources available for this work. The chapter list below is the cross-source
	// union, one row per chapter number; the DEFAULT source is the one each row opens from
	// whenever it carries that number (another source fills the numbers it lacks). It is
	// picked under "More info" here and in the reader's menu, and kept on this device.
	const translators = $derived(view?.translators ?? []);
	const workId = $derived(view?.workId ?? null);
	// The pick shows at once; the rows follow when the refetch lands.
	let sourcePending = $state<string | null>(null);
	const defaultSourceKey = $derived(sourcePending ?? view?.selectedTranslatorKey ?? null);
	async function selectTranslator(key: string): Promise<void> {
		if (!workId || key === defaultSourceKey) return;
		setPreferredTranslator(workId, key);
		sourcePending = key;
		// Pin the slug so a mid-flight navigation can't be clobbered; a newer pick owns
		// `sourcePending`, so an older answer landing late is dropped.
		const slug = data.slug;
		try {
			const r = await getSeries(workId);
			if (data.slug === slug && sourcePending === key && r.view) override = r;
		} finally {
			if (sourcePending === key) sourcePending = null;
		}
	}

	// Chapter rows whose other copies (other groups and sources) are shown.
	let openCopies = $state<Record<string, boolean>>({});

	// --- admin: fold duplicate works into this one ---
	//
	// GATED THREE WAYS, and all three are needed. `auth.ready` because the token is read
	// from localStorage on the client, so `auth.user` is null both when signed out AND
	// during SSR//first paint — without it the button flashes for nobody and is absent for
	// admins. `isAdmin` is the actual permission (the server re-checks it in
	// `require_admin`; this only decides whether to render the affordance). `mergeWorks` /
	// `searchForMerge` are OPTIONAL backend methods — the Suwayomi adapter and the native
	// offline backend don't implement them, and calling `!` on an absent method would throw
	// on click rather than simply not offering it.
	//
	// `workId` (not `seriesId`) is what the mutation takes: `seriesId` is the reader id,
	// which is numeric for a Suwayomi-anchored work. It resolves a beat after the page
	// does, which is why the button disables rather than hides while it's null — hiding
	// would make it look like the permission check failed.
	const canMerge = $derived(
		auth.ready && !!auth.user?.isAdmin && !!backend.mergeWorks && !!backend.searchForMerge,
	);
	let mergeOpen = $state(false);

	// A merge changes this work's sources, aliases and chapter list, so re-read the page
	// rather than patching state by hand. By `workId` for the same reason
	// `selectTranslator` uses it.
	async function afterMerge(): Promise<void> {
		if (!workId) return;
		const r = await getSeries(workId);
		if (r.view) override = r;
	}

	// --- admin: detach sources that were folded in by mistake ---
	//
	// Gated the same three ways as Merge, and for the same reasons; only the optional
	// backend methods differ. This is NOT an undo for the button above it — see the
	// header of UnmergeDialog.svelte — so it is offered independently of `canMerge`: a
	// wrongly-merged work is worth splitting whether or not this backend can merge.
	const canSplit = $derived(
		auth.ready && !!auth.user?.isAdmin && !!backend.workSourceRows && !!backend.splitSourceSeries,
	);
	let splitOpen = $state(false);

	// Available even when the current image loaded: admins can replace a damaged
	// or outdated stored cover through the same repair action as the admin console.
	const canFetchCover = $derived(auth.ready && !!auth.user?.isAdmin && !!backend.retryCover);
	let coverBusy = $state(false);
	let coverError = $state('');
	let fetchedCover = $state('');
	const heroCover = $derived(fetchedCover || detail?.cover || '');
	let coverGeneration = 0;
	$effect(() => {
		data.slug;
		workId;
		coverBusy = false;
		coverError = '';
		fetchedCover = '';
		return () => {
			coverGeneration += 1;
		};
	});

	async function fetchAdminCover(): Promise<void> {
		if (!canFetchCover || !workId || coverBusy) return;
		const id = workId;
		const generation = coverGeneration;
		coverBusy = true;
		coverError = '';
		try {
			const stored = await backend.retryCover!(id);
			if (generation !== coverGeneration) return;
			if (!stored) throw new Error('Could not fetch a source cover. Try again later.');
			// retryCover stores this work's processed image on the API origin. Use it
			// directly, including for numeric source pages whose metadata has an old URL.
			fetchedCover = retryCoverUrl(
				`${apiOrigin}/covers/${encodeURIComponent(id)}.webp`,
				`${Date.now()}-${Math.random().toString(36).slice(2)}`,
				[apiOrigin, config.imgWorkerBaseUrl].filter(Boolean),
			);
		} catch (error) {
			if (generation === coverGeneration)
				coverError = error instanceof Error ? error.message : 'Could not fetch the cover.';
		} finally {
			if (generation === coverGeneration) coverBusy = false;
		}
	}

	// Go to what was just created. Confirming the detached run landed where the admin
	// meant it to is the whole point of the action, and the new work's page is the only
	// place that shows it.
	//
	// `newReaderId`, never `newWorkId`: a work with no MangaDex anchor is addressed by
	// the numeric Suwayomi source key (0069), and `w_…` 404s for it.
	//
	// invalidateAll because the target URL can be THIS one. Detaching the mapping that
	// gave this work its numeric reader id hands that id to the new work, so
	// `/series/{id}` now resolves to the other side of the split — and a plain goto to
	// an unchanged URL would leave the pre-split page on screen. Dropping `override`
	// first stops the stale local refetch from outliving the navigation.
	async function afterSplit(r: { newReaderId: string }): Promise<void> {
		override = undefined;
		await goto(`/series/${r.newReaderId}`, { invalidateAll: true });
	}

	// --- admin: correct a mis-derived NSFW rating ---
	//
	// The catalogue DERIVES `isNsfw` from crawled genres and source flags, and it gets
	// some works wrong — a safe series flagged adult disappears for every reader who
	// hasn't opted in. An admin reading one can correct it here instead of leaving for
	// the console. Gated the same three ways as Merge and Split, for the same reasons;
	// only the optional backend methods differ.
	const canRate = $derived(
		auth.ready &&
			!!auth.user?.isAdmin &&
			!!backend.seriesAdminMeta &&
			!!backend.updateSeriesMetadata,
	);

	// Three states, matching the server's three-valued override:
	//   auto — nothing pinned; the crawler's derived value wins
	//   sfw  — pinned safe
	//   nsfw — pinned adult (hidden from viewers who haven't enabled Show NSFW)
	// `undefined` = we could not READ the override: not catalogued, or the backend
	// refused. Unlike Merge/Split — which disable rather than hide while `workId`
	// resolves, so a missing button never reads as a failed permission check — this one
	// HIDES, because `undefined` means the underlying mutation would be rejected
	// outright. A disabled three-way control explains nothing; absence is honest.
	let nsfwOverride = $state<boolean | null | undefined>(undefined);
	// The effective flag AFTER the override applies — what viewers actually get. Seeded
	// from the loaded series and re-read from the mutation, because clearing a pin can
	// land on either value and only the server knows which.
	let nsfwEffective = $state<boolean | null>(null);
	let nsfwBusy = $state(false);
	let nsfwError = $state('');

	const nsfwChoice = $derived<'auto' | 'sfw' | 'nsfw'>(
		nsfwOverride == null ? 'auto' : nsfwOverride ? 'nsfw' : 'sfw',
	);

	// Read the pinned override whenever an admin lands on a different series.
	//
	// Two guards, for two different hazards:
	//  • `cancelled` — a fast A→B navigation must not let A's response populate B's
	//    control (the same hazard the streaming and social effects guard against).
	//  • `lastNsfwSeries` — this effect ALSO re-runs when the viewer's admin status
	//    resolves (`auth.ready` flips after hydration), which is not a series change.
	//    Resetting unconditionally there would discard a just-saved `nsfwEffective` and
	//    restore the pre-edit `detail.isNsfw` the page still holds. So the reset is
	//    keyed on the series id actually changing — deliberately a plain variable, not
	//    `$state`, so writing it cannot re-trigger the effect.
	let lastNsfwSeries = '';
	$effect(() => {
		const id = seriesId;
		const allowed = canRate;
		let cancelled = false;
		if (id !== lastNsfwSeries) {
			lastNsfwSeries = id;
			nsfwOverride = undefined;
			nsfwError = '';
			// Untracked: the effect must not re-run (and reset) every time the loaded
			// series object is replaced — only when the series itself changes.
			nsfwEffective = untrack(() => detail?.isNsfw ?? false);
		}
		if (!id || !allowed) return;
		void getSeriesNsfwOverride(id).then((ov) => {
			if (!cancelled) nsfwOverride = ov;
		});
		return () => {
			cancelled = true;
		};
	});

	async function chooseNsfw(choice: 'auto' | 'sfw' | 'nsfw'): Promise<void> {
		if (nsfwBusy || choice === nsfwChoice) return;
		const next = choice === 'auto' ? null : choice === 'nsfw';
		const prev = nsfwOverride ?? null;
		// Pinned before the await: if the admin navigates away while the mutation is in
		// flight, this write belongs to the series they LEFT, and applying its result
		// would mislabel the one now on screen. The mutation still lands server-side —
		// only the UI write is dropped.
		const forSeries = seriesId;
		nsfwBusy = true;
		nsfwError = '';
		// Optimistic, like every other write on this page: show the choice at once and
		// roll back to `prev` if the server refuses.
		nsfwOverride = next;
		try {
			const r = await setSeriesNsfw(forSeries, next, prev);
			if (seriesId !== forSeries) return;
			nsfwOverride = r.value.override;
			if (r.ok) {
				// Only the server can say what clearing a pin fell back to.
				if (r.value.effective !== null) nsfwEffective = r.value.effective;
			} else {
				nsfwError = r.error ?? '';
			}
		} finally {
			// Runs even on the early return above; by then the control belongs to the new
			// series and must not be left disabled.
			nsfwBusy = false;
		}
	}

	// Credits (S2 enrichment): dedupe by name, collecting roles; fall back to the
	// single author/artist line when the work carries no structured credits.
	const ROLE_LABEL: Record<string, string> = { author: 'Story', artist: 'Art' };
	const creditList = $derived.by(() => {
		const raw = seriesDetail?.credits ?? [];
		if (raw.length) {
			const byName = new Map<string, Set<string>>();
			for (const c of raw) {
				if (!byName.has(c.name)) byName.set(c.name, new Set());
				byName.get(c.name)!.add(c.role);
			}
			return [...byName.entries()].map(([name, roles]) => ({
				name,
				label:
					roles.has('author') && roles.has('artist')
						? 'Story & Art'
						: [...roles].map((r) => ROLE_LABEL[r] ?? r).join(' · '),
			}));
		}
		const names = [seriesDetail?.author, seriesDetail?.artist].filter(
			(n): n is string => !!n && n.length > 0,
		);
		return [...new Set(names)].map((name) => ({ name, label: '' }));
	});

	// Localized descriptions (S2): default to the app-language pick; let the reader
	// switch language when the work carries more than one.
	const descriptions = $derived(seriesDetail?.descriptions ?? []);
	let descLangSel = $state<string | null>(null);
	$effect(() => {
		seriesId; // reset the language choice when navigating between series
		untrack(() => (descLangSel = null));
	});
	const synopsisText = $derived.by(() => {
		if (descLangSel) {
			const d = descriptions.find((x) => x.lang === descLangSel);
			if (d) return d.description;
		}
		return seriesDetail?.synopsis ?? '';
	});
	function langName(code: string): string {
		try {
			// `navigator` is absent during SSR — fall back to 'en' so the server and
			// first client render agree (avoids a hydration mismatch on the label).
			const locale = typeof navigator !== 'undefined' ? navigator.language || 'en' : 'en';
			const dn = new Intl.DisplayNames([locale], { type: 'language' });
			return dn.of(code) ?? code.toUpperCase();
		} catch {
			return code.toUpperCase();
		}
	}

	// Cover gallery (F2): the full MangaDex cover set; the hero keeps the primary.
	const covers = $derived(seriesDetail?.covers ?? []);
	let activeCoverIdx = $state<number | null>(null);
	const activeCover = $derived(activeCoverIdx != null ? (covers[activeCoverIdx] ?? null) : null);
	function coverLabel(c: {
		volume: string | null;
		lang: string | null;
		isPrimary: boolean;
	}): string {
		const parts: string[] = [];
		if (c.volume) parts.push(`Vol. ${c.volume}`);
		if (c.lang) parts.push(c.lang.toUpperCase());
		if (c.isPrimary) parts.push('Primary');
		return parts.length ? parts.join(' · ') : 'Cover';
	}
	function openCover(i: number): void {
		activeCoverIdx = i;
	}
	function closeCover(): void {
		activeCoverIdx = null;
	}
	// Close the lightbox on Escape.
	$effect(() => {
		if (activeCoverIdx == null) return;
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') closeCover();
		};
		window.addEventListener('keydown', onKey);
		return () => window.removeEventListener('keydown', onKey);
	});

	const ratingLabel = $derived(
		needsAuth
			? 'Sign in to rate this series'
			: userRating > 0
				? `You rated ${userRating} / 10 · tap a star to update`
				: 'Tap a star to rate — out of 10',
	);

	// ---- link previews / SEO --------------------------------------------------
	// The root layout emits site-wide defaults ("komiq" / "Read manga, together")
	// and steps aside when a route declares `ownsMeta` in its `load` — otherwise
	// both sets would end up in <head> and a crawler would read whichever came
	// first. These are $derived (not effects) so they render during SSR, which is
	// the only run a crawler or a chat link-unfurler ever sees.
	const metaTitle = $derived(title ? `${title} · komiq` : 'komiq');
	const metaDescription = $derived.by(() => {
		const raw = (detail?.synopsis ?? '').replace(/\s+/g, ' ').trim();
		if (!raw) {
			return title
				? `Read ${title} on komiq — chapters, ratings and discussion.`
				: 'Read manga, together. Track your library, follow updates, and discuss chapters.';
		}
		return raw.length > 200 ? `${raw.slice(0, 197).trimEnd()}…` : raw;
	});
	// The cover goes through the same resolver as the poster, so the preview image
	// is the proxied/cached absolute URL rather than an upstream host that blocks
	// hotlinking. Falls back to the site card (what the layout would have emitted)
	// when there's no cover to show — native has no sync resolver, and a not-found
	// slug has no detail at all.
	const metaCover = $derived(
		detail?.cover && images.resolveCoverSync ? images.resolveCoverSync(detail.cover) : '',
	);
	const ogImage = $derived(metaCover || '/og-image.png');
	// The page's canonical location: the id the BACKEND resolved (after any
	// `work_redirect`), not necessarily the one in the URL. A crawler landing on a
	// merged-away id never runs the client-side heal above, so without this the
	// retired URL advertises ITSELF and two URLs compete as duplicates of one work.
	const canonicalHref = $derived.by(() => {
		const id = detail?.id;
		if (!id || !id.startsWith('w_') || page.url.pathname === `/series/${id}`) return page.url.href;
		const u = new URL(page.url);
		u.pathname = `/series/${id}`;
		return u.href;
	});
	// ---- page layout state ------------------------------------------------------
	// Tabs split the long single column into Chapters / Discussion / Covers / Related.
	// Chapters is the default and the only one rendered during SSR, so crawlers and
	// link previews still get the chapter list; the others mount on demand (the
	// discussion thread doesn't load until someone opens it).
	type Tab = 'chapters' | 'discussion' | 'covers' | 'related';
	let tab = $state<Tab>('chapters');
	let aboutOpen = $state(false);
	let rateOpen = $state(false);
	let sortOpen = $state(false);
	$effect(() => {
		seriesId; // a different series starts fresh
		untrack(() => {
			// `#discussion` opens straight onto the thread — the profile's "My comments"
			// links here — and brings the tab strip into view.
			const toDiscussion = page.url.hash === '#discussion';
			tab = toDiscussion ? 'discussion' : 'chapters';
			if (toDiscussion) {
				requestAnimationFrame(() =>
					document.getElementById('chapters')?.scrollIntoView({ block: 'start' }),
				);
			}
			aboutOpen = false;
			rateOpen = false;
			sortOpen = false;
		});
	});
	// The rating popover closes on an outside press or Escape.
	$effect(() => {
		if (!rateOpen) return;
		const onDown = (e: PointerEvent) => {
			if (!(e.target as Element | null)?.closest?.('.rate-wrap')) rateOpen = false;
		};
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') rateOpen = false;
		};
		window.addEventListener('pointerdown', onDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('pointerdown', onDown);
			window.removeEventListener('keydown', onKey);
		};
	});

	// ---- chapter sort menu ------------------------------------------------------
	// A menu-button: the options are REAL links (`orderHref`) rendered even while closed
	// (`hidden`), so the order is crawlable and works as plain navigation; the script
	// only adds the open/close + arrow-key behaviour of an ARIA menu on top.
	let sortBtn = $state<HTMLButtonElement | null>(null);
	let sortMenu = $state<HTMLElement | null>(null);
	function sortItems(): HTMLElement[] {
		return [...(sortMenu?.querySelectorAll<HTMLElement>('[role="menuitemradio"]') ?? [])];
	}
	// Open and move focus into the menu: the checked option by default, or the
	// first / last one when opened with ArrowDown / ArrowUp.
	async function openSort(focus: 'checked' | 'first' | 'last' = 'checked') {
		sortOpen = true;
		await tick();
		const items = sortItems();
		const target =
			focus === 'first'
				? items[0]
				: focus === 'last'
					? items.at(-1)
					: (items.find((el) => el.getAttribute('aria-checked') === 'true') ?? items[0]);
		target?.focus();
	}
	function closeSort(returnFocus: boolean) {
		sortOpen = false;
		if (returnFocus) sortBtn?.focus();
	}
	// Choosing an order is still a real navigation, just one that keeps focus on the
	// sort button (a plain link click would drop it on <body>). Modified clicks
	// (new tab/window) are left to the browser.
	async function pickSort(e: MouseEvent, href: string) {
		if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
		e.preventDefault();
		closeSort(false);
		await goto(href, { keepFocus: true });
		sortBtn?.focus();
	}
	function onSortBtnKey(e: KeyboardEvent) {
		if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
			e.preventDefault();
			void openSort(e.key === 'ArrowDown' ? 'first' : 'last');
		}
	}
	function onSortMenuKey(e: KeyboardEvent) {
		const items = sortItems();
		const i = items.indexOf(document.activeElement as HTMLElement);
		if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
			e.preventDefault();
			const step = e.key === 'ArrowDown' ? 1 : -1;
			items[(i + step + items.length) % items.length]?.focus();
		} else if (e.key === 'Home' || e.key === 'End') {
			e.preventDefault();
			(e.key === 'Home' ? items[0] : items.at(-1))?.focus();
		} else if (e.key === 'Escape') {
			e.preventDefault();
			e.stopPropagation();
			closeSort(true);
		} else if (e.key === 'Tab') {
			closeSort(false);
		} else if (e.key === ' ' && i >= 0) {
			// Links only activate on Enter natively; menu items take Space too.
			e.preventDefault();
			items[i].click();
		}
	}
	// Outside press closes without stealing focus (the press is moving it anyway).
	$effect(() => {
		if (!sortOpen) return;
		const onDown = (e: PointerEvent) => {
			if (!(e.target as Element | null)?.closest?.('.sort-wrap')) closeSort(false);
		};
		window.addEventListener('pointerdown', onDown);
		return () => window.removeEventListener('pointerdown', onDown);
	});

	// Long-running series carry 1,000+ chapters; render them 100 at a time. The page
	// lives in the URL (`?cpage=`) so it survives Back and can be linked. `load` does
	// not read the URL, so flipping pages never refetches the series.
	const CH_PAGE = 100;
	const chPageCount = $derived(Math.max(1, Math.ceil(chapters.length / CH_PAGE)));
	const chPage = $derived(
		Math.min(chPageCount, Math.max(1, Math.floor(Number(page.url.searchParams.get('cpage'))) || 1)),
	);
	const pagedChapters = $derived(chapters.slice((chPage - 1) * CH_PAGE, chPage * CH_PAGE));
	// "Volume 3" headers where the volume changes between consecutive rows of THIS page, in
	// the reader's chosen sort — see `volumeHeaders` for why adjacency, not regrouping.
	const pagedVolumeHeaders = $derived(volumeHeaders(pagedChapters.map((c) => c.volume)));
	function chPageHref(p: number): string {
		const url = new URL(page.url);
		if (p <= 1) url.searchParams.delete('cpage');
		else url.searchParams.set('cpage', String(p));
		return url.pathname + url.search;
	}

	// Hero tag pills: the curated MangaDex genre/theme tags when the work has them,
	// else the plain genre list. Capped so the hero stays a summary — the full set is
	// under "More info".
	const heroTags = $derived.by(() => {
		const tags = seriesDetail?.metadata?.tags ?? [];
		const names = tags.length
			? [...tags]
					.sort((a, b) => Number(b.group === 'genre') - Number(a.group === 'genre'))
					.map((t) => t.name)
			: (seriesDetail?.genres ?? []);
		return [...new Set(names.filter((n) => n.trim()))].slice(0, 8);
	});
	const altTitle = $derived(seriesDetail?.altTitles?.[0] ?? '');
	const pubYear = $derived(seriesDetail?.metadata?.year ?? null);
	const isRead = $derived(continueCh > 1 || readCount > 0);
</script>

{#snippet srcLogo(iconUrl: string | null, name: string)}
	{#if iconUrl}
		<img
			class="ch-src-logo"
			width="16"
			height="16"
			src={iconUrl}
			alt=""
			loading="lazy"
			decoding="async"
			referrerpolicy="no-referrer"
		/>
	{:else}
		<span class="ch-src-logo ch-src-initial">{name.charAt(0).toUpperCase()}</span>
	{/if}
{/snippet}

<!--
	`load` sets `ownsMeta: true` for this route, which makes the root layout stand
	down from ALL its overridable head tags. So this block must emit a COMPLETE set
	on every state, not just when `detail` is present: gating og:*/twitter:* on
	`detail` left a not-found slug (a merged/renamed/deleted work — routine here,
	the dedup queue folds works continuously) with og:site_name and nothing else,
	because the layout had already stood down. Verified before the fix: GET
	/series/<bad-slug> returned 0 og:title and 0 twitter:card tags, where the same
	URL previously carried the full generic komiq card.

	`metaTitle`/`metaDescription` already fall back to the site defaults, so the
	no-detail case now reproduces exactly what the layout used to emit.
-->
<svelte:head>
	<title>{metaTitle}</title>
	<meta name="description" content={metaDescription} />
	<meta property="og:type" content={detail ? 'book' : 'website'} />
	<meta property="og:title" content={metaTitle} />
	<meta property="og:description" content={metaDescription} />
	<meta property="og:url" content={canonicalHref} />
	<meta property="og:image" content={ogImage} />
	<meta name="twitter:card" content="summary_large_image" />
	<meta name="twitter:title" content={metaTitle} />
	<meta name="twitter:description" content={metaDescription} />
	<meta name="twitter:image" content={ogImage} />
</svelte:head>

{#if loading}
	<!-- LOADING -->
	<div class="banner banner-empty" aria-hidden="true"></div>
	<div class="hero k-gutter">
		<div class="hero-grid">
			<div class="poster k-skeleton"></div>
			<div class="head skel-info">
				<div class="k-skeleton sk-h1"></div>
				<div class="k-skeleton sk-creators"></div>
			</div>
			<div class="actions skel-info">
				<div class="k-skeleton sk-cta"></div>
			</div>
			<div class="meta skel-info">
				<div class="k-skeleton sk-facts"></div>
			</div>
		</div>
	</div>
{:else if detail && seriesDetail}
	<!-- Blurred cover backdrop. Decorative, so hidden from assistive tech. -->
	<div class="banner" aria-hidden="true">
		<div class="banner-img"><Cover src={heroCover} alt="" sizing="card" /></div>
	</div>

	<!-- hero -->
	<section class="hero k-gutter">
		<div class="hero-grid">
			<button
				class="poster"
				onclick={() => (covers.length ? openCover(0) : undefined)}
				disabled={!covers.length}
				aria-label={covers.length ? `View covers for ${title}` : title}
			>
				<Cover src={heroCover} alt={title} priority />
			</button>

			<div class="head">
				<h1>{title}</h1>
				{#if altTitle}<p class="alt-title">{altTitle}</p>{/if}
				<p class="creators">
					{#if creditList.length}
						{#each creditList as c, i (c.name)}
							{#if i > 0}<span class="cr-sep">,</span>{/if}<span class="cr-name">{c.name}</span>
						{/each}
					{:else}
						{[seriesDetail.author, seriesDetail.artist].filter(Boolean).join(', ')}
					{/if}
				</p>
			</div>

			<div class="actions">
				<button class="lib" class:in={inLibrary} disabled={marking} onclick={toggleLibrary}>
					{#if inLibrary}
						<Icon name="check" size={16} strokeWidth={2.4} />In library
					{:else}
						<Icon name="plus" size={16} strokeWidth={2.4} />Add to library
					{/if}
				</button>
				{#if inLibrary}
					<StatusMenu
						status={effectiveShelf}
						onchange={chooseStatus}
						disabled={marking || statusBusy}
						variant="button"
						canComplete={ended}
					/>
				{/if}
				<a class="read-btn" href={readHref}
					><Icon name="book-open" size={16} />{isRead
						? `Continue Ch. ${continueCh}`
						: 'Start reading'}</a
				>
				<div class="rate-wrap">
					<button
						class="sq rate-btn"
						class:on={userRating > 0}
						aria-haspopup="dialog"
						aria-expanded={rateOpen}
						aria-label={userRating > 0 ? `Your rating: ${userRating} of 10` : 'Rate this series'}
						title={userRating > 0 ? `You rated ${userRating}/10` : 'Rate'}
						onclick={() => (rateOpen = !rateOpen)}
					>
						<Icon name="star" size={17} fill={userRating > 0 ? 'var(--k-star)' : 'none'} />
						{#if userRating > 0}<span class="rate-mine">{userRating}</span>{/if}
					</button>
					{#if rateOpen}
						<div class="rate-pop" role="dialog" aria-label="Rate this series">
							<div class="rate-pop-head">
								<span class="rate-pop-title">Your rating</span>
								<span class="rate-pop-val">{userRating > 0 ? `${userRating} / 10` : '—'}</span>
							</div>
							<Stars bind:value={userRating} max={10} size={22} onchange={onRate} />
							<div class="rate-pop-foot">
								<span class="rate-label">{ratingLabel}</span>
								<!-- Only offered when the viewer's review has NO written body.
								     Withdrawing a rating deletes the review row (the server cannot
								     store a scoreless review), so on a body-bearing row this would
								     silently destroy text the reader wrote. -->
								{#if userRating > 0 && !needsAuth && !myBody.trim()}
									<button class="clear k-hit" onclick={clearRating}>Remove</button>
								{/if}
							</div>
						</div>
					{/if}
				</div>
				<button
					class="sq fav"
					class:on={isFavorite}
					disabled={favBusy}
					aria-pressed={isFavorite}
					aria-label={isFavorite ? 'Remove from favourites' : 'Add to favourites'}
					title={isFavorite ? 'Favourited' : 'Add to favourites'}
					onclick={toggleFavorite}
				>
					<Icon name="heart" size={17} fill={isFavorite ? 'currentColor' : 'none'} />
				</button>
				<button
					class="sq share"
					class:copied={shareCopied}
					aria-label={shareCopied ? 'Link copied' : 'Share'}
					title={shareCopied ? 'Link copied' : 'Share'}
					onclick={shareSeries}><Icon name={shareCopied ? 'check' : 'share'} size={17} /></button
				>
				{#if canMerge}
					<button
						class="merge"
						disabled={!workId}
						title={workId ? 'Merge duplicate series into this one' : 'Resolving work…'}
						onclick={() => (mergeOpen = true)}>Merge</button
					>
				{/if}
				{#if canSplit}
					<button
						class="merge"
						disabled={!workId}
						title={workId ? 'Detach sources that belong to a different series' : 'Resolving work…'}
						onclick={() => (splitOpen = true)}>Split</button
					>
				{/if}
				{#if canFetchCover}
					<button
						class="merge"
						disabled={!workId || coverBusy}
						aria-busy={coverBusy}
						title={workId ? 'Fetch and store a fresh source cover' : 'Resolving work…'}
						onclick={fetchAdminCover}>{coverBusy ? 'Fetching…' : 'Fetch cover'}</button
					>
				{/if}
				{#if canRate && nsfwOverride !== undefined}
					<div class="rating" role="group" aria-label="Admin: content rating">
						<span class="rating-tag">RATING</span>
						<div class="rating-seg">
							<button
								class:on={nsfwChoice === 'auto'}
								disabled={nsfwBusy}
								aria-pressed={nsfwChoice === 'auto'}
								title={nsfwChoice === 'auto'
									? `No override — using the crawler's derived rating (currently ${
											nsfwEffective ? 'NSFW' : 'SFW'
										})`
									: 'Clear the pin and fall back to the crawler’s derived rating'}
								onclick={() => chooseNsfw('auto')}>Auto</button
							>
							<button
								class:on={nsfwChoice === 'sfw'}
								disabled={nsfwBusy}
								aria-pressed={nsfwChoice === 'sfw'}
								title="Pin as safe — shows to everyone"
								onclick={() => chooseNsfw('sfw')}>SFW</button
							>
							<button
								class:on={nsfwChoice === 'nsfw'}
								disabled={nsfwBusy}
								aria-pressed={nsfwChoice === 'nsfw'}
								title="Pin as adult — hidden from readers who haven't enabled Show NSFW"
								onclick={() => chooseNsfw('nsfw')}>NSFW</button
							>
						</div>
					</div>
				{/if}
			</div>

			<div class="meta">
				<div class="pills">
					<span class="pill type"><span class="type-flag">{FLAG[type]}</span>{type}</span>
					{#each heroTags as t (t)}<span class="pill">{t}</span>{/each}
				</div>
				<div class="pub">
					<span class="status-dot"></span>
					Publication: {pubYear ? `${pubYear}, ` : ''}{statusLabel}
				</div>
				<div class="stats">
					<span class="stat rating-stat" title={`${seriesDetail.votes} community ratings`}
						><Icon name="star" size={15} fill="var(--k-star)" />{rating}<span class="votes"
							>({seriesDetail.votes})</span
						></span
					>
					<Ratings md={seriesDetail.mdRating} />
					<span class="stat"><Icon name="list" size={15} />{totalCh} ch</span>
					{#if viewsTotal > 0}
						<span class="stat"><Icon name="eye" size={15} />{viewsLabel}</span>
					{/if}
					<!-- Chapter RELEASE time (the same clock the feed cards use), with our
					     detection time on hover when the backend reports it. -->
					<span
						class="stat"
						title={seriesDetail.detected
							? `Newest chapter released ${seriesDetail.updated} · we detected it ${seriesDetail.detected}`
							: `Newest chapter released ${seriesDetail.updated}`}
						>Updated {seriesDetail.updated}</span
					>
				</div>
			</div>
		</div>
		{#if coverError}
			<div class="write-error" role="alert">
				<Icon name="alert" size={15} />
				<span>{coverError}</span>
			</div>
		{:else if canFetchCover && fetchedCover && !coverBusy}
			<p role="status">Cover fetched.</p>
		{/if}
		{#if nsfwError}
			<div class="write-error" role="alert">
				<Icon name="alert" size={15} />
				<span>{nsfwError}</span>
			</div>
		{/if}
		{#if writeError}
			<div class="write-error" role="alert">
				<Icon name="alert" size={15} />
				<span>{writeError}</span>
			</div>
		{/if}
	</section>

	<!-- synopsis + details, collapsed to a preview until "More info" -->
	<section class="about k-gutter">
		<div class="about-box" class:open={aboutOpen} id="series-about">
			{#if descriptions.length > 1}
				<div class="desc-lang">
					<span class="desc-lang-label">Language</span>
					<select bind:value={descLangSel} aria-label="Description language">
						<option value={null}>Default ({(seriesDetail.descLang ?? 'en').toUpperCase()})</option>
						{#each descriptions as d (d.lang)}
							<option value={d.lang}>{langName(d.lang)}</option>
						{/each}
					</select>
				</div>
			{/if}
			{#if synopsisText}<p class="synopsis">{synopsisText}</p>{/if}

			<div class="info-grid">
				{#if creditList.length}
					<div class="info">
						<h3>Creators</h3>
						<div class="chips">
							{#each creditList as c (c.name)}
								<span class="chip"
									>{c.name}{#if c.label}<span class="chip-sub">{c.label}</span>{/if}</span
								>
							{/each}
						</div>
					</div>
				{/if}
				{#if !seriesDetail.metadata?.tags?.length && seriesDetail.genres.length}
					<div class="info">
						<h3>Genres</h3>
						<div class="chips">
							{#each seriesDetail.genres as g (g)}<span class="chip">{g}</span>{/each}
						</div>
					</div>
				{/if}
				{#if translators.length}
					<!-- Where this can be read from, and the default-source picker: chapters
					     open from the pressed source, and a chapter it lacks opens from
					     another one. -->
					<div class="info">
						<h3>Sources</h3>
						<div class="chips" role="group" aria-label="Default source for this series">
							{#each translators as t (t.key)}
								<button
									type="button"
									class="chip src-chip"
									class:on={t.key === defaultSourceKey}
									aria-pressed={t.key === defaultSourceKey}
									title={t.key === defaultSourceKey
										? 'Chapters open from this source'
										: 'Open chapters from this source'}
									onclick={() => selectTranslator(t.key)}
								>
									{#if t.iconUrl}
										<img
											class="src-logo"
											width="18"
											height="18"
											src={t.iconUrl}
											alt=""
											loading="lazy"
											decoding="async"
											referrerpolicy="no-referrer"
										/>
									{:else}
										<span class="src-logo src-initial">{t.name.charAt(0).toUpperCase()}</span>
									{/if}
									{t.name}{#if t.lang}<span class="chip-sub">{t.lang.toUpperCase()}</span>{/if}
									<span class="chip-sub">{t.chapterCount} ch</span>
								</button>
							{/each}
						</div>
					</div>
				{/if}
			</div>

			<div class="about-meta">
				<SeriesMetadata metadata={seriesDetail.metadata} />
			</div>

			{#if seriesDetail.altTitles.length}
				<div class="info alt-block">
					<h3>Alternative titles</h3>
					<ul class="alt-list">
						{#each seriesDetail.altTitles as t (t)}<li>{t}</li>{/each}
					</ul>
				</div>
			{/if}
		</div>
		<button
			class="more k-hit"
			aria-expanded={aboutOpen}
			aria-controls="series-about"
			onclick={() => (aboutOpen = !aboutOpen)}
		>
			{aboutOpen ? 'Show less' : 'More info'}
			<span class="more-icon" class:flip={aboutOpen}><Icon name="chevron-down" size={14} /></span>
		</button>
	</section>

	<!-- tabs -->
	<div class="tabs-wrap k-gutter" id="chapters">
		<div class="tabs" role="tablist" aria-label="Series sections">
			<button
				class="k-hit"
				role="tab"
				id="tab-chapters"
				aria-selected={tab === 'chapters'}
				aria-controls="panel"
				class:on={tab === 'chapters'}
				onclick={() => (tab = 'chapters')}>Chapters<span class="tab-n">{totalCh}</span></button
			>
			<button
				class="k-hit"
				role="tab"
				id="tab-discussion"
				aria-selected={tab === 'discussion'}
				aria-controls="panel"
				class:on={tab === 'discussion'}
				onclick={() => (tab = 'discussion')}>Discussion</button
			>
			{#if covers.length > 1}
				<button
					class="k-hit"
					role="tab"
					id="tab-covers"
					aria-selected={tab === 'covers'}
					aria-controls="panel"
					class:on={tab === 'covers'}
					onclick={() => (tab = 'covers')}>Covers<span class="tab-n">{covers.length}</span></button
				>
			{/if}
			{#if relatedSeries.length}
				<button
					class="k-hit"
					role="tab"
					id="tab-related"
					aria-selected={tab === 'related'}
					aria-controls="panel"
					class:on={tab === 'related'}
					onclick={() => (tab = 'related')}>Related</button
				>
			{/if}
		</div>
	</div>

	<div class="panel k-gutter" id="panel" role="tabpanel" aria-labelledby={`tab-${tab}`}>
		{#if tab === 'chapters'}
			<div class="ch-tools">
				<div class="sort-wrap">
					<button
						class="tool k-hit"
						bind:this={sortBtn}
						aria-haspopup="menu"
						aria-expanded={sortOpen}
						aria-controls="sort-menu"
						aria-label={`Sort chapters: ${sort === 'newest' ? 'Newest first' : 'Oldest first'}`}
						onclick={() => (sortOpen ? closeSort(false) : openSort())}
						onkeydown={onSortBtnKey}
					>
						<Icon name="arrow-up-down" size={15} />
						{sort === 'newest' ? 'Newest first' : 'Oldest first'}
						<span class="sort-icon" class:flip={sortOpen}
							><Icon name="chevron-down" size={15} /></span
						>
					</button>
					<!-- Rendered while closed too (`hidden`) so both orders stay real,
					     crawlable links in the SSR markup. -->
					<div
						class="sort-pop"
						id="sort-menu"
						role="menu"
						tabindex="-1"
						aria-label="Sort chapters"
						hidden={!sortOpen}
						bind:this={sortMenu}
						onkeydown={onSortMenuKey}
					>
						{#each SORTS as o (o.value)}
							<a
								class="sort-opt"
								role="menuitemradio"
								aria-checked={sort === o.value}
								tabindex="-1"
								href={orderHref(o.value)}
								onclick={(e) => pickSort(e, orderHref(o.value))}
							>
								<span class="sort-check"
									>{#if sort === o.value}<Icon
											name="check"
											size={14}
											strokeWidth={2.6}
										/>{/if}</span
								>
								{o.label}
							</a>
						{/each}
					</div>
				</div>
				{#if chapters.length}
					<span class="ch-count"
						>{chapters.length}
						{chapters.length === 1 ? 'chapter' : 'chapters'}{chPageCount > 1
							? ` · page ${chPage} of ${chPageCount}`
							: ''}</span
					>
				{/if}
			</div>

			<!-- EMPTY STATE. MangaDex REMOVES chapters when a series is licensed or
			     claimed, so this is usually a popular series whose chapters live on
			     another source, not a broken one. -->
			{#if chapters.length === 0}
				<div class="ch-empty">
					<div class="ch-empty-title">No chapters from any source yet</div>
					<div class="ch-empty-desc">
						{translators.length
							? 'Nothing is available to read here yet. It may arrive from one of this series’ sources, or be licensed elsewhere.'
							: 'Nothing is available to read here yet. It may arrive from another source, or be licensed elsewhere.'}
					</div>
				</div>
			{/if}

			<ol class="ch-list" id="chapter-list">
				{#each pagedChapters as c, ci (c.id ?? c.n)}
					{#if pagedVolumeHeaders[ci]}
						<li class="ch-vol">{pagedVolumeHeaders[ci]}</li>
					{/if}
					<!-- One card per chapter NUMBER (the cross-source union). No nested
					     interactive elements: the main row links to the default source's copy
					     and names it; the other copies sit in a dropdown, each a sibling link. -->
					{@const multi = (c.sources?.length ?? 0) > 1}
					{@const isRead = isChapterRead(c)}
					{@const rowMatch = c.sources?.find((s) => s.id === c.id && s.src === (c.src ?? null))}
					{@const rowCopy = rowMatch ?? c.sources?.[0]}
					{@const rowKey = c.id ?? String(c.n)}
					{@const copiesOpen = multi && !!openCopies[rowKey]}
					<li class="ch-card" class:read={isRead} class:multi>
						<!-- The read mark is a SIBLING of the row link, never inside it: a button
						     nested in an <a> is invalid and screen readers announce it badly. -->
						<div class="ch-head">
							{#if canMarkRead && c.id}
								<button
									class="ch-mark-btn k-hit"
									aria-pressed={isRead}
									aria-label={isRead
										? `Mark chapter ${c.n} as unread`
										: `Mark chapter ${c.n} as read`}
									title={isRead ? 'Mark as unread' : 'Mark as read'}
									disabled={!!readPending[c.id]}
									onclick={() => toggleChapterRead(c)}
								>
									<span class="ch-mark"
										>{#if isRead}<Icon name="check" size={13} strokeWidth={2.6} />{/if}</span
									>
								</button>
							{:else}
								<span class="ch-mark" title={isRead ? 'Read' : 'Unread'}
									>{#if isRead}<Icon name="check" size={13} strokeWidth={2.6} />{/if}</span
								>
							{/if}
							<a class="ch-main" href={chapterHref(c.id, c.src)}>
								<span class="ch-text">
									<span class="ch-line">
										<span class="ch-num">Ch. {c.n}</span>
										{#if c.title && c.title !== `Chapter ${c.n}`}<span class="ch-name"
												>{c.title}</span
											>{/if}
										{#if c.isNew}<span class="new">NEW</span>{/if}
										<!-- Off-site chapters (~35,000: MangaPlus, Comikey, NamiComi,
									     BiliBili) have no pages for us to serve — badged here so
									     the reader knows before tapping. -->
										{#if c.external}<span class="ext" title="Hosted on another site"
												><Icon name="globe" size={11} />OFF-SITE</span
											>{/if}
									</span>
								</span>
								<!-- The copy this row opens: its source and group. -->
								{#if rowCopy}
									<span class="ch-by" title={chapterAttribution(rowCopy.name, rowCopy.group)}>
										{@render srcLogo(rowCopy.iconUrl, rowCopy.name)}
										<span class="ch-src-name"
											>{chapterAttribution(rowCopy.name, rowCopy.group)}</span
										>
										<!-- The badge is for the copy the row OPENS. `rowCopy` can be
										     another copy's entry: the aggregation does not always list the
										     selected source's own copy. The row's own group and source are
										     read then. -->
										<AiBadge
											group={rowMatch ? rowMatch.group : c.group}
											sourceId={rowMatch ? rowMatch.sourceId : c.sourceId}
										/>
									</span>
								{:else if c.group || hasAiBadge(c.group, c.sourceId)}
									<!-- A single source: its chapters often credit no group, and then
									     the row has this column only for a tagged source's badge. -->
									<span class="ch-by"
										>{#if c.group}<Icon name="users" size={14} /><span class="ch-src-name"
												>{c.group}</span
											>{/if}<AiBadge group={c.group} sourceId={c.sourceId} /></span
									>
								{/if}
								<!-- TWO CLOCKS, NAMED: a source that holds chapters behind early
							     access publishes days before anyone here can read them. "Found"
							     only renders where the data layer allows it (recent or
							     newest-3); never a placeholder date. -->
								<span class="ch-dates">
									{#if c.date}<span>Released {c.date}</span>{/if}
									{#if c.foundDate}<span
											class="ch-found"
											title="When we first had this chapter available to read"
											>Found {c.foundDate}</span
										>{/if}
								</span>
								<!-- Same width on every row, so the columns line up: the arrow, or
								     the room the versions button below sits in. -->
								<span class="ch-tail"
									>{#if !multi}<Icon name="chevron-right" size={16} />{/if}</span
								>
							</a>
							{#if multi && c.sources}
								<button
									type="button"
									class="ch-more k-hit"
									aria-expanded={copiesOpen}
									aria-controls={`ch-copies-${ci}`}
									aria-label={`Chapter ${c.n}: ${copiesOpen ? 'hide' : 'show'} all ${c.sources.length} versions`}
									title="Other groups and sources"
									onclick={() => (openCopies[rowKey] = !copiesOpen)}
								>
									+{c.sources.length - 1}
									<span class="more-icon" class:flip={copiesOpen}
										><Icon name="chevron-down" size={14} /></span
									>
								</button>
							{/if}
						</div>
						{#if multi && c.sources}
							<!-- One sub-row per (source, group) copy, each a sibling link to
							     exactly that upload. Rendered while closed too (`hidden`), so the
							     button always has a list to control. -->
							<ul
								class="ch-copies"
								id={`ch-copies-${ci}`}
								aria-label={`Chapter ${c.n} versions`}
								hidden={!copiesOpen}
							>
								{#each c.sources as s, i (`${s.src ?? 'spine'}:${s.id ?? i}`)}
									<li>
										<a
											class="ch-copy"
											class:on={s === rowCopy}
											href={chapterHref(s.id, s.src)}
											aria-label={`Read Chapter ${c.n} on ${chapterAttribution(s.name, s.group)}${aiSpoken(s.group, s.sourceId)}`}
										>
											{@render srcLogo(s.iconUrl, s.name)}
											<span class="cp-text">
												<span class="cp-group">{chapterAttribution(s.name, s.group)}</span>
												<AiBadge group={s.group} sourceId={s.sourceId} />
											</span>
											{#if s.date}<span class="cp-date">{s.date}</span>{/if}
										</a>
									</li>
								{/each}
							</ul>
						{/if}
					</li>
				{/each}
			</ol>

			{#if chPageCount > 1}
				<Pager
					page={chPage}
					hasNext={chPage < chPageCount}
					total={chapters.length}
					pageSize={CH_PAGE}
					count={pagedChapters.length}
					href={chPageHref}
					label="Chapter pages"
					controls="chapter-list"
					scrollTarget="chapters"
				/>
			{/if}
		{:else if tab === 'discussion'}
			<!-- The single unified comment engine (replies + image attachments). The star
			     rating in the hero is the series' score; written discussion lives here. -->
			{#if seriesId}
				<CommentThread
					targetType="series"
					targetId={seriesId}
					storageKey={socialKey}
					prompt="Share your thoughts on this series…"
				/>
			{/if}
		{:else if tab === 'covers'}
			<div class="covers-grid">
				{#each covers as c, i (c.url)}
					<button class="cover-cell" onclick={() => openCover(i)} title={coverLabel(c)}>
						<div class="cover-thumb">
							<Cover src={c.thumbnailUrl} alt={coverLabel(c)} loading="lazy" sizing="card" />
							{#if c.isPrimary}<span class="cover-primary">Primary</span>{/if}
						</div>
						{#if c.volume || c.lang}
							<span class="cover-meta">
								{#if c.volume}<span class="cover-vol">Vol. {c.volume}</span>{/if}
								{#if c.lang}<span class="cover-lang">{c.lang.toUpperCase()}</span>{/if}
							</span>
						{/if}
					</button>
				{/each}
			</div>
		{:else if tab === 'related'}
			<h2 class="sr-only">Readers also enjoyed</h2>
			<div class="related-grid">
				{#each relatedSeries as item (item.id ?? item.title)}
					<MangaCard
						title={item.title}
						sub={`${item.genre} · ${item.ch} ch`}
						rating={item.rating}
						mdRating={item.mdRating}
						cover={item.cover}
						id={item.id}
					/>
				{/each}
			</div>
		{/if}
	</div>
{:else if loadError}
	<!-- LOAD ERROR (backend outage) — distinct from a genuinely missing series -->
	<div class="not-found k-gutter">
		<div class="nf-icon"><Icon name="alert" size={28} /></div>
		<h1>Couldn’t load this series</h1>
		<p>Something went wrong reaching the server. Check your connection and try again.</p>
		<div class="nf-btns">
			<button class="nf-primary" onclick={retrySeries}>Retry</button>
			<a class="nf-ghost" href="/browse">Browse series</a>
		</div>
	</div>
{:else}
	<!-- NOT FOUND -->
	<div class="not-found k-gutter">
		<div class="nf-icon"><Icon name="alert" size={28} /></div>
		<h1>Series not found</h1>
		<p>We couldn’t load this series. It may have been removed, or the link is broken.</p>
		<div class="nf-btns">
			<a class="nf-primary" href="/browse">Browse series</a>
			<a class="nf-ghost" href="/">Go home</a>
		</div>
	</div>
{/if}

<!-- admin merge picker. Mounted only for an admin with a resolved work, so a normal
     reader never ships its markup or its search handler. -->
{#if canMerge && workId}
	<MergeDialog
		bind:open={mergeOpen}
		targetWorkId={workId}
		targetTitle={title}
		onmerged={afterMerge}
	/>
{/if}

<!-- admin source splitter. Same mount conditions as the merge picker. -->
{#if canSplit && workId}
	<UnmergeDialog bind:open={splitOpen} {workId} workTitle={title} onsplit={afterSplit} />
{/if}

<!-- cover lightbox (F2) -->
{#if activeCover}
	<div
		class="lightbox"
		use:modalFocus
		role="dialog"
		aria-modal="true"
		aria-label={coverLabel(activeCover)}
		tabindex="-1"
	>
		<button class="lb-scrim" aria-label="Close cover" onclick={closeCover}></button>
		<div class="lb-inner">
			<div class="lb-image">
				<Cover src={activeCover.url} alt={coverLabel(activeCover)} fit="contain" />
			</div>
			<div class="lb-bar">
				<span class="lb-label">{coverLabel(activeCover)}</span>
				<button class="lb-close" onclick={closeCover} aria-label="Close"
					><Icon name="x" size={18} /></button
				>
			</div>
		</div>
	</div>
{/if}

<style>
	/* ---- banner ---- */
	.banner {
		position: absolute;
		top: var(--k-header-h);
		left: 0;
		right: 0;
		height: 300px;
		overflow: hidden;
		z-index: 0;
		pointer-events: none;
	}
	.banner-img {
		position: absolute;
		inset: -40px;
		filter: blur(28px) saturate(1.15);
		opacity: 0.55;
		transform: scale(1.08);
	}
	.banner::after {
		content: '';
		position: absolute;
		inset: 0;
		background: linear-gradient(
			180deg,
			color-mix(in srgb, var(--k-bg) 35%, transparent) 0%,
			color-mix(in srgb, var(--k-bg) 70%, transparent) 55%,
			var(--k-bg) 100%
		);
	}
	.banner-empty {
		background: var(--k-surface-2);
	}

	/* ---- hero ---- */
	/* Above the sections that follow (they are z-index 1 too, and later in the page):
	   the rating popover and the shelf menu open out of the hero and, on a phone, land
	   over the description, which otherwise painted through them. */
	.hero {
		position: relative;
		z-index: 2;
		padding-top: 40px;
	}
	.hero-grid {
		display: grid;
		grid-template-columns: 210px minmax(0, 1fr);
		grid-template-areas:
			'poster head'
			'poster actions'
			'poster meta';
		grid-template-rows: auto auto 1fr;
		column-gap: 32px;
		row-gap: 18px;
	}
	.poster {
		grid-area: poster;
		position: relative;
		width: 210px;
		aspect-ratio: 2 / 3;
		align-self: start;
		padding: 0;
		border-radius: var(--k-radius-lg);
		overflow: hidden;
		border: 1px solid var(--k-border-2);
		background: var(--k-cover);
		box-shadow: 0 22px 50px rgba(0, 0, 0, 0.45);
		cursor: zoom-in;
	}
	.poster:disabled {
		cursor: default;
	}
	.head {
		grid-area: head;
		min-width: 0;
		align-self: end;
		padding-top: 24px;
	}
	h1 {
		margin: 0;
		font-size: clamp(30px, 4vw, 52px);
		line-height: 1.05;
		color: var(--k-text-bright);
		overflow-wrap: anywhere;
		-webkit-hyphens: auto;
		hyphens: auto;
	}
	.alt-title {
		margin: 8px 0 0;
		font-size: 17px;
		font-weight: 600;
		color: var(--k-text-2);
		line-height: 1.35;
	}
	.creators {
		margin: 10px 0 0;
		font-size: 14px;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.cr-sep {
		margin-right: 6px;
	}
	.actions {
		grid-area: actions;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
	}
	.lib,
	.read-btn {
		height: 46px;
		padding: 0 22px;
		border-radius: var(--k-radius-md);
		display: inline-flex;
		align-items: center;
		gap: 9px;
		font-weight: 700;
		font-size: 14.5px;
		cursor: pointer;
		text-decoration: none;
		white-space: nowrap;
	}
	.lib {
		border: none;
		background: var(--k-accent);
		color: #fff;
		transition: filter 0.15s;
	}
	.lib:hover:not(:disabled) {
		filter: brightness(1.08);
	}
	.lib.in {
		background: rgba(95, 191, 126, 0.14);
		box-shadow: inset 0 0 0 1px rgba(95, 191, 126, 0.5);
		color: var(--k-ongoing);
	}
	.read-btn {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.read-btn:hover {
		background: var(--k-primary-hover);
	}
	.actions :global(.sm-btn),
	.actions :global(button.sm-trigger) {
		height: 46px;
	}
	.sq {
		position: relative;
		height: 46px;
		min-width: 46px;
		padding: 0 12px;
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 6px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-3);
		color: var(--k-text-3);
		cursor: pointer;
		transition:
			color 0.15s,
			background-color 0.15s,
			border-color 0.15s;
	}
	.sq:hover:not(:disabled) {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.sq:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.fav.on {
		background: rgba(233, 110, 110, 0.12);
		border-color: rgba(233, 110, 110, 0.5);
		color: #e96e6e;
	}
	.share.copied {
		border-color: rgba(95, 191, 126, 0.5);
		color: var(--k-ongoing);
	}
	.rate-btn.on {
		border-color: rgba(246, 183, 60, 0.5);
		color: var(--k-text);
	}
	.rate-mine {
		font-weight: 800;
		font-size: 14px;
	}
	.rate-wrap {
		position: relative;
	}
	.rate-pop {
		position: absolute;
		top: calc(100% + 8px);
		left: 50%;
		transform: translateX(-50%);
		z-index: 30;
		width: max-content;
		max-width: calc(100vw - 32px);
		padding: 14px 16px;
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		box-shadow: 0 22px 60px rgba(0, 0, 0, 0.5);
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.rate-pop-head,
	.rate-pop-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
	}
	.rate-pop-title {
		font-weight: 700;
		font-size: 13.5px;
		color: var(--k-text);
	}
	.rate-pop-val {
		font-weight: 800;
		font-size: 13.5px;
		color: var(--k-star);
	}
	.rate-label {
		font-size: 12px;
		color: var(--k-text-dim);
	}
	.clear {
		border: none;
		background: none;
		padding: 0;
		font: inherit;
		font-size: 12px;
		font-weight: 700;
		color: var(--k-cancelled);
		cursor: pointer;
	}
	.merge {
		height: 46px;
		padding: 0 16px;
		flex: 0 0 auto;
		border-radius: var(--k-radius-md);
		background: transparent;
		border: 1px dashed var(--k-border-4);
		color: var(--k-text-3);
		font: inherit;
		font-weight: 600;
		cursor: pointer;
	}
	.merge:hover:not(:disabled) {
		border-color: var(--k-border-strong);
		color: var(--k-text-1);
	}
	.merge:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.rating {
		display: flex;
		align-items: center;
		gap: 8px;
		height: 46px;
		flex: 0 0 auto;
		padding: 0 10px 0 12px;
		border-radius: var(--k-radius-md);
		border: 1px dashed var(--k-border-4);
	}
	.rating-tag {
		font-size: 10px;
		font-weight: 700;
		letter-spacing: 0.08em;
		color: var(--k-text-3);
	}
	.rating-seg {
		display: flex;
		gap: 2px;
	}
	.rating-seg button {
		padding: 6px 10px;
		border: none;
		border-radius: 6px;
		background: transparent;
		color: var(--k-text-3);
		font: inherit;
		font-size: 12px;
		font-weight: 700;
		cursor: pointer;
	}
	.rating-seg button.on {
		background: var(--k-surface);
		color: var(--k-text-1);
	}
	.rating-seg button:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.meta {
		grid-area: meta;
		display: flex;
		flex-direction: column;
		gap: 12px;
		min-width: 0;
	}
	.pills {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.pill {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 3px 9px;
		border-radius: 6px;
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		font-size: 11px;
		font-weight: 800;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-text-2);
	}
	.pill.type {
		color: var(--k-text-bright);
		border-color: var(--k-border-4);
	}
	.type-flag {
		font-size: 12px;
	}
	.pub {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 12px;
		font-weight: 800;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-text-2);
	}
	.status-dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--k-dot);
	}
	.stats {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 18px;
	}
	.stat {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 13.5px;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.rating-stat {
		font-weight: 800;
		color: var(--k-star);
	}
	.votes {
		font-weight: 600;
		color: var(--k-text-faint);
	}
	/* MangaDex's rating, next to ours. The row's own flex child with no wrapper, so
	   there is no empty stat left in the row when there is nothing to show. */
	.stats :global(.ratings) {
		font-size: 13.5px;
		color: var(--k-text-2);
	}
	.write-error {
		display: flex;
		align-items: center;
		gap: 8px;
		margin-top: 14px;
		padding: 9px 14px;
		border-radius: var(--k-radius);
		background: rgba(246, 183, 60, 0.1);
		border: 1px solid rgba(246, 183, 60, 0.34);
		color: var(--k-text-2);
		font-size: 13px;
		font-weight: 600;
	}

	/* ---- about (synopsis + details) ---- */
	.about {
		position: relative;
		z-index: 1;
		margin-top: 28px;
	}
	.about-box {
		position: relative;
		max-height: 132px;
		overflow: hidden;
	}
	.about-box:not(.open)::after {
		content: '';
		position: absolute;
		left: 0;
		right: 0;
		bottom: 0;
		height: 64px;
		background: linear-gradient(transparent, var(--k-bg));
		pointer-events: none;
	}
	.about-box.open {
		max-height: none;
	}
	.synopsis {
		margin: 0;
		max-width: 920px;
		font-size: 15px;
		line-height: 1.7;
		color: var(--k-text-2);
		white-space: pre-line;
	}
	.desc-lang {
		display: flex;
		align-items: center;
		gap: 10px;
		margin-bottom: 10px;
	}
	.desc-lang-label {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.09em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.desc-lang select {
		height: 32px;
		padding: 0 10px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text-2);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
	}
	.info-grid {
		display: flex;
		flex-wrap: wrap;
		gap: 22px 36px;
		margin-top: 22px;
	}
	.info h3 {
		margin: 0 0 8px;
		font-size: 14px;
		color: var(--k-text);
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 5px 10px;
		border-radius: 6px;
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-2);
	}
	.chip-sub {
		font-size: 11px;
		font-weight: 700;
		color: var(--k-text-faint);
	}
	/* The source picker: a chip is a button, and the default source has accented edges. */
	.src-chip {
		font: inherit;
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		transition:
			border-color 0.15s,
			background-color 0.15s,
			color 0.15s;
	}
	.src-chip:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.src-chip.on {
		border-color: var(--k-accent);
		box-shadow: 0 0 0 1px var(--k-accent);
		background: var(--k-accent-soft);
		color: var(--k-text-bright);
		cursor: default;
	}
	.src-chip:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.src-logo {
		width: 18px;
		height: 18px;
		border-radius: 4px;
		object-fit: cover;
		background: var(--k-surface-5);
	}
	.src-initial {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		font-size: 10px;
		font-weight: 800;
	}
	.about-meta :global(.metadata) {
		padding-inline: 0;
		margin-top: 22px;
	}
	.alt-block {
		margin-top: 22px;
	}
	.alt-list {
		margin: 0;
		padding: 0;
		list-style: none;
		display: flex;
		flex-direction: column;
		gap: 4px;
		font-size: 13.5px;
		color: var(--k-text-dim);
	}
	.more {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 6px;
		width: 100%;
		margin-top: 6px;
		padding: 6px 0;
		border: none;
		border-top: 1px solid var(--k-border-2);
		background: none;
		color: var(--k-accent);
		font: inherit;
		font-size: 12.5px;
		font-weight: 800;
		cursor: pointer;
	}
	.more-icon,
	.sort-icon {
		display: inline-flex;
		transition: transform 0.18s;
	}
	.more-icon.flip,
	.sort-icon.flip {
		transform: rotate(180deg);
	}

	/* ---- tabs ---- */
	.tabs-wrap {
		margin-top: 30px;
		scroll-margin-top: calc(var(--k-header-h) + 12px);
	}
	.tabs {
		display: inline-flex;
		max-width: 100%;
		overflow-x: auto;
		gap: 4px;
		padding: 4px;
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		scrollbar-width: none;
	}
	.tabs button {
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		gap: 7px;
		height: 36px;
		padding: 0 16px;
		border: none;
		border-radius: var(--k-radius);
		background: transparent;
		color: var(--k-text-dim);
		font: inherit;
		font-size: 14px;
		font-weight: 700;
		cursor: pointer;
	}
	.tabs button:hover {
		color: var(--k-text);
	}
	.tabs button.on {
		background: var(--k-surface-5);
		color: var(--k-text-bright);
		box-shadow: 0 1px 0 var(--k-border-3) inset;
	}
	.tab-n {
		font-size: 11px;
		font-weight: 800;
		color: var(--k-text-faint);
	}
	.panel {
		padding-top: 18px;
		padding-bottom: 56px;
	}

	/* ---- chapters ---- */
	.ch-tools {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		margin-bottom: 12px;
	}
	.sort-wrap {
		position: relative;
		flex: 0 0 auto;
	}
	.tool {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		height: 34px;
		padding: 0 10px 0 12px;
		border-radius: var(--k-radius);
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-2);
		font: inherit;
		font-size: 13px;
		font-weight: 700;
		cursor: pointer;
	}
	.tool:hover,
	.tool[aria-expanded='true'] {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	/* Same surface as the rating popover. Anchored to the button's LEFT edge — the
	   button sits at the start of the toolbar, so the menu always opens inward and
	   can't run off a phone screen. */
	.sort-pop {
		position: absolute;
		top: calc(100% + 6px);
		left: 0;
		z-index: 30;
		min-width: 100%;
		width: max-content;
		max-width: calc(100vw - 32px);
		padding: 5px;
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		box-shadow: 0 22px 60px rgba(0, 0, 0, 0.5);
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.sort-pop[hidden] {
		display: none;
	}
	.sort-opt {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 36px;
		padding: 0 12px 0 8px;
		border-radius: var(--k-radius);
		color: var(--k-text-2);
		font-size: 13px;
		font-weight: 700;
		text-decoration: none;
		white-space: nowrap;
	}
	.sort-opt:hover,
	.sort-opt:focus-visible {
		background: var(--k-hover-fill);
		color: var(--k-text);
	}
	.sort-opt:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: -2px;
	}
	.sort-opt[aria-checked='true'] {
		color: var(--k-text-bright);
	}
	.sort-check {
		width: 16px;
		display: inline-flex;
		justify-content: center;
		color: var(--k-accent);
	}
	.ch-count {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-faint);
	}
	.ch-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	/* Volume header: a quiet label row, not a card — it groups, it is not a chapter. */
	.ch-vol {
		margin: 10px 2px 0;
		font-size: 12px;
		font-weight: 800;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.ch-vol:first-child {
		margin-top: 0;
	}
	.ch-card {
		border-radius: var(--k-radius-md);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		overflow: hidden;
	}
	.ch-card:hover {
		border-color: var(--k-border-3);
	}
	/* Mark + row link side by side: the mark is its own control (see the markup). */
	.ch-head {
		position: relative;
		display: flex;
		align-items: center;
		padding-left: 14px;
	}
	.ch-head:hover {
		background: var(--k-hover-fill);
	}
	.ch-main {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 10px 14px 10px 12px;
		color: var(--k-text-ghost);
		text-decoration: none;
	}
	/* A 32px hit area around the 20px mark; negative margins keep the row's rhythm. */
	.ch-mark-btn {
		flex: 0 0 auto;
		width: 32px;
		height: 32px;
		margin: 0 -6px;
		padding: 0;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border: 0;
		border-radius: 50%;
		background: none;
		cursor: pointer;
	}
	.ch-mark-btn:hover .ch-mark,
	.ch-mark-btn:focus-visible .ch-mark {
		border-color: var(--k-ongoing);
	}
	.ch-mark-btn:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 1px;
	}
	.ch-mark-btn:disabled {
		cursor: progress;
		opacity: 0.6;
	}
	.ch-mark {
		width: 20px;
		height: 20px;
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border-radius: 50%;
		border: 1.5px solid var(--k-border-4);
		color: var(--k-on-primary);
	}
	.read .ch-mark {
		background: var(--k-ongoing);
		border-color: var(--k-ongoing);
		color: #0c0c0d;
	}
	.ch-text {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 3px;
	}
	.ch-line {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
	}
	.ch-num {
		flex: 0 0 auto;
		font-weight: 800;
		font-size: 14px;
		color: var(--k-text-bright);
	}
	.ch-name {
		min-width: 0;
		font-weight: 600;
		font-size: 14px;
		color: var(--k-text-2);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.read .ch-num,
	.read .ch-name {
		color: var(--k-text-faint);
	}
	.new {
		flex: 0 0 auto;
		font-size: 9.5px;
		font-weight: 800;
		letter-spacing: 0.06em;
		padding: 2px 6px;
		border-radius: 4px;
		background: var(--k-accent);
		color: #fff;
	}
	.ext {
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		font-size: 9.5px;
		font-weight: 800;
		letter-spacing: 0.05em;
		color: var(--k-text-faint);
		border: 1px solid var(--k-border-4);
		padding: 1px 6px;
		border-radius: 4px;
	}
	.ch-by {
		flex: 0 0 170px;
		display: inline-flex;
		align-items: center;
		gap: 7px;
		min-width: 0;
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.ch-dates {
		flex: 0 0 150px;
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: 2px;
		font-size: 12px;
		color: var(--k-text-faint);
		text-align: right;
	}
	.ch-found {
		color: var(--k-text-ghost);
	}
	.ch-tail {
		flex: 0 0 40px;
		display: inline-flex;
		justify-content: flex-end;
	}
	/* The versions dropdown button: over the row's tail, a sibling of the row link. */
	.ch-more {
		position: absolute;
		top: 50%;
		right: 8px;
		transform: translateY(-50%);
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 2px;
		min-width: 46px;
		height: 30px;
		padding: 0 6px 0 9px;
		border: 0;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		color: var(--k-text-dim);
		font: inherit;
		font-size: 12px;
		font-weight: 700;
		font-variant-numeric: tabular-nums;
		cursor: pointer;
	}
	.ch-more:hover,
	.ch-more[aria-expanded='true'] {
		background: var(--k-surface-5);
		color: var(--k-text);
	}
	.ch-more:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 1px;
	}
	/* Per-copy sub-rows (a chapter with more than one source/group upload), shown by the
	   versions button. */
	.ch-copies {
		list-style: none;
		margin: 0;
		padding: 0 0 4px;
	}
	.ch-copy {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 36px;
		padding: 6px 14px 6px 46px;
		border-top: 1px solid var(--k-border-1);
		color: var(--k-text-dim);
		font-size: 12.5px;
		text-decoration: none;
	}
	.ch-copy:hover {
		background: var(--k-hover-fill);
		color: var(--k-text);
	}
	.cp-text {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: baseline;
		gap: 8px;
		white-space: nowrap;
	}
	.cp-group {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.cp-group {
		flex: 0 1 auto;
		font-weight: 700;
		color: var(--k-text-2);
	}
	.ch-copy:hover .cp-group {
		color: var(--k-text);
	}
	.cp-date {
		flex: 0 0 auto;
		font-size: 12px;
		color: var(--k-text-faint);
		font-variant-numeric: tabular-nums;
	}
	.read .cp-group {
		color: var(--k-text-faint);
	}
	/* The copy the row itself opens. */
	.ch-copy.on .cp-group {
		color: var(--k-accent);
	}
	.ch-src-logo {
		width: 16px;
		height: 16px;
		flex: 0 0 auto;
		border-radius: 4px;
		object-fit: cover;
	}
	.ch-src-initial {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		font-size: 9px;
		font-weight: 800;
		background: var(--k-surface-5);
	}
	.ch-src-name {
		min-width: 0;
		max-width: 180px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.ch-empty {
		padding: 40px 20px;
		text-align: center;
		border-radius: var(--k-radius-lg);
		border: 1px dashed var(--k-border-3);
	}
	.ch-empty-title {
		font-weight: 700;
		font-size: 15px;
		color: var(--k-text);
	}
	.ch-empty-desc {
		margin: 6px auto 0;
		max-width: 420px;
		font-size: 13.5px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}

	/* ---- covers + related ---- */
	.covers-grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(130px, 1fr));
		gap: 18px 14px;
	}
	.cover-cell {
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 0;
		background: none;
		border: none;
		cursor: pointer;
		text-align: left;
	}
	.cover-thumb {
		position: relative;
		width: 100%;
		aspect-ratio: 2 / 3;
		border-radius: var(--k-radius-md);
		overflow: hidden;
		border: 1px solid var(--k-border-2);
		background: var(--k-surface-2);
	}
	.cover-cell:hover .cover-thumb {
		border-color: var(--k-border-strong);
	}
	.cover-primary {
		position: absolute;
		top: 7px;
		left: 7px;
		z-index: 1;
		font-size: 9.5px;
		font-weight: 800;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-on-primary);
		background: var(--k-primary);
		border-radius: 5px;
		padding: 2px 6px;
	}
	.cover-meta {
		display: flex;
		align-items: center;
		gap: 7px;
		font-size: 12px;
	}
	.cover-vol {
		font-weight: 700;
		color: var(--k-text-2);
	}
	.cover-lang {
		font-size: 9.5px;
		font-weight: 800;
		color: var(--k-text-faint);
		border: 1px solid var(--k-border-3);
		border-radius: 4px;
		padding: 1px 5px;
	}
	.related-grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
		gap: 22px 16px;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}

	/* ---- lightbox ---- */
	.lightbox {
		position: fixed;
		inset: 0;
		z-index: 200;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 40px;
	}
	.lb-scrim {
		position: fixed;
		inset: 0;
		border: none;
		background: rgba(6, 6, 7, 0.86);
		backdrop-filter: blur(6px);
		cursor: zoom-out;
	}
	.lb-inner {
		position: relative;
		z-index: 1;
		display: flex;
		flex-direction: column;
		gap: 14px;
		max-width: min(90vw, 560px);
		max-height: 90vh;
	}
	.lb-image {
		position: relative;
		width: 100%;
		flex: 1 1 auto;
		min-height: 0;
		aspect-ratio: 2 / 3;
		border-radius: var(--k-radius-lg);
		overflow: hidden;
		box-shadow: 0 30px 80px rgba(0, 0, 0, 0.7);
	}
	.lb-bar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 14px;
	}
	.lb-label {
		font-size: 14px;
		font-weight: 700;
		color: #f7f6f3;
	}
	.lb-close {
		width: 40px;
		height: 40px;
		flex: 0 0 auto;
		border-radius: var(--k-radius-md);
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
	}

	/* ---- loading + not found ---- */
	.poster.k-skeleton {
		box-shadow: none;
	}
	.skel-info {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.skel-info .k-skeleton {
		border-radius: var(--k-radius);
	}
	.sk-h1 {
		width: min(460px, 80%);
		height: 48px;
	}
	.sk-creators {
		width: 200px;
		height: 16px;
	}
	.sk-cta {
		width: min(340px, 100%);
		height: 46px;
	}
	.sk-facts {
		width: min(420px, 90%);
		height: 16px;
	}
	.not-found {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		padding: 120px 20px;
		text-align: center;
	}
	.nf-icon {
		width: 64px;
		height: 64px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-2);
		color: var(--k-hiatus);
	}
	.not-found h1 {
		font-size: 34px;
	}
	.not-found p {
		margin: 0;
		max-width: 380px;
		font-size: 14.5px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	.nf-btns {
		display: flex;
		gap: 12px;
		margin-top: 10px;
		flex-wrap: wrap;
		justify-content: center;
	}
	.nf-primary,
	.nf-ghost {
		height: 46px;
		padding: 0 24px;
		display: inline-flex;
		align-items: center;
		border-radius: var(--k-radius);
		font-weight: 700;
		font-size: 14px;
		text-decoration: none;
	}
	.nf-primary {
		background: var(--k-primary);
		color: var(--k-on-primary);
		border: none;
		cursor: pointer;
		font-family: inherit;
	}
	.nf-ghost {
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text);
	}

	/* ---- tablet / phone ---- */
	@media (max-width: 900px) {
		.hero-grid {
			grid-template-columns: 150px minmax(0, 1fr);
			column-gap: 22px;
		}
		.poster {
			width: 150px;
		}
		.head {
			padding-top: 0;
		}
	}
	@media (max-width: 640px) {
		.banner {
			height: 240px;
		}
		.hero {
			padding-top: 20px;
		}
		/* Phone: small cover beside the title, actions + meta full width below. */
		.hero-grid {
			grid-template-columns: 108px minmax(0, 1fr);
			grid-template-areas:
				'poster head'
				'actions actions'
				'meta meta';
			grid-template-rows: auto;
			column-gap: 16px;
			row-gap: 16px;
		}
		.poster {
			width: 108px;
		}
		.head {
			align-self: end;
		}
		/* Scales down on the narrowest phones, so a long word fits beside the cover. */
		h1 {
			font-size: clamp(20px, 6.4vw, 24px);
		}
		.alt-title {
			font-size: 14px;
		}
		.creators {
			font-size: 13px;
		}
		.actions {
			gap: 8px;
		}
		.lib,
		.read-btn {
			flex: 1 1 140px;
			justify-content: center;
			padding: 0 14px;
		}
		/* Phone: the popover docks above the bottom nav, full width, so it can
		   never run off either edge whatever row the star button wrapped onto. */
		.rate-pop {
			position: fixed;
			top: auto;
			left: 16px;
			right: 16px;
			bottom: calc(72px + env(safe-area-inset-bottom, 0px));
			width: auto;
			transform: none;
		}
		/* Phone: number + title on the first line, source and date beneath. */
		/* The mark sits beside the FIRST line (number + title), not centred on both. */
		.ch-head {
			align-items: flex-start;
			padding-left: 12px;
		}
		.ch-head > .ch-mark {
			margin-top: 10px;
		}
		.ch-mark-btn {
			margin-top: 4px;
		}
		.ch-main {
			flex-wrap: wrap;
			row-gap: 4px;
			padding: 10px 12px;
		}
		.ch-text {
			flex-basis: 100%;
		}
		.ch-tail {
			display: none;
		}
		/* The versions button sits beside the first line (number + title), so the source
		   and date beneath keep the full width, like a row with one copy. */
		.multi .ch-text {
			padding-right: 54px;
		}
		.ch-more {
			top: 7px;
			transform: none;
			height: 26px;
		}
		.ch-by {
			flex: 0 1 auto;
		}
		.ch-dates {
			flex: 1 1 auto;
			flex-direction: row;
			justify-content: flex-end;
			gap: 10px;
		}
		.ch-copy {
			padding: 6px 12px 6px 44px;
		}
		.tabs {
			display: flex;
		}
		.tabs button {
			flex: 1 0 auto;
			justify-content: center;
		}
		.lightbox {
			padding: 16px;
		}
	}
</style>
