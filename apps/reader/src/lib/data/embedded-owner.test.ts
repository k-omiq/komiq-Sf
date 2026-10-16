import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { beforeEach, mock, test } from 'node:test';
import { setImmediate } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import { testGlobals as g } from './node-test-hooks.ts';
import type { MediaRoute } from '@komika/types';

let target = 'embedded';
let fixtureId = 0;
const origin = 'https://fallback.example:8443';
const rebases: [string | null, string][] = [];
const owner: MediaRoute[] = [
	{ id: 'owner', base: 'https://main.example', kind: 'OWNER', region: null },
];

registerHooks({
	resolve(specifier, context, next) {
		if (specifier === '@tauri-apps/api/core') {
			return {
				url: 'data:text/javascript,export const invoke = async () => "http://127.0.0.1:18090"; export const convertFileSrc = () => "engine-image://localhost";',
				shortCircuit: true,
			};
		}
		const parent = context.parentURL ?? '';
		const media = /\/media\.ts\?/.test(parent);
		const layout = /\/\+layout\.ts\?/.test(parent);
		if ((media && specifier === './config') || (layout && specifier === '$lib/config')) {
			const source =
				`export const target = ${JSON.stringify(target)};` +
				`export const apiOrigin = ${JSON.stringify(origin)}; export const mediaHosts = [];`;
			return { url: `data:text/javascript,${encodeURIComponent(source)}`, shortCircuit: true };
		}
		if (media && specifier === './context') {
			return next('$lib/context', context);
		}
		if (media && specifier === '@komika/api') {
			return {
				url: new URL('../../../../../packages/api/src/media-routes.ts', import.meta.url).href,
				shortCircuit: true,
			};
		}
		if (layout && specifier === '$lib/media') {
			return {
				url: new URL(`../media.ts?fixture=${fixtureId}`, import.meta.url).href,
				shortCircuit: true,
			};
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (
			url.endsWith('/packages/api/src/media-routes.ts') ||
			url.endsWith('/packages/api/src/image-provider.ts')
		) {
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

async function fixture(buildTarget = 'embedded') {
	target = buildTarget;
	fixtureId++;
	const media = await import(`../media.ts?fixture=${fixtureId}`);
	const layout = await import(`../../routes/+layout.ts?fixture=${fixtureId}`);
	return { ...media, ...layout };
}

beforeEach(() => {
	rebases.length = 0;
	Object.defineProperty(globalThis, 'window', {
		configurable: true,
		value: { location: { origin } },
	});
	Object.assign(g.images, {
		setRebase: (from: string | null, to: string) => rebases.push([from, to]),
		resolvePage: async (page: { sourceUrl?: string }) => page.sourceUrl ?? '',
	});
	g.backend.mediaRoutes = async () => owner;
});

test('web (including static) and tauri never fetch the embedded owner or set a rebase', async () => {
	const lookup = mock.fn(async () => owner);
	g.backend.mediaRoutes = lookup;
	for (const buildTarget of ['web', 'tauri']) {
		const media = await fixture(buildTarget);
		assert.equal(await media.embeddedOwnerOrigin(), null);
		assert.equal(await media.ownerOriginReady(), null);
		assert.deepEqual(
			await media.routePage({ index: 0, sourceUrl: 'https://main.example/page' }, null),
			{ url: 'https://main.example/page', media: null },
		);
		assert.deepEqual(await media.load(), {});
	}
	assert.equal(lookup.mock.callCount(), 0);
	assert.deepEqual(rebases, []);
});

test('embedded startup waits for one owner lookup and sets the rebase before rendering', async () => {
	let finish!: (routes: MediaRoute[]) => void;
	const lookup = mock.fn(() => new Promise<MediaRoute[]>((resolve) => (finish = resolve)));
	g.backend.mediaRoutes = lookup;
	const media = await fixture();
	const ownerOrigin = media.embeddedOwnerOrigin();
	assert.equal(media.embeddedOwnerOrigin(), ownerOrigin, 'memoized lookup');
	let loaded = false;
	const loading = media.load().then(() => (loaded = true));
	await Promise.resolve();
	assert.equal(loaded, false);
	assert.deepEqual(rebases, []);
	finish(owner);
	await loading;
	assert.equal(await ownerOrigin, 'https://main.example');
	assert.deepEqual(rebases, [['https://main.example', origin]]);
	assert.equal(lookup.mock.callCount(), 1);
});

test('pages resolved alongside layout startup wait for the owner rebase', async () => {
	const { WebImageProvider } = await import('../../../../../packages/api/src/image-provider.ts');
	const images = new WebImageProvider({
		apiOrigin: origin,
		workerBaseUrl: origin,
		serverProxy: true,
	});
	Object.assign(g.images, {
		setRebase: images.setRebase.bind(images),
		resolvePage: images.resolvePage.bind(images),
	});
	let finish!: (routes: MediaRoute[]) => void;
	g.backend.mediaRoutes = () => new Promise<MediaRoute[]>((resolve) => (finish = resolve));
	const media = await fixture();
	let resolved = false;
	const pages = Promise.all(
		[0, 1].map((index) =>
			media.routePage(
				{ index, sourceUrl: `https://main.example/api/v1/manga/1/chapter/2/page/${index}?v=hash` },
				null,
			),
		),
	).then((pages) => {
		resolved = true;
		return pages;
	});
	const loading = media.load();
	await Promise.resolve();
	assert.equal(resolved, false);
	finish(owner);
	await loading;
	assert.deepEqual(
		await pages,
		[0, 1].map((index) => ({
			url: `${origin}/api/v1/manga/1/chapter/2/page/${index}?v=hash&cache_policy=2`,
			media: null,
		})),
	);
});

test('embedded startup waits only once and a late owner still sets the rebase', async () => {
	const media = await fixture();
	let finish!: (routes: MediaRoute[]) => void;
	const lookup = mock.fn(() => new Promise<MediaRoute[]>((resolve) => (finish = resolve)));
	g.backend.mediaRoutes = lookup;
	mock.timers.enable({ apis: ['setTimeout'] });
	try {
		let loaded = false;
		const loading = media.load().then(() => (loaded = true));
		const page = media.routePage({ index: 0, sourceUrl: 'https://main.example/page' }, null);
		await Promise.resolve();
		mock.timers.tick(2999);
		await Promise.resolve();
		assert.equal(loaded, false);
		const alongside = media.ownerOriginReady();
		mock.timers.tick(1);
		await loading;
		assert.equal(await alongside, null, 'concurrent calls share the startup deadline');
		assert.deepEqual(await page, { url: 'https://main.example/page', media: null });
		assert.deepEqual(rebases, []);
		let resolved = false;
		const laterPage = media.routePage({ index: 1, sourceUrl: 'https://main.example/next' }, null);
		void laterPage.then(() => (resolved = true));
		await setImmediate();
		assert.equal(resolved, true, 'later chapters do not wait again');
		assert.deepEqual(await laterPage, { url: 'https://main.example/next', media: null });
		assert.equal(lookup.mock.callCount(), 1);
		finish(owner);
		assert.equal(await media.embeddedOwnerOrigin(), 'https://main.example');
		assert.equal(await media.ownerOriginReady(), 'https://main.example');
		assert.deepEqual(rebases, [['https://main.example', origin]]);
	} finally {
		mock.timers.reset();
	}
});

test('failed owner discovery retries after a thirty-second cooldown without waiting again', async () => {
	mock.timers.enable({ apis: ['Date', 'setTimeout'], now: 1000 });
	try {
		for (const fail of [
			() => Promise.reject(new Error('lookup rejected')),
			() => {
				throw new Error('lookup threw');
			},
		]) {
			let attempts = 0;
			let finish!: (routes: MediaRoute[]) => void;
			const lookup = mock.fn(() =>
				++attempts === 1 ? fail() : new Promise<MediaRoute[]>((resolve) => (finish = resolve)),
			);
			g.backend.mediaRoutes = lookup;
			const media = await fixture();
			assert.equal(await media.ownerOriginReady(), null);
			mock.timers.tick(29_999);
			assert.equal(await media.ownerOriginReady(), null);
			assert.equal(await media.embeddedOwnerOrigin(), null);
			assert.equal(lookup.mock.callCount(), 1);
			mock.timers.tick(1);
			assert.equal(await media.ownerOriginReady(), null, 'a retry does not hold up a chapter');
			assert.equal(lookup.mock.callCount(), 2);
			const retry = media.embeddedOwnerOrigin();
			assert.equal(lookup.mock.callCount(), 2, 'one retry stays in flight');
			finish(owner);
			assert.equal(await retry, 'https://main.example');
			assert.equal(await media.ownerOriginReady(), 'https://main.example');
		}
		assert.deepEqual(rebases, [
			['https://main.example', origin],
			['https://main.example', origin],
		]);
	} finally {
		mock.timers.reset();
	}
});

test('missing, same-origin and failed owner lookups continue without rebasing', async () => {
	for (const response of [
		undefined,
		[],
		[{ ...owner[0], base: '' }],
		[{ ...owner[0], base: 'invalid' }],
	]) {
		g.backend.mediaRoutes = async () => response;
		const media = await fixture();
		assert.deepEqual(await media.load(), {});
		assert.equal(await media.embeddedOwnerOrigin(), null);
	}
	for (const failure of [
		() => Promise.reject(new Error('old server')),
		() => {
			throw new Error('lookup failed');
		},
	]) {
		g.backend.mediaRoutes = failure;
		const media = await fixture();
		assert.deepEqual(await media.load(), {});
		assert.equal(await media.embeddedOwnerOrigin(), null);
	}
	delete g.backend.mediaRoutes;
	assert.equal(await (await fixture()).embeddedOwnerOrigin(), null);
	Reflect.deleteProperty(globalThis, 'window');
	assert.equal(await (await fixture()).embeddedOwnerOrigin(), null);
	assert.deepEqual(rebases, []);
});
