import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';

registerHooks({
	resolve(specifier, context, next) {
		if (specifier === '@tauri-apps/api/core') {
			return {
				url: 'data:text/javascript,export const invoke = async () => "http://127.0.0.1:18090"; export const convertFileSrc = () => "engine-image://localhost";',
				shortCircuit: true,
			};
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (url.endsWith('/packages/api/src/image-provider.ts')) {
			return {
				format: 'module',
				source: ts.transpileModule(readFileSync(fileURLToPath(url), 'utf8'), {
					compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
				}).outputText,
				shortCircuit: true,
			};
		}
		return next(url, context);
	},
});
const { WebImageProvider, NativeImageProvider } =
	await import('../../../../../packages/api/src/image-provider.ts');

test('own API pages and covers bypass old browser keys and preserve revisions/variants', async () => {
	const images = new WebImageProvider({
		apiOrigin: 'https://api.fixture',
		workerBaseUrl: 'https://img.fixture',
	});
	const page = await images.resolvePage({
		index: 0,
		sourceUrl: 'https://api.fixture/api/v1/manga/1/chapter/2/page/0?v=hash&w=320#page',
	});
	const url = new URL(page);
	assert.equal(url.origin, 'https://api.fixture');
	assert.equal(url.searchParams.get('v'), 'hash');
	assert.equal(url.searchParams.get('w'), '320');
	assert.equal(url.searchParams.get('cache_policy'), '2');
	assert.equal(url.hash, '#page');
	assert.equal(
		images.resolveCoverSync('/covers/w_fixture.webp?v=hash'),
		'https://api.fixture/covers/w_fixture.webp?v=hash&cache_policy=2',
	);
	const srcset = images.coverCardSrcSet('/covers/w_fixture.webp?v=hash');
	for (const candidate of srcset.split(', ')) {
		const variant = new URL(candidate.split(' ')[0]);
		assert.equal(variant.searchParams.get('v'), 'hash');
		assert.equal(variant.searchParams.get('cache_policy'), '2');
	}
});

test('direct mode versions first-party assets but leaves a foreign signed locator byte-exact', async () => {
	const images = new WebImageProvider({
		apiOrigin: 'https://api.fixture',
		workerBaseUrl: '',
		direct: true,
	});
	const signed = 'https://source.fixture/a.jpg?sig=a%2Bb%2Fz&expires=123';
	assert.equal(await images.resolvePage({ index: 0, sourceUrl: signed }), signed);
	assert.equal(images.resolveCoverSync(signed), signed);
	assert.equal(images.coverCardSrcSet('https://source.fixture/covers/work.webp?sig=a%2Bb'), '');
	assert.equal(
		await images.resolvePage({ index: 0, sourceUrl: '/api/v1/manga/1/chapter/2/page/0' }),
		'https://api.fixture/api/v1/manga/1/chapter/2/page/0?cache_policy=2',
	);
});

test('Worker and server-proxy outer cache keys change without rewriting the upstream src', async () => {
	const source = 'https://uploads.mangadex.org/fixture.jpg?sig=a%2Bb%2Fz&expires=123';
	for (const serverProxy of [false, true]) {
		const images = new WebImageProvider({ workerBaseUrl: 'https://proxy.fixture', serverProxy });
		const url = new URL(await images.resolvePage({ index: 0, sourceUrl: source }));
		assert.equal(url.origin, 'https://proxy.fixture');
		assert.equal(url.pathname, '/img');
		assert.equal(url.searchParams.get('src'), source);
		assert.equal(url.searchParams.get('cache_policy'), '2');
	}
});

test('own-origin boundary checks remain exact and an existing policy query is replaced once', async () => {
	const images = new WebImageProvider({
		apiOrigin: 'https://api.fixture',
		workerBaseUrl: 'https://img.fixture',
	});
	const foreign = 'https://api.fixture.evil.test/a.jpg?cache_policy=1';
	const proxied = new URL(await images.resolvePage({ index: 0, sourceUrl: foreign }));
	assert.equal(proxied.origin, 'https://img.fixture');
	assert.equal(proxied.searchParams.get('src'), foreign);
	const own = new URL(
		await images.resolvePage({
			index: 0,
			sourceUrl: 'https://api.fixture/a.jpg?cache_policy=1&v=hash',
		}),
	);
	assert.deepEqual(own.searchParams.getAll('cache_policy'), ['2']);
	assert.equal(own.searchParams.get('v'), 'hash');
});

test('native chosen-server URLs retain their locator without a web cache-policy query', async () => {
	const images = new NativeImageProvider({
		workerBaseUrl: '',
		serverOrigin: 'https://api.fixture',
	});
	const source = 'https://api.fixture/api/v1/manga/1/chapter/2/page/0?v=hash';
	assert.equal(await images.resolvePage({ index: 0, sourceUrl: source }), source);
});

test('owner covers, pages and thumbnails rebase before own-asset and media-route checks', async () => {
	for (const serverProxy of [false, true]) {
		const images = new WebImageProvider({
			apiOrigin: 'https://fallback.example:8443',
			workerBaseUrl: 'https://fallback.example:8443',
			serverProxy,
			mediaOrigins: ['https://owner.example'],
		});
		images.setRebase('https://OWNER.example:443/', 'https://fallback.example:8443');
		for (const path of [
			'/covers/w_fixture.webp',
			'/api/v1/manga/1/thumbnail',
			'/api/v1/manga/1/chapter/2/page/0',
			'/media/v1/c/7/0',
		]) {
			const source = `https://owner.example${path}?v=hash#image`;
			const expected = `https://fallback.example:8443${path}?v=hash&cache_policy=2#image`;
			assert.equal(images.resolveCoverSync(source), expected);
			assert.equal(await images.resolveCover(source), expected);
			assert.equal(await images.resolvePage({ index: 0, sourceUrl: source }), expected);
			if (path.endsWith('.webp') || path.endsWith('/thumbnail')) {
				assert.equal(
					images.coverCardSrcSet(source),
					`https://fallback.example:8443${path}?v=hash&cache_policy=2&w=192#image 1x, ` +
						`https://fallback.example:8443${path}?v=hash&cache_policy=2&w=320#image 2x, ${expected} 3x`,
				);
			}
		}
	}
});

test('rebasing leaves foreign URLs, look-alike hosts and different ports on their usual routes', async () => {
	const images = new WebImageProvider({
		apiOrigin: 'https://fallback.example',
		workerBaseUrl: 'https://fallback.example',
		serverProxy: true,
		mediaOrigins: ['https://regional.example'],
	});
	const sources = [
		'https://foreign.example/covers/w_fixture.webp?sig=a%2Bb#image',
		'https://owner.example.evil/covers/w_fixture.webp',
		'https://owner.example:8443/covers/w_fixture.webp',
		'http://owner.example/covers/w_fixture.webp',
		'https://regional.example/covers/w_fixture.webp',
		'https://uploads.mangadex.org/covers/w_fixture.webp',
		'/covers/w_fixture.webp',
		'',
	];
	for (const source of sources) {
		images.setRebase(null, 'https://fallback.example');
		const expected = images.resolveCoverSync(source);
		images.setRebase('https://owner.example', 'https://fallback.example');
		assert.equal(images.resolveCoverSync(source), expected);
		assert.equal(await images.resolveCover(source), expected);
		assert.equal(await images.resolvePage({ index: 0, sourceUrl: source }), expected);
		if (!source.startsWith('/')) assert.equal(images.coverCardSrcSet(source), '');
		if (source.startsWith('https://foreign.example')) assert.equal(expected, source);
	}
});

test('null and equal origins disable a previously configured rebase', async () => {
	const images = new WebImageProvider({
		apiOrigin: 'https://fallback.example',
		workerBaseUrl: 'https://fallback.example',
		serverProxy: true,
	});
	const source = 'https://owner.example/covers/w_fixture.webp?v=hash#image';
	for (const from of [null, 'https://fallback.example:443/']) {
		images.setRebase('https://owner.example', 'https://fallback.example');
		images.setRebase(from, 'https://fallback.example');
		assert.equal(images.resolveCoverSync(source), source);
		assert.equal(await images.resolveCover(source), source);
		assert.equal(await images.resolvePage({ index: 0, sourceUrl: source }), source);
		assert.equal(images.coverCardSrcSet(source), '');
	}
});

test('the rebase destination is an own origin even without a configured API origin', async () => {
	const images = new WebImageProvider({ workerBaseUrl: 'https://img.fixture' });
	images.setRebase('https://owner.example', 'https://fallback.example');
	const source = 'https://owner.example/covers/w_fixture.webp';
	assert.equal(
		await images.resolveCover(source),
		'https://fallback.example/covers/w_fixture.webp?cache_policy=2',
	);
	assert.ok(images.coverCardSrcSet(source).startsWith('https://fallback.example/'));
});

test('native engine locators preserve their path and query without a web cache-policy query', async () => {
	const images = new NativeImageProvider({
		workerBaseUrl: '',
		serverOrigin: 'https://api.fixture',
	});
	assert.equal(
		await images.resolvePage({
			index: 0,
			sourceUrl: 'engine:/api/v1/manga/1/chapter/2/page/0?v=engine-revision',
		}),
		'http://127.0.0.1:18090/api/v1/manga/1/chapter/2/page/0?v=engine-revision',
	);
});
