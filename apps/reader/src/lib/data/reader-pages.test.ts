/**
 * Run: node --test --experimental-strip-types src/lib/data/reader-pages.test.ts
 * (from apps/reader; Node 22 strips the types natively — no test runner needed).
 *
 * The regression these guard: opening a multi-source work (One Piece: a MangaDex spine copy
 * plus a "MANGA Plus" Suwayomi copy) with the Suwayomi engine down rendered an EMPTY reader —
 * "Ch. 0", no title, no chapter list, no pickers. `pages("95")` answered a GraphQL
 * "Internal error", the rejection escaped the page fetch, and `getReaderChapter`'s `live()`
 * wrapper threw away the whole resolved view for `emptyReader`.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { loadFirstReadable, settleWithin, TimeoutError, withTimeout } from './reader-pages.ts';

interface Copy {
	id: string;
	src: string | null;
}
const MANGA_PLUS: Copy = { id: '95', src: '1998944621602463790' };
const MANGADEX: Copy = { id: 'e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0', src: null };

const deadEngine = () => Promise.reject(new Error('Internal error'));

test('a dead secondary source falls back to the MangaDex copy of the same chapter', async () => {
	const errors: string[] = [];
	const got = await loadFirstReadable(
		[MANGA_PLUS, MANGADEX],
		(c) => (c.src ? deadEngine() : Promise.resolve(['p1', 'p2'])),
		(c) => errors.push(c.id),
	);
	assert.equal(got.copy, MANGADEX);
	assert.deepEqual(got.pages, ['p1', 'p2']);
	assert.equal(got.failed, false);
	assert.deepEqual(errors, ['95']);
});

test('when every copy fails it RESOLVES (never rejects) with the first copy and no pages', async () => {
	// The bug was precisely a rejection here: it is what turned one dead source into a blank
	// reader. The caller must still get a copy to build the full view around.
	const got = await loadFirstReadable([MANGADEX, MANGA_PLUS], deadEngine);
	assert.equal(got.copy, MANGADEX);
	assert.deepEqual(got.pages, []);
	assert.equal(got.failed, true);
});

test('the first readable copy wins and later copies are never fetched', async () => {
	const asked: string[] = [];
	const got = await loadFirstReadable([MANGADEX, MANGA_PLUS], async (c) => {
		asked.push(c.id);
		return ['p'];
	});
	assert.equal(got.copy, MANGADEX);
	assert.deepEqual(asked, [MANGADEX.id]);
});

test('an empty copy falls through, but an all-empty chapter is not reported as a failure', async () => {
	const fallback = await loadFirstReadable([MANGA_PLUS, MANGADEX], async (c) =>
		c.src ? [] : ['p'],
	);
	assert.equal(fallback.copy, MANGADEX);

	const allEmpty = await loadFirstReadable([MANGA_PLUS, MANGADEX], async () => []);
	assert.equal(allEmpty.copy, MANGA_PLUS);
	assert.equal(allEmpty.failed, false);
});

// ---------------------------------------------------------------------------------------
// `getReaderChapter` end to end, against a fake backend. These drive the REAL reader data
// path (resolveWork → copy pool → loadFirstReadable → buildReaderView) through
// `node-test-hooks.ts`, which stubs the SvelteKit-only imports, so they guard the wiring in
// `source.ts` that the pure tests above cannot see.
//
// The fixture work: a MangaDex spine whose list keeps ONE upload per number (Group A's),
// a second MangaDex upload of ch. 5 by Group B that ONLY the aggregation carries, and a
// Suwayomi source ("MANGA Plus", manga 777) with its own copies of ch. 4 and 5.
// ---------------------------------------------------------------------------------------
import { testGlobals } from './node-test-hooks.ts';
import type { ReaderFormatMetadata } from './reader-format.ts';

const { getReaderChapter, clearReaderWorkCache, saveProgress } = await import('./source.ts');

const WORK = 'w_fixture';
const SUW = '777';

function chapter(id: string, number: number, scanlator: string | null) {
	return {
		id,
		seriesId: WORK,
		number,
		label: String(number),
		title: null,
		externalUrl: null,
		pageCount: 0,
		uploadedAt: null,
		foundAt: null,
		scanlator,
		read: false,
		lastPageRead: 0,
		bookmarked: false,
		isDownloaded: false,
	};
}
function copy(chapterId: string, suwayomiMangaId: string | null, scanlator: string) {
	return {
		sourceType: suwayomiMangaId ? 'suwayomi' : 'mangadex',
		sourceId: suwayomiMangaId ? '1998944621602463790' : 'mangadex',
		suwayomiMangaId,
		chapterId,
		scanlator,
		copyReleasedAt: null,
		copyExternalUrl: null,
	};
}

const SPINE = [chapter('md-4', 4, 'Group A'), chapter('md-5a', 5, 'Group A')];
const SUW_CHAPTERS = [chapter('9004', 4, 'MANGA Plus'), chapter('9005', 5, 'MANGA Plus')];
const AGGREGATED = [
	{
		number: 4,
		label: '4',
		title: null,
		sources: [copy('md-4', null, 'Group A'), copy('9004', SUW, 'MANGA Plus')],
	},
	{
		number: 5,
		label: '5',
		title: null,
		sources: [
			copy('md-5a', null, 'Group A'),
			// Not in the spine's list: reachable only through the aggregation.
			copy('md-5b', null, 'Group B'),
			copy('9005', SUW, 'MANGA Plus'),
		],
	},
];

interface FakeOpts {
	publication?: ReaderFormatMetadata;
	/** Pages per MangaDex chapter id; an id not listed REJECTS (taken down upstream). */
	mdPages?: Record<string, string[]>;
	/** The Suwayomi source's chapter list as the server reports it (`[]` = engine down). */
	suwChapters?: ReturnType<typeof chapter>[];
	/** The aggregation as the bundle carries it. */
	aggregated?: unknown[];
	/** The Suwayomi mapping's series URL (`WorkSource.sourceUrl`). */
	suwSourceUrl?: string | null;
}

const DEAD_ENGINE = 'Internal error';

/** Installs a fake backend and records every page request it answers. */
function fakeBackend(opts: FakeOpts = {}) {
	const asked: string[] = [];
	const md = opts.mdPages ?? {};
	const toPages = (urls: string[]) => urls.map((sourceUrl, index) => ({ index, sourceUrl }));
	const b = testGlobals.backend;
	for (const k of Object.keys(b)) delete b[k];
	// Every case shares one work id; the reader memoizes its resolution per work.
	clearReaderWorkCache();
	Object.assign(b, {
		seriesPage: async (workId: string) => ({
			workId,
			series: { id: WORK, title: 'Fixture Work', ...opts.publication },
			spineChapters: SPINE,
			sources: [
				{
					source: {
						sourceType: 'suwayomi',
						sourceId: '1998944621602463790',
						sourceKey: SUW,
						sourceUrl: opts.suwSourceUrl ?? null,
						isNsfw: false,
						lang: 'en',
						extension: { pkgName: 'eu.kanade.tachiyomi.extension.all.mangaplus' },
					},
					chapters: opts.suwChapters ?? SUW_CHAPTERS,
					series: null,
				},
			],
			aggregated: opts.aggregated ?? AGGREGATED,
		}),
		canonicalPages: async (id: string) => {
			asked.push(id);
			if (!md[id]) throw new Error(`MangaDex 404 for ${id}`);
			return toPages(md[id]);
		},
		// The Suwayomi engine is down for every test here: Suwayomi is not reachable in this
		// environment, and a dead engine is a real production failure mode.
		pages: async (id: string) => {
			asked.push(id);
			throw new Error(DEAD_ENGINE);
		},
		chapters: async () => {
			throw new Error(DEAD_ENGINE);
		},
		series: async () => {
			throw new Error(DEAD_ENGINE);
		},
	});
	return { asked };
}

test('reader classification carries canonical long-strip tags even with an English source', async () => {
	fakeBackend({
		mdPages: { 'md-5a': ['image'] },
		publication: { type: 'MANGA', metadata: { tags: [{ name: 'Long Strip' }] } },
	});
	const got = await getReaderChapter(WORK, 'md-5a');
	assert.equal(got.longStrip, true);
	assert.equal(got.pages.length, 1);
});

test('reader classification preserves ordinary manga and respects original Korean language', async () => {
	for (const [language, expected] of [
		['ja', false],
		['ko', true],
	] as const) {
		fakeBackend({
			mdPages: { 'md-5a': ['image'] },
			publication: { type: 'MANGA', metadata: { originalLanguage: language } },
		});
		const got = await getReaderChapter(WORK, 'md-5a');
		assert.equal(got.longStrip, expected);
	}
});

test('single-source readers also carry publication layout metadata', async () => {
	fakeBackend();
	testGlobals.backend.series = async () => ({ type: 'MANHUA', title: 'Single-source comic' });
	testGlobals.backend.chapters = async () => [chapter('9005', 5, null)];
	testGlobals.backend.pages = async () => [{ index: 0, sourceUrl: 'image' }];
	const got = await getReaderChapter('777', '9005');
	assert.equal(got.longStrip, true);
	assert.equal(got.seriesTitle, 'Single-source comic');
});

// The reader logs every failed copy on purpose; keep the test output readable.
console.warn = () => {};

test('(a) the open copy fails and a MangaDex upload that ONLY the aggregation carries loads', async () => {
	const { asked } = fakeBackend({ mdPages: { 'md-5b': ['b1.webp', 'b2.webp'] } });
	const view = await getReaderChapter(WORK, 'md-5a', null);
	assert.equal(view.chapterId, 'md-5b');
	assert.deepEqual(
		view.pages.map((p) => p.url),
		['b1.webp', 'b2.webp'],
	);
	assert.equal(view.group, 'Group B');
	assert.equal(view.readingSrc, null);
	// The switcher marks the copy actually being read, and still offers all three.
	assert.deepEqual(
		view.versions?.map((v) => [v.id, v.current]),
		[
			['md-5a', false],
			['md-5b', true],
			['9005', false],
		],
	);
	// The open copy was tried first; the aggregation-only sibling rescued it.
	assert.equal(asked[0], 'md-5a');
	assert.ok(asked.includes('md-5b'));
});

test('(b) when every copy fails the view keeps its title, chapter list and versions', async () => {
	const { asked } = fakeBackend({ mdPages: {} });
	const view = await getReaderChapter(WORK, 'md-5a', null);
	// Not `emptyReader`: the chapter that was asked for opens, with its chrome, and no pages.
	assert.equal(view.seriesTitle, 'Fixture Work');
	assert.equal(view.chapterId, 'md-5a');
	assert.equal(view.chNum, 5);
	assert.deepEqual(view.pages, []);
	assert.deepEqual(
		view.chapters.map((c) => c.id),
		['md-5a', 'md-4'],
	);
	assert.equal(view.prevChapterId, 'md-4');
	assert.deepEqual(
		view.versions?.map((v) => v.id),
		['md-5a', 'md-5b', '9005'],
	);
	assert.ok(view.translators.length >= 2, 'the source picker survives');
	// Every copy of ch. 5 was tried exactly once before giving up.
	assert.deepEqual([...new Set(asked)].sort(), ['9005', 'md-5a', 'md-5b']);
});

test('(c) Suwayomi is down: its copy falls back to the MangaDex copy of the same chapter', async () => {
	fakeBackend({ mdPages: { 'md-5a': ['a1.webp'] } });
	const view = await getReaderChapter(WORK, '9005', SUW);
	assert.equal(view.chapterId, 'md-5a');
	assert.deepEqual(
		view.pages.map((p) => p.url),
		['a1.webp'],
	);
	// Reading from the spine now — the list, prev/next and the picker describe IT.
	assert.equal(view.readingSrc, null);
	assert.equal(view.versions?.find((v) => v.current)?.id, 'md-5a');
});

test('(c) Suwayomi is down and its list came back EMPTY: a `?ch=` naming its copy still opens', async () => {
	// The server answers a dead source's chapter list with `[]`, so the Suwayomi copy the URL
	// names is in no list at all — only in the aggregation. This used to render emptyReader.
	fakeBackend({ mdPages: { 'md-5a': ['a1.webp'] }, suwChapters: [] });
	const view = await getReaderChapter(WORK, '9005', SUW);
	assert.equal(view.seriesTitle, 'Fixture Work');
	assert.equal(view.chapterId, 'md-5a');
	assert.equal(view.chNum, 5);
	assert.deepEqual(
		view.pages.map((p) => p.url),
		['a1.webp'],
	);
	assert.equal(view.readingSrc, null);
});

test('a `?ch=` unknown to every list AND the aggregation stays an honest empty reader', async () => {
	fakeBackend({ mdPages: { 'md-5a': ['a1.webp'] } });
	const view = await getReaderChapter(WORK, 'not-a-chapter', null);
	assert.equal(view.chapterId, undefined);
	assert.deepEqual(view.pages, []);
	assert.equal(view.seriesTitle, 'Fixture Work');
});

test('a failed aggregation (degraded to []) still opens the chapter from the list alone', async () => {
	// `GraphQLBackend.seriesPage` turns a failing `aggregated` into `[]` rather than failing
	// the bundle; the reader must then behave exactly like its single-copy self.
	fakeBackend({ mdPages: { 'md-5a': ['a1.webp'] }, aggregated: [] });
	const view = await getReaderChapter(WORK, 'md-5a', null);
	assert.equal(view.seriesTitle, 'Fixture Work');
	assert.equal(view.chapterId, 'md-5a');
	assert.deepEqual(
		view.pages.map((p) => p.url),
		['a1.webp'],
	);
	assert.deepEqual(
		view.chapters.map((c) => c.id),
		['md-5a', 'md-4'],
	);
	// Versions fall back to the open source's own copies of the number.
	assert.deepEqual(
		view.versions?.map((v) => v.id),
		['md-5a'],
	);
});

test('"Read on source": the view and each version carry their copy\'s original-site URL', async () => {
	const uuid = 'e9fb431b-5a4d-4bf8-9c4a-0510c5eca6d0';
	const spine = [chapter(uuid, 5, 'Group A')];
	fakeBackend({ mdPages: { [uuid]: ['a1.webp'] }, aggregated: [] });
	// Re-point the fixture's spine at a real MangaDex-shaped id for this case.
	const seriesPage = testGlobals.backend.seriesPage as (id: string) => Promise<object>;
	testGlobals.backend.seriesPage = async (id: string) => ({
		...(await seriesPage(id)),
		spineChapters: spine,
	});
	const view = await getReaderChapter(WORK, uuid, null);
	assert.equal(view.sourceUrl, `https://mangadex.org/chapter/${uuid}`);
	assert.equal(view.versions?.[0].sourceUrl, `https://mangadex.org/chapter/${uuid}`);

	// A Suwayomi copy (engine reachable here only in the sense that the list loaded): no URL.
	fakeBackend({ mdPages: {}, aggregated: [] });
	const suw = await getReaderChapter(WORK, '9005', SUW);
	assert.equal(suw.readingSrc, SUW);
	assert.equal(suw.sourceUrl, null);
});

test('"Read on source" falls back to the reading source\'s SERIES page when the chapter URL is unknown', async () => {
	fakeBackend({
		mdPages: {},
		aggregated: [],
		suwSourceUrl: 'https://mangaplus.shueisha.co.jp/titles/100020',
	});
	const suw = await getReaderChapter(WORK, '9005', SUW);
	assert.equal(suw.readingSrc, SUW);
	assert.equal(suw.sourceUrl, null);
	assert.equal(suw.seriesSourceUrl, 'https://mangaplus.shueisha.co.jp/titles/100020');

	// Upstream text bound for an href: anything but absolute http(s) is dropped.
	fakeBackend({ mdPages: {}, aggregated: [], suwSourceUrl: 'javascript:alert(1)' });
	const hostile = await getReaderChapter(WORK, '9005', SUW);
	assert.equal(hostile.seriesSourceUrl, null);
});

// ---------------------------------------------------------------------------------------
// The production "page goes grey, chapter never changes" bug and its guards.
// ---------------------------------------------------------------------------------------

/** A promise that never settles — a request the server accepted and never answered. */
const hang = <T>() => new Promise<T>(() => {});

test('withTimeout rejects a stalled promise with a TimeoutError, and passes a fast one through', async () => {
	await assert.rejects(withTimeout(hang(), 10, 'page list'), TimeoutError);
	assert.equal(await withTimeout(Promise.resolve(7), 1000), 7);
	await assert.rejects(withTimeout(Promise.reject(new Error('boom')), 1000), /boom/);
});

test('settleWithin answers the fallback for a stalled or failed promise', async () => {
	assert.equal(await settleWithin(hang<string>(), 10, 'fallback'), 'fallback');
	assert.equal(await settleWithin(Promise.reject(new Error('x')), 1000, 'fallback'), 'fallback');
	assert.equal(await settleWithin(Promise.resolve('real'), 1000, 'fallback'), 'real');
});

test('a copy that never answers falls through to the next one instead of hanging the load', async () => {
	const errors: unknown[] = [];
	const got = await loadFirstReadable(
		[MANGA_PLUS, MANGADEX],
		(c) => (c.src ? hang<string[]>() : Promise.resolve(['p1'])),
		(_c, err) => errors.push(err),
		{ timeoutMs: 20 },
	);
	assert.equal(got.copy, MANGADEX);
	assert.deepEqual(got.pages, ['p1']);
	assert.ok(errors[0] instanceof TimeoutError);

	// Every copy stalled: the load still RESOLVES (no pages, flagged failed) — it never hangs.
	const allHung = await loadFirstReadable(
		[MANGADEX, MANGA_PLUS],
		() => hang<string[]>(),
		undefined,
		{
			timeoutMs: 10,
		},
	);
	assert.deepEqual(allHung.pages, []);
	assert.equal(allHung.failed, true);
});

test('saving progress never calls invalidate() while a navigation is in flight', async () => {
	// SvelteKit's `invalidate()` steals the router's navigation token, which ABORTS a
	// navigation still waiting on its load and leaves `navigating` set forever: the reader
	// dimmed the old chapter and the next one never appeared. `openChapter` saves progress
	// right before `goto`, so the save always resolves mid-navigation.
	fakeBackend();
	testGlobals.backend.setProgress = async () => {};
	testGlobals.invalidated.length = 0;
	testGlobals.page.route.id = '/read/[slug]';
	testGlobals.navigating.to = { url: new URL('https://komiq.cc/read/w_fixture?ch=md-5a') };
	await saveProgress('md-4', 3, true);
	assert.deepEqual(testGlobals.invalidated, []);

	// Idle, but in the reader: nothing depends on the key here, so nothing to refresh.
	testGlobals.navigating.to = null;
	await saveProgress('md-4', 3, true);
	assert.deepEqual(testGlobals.invalidated, []);

	// The library is on screen and idle — the one case the refresh exists for.
	testGlobals.page.route.id = '/(app)/library';
	await saveProgress('md-4', 3, true);
	assert.deepEqual(testGlobals.invalidated, ['komika:library']);

	testGlobals.page.route.id = null;
	testGlobals.invalidated.length = 0;
});

test('chapter-to-chapter loads share ONE series bundle; a bare open always re-resolves', async () => {
	fakeBackend({ mdPages: { 'md-4': ['a.webp'], 'md-5a': ['b.webp'] } });
	const seriesPage = testGlobals.backend.seriesPage as (id: string) => Promise<object>;
	let bundles = 0;
	testGlobals.backend.seriesPage = (id: string) => {
		bundles++;
		return seriesPage(id);
	};
	// The next-chapter prefetch and the click race each other: one request between them.
	const [a, b] = await Promise.all([
		getReaderChapter(WORK, 'md-5a', null),
		getReaderChapter(WORK, 'md-4', null),
	]);
	assert.equal(bundles, 1);
	assert.equal(a.chapterId, 'md-5a');
	assert.equal(b.chapterId, 'md-4');
	// A later chapter turn reuses it too — only the pages are fetched.
	const c = await getReaderChapter(WORK, 'md-5a', null);
	assert.equal(bundles, 1);
	assert.deepEqual(
		c.pages.map((p) => p.url),
		['b.webp'],
	);
	// Opening the work bare picks "first unread", which needs fresh read state.
	await getReaderChapter(WORK, null, null);
	assert.equal(bundles, 2);
});

test('a chapter marked read is still read when reopened from the shared bundle', async () => {
	fakeBackend({ mdPages: { 'md-4': ['a.webp'] } });
	testGlobals.backend.setProgress = async () => {};
	const before = await getReaderChapter(WORK, 'md-4', null);
	assert.equal(before.chapterRead, false);
	await saveProgress('md-4', 0, true);
	const after = await getReaderChapter(WORK, 'md-4', null);
	assert.equal(after.chapterRead, true);
});

test('a failed series bundle is not memoized: the next load asks again', async () => {
	fakeBackend({ mdPages: { 'md-4': ['a.webp'] } });
	const seriesPage = testGlobals.backend.seriesPage as (id: string) => Promise<object>;
	let calls = 0;
	testGlobals.backend.seriesPage = (id: string) => {
		calls++;
		return calls === 1 ? Promise.reject(new Error('Backend error 502')) : seriesPage(id);
	};
	const failed = await getReaderChapter(WORK, 'md-4', null);
	assert.equal(failed.chapterId, undefined);
	const ok = await getReaderChapter(WORK, 'md-4', null);
	assert.equal(ok.chapterId, 'md-4');
	assert.equal(calls, 2);
});
