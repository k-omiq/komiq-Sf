/**
 * The scanlators and the sources an admin tagged as using AI, loaded once per session.
 *
 * Every chapter row and every copy in the reader's drawer asks the same question ("is
 * this copy tagged?"), so the answer is two small lists held here rather than a field on
 * each chapter: each list is a handful of entries, it is the same for every viewer, and
 * the server caches it at the edge. {@link AiBadge} draws the answer.
 *
 * Browser only. On the server render the index stays empty, so the page hydrates
 * without badges and they appear when the lists land; a badge sits inside a row that
 * already has its width, so nothing moves.
 */
import type { ScanlatorTag, SourceAiTag } from '@komika/api';
import { backend } from '$lib/context';
import { config } from '$lib/config';
import { AI_LABEL, aiIndex, copyAi, type AiIndex } from '$lib/data/scanlator-ai';

export const scanlatorAi = $state<{ index: AiIndex }>({ index: aiIndex([]) });

// Each list as it last arrived; null until it has.
let groups: ScanlatorTag[] | null = null;
let sources: SourceAiTag[] | null = null;
let loading: Promise<void> | null = null;

/** Fetch the two tag lists, once. Never rejects. Each list fails on its own: a server
 *  with no source tags still gives the group badges. The next caller asks again for a
 *  list that failed. */
export function loadScanlatorAi(): Promise<void> {
	if (!config.backendEnabled || typeof window === 'undefined') return Promise.resolve();
	loading ??= (async () => {
		// Asked inside `then`, so a call that throws settles as a failure like one that
		// rejects. A backend without the query, or a reply without the list, is no tags.
		const [g, s] = await Promise.allSettled([
			groups ?? Promise.resolve().then(() => backend.scanlatorTags?.()),
			sources ?? Promise.resolve().then(() => backend.sourceAiTags?.()),
		]);
		if (g.status === 'fulfilled') groups = g.value ?? [];
		else console.warn('[komika] scanlator tags unavailable:', g.reason);
		if (s.status === 'fulfilled') sources = s.value ?? [];
		else console.warn('[komika] source AI tags unavailable:', s.reason);
		scanlatorAi.index = aiIndex(groups ?? [], sources ?? []);
		if (!groups || !sources) loading = null;
	})();
	return loading;
}

/**
 * Whether a copy gets a badge. For a row that has a place for the badge only when there
 * is one: a chapter with no group credit shows no attribution at all, unless its source
 * is tagged. Does not start the fetch; such a row mounts no {@link AiBadge} to do it, so
 * its page calls {@link loadScanlatorAi} itself.
 */
export function hasAiBadge(
	group: string | null | undefined,
	sourceId: string | null | undefined,
): boolean {
	return copyAi(group, sourceId, scanlatorAi.index) !== null;
}

/**
 * The badge in words, for a control whose `aria-label` replaces its text: the label is
 * then the control's whole name, and the badge inside it is not read out. Empty when
 * the copy has no badge.
 */
export function aiSpoken(
	group: string | null | undefined,
	sourceId: string | null | undefined,
): string {
	const mark = copyAi(group, sourceId, scanlatorAi.index);
	return mark ? `, ${AI_LABEL[mark.level]}` : '';
}
