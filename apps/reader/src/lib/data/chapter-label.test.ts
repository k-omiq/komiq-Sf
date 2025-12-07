/**
 * Run: node --test --experimental-strip-types src/lib/data/chapter-label.test.ts
 * (from apps/reader; Node 22 strips the types natively — no test runner needed).
 *
 * Two regressions these guard:
 *  • A chapter COUNT printed as a chapter NUMBER — "Ch. 412" on a series whose newest
 *    release is 10.5. The fix is a deletion, so what is left to test is that the ONLY
 *    surviving fallback is the empty string, and that a word label ("Oneshot") is never
 *    dressed up as "Ch. Oneshot".
 *  • An off-site chapter URL reaching an `href` unvalidated — ~35,000 chapters carry one,
 *    and it is upstream-controlled text.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
	chapterChip,
	chapterSourceUrl,
	externalChapterHost,
	externalChapterHref,
} from './chapter-label.ts';

test('a numeric label gets the Ch. prefix', () => {
	assert.equal(chapterChip('45'), 'Ch. 45');
	// Half chapters are 35,091 real rows on the Suwayomi side — the decimal must survive.
	assert.equal(chapterChip('10.5'), 'Ch. 10.5');
	// `Chapter 0` / `Prologue` are legitimate; a `<= 0` filter would drop them.
	assert.equal(chapterChip('0'), 'Ch. 0');
});

test('a word label is printed verbatim, never as "Ch. <word>"', () => {
	// The 5 non-numeric labels in a 600-row live sample of /updates.
	assert.equal(chapterChip('Oneshot'), 'Oneshot');
	assert.equal(chapterChip('Brosquito'), 'Brosquito');
	assert.equal(chapterChip('Extra'), 'Extra');
});

test('a blank label renders nothing rather than inventing a number', () => {
	assert.equal(chapterChip(null), '');
	assert.equal(chapterChip(undefined), '');
	assert.equal(chapterChip(''), '');
	assert.equal(chapterChip('   '), '');
});

test('an already-prefixed label is not double-prefixed', () => {
	// getUpdates re-parses a formatted chip back into a label when it merges the two feed
	// halves; without the strip, that round trip renders "Ch. Ch. 45".
	assert.equal(chapterChip('Ch. 45'), 'Ch. 45');
	assert.equal(chapterChip('Ch.45'), 'Ch. 45');
	assert.equal(chapterChip('Chapter 45'), 'Ch. 45');
});

test('a word label that merely starts with "Chapter" keeps its own words', () => {
	assert.equal(chapterChip('Chapter Zero'), 'Chapter Zero');
});

test('a multi-chapter label is not printed as one number', () => {
	// `Chapter 13,14` is a real upstream label. It is not chapter 13.
	assert.equal(chapterChip('13,14'), '13,14');
	assert.equal(chapterChip('45 and 46'), '45 and 46');
});

test('http(s) external URLs pass through', () => {
	assert.equal(
		externalChapterHref('https://mangaplus.shueisha.co.jp/viewer/1019123'),
		'https://mangaplus.shueisha.co.jp/viewer/1019123',
	);
	assert.equal(externalChapterHref('http://comikey.com/read/x'), 'http://comikey.com/read/x');
	assert.equal(externalChapterHref('  https://namicomi.com/x  '), 'https://namicomi.com/x');
});

test('non-http schemes are rejected — the value is upstream-controlled', () => {
	assert.equal(externalChapterHref('javascript:alert(1)'), null);
	assert.equal(externalChapterHref('data:text/html,<script>alert(1)</script>'), null);
	assert.equal(externalChapterHref('JavaScript:alert(1)'), null);
});

test('missing, blank and relative values yield no link', () => {
	assert.equal(externalChapterHref(null), null);
	assert.equal(externalChapterHref(undefined), null);
	assert.equal(externalChapterHref(''), null);
	assert.equal(externalChapterHref('   '), null);
	assert.equal(externalChapterHref('/read/w_abc'), null);
	assert.equal(externalChapterHref('not a url'), null);
});

test('the host names the destination, without the www. noise', () => {
	assert.equal(
		externalChapterHost('https://mangaplus.shueisha.co.jp/viewer/1019123'),
		'mangaplus.shueisha.co.jp',
	);
	assert.equal(
		externalChapterHost('https://www.bilibilicomics.com/mc123/456'),
		'bilibilicomics.com',
	);
	assert.equal(externalChapterHost('javascript:alert(1)'), '');
	assert.equal(externalChapterHost(null), '');
});

// --- "Read on <host>" (chapterSourceUrl) -------------------------------------------------

const MD_UUID = 'E9FB431B-5A4D-4BF8-9C4A-0510C5ECA6D0';

test('a MangaDex spine copy links to its mangadex.org chapter page', () => {
	const href = chapterSourceUrl({ id: MD_UUID }, null);
	assert.equal(href, 'https://mangadex.org/chapter/e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0');
	assert.equal(externalChapterHost(href), 'mangadex.org');
});

test('an off-site copy links to its validated external URL, whatever its source', () => {
	const ext = 'https://mangaplus.shueisha.co.jp/viewer/1000486';
	assert.equal(chapterSourceUrl({ id: MD_UUID, externalUrl: ext }, null), ext);
	assert.equal(chapterSourceUrl({ id: '9005', externalUrl: ext }, '777'), ext);
	assert.equal(externalChapterHost(ext), 'mangaplus.shueisha.co.jp');
});

test('a Suwayomi copy has no known chapter URL, so the link hides', () => {
	assert.equal(chapterSourceUrl({ id: '9005' }, '777'), null);
	// Even a uuid-shaped id: `src` says it is not the MangaDex spine.
	assert.equal(chapterSourceUrl({ id: MD_UUID }, '777'), null);
});

test('a non-uuid id without a source never becomes a dead MangaDex link', () => {
	// A numeric Suwayomi-only series reads with `src === null` too.
	assert.equal(chapterSourceUrl({ id: '9005' }, null), null);
	assert.equal(chapterSourceUrl({ id: '../admin' }, null), null);
	assert.equal(chapterSourceUrl({ id: `${MD_UUID}/../x` }, null), null);
});

test('a hostile external URL is rejected, and the MangaDex fallback is NOT used for it', () => {
	assert.equal(chapterSourceUrl({ id: '9005', externalUrl: 'javascript:alert(1)' }, '777'), null);
	assert.equal(chapterSourceUrl({ id: '9005', externalUrl: 'data:text/html,x' }, null), null);
	// An invalid external URL on a MangaDex copy falls back to the copy's own MangaDex page.
	assert.equal(
		chapterSourceUrl({ id: MD_UUID, externalUrl: 'javascript:alert(1)' }, null),
		'https://mangadex.org/chapter/e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0',
	);
});
