import assert from 'node:assert/strict';
import { beforeEach, test } from 'node:test';
import { testGlobals as g } from './node-test-hooks.ts';

// Work-entity Stage 1: a canonical series page shows the WORK's own metadata whichever
// source is selected; the selected source only decides which chapters open.
const { clearReaderWorkCache, getReaderChapter, getSeries, workDisplaySeries } =
	await import('./source.ts');

function series(over: Record<string, unknown>) {
	return {
		id: 'w_work',
		title: 'Work Title',
		altTitles: [],
		author: 'Work Author',
		artist: '',
		description: 'Work blurb.',
		genres: [],
		type: 'MANGA',
		status: 'ONGOING',
		coverUrl: '/covers/w_work.webp?v=1',
		sourceId: 'mangadex',
		chapterCount: 0,
		isMarked: true,
		isFavorite: false,
		libraryStatus: null,
		isNsfw: false,
		rating: { average: 0, count: 0, distribution: [] },
		mangadexRating: null,
		updatedAt: '',
		latestChapterAt: '',
		detectedAt: null,
		localizedDescriptions: [],
		credits: [],
		covers: [],
		...over,
	};
}

function chapter(id: string, number: number) {
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
	};
}

const SOURCE = {
	sourceType: 'suwayomi',
	sourceId: '9001',
	sourceKey: '77',
	sourceUrl: null,
	isNsfw: false,
	lang: 'en',
	extension: { pkgName: 'eu.kanade.tachiyomi.extension.en.fixture' },
};
const MANGADEX = { ...SOURCE, sourceType: 'mangadex', sourceId: 'mangadex', sourceKey: 'md-1' };

/** The selected source disagrees with the work on every display field. */
const sourceSeries = series({
	id: '77',
	title: 'Source Title',
	author: 'Source Author',
	description: 'Source blurb.',
	status: 'COMPLETED',
	coverUrl: 'https://source.test/thumb.jpg',
	sourceId: '9001',
	isMarked: false,
	detectedAt: '2026-10-01T00:00:00Z',
});
const sourceChapters = [chapter('c1', 1), chapter('c2', 2), chapter('c3', 3), chapter('c4', 4)];

function bundle(work: unknown, opts: { mangadex?: boolean; sourceMeta?: unknown } = {}) {
	const sources = [
		{
			source: SOURCE,
			chapters: sourceChapters,
			series: 'sourceMeta' in opts ? opts.sourceMeta : sourceSeries,
		},
	];
	if (opts.mangadex) sources.unshift({ source: MANGADEX, chapters: [], series: null });
	return { workId: 'w_work', series: work, spineChapters: [], sources, aggregated: [] };
}

beforeEach(() => {
	clearReaderWorkCache();
	for (const key of Object.keys(g.backend)) delete g.backend[key];
	g.auth.user = { id: 'A', username: 'A', isAdmin: false, showNsfw: false };
});

test('the work, not the selected source, owns the display fields', async () => {
	g.backend.seriesPage = async () => bundle(series({}), { mangadex: true });
	const { view, error } = await getSeries('w_work');
	assert.equal(error, false);
	assert.ok(view);
	// The source with chapters is selected over the empty MangaDex spine…
	assert.deepEqual(
		view.translators.map((t) => [t.name, t.chapterCount]),
		[
			['MangaDex', 0],
			['Fixture', 4],
		],
	);
	assert.equal(view.selectedTranslatorKey, view.translators[1].key);
	// …but the page shows the work.
	const d = view.detail;
	assert.equal(d.id, 'w_work');
	assert.equal(d.title, 'Work Title');
	assert.equal(d.author, 'Work Author');
	assert.equal(d.synopsis, 'Work blurb.');
	assert.equal(d.statusLabel, 'ONGOING'); // the source says COMPLETED
	assert.equal(d.cover, '/covers/w_work.webp?v=1');
	// Viewer state is the work id's, never the source's numeric row.
	assert.equal(d.isMarked, true);
	// Chapters still open from, and are attributed to, the selected source.
	assert.deepEqual(
		view.chapters.map((c) => [c.id, c.sourceId]),
		[
			['c1', '9001'],
			['c2', '9001'],
			['c3', '9001'],
			['c4', '9001'],
		],
	);
});

test('a work with no MangaDex anchor gets no MangaDex option', async () => {
	g.backend.seriesPage = async () => bundle(series({ sourceId: '', coverUrl: '' }));
	const { view } = await getSeries('w_work');
	assert.ok(view);
	assert.deepEqual(
		view.translators.map((t) => t.name),
		['Fixture'],
	);
	assert.equal(view.detail.title, 'Work Title');
	// No cover cached for the work yet: the source's thumbnail stands in.
	assert.equal(view.detail.cover, 'https://source.test/thumb.jpg');
});

test('a failed source series no longer blanks the page', async () => {
	g.backend.seriesPage = async () => bundle(series({ sourceId: '' }), { sourceMeta: null });
	const { view, error } = await getSeries('w_work');
	assert.equal(error, false);
	assert.ok(view, 'the work series alone renders the page');
	assert.equal(view.detail.title, 'Work Title');
	// The rows keep the selected mapping's provenance (its AI badge), not the work's "".
	assert.deepEqual(
		view.chapters.map((c) => c.sourceId),
		['9001', '9001', '9001', '9001'],
	);
});

test('an older server without the work series falls back to the source', async () => {
	g.backend.seriesPage = async () => bundle(null);
	const { view } = await getSeries('w_work');
	assert.ok(view);
	assert.deepEqual(
		view.translators.map((t) => t.name),
		['Fixture'],
	);
	assert.equal(view.detail.title, 'Source Title');
	assert.equal(view.detail.author, 'Source Author');
});

test('the legacy fan-out shows the same work series', async () => {
	g.backend.workSources = async () => [SOURCE];
	g.backend.canonicalChapters = async () => {
		throw new Error('No such work');
	};
	g.backend.canonicalSeries = async () => series({ sourceId: '' });
	g.backend.chapters = async () => sourceChapters;
	g.backend.series = async () => sourceSeries;
	const { view } = await getSeries('w_work');
	assert.ok(view);
	assert.deepEqual(
		view.translators.map((t) => t.name),
		['Fixture'],
	);
	assert.equal(view.detail.title, 'Work Title');
	assert.equal(view.detail.author, 'Work Author');

	// An older server refuses the work series: the source's own metadata remains.
	clearReaderWorkCache();
	g.backend.canonicalSeries = async () => {
		throw new Error('No such work');
	};
	const old = await getSeries('w_work');
	assert.equal(old.view?.detail.title, 'Source Title');
});

test('an empty work field stays empty; only the cover, detection time and source id fall back', () => {
	const work = series({ author: '', artist: '', coverUrl: '', detectedAt: null, sourceId: '' });
	const shown = workDisplaySeries(work as never, sourceSeries as never);
	assert.ok(shown);
	assert.equal(shown.author, '');
	assert.equal(shown.title, 'Work Title');
	assert.equal(shown.coverUrl, 'https://source.test/thumb.jpg');
	assert.equal(shown.detectedAt, '2026-10-01T00:00:00Z');
	assert.equal(shown.sourceId, '9001');
	assert.equal(workDisplaySeries(null, sourceSeries as never), sourceSeries);
	assert.equal(workDisplaySeries(work as never, null)?.sourceId, '');
	assert.equal(workDisplaySeries(work as never, null, '9001')?.sourceId, '9001');
});

test("the reader keeps the work title and still honours the source's long-strip tag", async () => {
	g.backend.seriesPage = async () =>
		bundle(series({ sourceId: '' }), { sourceMeta: { ...sourceSeries, genres: ['Long Strip'] } });
	g.backend.pages = async () => [{ index: 0, sourceUrl: 'https://fixture.test/p1' }];
	const view = await getReaderChapter('w_work', 'c2');
	assert.equal(view.seriesTitle, 'Work Title');
	assert.equal(view.longStrip, true);
});

test('a series id the server does not know is not found, not an outage', async () => {
	g.backend.series = async () => {
		throw new Error('No such series');
	};
	g.backend.chapters = async () => {
		throw new Error('No such series');
	};
	g.backend.search = async () => ({ items: [] });
	assert.deepEqual(await getSeries('kci-no-such-series'), { view: null, error: false });

	// Any other failure still shows the error state.
	g.backend.series = async () => {
		throw new Error('Failed to fetch');
	};
	assert.deepEqual(await getSeries('kci-no-such-series'), { view: null, error: true });
});
