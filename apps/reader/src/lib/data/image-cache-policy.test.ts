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
