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

import { chapterChip, externalChapterHost, externalChapterHref } from './chapter-label.ts';

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
