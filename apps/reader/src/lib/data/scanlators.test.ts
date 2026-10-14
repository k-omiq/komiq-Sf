/**
 * Unit tests for the Scanlators page's URL and numbers.
 *
 *   node --test src/lib/data/scanlators.test.ts
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { RatedScanlator } from '@komika/api';
import {
	kindLabel,
	parseScanlatorUrl,
	scanlatorHref,
	scanlatorsHref,
	showFilter,
	siteSourceId,
	withMyScore,
} from './scanlators.ts';

const parse = (qs: string) => parseScanlatorUrl(new URLSearchParams(qs));

function scanlator(over: Partial<RatedScanlator> = {}): RatedScanlator {
	return {
		id: 'group:quiet harbor',
		kind: 'GROUP',
		name: 'Quiet Harbor',
		lang: null,
		seriesCount: 3,
		chapterCount: 40,
		ai: null,
		rating: 8,
		ratingCount: 2,
		rank: null,
		distribution: [0, 0, 0, 0, 0, 0, 0, 2, 0, 0],
		myScore: null,
		commentCount: 0,
		...over,
	};
}

test('an empty address is the default list', () => {
	assert.deepEqual(parse(''), { q: '', show: 'all', sort: 'TOP', page: 1 });
});

test('an unknown value is the default, not an error', () => {
	assert.deepEqual(parse('show=everyone&sort=best&page=-3'), {
		q: '',
		show: 'all',
		sort: 'TOP',
		page: 1,
	});
	assert.equal(parse('page=2.5').page, 1);
	assert.equal(parse('page=abc').page, 1);
});

test('a state and its address round-trip, with the defaults left out', () => {
	assert.equal(scanlatorsHref(parse('')), '/scanlators');
	const href = scanlatorsHref({ q: ' tea ', show: 'group', sort: 'VOTES', page: 3 });
	assert.equal(href, '/scanlators?q=tea&show=group&sort=votes&page=3');
	assert.deepEqual(parse(href.split('?')[1]), { q: 'tea', show: 'group', sort: 'VOTES', page: 3 });
});

test('a link to one scanlator opens its row on the default list', () => {
	assert.equal(scanlatorHref('site:123'), '/scanlators?s=site%3A123');
	assert.equal(scanlatorHref('group:a & b'), '/scanlators?s=group%3Aa+%26+b');
});

test('show maps to the kind or to the viewer filter', () => {
	assert.deepEqual(showFilter('all'), {});
	assert.deepEqual(showFilter('site'), { kind: 'SITE' });
	assert.deepEqual(showFilter('group'), { kind: 'GROUP' });
	assert.deepEqual(showFilter('mine'), { mine: true });
});

test('the kind is said in words', () => {
	assert.equal(kindLabel({ kind: 'GROUP', lang: null }), 'MangaDex group');
	assert.equal(kindLabel({ kind: 'SITE', lang: 'en' }), 'Site · EN');
	assert.equal(kindLabel({ kind: 'SITE', lang: null }), 'Site');
});

test('only a site has a source id', () => {
	assert.equal(siteSourceId({ kind: 'SITE', id: 'site:123' }), '123');
	assert.equal(siteSourceId({ kind: 'GROUP', id: 'group:site:x' }), null);
});

test('a first rating adds a vote', () => {
	const next = withMyScore(scanlator(), 9);
	assert.equal(next.myScore, 9);
	assert.equal(next.ratingCount, 3);
	assert.deepEqual(next.distribution, [0, 0, 0, 0, 0, 0, 0, 2, 1, 0]);
});

test('a changed rating moves the vote and keeps the count', () => {
	const mine = scanlator({ myScore: 8 });
	const next = withMyScore(mine, 10);
	assert.equal(next.ratingCount, 2);
	assert.deepEqual(next.distribution, [0, 0, 0, 0, 0, 0, 0, 1, 0, 1]);
	// The row it came from is not changed.
	assert.deepEqual(mine.distribution, [0, 0, 0, 0, 0, 0, 0, 2, 0, 0]);
});

test('removing a rating takes the vote back', () => {
	const next = withMyScore(scanlator({ myScore: 8 }), null);
	assert.equal(next.myScore, null);
	assert.equal(next.ratingCount, 1);
	assert.deepEqual(next.distribution, [0, 0, 0, 0, 0, 0, 0, 1, 0, 0]);
});
