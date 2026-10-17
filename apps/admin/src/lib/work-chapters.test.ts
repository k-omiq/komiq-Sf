import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { WorkChapterEntry } from '@komika/api';
import { entryLabel, entryTargetLabel, moveSortKey, otherEntries } from './work-chapters.ts';

function entry(overrides: Partial<WorkChapterEntry> = {}): WorkChapterEntry {
	return {
		id: 'entry',
		matchKey: null,
		sortKey: 1,
		sortKeyText: '1',
		number: '1',
		label: null,
		special: false,
		specialBy: null,
		hidden: false,
		titleOverride: null,
		copies: [],
		...overrides,
	};
}

function entries(...keys: number[]): WorkChapterEntry[] {
	return keys.map((sortKey, index) =>
		entry({ id: String(index), sortKey, sortKeyText: String(sortKey) }),
	);
}

test('middle moves use the midpoint of the destination neighbours without changing the list', () => {
	const rows = entries(1, 3, 7, 11);
	const snapshot = structuredClone(rows);
	assert.equal(moveSortKey(rows, 2, -1), 2);
	assert.equal(moveSortKey(rows, 1, 1), 9);
	assert.deepEqual(rows, snapshot);
});

test('moves to either end step one past the first or last key', () => {
	const rows = entries(2, 4, 8);
	assert.equal(moveSortKey(rows, 1, -1), 1);
	assert.equal(moveSortKey(rows, 1, 1), 9);
});

test('first up, last down, and missing entries cannot move', () => {
	const rows = entries(1, 2, 3);
	assert.equal(moveSortKey(rows, 0, -1), null);
	assert.equal(moveSortKey(rows, 2, 1), null);
	assert.equal(moveSortKey(entries(1), 0, -1), null);
	assert.equal(moveSortKey(entries(1), 0, 1), null);
	assert.equal(moveSortKey([], 0, 1), null);
	assert.equal(moveSortKey(rows, -1, 1), null);
	assert.equal(moveSortKey(rows, 3, -1), null);
	assert.equal(moveSortKey(rows, 0.5, 1), null);
});

test('equal destination neighbour keys leave no space for a move', () => {
	assert.equal(moveSortKey(entries(1, 1, 2), 2, -1), null);
	assert.equal(moveSortKey(entries(0, 1, 1), 0, 1), null);
});

test('precision exhaustion cannot round a move onto either neighbour', () => {
	const next = 1 + Number.EPSILON;
	assert.equal(moveSortKey(entries(1, next, 2), 2, -1), null);
	assert.equal(moveSortKey(entries(0, 1, next), 0, 1), null);
	const nextAgain = next + Number.EPSILON;
	assert.equal(moveSortKey(entries(next, nextAgain, 2), 2, -1), null);
	assert.equal(moveSortKey(entries(0, next, nextAgain), 0, 1), null);
});

test('moves accept the inclusive bounds and reject keys outside them', () => {
	assert.equal(moveSortKey(entries(-999999, 0), 1, -1), -1000000);
	assert.equal(moveSortKey(entries(0, 999999), 0, 1), 1000000);
	assert.equal(moveSortKey(entries(-1000000, 0), 1, -1), null);
	assert.equal(moveSortKey(entries(0, 1000000), 0, 1), null);
	assert.equal(moveSortKey(entries(-1000001, -1000000, 0), 2, -1), null);
	assert.equal(moveSortKey(entries(0, 1000000, 1000001), 0, 1), null);
});

test('non-finite computed keys cannot move', () => {
	assert.equal(moveSortKey(entries(-Infinity, 0), 1, -1), null);
	assert.equal(moveSortKey(entries(0, Infinity), 0, 1), null);
	assert.equal(moveSortKey(entries(0, NaN), 0, 1), null);
	assert.equal(moveSortKey(entries(-Infinity, Infinity, Infinity), 2, -1), null);
});

test('entry labels prefer the label, then the number, and otherwise read Special', () => {
	assert.equal(entryLabel(entry({ label: '1a', number: '1' })), 'Ch. 1a');
	assert.equal(entryLabel(entry({ number: '0' })), 'Ch. 0');
	assert.equal(entryLabel(entry({ label: '', number: '2.5' })), 'Ch. 2.5');
	assert.equal(entryLabel(entry({ label: 'Extra', number: null })), 'Ch. Extra');
	assert.equal(entryLabel(entry({ number: null })), 'Special');
	assert.equal(entryLabel(entry({ label: '', number: '' })), 'Special');
});

test('entry targets exclude the current entry by id and preserve order without changing the list', () => {
	const rows = entries(1, 2, 3);
	const snapshot = structuredClone(rows);
	assert.deepEqual(otherEntries(rows, '1'), [rows[0], rows[2]]);
	assert.deepEqual(rows, snapshot);
	assert.deepEqual(otherEntries(rows, 'missing'), rows);
	assert.deepEqual(otherEntries([rows[0]], '0'), []);
	assert.deepEqual(otherEntries([], '0'), []);
});

test('entry target labels include the chapter label and prefer an overridden title', () => {
	const copy = {
		chapterId: 'copy',
		sourceSeriesId: 'series',
		sourceType: 'mangadex',
		sourceId: null,
		number: '1',
		title: 'Source title',
		lang: 'en',
		scanlator: null,
		releasedAt: null,
		linkedBy: 'matcher',
	};
	assert.equal(
		entryTargetLabel(entry({ label: '1a', titleOverride: 'Edited title', copies: [copy] })),
		'Ch. 1a — Edited title',
	);
	assert.equal(entryTargetLabel(entry({ copies: [copy] })), 'Ch. 1 — Source title');
	assert.equal(entryTargetLabel(entry({ copies: [{ ...copy, title: null }] })), 'Ch. 1 — Untitled');
	assert.equal(entryTargetLabel(entry({ number: null })), 'Special — Untitled');
});
