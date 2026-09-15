import { PUBLIC_KOMIKA_API } from '$env/static/public';

/** Where this build runs: see `ADMIN_TARGET` in vite.config.ts. */
export const target = __ADMIN_TARGET__;

/**
 * The server the desktop app chose on `/start`: a remote origin (no path), or
 * `local` for the server the app runs on this computer.
 */
const SERVER_KEY = 'komiq-server';
const LOCAL = 'local';
/**
 * The admin session token. The app keeps one, and a token is only for the server that
 * issued it: choosing another server drops it (`saveServer`, `saveLocalServer`), or the
 * next request would hand it to that other server.
 */
export const TOKEN_KEY = 'komika-admin-token';
/**
 * Where the app's own server answers in THIS run of the app. Session-only on purpose:
 * every launch starts that server again through `/start`, which records it here.
 */
const LOCAL_ADDRESS_KEY = 'komiq-local-address';

function read(storage: () => Storage, key: string): string | null {
	try {
		return storage().getItem(key);
	} catch {
		return null;
	}
}

function write(storage: () => Storage, key: string, value: string | null): void {
	try {
		if (value === null) storage().removeItem(key);
		else storage().setItem(key, value);
	} catch {
		/* private mode: the choice lasts until reload only */
	}
}

const local = () => localStorage;
const session = () => sessionStorage;

function savedServer(): string | null {
	const saved = read(local, SERVER_KEY);
	return saved === LOCAL ? read(session, LOCAL_ADDRESS_KEY) : saved;
}

/**
 * Remember a remote server for the desktop app. The backend is built once at load, so
 * the caller reloads the app after saving. `sameServer`: the server only moved to a new
 * address (`followServer`), so the session stays.
 */
export function saveServer(origin: string, sameServer = false): void {
	if (!sameServer && read(local, SERVER_KEY) !== origin) write(local, TOKEN_KEY, null);
	write(local, SERVER_KEY, origin);
	write(session, LOCAL_ADDRESS_KEY, null);
}

/**
 * Remember "run komiq on this computer", with the address the app's server answers
 * on now. Later launches start that server by themselves (`/start`). Reload after.
 */
export function saveLocalServer(address: string): void {
	if (read(local, SERVER_KEY) !== LOCAL) write(local, TOKEN_KEY, null);
	write(local, SERVER_KEY, LOCAL);
	write(session, LOCAL_ADDRESS_KEY, address);
}

/**
 * The server moved to `address` ("public without a domain" turned HTTPS on). The
 * desktop app follows it, once it answers there: its old plain HTTP address stops
 * answering from the internet. Reloads the app when it follows. The console a server
 * serves itself cannot follow (another origin): see `HttpsMove.svelte`.
 *
 * Resolves false when the app asked there and got no answer: it stays on the address
 * that works, and the owner must be told (`moveTo` in `server-move.svelte.ts`).
 * `beforeReload` runs when the app is about to follow: the last moment to save where
 * the owner is. `wanted` is asked once the server answered: false when a later move
 * took over, and then the app does not follow this one.
 *
 * `typed` is the address the owner entered. The app follows to that host only: the
 * answer that names `address` came over plain HTTP, where someone on the network path
 * could have changed it to send the app, and its session, to another server.
 */
export async function followServer(
	address: string,
	typed: string,
	beforeReload?: () => void,
	wanted: () => boolean = () => true,
): Promise<boolean> {
	if (target !== 'desktop' || runsLocalServer) return true;
	if (!address.startsWith('https://') || address === serverAddress()) return true;
	try {
		if (new URL(address).hostname !== typedHostname(typed)) return true;
	} catch {
		return true;
	}
	if (!(await answers(address))) return false;
	if (!wanted()) return true;
	saveServer(address, true);
	beforeReload?.();
	location.reload();
	return true;
}

/** The host of an address as its owner typed it: `203.0.113.5:8080`, `2001:db8::1`. */
function typedHostname(typed: string): string {
	const entered = typed.trim();
	if (entered.includes('://')) return new URL(entered).hostname;
	try {
		return new URL(`http://${entered}`).hostname;
	} catch {
		// A bare IPv6 address: a URL wants it in brackets.
		return new URL(`http://[${entered}]`).hostname;
	}
}

/**
 * Whether the server answers at `address`. The HTTPS port and a firewall rule can need
 * a few seconds, so it asks four times, 1, 2 and 4 seconds apart. Each try waits 5
 * seconds at most: 27 seconds when nothing ever answers.
 */
async function answers(address: string): Promise<boolean> {
	for (const wait of [0, 1000, 2000, 4000]) {
		if (wait) await new Promise((r) => setTimeout(r, wait));
		try {
			const health = await fetch(`${address}/health`, { signal: AbortSignal.timeout(5000) });
			if (health.ok) return true;
		} catch {
			/* port 443 is not open yet */
		}
	}
	return false;
}

/** Forget the desktop app's server (back to `/start`). */
export function forgetServer(): void {
	write(local, SERVER_KEY, null);
	write(session, LOCAL_ADDRESS_KEY, null);
}

/** The desktop app runs its server on this computer (chosen on `/start` before). */
export const runsLocalServer = target === 'desktop' && read(local, SERVER_KEY) === LOCAL;

/**
 * True when the desktop app has no server to talk to yet: none chosen, or its own
 * server is not started in this run.
 */
export const needsServer = target === 'desktop' && !savedServer();

function endpoint(): string {
	// Served by the API server itself: always same origin, whatever the build env says.
	if (target === 'embedded') return '/graphql';
	if (target === 'desktop') {
		const saved = savedServer();
		if (saved) return `${saved}/graphql`;
	}
	return PUBLIC_KOMIKA_API || 'http://localhost:8080/graphql';
}

/**
 * Admin console config. The console only makes sense against the unified Komika
 * API (it needs the admin mutations + auth), so it always targets that endpoint.
 *
 * The hosted build resolves it at BUILD time from `$env/static/public`, so Vite
 * inlines the value into the bundle. This must NOT use `$env/dynamic/public`: the
 * console ships as an adapter-static SPA served straight off the edge with no
 * SvelteKit server, so the runtime `env` object is always empty there and every
 * request silently fell back to the localhost default. `apps/admin/.env.production`
 * supplies the prod endpoint. The desktop build reads the server chosen on `/start`.
 */
export const config = {
	apiEndpoint: endpoint(),
};

/** Origin serving the API's own static assets (`/covers/…`, `/avatars/…`). */
const apiOrigin = config.apiEndpoint.replace(/\/graphql\/?$/, '');

/** The server's public address as a person would type it (no `/graphql`). */
export function serverAddress(): string {
	return apiOrigin || (typeof location === 'undefined' ? '' : location.origin);
}

/**
 * Whether another device can open `address` (so a QR code of it is worth showing):
 * anything but this computer's own loopback.
 */
export function reachableFromOtherDevices(address: string): boolean {
	return (
		/^https?:\/\//i.test(address) &&
		!/^https?:\/\/(localhost|127(\.\d{1,3}){3}|\[::1\])(:\d+)?(\/|$)/i.test(address)
	);
}

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
