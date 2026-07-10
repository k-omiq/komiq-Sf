import assert from 'node:assert/strict';
import { test } from 'node:test';
import { isLongStripSeries, stripProgress } from './reader-format.ts';

test('manhwa, manhua and webtoons use continuous reading regardless of translation language', () => {
	for (const type of ['MANHWA', 'MANHUA', 'WEBTOON', 'webtoons']) {
		assert.equal(isLongStripSeries({ type }), true, type);
	}
	for (const language of ['ko', 'kor', 'Korean', 'zh', 'zh-Hans', 'zh_Hant', 'Chinese']) {
		assert.equal(
			isLongStripSeries({ type: 'MANGA', metadata: { originalLanguage: language } }),
			true,
		);
	}
});

test('a manga with explicit long-strip taxonomy uses continuous reading', () => {
	for (const tag of [
		'Long Strip',
		'long-strip',
		'Longstrip',
		'Webtoons',
		'Manhwa',
		'Manhua',
		'Korean',
		'Chinese',
	]) {
		assert.equal(isLongStripSeries({ type: 'MANGA', genres: [tag] }), true, tag);
		assert.equal(
			isLongStripSeries({ type: 'MANGA', metadata: { tags: [{ name: tag }] } }),
			true,
			tag,
		);
	}
	assert.equal(
		isLongStripSeries({
			type: 'MANGA',
			metadata: { tags: [{ id: 'long-strip', name: 'Vertical' }] },
		}),
		true,
	);
});

test('ordinary manga and unknown metadata retain all existing reader choices', () => {
	for (const language of ['ja', 'en', 'fr', undefined]) {
		assert.equal(
			isLongStripSeries({
				type: 'MANGA',
				genres: ['Action'],
				metadata: { originalLanguage: language },
			}),
			false,
		);
	}
	assert.equal(isLongStripSeries({ type: 'MANGA', genres: ['Webtoon adaptations'] }), false);
	assert.equal(isLongStripSeries(null), false);
});

test('scroll progress is continuous inside a tall image and stops at the strip end', () => {
	assert.equal(stripProgress(56, 3000, 1000), 0);
	assert.equal(stripProgress(-500, 3000, 1000), 0.25);
	assert.equal(stripProgress(-501, 3000, 1000), 0.2505);
	assert.equal(stripProgress(-2000, 3000, 1000), 1);
	assert.equal(stripProgress(-2600, 3000, 1000), 1);
	assert.equal(stripProgress(56, 500, 1000), 0);
	assert.equal(stripProgress(0, 500, 1000), 1);
});
