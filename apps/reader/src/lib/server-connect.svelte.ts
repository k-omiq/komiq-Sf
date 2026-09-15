/**
 * "Connect to a server" for the native reader app (see `canChooseServer` in
 * $lib/config): turn what a person typed into a server origin, check that a komiq
 * server answers there, and switch the app over to it.
 */
import { createBackend, type InstanceInfo } from '@komika/api';
import { chosenServer, OFFLINE_WRITES_KEY, saveFetchOnDevice, saveServer } from './config';
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

/** A home-network, VPN or this-computer address: plain HTTP is normal there. */
function isPrivateAddress(host: string): boolean {
	const v4 = host.match(/^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/);
	if (v4) {
		const [a, b] = [Number(v4[1]), Number(v4[2])];
		return (
			a === 10 ||
			a === 127 ||
			(a === 172 && b >= 16 && b <= 31) ||
			(a === 192 && b === 168) ||
			(a === 169 && b === 254) ||
			(a === 100 && b >= 64 && b <= 127)
		);
	}
	return /^\[(::1|f[cd][0-9a-f]{2}:|fe[89ab][0-9a-f]:)/i.test(host);
}

/**
 * The origins to try for what a person typed, best first. A bare IP address with no
 * scheme and no port may be a server that is "public without a domain": it serves
 * HTTPS with its own certificate, so HTTPS is tried first. Plain HTTP is the second
 * try for a home-network address only. For an address on the internet, the app never
 * steps down to plain HTTP by itself: someone on the network path could block HTTPS
 * to make it do that. Plain HTTP there takes a typed `http://` or a port.
 */
export function serverOrigins(raw: string): string[] {
	const origin = toServerOrigin(raw);
	if (!origin) return [];
	const host = raw.trim().split(/[/?#]/)[0];
	if (!/^(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-f:.]+\])$/i.test(host)) return [origin];
	const encrypted = origin.replace(/^http:/, 'https:');
	return isPrivateAddress(host) ? [encrypted, origin] : [encrypted];
}

/**
 * Where a check of `asked` was answered, when a server sent its plain HTTP address on
 * to HTTPS (it turned encryption on). Followed to the same host only, and only from
 * plain HTTP to HTTPS, so that the app then talks to the encrypted address and no
 * password goes to the plain one.
 */
function movedToHttps(asked: string, answered: string): string {
	try {
		const [from, to] = [new URL(asked), new URL(answered)];
		const moved =
			from.protocol === 'http:' && to.protocol === 'https:' && to.hostname === from.hostname;
		return moved ? to.origin : asked;
	} catch {
		return asked;
	}
}

/**
 * Ask `origin` for `/health` and the server's `instance` facts. Answers with those
 * facts and the origin to use, which is the HTTPS one when the server moved there.
 * Throws an Error whose message is meant for the person (no address answered, or not
 * a komiq server).
 */
export async function checkServer(origin: string): Promise<{ origin: string; info: InstanceInfo }> {
	try {
		const health = await fetch(`${origin}/health`, { signal: AbortSignal.timeout(CHECK_MS) });
		if (!health.ok) throw new Error(String(health.status));
		origin = movedToHttps(origin, health.url);
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
	return { origin, info };
}

/**
 * Whether this app can fetch chapters itself (`engine_supported` on the Rust side:
 * desktop today). Where it cannot, there is no choice to offer and the server fetches.
 */
export async function engineSupported(): Promise<boolean> {
	try {
		const { invoke } = await import('@tauri-apps/api/core');
		return await invoke<boolean>('engine_supported');
	} catch {
		return false; // an app build without the command has no engine
	}
}

/**
 * Switch the app to `origin`: remember it and where its chapters are fetched, drop the
 * session of the previous server (a token is only valid where it was issued), and
 * restart on the sign-in page so every client is rebuilt against the new address.
 */
export function useServer(origin: string, fetchOnDevice = false): void {
	saveServer(origin);
	// Always written: the choice made for the previous server must not carry over.
	saveFetchOnDevice(fetchOnDevice);
	try {
		localStorage.removeItem(TOKEN_KEY);
		// Unsent changes were made on the previous server: another one must not get them.
		if (origin !== chosenServer) localStorage.removeItem(OFFLINE_WRITES_KEY);
	} catch {
		/* nothing stored */
	}
	location.assign('/login');
}
