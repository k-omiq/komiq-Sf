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
	pickDefaultKey,
	type Selectable,
} from './translator-select.ts';

const spine = (chapterCount: number): Selectable => ({
	key: 'mangadex:spine',
	chapterCount,
	redundant: false,
});
const mdExt = (chapterCount: number, redundant: boolean): Selectable => ({
	key: 'suwayomi:999',
	chapterCount,
	redundant,
});
const asura = (chapterCount: number): Selectable => ({
	key: 'suwayomi:211',
	chapterCount,
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
