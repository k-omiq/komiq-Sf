<script lang="ts">
	import { page } from '$app/state';
	import { goto, replaceState } from '$app/navigation';
	import { untrack } from 'svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Stars from '$lib/components/Stars.svelte';
	import MangaCard from '$lib/components/MangaCard.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import CommentThread from '$lib/components/CommentThread.svelte';
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
	} from '$lib/data/source';
	import { setPreferredTranslator } from '$lib/data/translator-pref.svelte';
	import { backend, images } from '$lib/context';
	import { auth } from '$lib/auth.svelte';
	import {
		socialLive,
		loadSeriesSocial,
		saveSeriesRating,
		clearSeriesRating,
	} from '$lib/data/social-repo';

	import type { SeriesResult } from '$lib/data/source';

	let { data } = $props();

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
	let sort = $state<'newest' | 'oldest'>('newest');

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
	const readCount = $derived((view?.chapters ?? []).filter((c) => c.read).length);
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

	// Translators (sources) available for this work, and switching between them.
	const translators = $derived(view?.translators ?? []);
	const selectedTranslatorKey = $derived(view?.selectedTranslatorKey ?? null);
	const workId = $derived(view?.workId ?? null);
	let switching = $state(false);

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

	// The picker is a POPOVER rather than the flat button list it used to be. With a
	// handful of sources the flat list pushed the chapter list a screen down the page, and
	// it read as a set of tabs rather than "the source this list is coming from" — which
	// is what it now literally controls (see `getSeries`: the selected source owns the
	// chapter list). Same dismissal contract as `StatusMenu`: outside click on the capture
	// phase, plus Escape.
	let pickerOpen = $state(false);
	let pickerRoot = $state<HTMLElement | null>(null);
	const selectedTranslator = $derived(
		translators.find((t) => t.key === selectedTranslatorKey) ?? translators[0] ?? null,
	);
	// One source is not a choice — render it as a static label, not a dead dropdown.
	const pickerInteractive = $derived(translators.length > 1);

	function togglePicker(): void {
		if (switching || !pickerInteractive) return;
		pickerOpen = !pickerOpen;
	}

	$effect(() => {
		if (!pickerOpen) return;
		const onDoc = (e: MouseEvent) => {
			if (pickerRoot && !pickerRoot.contains(e.target as Node)) pickerOpen = false;
		};
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') pickerOpen = false;
		};
		window.addEventListener('click', onDoc, true);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('click', onDoc, true);
			window.removeEventListener('keydown', onKey);
		};
	});

	async function selectTranslator(key: string): Promise<void> {
		pickerOpen = false;
		if (!workId || switching || key === selectedTranslatorKey) return;
		setPreferredTranslator(workId, key);
		switching = true;
		try {
			// Refetch by `workId`, NOT `data.slug`: after a `work_redirect` the two are
			// different ids, and getSeries() re-reads the preference we just stored
			// under `workId` — asking with the retired id would look up a preference
			// that isn't there and silently ignore the reader's choice.
			const r = await getSeries(workId);
			if (r.view) override = r;
		} finally {
			switching = false;
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
	// The cover goes through the SAME resolver the <img> uses, so the preview image
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
</script>

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
	<div class="hero k-gutter">
		<div class="hero-row">
			<div class="poster k-skeleton"></div>
			<div class="info skel-info">
				<div class="k-skeleton sk-badges"></div>
				<div class="k-skeleton sk-h1"></div>
				<div class="k-skeleton sk-creators"></div>
				<div class="k-skeleton sk-facts"></div>
				<div class="k-skeleton sk-cta"></div>
			</div>
		</div>
	</div>
{:else if detail && seriesDetail}
	<!-- hero info -->
	<div class="hero k-gutter">
		<div class="hero-row">
			<div class="poster"><Cover src={detail.cover} alt={title} /></div>
			<div class="info">
				<div class="badges">
					<span class="badge status"><span class="status-dot"></span>{statusLabel}</span>
					<span class="badge type"><span class="type-flag">{FLAG[type]}</span>{type}</span>
				</div>
				<h1>{title}</h1>
				<div class="creators">
					{#if creditList.length}
						{#each creditList as c, i (c.name)}
							{#if i > 0}<span class="cr-sep">·</span>{/if}<span class="cr-name">{c.name}</span
							>{#if c.label}<span class="cr-role">{c.label}</span>{/if}
						{/each}
					{:else}
						{seriesDetail.author} · {seriesDetail.artist}
					{/if}
				</div>
				<div class="facts">
					<span class="fact rating"
						><Icon name="star" size={16} fill="var(--k-star)" />{rating}<span class="votes"
							>({seriesDetail.votes})</span
						></span
					>
					<span class="sep"></span>
					<span class="fact">{totalCh} chapters</span>
					{#if viewsTotal > 0}
						<span class="sep"></span>
						<span class="fact"><Icon name="eye" size={15} />{viewsLabel} views</span>
					{/if}
					<span class="sep"></span>
					<!-- Chapter RELEASE time (the same clock the feed cards use), with our
					     detection time on hover when the backend reports it. -->
					<span
						class="fact"
						title={seriesDetail.detected
							? `Newest chapter released ${seriesDetail.updated} · we detected it ${seriesDetail.detected}`
							: `Newest chapter released ${seriesDetail.updated}`}
						>Updated {seriesDetail.updated}</span
					>
				</div>
				<div class="cta">
					<a class="read" href={readHref}
						><Icon name="play" size={14} fill="currentColor" />Continue Ch. {continueCh}</a
					>
					<button class="lib" class:in={inLibrary} disabled={marking} onclick={toggleLibrary}>
						{#if inLibrary}
							<Icon name="check" size={16} strokeWidth={2.4} />In Library
						{:else}
							<Icon name="plus" size={16} strokeWidth={2.2} />Add to Library
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
					<button
						class="fav"
						class:on={isFavorite}
						disabled={favBusy}
						aria-pressed={isFavorite}
						aria-label={isFavorite ? 'Remove from favourites' : 'Add to favourites'}
						title={isFavorite ? 'Favourited' : 'Add to favourites'}
						onclick={toggleFavorite}
					>
						<Icon name="heart" size={18} fill={isFavorite ? 'currentColor' : 'none'} />
					</button>
					<button
						class="share"
						class:copied={shareCopied}
						aria-label={shareCopied ? 'Link copied' : 'Share'}
						title={shareCopied ? 'Link copied' : 'Share'}
						onclick={shareSeries}><Icon name={shareCopied ? 'check' : 'share'} size={18} /></button
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
							title={workId
								? 'Detach sources that belong to a different series'
								: 'Resolving work…'}
							onclick={() => (splitOpen = true)}>Split</button
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
			</div>
		</div>
	</div>

	<!-- genres + synopsis -->
	<div class="genres-syn k-gutter">
		<div class="genre-chips">
			{#each seriesDetail.genres as g (g)}
				<span class="gchip">{g}</span>
			{/each}
		</div>
		{#if seriesDetail.altTitles.length}
			<div class="alt-titles">
				<span class="alt-label">Also known as</span>
				<div class="alt-chips">
					{#each seriesDetail.altTitles as t (t)}
						<span class="alt-chip">{t}</span>
					{/each}
				</div>
			</div>
		{/if}
		{#if descriptions.length > 1}
			<div class="desc-lang">
				<span class="desc-lang-label">Description language</span>
				<select bind:value={descLangSel} aria-label="Description language">
					<option value={null}>Default ({(seriesDetail.descLang ?? 'en').toUpperCase()})</option>
					{#each descriptions as d (d.lang)}
						<option value={d.lang}>{langName(d.lang)}</option>
					{/each}
				</select>
			</div>
		{/if}
		<p class="synopsis">{synopsisText}</p>
	</div>

	<!-- covers (F2): additional per-volume/locale covers; hidden for ≤1 cover -->
	{#if covers.length > 1}
		<div class="covers k-gutter">
			<div class="covers-head">
				<h2>Covers</h2>
				<span class="covers-count">{covers.length}</span>
			</div>
			<div class="covers-strip">
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
		</div>
	{/if}

	<!-- rate -->
	<div class="rate-wrap k-gutter">
		<div class="rate">
			<div class="rate-left">
				<div class="score">
					<span class="score-num"><Icon name="star" size={26} fill="var(--k-star)" />{rating}</span>
					<span class="score-sub">{seriesDetail.votes} community ratings</span>
				</div>
				<div class="divider"></div>
				<div>
					<div class="rate-title">Rate this series</div>
					<div class="rate-label">{ratingLabel}</div>
				</div>
			</div>
			<div class="rate-right">
				<Stars bind:value={userRating} max={10} size={24} onchange={onRate} />
				<!-- Only offered when the viewer's review has NO written body. Withdrawing a
				     rating deletes the review row (the server cannot store a scoreless
				     review), so on a body-bearing row this button would silently destroy
				     text the reader wrote. Hiding it beats a confirm dialog for something
				     nobody expects a "Clear" next to a star row to be able to do. -->
				{#if userRating > 0 && !needsAuth && !myBody.trim()}
					<button class="clear" onclick={clearRating}>Clear</button>
				{/if}
			</div>
		</div>
	</div>

	<!-- source picker — chooses WHICH source's chapters the list below shows -->
	{#if translators.length}
		<div class="translators k-gutter">
			<div class="tr-head">
				<span class="tr-label">Source</span>
				<span class="tr-sub"
					>{translators.length === 1
						? 'Only one source available'
						: `${translators.length} sources · switch to re-list chapters`}</span
				>
			</div>
			<div class="tr-picker" class:open={pickerOpen} bind:this={pickerRoot}>
				<button
					type="button"
					class="tr-trigger"
					class:static={!pickerInteractive}
					disabled={switching}
					aria-haspopup="listbox"
					aria-expanded={pickerOpen}
					aria-label="Change source"
					onclick={togglePicker}
				>
					{#if selectedTranslator}
						{#if selectedTranslator.iconUrl}
							<img
								class="tr-logo"
								src={selectedTranslator.iconUrl}
								alt=""
								loading="lazy"
								decoding="async"
								referrerpolicy="no-referrer"
							/>
						{:else}
							<span class="tr-logo tr-initial"
								>{selectedTranslator.name.charAt(0).toUpperCase()}</span
							>
						{/if}
						<span class="tr-name">
							<span class="tr-name-main"
								>{selectedTranslator.name}{#if selectedTranslator.lang}<span class="tr-lang"
										>{selectedTranslator.lang.toUpperCase()}</span
									>{/if}</span
							>
							<span class="tr-count"
								>{selectedTranslator.chapterCount}
								{selectedTranslator.chapterCount === 1 ? 'chapter' : 'chapters'}</span
							>
						</span>
					{/if}
					{#if pickerInteractive}
						<Icon name="chevron-down" size={16} />
					{/if}
				</button>
				{#if pickerOpen}
					<div class="tr-menu" role="listbox" aria-label="Sources">
						{#each translators as t (t.key)}
							<button
								type="button"
								role="option"
								aria-selected={t.key === selectedTranslatorKey}
								class="tr-opt"
								class:on={t.key === selectedTranslatorKey}
								disabled={switching}
								onclick={() => selectTranslator(t.key)}
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
								<span class="tr-name">
									<span class="tr-name-main"
										>{t.name}{#if t.lang}<span class="tr-lang">{t.lang.toUpperCase()}</span
											>{/if}</span
									>
									<span class="tr-count"
										>{t.chapterCount} {t.chapterCount === 1 ? 'chapter' : 'chapters'}</span
									>
								</span>
								{#if t.key === selectedTranslatorKey}
									<Icon name="check" size={15} strokeWidth={2.6} />
								{/if}
							</button>
						{/each}
					</div>
				{/if}
			</div>
		</div>
	{/if}

	<!-- chapters -->
	<div class="chapters k-gutter">
		<div class="ch-head">
			<div class="ch-title">
				<h2>Chapters</h2>
				<span class="ch-total">{totalCh} total</span>
			</div>
			<div class="ch-sort">
				<button class="sortchip" class:on={sort === 'newest'} onclick={() => (sort = 'newest')}
					>Newest</button
				>
				<button class="sortchip" class:on={sort === 'oldest'} onclick={() => (sort = 'oldest')}
					>Oldest</button
				>
			</div>
		</div>
		<div class="ch-list">
			<!-- EMPTY STATE. There was none: the list simply rendered nothing under a
			     "Chapters · 0 total" header, which reads as a page that failed to load. It is
			     reachable in bulk now that Browse pages the whole catalogue (~67k works have
			     no chapter yet), and the honest wording matters — MangaDex REMOVES chapters
			     when a series is licensed or claimed, so this is usually a popular series
			     whose chapters live on another source, not a broken one. The source picker
			     above is the actual next step when there is more than one source, so the copy
			     points at it only when it exists. -->
			{#if chapters.length === 0}
				<div class="ch-empty">
					<div class="ch-empty-title">No chapters from any source yet</div>
					<div class="ch-empty-desc">
						{pickerInteractive
							? 'Nothing is available from this source. Try another source above.'
							: 'Nothing is available to read here yet — it may arrive from another source, or be licensed elsewhere.'}
					</div>
				</div>
			{/if}
			{#each chapters as c (c.id ?? c.n)}
				<a class="ch-row" class:read={c.read} href={chapterHref(c.id, c.src)}>
					<span class="ch-num">{c.n}</span>
					<div class="ch-info">
						<div class="ch-line">
							<span class="ch-name">{c.title}</span>
							{#if c.isNew}<span class="new">NEW</span>{/if}
							<!-- Off-site chapters (~35,000 of them: MangaPlus, Comikey, NamiComi,
							     BiliBili) have no pages for us to serve — tapping one opens a
							     "read it on <host>" hand-off. Badged HERE, in the list people
							     actually browse, because learning it only after you have opened
							     the chapter is the dead end the badge exists to remove. The
							     reader's own dropdown badges too, but that is already past the
							     point of surprise. -->
							{#if c.external}<span class="ext" title="Hosted on another site"
									><Icon name="globe" size={11} />OFF-SITE</span
								>{/if}
						</div>
						<!-- TWO CLOCKS, NAMED, because they answer different questions and
						     the gap between them is the point: a source that holds chapters
						     behind early access publishes days before anyone here can read
						     them.

						     "Found" is NEVER allowed to fall back to the release date — a
						     chapter we have no record of finding renders an em dash, which
						     is what `foundDate` already carries. Expect that dash on most
						     older chapters: no discovery time was invented for the back
						     catalogue (migration 0099).

						     Multi-source works can carry a chapter no source we can date
						     provides (S2 aggregation), so the released half still drops out
						     entirely rather than leaving an empty line among dated rows. -->
						<div class="ch-dates">
							{#if c.date}<span class="ch-date">Released {c.date}</span>{/if}
							<span
								class="ch-date ch-found"
								title="When we first had this chapter available to read"
								>Found {c.foundDate}</span
							>
						</div>
					</div>
					{#if c.read}<span class="ch-read"
							><Icon name="check" size={13} strokeWidth={2.4} />Read</span
						>{/if}
					<Icon name="chevron-right" size={16} stroke="#57554f" />
				</a>
			{/each}
		</div>
	</div>

	<!-- related -->
	{#if relatedSeries.length}
		<div class="related k-gutter">
			<h2>Readers Also Enjoyed</h2>
			<div class="related-row">
				{#each relatedSeries as item (item.id ?? item.title)}
					<MangaCard
						title={item.title}
						sub={`${item.genre} · ${item.ch} ch`}
						rating={item.rating}
						cover={item.cover}
						id={item.id}
						fixed
					/>
				{/each}
			</div>
		</div>
	{/if}
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

<!-- admin source splitter. Same mount conditions as the merge picker: an admin with a
     resolved work, so its row query never leaves a reader's page. -->
{#if canSplit && workId}
	<UnmergeDialog bind:open={splitOpen} {workId} workTitle={title} onsplit={afterSplit} />
{/if}

<!-- cover lightbox (F2) -->
{#if activeCover}
	<div
		class="lightbox"
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

<!-- discussion — the single unified comment engine (replies + image attachments).
     The old bespoke "Reviews" comment list was removed; the star rating above is
     the series' score, and all written discussion lives in this one thread. -->
<div class="discussion k-gutter">
	<h2 class="discussion-title">Discussion</h2>
	{#if seriesId}
		<CommentThread
			targetType="series"
			targetId={seriesId}
			storageKey={socialKey}
			prompt="Share your thoughts on this series…"
		/>
	{/if}
</div>

<style>
	.discussion {
		margin-top: 40px;
		padding-bottom: 8px;
	}
	.discussion-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 22px;
		color: var(--k-text-bright);
		margin: 0 0 20px;
	}
	.hero {
		position: relative;
		padding-top: 48px;
		z-index: 5;
	}
	/* Centred stacked hero: cover on top, metadata column beneath it. */
	.hero-row {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 28px;
		text-align: center;
	}
	.poster {
		position: relative;
		flex: 0 0 auto;
		width: 224px;
		height: 328px;
		border-radius: 12px;
		overflow: hidden;
		border: 1px solid var(--k-border-2);
		box-shadow: 0 26px 60px rgba(0, 0, 0, 0.6);
	}
	.info {
		width: 100%;
		max-width: 720px;
		min-width: 0;
	}
	/* loading skeleton */
	.poster.k-skeleton {
		border-radius: 12px;
	}
	.skel-info {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 16px;
	}
	.skel-info .k-skeleton {
		border-radius: 8px;
	}
	.sk-badges {
		width: 180px;
		height: 24px;
	}
	.sk-h1 {
		width: 60%;
		max-width: 460px;
		height: 54px;
	}
	.sk-creators {
		width: 220px;
		height: 16px;
	}
	.sk-facts {
		width: 80%;
		max-width: 520px;
		height: 16px;
	}
	.sk-cta {
		width: 340px;
		height: 50px;
		margin-top: 6px;
	}
	/* not-found */
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
		margin: 0;
		color: var(--k-text-bright);
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
		border-radius: 8px;
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
	.nf-ghost:hover {
		border-color: rgba(255, 255, 255, 0.34);
	}
	.badges {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 10px;
		margin-bottom: 16px;
	}
	.badge {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		border-radius: var(--k-radius-pill);
		font-size: 11.5px;
		font-weight: 700;
	}
	.badge.status {
		padding: 5px 12px;
		background: rgba(255, 255, 255, 0.06);
		border: 1px solid var(--k-border-2);
		letter-spacing: 0.05em;
		color: var(--k-text-2);
	}
	.status-dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--k-dot);
	}
	.badge.type {
		padding: 5px 13px 5px 10px;
		background: rgba(224, 131, 105, 0.12);
		border: 1px solid rgba(224, 131, 105, 0.4);
		font-weight: 800;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-accent);
	}
	.type-flag {
		font-size: 15px;
		line-height: 1;
	}
	h1 {
		font-size: 58px;
		line-height: 1;
		letter-spacing: -0.03em;
		margin: 0 0 8px;
		color: var(--k-text-bright);
	}
	.creators {
		font-size: 15px;
		color: var(--k-text-dim);
		margin-bottom: 22px;
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 8px;
		flex-wrap: wrap;
	}
	.cr-name {
		color: var(--k-text-2);
		font-weight: 600;
	}
	.cr-role {
		font-size: 10.5px;
		font-weight: 800;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-text-faint);
		background: var(--k-hover-fill);
		border: 1px solid var(--k-border-3);
		border-radius: 5px;
		padding: 1px 6px;
		margin-left: 5px;
	}
	.cr-sep {
		color: var(--k-text-disabled);
	}
	.desc-lang {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.desc-lang-label {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.09em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.desc-lang select {
		height: 34px;
		padding: 0 10px;
		border-radius: 8px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text-2);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
	}
	.facts {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 22px;
		flex-wrap: wrap;
		margin-bottom: 24px;
	}
	.fact {
		font-size: 14px;
		color: var(--k-text-muted);
	}
	.fact.rating {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-weight: 700;
		font-size: 15px;
		color: var(--k-text);
	}
	.votes {
		color: var(--k-text-faint);
		font-weight: 500;
		font-size: 13px;
	}
	.sep {
		width: 4px;
		height: 4px;
		border-radius: 50%;
		background: #403f3b;
	}
	.cta {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 12px;
		flex-wrap: wrap;
	}
	.write-error {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 8px;
		margin-top: 12px;
		padding: 9px 14px;
		border-radius: 8px;
		background: rgba(246, 183, 60, 0.1);
		border: 1px solid rgba(246, 183, 60, 0.34);
		color: var(--k-text-2);
		font-size: 13px;
		font-weight: 600;
	}
	.read {
		height: 50px;
		padding: 0 30px;
		border: none;
		border-radius: 8px;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 15px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		gap: 9px;
		text-decoration: none;
	}
	.read:hover {
		background: var(--k-primary-hover);
	}
	.lib {
		height: 50px;
		padding: 0 22px;
		border-radius: 8px;
		font-weight: 700;
		font-size: 14px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		gap: 9px;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text);
		transition: all 0.15s;
	}
	.lib:hover {
		border-color: rgba(255, 255, 255, 0.34);
	}
	.lib.in {
		background: rgba(95, 191, 126, 0.12);
		border-color: rgba(95, 191, 126, 0.5);
		color: #7fd39a;
	}
	/* Admin-only, so it is deliberately the quietest thing in the CTA row — an outline
	   button sized to the others. It sits last, after Share, because it is a maintenance
	   action and must never compete with Continue / Add to Library. */
	.merge {
		height: 50px;
		padding: 0 16px;
		flex: 0 0 auto;
		border-radius: 8px;
		background: transparent;
		border: 1px dashed var(--k-border-4);
		color: var(--k-text-3);
		font: inherit;
		font-weight: 600;
		cursor: pointer;
	}
	.merge:hover:not(:disabled) {
		border-color: rgba(255, 255, 255, 0.34);
		color: var(--k-text-1);
	}
	.merge:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	/* Same vocabulary as `.merge` above — admin-only, quiet, sized to the CTA row and
	   last in it. A segmented group rather than a button because the choice is
	   three-way and the current state has to be readable at a glance. */
	.rating {
		display: flex;
		align-items: center;
		gap: 8px;
		height: 50px;
		flex: 0 0 auto;
		padding: 0 10px 0 12px;
		border-radius: 8px;
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
		transition: all 0.15s;
	}
	.rating-seg button:hover:not(:disabled):not(.on) {
		color: var(--k-text-1);
	}
	.rating-seg button.on {
		background: var(--k-surface);
		color: var(--k-text-1);
	}
	.rating-seg button:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.share {
		width: 50px;
		height: 50px;
		flex: 0 0 auto;
		border-radius: 8px;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-3);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	/* Admin content-rating override. Sized to the 50px CTA buttons it sits beside so
	   the row keeps one baseline, and visually quieter than them — it is a moderation
	   tool sharing space with the reader's own actions, not a peer of them. */
	.share:hover {
		border-color: rgba(255, 255, 255, 0.34);
		color: var(--k-text);
	}
	.share.copied {
		border-color: rgba(95, 191, 126, 0.5);
		color: #7fd39a;
	}
	.fav {
		width: 50px;
		height: 50px;
		flex: 0 0 auto;
		border-radius: 8px;
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-3);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
		transition: all 0.15s;
	}
	.fav:hover:not(:disabled) {
		border-color: rgba(233, 110, 110, 0.5);
		color: #e96e6e;
	}
	.fav.on {
		background: rgba(233, 110, 110, 0.12);
		border-color: rgba(233, 110, 110, 0.5);
		color: #e96e6e;
	}
	.fav:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.genres-syn {
		padding-top: 44px;
		padding-bottom: 8px;
		display: flex;
		flex-direction: column;
		gap: 22px;
	}
	.genre-chips {
		display: flex;
		gap: 9px;
		flex-wrap: wrap;
	}
	.gchip {
		font-size: 13px;
		color: var(--k-text-3);
		padding: 7px 15px;
		border-radius: var(--k-radius-pill);
		border: 1px solid var(--k-border-3);
	}
	.alt-titles {
		display: flex;
		flex-direction: column;
		gap: 7px;
	}
	.alt-label {
		font-size: 11.5px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.alt-chips {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.alt-chip {
		font-size: 12.5px;
		color: var(--k-text-muted);
		padding: 5px 12px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
	}
	.synopsis {
		max-width: 820px;
		font-size: 15.5px;
		line-height: 1.7;
		color: var(--k-text-muted);
		margin: 0;
	}
	/* covers (F2) */
	.covers {
		padding-top: 44px;
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	.covers-head {
		display: flex;
		align-items: baseline;
		gap: 12px;
	}
	.covers-head h2 {
		font-size: 21px;
		margin: 0;
		color: var(--k-text);
	}
	.covers-count {
		font-size: 13px;
		color: var(--k-text-faint);
	}
	.covers-strip {
		display: flex;
		gap: 14px;
		overflow-x: auto;
		padding-bottom: 8px;
		scroll-snap-type: x proximity;
	}
	.cover-cell {
		flex: 0 0 auto;
		width: 120px;
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 0;
		background: none;
		border: none;
		cursor: pointer;
		text-align: left;
		scroll-snap-align: start;
	}
	.cover-thumb {
		position: relative;
		width: 120px;
		height: 180px;
		border-radius: 9px;
		overflow: hidden;
		border: 1px solid var(--k-border-2);
		background: var(--k-surface-2);
		transition: border-color 0.15s;
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
		letter-spacing: 0.04em;
		color: var(--k-text-faint);
		background: var(--k-hover-fill);
		border: 1px solid var(--k-border-3);
		border-radius: 4px;
		padding: 1px 5px;
	}
	/* cover lightbox */
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
		border-radius: 12px;
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
		color: var(--k-text-bright);
	}
	.lb-close {
		width: 40px;
		height: 40px;
		flex: 0 0 auto;
		border-radius: 9px;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.lb-close:hover {
		border-color: var(--k-border-strong);
	}
	.rate-wrap {
		padding-top: 40px;
	}
	.rate {
		display: flex;
		flex-wrap: wrap;
		gap: 28px;
		align-items: center;
		justify-content: space-between;
		border: 1px solid var(--k-border);
		border-radius: 12px;
		padding: 24px 30px;
		background: var(--k-surface);
	}
	.rate-left {
		display: flex;
		align-items: center;
		gap: 24px;
	}
	.score {
		display: flex;
		flex-direction: column;
	}
	.score-num {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 38px;
		line-height: 1;
		color: var(--k-text-bright);
	}
	.score-sub {
		font-size: 12px;
		color: var(--k-text-faint);
		margin-top: 6px;
	}
	.divider {
		width: 1px;
		height: 52px;
		background: var(--k-border-2);
	}
	.rate-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 16px;
		color: var(--k-text);
	}
	.rate-label {
		font-size: 13px;
		color: var(--k-text-dimmer);
		margin-top: 4px;
	}
	.rate-right {
		display: flex;
		align-items: center;
		gap: 16px;
	}
	.clear {
		background: none;
		border: none;
		font-size: 12.5px;
		font-weight: 700;
		color: var(--k-text-dimmer);
		cursor: pointer;
		white-space: nowrap;
	}
	.clear:hover {
		color: var(--k-text);
	}
	.translators {
		padding-top: 44px;
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	.tr-head {
		display: flex;
		align-items: baseline;
		gap: 12px;
		flex-wrap: wrap;
	}
	.tr-label {
		font-size: 21px;
		font-weight: 700;
		font-family: var(--k-font-display);
		color: var(--k-text);
	}
	.tr-sub {
		font-size: 13px;
		color: var(--k-text-faint);
	}
	.tr-picker {
		position: relative;
		display: inline-flex;
		align-self: flex-start;
	}
	/* The trigger displays the CURRENT source, so it carries the same accent the chosen
	   row in the menu does — control and value read as one thing, rather than a neutral
	   button sitting above a highlighted list. */
	.tr-trigger {
		display: inline-flex;
		align-items: center;
		gap: 12px;
		padding: 10px 14px 10px 12px;
		border-radius: 12px;
		background: rgba(224, 131, 105, 0.1);
		border: 1px solid var(--k-primary);
		color: var(--k-text-bright);
		cursor: pointer;
		transition: filter 0.15s;
		text-align: left;
		font-family: inherit;
		min-width: 260px;
	}
	.tr-trigger:not(:disabled):hover {
		filter: brightness(1.1);
	}
	.tr-trigger:disabled {
		opacity: 0.6;
		cursor: default;
	}
	/* One source is not a choice: drop the accent and the affordance both. */
	.tr-trigger.static {
		cursor: default;
		background: var(--k-surface);
		border-color: var(--k-border-2);
		color: var(--k-text-2);
	}
	.tr-trigger.static:hover {
		filter: none;
	}
	/* The chevron/check are the only <svg> here; push the chevron to the far edge. */
	.tr-trigger :global(svg) {
		margin-left: auto;
		flex: 0 0 auto;
	}
	.tr-menu {
		position: absolute;
		z-index: 40;
		top: calc(100% + 6px);
		left: 0;
		min-width: 100%;
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 6px;
		border-radius: 12px;
		background: var(--k-surface-3, var(--k-surface));
		border: 1px solid var(--k-border-3);
		box-shadow: 0 16px 40px rgba(0, 0, 0, 0.5);
		/* A work can carry a dozen sources; cap the popover and scroll rather than
		   running it off the bottom of the viewport. */
		max-height: 320px;
		overflow-y: auto;
	}
	.tr-opt {
		display: inline-flex;
		align-items: center;
		gap: 12px;
		width: 100%;
		padding: 9px 12px;
		border-radius: 9px;
		background: transparent;
		border: 1px solid transparent;
		color: var(--k-text-2);
		cursor: pointer;
		transition: all 0.15s;
		text-align: left;
		font-family: inherit;
	}
	.tr-opt:hover {
		background: var(--k-hover-fill, rgba(255, 255, 255, 0.06));
	}
	.tr-opt.on {
		border-color: var(--k-primary);
		background: rgba(224, 131, 105, 0.1);
		color: var(--k-text-bright);
	}
	.tr-opt:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.tr-opt :global(svg) {
		margin-left: auto;
		flex: 0 0 auto;
	}
	.tr-logo {
		width: 34px;
		height: 34px;
		flex: 0 0 auto;
		border-radius: 8px;
		object-fit: cover;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-2);
	}
	.tr-initial {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		font-size: 16px;
		font-weight: 800;
		color: var(--k-text-2);
	}
	.tr-name {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.tr-name-main {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		font-size: 14px;
		font-weight: 700;
	}
	.tr-lang {
		font-size: 9.5px;
		font-weight: 800;
		letter-spacing: 0.04em;
		color: var(--k-text-3);
		background: var(--k-hover-fill);
		border: 1px solid var(--k-border-3);
		border-radius: 4px;
		padding: 1px 5px;
	}
	.tr-count {
		font-size: 12px;
		color: var(--k-text-faint);
	}
	.ch-empty {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 22px 2px;
	}
	.ch-empty-title {
		font-size: 14px;
		font-weight: 600;
		color: var(--k-text-1);
	}
	.ch-empty-desc {
		font-size: 12.5px;
		line-height: 1.5;
		color: var(--k-text-faint);
	}
	.chapters {
		padding-top: 48px;
		display: flex;
		flex-direction: column;
		gap: 20px;
	}
	.ch-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 20px;
		flex-wrap: wrap;
	}
	.ch-title {
		display: flex;
		align-items: baseline;
		gap: 12px;
	}
	.ch-title h2 {
		font-size: 21px;
		margin: 0;
		color: var(--k-text);
	}
	.ch-total {
		font-size: 13px;
		color: var(--k-text-faint);
	}
	.ch-sort {
		display: flex;
		gap: 8px;
	}
	.sortchip {
		font-size: 13px;
		font-weight: 700;
		padding: 7px 15px;
		border-radius: var(--k-radius-pill);
		cursor: pointer;
		background: transparent;
		color: var(--k-text-dimmer);
		border: 1px solid var(--k-border-4);
		transition: all 0.15s;
	}
	.sortchip.on {
		background: var(--k-primary);
		color: var(--k-on-primary);
		border-color: var(--k-primary);
	}
	.ch-list {
		border: 1px solid var(--k-border);
		border-radius: 12px;
		overflow: hidden;
		max-height: 560px;
		overflow-y: auto;
	}
	.ch-row {
		display: flex;
		align-items: center;
		gap: 18px;
		padding: 15px 20px;
		border-bottom: 1px solid rgba(255, 255, 255, 0.05);
		text-decoration: none;
		transition: background 0.12s;
	}
	.ch-row:hover {
		background: rgba(255, 255, 255, 0.035);
	}
	.ch-num {
		flex: 0 0 auto;
		width: 52px;
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 16px;
		color: var(--k-text);
	}
	.ch-row.read .ch-num {
		color: var(--k-text-ghost);
	}
	.ch-info {
		flex: 1;
		min-width: 0;
	}
	.ch-line {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.ch-name {
		font-weight: 600;
		font-size: 14.5px;
		color: var(--k-text-1);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.ch-row.read .ch-name {
		color: var(--k-text-dim);
	}
	.new {
		flex: 0 0 auto;
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.06em;
		color: var(--k-on-primary);
		background: var(--k-star);
		padding: 2px 7px;
		border-radius: 5px;
	}
	/* Outlined, not filled — unlike `.new`. An off-site chapter is a CAVEAT, not a
	   highlight, and giving it the same visual weight as NEW would read as a promotion.
	   Matches the reader dropdown's own `.ext-tag` so the same fact looks the same on both
	   surfaces. */
	.ext {
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
	.ch-dates {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		/* Column gap only: the separator between the two halves is space, and on a narrow
		   screen they wrap onto their own lines rather than being truncated. */
		gap: 0 8px;
		margin-top: 3px;
	}
	.ch-date {
		font-size: 12px;
		color: var(--k-text-fainter);
	}
	/* Fainter than the release date on purpose. The release date is what a reader is
	   looking for; the found date is the explanation for why a chapter is where it is in
	   /updates, which matters only when the two disagree. */
	.ch-found {
		color: var(--k-text-ghost);
	}
	.ch-read {
		flex: 0 0 auto;
		font-size: 11.5px;
		color: var(--k-text-ghost);
		display: inline-flex;
		align-items: center;
		gap: 5px;
	}
	.related {
		display: flex;
		flex-direction: column;
		gap: 20px;
		padding-top: 56px;
	}
	.related h2 {
		font-size: 21px;
		margin: 0;
		color: var(--k-text);
	}
	.related-row {
		display: flex;
		gap: 22px;
		overflow-x: auto;
		padding-bottom: 4px;
	}
	@media (max-width: 640px) {
		.hero {
			padding-top: 28px;
		}
		/* Keep the centred stack; just tighten spacing and shrink the cover. */
		.hero-row {
			gap: 18px;
		}
		.poster {
			width: 132px;
			height: 194px;
		}
		.info {
			width: 100%;
		}
		.badges {
			flex-wrap: wrap;
		}
		h1 {
			font-size: 30px;
		}
		.creators {
			font-size: 13.5px;
		}
		/* CTA row wraps so Continue / Library / Share never overflow; primary grows. */
		.cta {
			flex-wrap: wrap;
		}
		.read {
			flex: 1 1 auto;
			justify-content: center;
			min-width: 160px;
		}
		/* Give the source picker a full-width one-handed target; the popover it opens
		   inherits the width via `.tr-menu { min-width: 100% }`. */
		.tr-picker,
		.tr-trigger {
			width: 100%;
			min-width: 0;
		}
		.covers-strip {
			gap: 12px;
		}
		.cover-cell,
		.cover-thumb {
			width: 104px;
		}
		.cover-thumb {
			height: 156px;
		}
		/* Rate panel stacks so the stars aren't cramped beside the score. */
		.rate {
			padding: 20px;
		}
		.rate-left {
			flex-wrap: wrap;
			gap: 16px;
		}
	}
</style>
