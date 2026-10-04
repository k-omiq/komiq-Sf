/**
 * Run: node --test --experimental-strip-types src/lib/data/scanlator-ai.test.ts
 *
 * Same fixtures as the server's `a_credit_takes_its_own_tag_else_the_strongest_member_tag`
 * (apps/server/src/scanlators.rs); change both together.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { AI_TITLE, aiIndex, aiLevel, copyAi } from './scanlator-ai.ts';

const index = aiIndex([
	{ name: 'Robo Scans', ai: 'AI' },
	{ name: 'Half Scans', ai: 'PARTIAL' },
	{ name: 'Tiramisu & Strawberry', ai: 'PARTIAL' },
]);

test('a credit takes its own tag, whatever the spelling', () => {
	assert.equal(aiLevel('  robo scans ', index), 'AI');
	assert.equal(aiLevel('Human Scans', index), null);
	assert.equal(aiLevel(null, index), null);
	assert.equal(aiLevel('', index), null);
});

test('a joint credit takes the strongest tag of its members', () => {
	assert.equal(aiLevel('Human Scans & Robo Scans', index), 'AI');
	assert.equal(aiLevel('Half Scans & Human Scans', index), 'PARTIAL');
	assert.equal(aiLevel('Half Scans & Robo Scans', index), 'AI');
});

test('a name that contains the separator is matched whole first', () => {
	assert.equal(aiLevel('Tiramisu & Strawberry', index), 'PARTIAL');
});

test('part of a word is not a member', () => {
	assert.equal(aiLevel('Robo Scans Fans', index), null);
});

test('no tags means no badge', () => {
	assert.equal(aiLevel('Robo Scans', aiIndex([])), null);
	assert.equal(copyAi('Robo Scans', '7001', aiIndex([])), null);
});

// ---- a whole source ----

const both = aiIndex(
	[
		{ name: 'Robo Scans', ai: 'AI' },
		{ name: 'Half Scans', ai: 'PARTIAL' },
	],
	[
		{ sourceId: '7001', ai: 'AI' },
		{ sourceId: '7002', ai: 'PARTIAL' },
	],
);

test('a copy with no credit takes the tag of its source', () => {
	assert.deepEqual(copyAi(null, '7001', both), { level: 'AI', by: 'source' });
	assert.deepEqual(copyAi('', '7002', both), { level: 'PARTIAL', by: 'source' });
	assert.deepEqual(copyAi('Human Scans', '7002', both), { level: 'PARTIAL', by: 'source' });
	assert.equal(copyAi(null, '7003', both), null);
	assert.equal(copyAi(null, null, both), null);
	assert.equal(copyAi(null, undefined, both), null);
});

test('a copy on an untagged source keeps the tag of its credit', () => {
	assert.deepEqual(copyAi('Robo Scans', '7003', both), { level: 'AI', by: 'group' });
	assert.deepEqual(copyAi('Half Scans', null, both), { level: 'PARTIAL', by: 'group' });
	assert.deepEqual(copyAi('Half Scans & Robo Scans', 'mangadex', both), {
		level: 'AI',
		by: 'group',
	});
});

test('the stronger of the two tags wins, and the credit is named on a tie', () => {
	assert.deepEqual(copyAi('Half Scans', '7001', both), { level: 'AI', by: 'source' });
	assert.deepEqual(copyAi('Robo Scans', '7002', both), { level: 'AI', by: 'group' });
	assert.deepEqual(copyAi('Robo Scans', '7001', both), { level: 'AI', by: 'group' });
	assert.deepEqual(copyAi('Half Scans', '7002', both), { level: 'PARTIAL', by: 'group' });
});

test('a source id is matched whole, and never against the group names', () => {
	assert.equal(copyAi(null, '700', both), null);
	assert.equal(copyAi(null, ' 7001 ', both), null);
	assert.equal(copyAi(null, 'Robo Scans', both), null);
	assert.equal(aiLevel('7001', both), null);
});

test('the title names what was tagged', () => {
	assert.equal(AI_TITLE.group.AI, 'This group is marked as AI translated');
	assert.equal(AI_TITLE.group.PARTIAL, 'This group is marked as partly AI translated');
	assert.equal(AI_TITLE.source.AI, 'This source is marked as AI translated');
	assert.equal(AI_TITLE.source.PARTIAL, 'This source is marked as partly AI translated');
});
