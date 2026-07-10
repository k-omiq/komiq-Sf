<script lang="ts">
	import { untrack } from 'svelte';
	import { backend, images } from '$lib/context';
	import { retryCoverUrl, retryCoverSrcSet } from '$lib/data/cover-retry';
	import { withTimeout } from '$lib/data/reader-pages';
	import { apiOrigin, config } from '$lib/config';
	import { fetchCoverSource } from '$lib/data/cover-source';
	const cacheOrigins = [apiOrigin, config.imgWorkerBaseUrl].filter(Boolean);

	let {
		src = '',
		alt = '',
		loading = 'eager',
		fit = 'cover',
		sizing = 'full',
		priority = false,
		seriesId = '',
	}: {
		src?: string;
		alt?: string;
		/** Native lazy-load hint — pass 'lazy' for offscreen thumbnails (galleries). */
		loading?: 'eager' | 'lazy';
		/** object-fit; 'contain' for a full-cover viewer, 'cover' (default) to fill. */
		fit?: 'cover' | 'contain';
		/**
		 * 'card' opts this cover into the narrow `?w=` derivative at 1x (see
		 * `coverCardSrcSet`). Only correct for a box around card size (~150 CSS px):
		 * the srcset uses `x` descriptors, which ignore the CSS box entirely, so a hero
		 * or a poster marked 'card' would render the small derivative stretched.
		 * 'full' (default) always loads the master.
		 */
		sizing?: 'card' | 'full';
		/**
		 * Mark this cover as the LCP candidate — sets `fetchpriority="high"` so it is
		 * not queued behind the grid's covers. At most ONE per page should set it;
		 * marking several is the same as marking none.
		 */
		priority?: boolean;
		/** Enables manual recovery on series cards, including missing cover metadata. */
		seriesId?: string;
	} = $props();

	// Web: the provider resolves covers synchronously (a pure URL rewrite), so resolve
	// impeccable-disable-next-line broken-image: literal image element in explanatory prose.
	// eagerly — including during SSR — so the <img src> is in the server HTML and the
	// cover counts toward LCP instead of appearing only after a browser-only effect.
	// Native (Tauri): resolveCoverSync is absent (covers are async blob URLs), so we
	// fall back to the async effect below.
	const syncResolve = (s: string): string =>
		s && images.resolveCoverSync ? images.resolveCoverSync(s) : '';

	// Empty whenever the source can't be resized (native, `direct` mode, a foreign CDN)
	// impeccable-disable-next-line broken-image: literal image element in explanatory prose.
	// or this isn't a card — in which case the <img> falls back to `src` alone.
	let replacement = $state<{ original: string; seriesId: string; url: string } | null>(null);
	let refreshToken = $state('');
	let fetching = $state(false);
	let fetchError = $state('');
	let loaded = $state(false);
	const requestedSrc = $derived(
		replacement?.original === src && replacement.seriesId === seriesId ? replacement.url : src,
	);
	const srcSet = $derived(
		retryCoverSrcSet(
			sizing === 'card' && requestedSrc && images.coverCardSrcSet
				? images.coverCardSrcSet(requestedSrc)
				: '',
			refreshToken,
			cacheOrigins,
		),
	);

	// Snapshot only the initial source; the effect below owns later source changes.
	let resolved = $state(untrack(() => syncResolve(src)));
	let broken = $state(false);
	let el = $state<HTMLImageElement | undefined>(undefined);

	// A cover failure is usually TRANSIENT — a dropped request on a flaky connection,
	// a cold proxy, a rate-limited upstream. `broken` was only ever cleared when `src`
	// changed, so one such blip left a grey placeholder in that slot for the rest of
	// the page's life, even after the network recovered. Retry a bounded number of
	// times with a backoff (the reader does the same for pages via `retryImage`);
	// impeccable-disable-next-line broken-image: literal image element in explanatory prose.
	// `attempt` is part of the <img> key so a retry remounts a fresh element rather
	// than relying on the browser to re-request an identical src.
	const MAX_RETRIES = 2;
	let attempt = $state(0);
	let retries = 0;
	let retryTimer: ReturnType<typeof setTimeout> | undefined;

	// There is deliberately NO "a fast failure means the origin refused us, so don't
	// retry" discriminator here. It was tried and removed, because its premises are
	// false against this stack:
	//
	//  • Retrying the saturation case is CORRECT, not wasteful. When the cover pool is
	//    full the API answers `503 + Retry-After: 2 + no-store`
	//    (`cover_warming_response` in apps/server/src/main.rs) and materializes the
	//    cover in the background, so the bytes usually exist within a second or two —
	//    a bounded, jittered retry is exactly what turns that into a cover appearing
	//    instead of a grey block that sits there until the next navigation. The 503 is
	//    also cheap to serve: it is returned by a `try_acquire` that fails instantly,
	//    never by a queued upstream fetch.
	//  • It measured the wrong interval. The clock could only be started at mount, but
	//    every cover in a grid is `loading="lazy"` (MangaCard's default), so the
	//    request begins whenever the row scrolls into view — seconds or minutes later.
	//    "Elapsed" was therefore time-since-mount, and the check was inert in exactly
	//    the grid it was aimed at.
	//  • The failures that DO land under a second are `ERR_INTERNET_DISCONNECTED`,
	//    `ERR_NAME_NOT_RESOLVED` and `ERR_NETWORK_CHANGED` — offline, DNS, and the
	//    Wi-Fi/cellular handoff. That is the transient flaky-mobile case retries exist
	//    for, and it was the one case being suppressed.
	//
	// The real anti-stampede lever is the backoff below, which is kept.
	//
	// Base backoff, plus jitter. Without jitter every failing cover in a grid retries in
	// the same animation frame, so one bad moment becomes a synchronised second wave of
	// ~30 requests. Jitter spreads that wave over the whole window.
	const RETRY_BASE_MS = 1200;
	const RETRY_JITTER_MS = 900;

	function onError(): void {
		if (retries >= MAX_RETRIES) {
			broken = true;
			return;
		}
		retries += 1;
		loaded = false;
		broken = true; // drop the failed element, then remount a fresh one
		clearTimeout(retryTimer);
		const delay = retries * RETRY_BASE_MS + Math.random() * RETRY_JITTER_MS;
		retryTimer = setTimeout(() => {
			broken = false;
			attempt += 1;
		}, delay);
	}

	/** Clear retry bookkeeping — a new `src` gets a full retry budget of its own. */
	function resetRetries(): void {
		clearTimeout(retryTimer);
		retryTimer = undefined;
		retries = 0;
		attempt = 0;
		broken = false;
		loaded = false;
	}

	async function fetchCover(event: MouseEvent): Promise<void> {
		event.preventDefault();
		event.stopPropagation();
		if (fetching) return;
		const originalSrc = src;
		const originalId = seriesId;
		const generation = fetchGeneration;
		const current = () =>
			generation === fetchGeneration && src === originalSrc && seriesId === originalId;
		fetching = true;
		fetchError = '';
		try {
			// Re-read metadata in case the source has repaired or replaced its cover.
			const cover = await withTimeout(
				fetchCoverSource(backend, originalId),
				8000,
				'cover metadata',
			);
			if (!current()) return;
			const url = cover || requestedSrc;
			if (!url) throw new Error('No cover is available from the source yet.');
			replacement = { original: originalSrc, seriesId: originalId, url };
			refreshToken = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
		} catch (error) {
			if (!current()) return;
			// Existing URLs can still recover if metadata is temporarily unavailable.
			if (requestedSrc) refreshToken = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
			else fetchError = error instanceof Error ? error.message : 'Cover fetch failed.';
		} finally {
			if (current()) fetching = false;
		}
	}

	let fetchGeneration = 0;
	$effect(() => {
		// Cancel metadata results when a reused component switches series/source or unmounts.
		src;
		seriesId;
		return () => {
			fetchGeneration += 1;
			fetching = false;
			fetchError = '';
		};
	});

	function onLoad(): void {
		if (el && el.naturalWidth <= 1) {
			onError();
			return;
		}
		loaded = true;
		broken = false;
	}

	// Start the stall watchdog only when lazy images enter the viewport.
	$effect(() => {
		const img = el;
		if (!img || loaded || broken) return;
		let timeout: ReturnType<typeof setTimeout> | undefined;
		const start = () => {
			if (!timeout) timeout = setTimeout(onError, 15000);
		};
		if (img.complete && img.naturalWidth > 0) {
			onLoad();
			return;
		}
		let observer: IntersectionObserver | undefined;
		if (loading === 'eager' || typeof IntersectionObserver === 'undefined') start();
		else {
			observer = new IntersectionObserver(
				(entries) => {
					if (entries.some((entry) => entry.isIntersecting)) {
						start();
						observer?.disconnect();
					}
				},
				{ rootMargin: '300px' },
			);
			observer.observe(img);
		}
		return () => {
			clearTimeout(timeout);
			observer?.disconnect();
		};
	});

	$effect(() => () => clearTimeout(retryTimer));

	$effect(() => {
		const source = requestedSrc;
		const token = refreshToken;
		// Web sync path: just track `src` changes; no async round-trip, no blob URL to
		// release. (This branch also re-runs the initial resolve on hydration.)
		if (images.resolveCoverSync) {
			resolved = retryCoverUrl(syncResolve(source), token, cacheOrigins);
			resetRetries();
			return;
		}
		// Native async path: fetch the blob URL and release it on teardown.
		resolved = '';
		resetRetries();
		if (!source) return;
		let alive = true;
		let timedOut = false;
		const deadline = setTimeout(() => {
			timedOut = true;
			if (alive) broken = true;
		}, 15000);
		images
			.resolveCover(source)
			.then((u) => {
				clearTimeout(deadline);
				// The provider mints the object URL before this settles, so a fetch that
				// lands after unmount (or after `src` changed) has already pinned the
				// cover's bytes to the document — and the teardown below never saw it, so
				// nothing would ever release it. Scroll-flinging a grid abandons dozens of
				// these; on iOS that is tens of MB held in a jetsam-limited process.
				if (!alive || timedOut) {
					if (u && images.release) images.release(u);
					return;
				}
				resolved = u;
			})
			.catch(() => {
				clearTimeout(deadline);
				if (alive) broken = true;
			});
		return () => {
			alive = false;
			clearTimeout(deadline);
			if (resolved && images.release) images.release(resolved);
		};
	});

	// HYDRATION: Svelte deliberately never writes `src` (nor `srcset`) while hydrating —
	// see `set_attribute` in svelte/internal/client/dom/elements/attributes.js, which
	// records the SERVER's attribute and returns early to avoid a second network request,
	// "assum[ing] they are the same between client and server".
	//
	// On this app they are routinely NOT the same. Edge SSR is always anonymous (the
	// bearer token lives in localStorage; there is no hooks.server.ts forwarding it), so
	// a signed-in viewer's feed is server-rendered WITHOUT their NSFW rows and up to
	// `s-maxage` seconds stale, then the universal load re-runs at hydration WITH the
	// token and returns a different, longer row set. Keyed `{#each}` blocks do not save
	// us: hydration claims server nodes POSITIONALLY (`each.js`), keys are only consulted
	// for later reconciliation. Titles, hrefs and text all get rewritten from client data
	// because `set_text` has no such guard — only `src` is frozen. The visible result is
	// covers that belong to a different row than their title, permanently, plus NSFW
	// slots showing an unrelated SFW cover or a placeholder.
	//
	// So re-assert `src` from client state once the DOM is live. Guarded on inequality, so
	// the overwhelmingly common case (server and client DID agree) costs one string
	// compare and issues no request; only genuinely mismatched slots re-fetch, which is
	// the whole point. Post-hydration `src` changes are handled by Svelte normally — this
	// effect just no-ops on them.
	//
	// `srcset` gets the same treatment, and it is the MORE important of the two: Svelte's
	// early return covers it as well, and when both are present the browser picks the
	// candidate from `srcset` and ignores `src` entirely. Re-asserting only `src` would
	// therefore fix nothing on a mismatched slot — the stale server `srcset` would still
	// win. Cleared (not left stale) when client state has no srcset for this cover, so a
	// slot that hydrated into a non-card rendering cannot keep a card candidate.
	$effect(() => {
		const img = el;
		if (!img || !resolved) return;
		if (img.getAttribute('src') !== resolved) img.setAttribute('src', resolved);
		const current = img.getAttribute('srcset') ?? '';
		if (current === srcSet) return;
		if (srcSet) img.setAttribute('srcset', srcSet);
		else img.removeAttribute('srcset');
	});
</script>

{#if resolved && !broken}
	{#key `${refreshToken}:${attempt}`}
		<img
			bind:this={el}
			class="cover-img"
			width="300"
			height="450"
			src={resolved}
			srcset={srcSet || undefined}
			{alt}
			{loading}
			fetchpriority={priority ? 'high' : undefined}
			decoding="async"
			referrerpolicy="no-referrer"
			style="object-fit:{fit}"
			onerror={onError}
			onload={onLoad}
		/>
	{/key}
{:else}
	<div
		class="cover-ph k-cover"
		role={alt ? 'img' : undefined}
		aria-label={alt ? `${alt}: cover unavailable` : undefined}
	></div>
{/if}

{#if seriesId && (broken || !requestedSrc)}
	<div class="cover-recovery">
		<button
			type="button"
			onclick={fetchCover}
			disabled={fetching}
			aria-label={`Fetch cover for ${alt || 'series'}`}
		>
			{fetching ? 'Fetching…' : 'Fetch cover'}
		</button>
		{#if fetchError}<span role="status">{fetchError}</span>{/if}
	</div>
{/if}

<style>
	.cover-recovery {
		position: absolute;
		inset: 0;
		z-index: 2;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 8px;
		padding: 12px;
		pointer-events: none;
	}
	.cover-recovery button {
		pointer-events: auto;
		border: 1px solid var(--k-border-3);
		border-radius: 6px;
		background: var(--k-surface);
		color: var(--k-text-1);
		padding: 8px 12px;
		font-size: 12px;
		cursor: pointer;
	}
	.cover-recovery button:disabled {
		opacity: 0.6;
		cursor: wait;
	}
	.cover-recovery span {
		font-size: 11px;
		color: var(--k-text-1);
		text-align: center;
	}

	/* CONTRACT: the parent must establish a containing block — `position: relative` (or
	   any non-static position) AND a size. Both boxes below are `position: absolute`, so
	   against a STATIC parent they size to the nearest positioned ancestor instead, which
	   is somewhere further up the page and is never what the call site meant. `overflow:
	   hidden` on that parent does not save you: it does not clip a descendant whose
	   containing block is outside it.
	   This is not hypothetical — MergeDialog's `.thumb` shipped without it and every
	   candidate's cover rendered at the dialog's full size, on top of the dialog. */
	.cover-img,
	.cover-ph {
		position: absolute;
		inset: 0;
		width: 100%;
		height: 100%;
	}
	/* The same cover tone the `.cover-ph` fallback uses, UNDER the image. Two states
	   otherwise render as a see-through hole rather than an unloaded cover: the moment
	   before the bytes arrive, and the API's "warming" answer on cover-pool saturation —
	   a 200 carrying a 1x1 fully-transparent WebP, which loads fine so `onerror` never
	   fires and there is nothing to fall back to. The server picked transparent so "the
	   card's own CSS placeholder shows through", but MangaCard's `.cover` sets no
	   background of its own, so what showed through was the page. Costs one paint and
	   is covered completely by any real cover. */
	.cover-img {
		background: var(--k-cover);
	}
</style>
