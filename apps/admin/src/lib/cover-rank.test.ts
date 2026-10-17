import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
	coverAmount,
	defaultSourceOrder,
	formatBytes,
	moveItem,
	rankToSave,
	rankedOrder,
	sameOrder,
} from './cover-rank.ts';

const installed = [
	{ id: 22, name: 'Zeta' },
	{ id: '11', name: 'Alpha' },
	{ id: 'mangadex', name: 'MangaDex (EN)' },
];

test('default order puts MangaDex first, then sources by name', () => {
	assert.deepEqual(
		defaultSourceOrder(installed).map((s) => s.id),
		['mangadex', '11', '22'],
	);
});

test('ranked ids come first, then unranked in default order, gone ids keep their id as name', () => {
	const rows = rankedOrder(['22', 'gone', '22'], defaultSourceOrder(installed));
	assert.deepEqual(
		rows.map((s) => s.id),
		['22', 'gone', 'mangadex', '11'],
	);
	assert.deepEqual(rows[1], { id: 'gone', name: 'gone', installed: false });
	assert.equal(rows[0].installed, true);
});

test('an empty ranking is the default order', () => {
	const defaults = defaultSourceOrder(installed);
	assert.deepEqual(rankedOrder([], defaults), defaults);
});

test('moveItem moves within range and ignores out-of-range moves', () => {
	assert.deepEqual(moveItem(['a', 'b', 'c'], 0, 2), ['b', 'c', 'a']);
	assert.deepEqual(moveItem(['a', 'b', 'c'], 2, 0), ['c', 'a', 'b']);
	assert.deepEqual(moveItem(['a', 'b', 'c'], 0, -1), ['a', 'b', 'c']);
	assert.deepEqual(moveItem(['a', 'b', 'c'], 2, 3), ['a', 'b', 'c']);
});

test('sameOrder compares ids in order', () => {
	assert.equal(sameOrder(['a', 'b'], ['a', 'b']), true);
	assert.equal(sameOrder(['a', 'b'], ['b', 'a']), false);
	assert.equal(sameOrder(['a'], ['a', 'b']), false);
});

test('byte and cover amounts read plainly', () => {
	assert.equal(formatBytes(0), '0 B');
	assert.equal(formatBytes(999), '999 B');
	assert.equal(formatBytes(38_200_000), '38.2 MB');
	assert.equal(formatBytes(1_500), '1.5 kB');
	assert.equal(coverAmount(1204, 38_200_000), '1,204 covers, 38.2 MB');
	assert.equal(coverAmount(1, 2_000), '1 cover, 2 kB');
});

test('rankToSave stores only the sources moved out of default order', () => {
	const defaults = defaultSourceOrder(installed); // mangadex, 11, 22
	assert.deepEqual(rankToSave(['mangadex', '11', '22'], defaults, []), []);
	assert.deepEqual(rankToSave(['22', 'mangadex', '11'], defaults, []), ['22']);
	assert.deepEqual(rankToSave(['mangadex', '22', '11'], defaults, []), ['mangadex', '22']);
});

test('rankToSave keeps a stored ranking and gone ids, even in default order', () => {
	const defaults = defaultSourceOrder(installed);
	assert.deepEqual(rankToSave(['mangadex', '11', '22'], defaults, ['mangadex', '11']), [
		'mangadex',
		'11',
	]);
	assert.deepEqual(rankToSave(['gone', 'mangadex', '11', '22'], defaults, ['gone']), ['gone']);
	assert.deepEqual(rankToSave(['mangadex', 'gone', '11', '22'], defaults, []), [
		'mangadex',
		'gone',
	]);
});
