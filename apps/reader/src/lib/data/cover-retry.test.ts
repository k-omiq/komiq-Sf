import assert from 'node:assert/strict';
import { test } from 'node:test';
import { retryCoverSrcSet, retryCoverUrl } from './cover-retry.ts';
const cacheOrigins = ['https://api.example', 'https://img.example'];

test('manual cover recovery preserves version, width and signed proxy source', () => {
	const proxy = 'https://img.example/img?src=https%3A%2F%2Fcdn.example%2Fa.jpg%3Fsig%3Dabc';
	const retried = new URL(retryCoverUrl(proxy, 'new request', cacheOrigins));
	assert.equal(retried.searchParams.get('src'), 'https://cdn.example/a.jpg?sig=abc');
	assert.equal(retried.searchParams.get('retry'), '1');
	assert.equal(retried.searchParams.get('_coverRetry'), 'new request');
	const own = new URL(
		retryCoverUrl('https://api.example/covers/w_1.webp?v=9&w=192', 'next', cacheOrigins),
	);
	assert.equal(own.searchParams.get('v'), '9');
	assert.equal(own.searchParams.get('w'), '192');
});

test('recovery refreshes every srcset candidate and leaves object URLs intact', () => {
	const result = retryCoverSrcSet(
		'https://api.example/a?w=192 1x, https://api.example/a?w=320 2x, https://api.example/a 3x',
		'next',
		cacheOrigins,
	);
	for (const [index, candidate] of result.split(', ').entries()) {
		const [url, density] = candidate.split(' ');
		assert.equal(new URL(url).searchParams.get('_coverRetry'), 'next');
		assert.equal(density, `${index + 1}x`);
	}
	assert.equal(retryCoverUrl('blob:native', 'next', cacheOrigins), 'blob:native');
	assert.equal(retryCoverUrl('', 'next', cacheOrigins), '');
});

test('direct signed CDN URLs are preserved during recovery', () => {
	const url = 'https://cdn.example/image.jpg?sig=abc';
	assert.equal(retryCoverUrl(url, 'next', cacheOrigins), url);
	assert.equal(
		retryCoverUrl('https://api.example.evil.com/image.jpg', 'next', cacheOrigins),
		'https://api.example.evil.com/image.jpg',
	);
});
