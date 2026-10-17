import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
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
]);
const statements = ast.statements.filter((node) => {
	if (ts.isVariableStatement(node)) {
		return node.declarationList.declarations.some((d) => names.has(d.name.getText(ast)));
	}
	if (ts.isFunctionDeclaration(node)) return names.has(node.name?.text);
	return (
		ts.isExpressionStatement(node) &&
		['relaidPages = null;', 'loadedChapterId;', 'const current = pages;', 'recovery;'].some(
			(text) => node.getText(ast).includes(text),
		)
	);
});
assert.equal(
	statements.filter(ts.isExpressionStatement).length,
	4,
	'relayout reset, chapter reset, cleanup, recovery effects',
);
const apiUrl = new URL('../../../packages/api/src/media-routes.ts', import.meta.url).href;
const fixtureUrl = new URL('./reader-relayout-fixture.svelte.js', import.meta.url).href;
const fixtureSource = `
import { ChapterMediaRecovery } from ${JSON.stringify(apiUrl)};
export function createReader(fixture) {
	let data = $state.raw(fixture.data);
	const untrack = fixture.untrack;
	const currentMediaRouter = () => fixture.router;
	const freshRoutedPages = fixture.refresh;
	const resetWindowScroll = () => {};
	const updateStripProgress = () => {};
	const isStrip = fixture.isStrip ?? false;
	const jumpTo = (i) => fixture.jumped?.push(i);
	const tick = () => Promise.resolve();
	${statements.map((node) => node.getText(ast)).join('\n')}
	return {
		fail: onImgError,
		load: (next) => { data = next; },
		seed: () => {
			pageRatio = { 0: 2 }; pageError = { 0: true }; pageRoute = { 9: { url: 'old', media: null } }; verifiedSrc = { 9: { url: 'old', blob: 'blob:old' } };
			settledPages.add(0); queueUntil = 10; warmedUrls.add('old');
			barHover = { i: 10, x: 10 }; pageIdx = 10;
			scrolledPage = 10; maxScrolledPage = 10;
		},
		get view() {
			return { pages, total, pageRoute, verifiedSrc, pageError, pageRatio, pageIdx, pagedIdx, scrolledPage,
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
