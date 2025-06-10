<script lang="ts">
	import { images } from '$lib/context';

	let {
		src = '',
		alt = '',
		loading = 'eager',
		fit = 'cover',
	}: {
		src?: string;
		alt?: string;
		/** Native lazy-load hint — pass 'lazy' for offscreen thumbnails (galleries). */
		loading?: 'eager' | 'lazy';
		/** object-fit; 'contain' for a full-cover viewer, 'cover' (default) to fill. */
		fit?: 'cover' | 'contain';
	} = $props();

	// Web: the provider resolves covers synchronously (a pure URL rewrite), so resolve
	// eagerly — including during SSR — so the <img src> is in the server HTML and the
	// cover counts toward LCP instead of appearing only after a browser-only effect.
	// Native (Tauri): resolveCoverSync is absent (covers are async blob URLs), so we
	// fall back to the async effect below.
	const syncResolve = (s: string): string =>
		s && images.resolveCoverSync ? images.resolveCoverSync(s) : '';

	let resolved = $state(syncResolve(src));
	let broken = $state(false);
	let el = $state<HTMLImageElement | undefined>(undefined);

	// A cover failure is usually TRANSIENT — a dropped request on a flaky connection,
	// a cold proxy, a rate-limited upstream. `broken` was only ever cleared when `src`
	// changed, so one such blip left a grey placeholder in that slot for the rest of
	// the page's life, even after the network recovered. Retry a bounded number of
	// times with a backoff (the reader does the same for pages via `retryImage`);
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
	}

	$effect(() => () => clearTimeout(retryTimer));

	$effect(() => {
		const source = src;
		// Web sync path: just track `src` changes; no async round-trip, no blob URL to
		// release. (This branch also re-runs the initial resolve on hydration.)
		if (images.resolveCoverSync) {
			resolved = syncResolve(source);
			resetRetries();
			return;
		}
		// Native async path: fetch the blob URL and release it on teardown.
		resolved = '';
		resetRetries();
		if (!source) return;
		let alive = true;
		images
			.resolveCover(source)
			.then((u) => {
				// The provider mints the object URL before this settles, so a fetch that
				// lands after unmount (or after `src` changed) has already pinned the
				// cover's bytes to the document — and the teardown below never saw it, so
				// nothing would ever release it. Scroll-flinging a grid abandons dozens of
				// these; on iOS that is tens of MB held in a jetsam-limited process.
				if (!alive) {
					if (u && images.release) images.release(u);
					return;
				}
				resolved = u;
			})
			.catch(() => {
				if (alive) broken = true;
			});
		return () => {
			alive = false;
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
	$effect(() => {
		const img = el;
		if (!img || !resolved) return;
		if (img.getAttribute('src') !== resolved) img.setAttribute('src', resolved);
	});
</script>

{#if resolved && !broken}
	{#key attempt}
		<img
			bind:this={el}
			class="cover-img"
			src={resolved}
			{alt}
			{loading}
			decoding="async"
			referrerpolicy="no-referrer"
			style="object-fit:{fit}"
			onerror={onError}
		/>
	{/key}
{:else}
	<div class="cover-ph k-cover"></div>
{/if}

<style>
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
