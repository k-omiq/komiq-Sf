/**
 * The server's Cloudflare Turnstile site key (`instance.turnstileSiteKey`): set when the
 * server asks for a security check on sign-up, sign-in, password changes, comments,
 * reviews and reports. Asked once per visit, by the first form that shows the check.
 */
import { turnstileKeyLoader } from '@komika/api';
import { backend } from './context';

export const turnstile = $state<{ siteKey: string | null }>({ siteKey: null });

const load = turnstileKeyLoader(backend);

/** The key, or `null` when the server asks for no check. A failed request leaves the
 *  key as it was; the next form asks again, and a server that wanted a token says so. */
export async function loadTurnstileKey(): Promise<string | null> {
	try {
		turnstile.siteKey = await load();
	} catch {
		/* asked again by the next form */
	}
	return turnstile.siteKey;
}
