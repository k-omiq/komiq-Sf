/**
 * Run: node --test --experimental-strip-types src/lib/data/reader-pages.test.ts
 * (from apps/reader; Node 22 strips the types natively — no test runner needed).
 *
 * The regression these guard: opening a multi-source work (One Piece: a MangaDex spine copy
 * plus a "MANGA Plus" Suwayomi copy) with the Suwayomi engine down rendered an EMPTY reader —
 * "Ch. 0", no title, no chapter list, no pickers. `pages("95")` answered a GraphQL
 * "Internal error", the rejection escaped the page fetch, and `getReaderChapter`'s `live()`
 * wrapper threw away the whole resolved view for `emptyReader`.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { loadFirstReadable } from './reader-pages.ts';

interface Copy {
	id: string;
	src: string | null;
}
const MANGA_PLUS: Copy = { id: '95', src: '1998944621602463790' };
const MANGADEX: Copy = { id: 'e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0', src: null };

const deadEngine = () => Promise.reject(new Error('Internal error'));

test('a dead secondary source falls back to the MangaDex copy of the same chapter', async () => {
	const errors: string[] = [];
	const got = await loadFirstReadable(
		[MANGA_PLUS, MANGADEX],
		(c) => (c.src ? deadEngine() : Promise.resolve(['p1', 'p2'])),
		(c) => errors.push(c.id),
	);
	assert.equal(got.copy, MANGADEX);
	assert.deepEqual(got.pages, ['p1', 'p2']);
	assert.equal(got.failed, false);
	assert.deepEqual(errors, ['95']);
});

test('when every copy fails it RESOLVES (never rejects) with the first copy and no pages', async () => {
	// The bug was precisely a rejection here: it is what turned one dead source into a blank
	// reader. The caller must still get a copy to build the full view around.
	const got = await loadFirstReadable([MANGADEX, MANGA_PLUS], deadEngine);
	assert.equal(got.copy, MANGADEX);
	assert.deepEqual(got.pages, []);
	assert.equal(got.failed, true);
});

test('the first readable copy wins and later copies are never fetched', async () => {
	const asked: string[] = [];
	const got = await loadFirstReadable([MANGADEX, MANGA_PLUS], async (c) => {
		asked.push(c.id);
		return ['p'];
	});
	assert.equal(got.copy, MANGADEX);
	assert.deepEqual(asked, [MANGADEX.id]);
});

test('an empty copy falls through, but an all-empty chapter is not reported as a failure', async () => {
	const fallback = await loadFirstReadable([MANGA_PLUS, MANGADEX], async (c) =>
		c.src ? [] : ['p'],
	);
	assert.equal(fallback.copy, MANGADEX);

	const allEmpty = await loadFirstReadable([MANGA_PLUS, MANGADEX], async () => []);
	assert.equal(allEmpty.copy, MANGA_PLUS);
	assert.equal(allEmpty.failed, false);
});
