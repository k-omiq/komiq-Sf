import assert from 'node:assert/strict';
import { beforeEach, test } from 'node:test';
import type { Backend } from '@komika/api';
import type { ChapterWork } from '@komika/types';
import { testGlobals as g } from './node-test-hooks.ts';

const { clearReaderWorkCache, getReaderChapter } = await import('./source.ts');

const WORK = 'w_original';
const CHAPTER = 'moved-copy';

function fixture(effectiveId = WORK) {
	let lookups = 0;
	const fake = {
		seriesPage: async () => ({
			workId: effectiveId,
			series: { id: effectiveId, title: 'Original Work', sourceId: 'mangadex' },
			spineChapters: [],
			sources: [],
			aggregated: [],
		}),
		canonicalPages: async () => [],
	};
	Object.assign(g.backend, fake);
	return {
		lookups: () => lookups,
		chapterWork(result: ChapterWork | null | undefined) {
			const lookup: NonNullable<Backend['chapterWork']> = async (chapterId) => {
				lookups++;
				assert.equal(chapterId, CHAPTER);
				return result;
			};
			g.backend.chapterWork = lookup;
		},
	};
}

beforeEach(() => {
	clearReaderWorkCache();
	for (const key of Object.keys(g.backend)) delete g.backend[key];
});

test('a chapter missing from work A follows its current owner in work B', async () => {
	const backend = fixture();
	backend.chapterWork({ workId: 'w_destination', entryId: 'entry' });
	const view = await getReaderChapter(WORK, CHAPTER);
	assert.deepEqual(view.movedTo, { workId: 'w_destination', chapterId: CHAPTER });
	assert.equal(view.chapterId, undefined);
	assert.deepEqual(view.pages, []);
	assert.equal(view.seriesTitle, 'Original Work');
	assert.equal(backend.lookups(), 1);
});

for (const owner of [{ workId: WORK, entryId: null }, null, undefined]) {
	test(`a missing chapter keeps the plain empty reader for owner ${owner?.workId}`, async () => {
		const backend = fixture();
		backend.chapterWork(owner);
		const view = await getReaderChapter(WORK, CHAPTER);
		assert.equal(view.movedTo, undefined);
		assert.equal(view.chapterId, undefined);
		assert.deepEqual(view.pages, []);
		assert.equal(view.seriesTitle, 'Original Work');
		assert.equal(backend.lookups(), 1);
	});
}

test('a backend without chapterWork keeps the plain empty reader', async () => {
	fixture();
	const view = await getReaderChapter(WORK, CHAPTER);
	assert.equal(view.movedTo, undefined);
	assert.equal(view.chapterId, undefined);
	assert.equal(view.seriesId, WORK);
	assert.equal(view.seriesTitle, 'Original Work');
	assert.deepEqual(view.pages, []);
});

for (const ownerId of [WORK, 'w_survivor']) {
	test(`a merged route does not follow an owner matching its requested or effective id (${ownerId})`, async () => {
		const backend = fixture('w_survivor');
		backend.chapterWork({ workId: ownerId, entryId: null });
		const view = await getReaderChapter(WORK, CHAPTER);
		assert.equal(view.seriesId, 'w_survivor');
		assert.equal(view.movedTo, undefined);
		assert.equal(backend.lookups(), 1);
	});
}

test('a failed ownership lookup preserves the resolved empty reader', async () => {
	fixture();
	g.backend.chapterWork = async () => {
		throw new Error('lookup failed');
	};
	const view = await getReaderChapter(WORK, CHAPTER);
	assert.equal(view.movedTo, undefined);
	assert.equal(view.seriesTitle, 'Original Work');
});

test('a chapter read from a source outside the work follows its current owner', async () => {
	const backend = fixture();
	g.backend.chapters = async (seriesId: string) => {
		assert.equal(seriesId, '77');
		return [{ id: CHAPTER, name: 'Chapter 42', chapterNumber: 42, read: false, lastPageRead: 0 }];
	};
	backend.chapterWork({ workId: 'w_destination', entryId: 'entry' });
	const view = await getReaderChapter(WORK, CHAPTER, '77');
	assert.deepEqual(view.movedTo, { workId: 'w_destination', chapterId: CHAPTER });
	assert.deepEqual(view.pages, []);
	assert.equal(backend.lookups(), 1);
});

test('the first unread chapter of a source outside the work follows its current owner', async () => {
	const backend = fixture();
	g.backend.chapters = async () => [
		{ id: CHAPTER, name: 'Chapter 42', chapterNumber: 42, read: false, lastPageRead: 0 },
	];
	backend.chapterWork({ workId: 'w_destination', entryId: 'entry' });
	const view = await getReaderChapter(WORK, null, '77');
	assert.deepEqual(view.movedTo, { workId: 'w_destination', chapterId: CHAPTER });
	assert.equal(backend.lookups(), 1);
});
