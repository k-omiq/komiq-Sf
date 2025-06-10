import {
	PUBLIC_KOMIKA_API,
	PUBLIC_KOMIKA_IMG_WORKER,
	PUBLIC_KOMIKA_BACKEND,
	PUBLIC_KOMIKA_BACKEND_KIND,
	PUBLIC_SUWAYOMI_URL,
	PUBLIC_KOMIKA_IMG_MODE,
	PUBLIC_KOMIKA_NATIVE_ENGINE,
	PUBLIC_KOMIKA_SPONSOR_URL,
} from '$env/static/public';

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
export const config = {
	apiEndpoint: PUBLIC_KOMIKA_API,
	imgWorkerBaseUrl: PUBLIC_KOMIKA_IMG_WORKER,
	/**
	 * Whether screens pull from the live backend. `PUBLIC_KOMIKA_BACKEND=on` enables
	 * it; anything else runs standalone against `mock.ts`. The repository still falls
	 * back to empty/mock on error.
	 */
	backendEnabled: PUBLIC_KOMIKA_BACKEND === 'on',
	/**
	 * Which backend implementation to use:
	 *  - 'komika'   → the unified Komika GraphQL API (default, to-spec).
	 *  - 'suwayomi' → a direct Suwayomi/Tachidesk adapter (real catalog now, no social).
	 */
	backendKind: (PUBLIC_KOMIKA_BACKEND_KIND || 'komika') as 'komika' | 'suwayomi',
	/** Suwayomi base URL when backendKind === 'suwayomi'. */
	suwayomiUrl: PUBLIC_SUWAYOMI_URL,
	/**
	 * Image mode: 'direct' returns source URLs unchanged (for already-CORS-safe
	 * hosts like Suwayomi); 'proxy' routes through the Cloudflare image Worker.
	 */
	imgDirect: PUBLIC_KOMIKA_IMG_MODE === 'direct',
	/**
	 * Native content engine (embedded Suwayomi). Off by default. When on AND running
	 * inside Tauri, the reader fetches chapter lists/pages on-device via a
	 * CompositeBackend; otherwise it uses the hosted backend unchanged. The engine
	 * transport itself lands later — this flag only selects the composite wrapper.
	 */
	nativeEngine: PUBLIC_KOMIKA_NATIVE_ENGINE === 'on',
	/**
	 * External donation link. Donations are framed as sponsoring the open-source
	 * project (not the hosted catalog), so this points at GitHub Sponsors by
	 * default. Set PUBLIC_KOMIKA_SPONSOR_URL empty to hide the Sponsor links entirely.
	 */
	sponsorUrl: PUBLIC_KOMIKA_SPONSOR_URL,
};

/**
 * Origin of the Komika API (the GraphQL endpoint minus `/graphql`). User avatars
 * are served from this origin under `/avatars/...`, so a stored `/avatars/x.webp`
 * path is resolved against it. Falls back to the raw endpoint if the shape is
 * unexpected.
 */
export const apiOrigin = config.apiEndpoint.replace(/\/graphql\/?$/, '');

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
