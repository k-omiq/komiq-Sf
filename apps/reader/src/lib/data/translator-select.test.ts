/**
 * Run: node --test --experimental-strip-types src/lib/data/translator-select.test.ts
 * (from apps/reader; Node 22 strips the types natively — no test runner needed).
 *
 * The regression these guard: MangaDex content was served through the all.mangadex
 * Suwayomi extension (page images proxied via api.komiq.cc) instead of the MangaDex-direct
 * spine (mangadex.network), because the default picked the source with the MOST chapters
 * and the extension often out-counts a partially-stripped spine. That funnelled reads onto
 * the origin proxy and made the image Worker unstable.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
	MANGADEX_EXT_PKG,
	isRedundantMangadexExt,
	maxSaneChapterNumber,
	pickDefaultKey,
	type Selectable,
} from './translator-select.ts';

// `max` defaults to `chapterCount`: the ordinary source that carries chapters 1..N, where
// "most chapters" and "furthest ahead" agree. The F12 cases below pass it explicitly,
// because they are exactly the works where the two answers differ.
const spine = (chapterCount: number, max = chapterCount): Selectable => ({
	key: 'mangadex:spine',
	chapterCount,
	maxChapterNumber: chapterCount > 0 ? max : undefined,
	redundant: false,
});
const mdExt = (chapterCount: number, redundant: boolean, max = chapterCount): Selectable => ({
	key: 'suwayomi:999',
	chapterCount,
	maxChapterNumber: chapterCount > 0 ? max : undefined,
	redundant,
});
const asura = (chapterCount: number, max = chapterCount): Selectable => ({
	key: 'suwayomi:211',
	chapterCount,
	maxChapterNumber: chapterCount > 0 ? max : undefined,
	redundant: false,
});
const partial = (chapterCount: number, max: number): Selectable => ({
	key: 'suwayomi:777',
	chapterCount,
	maxChapterNumber: max,
	redundant: false,
});

// --- isRedundantMangadexExt -------------------------------------------------

test('the all.mangadex extension is redundant only when a readable spine exists', () => {
	assert.equal(isRedundantMangadexExt(MANGADEX_EXT_PKG, true), true);
	assert.equal(isRedundantMangadexExt(MANGADEX_EXT_PKG, false), false); // empty spine ⇒ it IS the only path
});

test('a non-MangaDex extension is never redundant, even with a readable spine', () => {
	assert.equal(isRedundantMangadexExt('eu.kanade.tachiyomi.extension.en.asurascans', true), false);
	assert.equal(isRedundantMangadexExt(null, true), false);
	assert.equal(isRedundantMangadexExt(undefined, true), false);
});

// --- pickDefaultKey ---------------------------------------------------------

test('prefers the direct spine over a redundant all.mangadex extension that has MORE chapters', () => {
	// The exact leak: extension out-counts the spine, but its content IS the spine.
	const ordered = [spine(120), mdExt(180, true)];
	assert.equal(pickDefaultKey(ordered), 'mangadex:spine');
});

test('a stale persisted preference for the redundant extension heals to the spine', () => {
	const ordered = [spine(120), mdExt(180, true)];
	assert.equal(pickDefaultKey(ordered, 'suwayomi:999'), 'mangadex:spine');
});

test('does NOT override a genuine non-MangaDex source that has more chapters', () => {
	// Solo Leveling → Asura: the spine is thin, Asura is complete and is a distinct source.
	const ordered = [spine(20), asura(200)];
	assert.equal(pickDefaultKey(ordered), 'suwayomi:211');
});

test('honours a valid persisted preference for a non-redundant source', () => {
	const ordered = [spine(120), asura(90)];
	assert.equal(pickDefaultKey(ordered, 'suwayomi:211'), 'suwayomi:211');
});

test('with an empty spine the all.mangadex extension is eligible and can be the default', () => {
	// Takedown: spine carries nothing, so the extension is NOT redundant (its `redundant`
	// flag is false) and remains the readable MangaDex path.
	const ordered = [spine(0), mdExt(150, false)];
	assert.equal(pickDefaultKey(ordered), 'suwayomi:999');
});

test('returns undefined for an empty candidate list', () => {
	assert.equal(pickDefaultKey([]), undefined);
});

test('falls back to the first candidate when nothing is eligible', () => {
	// Defensive: an all-redundant list should still yield a key rather than undefined.
	const ordered: Selectable[] = [{ key: 'suwayomi:999', chapterCount: 10, redundant: true }];
	assert.equal(pickDefaultKey(ordered), 'suwayomi:999');
});

// --- F12: furthest ahead, not most chapters ---------------------------------

test('defaults to the source that reads FURTHEST, not the one with the most chapters', () => {
	// The Immortal Emperor Luo Wuji Has Returned, from production: a 130-chapter source
	// topping out at 130 vs a 46-chapter source that reaches 199. The old rule defaulted
	// the reader 69 chapters behind.
	const ordered = [asura(130, 130), partial(46, 199)];
	assert.equal(pickDefaultKey(ordered), 'suwayomi:777');
});

test('a one-chapter source claiming a huge number cannot win (mis-merge guard)', () => {
	// One Piece (Official Colored), from production: 764 chapters reaching 763, against a
	// single chapter claiming 1183 — almost certainly a bad dedup.
	const ordered = [asura(764, 763), partial(1, 1183)];
	assert.equal(pickDefaultKey(ordered), 'suwayomi:211');
});

test('a source under 10% of the leader cannot win on reach alone', () => {
	// 3 chapters clears MIN_CHAPTERS_TO_LEAD but is 3% of a 100-chapter leader.
	assert.equal(pickDefaultKey([asura(100, 100), partial(3, 500)]), 'suwayomi:211');
	// …and at 10% it is a genuine partial pickup, so it does win.
	assert.equal(pickDefaultKey([asura(100, 100), partial(10, 500)]), 'suwayomi:777');
});

test('a saved preference still beats the furthest-ahead source', () => {
	const ordered = [asura(130, 130), partial(46, 199)];
	assert.equal(pickDefaultKey(ordered, 'suwayomi:211'), 'suwayomi:211');
});

test('falls back to most-chapters when no source has a usable chapter number', () => {
	// A oneshot-only work: nothing is numbered, so reach cannot decide it.
	const ordered: Selectable[] = [
		{ key: 'a', chapterCount: 1, redundant: false },
		{ key: 'b', chapterCount: 4, redundant: false },
	];
	assert.equal(pickDefaultKey(ordered), 'b');
});

test('ties on reach keep the incoming spine-first order', () => {
	// Both sources fully mirror the series — the spine must stay the default so reads keep
	// going to mangadex.network rather than the api.komiq.cc proxy.
	assert.equal(pickDefaultKey([spine(200, 200), asura(200, 200)]), 'mangadex:spine');
});

// --- maxSaneChapterNumber ---------------------------------------------------

test('the chapter-number clamp rejects corruption but keeps real chapters', () => {
	assert.equal(maxSaneChapterNumber([1, 2, 10.5]), 10.5);
	// Both measured in production: a TEST upload and a date used as a chapter number.
	assert.equal(maxSaneChapterNumber([1, 2, 99999999]), 2);
	assert.equal(maxSaneChapterNumber([1, 20240120]), 1);
	// Suwayomi's oneshot sentinel is -1, not a chapter that reads further than any other.
	assert.equal(maxSaneChapterNumber([-1, 3]), 3);
	assert.equal(maxSaneChapterNumber([-1]), undefined);
	// Chapter 0 is real and common in webtoons.
	assert.equal(maxSaneChapterNumber([0]), 0);
	assert.equal(maxSaneChapterNumber([]), undefined);
	assert.equal(maxSaneChapterNumber([NaN, Infinity]), undefined);
});
