/**
 * A viewer's content block rules: what a rule says in words, and what the "Add rule" form
 * in Settings may send.
 *
 * A rule names tags, content ratings, or both. It matches a series that carries ALL of
 * its tags and, when it names ratings, has ONE of them. A series is hidden when ANY rule
 * matches. The limits and the canonical form are the server's (`block::BlockRule::new`,
 * apps/server/src/block.rs); change both together. The wording of {@link ruleText} is the
 * admin console's and the docs' too.
 *
 * Dependency-free (no backend, no Svelte) so it is unit-testable on its own, like
 * `scanlator-ai.ts`; the settings page owns the fetch and the save.
 */
import type { BlockRule, ContentRating } from '@komika/api';

/** The four content ratings, mildest first: the order a rule keeps them in. */
export const RATINGS: readonly ContentRating[] = ['SAFE', 'SUGGESTIVE', 'EROTICA', 'PORNOGRAPHIC'];

export const RATING_LABEL: Record<ContentRating, string> = {
	SAFE: 'Safe',
	SUGGESTIVE: 'Suggestive',
	EROTICA: 'Erotica',
	PORNOGRAPHIC: 'Pornographic',
};

/** Most rules one viewer can hold. */
export const MAX_RULES = 50;

/** Most tags one rule can name. A series needs every one of them, so a long list
 *  matches almost nothing. */
export const MAX_RULE_TAGS = 8;

/** A rule as the server stores it: tags sorted, ratings mildest first, no repeats. */
export function canonicalRule(rule: BlockRule): BlockRule {
	return {
		tags: [...new Set(rule.tags)].sort(),
		ratings: RATINGS.filter((r) => rule.ratings.includes(r)),
	};
}

/** A rule as one string. Two rules share it only when they hide the same series: the
 *  same tags and the same ratings, in any order. */
export function ruleKey(rule: BlockRule): string {
	const { tags, ratings } = canonicalRule(rule);
	// A tag id is a slug and a rating is a word, so neither separator occurs in them.
	return `${tags.join(',')}/${ratings.join(',')}`;
}

export function sameRule(a: BlockRule, b: BlockRule): boolean {
	return ruleKey(a) === ruleKey(b);
}

/** A list without repeats: the first of each rule, in the order given. The server stores
 *  a list this way; a list keyed by {@link ruleKey} must never hold a repeat. */
export function uniqueRules(rules: readonly BlockRule[]): BlockRule[] {
	const seen = new Set<string>();
	return rules.filter((rule) => {
		const key = ruleKey(rule);
		if (seen.has(key)) return false;
		seen.add(key);
		return true;
	});
}

/**
 * A rule in words. `tagName` gives a tag's display name; a tag the vocabulary does not
 * know (removed since, or the list did not load) is shown by its id.
 *
 *   tags only      `Gore + Harem`
 *   ratings only   `Any series rated Erotica or Pornographic`
 *   both           `Gore + Harem, when rated Erotica or Pornographic`
 */
export function ruleText(rule: BlockRule, tagName: (id: string) => string | undefined): string {
	const tags = rule.tags.map((id) => tagName(id) ?? id).join(' + ');
	// A newer server may know a rating this app does not: show its word.
	const ratings = rule.ratings.map((r) => RATING_LABEL[r] ?? r).join(' or ');
	if (!ratings) return tags;
	return tags ? `${tags}, when rated ${ratings}` : `Any series rated ${ratings}`;
}

/** Why a draft cannot be added to a list. */
export type RuleProblem = 'full' | 'empty' | 'tooManyTags' | 'everything' | 'duplicate';

/** What the form says about a problem. An empty draft has none: nothing is picked yet. */
export const RULE_PROBLEM_TEXT: Record<Exclude<RuleProblem, 'empty'>, string> = {
	full: `You have ${MAX_RULES} rules, and that is the most. Remove one to add another.`,
	tooManyTags: `A rule can name at most ${MAX_RULE_TAGS} tags.`,
	everything: 'A rule with every content rating and no tag would hide everything.',
	duplicate: 'You already have this rule.',
};

/**
 * Whether `draft` can be added to `rules`: null when it can, else the reason. The server
 * refuses the same drafts (`RuleError`), except a repeat, which it stores once.
 */
export function ruleProblem(draft: BlockRule, rules: readonly BlockRule[]): RuleProblem | null {
	if (rules.length >= MAX_RULES) return 'full';
	const rule = canonicalRule(draft);
	if (!rule.tags.length && !rule.ratings.length) return 'empty';
	if (rule.tags.length > MAX_RULE_TAGS) return 'tooManyTags';
	if (!rule.tags.length && rule.ratings.length === RATINGS.length) return 'everything';
	if (rules.some((r) => sameRule(r, rule))) return 'duplicate';
	return null;
}
