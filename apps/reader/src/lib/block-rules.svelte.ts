import type { BlockRule } from '@komika/api';
import { auth } from './auth.svelte';
import { backend } from './context';
import { viewerCacheChanged } from './viewer-cache.svelte';

/**
 * How many times the signed-in viewer changed their block rules in this session.
 *
 * A rule change changes what every list may show the viewer, and nothing else about them
 * says so: the user id and the NSFW setting stay as they were. A list that keeps rows in
 * memory across navigations (Browse's back-nav cache) puts `n` in its key, so rows
 * fetched under the old rules are never restored. Settings counts up after each save.
 */
export const blockRulesVersion = $state({ n: 0 });

/**
 * Call after the rules were saved: lists fetched under the old rules must not come back.
 *
 * Browse keys its back-nav cache on the version. `invalidateAll` runs the `load` of the
 * page on screen again and drops a list page SvelteKit preloaded when a link to it was
 * hovered. It must not run while a navigation is in flight: an invalidation aborts that
 * navigation and leaves `navigating` set (see `refreshLibraryIfShown` in
 * $lib/data/source). The page being opened can have been read under the old rules, so
 * the invalidation is not dropped then. It waits for the navigation to end.
 */
export function blockRulesChanged(): void {
	blockRulesVersion.n += 1;
	viewerCacheChanged();
}

/**
 * Save the viewer's whole rule list and invalidate what was read under the old one.
 *
 * Like `saveShowNsfw`, the owner is the account that asked: a write the server applied
 * still invalidates after an unrelated viewer-cache change, and never applies to a
 * different signed-in account. Resolves to the stored list, or `null` when it no longer
 * applies to the viewer on screen.
 */
export async function saveBlockRules(next: BlockRule[]): Promise<BlockRule[] | null> {
	if (!backend.setMyBlockRules) return null;
	const owner = auth.user?.id;
	const saved = await backend.setMyBlockRules(next);
	if (auth.user?.id !== owner) return null;
	blockRulesChanged();
	return saved;
}
