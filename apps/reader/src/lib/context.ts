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
import { apiOrigin, chosenServer, config, mediaHosts, OFFLINE_WRITES_KEY } from './config';
import { confirmExtensionInstall } from './extension-consent.svelte';

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
// `publicReads`: a signed-in reader's catalogue reads go out anonymous when that changes
// nothing, so an edge cache can serve them (see `BackendConfig.publicReads`).
const komikaHosted = createBackend({ endpoint: config.apiEndpoint, publicReads: true });
export const backend =
	config.backendKind === 'suwayomi'
		? createSuwayomiBackend({ baseUrl: config.suwayomiUrl })
		: config.nativeEngine && isTauri()
			? createCompositeBackend({
					hosted: komikaHosted,
					// The server names the extensions; the person allows each repository.
					local: createLocalSuwayomiBackend({ confirmInstall: confirmExtensionInstall }),
					// Durable offline write-queue (native only; plan §9): captures failed
					// `mark`/`setProgress` writes and replays them when connectivity returns.
					queue: new OfflineWriteQueue(createLocalStorageAdapter(OFFLINE_WRITES_KEY)),
				})
			: komikaHosted;
export const images = createImageProvider({
	// The server proxy lives on the API origin (the page's own origin when embedded).
	workerBaseUrl: config.imgServerProxy ? apiOrigin : config.imgWorkerBaseUrl,
	direct: config.imgDirect,
	serverProxy: config.imgServerProxy,
	// Cached covers are served from the API origin under `/covers/...`; the provider
	// passes those through directly instead of routing them via the Worker.
	apiOrigin,
	// Native app pointed at a server from the connect screen: load its images from it.
	serverOrigin: chosenServer,
	// Media route origins load as they are, never through the Worker.
	mediaOrigins: mediaHosts,
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
