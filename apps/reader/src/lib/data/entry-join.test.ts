import assert from 'node:assert/strict';
import { beforeEach, test } from 'node:test';
import type { AggregatedChapter, Chapter } from '@komika/types';
import { testGlobals as g } from './node-test-hooks.ts';

const { clearReaderWorkCache, getReaderChapter, getSeries } = await import('./source.ts');

const WORK = 'w_entries';
const SOURCE = {
	sourceType: 'suwayomi',
	sourceId: '9001',
	sourceKey: '77',
	sourceUrl: null,
	isNsfw: false,
	lang: 'en',
	extension: { pkgName: 'eu.kanade.tachiyomi.extension.en.fixture' },
};
const SERIES = {
	id: WORK,
	title: 'Entry Fixture',
	altTitles: [],
	author: '',
	artist: '',
	description: '',
	genres: [],
	type: 'MANGA',
	status: 'ONGOING',
	coverUrl: '',
	sourceId: '9001',
	chapterCount: 0,
	isMarked: false,
	isNsfw: false,
	rating: { average: 0, count: 0, distribution: [] },
	updatedAt: '',
};

function chapter(id: string, number: number, over: Partial<Chapter> = {}): Chapter {
	return {
		id,
		seriesId: '77',
		number,
		label: String(number),
		title: null,
		externalUrl: null,
		pageCount: 1,
		uploadedAt: null,
		foundAt: null,
		scanlator: 'Group',
		read: false,
		lastPageRead: 0,
		bookmarked: false,
		isDownloaded: false,
		...over,
	};
}

function aggregate(c: Chapter, over: Partial<AggregatedChapter> = {}): AggregatedChapter {
	return {
		number: c.number,
		label: c.label,
		title: c.title,
		entryId: c.entryId,
		entryOrder: c.entryOrder,
		sources: [
			{
				sourceType: 'suwayomi',
				sourceId: '9001',
				suwayomiMangaId: '77',
				chapterId: c.id,
				scanlator: c.scanlator,
				copyReleasedAt: c.uploadedAt,
				copyExternalUrl: c.externalUrl,
			},
		],
		...over,
	};
}

function fixture(chapters: Chapter[], aggregated = chapters.map((c) => aggregate(c))) {
	g.backend.seriesPage = async () => ({
		workId: WORK,
		series: SERIES,
		spineChapters: [],
		sources: [{ source: SOURCE, chapters, series: { ...SERIES, id: '77' } }],
		aggregated,
	});
	g.backend.pages = async () => [{ index: 0, sourceUrl: '/page.webp' }];
}

async function seriesView() {
	const { view, error } = await getSeries(WORK);
	assert.equal(error, false);
	assert.ok(view);
	return view;
}

beforeEach(() => {
	clearReaderWorkCache();
	for (const key of Object.keys(g.backend)) delete g.backend[key];
	g.auth.user = { id: 'entries', username: 'entries', isAdmin: false, showNsfw: false };
});

test('equal numbers in different entries join their own selected-source copies', async () => {
	const a = chapter('a', 4, { entryId: 'a', entryOrder: 1, read: true, scanlator: 'A' });
	const b = chapter('b', 4, {
		entryId: 'b',
		entryOrder: 2,
		scanlator: 'B',
		externalUrl: 'https://example.test/b',
	});
	fixture([a, b]);
	const view = await seriesView();
	assert.deepEqual(
		view.chapters.map((c) => [c.id, c.n, c.read, c.group, c.external]),
		[
			['a', 4, true, 'A', false],
			['b', 4, false, 'B', true],
		],
	);
	assert.deepEqual(
		view.chapters.map((c) => c.sources?.map((s) => s.id)),
		[['a'], ['b']],
	);
	assert.equal(view.detail.startChapterId, 'b');
});

test('chapter zero and an unnumbered special remain distinct entries', async () => {
	const zero = chapter('zero', 0, { entryId: 'zero', entryOrder: 1, read: true });
	const extra = chapter('extra', 0, { entryId: 'extra', entryOrder: 2, label: 'Extra' });
	fixture(
		[zero, extra],
		[aggregate(zero, { special: false }), aggregate(extra, { special: true })],
	);
	const view = await seriesView();
	assert.deepEqual(
		view.chapters.map((c) => [c.id, c.read, c.special]),
		[
			['zero', true, false],
			['extra', false, true],
		],
	);
});

test('entry order controls rows, continue target, new markers and found-date eligibility', async () => {
	const chapters = [90, 1, 2, 0, 30].map((number, i) =>
		chapter(`c${number}`, number, {
			entryId: `e${number}`,
			entryOrder: i + 1,
			read: i === 0,
			foundAt: '2001-01-01T00:00:00Z',
		}),
	);
	fixture(
		chapters,
		[...chapters].reverse().map((c) => aggregate(c)),
	);
	const view = await seriesView();
	assert.deepEqual(
		view.chapters.map((c) => c.n),
		[90, 1, 2, 0, 30],
	);
	assert.deepEqual(
		view.chapters.map((c) => c.entryOrder),
		[1, 2, 3, 4, 5],
	);
	assert.deepEqual(
		view.chapters.map((c) => c.isNew),
		[false, false, true, true, true],
	);
	assert.deepEqual(
		view.chapters.map((c) => !!c.foundDate),
		[false, false, true, true, true],
	);
	assert.equal(view.detail.continueCh, 1);
	assert.equal(view.detail.startChapterId, 'c1');
});

test('entry joins never borrow an unlinked chapter with the same number', async () => {
	const unlinked = chapter('unlinked', 7, { read: true });
	const other = chapter('other', 7, { entryId: 'linked', entryOrder: 1 });
	fixture([unlinked], [aggregate(other)]);
	const view = await seriesView();
	assert.equal(view.chapters[0].id, 'other');
	assert.equal(view.chapters[0].read, false);
});

test('equal entry ranks fall back to number order and keep equal numbers stable', async () => {
	const chapters = [chapter('b', 3), chapter('a', 2), chapter('a-copy', 2)].map((c) => ({
		...c,
		entryId: c.id,
		entryOrder: 1,
	}));
	fixture(chapters);
	assert.deepEqual(
		(await seriesView()).chapters.map((c) => c.id),
		['a', 'a-copy', 'b'],
	);
});

test('stripped and null entry fields retain the complete legacy row snapshot', async () => {
	const linked = [
		chapter('a', 4, { entryId: 'a', entryOrder: 1, read: true }),
		chapter('zero', 0, { entryId: 'zero', entryOrder: 2, read: true }),
		chapter('b', 4, { entryId: 'b', entryOrder: 3 }),
		chapter('extra', 0, { entryId: 'extra', entryOrder: 4, label: 'Extra' }),
	];
	const chapters = linked.map(({ entryId, entryOrder, ...c }) => c);
	const aggregated = linked.map((c) => {
		const { entryId, entryOrder, special, ...row } = aggregate(c, { special: true });
		return row;
	});
	fixture(chapters, aggregated);
	const old = (await seriesView()).chapters;
	const expected = ['zero', 'extra', 'a', 'b'].map((copyId, i) => {
		const n = i < 2 ? 0 : 4;
		return {
			id: i < 2 ? 'extra' : 'b',
			n,
			entryOrder: undefined,
			title: `Chapter ${n}`,
			date: '',
			foundDate: '',
			isNew: i >= 1,
			special: false,
			read: false,
			src: '77',
			sources: [
				{
					id: copyId,
					src: '77',
					name: 'Fixture',
					iconUrl: '/ext-icons/eu.kanade.tachiyomi.extension.en.fixture.png',
					group: 'Group',
					sourceId: '9001',
					date: '',
				},
			],
			external: false,
			group: 'Group',
			sourceId: '9001',
			volume: null,
		};
	});
	assert.deepEqual(old, expected);
	fixture(
		linked,
		aggregated.map((c) => ({ ...c, entryOrder: 99 - c.number })),
	);
	assert.deepEqual((await seriesView()).chapters, old);
	fixture(
		chapters.map((c) => ({ ...c, entryId: null, entryOrder: null })),
		aggregated.map((c) => ({ ...c, entryId: null, entryOrder: null, special: null })),
	);
	assert.deepEqual(
		(await seriesView()).chapters.map(({ entryOrder, ...c }) => c),
		old.map(({ entryOrder, ...c }) => c),
	);
});

test('reader navigation carries entry slots, order and special flags for aggregate-only copies', async () => {
	const start = chapter('start', 9, { entryId: 'start', entryOrder: 1 });
	const end = chapter('end', 2, { entryId: 'end', entryOrder: 3 });
	const extra = chapter('extra', 0, { entryId: 'extra', entryOrder: 2, label: 'Extra' });
	const row = aggregate(start);
	row.sources.push({ ...row.sources[0], chapterId: 'alternate' });
	fixture([start, end], [aggregate(end), aggregate(extra, { special: true }), row]);
	const view = await getReaderChapter(WORK, 'start');
	assert.equal(view.nextChapterId, 'extra');
	assert.equal(view.prevChapterId, undefined);
	assert.deepEqual(
		view.chapters.map((c) => [c.id, c.special]),
		[
			['end', false],
			['extra', true],
			['start', false],
		],
	);
	assert.deepEqual(
		view.versions?.map((c) => c.id),
		['start', 'alternate'],
	);
});
