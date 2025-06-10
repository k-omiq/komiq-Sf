import {
	createBackend,
	createCompositeBackend,
	createLocalStorageAdapter,
	createLocalSuwayomiBackend,
	createSuwayomiBackend,
	createImageProvider,
	currentPlatform,
	isTauri,
	OfflineWriteQueue,
} from '@komika/api';
import { apiOrigin, config } from './config';

/**
 * App-wide singletons. The UI only ever talks to these two seams:
 *  - `backend` — the data backend (catalog, library, social, auth). The unified
 *                Komika API, a direct Suwayomi adapter, or — when the native
 *                engine flag is on inside Tauri — a CompositeBackend that can
 *                serve content on-device while delegating everything else to the
 *                hosted API. Selected per config.
 *  - `images`  — resolves source image URLs; automatically Worker-proxied on web
 *                and Rust-fetched-direct on native (Tauri). Components never know
 *                which platform they're on.
 */
const komikaHosted = createBackend({ endpoint: config.apiEndpoint });
export const backend =
	config.backendKind === 'suwayomi'
		? createSuwayomiBackend({ baseUrl: config.suwayomiUrl })
		: config.nativeEngine && isTauri()
			? createCompositeBackend({
					hosted: komikaHosted,
					local: createLocalSuwayomiBackend(),
					// Durable offline write-queue (native only; plan §9): captures failed
					// `mark`/`setProgress` writes and replays them when connectivity returns.
					queue: new OfflineWriteQueue(createLocalStorageAdapter('komika.offlineWrites')),
				})
			: komikaHosted;
export const images = createImageProvider({
	workerBaseUrl: config.imgWorkerBaseUrl,
	direct: config.imgDirect,
	// Cached covers are served from the API origin under `/covers/...`; the provider
	// passes those through directly instead of routing them via the Worker.
	apiOrigin,
});
export const platform = currentPlatform();

// Restore any persisted session token onto the backend SYNCHRONOUSLY at module
// load — before the first SvelteKit `load()` runs. Page `load`s (e.g. the library
// and profile) fetch per-user data immediately, but `initAuth` (which sets the
// token) runs later in a layout effect; without this, that first fetch would go
// out unauthenticated and a signed-in user would briefly see an empty library /
// no profile. `initAuth` still validates the token afterwards and populates
// `auth.user`; a stale token just yields empty results until it clears.
try {
	const token = typeof localStorage !== 'undefined' ? localStorage.getItem('komika-token') : null;
	if (token) backend.setToken?.(token);
} catch {
	/* private mode / no storage — initAuth will still restore it shortly */
}
