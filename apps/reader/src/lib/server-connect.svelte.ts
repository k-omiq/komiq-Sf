/**
 * "Connect to a server" for the native reader app (see `canChooseServer` in
 * $lib/config): turn what a person typed into a server origin, check that a komiq
 * server answers there, and switch the app over to it.
 */
import { createBackend, type InstanceInfo } from '@komika/api';
import { saveServer } from './config';
import { TOKEN_KEY } from './auth.svelte';

/** Open from the sign-in page ("Change server"); first run opens it by itself. */
export const connectScreen = $state({ open: false });

/** How long a check may take before the address counts as not answering. */
const CHECK_MS = 8000;

/**
 * "192.168.1.20:8080", "komiq.example.com", or a pasted link such as
 * "https://komiq.example.com/admin/setup" → a bare origin, or null.
 *
 * Without a scheme, a home-network address (an IP, `localhost`, a `.local` name, or
 * anything with an explicit port) gets `http://`; a plain domain gets `https://`,
 * which is how a server behind Cloudflare is reached.
 */
export function toServerOrigin(raw: string): string | null {
	let value = raw.trim();
	if (!value) return null;
	if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(value)) {
		const host = value.split(/[/?#]/)[0];
		const home =
			/:\d+$/.test(host) ||
			/^(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-f:.]+\]|localhost)$/i.test(host) ||
			/\.local$/i.test(host);
		value = `${home ? 'http' : 'https'}://${value}`;
	}
	try {
		const url = new URL(value);
		if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;
		if (!url.hostname) return null;
		return url.origin;
	} catch {
		return null;
	}
}

/**
 * Ask `origin` for `/health` and the server's `instance` facts. Throws an Error whose
 * message is meant for the person (no address answered, or not a komiq server).
 */
export async function checkServer(origin: string): Promise<InstanceInfo> {
	try {
		const health = await fetch(`${origin}/health`, { signal: AbortSignal.timeout(CHECK_MS) });
		if (!health.ok) throw new Error(String(health.status));
	} catch {
		throw new Error(
			'No komiq server answered at this address. Check that it is running and that this device can reach it.',
		);
	}
	const backend = createBackend({ endpoint: `${origin}/graphql` });
	let info: InstanceInfo | undefined;
	try {
		info = await backend.instance?.();
	} catch {
		info = undefined;
	}
	if (!info) {
		throw new Error(
			'This address answers, but not as a komiq server this app can use. Update the server, or check the address.',
		);
	}
	return info;
}

/**
 * Switch the app to `origin`: remember it, drop the session of the previous server
 * (a token is only valid where it was issued), and restart on the sign-in page so
 * every client is rebuilt against the new address.
 */
export function useServer(origin: string): void {
	saveServer(origin);
	try {
		localStorage.removeItem(TOKEN_KEY);
	} catch {
		/* nothing stored */
	}
	location.assign('/login');
}
