/**
 * Run: node --test --experimental-strip-types src/lib/data/chapter-owner.test.ts
 * (from apps/reader; Node 22 strips the types natively — no test runner needed).
 *
 * The regression these cover is the one that shipped: a reader opened at a chapter
 * belonging to a NON-default source rendered `pages: []`, so it never requested an
 * image and looked exactly like a broken image proxy.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { findChapterOwner, type ChapterCandidate } from './chapter-owner.ts';

/** The real shape of the bug: work w_436b98…, chapter 632413 lives in Asura only. */
const WORK: ChapterCandidate[] = [
	{ key: 'suwayomi:10423', suwayomiMangaId: '10423', chapters: ids(['900', '901']) }, // Arven, most chapters -> default
	{ key: 'suwayomi:662', suwayomiMangaId: '662', chapters: ids(['700']) }, // Qi
	{ key: 'suwayomi:211', suwayomiMangaId: '211', chapters: ids(['632413']) }, // Asura
];

function ids(list: string[]) {
	return list.map((id) => ({ id }));
}

test('finds a chapter carried by a source OTHER than the selected one', () => {
	const owner = findChapterOwner(WORK, '632413', 'suwayomi:10423');
	assert.ok(owner, 'must not give up — this rendered a blank reader in production');
	assert.equal(owner.candidate.key, 'suwayomi:211');
	assert.equal(owner.candidate.suwayomiMangaId, '211');
	assert.equal(owner.chapter.id, '632413');
	assert.equal(owner.switched, true);
});

test('prefers the selected source when it carries the chapter', () => {
	// Same id present in two sources: the reader's own choice must win, so an
	// ordinary read never switches source (and never re-lists) as a side effect.
	const dupe: ChapterCandidate[] = [
		{ key: 'a', suwayomiMangaId: 'a', chapters: ids(['dup']) },
		{ key: 'b', suwayomiMangaId: 'b', chapters: ids(['dup']) },
	];
	const owner = findChapterOwner(dupe, 'dup', 'b');
	assert.equal(owner?.candidate.key, 'b');
	assert.equal(owner?.switched, false);
});

test('returns null when no source carries the id (stays honest)', () => {
	// Must NOT fall back to "some other chapter" — an unknown id degrades to empty.
	assert.equal(findChapterOwner(WORK, 'does-not-exist', 'suwayomi:10423'), null);
});

test('preserves a null suwayomiMangaId so the spine keeps its page resolver', () => {
	// null selects canonicalPages downstream; collapsing it to a string would route
	// MangaDex chapters through the Suwayomi resolver and fetch nothing.
	const withSpine: ChapterCandidate[] = [
		{ key: 'mangadex', suwayomiMangaId: null, chapters: ids(['uuid-1']) },
		{ key: 'suwayomi:5', suwayomiMangaId: '5', chapters: ids(['5001']) },
	];
	const owner = findChapterOwner(withSpine, 'uuid-1', 'suwayomi:5');
	assert.equal(owner?.candidate.suwayomiMangaId, null);
	assert.equal(owner?.switched, true);
});

test('works with no selection at all (first carrier wins, in given order)', () => {
	const owner = findChapterOwner(WORK, '632413', null);
	assert.equal(owner?.candidate.key, 'suwayomi:211');
	assert.equal(owner?.switched, true);
});

test('empty chapter id resolves to null rather than matching anything', () => {
	assert.equal(findChapterOwner(WORK, '', 'suwayomi:10423'), null);
});
