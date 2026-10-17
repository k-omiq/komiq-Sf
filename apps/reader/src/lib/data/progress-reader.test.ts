import assert from 'node:assert/strict';
import { beforeEach, test } from 'node:test';
import { get } from 'svelte/store';
import type { Backend, SaveChapterReadInput, SaveProgressInput } from '@komika/api';
import type {
	AggregatedChapter,
	Chapter,
	ProgressResult,
	Series,
	ViewerProgress,
} from '@komika/types';
import { testGlobals as g } from './node-test-hooks.ts';

const {
	clearReaderWorkCache,
	dropWorkProgressState,
	getReaderChapter,
	getSeries,
	saveProgress,
	setChapterRead,
	workProgressGeneration,
} = await import('./source.ts');
const { viewerCache, refreshViewerLoads } = await import('../viewer-cache.svelte.ts');

const WORK = 'w_progress';
const SERIES: Series = {
	id: WORK,
	title: 'Progress Fixture',
	altTitles: [],
	author: null,
	artist: null,
	description: '',
	genres: [],
	type: 'MANGA',
	status: 'ONGOING',
	coverUrl: '',
	sourceId: 'mangadex',
	chapterCount: 0,
	isMarked: false,
	isNsfw: false,
	rating: { average: 0, count: 0, distribution: [] },
	scan: {
		avgIntervalHours: 24,
		overrideIntervalHours: null,
		pollEveryMinutes: 60,
		paused: false,
		statusOverride: null,
		pausedOverride: null,
		pollEveryMinutesOverride: null,
		lastScannedAt: null,
		nextScanAt: null,
	},
	createdAt: '',
	updatedAt: '',
};

function chapter(id: string, number: number, over: Partial<Chapter> = {}): Chapter {
	return {
		id,
		seriesId: WORK,
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

function aggregate(c: Chapter): AggregatedChapter {
	return {
		number: c.number,
		label: c.label,
		title: c.title,
		entryId: c.entryId,
		entryOrder: c.entryOrder,
		special: c.label === 'Extra',
		sources: [
			{
				sourceType: 'mangadex',
				sourceId: 'mangadex',
				suwayomiMangaId: null,
				chapterId: c.id,
				scanlator: c.scanlator,
				copyReleasedAt: null,
				copyExternalUrl: null,
			},
		],
	};
}

function fixture(chapters = [chapter('ch-1', 1)], viewerProgress?: ViewerProgress | null) {
	let bundles = 0;
	const fake: Partial<Backend> = {
		seriesPage: async () => {
			bundles++;
			return {
				workId: WORK,
				series: SERIES,
				spineChapters: chapters.map((c) => ({ ...c })),
				sources: [],
				aggregated: chapters.map(aggregate).reverse(),
				viewerProgress,
			};
		},
		canonicalPages: async () => [],
	};
	Object.assign(g.backend, fake);
	return { bundles: () => bundles };
}

beforeEach(async () => {
	await refreshViewerLoads();
	clearReaderWorkCache();
	for (const key of Object.keys(g.backend)) delete g.backend[key];
	g.auth.user = { id: 'progress', username: 'progress', isAdmin: false };
	g.page.route.id = null;
	g.navigating.to = null;
	g.invalidated.length = 0;
});

test('saveProgress sends the reader work id through the revisioned backend method', async () => {
	const calls: SaveProgressInput[] = [];
	g.backend.saveProgress = async (input: SaveProgressInput) => {
		calls.push(input);
		return { status: 'APPLIED', revision: 8, workId: WORK };
	};
	g.backend.setProgress = async () => assert.fail('revisioned save must own the write');
	g.page.route.id = '/(app)/library';
	await saveProgress('ch-1', 4, true, WORK);
	assert.deepEqual(calls, [{ chapterId: 'ch-1', workId: WORK, lastPageRead: 4, read: true }]);
	assert.deepEqual(g.invalidated, ['komika:library']);
});

test('saveProgress preserves setProgress when the capability or work id is absent', async () => {
	const calls: unknown[][] = [];
	g.backend.setProgress = async (...args: unknown[]) => calls.push(args);
	await saveProgress('ch-1', 2, false, WORK);
	g.backend.saveProgress = async () => assert.fail('the old call shape has no work id');
	await saveProgress('ch-1', 3, true);
	assert.deepEqual(calls, [
		['ch-1', 2, false],
		['ch-1', 3, true],
	]);
});

for (const status of ['APPLIED', 'PAGE_ONLY', 'REFUSED', 'MOVED', null] as const) {
	test(`saveProgress ${status} only caches a completed chapter when accepted`, async () => {
		const { bundles } = fixture();
		g.backend.saveProgress = async (): Promise<ProgressResult | null> =>
			status === null ? null : { status, revision: 8, workId: WORK };
		const before = await getReaderChapter(WORK, 'ch-1');
		assert.equal(before.chapterRead, false);
		g.page.route.id = '/(app)/library';
		await saveProgress('ch-1', 4, true, WORK);
		const after = await getReaderChapter(WORK, 'ch-1');
		assert.equal(after.chapterRead, status === null || status === 'APPLIED');
		assert.equal(bundles(), 1, 'the second open reads the cached bundle');
		assert.deepEqual(
			g.invalidated,
			status === 'REFUSED' || status === 'MOVED' ? [] : ['komika:library'],
		);
	});
}

for (const status of ['APPLIED', 'PAGE_ONLY', 'REFUSED', 'MOVED', null] as const) {
	test(`setChapterRead ${status} reports whether the list toggle landed`, async () => {
		const calls: SaveChapterReadInput[] = [];
		g.backend.saveChapterRead = async (
			input: SaveChapterReadInput,
		): Promise<ProgressResult | null> => {
			calls.push(input);
			return status === null ? null : { status, revision: 8, workId: WORK };
		};
		g.backend.setChapterRead = async () => assert.fail('revisioned toggle must own the write');
		const generation = viewerCache.generation;
		const accepted = status === null || status === 'APPLIED';
		assert.equal(await setChapterRead('ch-1', false, WORK), accepted);
		assert.deepEqual(calls, [{ chapterId: 'ch-1', workId: WORK, read: false }]);
		assert.equal(viewerCache.generation, generation + (accepted ? 1 : 0));
	});
}

test('setChapterRead returns false when the revisioned save throws', async (t) => {
	t.mock.method(console, 'warn', () => {});
	g.backend.saveChapterRead = async () => {
		throw new Error('save failed');
	};
	assert.equal(await setChapterRead('ch-1', true, WORK), false);
});

test('setChapterRead preserves the exact legacy toggle without the capability or work id', async () => {
	const calls: unknown[][] = [];
	g.backend.setChapterRead = async (...args: unknown[]) => {
		calls.push(args);
		return true;
	};
	assert.equal(await setChapterRead('ch-1', true, WORK), true);
	g.backend.saveChapterRead = async () => assert.fail('the old call shape has no work id');
	assert.equal(await setChapterRead('ch-1', false), true);
	assert.deepEqual(calls, [
		['ch-1', true],
		['ch-1', false],
	]);
});

test('dropWorkProgressState bumps only that work and drops cached reader state before refetch', async () => {
	const { bundles } = fixture();
	await getReaderChapter(WORK, 'ch-1');
	await getReaderChapter(WORK, 'ch-1');
	assert.equal(bundles(), 1);
	const before = get(workProgressGeneration);
	const readerGeneration = viewerCache.readerGeneration;
	const viewerGeneration = viewerCache.generation;
	dropWorkProgressState(WORK);
	assert.deepEqual(get(workProgressGeneration), { ...before, [WORK]: (before[WORK] ?? 0) + 1 });
	assert.equal(viewerCache.readerGeneration, readerGeneration + 1);
	assert.equal(viewerCache.generation, viewerGeneration + 1);
	await refreshViewerLoads();
	assert.deepEqual(g.invalidated, ['all']);
	await getReaderChapter(WORK, 'ch-1');
	assert.equal(bundles(), 2);
});

function entryChapters() {
	return [
		chapter('early', 9, { entryId: 'e-early', entryOrder: 1 }),
		chapter('through', 1, { entryId: 'e-through', entryOrder: 2, read: true }),
		chapter('special', 0, { entryId: 'e-special', entryOrder: 3, label: 'Extra' }),
		chapter('late', 2, { entryId: 'e-late', entryOrder: 4 }),
	];
}

async function continueTarget() {
	const { view, error } = await getSeries(WORK);
	assert.equal(error, false);
	assert.ok(view);
	return view;
}

test('continue follows throughEntryId in entry order and includes unread specials', async () => {
	const progress = { revision: 7, throughEntryId: 'e-through' };
	fixture(entryChapters(), progress);
	const view = await continueTarget();
	assert.deepEqual(view.viewerProgress, progress);
	assert.deepEqual(
		view.chapters.map((c) => c.entryId),
		['e-early', 'e-through', 'e-special', 'e-late'],
	);
	assert.equal(view.detail.startChapterId, 'special');
	assert.equal(view.detail.continueCh, 0);
	assert.equal(view.chapters[2].special, true);
});

test('continue skips read entries after throughEntryId to the next unread entry', async () => {
	const chapters = entryChapters();
	chapters[2].read = true;
	fixture(chapters, { revision: 7, throughEntryId: 'e-through' });
	assert.equal((await continueTarget()).detail.startChapterId, 'late');
});

test('continue falls back to the first unread when every entry after the pointer is read', async () => {
	const chapters = entryChapters();
	chapters[2].read = chapters[3].read = true;
	fixture(chapters, { revision: 7, throughEntryId: 'e-through' });
	assert.equal((await continueTarget()).detail.startChapterId, 'early');
});

for (const progress of [
	undefined,
	null,
	{ revision: 7, throughEntryId: null },
	{ revision: 7, throughEntryId: 'gone' },
]) {
	test(`continue keeps first-unread behavior without a matching pointer (${progress?.throughEntryId})`, async () => {
		fixture(entryChapters(), progress);
		assert.equal((await continueTarget()).detail.startChapterId, 'early');
	});
}

test('continue on number-based rows ignores a pointer and keeps the legacy order', async () => {
	fixture([chapter('one', 1), chapter('two', 2, { read: true }), chapter('three', 3)], {
		revision: 7,
		throughEntryId: 'two',
	});
	const view = await continueTarget();
	assert.equal(view.detail.startChapterId, 'one');
	assert.ok(view.chapters.every((c) => c.entryId === undefined));
});

test('continue falls back to the first row when every entry is read', async () => {
	fixture(
		entryChapters().map((c) => ({ ...c, read: true })),
		{
			revision: 7,
			throughEntryId: 'e-through',
		},
	);
	assert.equal((await continueTarget()).detail.startChapterId, 'early');
});
