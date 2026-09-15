/**
 * Run: node --test --experimental-strip-types src/lib/data/scanlator-ai.test.ts
 *
 * Same fixtures as the server's `a_credit_takes_its_own_tag_else_the_strongest_member_tag`
 * (apps/server/src/scanlators.rs); change both together.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { aiIndex, aiLevel } from './scanlator-ai.ts';

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
});
