/**
 * Run: node --test --experimental-strip-types src/lib/data/chapter-read.test.ts
 *
 * The series list's per-chapter read toggle paints optimistically and UNDOES itself when
 * the write fails, so `setChapterRead` must report failure instead of swallowing it the
 * way the reader's fire-and-forget `saveProgress` does.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { testGlobals } from './node-test-hooks.ts';

const { setChapterRead } = await import('./source.ts');

console.warn = () => {};

test('a landed write reports success and sends (id, page 0, read)', async () => {
	const calls: unknown[][] = [];
	testGlobals.backend.setProgress = async (...args: unknown[]) => {
		calls.push(args);
	};
	assert.equal(await setChapterRead('e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0', true), true);
	assert.equal(await setChapterRead('9005', false), true);
	assert.deepEqual(calls, [
		['e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0', 0, true],
		['9005', 0, false],
	]);
});

test('a failed write reports failure so the list can undo its optimistic mark', async () => {
	testGlobals.backend.setProgress = async () => {
		throw new Error('Not authenticated');
	};
	assert.equal(await setChapterRead('9005', true), false);
});

test('no chapter id is a no-op failure, never a request', async () => {
	let called = false;
	testGlobals.backend.setProgress = async () => {
		called = true;
	};
	assert.equal(await setChapterRead('', true), false);
	assert.equal(called, false);
});
