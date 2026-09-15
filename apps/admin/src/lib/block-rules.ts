/**
 * Content block rules: their wording, their limits and the checks the server also
 * makes. Pure logic, kept out of the Settings page so the page stays readable.
 */
import type { BlockRule, ContentRating } from '@komika/api';
import type { TagGroup, TagInfo } from '@komika/types';

/** Mildest first: the order the server keeps a rule's ratings in. */
export const RATINGS: readonly ContentRating[] = ['SAFE', 'SUGGESTIVE', 'EROTICA', 'PORNOGRAPHIC'];

export const RATING_LABEL: Record<ContentRating, string> = {
	SAFE: 'Safe',
	SUGGESTIVE: 'Suggestive',
	EROTICA: 'Erotica',
	PORNOGRAPHIC: 'Pornographic',
};

/** The server refuses a rule with more tags, and a list with more rules. */
export const MAX_RULE_TAGS = 8;
export const MAX_RULES = 50;

/** A rule the way the server stores it: each tag once and sorted, ratings mildest first. */
export function tidyRule(rule: BlockRule): BlockRule {
	return {
		tags: [...new Set(rule.tags)].sort(),
		ratings: RATINGS.filter((r) => rule.ratings.includes(r)),
	};
}

/** True when both rules match the same series, whatever order their parts are in. */
export function sameRule(a: BlockRule, b: BlockRule): boolean {
	const x = tidyRule(a);
	const y = tidyRule(b);
	// A tag id is a slug, so it holds no space and no comma.
	return x.tags.join(' ') === y.tags.join(' ') && x.ratings.join() === y.ratings.join();
}

/**
 * A rule in words. The reader app and the docs use the same wording, so change all
 * three together. A tag `tagName` does not know is shown by its id.
 */
export function ruleText(rule: BlockRule, tagName: (id: string) => string | undefined): string {
	const tags = rule.tags.map((id) => tagName(id) ?? id).join(' + ');
	const ratings = rule.ratings.map((r) => RATING_LABEL[r] ?? r).join(' or ');
	if (!ratings) return tags;
	return tags ? `${tags}, when rated ${ratings}` : `Any series rated ${ratings}`;
}

export interface DraftCheck {
	/** The server would take the rule, and the list has room for it. */
	ok: boolean;
	/** Why not, in words for the form. Null for an empty draft: there is nothing to say yet. */
	reason: string | null;
}

/** Whether `draft` can be added to `rules`. The same checks the server makes. */
export function checkDraft(draft: BlockRule, rules: BlockRule[]): DraftCheck {
	const no = (reason: string | null): DraftCheck => ({ ok: false, reason });
	if (rules.length >= MAX_RULES) {
		return no(`The list is full. A server keeps at most ${MAX_RULES} rules.`);
	}
	const rule = tidyRule(draft);
	if (rule.tags.length === 0 && rule.ratings.length === 0) return no(null);
	if (rule.tags.length > MAX_RULE_TAGS) return no(`A rule takes at most ${MAX_RULE_TAGS} tags.`);
	if (rule.tags.length === 0 && rule.ratings.length === RATINGS.length) {
		return no('A rule with every rating and no tag would block every series.');
	}
	if (rules.some((r) => sameRule(r, rule))) return no('This rule is already in the list.');
	return { ok: true, reason: null };
}

const TAG_GROUPS: readonly TagGroup[] = ['GENRE', 'THEME', 'FORMAT', 'CONTENT'];
const TAG_GROUP_LABEL: Record<TagGroup, string> = {
	GENRE: 'Genre',
	THEME: 'Theme',
	FORMAT: 'Format',
	CONTENT: 'Content',
};

export interface TagShelf {
	label: string;
	tags: TagInfo[];
}

/**
 * The tag picker's options: the tags not picked yet, by group, A to Z inside a group.
 * A group with nothing left to pick is left out.
 */
export function tagShelves(tags: TagInfo[], picked: string[]): TagShelf[] {
	const free = tags.filter((t) => !picked.includes(t.id));
	return TAG_GROUPS.map((group) => ({
		label: TAG_GROUP_LABEL[group],
		tags: free.filter((t) => t.group === group).sort((a, b) => a.name.localeCompare(b.name)),
	})).filter((shelf) => shelf.tags.length > 0);
}
