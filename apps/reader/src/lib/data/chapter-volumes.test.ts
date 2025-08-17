/**
 * Run: node --test --experimental-strip-types src/lib/data/chapter-volumes.test.ts
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { volumeHeaders } from './chapter-volumes.ts';

test('a header opens wherever the volume changes, in the order given', () => {
	// Newest-first, as the default sort renders it.
	assert.deepEqual(volumeHeaders(['3', '3', '2', '2', '1']), [
		'Volume 3',
		null,
		'Volume 2',
		null,
		'Volume 1',
	]);
});

test('no volume anywhere means no headers at all', () => {
	assert.deepEqual(volumeHeaders([null, undefined, '', '  ']), [null, null, null, null]);
	assert.deepEqual(volumeHeaders([]), []);
});

test('unlabelled rows next to labelled ones read "No volume", not part of the volume above', () => {
	// Fresh chapters not collected into a volume yet sit above the last tankōbon.
	assert.deepEqual(volumeHeaders([null, null, '12', '12']), ['No volume', null, 'Volume 12', null]);
});

test('labels are trimmed, and a volume split by the sort gets a header each time', () => {
	assert.deepEqual(volumeHeaders([' 1 ', '1', '2', '1']), [
		'Volume 1',
		null,
		'Volume 2',
		'Volume 1',
	]);
});
