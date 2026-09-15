/**
 * The server's public facts (`instance` query): whether it still needs its owner,
 * and whether anyone may sign up. Loaded at most once per visit.
 *
 * Every failure is SILENT and leaves `info` null, which every caller reads as "carry
 * on as before": a server that predates the query, the Suwayomi backend (no
 * `instance` at all), or a network blip must never hide sign-up or block the app.
 */
import type { InstanceInfo } from '@komika/api';
import { backend } from './context';
import { canChooseFetch, fetchOnDevice, saveFetchOnDevice } from './config';

export const instance = $state<{ info: InstanceInfo | null; ready: boolean }>({
	info: null,
	ready: false,
});

let pending: Promise<void> | null = null;

/** Never hold the app longer than this for an answer. */
const WAIT_MS = 5000;

export function loadInstance(): Promise<void> {
	pending ??= (async () => {
		let timer: ReturnType<typeof setTimeout> | undefined;
		try {
			if (backend.instance) {
				const gaveUp = new Promise<null>((resolve) => {
					timer = setTimeout(() => resolve(null), WAIT_MS);
				});
				instance.info = await Promise.race([backend.instance(), gaveUp]);
				// The server does not fetch for apps and this one is still set to ask it (the
				// server changed its mind, or the app was built for it and never went through
				// the connect screen): store "this device". Clients are built once at load,
				// so it takes effect at the next start instead of restarting the app mid-use.
				if (canChooseFetch && !fetchOnDevice && fetchLocked()) saveFetchOnDevice(true);
			}
		} catch {
			instance.info = null;
		} finally {
			clearTimeout(timer);
		}
		instance.ready = true;
	})();
	return pending;
}

/** True when the server leaves no choice: apps fetch chapters themselves. */
export function fetchLocked(): boolean {
	return instance.info?.appsFetchThemselves === true;
}

/** False only when the server says sign-up is closed. */
export function registrationOpen(): boolean {
	return instance.info?.registrationOpen !== false;
}
