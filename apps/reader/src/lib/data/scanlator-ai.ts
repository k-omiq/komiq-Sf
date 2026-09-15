/**
 * Which chapter copies an admin marked as AI translated.
 *
 * The server keeps two tag lists, neither of them per chapter: one tag per scanlator NAME
 * (`scanlatorTags`) and one per SOURCE (`sourceAiTags`). A source that translates its own
 * series credits no group, so there the source is the scanlator. The reader resolves a
 * copy against both lists itself. The rule is the server's (`scanlators::effective_level`
 * for a credit); change both together:
 *
 *   1. the credit's own tag, compared trimmed and case-folded;
 *   2. else, for a joint credit ("A & B"), the strongest tag among its members:
 *      `AI` over `PARTIAL`;
 *   3. the tag on the copy's source id, compared as is;
 *   4. the copy takes the stronger of the credit's tag (1 or 2) and the source's (3).
 *
 * Dependency-free (no backend, no Svelte) so it is unit-testable on its own, like
 * `chapter-versions.ts`; `$lib/scanlator-ai.svelte` owns the fetch.
 */
import type { ScanlatorAi, ScanlatorTag, SourceAiTag } from '@komika/api';

/** Both tag lists: groups by folded name, sources by id. */
export interface AiIndex {
	groups: ReadonlyMap<string, ScanlatorAi>;
	sources: ReadonlyMap<string, ScanlatorAi>;
}

/** What tagged a copy: its group credit or the source it is read from. */
export type AiOrigin = 'group' | 'source';

/** The tag that applies to one copy, and which list it came from (the badge's title). */
export interface CopyAi {
	level: ScanlatorAi;
	by: AiOrigin;
}

/** How MangaDex joins the groups of a joint release. */
const JOINT_SEPARATOR = ' & ';

export const AI_LABEL: Record<ScanlatorAi, string> = {
	AI: 'AI',
	PARTIAL: 'Partial AI',
};
export const AI_TITLE: Record<AiOrigin, Record<ScanlatorAi, string>> = {
	group: {
		AI: 'This group is marked as AI translated',
		PARTIAL: 'This group is marked as partly AI translated',
	},
	source: {
		AI: 'This source is marked as AI translated',
		PARTIAL: 'This source is marked as partly AI translated',
	},
};

function nameKey(name: string): string {
	return name.trim().toLowerCase();
}

export function aiIndex(
	groups: readonly ScanlatorTag[],
	sources: readonly SourceAiTag[] = [],
): AiIndex {
	return {
		groups: new Map(groups.map((t) => [nameKey(t.name), t.ai])),
		sources: new Map(sources.map((t) => [t.sourceId, t.ai])),
	};
}

/** The tag that applies to a copy's credit; null when it has none (or no credit). */
export function aiLevel(group: string | null | undefined, index: AiIndex): ScanlatorAi | null {
	if (!group || index.groups.size === 0) return null;
	const key = nameKey(group);
	const own = index.groups.get(key);
	if (own) return own;
	let level: ScanlatorAi | null = null;
	for (const member of key.split(JOINT_SEPARATOR)) {
		const tag = index.groups.get(member.trim());
		if (tag === 'AI') return 'AI';
		if (tag) level = tag;
	}
	return level;
}

/**
 * The tag that applies to one copy: the stronger of its credit's tag and its source's.
 * On a tie the credit is named, because it says more about this copy than its source
 * does. Null when neither is tagged.
 */
export function copyAi(
	group: string | null | undefined,
	sourceId: string | null | undefined,
	index: AiIndex,
): CopyAi | null {
	const byGroup = aiLevel(group, index);
	const bySource = (sourceId && index.sources.get(sourceId)) || null;
	if (bySource && (!byGroup || (bySource === 'AI' && byGroup === 'PARTIAL'))) {
		return { level: bySource, by: 'source' };
	}
	return byGroup ? { level: byGroup, by: 'group' } : null;
}
