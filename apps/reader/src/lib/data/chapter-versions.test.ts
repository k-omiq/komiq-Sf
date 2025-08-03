/**
 * Run: node --test --experimental-strip-types src/lib/data/chapter-versions.test.ts
 * (from apps/reader; Node 22 strips the types natively — no test runner needed).
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
	NO_GROUP,
	adjacentChapters,
	copiesOf,
	isNumberedLabel,
	normalizeGroup,
	sortChaptersAsc,
	type VersionedChapter,
} from './chapter-versions.ts';

const ch = (id: string, number: number, scanlator: string | null = null, label?: string) =>
	({ id, number, scanlator, label: label ?? String(number) }) satisfies VersionedChapter;

test('a newest-first list still yields prev = the LOWER number', () => {
	// The ordering trap: `list[idx - 1]` over a list that arrives newest-first is the NEXT
	// chapter, and the first chapter of the list (the newest) gets no "next".
	const desc = [ch('57', 57), ch('26', 26), ch('25', 25), ch('24', 24)];
	const { prev, next } = adjacentChapters(desc, desc[2]);
	assert.equal(prev?.id, '24');
	assert.equal(next?.id, '26');
});

test('the first chapter has no prev and the last has no next', () => {
	const list = [ch('25', 25), ch('26', 26)];
	assert.equal(adjacentChapters(list, list[0]).prev, undefined);
	assert.equal(adjacentChapters(list, list[1]).next, undefined);
});

test('a second copy of the SAME number is never prev/next', () => {
	const list = [
		ch('a3', 3, 'Uwaki Scans'),
		ch('b3', 3, 'duaduapertujuh'),
		ch('a4', 4, 'Uwaki Scans'),
	];
	const { prev, next } = adjacentChapters(list, list[1]);
	assert.equal(prev, undefined, 'ch. 3 has no previous number here');
	assert.equal(next?.id, 'a4');
});

test('the adjacent number prefers the open copy’s group, then the default copy', () => {
	const list = [
		ch('u2', 2, 'Uwaki Scans'),
		ch('d2', 2, 'duaduapertujuh'),
		ch('d3', 3, 'duaduapertujuh'),
		ch('u4', 4, 'Uwaki Scans'),
		ch('d4', 4, 'duaduapertujuh'),
		ch('x5', 5, 'Someone Else'),
		ch('u5', 5, 'Other'),
	];
	const open = list[2]; // duaduapertujuh's ch. 3
	const { prev, next } = adjacentChapters(list, open);
	assert.equal(prev?.id, 'd2');
	assert.equal(next?.id, 'd4');
	// No same-group copy of ch. 5 → the DEFAULT copy, not merely the first one.
	const fromU4 = adjacentChapters(list, list[3], (c) => c.id === 'u5');
	assert.equal(fromU4.next?.id, 'u5');
});

test('"No Group" and unknown groups are not an identity to follow', () => {
	const list = [
		ch('n1', 1, NO_GROUP),
		ch('a2', 2, 'Asura'),
		ch('n2', 2, NO_GROUP),
		ch('q2', 2, ''),
	];
	const isDefault = (c: VersionedChapter) => c.id === 'a2';
	assert.equal(adjacentChapters(list, list[0], isDefault).next?.id, 'a2');
	const unknown = ch('u1', 1, null);
	assert.equal(adjacentChapters([unknown, ...list.slice(1)], unknown, isDefault).next?.id, 'a2');
});

test('a target outside the pool (a non-default copy opened by id) is located by number', () => {
	const spineDefaults = [ch('d40', 40, 'A'), ch('d41', 41, 'A'), ch('d42', 42, 'A')];
	const alternate = ch('alt41', 41, 'B');
	const { prev, next } = adjacentChapters(spineDefaults, alternate);
	assert.equal(prev?.id, 'd40');
	assert.equal(next?.id, 'd42');
});

test('oneshots sort after numbered chapters, never as chapter 0', () => {
	const list = [ch('one', 0, null, 'Oneshot'), ch('c1', 1), ch('c0', 0), ch('c2', 2)];
	assert.deepEqual(
		sortChaptersAsc(list).map((c) => c.id),
		['c0', 'c1', 'c2', 'one'],
	);
	assert.equal(adjacentChapters(list, list[3]).next?.id, 'one');
	assert.equal(adjacentChapters(list, list[0]).prev?.id, 'c2');
});

test('copiesOf groups by number and keeps unnumbered chapters apart', () => {
	const list = [ch('a', 7, 'A'), ch('b', 7.0, 'B'), ch('c', 7.5), ch('x', 0, null, 'Extra')];
	assert.deepEqual(
		copiesOf(list, list[1]).map((c) => c.id),
		['a', 'b'],
	);
	assert.deepEqual(
		copiesOf(list, list[3]).map((c) => c.id),
		['x'],
	);
});

test('labels and groups normalise', () => {
	assert.equal(isNumberedLabel('10.5'), true);
	assert.equal(isNumberedLabel('Oneshot'), false);
	assert.equal(isNumberedLabel(undefined), true);
	assert.equal(normalizeGroup(''), null);
	assert.equal(normalizeGroup('  '), null);
	assert.equal(normalizeGroup(' Uwaki Scans '), 'Uwaki Scans');
});
