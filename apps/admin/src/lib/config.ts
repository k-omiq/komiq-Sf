import { env } from '$env/dynamic/public';

/**
 * Admin console config. The console only makes sense against the unified Komika
 * API (it needs the admin mutations + auth), so it always targets that endpoint.
 */
export const config = {
	apiEndpoint: env.PUBLIC_KOMIKA_API ?? 'http://localhost:8080/graphql',
};

/** Origin serving the API's own static assets (`/covers/…`, `/avatars/…`). */
const apiOrigin = config.apiEndpoint.replace(/\/graphql\/?$/, '');

/**
 * Resolve a server-supplied image URL against the API ORIGIN.
 *
 * `Series.coverUrl` comes back as a root-relative `/covers/{workId}.webp?v=…`, which
 * is relative to the API — not to the console. Rendering it verbatim resolves it
 * against `admin.komiq.cc`, where nothing serves `/covers/*`; worse, the Worker's
 * `not_found_handling = "single-page-application"` answers with `index.html`, so the
 * `<img>` gets a 200 of HTML and every cover renders broken rather than 404-ing
 * visibly. Absolute URLs (external source covers, extension icons) pass through.
 *
 * The CSP in `static/_headers` allows both shapes (`img-src 'self' https: data:`), so
 * this is purely about which origin the path resolves against.
 */
export function assetSrc(url: string | null | undefined): string {
	if (!url) return '';
	return /^([a-z][a-z0-9+.-]*:)?\/\//i.test(url) || url.startsWith('data:')
		? url
		: `${apiOrigin}${url}`;
}
