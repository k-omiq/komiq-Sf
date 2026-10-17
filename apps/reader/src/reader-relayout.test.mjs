import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { setImmediate } from 'node:timers/promises';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { compileModule } from 'svelte/compiler';
import ts from 'typescript';

// Exercise the page's actual recovery declarations and effects with Svelte runes,
// without mounting the unrelated reader chrome or needing a browser/backend.
const page = readFileSync(new URL('./routes/read/[slug]/+page.svelte', import.meta.url), 'utf8');
const script = page.match(/<script lang="ts">([\s\S]*?)<\/script>/)[1];
const ast = ts.createSourceFile('reader.ts', script, ts.ScriptTarget.Latest, true);
const names = new Set([
	'loadedChapterId',
	'loadedPages',
	'relaidPages',
	'pages',
	'total',
	'pageIdx',
	'pagedIdx',
	'scrolledPage',
	'scrollFraction',
	'maxScrolledPage',
	'warmedUrls',
	'readMarked',
	'pageRatio',
	'STARTER_PAGES',
	'queueUntil',
	'settledPages',
	'pageError',
	'pageRoute',
	'routeOf',
	'verifiedSrc',
	'recovery',
	'barHover',
	'QUEUE_CONCURRENCY',
	'waitsInQueue',
	'advanceQueue',
	'settlePage',
	'onImgError',
	'failPage',
	'packs',
	'packSrc',
	'pageSrc',
	'learnRatio',
]);
const statements = ast.statements.filter((node) => {
	if (ts.isVariableStatement(node)) {
		return node.declarationList.declarations.some((d) => names.has(d.name.getText(ast)));
	}
	if (ts.isFunctionDeclaration(node)) return names.has(node.name?.text);
	return (
		ts.isExpressionStatement(node) &&
		[
			'relaidPages = null;',
			'loadedChapterId;',
			'const current = pages;',
			'recovery;',
			'const prefetch = new ChapterPackPrefetch',
		].some((text) => node.getText(ast).includes(text))
	);
});
assert.equal(
	statements.filter(ts.isExpressionStatement).length,
	5,
	'relayout reset, chapter reset, cleanup, recovery and pack effects',
);
const apiUrl = new URL('../../../packages/api/src/media-routes.ts', import.meta.url).href;
const fixtureUrl = new URL('./reader-relayout-fixture.svelte.js', import.meta.url).href;
const fixtureSource = `
import { ChapterMediaRecovery, ChapterPackPrefetch } from ${JSON.stringify(apiUrl)};
export function createReader(fixture) {
	let data = $state.raw(fixture.data);
	const untrack = fixture.untrack;
	const currentMediaRouter = () => fixture.router;
	const freshRoutedPages = fixture.refresh;
	const resetWindowScroll = () => {};
	const updateStripProgress = () => {};
	const isStrip = fixture.isStrip ?? false;
	const mode = fixture.mode ?? 'strip';
	const Image = fixture.Image ?? globalThis.Image;
	const jumpTo = (i) => fixture.jumped?.push(i);
	const tick = () => Promise.resolve();
	const window = fixture.window ?? new EventTarget();
	${statements.map((node) => node.getText(ast)).join('\n')}
	return {
		fail: onImgError,
		src: pageSrc,
		enablePacks: () => { packs = true; },
		verifiedSingle: (i, blob) => { verifiedSrc = { ...verifiedSrc, [i]: { url: pages[i].url, blob } }; },
		load: (next) => { data = next; },
		seed: () => {
			pageRatio = { 0: 2 }; pageError = { 0: true }; pageRoute = { 9: { url: 'old', media: null } }; verifiedSrc = { 9: { url: 'old', blob: 'blob:old' } };
			settledPages.add(0); queueUntil = 10; warmedUrls.add('old');
			barHover = { i: 10, x: 10 }; pageIdx = 10;
			scrolledPage = 10; maxScrolledPage = 10;
		},
		get view() {
			return { pages, total, pageRoute, verifiedSrc, packSrc, pageError, pageRatio, pageIdx, pagedIdx, scrolledPage,
				maxScrolledPage,
				queueUntil, settled: [...settledPages], warmed: [...warmedUrls], barHover,
				relaid: relaidPages !== null };
		}
	};
}`;

registerHooks({
	resolve(specifier, context, next) {
		if (specifier === 'komika-reader-relayout-fixture') {
			return { url: fixtureUrl, shortCircuit: true };
		}
		if (specifier === './pack.js' && context.parentURL === apiUrl) {
			return {
				url: new URL('../../../packages/api/src/pack.ts', import.meta.url).href,
				shortCircuit: true,
			};
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (url === fixtureUrl || url === apiUrl) {
			const source = ts.transpileModule(
				url === fixtureUrl ? fixtureSource : readFileSync(fileURLToPath(url), 'utf8'),
				{ compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext } },
			).outputText;
			return {
				format: 'module',
				source:
					url === fixtureUrl
						? compileModule(source, { filename: 'reader-relayout.svelte.js', generate: 'client' })
								.js.code
						: source,
				shortCircuit: true,
			};
		}
		return next(url, context);
	},
});

const { effect_root, flush, untrack } = await import('svelte/internal/client');
const { createReader } = await import('komika-reader-relayout-fixture');
const { MediaRouter } = await import(apiUrl);
const router = new MediaRouter({
	routes: [{ id: 'owner', origin: 'http://localhost', kind: 'OWNER', region: null }],
});
const media = { kind: 'media', routeId: 'owner', path: `/media/v1/o/copy/${'a'.repeat(64)}` };
const routed = (prefix, count) =>
	Array.from({ length: count }, (_, i) => ({
		url: `${prefix}${i}`,
		media,
	}));
const view = (chapterId, pages) => ({ chapterId, pages });
function open(data, refresh, strip = {}) {
	let reader;
	const dispose = effect_root(() => {
		reader = createReader({ data, refresh, router, untrack, ...strip });
	});
	flush();
	return { reader, dispose };
}
async function settle() {
	for (let i = 0; i < 5; i++) await Promise.resolve();
	flush();
}

for (const count of [1, 3]) {
	test(`reader applies a ${count}-page relayout without stale errors or another refresh`, async (t) => {
		const released = [];
		t.mock.method(URL, 'revokeObjectURL', (url) => released.push(url));
		const initial = routed('blob:old-', 2);
		const fresh = routed('blob:fresh-', count);
		let refreshes = 0;
		const { reader, dispose } = open(view('chapter', initial), async () => {
			refreshes++;
			return fresh;
		});
		t.after(dispose);
		reader.seed();
		reader.fail(0);
		reader.fail(1);
		await settle();
		const result = reader.view;
		assert.equal(result.total, count);
		assert.deepEqual(
			result.pages,
			fresh.map((p, index) => ({
				...p,
				verify: null,
				pack: null,
				index,
				ratio: '800 / 1200',
				label: String(index + 1).padStart(2, '0'),
				dim: '',
			})),
		);
		for (const name of ['pageRoute', 'verifiedSrc', 'pageError', 'pageRatio']) {
			assert.deepEqual(result[name], {});
		}
		assert.deepEqual(result.settled, []);
		assert.deepEqual(result.warmed, []);
		assert.equal(result.barHover, null);
		assert.equal(result.queueUntil, 3);
		assert.equal(result.pageIdx, count - 1);
		assert.equal(result.pagedIdx, count - 1);
		assert.equal(result.scrolledPage, count);
		assert.equal(result.maxScrolledPage, count);
		assert.deepEqual(
			released,
			initial.map((p) => p.url),
		);
		reader.fail(0);
		await settle();
		assert.equal(refreshes, 1, 'replacement recovery cannot fetch or relayout again');
		assert.equal(reader.view.pageError[0], true, 'a fresh page can still fail normally');
		dispose();
		assert.deepEqual(
			released,
			[...initial, ...fresh].map((p) => p.url),
		);
	});
}

for (const change of ['chapter', 'pages']) {
	test(`reader clears its relayout when loaded ${change} changes`, async (t) => {
		const initial = routed('http://localhost/old/', 2);
		const fresh = routed('http://localhost/fresh/', 1);
		let refreshes = 0;
		const { reader, dispose } = open(view('chapter', initial), async () => {
			refreshes++;
			return fresh;
		});
		t.after(dispose);
		reader.fail(0);
		await settle();
		assert.equal(reader.view.relaid, true);
		const next = change === 'chapter' ? view('next', initial) : view('chapter', [...initial]);
		reader.load(next);
		flush();
		assert.equal(reader.view.relaid, false);
		assert.equal(reader.view.pages, next.pages);
		assert.equal(reader.view.total, 2);
		reader.fail(0);
		await settle();
		assert.equal(refreshes, 2, 'the newly loaded chapter has its own refresh');
	});
}

test('reader retains its relayout and refresh budget when only chapter metadata changes', async (t) => {
	const initial = view('chapter', routed('http://localhost/old/', 2));
	let refreshes = 0;
	const { reader, dispose } = open(initial, async () => {
		refreshes++;
		return routed('http://localhost/fresh/', 1);
	});
	t.after(dispose);
	reader.fail(0);
	await settle();
	const fresh = reader.view.pages;
	reader.load({ ...initial, chapterRead: true });
	flush();
	assert.equal(reader.view.relaid, true);
	assert.equal(reader.view.pages, fresh);
	reader.fail(0);
	await settle();
	assert.equal(refreshes, 1);
});

test('reader ignores a relayout arriving after a chapter change and releases its blob URLs', async (t) => {
	const released = [];
	t.mock.method(URL, 'revokeObjectURL', (url) => released.push(url));
	let finish;
	const { reader, dispose } = open(
		view('old', routed('http://localhost/old/', 2)),
		() =>
			new Promise((resolve) => {
				finish = resolve;
			}),
	);
	t.after(dispose);
	reader.fail(0);
	const next = view('next', routed('http://localhost/next/', 2));
	reader.load(next);
	flush();
	finish(routed('blob:discard-', 1));
	await settle();
	assert.equal(reader.view.pages, next.pages);
	assert.equal(reader.view.relaid, false);
	assert.deepEqual(reader.view.pageError, {});
	assert.deepEqual(released, ['blob:discard-0']);
});

test('strip relayout goes to the start of the clamped page', async (t) => {
	const jumped = [];
	const { reader, dispose } = open(
		view('chapter', routed('http://localhost/old/', 12)),
		async () => routed('http://localhost/fresh/', 3),
		{ isStrip: true, jumped },
	);
	t.after(dispose);
	reader.seed();
	reader.fail(0);
	await settle();
	assert.equal(reader.view.scrolledPage, 3);
	assert.deepEqual(jumped, [2]);
});

function packPages() {
	const data = ['one', 'two', 'one'];
	const expected = data.map((s) => ({
		sha256: createHash('sha256').update(s).digest('hex'),
		length: 3,
		mime: 'image/png',
	}));
	const chosen = new MediaRouter({
		routes: [
			{ id: 'owner', origin: 'http://localhost', kind: 'OWNER', region: null },
			{ id: 'regional', origin: 'http://localhost:9001', kind: 'REGIONAL', region: null },
		],
		storage: {
			getItem: () => JSON.stringify({ choice: 'regional', rounds: 3 }),
			setItem: () => {},
		},
	});
	const pages = expected.map((expect) => {
		const domain = {
			index: 0,
			sourceUrl: '',
			mediaPath: `/media/v1/o/copy/${expect.sha256}`,
			mediaSha256: expect.sha256,
			mediaLength: expect.length,
			mediaMime: expect.mime,
			mediaPackPath: '/media/v1/p/copy/2',
		};
		const media = chosen.pageMedia(domain);
		return {
			url: chosen.href(media),
			media,
			verify: chosen.verification(media),
			pack: chosen.pagePack(domain, media),
		};
	});
	const json = new TextEncoder().encode(
		JSON.stringify({
			v: 1,
			copy_id: 'copy',
			revision: 2,
			pages: expected.map((p, i) => ({ ...p, offset: i === 1 ? 3 : 0 })),
		}),
	);
	const prefix = new Uint8Array(8 + json.length + 3);
	prefix.set(new TextEncoder().encode('KPK1'));
	new DataView(prefix.buffer).setUint32(4, json.length);
	prefix.set(json, 8);
	prefix.set(new TextEncoder().encode('one'), 8 + json.length);
	return { pages, chosen, prefix };
}

async function waitFor(condition) {
	for (let n = 0; n < 50; n++) {
		await setImmediate();
		flush();
		if (condition()) return;
	}
	assert.fail('reader did not settle');
}

test('reader default-off pack effect makes no request and leaves owner singles immediate', (t) => {
	const { pages } = packPages();
	t.mock.method(globalThis, 'fetch', async () => assert.fail('default-off pack request'));
	const ownerPages = pages.map((p) => ({ ...p, verify: null }));
	const { reader, dispose } = open(view('chapter', ownerPages), async () => []);
	t.after(dispose);
	assert.deepEqual(reader.view.packSrc, {});
	assert.equal(reader.src(0), ownerPages[0].url);
});

/** Resolves when the page reports its pack incomplete, with the undelivered count. */
function packFailure(t) {
	let report;
	const reported = new Promise((resolve) => {
		report = resolve;
	});
	t.mock.method(console, 'debug', (message) => report(message));
	return reported;
}

test('reader pack effect keeps verified pages on a short body, leaves the rest on regional singles, and releases on pagehide', async (t) => {
	const { pages, chosen, prefix } = packPages();
	const win = new EventTarget();
	const revoked = [];
	let calls = 0;
	let signal;
	const failed = packFailure(t);
	t.mock.method(URL, 'createObjectURL', () => 'blob:pack');
	t.mock.method(URL, 'revokeObjectURL', (url) => revoked.push(url));
	t.mock.method(globalThis, 'fetch', async (_url, init) => {
		calls++;
		signal = init.signal;
		return new Response(prefix);
	});
	const { reader, dispose } = open(view('chapter', pages), async () => [], {
		router: chosen,
		window: win,
	});
	t.after(dispose);
	assert.equal(calls, 0);
	reader.enablePacks();
	flush();
	assert.match(await failed, /1 page\(s\) load as singles/);
	await settle();
	assert.equal(calls, 1, 'blob publication and failure do not restart the pack effect');
	assert.equal(reader.src(0), 'blob:pack');
	assert.equal(reader.src(2), 'blob:pack');
	assert.deepEqual(reader.view.pageRoute, {}, 'pack failure changes no page route');
	assert.equal(reader.view.pages[1].url, pages[1].url);
	assert.equal(reader.src(1), null, 'page 1 waits for its own verified regional single');
	assert.deepEqual(reader.view.pageError, {});
	assert.equal(chosen.current().id, 'regional', 'the regional route is not marked down');
	assert.deepEqual(revoked, [], 'partial pack failure keeps verified URLs live');
	win.dispatchEvent(new Event('pagehide'));
	flush();
	assert.equal(signal.aborted, true);
	assert.deepEqual(revoked, ['blob:pack']);
	assert.deepEqual(reader.view.packSrc, {});
});

test('reader aborts its previous pack on chapter navigation and revokes its verified pages', async (t) => {
	const { pages, chosen, prefix } = packPages();
	let cancelled = false;
	const revoked = [];
	t.mock.method(URL, 'createObjectURL', () => 'blob:pack');
	t.mock.method(URL, 'revokeObjectURL', (url) => revoked.push(url));
	t.mock.method(
		globalThis,
		'fetch',
		async () =>
			new Response(
				new ReadableStream({
					start(c) {
						c.enqueue(prefix);
					},
					cancel() {
						cancelled = true;
					},
				}),
			),
	);
	const { reader, dispose } = open(view('chapter', pages), async () => [], { router: chosen });
	t.after(dispose);
	reader.enablePacks();
	flush();
	await waitFor(() => reader.src(0) === 'blob:pack');
	reader.load(view('next', []));
	flush();
	await waitFor(() => cancelled);
	assert.deepEqual(revoked, ['blob:pack']);
	assert.deepEqual(reader.view.packSrc, {});
});

test('pack failure also preserves pages already verified by the single-object window', async (t) => {
	const { pages, chosen } = packPages();
	let finish;
	const failed = packFailure(t);
	t.mock.method(
		globalThis,
		'fetch',
		() =>
			new Promise((resolve) => {
				finish = resolve;
			}),
	);
	const { reader, dispose } = open(view('chapter', pages), async () => [], { router: chosen });
	t.after(dispose);
	reader.enablePacks();
	flush();
	reader.verifiedSingle(0, 'blob:single');
	finish(new Response('bad header'));
	assert.match(await failed, /3 page\(s\) load as singles/);
	await settle();
	assert.equal(reader.src(0), 'blob:single');
	assert.deepEqual(reader.view.pageRoute, {});
	assert.equal(reader.src(1), null, 'page 1 stays on its regional single');
	assert.deepEqual(reader.view.pageError, {});
	assert.equal(chosen.current().id, 'regional');
});

for (const [name, respond] of [
	['404', async () => new Response('missing', { status: 404 })],
	[
		'network error',
		async () => {
			throw new TypeError('Failed to fetch');
		},
	],
]) {
	test(`a pack ${name} marks no route down and leaves every page on its regional single`, async (t) => {
		const { pages, chosen } = packPages();
		const failed = packFailure(t);
		t.mock.method(globalThis, 'fetch', respond);
		const { reader, dispose } = open(
			view('chapter', pages),
			async () => assert.fail('no refresh'),
			{
				router: chosen,
			},
		);
		t.after(dispose);
		reader.enablePacks();
		flush();
		assert.match(await failed, /3 page\(s\) load as singles/);
		await settle();
		assert.equal(chosen.current().id, 'regional', 'the regional route stays up');
		assert.deepEqual(reader.view.pageRoute, {});
		assert.deepEqual(reader.view.pageError, {});
		assert.deepEqual(reader.view.packSrc, {});
		for (const [i, page] of pages.entries()) {
			assert.equal(reader.view.pages[i].url, page.url);
			assert.ok(page.url.startsWith('http://localhost:9001/'));
		}
	});
}

test('paged mode learns the ratio of pack-delivered pages from their blobs', async (t) => {
	const { pages, chosen, prefix } = packPages();
	const probes = [];
	class FakeImage {
		naturalWidth = 1600;
		naturalHeight = 1000;
		set src(url) {
			probes.push(url);
			queueMicrotask(() => this.onload?.());
		}
	}
	packFailure(t);
	t.mock.method(URL, 'createObjectURL', () => 'blob:pack');
	t.mock.method(URL, 'revokeObjectURL', () => {});
	t.mock.method(globalThis, 'fetch', async () => new Response(prefix));
	const { reader, dispose } = open(view('chapter', pages), async () => [], {
		router: chosen,
		mode: 'double',
		Image: FakeImage,
	});
	t.after(dispose);
	reader.enablePacks();
	flush();
	await waitFor(() => reader.view.pageRatio[2] !== undefined);
	assert.deepEqual(probes, ['blob:pack', 'blob:pack']);
	assert.deepEqual(reader.view.pageRatio, { 0: 1.6, 2: 1.6 });
});

test('strip mode does not probe pack-delivered pages', async (t) => {
	const { pages, chosen, prefix } = packPages();
	const failed = packFailure(t);
	t.mock.method(URL, 'createObjectURL', () => 'blob:pack');
	t.mock.method(URL, 'revokeObjectURL', () => {});
	t.mock.method(globalThis, 'fetch', async () => new Response(prefix));
	const { reader, dispose } = open(view('chapter', pages), async () => [], {
		router: chosen,
		mode: 'strip',
		Image: class {
			set src(_url) {
				assert.fail('strip cells learn ratios from their own <img>');
			}
		},
	});
	t.after(dispose);
	reader.enablePacks();
	flush();
	await failed;
	assert.equal(reader.src(0), 'blob:pack');
	assert.deepEqual(reader.view.pageRatio, {});
});
