import { invalidateAll } from '$app/navigation';
import { navigating } from '$app/state';

/** Changes whenever results belonging to the viewer cease to be reusable. */
export const viewerCache = $state({ generation: 0, readerGeneration: 0 });

let refresh: Promise<void> | undefined;

/**
 * Kit also keeps completed preloads outside our data caches. Drop those and refresh
 * the visible page after a transition, without stealing an active navigation's token.
 * Reader views carry this generation too, so an old preload is hidden while we wait.
 */
export function refreshViewerLoads(): Promise<void> {
	return (refresh ??= (async () => {
		// Auth effects may start a goto in the same microtask flush. Let it publish
		// its navigation before invalidating, then wait on its own completion.
		await new Promise<void>((resolve) => setTimeout(resolve, 0));
		let generation: number;
		do {
			generation = viewerCache.generation;
			while (navigating.to && navigating.complete) {
				const complete = navigating.complete;
				await complete.catch(() => {});
				// An aborted navigation can leave `to` set; its settled promise is enough.
				if (navigating.complete === complete) break;
			}
			await invalidateAll();
		} while (generation !== viewerCache.generation);
	})()
		.catch((err) => console.warn('[komika] viewer refresh failed:', err))
		.finally(() => {
			refresh = undefined;
		}));
}

/** Auth, content preferences, or an explicit read-state change invalidated viewer data. */
export function viewerCacheChanged(): void {
	viewerCache.generation += 1;
	void refreshViewerLoads();
}
