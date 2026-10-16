import { MediaRouter, mediaOrigin, trustedMediaRoutes, type RoutedPage } from '@komika/api';
import type { MediaRoute, Page } from '@komika/types';
import { backend, images } from './context';
import { apiOrigin, mediaHosts, target } from './config';

/**
 * Typed media routes (M5) for the hosted web build. With a non-empty
 * `PUBLIC_KOMIKA_MEDIA_HOSTS`, pages that carry an exact object (`mediaPath`) load it from
 * the session's media route; everything else loads `sourceUrl` through the image provider
 * as before. Exact object URLs never pass through the provider.
 */
// `mediaHosts` is already empty outside a web build on the komiq backend.
const routesOn = mediaHosts.length > 0;

/** The route list may hold up the first chapter this long; then the owner serves alone. */
const ROUTES_DEADLINE_MS = 3000;
/** A failed embedded owner lookup may be retried after this cooldown. */
const OWNER_RETRY_MS = 30_000;
/** The one fresh refresh after an owner failure; then the retry button. */
const REFRESH_DEADLINE_MS = 15_000;

function within<T>(promise: Promise<T>, ms: number, late: () => T): Promise<T> {
	return new Promise((resolve, reject) => {
		const timer = setTimeout(() => {
			try {
				resolve(late());
			} catch (error) {
				reject(error);
			}
		}, ms);
		promise.then(
			(value) => {
				clearTimeout(timer);
				resolve(value);
			},
			(error) => {
				clearTimeout(timer);
				reject(error);
			},
		);
	});
}

let routerPromise: Promise<MediaRouter | null> | undefined;
let routerNow: MediaRouter | null = null;

function sessionStore(): Storage | null {
	try {
		return typeof sessionStorage === 'undefined' ? null : sessionStorage;
	} catch {
		return null;
	}
}

/**
 * The session's media router, or null when this build does not use media routes or the
 * server predates `mediaRoutes`. A failed route list falls back to the owner alone (the
 * API origin), which is always trusted. Probes start in the background; nothing waits
 * for them.
 */
export function mediaRouter(): Promise<MediaRouter | null> {
	if (!routesOn || typeof window === 'undefined' || !backend.mediaRoutes) {
		return Promise.resolve(null);
	}
	routerPromise ??= within(backend.mediaRoutes(), ROUTES_DEADLINE_MS, (): MediaRoute[] => [])
		.catch((): MediaRoute[] => [])
		.then((listed) => {
			if (!listed) return null;
			const router = new MediaRouter({
				routes: trustedMediaRoutes(listed, mediaHosts, apiOrigin),
				storage: sessionStore(),
			});
			void router.probe();
			routerNow = router;
			return router;
		});
	return routerPromise;
}

/** The router once {@link mediaRouter} has resolved (the reader page needs it in sync). */
export function currentMediaRouter(): MediaRouter | null {
	return routerNow;
}

/** One page as the reader loads it: the exact object on the current route, else the
 *  provider's URL for `sourceUrl`. */
export async function routePage(page: Page, router: MediaRouter | null): Promise<RoutedPage> {
	await ownerOriginReady();
	const media = router?.pageMedia(page) ?? null;
	if (media && router) return { url: router.href(media), media };
	return { url: await images.resolvePage(page), media: null };
}

/** The chapter's pages again with every HTTP cache skipped (POST), routed: the one refresh
 *  after an exact object failed on the owner too. */
export async function freshRoutedPages(chapterId: string): Promise<RoutedPage[]> {
	const router = await mediaRouter();
	const pages = await within(backend.pages(chapterId, { fresh: true }), REFRESH_DEADLINE_MS, () => {
		throw new Error('page refresh timed out');
	});
	return Promise.all(
		pages.map((p) => routePage(p, router).catch(() => ({ url: '', media: null }))),
	);
}

let ownerOriginPromise: Promise<string | null> | undefined;
let ownerOriginNow: string | null = null;
let ownerOriginRetryAt = 0;
let ownerOriginWait: Promise<string | null> | undefined;

/**
 * Embedded build only: the owner's absolute media origin from `mediaRoutes`, or null when
 * the owner is this same origin (base `''`), the server predates the query, or the lookup
 * failed. The embedded build reads `mediaRoutes` for
 * this alone (to rebase owner URLs onto its own origin when it is served on another
 * address); it never routes pages.
 */
export function embeddedOwnerOrigin(): Promise<string | null> {
	if (target !== 'embedded' || typeof window === 'undefined' || !backend.mediaRoutes) {
		return Promise.resolve(null);
	}
	if (Date.now() < ownerOriginRetryAt) return Promise.resolve(ownerOriginNow);
	ownerOriginPromise ??= Promise.resolve()
		.then(() => backend.mediaRoutes!())
		.then((listed) => {
			const base = listed?.find((r) => r.kind === 'OWNER')?.base;
			const owner = base ? mediaOrigin(base) : null;
			if (owner) images.setRebase?.(owner, window.location.origin);
			ownerOriginNow = owner;
			return owner;
		})
		.catch(() => {
			ownerOriginRetryAt = Date.now() + OWNER_RETRY_MS;
			ownerOriginPromise = undefined;
			return null;
		});
	return ownerOriginPromise;
}

/** Wait once at startup without discarding a late discovery and its rebase. */
export function ownerOriginReady(): Promise<string | null> {
	if (target !== 'embedded' || typeof window === 'undefined' || !backend.mediaRoutes) {
		return Promise.resolve(null);
	}
	const discovery = embeddedOwnerOrigin();
	ownerOriginWait ??= within(discovery, ROUTES_DEADLINE_MS, () => null);
	return ownerOriginWait.then(() => ownerOriginNow);
}
