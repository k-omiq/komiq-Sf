import {
	PUBLIC_KOMIKA_API,
	PUBLIC_KOMIKA_IMG_WORKER,
	PUBLIC_KOMIKA_BACKEND,
	PUBLIC_KOMIKA_BACKEND_KIND,
	PUBLIC_SUWAYOMI_URL,
	PUBLIC_KOMIKA_IMG_MODE,
	PUBLIC_KOMIKA_NATIVE_ENGINE,
} from '$env/static/public';
import { isTauri } from '@komika/api';

/**
 * App configuration, resolved from PUBLIC_* env vars at BUILD time.
 *
 * These come from `$env/static/public`, so Vite INLINES the values into every
 * bundle (server + client) at build. That is deliberate: on adapter-cloudflare
 * this module is evaluated at edge cold-start with NO request context, where the
 * runtime `$env/dynamic/public` is empty — freezing `apiEndpoint`/`backendEnabled`
 * to their localhost defaults and silently rendering empty feeds. Inlined statics
 * have no such dependency and resolve identically at module scope on the edge, in
 * the Tauri/static SPA, and in the Docker build.
 *
 * The values are supplied per target at build time (all public, non-secret URLs):
 *  - committed `apps/reader/.env`            → dev defaults (localhost), always loaded;
 *  - committed `apps/reader/.env.production`  → prod values (api.komiq.cc, backend on),
 *                                               loaded for the default web `pnpm build`;
 *  - `deploy/reader.Dockerfile` build-args    → self-host overrides (highest priority).
 * `.env` defines every var below so the static imports always resolve (a missing
 * `$env/static/public` export is a hard build error), letting `.env.production` /
 * Docker override only the subset that changes.
 */
/** Which build this is: see `__KOMIKA_TARGET__` in vite.config.ts. */
export const target = __KOMIKA_TARGET__;

/**
 * The embedded build is served by the API server itself (`KOMIKA_WEB_DIR`), so it
 * always talks to its own origin: `/graphql` for data and the same origin for images
 * (covers, avatars and source pages are served or proxied there), with no image
 * Worker. Whatever PUBLIC_* values the build environment holds are ignored, so one
 * build works at any address the server is reached on.
 */
const embedded = target === 'embedded';

/**
 * The native reader app (Tauri, desktop and mobile) can be pointed at any komiq
 * server from its "Connect to a server" screen. Only there: the hosted web build
 * and the embedded build talk to a fixed server, and so does the static SPA when a
 * browser opens it (the Docker reader image), which `isTauri` tells apart at runtime.
 */
export const canChooseServer = target === 'tauri' && isTauri();

/** The server picked on the connect screen (an origin, no path). Same key the
 * desktop admin uses. */
const SERVER_KEY = 'komiq-server';

function savedServer(): string | null {
	if (!canChooseServer) return null;
	try {
		const origin = localStorage.getItem(SERVER_KEY);
		return origin && /^https?:\/\/[^/?#]+$/i.test(origin) ? origin : null;
	} catch {
		return null;
	}
}

/**
 * Remember the server on this device. Every client is built once at load, so the
 * caller reloads the app afterwards.
 */
export function saveServer(origin: string): void {
	try {
		localStorage.setItem(SERVER_KEY, origin);
	} catch {
		/* private mode: the choice lasts until reload only */
	}
}

/** Server chosen on this device, or null for the one the app was built against. */
export const chosenServer = savedServer();

/**
 * True when this native build has no server to talk to yet: nothing chosen and no
 * address baked in at build time. The app then opens on the connect screen.
 */
export const needsServer = canChooseServer && !chosenServer && !PUBLIC_KOMIKA_API;

export const config = {
	apiEndpoint: embedded ? '/graphql' : chosenServer ? `${chosenServer}/graphql` : PUBLIC_KOMIKA_API,
	// A chosen server serves its own images (and proxies source images), so there is
	// no image Worker to route through.
	imgWorkerBaseUrl: embedded || chosenServer ? '' : PUBLIC_KOMIKA_IMG_WORKER,
	/**
	 * Whether screens pull from the live backend. `PUBLIC_KOMIKA_BACKEND=on` enables
	 * it; anything else runs standalone against `mock.ts`. The repository still falls
	 * back to empty/mock on error.
	 */
	backendEnabled: embedded || !!chosenServer || PUBLIC_KOMIKA_BACKEND === 'on',
	/**
	 * Which backend implementation to use:
	 *  - 'komika'   → the unified Komika GraphQL API (default, to-spec).
	 *  - 'suwayomi' → a direct Suwayomi/Tachidesk adapter (real catalog now, no social).
	 */
	backendKind: (embedded || chosenServer ? 'komika' : PUBLIC_KOMIKA_BACKEND_KIND || 'komika') as
		'komika' | 'suwayomi',
	/** Suwayomi base URL when backendKind === 'suwayomi'. */
	suwayomiUrl: PUBLIC_SUWAYOMI_URL,
	/**
	 * Image mode: 'direct' returns source URLs unchanged (for already-CORS-safe
	 * hosts like Suwayomi); 'proxy' routes through the Cloudflare image Worker.
	 */
	imgDirect: embedded || PUBLIC_KOMIKA_IMG_MODE === 'direct',
	/**
	 * Native content engine (embedded Suwayomi). Off by default. When on AND running
	 * inside Tauri, the reader fetches chapter lists/pages on-device via a
	 * CompositeBackend; otherwise it uses the hosted backend unchanged. The engine
	 * transport itself lands later — this flag only selects the composite wrapper.
	 */
	nativeEngine: !embedded && !chosenServer && PUBLIC_KOMIKA_NATIVE_ENGINE === 'on',
};

/**
 * Origin of the Komika API (the GraphQL endpoint minus `/graphql`). User avatars
 * are served from this origin under `/avatars/...`, so a stored `/avatars/x.webp`
 * path is resolved against it. Falls back to the raw endpoint if the shape is
 * unexpected. The embedded build uses the page's own origin.
 */
export const apiOrigin = embedded
	? typeof location === 'undefined'
		? ''
		: location.origin
	: config.apiEndpoint.replace(/\/graphql\/?$/, '');

/**
 * Resolve a stored `avatarUrl` to a loadable URL. Relative `/avatars/...` paths
 * are prefixed with the API origin; already-absolute URLs pass through; null
 * stays null (the UI then renders an initial).
 */
export function avatarSrc(url: string | null | undefined): string | null {
	return apiAssetSrc(url);
}

/**
 * Resolve any stored API-origin asset path (`/avatars/...`, `/comment-media/...`)
 * to a loadable URL: relative paths are prefixed with the API origin, absolute
 * URLs pass through, and null/empty stays null.
 */
export function apiAssetSrc(url: string | null | undefined): string | null {
	if (!url) return null;
	if (/^https?:\/\//.test(url)) return url;
	return apiOrigin + (url.startsWith('/') ? '' : '/') + url;
}
