/**
 * Whether a universal `load` should STREAM its data (hand the page a pending
 * promise, so it shows skeletons) or AWAIT it (hand the page a resolved value).
 *
 * The obvious test — `browser ? stream : await` — is wrong, and was the cause of a
 * visible flash on every cold load of the home and updates routes. Universal loads
 * re-run in the browser during HYDRATION, unconditionally: SvelteKit's client
 * `_hydrate` walks the same `load_node` path a client-side navigation does, with no
 * "we already have this from the server" guard. So on a cold load the sequence was:
 *
 *   edge SSR renders real cards → hydration re-runs `load` → `browser === true` →
 *   data is a pending promise → the page hydrates its SKELETON branch against DOM
 *   holding the grid branch → mismatch, server nodes discarded → skeletons → the
 *   fetch lands → cards again.
 *
 * A large layout shift plus a duplicated request, on the one path that was supposed
 * to be fastest. What we actually want is: await while there is server-rendered
 * markup to preserve (the hydration run), stream on every later navigation.
 *
 * Hydration is exactly "the browser `load` runs that happen before the app has
 * mounted" — every later run is a user-initiated navigation. The root layout calls
 * {@link markHydrated} from an effect, which Svelte flushes after hydration
 * completes and therefore after the initial `load` has already resolved.
 *
 * When the build has SSR off (Tauri/static — see `$lib/ssr`) there is no
 * server-rendered markup to protect and awaiting would just hold a blank screen, so
 * the browser always streams.
 */
import { browser } from '$app/environment';
import { SSR_PUBLIC } from './ssr';

let hydrated = false;

/** Called once from the root layout, after the app has mounted. */
export function markHydrated(): void {
	hydrated = true;
}

/** True when `load` should return the promise unawaited. */
export function shouldStream(): boolean {
	if (!browser) return false; // server: always await, or SSR emits permanent skeletons
	if (!SSR_PUBLIC) return true; // no server-rendered markup to preserve
	return hydrated;
}
