import type { KitConfig } from '@sveltejs/kit';

type CspDirectives = NonNullable<NonNullable<KitConfig['csp']>['directives']>;
type ConnectSource = NonNullable<CspDirectives['connect-src']>[number];

/** Network destinations used by the hosted reader, derived from its public build config. */
export function readerConnectSources(
	env: Record<string, string | undefined>,
	development = false,
): ConnectSource[] {
	const sources = new Set<ConnectSource>(['self']);
	function addEndpoint(key: string): void {
		const value = env[key]?.trim();
		if (!value) return;
		// Same-origin API paths need no extra permission.
		if (value.startsWith('/') && !value.startsWith('//')) return;
		let url: URL;
		try {
			url = new URL(value);
		} catch {
			throw new Error(`${key} must be an absolute HTTP(S) URL or a same-origin path`);
		}
		if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) {
			throw new Error(`${key} must use HTTP(S) without embedded credentials`);
		}
		// URL parsing validated the origin; Kit models host sources as template literals.
		sources.add(url.origin as ConnectSource);
	}
	if (env.PUBLIC_KOMIKA_BACKEND === 'on') {
		// The hosted backend also supplies account/social features in Suwayomi mode.
		addEndpoint('PUBLIC_KOMIKA_API');
		if (env.PUBLIC_KOMIKA_BACKEND_KIND === 'suwayomi') addEndpoint('PUBLIC_SUWAYOMI_URL');
		// Media route probes (`/media/v1/ready`) are fetches; the objects are `<img>`s,
		// which `img-src https:` already allows.
		else for (const origin of mediaHostOrigins(env.PUBLIC_KOMIKA_MEDIA_HOSTS)) sources.add(origin);
	}
	if (env.PUBLIC_KOMIKA_IMG_MODE === 'proxy') addEndpoint('PUBLIC_KOMIKA_IMG_WORKER');
	if (development) {
		// Vite's development WebSocket can use a different port or remote device host.
		sources.add('ws:');
		sources.add('wss:');
	}
	return [...sources];
}

/**
 * `PUBLIC_KOMIKA_MEDIA_HOSTS` as origins: comma- or space-separated, each a bare http(s)
 * origin (the same rule as `parseMediaHosts` in `@komika/api`, which the app uses).
 */
function mediaHostOrigins(raw: string | undefined): ConnectSource[] {
	const out: ConnectSource[] = [];
	for (const entry of (raw ?? '').split(/[\s,]+/).filter(Boolean)) {
		let url: URL | null = null;
		try {
			url = new URL(entry);
		} catch {
			/* reported below */
		}
		if (
			!url ||
			!['http:', 'https:'].includes(url.protocol) ||
			url.username ||
			url.password ||
			url.pathname !== '/' ||
			url.search ||
			url.hash
		) {
			throw new Error(`PUBLIC_KOMIKA_MEDIA_HOSTS: "${entry}" is not an http(s) origin`);
		}
		out.push(url.origin as ConnectSource);
	}
	return out;
}
