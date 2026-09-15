/**
 * The scanlators an admin tagged as using AI, loaded once per session.
 *
 * Every chapter row and every copy in the reader's drawer asks the same question ("is
 * this group tagged?"), so the answer is one small list held here rather than a field on
 * each chapter: the list is a handful of names, it is the same for every viewer, and the
 * server caches it at the edge. {@link AiBadge} is the only consumer.
 *
 * Browser only. On the server render the index stays empty, so the page hydrates
 * without badges and they appear when the list lands; a badge sits inside a row that
 * already has its width, so nothing moves.
 */
import { backend } from '$lib/context';
import { config } from '$lib/config';
import { aiIndex, type AiIndex } from '$lib/data/scanlator-ai';

export const scanlatorAi = $state<{ index: AiIndex }>({ index: new Map() });

let loading: Promise<void> | null = null;

/** Fetch the tag list, once. Never rejects: on failure there are no badges, and the next
 *  caller tries again. */
export function loadScanlatorAi(): Promise<void> {
	if (!config.backendEnabled || typeof window === 'undefined') return Promise.resolve();
	loading ??= (async () => {
		try {
			scanlatorAi.index = aiIndex((await backend.scanlatorTags?.()) ?? []);
		} catch (err) {
			console.warn('[komika] scanlator tags unavailable:', err);
			loading = null;
		}
	})();
	return loading;
}
