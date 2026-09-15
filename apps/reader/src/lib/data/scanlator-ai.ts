/**
 * Which chapter copies an admin marked as AI translated.
 *
 * The server keeps one tag per scanlator NAME (`scanlatorTags`), not per chapter, so the
 * reader resolves a copy's credit against the tag list itself. The rule is the server's
 * (`scanlators::effective_level`); change both together:
 *
 *   1. the credit's own tag, compared trimmed and case-folded;
 *   2. else, for a joint credit ("A & B"), the strongest tag among its members:
 *      `AI` over `PARTIAL`.
 *
 * Dependency-free (no backend, no Svelte) so it is unit-testable on its own, like
 * `chapter-versions.ts`; `$lib/scanlator-ai.svelte` owns the fetch.
 */
import type { ScanlatorAi, ScanlatorTag } from '@komika/api';

/** Tag by folded name. */
export type AiIndex = ReadonlyMap<string, ScanlatorAi>;

/** How MangaDex joins the groups of a joint release. */
const JOINT_SEPARATOR = ' & ';

export const AI_LABEL: Record<ScanlatorAi, string> = {
	AI: 'AI',
	PARTIAL: 'Partial AI',
};
export const AI_TITLE: Record<ScanlatorAi, string> = {
	AI: 'This group is marked as AI translated',
	PARTIAL: 'This group is marked as partly AI translated',
};

function nameKey(name: string): string {
	return name.trim().toLowerCase();
}

export function aiIndex(tags: readonly ScanlatorTag[]): AiIndex {
	return new Map(tags.map((t) => [nameKey(t.name), t.ai]));
}

/** The tag that applies to a copy's credit; null when it has none (or no credit). */
export function aiLevel(group: string | null | undefined, index: AiIndex): ScanlatorAi | null {
	if (!group || index.size === 0) return null;
	const key = nameKey(group);
	const own = index.get(key);
	if (own) return own;
	let level: ScanlatorAi | null = null;
	for (const member of key.split(JOINT_SEPARATOR)) {
		const tag = index.get(member.trim());
		if (tag === 'AI') return 'AI';
		if (tag) level = tag;
	}
	return level;
}
