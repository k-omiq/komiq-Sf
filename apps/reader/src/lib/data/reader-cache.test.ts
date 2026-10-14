import assert from 'node:assert/strict';
import { beforeEach, test } from 'node:test';
import { testGlobals as g } from './node-test-hooks.ts';
const { viewerCacheChanged, refreshViewerLoads } = await import('../viewer-cache.svelte.ts');

const {
	clearReaderWorkCache,
	getReaderChapter,
	readerViewForCurrentScope,
	readerViewIsCurrent,
	releaseStaleReaderView,
	saveProgress,
	setChapterRead,
} = await import('./source.ts');

const CHAPTER = 'md-fixture';
let calls: string[];
let read: boolean;

function bundle(workId: string) {
	return {
		workId,
		series: { id: workId, title: g.auth.user?.id ?? 'anonymous' },
		spineChapters: [
			{
				id: CHAPTER,
				seriesId: workId,
				number: 1,
				label: '1',
				title: null,
				externalUrl: null,
				pageCount: 1,
				uploadedAt: null,
				foundAt: null,
				scanlator: 'Fixture',
				read,
				lastPageRead: 0,
				bookmarked: false,
				isDownloaded: false,
			},
		],
		sources: [],
		aggregated: [],
	};
}

function deferred<T>() {
	let resolve!: (value: T) => void;
	const promise = new Promise<T>((r) => (resolve = r));
	return { promise, resolve };
}

beforeEach(async () => {
	await refreshViewerLoads();
	clearReaderWorkCache();
	for (const key of Object.keys(g.backend)) delete g.backend[key];
	g.auth.user = { id: 'A', username: 'A', isAdmin: false, showNsfw: false };
	g.config.apiEndpoint = 'https://fixture.test/graphql';
	g.config.backendKind = 'komika';
	g.config.nativeEngine = false;
	g.navigating.to = null;
	g.navigating.complete = undefined;
	calls = [];
	read = true;
	g.backend.seriesPage = async (workId: string) => {
		calls.push(workId);
		return bundle(workId);
	};
	g.backend.canonicalPages = async () => [{ index: 0, sourceUrl: 'https://fixture.test/page' }];
});

test('settled bundles and completed preloads cannot expose another viewer', async () => {
	const a = await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(a.chapterRead, true);
	g.auth.user = { id: 'B', username: 'B', isAdmin: false };
	read = false;
	viewerCacheChanged();
	assert.equal(readerViewIsCurrent(a), false);
	const hidden = readerViewForCurrentScope(a);
	assert.equal(hidden.seriesTitle, '');
	assert.deepEqual(hidden.pages, []);
	assert.deepEqual(hidden.chapters, []);
	assert.equal(hidden.chapterRead, undefined);
	const b = await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(b.seriesTitle, 'B');
	assert.equal(b.chapterRead, false);
	assert.equal(readerViewIsCurrent(b), true);
	assert.equal(calls.length, 2);
});

test('a hidden stale view releases its pages, a current one keeps them', async () => {
	const released: string[] = [];
	(g.images as { release?: (url: string) => void }).release = (url) => released.push(url);
	try {
		const a = await getReaderChapter('w_fixture', CHAPTER);
		assert.equal(a.pages.length, 1);
		releaseStaleReaderView(a);
		assert.deepEqual(released, [], 'the view on screen is still current');
		g.auth.user = { id: 'B', username: 'B', isAdmin: false };
		viewerCacheChanged();
		releaseStaleReaderView(a);
		assert.deepEqual(
			released,
			a.pages.map((p) => p.url),
		);
	} finally {
		delete (g.images as { release?: unknown }).release;
	}
});

test('a late old bundle is discarded and callers reload under the new generation', async () => {
	const old = bundle('w_fixture');
	const delayed = deferred<typeof old>();
	g.backend.seriesPage = async (workId: string) => {
		calls.push(workId);
		return calls.length === 1 ? delayed.promise : bundle(workId);
	};
	const pendingA = getReaderChapter('w_fixture', CHAPTER);
	g.auth.user = { id: 'B', username: 'B', isAdmin: false };
	read = false;
	viewerCacheChanged();
	const b = await getReaderChapter('w_fixture', CHAPTER);
	delayed.resolve(old);
	const late = await pendingA;
	assert.equal(b.seriesTitle, 'B');
	assert.equal(late.seriesTitle, 'B');
	assert.equal(late.chapterRead, false);
	await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(calls.length, 2, 'late A completion cannot replace the current B memo');
});

test('token generations, content preferences and backend selection each invalidate bundles', async () => {
	await getReaderChapter('w_fixture', CHAPTER);
	viewerCacheChanged(); // Same user, new token or revoked session.
	await getReaderChapter('w_fixture', CHAPTER);
	g.auth.user!.showNsfw = true;
	await getReaderChapter('w_fixture', CHAPTER);
	const beforePromotion = await getReaderChapter('w_fixture', CHAPTER);
	g.auth.user!.isAdmin = true;
	assert.equal(
		readerViewIsCurrent(beforePromotion),
		false,
		'role changes invalidate completed views',
	);
	await getReaderChapter('w_fixture', CHAPTER);
	viewerCacheChanged(); // Saved block rules or a translator change.
	await getReaderChapter('w_fixture', CHAPTER);
	g.config.apiEndpoint = 'https://other.fixture/graphql';
	await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(calls.length, 6);
});

test('explicit unread succeeds and the next reader fetch reflects it', async () => {
	const before = await getReaderChapter('w_fixture', CHAPTER);
	g.backend.setChapterRead = async (_id: string, value: boolean) => {
		read = value;
		return true;
	};
	assert.equal(await setChapterRead(CHAPTER, false), true);
	assert.equal(readerViewIsCurrent(before), false, 'completed read-state preloads are invalid too');
	const after = await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(after.chapterRead, false);
	assert.equal(calls.length, 2);
});

test('an old viewer progress completion cannot mark a new viewer memo read', async () => {
	await getReaderChapter('w_fixture', CHAPTER);
	const write = deferred<void>();
	g.backend.setProgress = () => write.promise;
	const saved = saveProgress(CHAPTER, 0, true);
	g.auth.user = { id: 'B', username: 'B', isAdmin: false };
	read = false;
	viewerCacheChanged();
	await getReaderChapter('w_fixture', CHAPTER);
	write.resolve();
	await saved;
	assert.equal((await getReaderChapter('w_fixture', CHAPTER)).chapterRead, false);
	assert.equal(calls.length, 2);
});

test('a read saved while the bundle fill is in flight survives the older fill answer', async () => {
	const fill = deferred<ReturnType<typeof bundle>>();
	g.backend.seriesPage = async (workId: string) => {
		calls.push(workId);
		return fill.promise;
	};
	read = false;
	const pending = getReaderChapter('w_fixture', CHAPTER);
	g.backend.setProgress = async () => {};
	await saveProgress(CHAPTER, 0, true);
	// The server answered from a snapshot taken before the progress write landed.
	fill.resolve(bundle('w_fixture'));
	assert.equal((await pending).chapterRead, true);
	assert.equal((await getReaderChapter('w_fixture', CHAPTER)).chapterRead, true);
	assert.equal(calls.length, 1, 'the corrected fill is the memo that is reused');
});

test('bounded LRU evicts the least recently used resolved work', async () => {
	for (let n = 0; n < 32; n++) await getReaderChapter('w_' + n, CHAPTER);
	await getReaderChapter('w_0', CHAPTER); // Promote the earliest entry.
	await getReaderChapter('w_32', CHAPTER);
	await getReaderChapter('w_0', CHAPTER);
	assert.equal(calls.filter((id) => id === 'w_0').length, 1);
	await getReaderChapter('w_1', CHAPTER);
	assert.equal(calls.filter((id) => id === 'w_1').length, 2);
});

test('resolved eviction keeps ownership of a pending work and shares its fill', async () => {
	const pending = deferred<ReturnType<typeof bundle>>();
	const regular = g.backend.seriesPage as (id: string) => Promise<ReturnType<typeof bundle>>;
	g.backend.seriesPage = (id: string) => {
		if (id !== 'w_pending') return regular(id);
		calls.push(id);
		return pending.promise;
	};
	const first = getReaderChapter('w_pending', CHAPTER);
	for (let n = 0; n < 34; n++) await getReaderChapter('w_' + n, CHAPTER);
	const second = getReaderChapter('w_pending', CHAPTER);
	pending.resolve(bundle('w_pending'));
	await Promise.all([first, second]);
	assert.equal(calls.filter((id) => id === 'w_pending').length, 1);
});

test('expiry causes a fresh bundle without sacrificing non-expired reuse', async (t) => {
	let now = 100_000;
	t.mock.method(Date, 'now', () => now);
	await getReaderChapter('w_fixture', CHAPTER);
	now += 59_000;
	await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(calls.length, 1);
	now += 2_000;
	await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(calls.length, 2);
});

test('a waiter timeout does not drop the raw owner or start a duplicate bundle RPC', async (t) => {
	const pending = deferred<ReturnType<typeof bundle>>();
	g.backend.seriesPage = async (id: string) => {
		calls.push(id);
		return pending.promise;
	};
	t.mock.timers.enable({ apis: ['setTimeout'] });
	const first = getReaderChapter('w_fixture', CHAPTER);
	for (let n = 0; n < 5; n++) await Promise.resolve();
	t.mock.timers.tick(20_000);
	assert.deepEqual((await first).pages, []);
	const second = getReaderChapter('w_fixture', CHAPTER);
	assert.equal(calls.length, 1, 'still shares the actual RPC after an earlier waiter timed out');
	pending.resolve(bundle('w_fixture'));
	assert.equal((await second).seriesTitle, 'A');
	await getReaderChapter('w_fixture', CHAPTER);
	assert.equal(calls.length, 1);
});

test('scope changes and explicit clears retain global pending capacity until raw settlement', async () => {
	const pending = deferred<ReturnType<typeof bundle>>();
	g.backend.seriesPage = async (id: string) => {
		calls.push(id);
		return pending.promise;
	};
	const oldWaiters = Array.from({ length: 32 }, (_, n) => getReaderChapter('w_' + n, CHAPTER));
	g.auth.user = { id: 'B', username: 'B', isAdmin: false };
	read = false;
	viewerCacheChanged();
	clearReaderWorkCache();
	assert.deepEqual((await getReaderChapter('w_new', CHAPTER)).pages, []);
	assert.equal(calls.length, 32, 'old-generation owners still consume the global 32-fill limit');
	// All raw owners settle together. Their waiters retry under B and share the new
	// scope, but old values themselves cannot enter B's resolved cache.
	g.backend.seriesPage = async (id: string) => {
		calls.push(id);
		return bundle(id);
	};
	pending.resolve(bundle('w_old'));
	await Promise.all(oldWaiters);
	assert.equal((await getReaderChapter('w_new', CHAPTER)).seriesTitle, 'B');
});

test('viewer refresh waits for an active navigation and still handles a settled aborted one', async () => {
	const navigation = deferred<void>();
	g.navigating.to = { url: new URL('https://fixture.test/read/w_fixture') };
	g.navigating.complete = navigation.promise;
	g.invalidated.length = 0;
	viewerCacheChanged();
	const refresh = refreshViewerLoads();
	await new Promise((resolve) => setTimeout(resolve, 5));
	assert.deepEqual(g.invalidated, []);
	navigation.resolve();
	await refresh;
	assert.deepEqual(g.invalidated, ['all']);
	// `to` intentionally remains set, as it can after an aborted Kit navigation.
	viewerCacheChanged();
	await refreshViewerLoads();
	assert.deepEqual(g.invalidated, ['all', 'all']);
	g.navigating.to = null;
	g.navigating.complete = undefined;
});
