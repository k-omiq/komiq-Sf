/**
 * Run: node --test --experimental-strip-types src/lib/data/chapter-read.test.ts
 *
 * The series list's per-chapter read toggle paints optimistically and UNDOES itself when
 * the write fails, so `setChapterRead` must report failure instead of swallowing it the
 * way the reader's fire-and-forget `saveProgress` does — and it must use the exact
 * `setChapterRead` mutation, because `setProgress` can never un-read (the server merges
 * `read` as MAX) and cascades a read to every earlier chapter.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { testGlobals } from './node-test-hooks.ts';

const { setChapterRead } = await import('./source.ts');

console.warn = () => {};

function reset() {
	for (const k of Object.keys(testGlobals.backend)) delete testGlobals.backend[k];
}

test('uses the exact setChapterRead mutation, never setProgress, when the API has it', async () => {
	reset();
	const calls: unknown[][] = [];
	testGlobals.backend.setChapterRead = async (...args: unknown[]) => {
		calls.push(args);
		return true;
	};
	testGlobals.backend.setProgress = async () => {
		throw new Error('setProgress must not be used for the list toggle');
	};
	assert.equal(await setChapterRead('e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0', true), true);
	assert.equal(await setChapterRead('9005', false), true);
	assert.deepEqual(calls, [
		['e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0', true],
		['9005', false],
	]);
});

test('an older API refuses BOTH directions instead of falling back to setProgress', async () => {
	reset();
	let progressCalls = 0;
	testGlobals.backend.setChapterRead = async () => false; // "Unknown field" → false
	testGlobals.backend.setProgress = async () => {
		progressCalls++;
	};
	// setProgress would cascade a read to every earlier chapter and can never un-read, so
	// neither direction may use it: the list undoes its optimistic mark instead.
	assert.equal(await setChapterRead('9005', true), false);
	assert.equal(await setChapterRead('9005', false), false);
	assert.equal(progressCalls, 0);
});

test('a failed write reports failure so the list can undo its optimistic mark', async () => {
	reset();
	testGlobals.backend.setChapterRead = async () => {
		throw new Error('Not authenticated');
	};
	assert.equal(await setChapterRead('9005', true), false);
});

test('no chapter id is a no-op failure, never a request', async () => {
	reset();
	let called = false;
	testGlobals.backend.setChapterRead = async () => {
		called = true;
		return true;
	};
	assert.equal(await setChapterRead('', true), false);
	assert.equal(called, false);
});
