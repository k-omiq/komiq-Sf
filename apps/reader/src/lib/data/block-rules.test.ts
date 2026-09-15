/**
 * Run: node --test --experimental-strip-types src/lib/data/block-rules.test.ts
 *
 * The limits and the canonical form mirror `BlockRule::new` (apps/server/src/block.rs);
 * change both together.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { BlockRule } from '@komika/api';
import {
	MAX_RULES,
	MAX_RULE_TAGS,
	RATINGS,
	RULE_PROBLEM_TEXT,
	canonicalRule,
	ruleKey,
	ruleProblem,
	ruleText,
	sameRule,
	uniqueRules,
} from './block-rules.ts';

const NAMES: Record<string, string> = { gore: 'Gore', harem: 'Harem', 'boys-love': "Boys' Love" };
const tagName = (id: string) => NAMES[id];

const rule = (tags: string[], ratings: BlockRule['ratings'] = []): BlockRule => ({ tags, ratings });

test('a rule with tags only reads as its tag names', () => {
	assert.equal(ruleText(rule(['gore']), tagName), 'Gore');
	assert.equal(ruleText(rule(['gore', 'harem']), tagName), 'Gore + Harem');
});

test('a rule with ratings only reads as any series with one of them', () => {
	assert.equal(ruleText(rule([], ['PORNOGRAPHIC']), tagName), 'Any series rated Pornographic');
	assert.equal(
		ruleText(rule([], ['EROTICA', 'PORNOGRAPHIC']), tagName),
		'Any series rated Erotica or Pornographic',
	);
	assert.equal(
		ruleText(rule([], ['SAFE', 'SUGGESTIVE', 'EROTICA']), tagName),
		'Any series rated Safe or Suggestive or Erotica',
	);
});

test('a rule with both reads as the tags, when rated one of the ratings', () => {
	assert.equal(
		ruleText(rule(['gore', 'harem'], ['EROTICA', 'PORNOGRAPHIC']), tagName),
		'Gore + Harem, when rated Erotica or Pornographic',
	);
	assert.equal(ruleText(rule(['boys-love'], ['SAFE']), tagName), "Boys' Love, when rated Safe");
});

test('a tag the vocabulary does not know is shown by its id', () => {
	assert.equal(ruleText(rule(['gore', 'old-tag']), tagName), 'Gore + old-tag');
	assert.equal(
		ruleText(rule(['gore'], ['SAFE']), () => undefined),
		'gore, when rated Safe',
	);
});

test('the canonical form sorts the tags, orders the ratings and drops repeats', () => {
	assert.deepEqual(
		canonicalRule(rule(['harem', 'gore', 'harem'], ['PORNOGRAPHIC', 'SAFE', 'SAFE'])),
		rule(['gore', 'harem'], ['SAFE', 'PORNOGRAPHIC']),
	);
	assert.deepEqual(canonicalRule(rule([])), rule([]));
});

test('the canonical form is a new rule', () => {
	const draft = rule(['harem', 'gore'], ['PORNOGRAPHIC', 'SAFE']);
	canonicalRule(draft);
	assert.deepEqual(draft, rule(['harem', 'gore'], ['PORNOGRAPHIC', 'SAFE']));
});

test('two rules are the same whatever the order of their parts', () => {
	assert.ok(sameRule(rule(['gore', 'harem'], ['EROTICA']), rule(['harem', 'gore'], ['EROTICA'])));
	assert.ok(sameRule(rule([], ['SAFE', 'EROTICA']), rule([], ['EROTICA', 'SAFE'])));
	assert.ok(!sameRule(rule(['gore']), rule(['gore', 'harem'])));
	assert.ok(!sameRule(rule(['gore']), rule(['gore'], ['SAFE'])));
	assert.ok(!sameRule(rule(['gore'], ['SAFE']), rule(['gore'], ['EROTICA'])));
	// One tag is not two tags that spell the same when joined.
	assert.ok(!sameRule(rule(['a-b']), rule(['a', 'b'])));
	// A tag is not a rating of the same spelling.
	assert.notEqual(ruleKey(rule(['safe'])), ruleKey(rule([], ['SAFE'])));
});

test('a list keeps the first of each rule, in order', () => {
	const gore = rule(['gore', 'harem'], ['EROTICA']);
	const adult = rule([], ['PORNOGRAPHIC']);
	assert.deepEqual(uniqueRules([gore, adult, rule(['harem', 'gore'], ['EROTICA']), adult]), [
		gore,
		adult,
	]);
	assert.deepEqual(uniqueRules([]), []);
});

test('a draft needs a tag or a rating', () => {
	assert.equal(ruleProblem(rule([]), []), 'empty');
	assert.equal(ruleProblem(rule(['gore']), []), null);
	assert.equal(ruleProblem(rule([], ['EROTICA']), []), null);
	assert.equal(ruleProblem(rule(['gore'], ['EROTICA']), []), null);
});

test('a draft can name at most eight tags', () => {
	const tags = Array.from({ length: MAX_RULE_TAGS + 1 }, (_, i) => `tag-${i}`);
	assert.equal(MAX_RULE_TAGS, 8);
	assert.equal(ruleProblem(rule(tags.slice(0, MAX_RULE_TAGS)), []), null);
	assert.equal(ruleProblem(rule(tags), []), 'tooManyTags');
	// A repeat is one tag.
	assert.equal(ruleProblem(rule([...tags.slice(0, MAX_RULE_TAGS), 'tag-0']), []), null);
});

test('every rating at once needs a tag', () => {
	assert.equal(ruleProblem(rule([], [...RATINGS]), []), 'everything');
	assert.equal(ruleProblem(rule(['gore'], [...RATINGS]), []), null);
	assert.equal(ruleProblem(rule([], RATINGS.slice(0, 3)), []), null);
});

test('a draft that is already in the list is a duplicate', () => {
	const rules = [rule(['gore', 'harem'], ['EROTICA']), rule([], ['PORNOGRAPHIC'])];
	assert.equal(ruleProblem(rule(['harem', 'gore'], ['EROTICA']), rules), 'duplicate');
	assert.equal(ruleProblem(rule([], ['PORNOGRAPHIC']), rules), 'duplicate');
	assert.equal(ruleProblem(rule(['gore']), rules), null);
	assert.equal(ruleProblem(rule(['gore', 'harem']), rules), null);
});

test('a full list takes no more rules, whatever the draft', () => {
	const rules = Array.from({ length: MAX_RULES }, (_, i) => rule([`tag-${i}`]));
	assert.equal(MAX_RULES, 50);
	assert.equal(ruleProblem(rule(['gore']), rules), 'full');
	assert.equal(ruleProblem(rule([]), rules), 'full');
	assert.equal(ruleProblem(rule(['gore']), rules.slice(1)), null);
});

test('every problem but an empty draft has something to say', () => {
	assert.deepEqual(Object.keys(RULE_PROBLEM_TEXT).sort(), [
		'duplicate',
		'everything',
		'full',
		'tooManyTags',
	]);
	assert.match(RULE_PROBLEM_TEXT.full, /50 rules/);
	assert.match(RULE_PROBLEM_TEXT.tooManyTags, /at most 8 tags/);
});
