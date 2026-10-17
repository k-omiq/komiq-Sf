// Endpoint classification for typed media routes (M5 design section 11): which origin
// each URL goes to, probe limits, stickiness, the owner fallback and the one refresh.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { mock, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import type { RoutedPage } from '@komika/api';
import './node-test-hooks.ts';

registerHooks({
	resolve(specifier, context, next) {
		if (specifier === '@tauri-apps/api/core') {
			return {
				url: 'data:text/javascript,export const invoke = async () => null;',
				shortCircuit: true,
			};
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (url.includes('/packages/api/src/') && url.endsWith('.ts')) {
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

const api = '../../../../../packages/api/src/';
const {
	ChapterMediaRecovery,
	MediaCheckError,
	MediaRouter,
	VerifiedObjectUrls,
	fetchVerifiedObject,
	mediaOrigin,
	parseMediaHosts,
	routedMedia,
	trustedMediaRoutes,
} = await import(api + 'media-routes.ts');
const { WebImageProvider } = await import(api + 'image-provider.ts');
const { GraphQLBackend } = await import(api + 'graphql-backend.ts');

const API = 'https://api.example';
const SG = 'https://media-sg.example';
const EU = 'https://media-eu.example';
const HELPER = 'https://media-helper.example';
const WORKER = 'https://img.example';
const HASH = 'a'.repeat(64);
const PATH = `/media/v1/o/mc_0123/${HASH}`;
const EXPECT = { sha256: HASH, length: 3, mime: 'image/webp' };
const page = (mediaPath: string | null = PATH, i = 0) => ({
	index: i,
	sourceUrl: `${API}/media/v1/c/7/${i}`,
	mediaPath,
	mediaSha256: mediaPath?.split('/').at(-1) ?? null,
	mediaLength: EXPECT.length,
	mediaMime: EXPECT.mime,
});
const serverRoutes = [
	{ id: 'owner', base: '', kind: 'OWNER', region: null },
	{ id: 'sg', base: SG, kind: 'REGIONAL', region: 'sg' },
	{ id: 'eu', base: EU, kind: 'REGIONAL', region: 'eu' },
	{ id: 'evil', base: 'https://evil.example', kind: 'REGIONAL', region: 'x' },
];
const helperRoute = { id: 'helper', base: HELPER, kind: 'HELPER', region: null };

function memoryStorage() {
	const map = new Map<string, string>();
	return {
		getItem: (k: string) => map.get(k) ?? null,
		setItem: (k: string, v: string) => void map.set(k, v),
	};
}

/**
 * A fake network on mocked timers: each origin answers `/media/v1/ready` after its
 * latency in ms, or fails at once ('fail'). Records every request.
 */
function fakeNet(latency: Record<string, number | 'fail'>) {
	const calls: { url: string; init: RequestInit | undefined }[] = [];
	const fetch = (input: string, init?: RequestInit) => {
		calls.push({ url: String(input), init });
		const ms = latency[new URL(String(input)).origin];
		if (ms === undefined || ms === 'fail') return Promise.reject(new TypeError('network'));
		return new Promise<Response>((resolve, reject) => {
			const timer = setTimeout(() => resolve(new Response(null, { status: 204 })), ms);
			init?.signal?.addEventListener('abort', () => {
				clearTimeout(timer);
				reject(new DOMException('aborted', 'AbortError'));
			});
		});
	};
	return { fetch: fetch as typeof globalThis.fetch, calls };
}

const flush = () => new Promise((r) => setImmediate(r));

/** Run a probe round to the end, moving the mocked clock in 5 ms steps. */
async function settle(round: Promise<void>): Promise<void> {
	let done = false;
	void round.then(() => (done = true));
	await flush();
	while (!done) {
		mock.timers.tick(5);
		await flush();
	}
}

/** Mocked `setTimeout` and `Date` for one test body. */
async function withClock(body: () => Promise<void>): Promise<void> {
	mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 1_000_000 });
	try {
		await body();
	} finally {
		mock.timers.reset();
	}
}

test('trusted routes: the server list intersected with the build allowlist, owner = API origin', () => {
	const routes = trustedMediaRoutes(serverRoutes, [SG, EU], API);
	assert.deepEqual(
		routes.map((r: { id: string; origin: string }) => [r.id, r.origin]),
		[
			['owner', API],
			['sg', SG],
			['eu', EU],
		],
	);
	// A server cannot add a host the build does not list, nor move the owner off the API.
	const moved = trustedMediaRoutes(
		[{ id: 'owner', base: 'https://evil.example', kind: 'OWNER', region: null }],
		[SG],
		API,
	);
	assert.deepEqual(
		moved.map((r: { origin: string }) => r.origin),
		[API],
	);
	// An old server (no list) or a failed list still leaves the owner.
	assert.deepEqual(
		trustedMediaRoutes(undefined, [SG], `${API}/`).map((r: { origin: string }) => r.origin),
		[API],
	);
	assert.deepEqual(parseMediaHosts(` ${SG}/ ,${EU} ${SG}`), [SG, EU]);
	assert.throws(() => parseMediaHosts('media-sg.example'), /PUBLIC_KOMIKA_MEDIA_HOSTS/);
	assert.throws(() => parseMediaHosts(`${SG}/path`), /PUBLIC_KOMIKA_MEDIA_HOSTS/);
});

test('a HELPER on the build allowlist is kept with its kind', () => {
	assert.deepEqual(trustedMediaRoutes([...serverRoutes, helperRoute], [HELPER], API), [
		{ id: 'owner', origin: API, kind: 'OWNER', region: null },
		{ id: 'helper', origin: HELPER, kind: 'HELPER', region: null },
	]);
});

test('a HELPER outside the build allowlist is dropped', () => {
	assert.deepEqual(
		trustedMediaRoutes([...serverRoutes, helperRoute], [SG], API),
		trustedMediaRoutes(serverRoutes, [SG], API),
	);
});

test('unknown route kinds are skipped even on the build allowlist', () => {
	assert.deepEqual(
		trustedMediaRoutes([...serverRoutes, { ...helperRoute, kind: 'EDGE' }], [HELPER], API),
		trustedMediaRoutes([], [], API),
	);
});

test('wildcard media hosts fail the build allowlist and are ignored in server routes', () => {
	for (const value of [
		'https://*',
		'https://*.example.com',
		'https://media*.example.com',
		'http://*.example.com',
	]) {
		assert.equal(mediaOrigin(value), null, value);
		assert.throws(() => parseMediaHosts(value), /PUBLIC_KOMIKA_MEDIA_HOSTS/, value);
		const routes = trustedMediaRoutes(
			[
				{ id: 'owner', base: value, kind: 'OWNER', region: null },
				{ id: 'wildcard', base: value, kind: 'REGIONAL', region: null },
				serverRoutes[1],
			],
			[value, SG],
			API,
		);
		assert.deepEqual(
			routes.map((r: { origin: string }) => r.origin),
			[API, SG],
			value,
		);
	}
});

test('classification: owner and regional objects load direct, foreign hosts via the Worker', async () => {
	const images = new WebImageProvider({
		workerBaseUrl: WORKER,
		apiOrigin: API,
		mediaOrigins: [SG, EU],
	});
	const cases: [string, string][] = [
		// owner (API origin): never through the Worker
		[`${API}/media/v1/c/7/0`, 'direct'],
		['/media/v1/c/7/0', 'direct'],
		[`${API}${PATH}`, 'direct'],
		// a configured regional route origin: never Worker-wrapped
		[`${SG}${PATH}`, 'direct'],
		[`${EU}/anything.webp`, 'direct'],
		// foreign hosts: the Worker
		['https://uploads.mangadex.org/covers/x.jpg', 'worker'],
		[`${API}:8443/media/v1/c/7/0`, 'worker'],
		['http://api.example/media/v1/c/7/0', 'worker'],
		['https://api.example.evil.test/media/v1/c/7/0', 'worker'],
		['https://media-sg.example.evil.test/x.jpg', 'worker'],
		['https://evil.example/media/v1/o/x', 'worker'],
	];
	for (const [src, want] of cases) {
		const got = await images.resolvePage({ index: 0, sourceUrl: src });
		const kind = got.startsWith(`${WORKER}/img?`) ? 'worker' : 'direct';
		assert.equal(kind, want, src);
		if (kind === 'direct') assert.ok(got.startsWith(src.startsWith('/') ? API : src), src);
	}
	// The serverProxy and direct modes keep route origins unwrapped too.
	const proxied = new WebImageProvider({
		workerBaseUrl: API,
		serverProxy: true,
		apiOrigin: API,
		mediaOrigins: [SG],
	});
	assert.equal(await proxied.resolveCover(`${SG}${PATH}`), `${SG}${PATH}`);
});

test('classification: owner default ports and host case keep locators off the Worker', async () => {
	for (const [apiOrigin, sourceOrigin] of [
		[`${API}:443`, API],
		['https://API.example', API],
		[API, `${API}:443`],
		[API, 'https://API.example'],
	]) {
		const images = new WebImageProvider({ workerBaseUrl: WORKER, apiOrigin });
		const sourceUrl = `${sourceOrigin}/media/v1/c/7/0`;
		assert.equal(
			await images.resolvePage({ index: 0, sourceUrl }),
			`${API}/media/v1/c/7/0?cache_policy=2`,
			`${apiOrigin} owns ${sourceUrl}`,
		);
	}
});

test('owner URLs with upper-case or mixed-case HTTP schemes stay absolute in every web mode', async () => {
	for (const scheme of ['HTTPS', 'HtTpS', 'HTTP', 'HtTp']) {
		const apiOrigin = `${scheme.toLowerCase()}://api.example`;
		for (const mode of [{}, { direct: true }, { serverProxy: true }]) {
			const images = new WebImageProvider({ workerBaseUrl: WORKER, apiOrigin, ...mode });
			const sourceOrigin = `${scheme}://api.example`;
			assert.equal(
				await images.resolvePage({ index: 0, sourceUrl: `${sourceOrigin}/media/v1/c/7/0` }),
				`${apiOrigin}/media/v1/c/7/0?cache_policy=2`,
			);
			const cover = `${sourceOrigin}/covers/w_case.webp?v=17`;
			const master = `${apiOrigin}/covers/w_case.webp?v=17&cache_policy=2`;
			assert.equal(await images.resolveCover(cover), master);
			assert.equal(
				images.coverCardSrcSet(cover),
				`${master}&w=192 1x, ${master}&w=320 2x, ${master} 3x`,
			);
		}
	}
});

test('exact objects skip the image provider; probes omit credentials and reject redirects', () =>
	withClock(async () => {
		const net = fakeNet({ [API]: 80, [SG]: 20 });
		const router = new MediaRouter({
			routes: trustedMediaRoutes(serverRoutes, [SG], API),
			fetch: net.fetch,
		});
		const media = router.pageMedia(page());
		assert.deepEqual(media, { kind: 'media', routeId: 'owner', path: PATH, expect: EXPECT });
		assert.equal(router.href(media), `${API}${PATH}`);
		// Not an exact object path: the page keeps its sourceUrl.
		assert.equal(router.pageMedia(page(null)), null);
		assert.equal(router.pageMedia(page('/media/v1/c/7/0')), null);
		assert.equal(router.pageMedia(page(`https://evil.example${PATH}`)), null);
		await settle(router.probe());
		assert.equal(net.calls.length, 2);
		for (const { url, init } of net.calls) {
			assert.match(url, /\/media\/v1\/ready$/);
			assert.equal(init?.credentials, 'omit');
			assert.equal(init?.redirect, 'error');
			assert.equal(init?.headers, undefined, 'no Authorization or any other header');
			assert.equal(init?.method, 'GET');
		}
	}));

test('only verifiable exact objects use the regional route; owner objects load directly', () =>
	withClock(async () => {
		const net = fakeNet({ [API]: 80, [SG]: 20 });
		const router = new MediaRouter({
			routes: trustedMediaRoutes(serverRoutes, [SG], API),
			fetch: net.fetch,
		});
		const owner = router.pageMedia(page())!;
		assert.equal(owner.routeId, 'owner');
		assert.deepEqual(owner.expect, EXPECT);
		assert.equal(router.verification(owner), null);
		assert.deepEqual(routedMedia(router, owner), {
			url: `${API}${PATH}`,
			media: owner,
			verify: null,
		});
		await settle(router.probe());
		assert.equal(router.current().id, 'sg');
		const regional = router.pageMedia({ ...page(), mediaMime: ' Image/WebP ; charset=x ' })!;
		assert.deepEqual(regional, { kind: 'media', routeId: 'sg', path: PATH, expect: EXPECT });
		assert.equal(router.verification(regional), regional.expect);
		assert.deepEqual(routedMedia(router, regional), {
			url: `${SG}${PATH}`,
			media: regional,
			verify: EXPECT,
		});
		const unknown = { ...regional, routeId: 'unknown' };
		assert.equal(router.href(unknown), `${API}${PATH}`);
		assert.equal(router.verification(unknown), null);
		const invalid = [
			{ mediaSha256: undefined },
			{ mediaSha256: null },
			{ mediaSha256: HASH.toUpperCase() },
			{ mediaSha256: 'a'.repeat(63) },
			{ mediaSha256: 'g'.repeat(64) },
			{ mediaSha256: HASH + '\n' },
			{ mediaSha256: 'b'.repeat(64) },
			{ mediaLength: undefined },
			{ mediaLength: null },
			{ mediaLength: 0 },
			{ mediaLength: -1 },
			{ mediaLength: 1.5 },
			{ mediaLength: NaN },
			{ mediaLength: Infinity },
			{ mediaLength: Number.MAX_SAFE_INTEGER + 1 },
			{ mediaMime: undefined },
			{ mediaMime: null },
			{ mediaMime: '' },
			{ mediaMime: '; charset=x' },
			{ mediaMime: 'webp' },
			{ mediaMime: 'image/' },
			{ mediaMime: '/webp' },
			{ mediaMime: 'image/ webp' },
			{ mediaMime: 'image/webp/extra' },
		];
		for (const fields of invalid) {
			const media = router.pageMedia({ ...page(), ...fields })!;
			assert.equal(media.routeId, 'owner', JSON.stringify(fields));
			assert.equal(media.expect, null);
			assert.equal(router.verification(media), null);
		}
		const fallback = router.fallback(regional)!;
		assert.equal(fallback.expect, regional.expect);
		assert.equal(router.verification(fallback), null);
	}));

test('a page without mediaSha256 uses the owner even when a HELPER is current', () =>
	withClock(async () => {
		const net = fakeNet({ [API]: 80, [HELPER]: 20 });
		const router = new MediaRouter({
			routes: trustedMediaRoutes([...serverRoutes, helperRoute], [HELPER], API),
			fetch: net.fetch,
		});
		await settle(router.probe());
		assert.equal(router.current().kind, 'HELPER');
		const media = router.pageMedia({ ...page(), mediaSha256: undefined })!;
		assert.deepEqual(media, { kind: 'media', routeId: 'owner', path: PATH, expect: null });
		assert.deepEqual(routedMedia(router, media), {
			url: `${API}${PATH}`,
			media,
			verify: null,
		});
		assert.equal(router.current().kind, 'HELPER');
	}));

test('an old server page without exact-object fields keeps its legacy URL', async () => {
	const legacy = { index: 0, sourceUrl: `${API}/media/v1/c/7/0` };
	const router = new MediaRouter({ routes: trustedMediaRoutes(serverRoutes, [SG], API) });
	assert.equal(router.pageMedia(legacy), null);
	const images = new WebImageProvider({ workerBaseUrl: WORKER, apiOrigin: API });
	assert.equal(await images.resolvePage(legacy), `${legacy.sourceUrl}?cache_policy=2`);
});

test('probes: only with more than one route, 1 per route per 60 s, 3 rounds per session', () =>
	withClock(async () => {
		const single = fakeNet({ [API]: 10 });
		await settle(
			new MediaRouter({
				routes: trustedMediaRoutes(serverRoutes, [], API),
				fetch: single.fetch,
			}).probe(),
		);
		assert.equal(single.calls.length, 0, 'owner alone: no probe');

		const net = fakeNet({ [API]: 100, [SG]: 20 });
		const storage = memoryStorage();
		const make = () =>
			new MediaRouter({
				routes: trustedMediaRoutes(serverRoutes, [SG], API),
				fetch: net.fetch,
				storage,
			});
		const router = make();
		await settle(Promise.all([router.probe(), router.probe()]).then(() => {}));
		assert.equal(net.calls.length, 2, 'concurrent calls share one round');
		await settle(router.probe());
		assert.equal(net.calls.length, 2, 'inside 60 s: no new probe');
		// A reload in the same tab keeps the limits (sessionStorage).
		mock.timers.tick(30_000);
		await settle(make().probe());
		assert.equal(net.calls.length, 2);
		for (let i = 0; i < 5; i++) {
			mock.timers.tick(61_000);
			await settle(make().probe());
		}
		assert.equal(net.calls.length, 6, 'three rounds per session, two routes each');
	}));

test('probe deadline: a hanging route is cut at 1.5 s', () =>
	withClock(async () => {
		let aborted = false;
		const fetch = (input: string, init?: RequestInit) =>
			new URL(input).origin === SG
				? new Promise<Response>((_, reject) => {
						init?.signal?.addEventListener('abort', () => {
							aborted = true;
							reject(new DOMException('aborted', 'AbortError'));
						});
					})
				: Promise.resolve(new Response(null, { status: 204 }));
		const router = new MediaRouter({
			routes: trustedMediaRoutes(serverRoutes, [SG], API),
			fetch: fetch as typeof globalThis.fetch,
		});
		const round = router.probe();
		await flush();
		mock.timers.tick(1499);
		await flush();
		assert.equal(aborted, false);
		mock.timers.tick(1);
		await round;
		assert.equal(aborted, true);
		assert.equal(router.current().id, 'owner', 'the hanging route is not chosen');
	}));

test('a probe whose body stalls past the deadline fails; a `__proto__` route id is throttled', () =>
	withClock(async () => {
		const calls: string[] = [];
		const fetch = (input: string, init?: RequestInit) => {
			calls.push(new URL(input).origin);
			if (new URL(input).origin === API) return Promise.reject(new TypeError('down'));
			// Headers at once, then a body that never ends until the abort.
			const body = new ReadableStream({
				start(controller) {
					init?.signal?.addEventListener('abort', () =>
						controller.error(new DOMException('aborted', 'AbortError')),
					);
				},
			});
			return Promise.resolve(new Response(body, { status: 200 }));
		};
		const storage = memoryStorage();
		const routes = trustedMediaRoutes(
			[...serverRoutes, { id: '__proto__', base: EU, kind: 'REGIONAL', region: null }],
			[SG, EU],
			API,
		);
		const make = () =>
			new MediaRouter({ routes, fetch: fetch as typeof globalThis.fetch, storage });
		const first = make();
		await settle(first.probe());
		assert.equal(first.current().id, 'owner', 'a stalled body is not a fast route');
		const before = calls.length;
		mock.timers.tick(10_000);
		await settle(make().probe());
		assert.equal(calls.length, before, 'every route, `__proto__` too, waits 60 s');
	}));

test('stickiness: a switch needs > 30 % and > 50 ms faster in two rounds in a row', () =>
	withClock(async () => {
		const latency: Record<string, number> = { [API]: 200, [SG]: 100, [EU]: 400 };
		const net = fakeNet(latency);
		const session = () => {
			const storage = memoryStorage();
			return async () => {
				mock.timers.tick(61_000);
				const r = new MediaRouter({
					routes: trustedMediaRoutes(serverRoutes, [SG, EU], API),
					fetch: net.fetch,
					storage,
				});
				await settle(r.probe());
				return r.current().id;
			};
		};
		let round = session();
		// Round 1 has nothing to hold: the fastest wins.
		assert.equal(await round(), 'sg');
		// The owner is faster by 40 ms only: not enough (needs > 50 ms).
		latency[API] = 60;
		assert.equal(await round(), 'sg');
		// Faster by > 30 % and > 50 ms once: still held (needs two rounds in a row).
		latency[API] = 30;
		assert.equal(await round(), 'sg');
		// The session's three rounds are used: nothing switches any more.
		assert.equal(await round(), 'sg');
		assert.equal(net.calls.length, 9);

		// A round that does not beat the choice resets the streak.
		round = session();
		latency[API] = 200;
		assert.equal(await round(), 'sg');
		latency[API] = 30;
		assert.equal(await round(), 'sg');
		latency[API] = 90; // 10 ms faster, under 30 %
		assert.equal(await round(), 'sg', 'non-consecutive wins never switch');

		// Faster by 40 % but by 40 ms only: the 50 ms floor holds.
		round = session();
		latency[API] = 200;
		latency[SG] = 100;
		assert.equal(await round(), 'sg');
		latency[SG] = 100;
		latency[API] = 60;
		assert.equal(await round(), 'sg');
		assert.equal(await round(), 'sg');

		// Two consecutive wins do switch.
		round = session();
		latency[API] = 200;
		assert.equal(await round(), 'sg');
		latency[API] = 30;
		assert.equal(await round(), 'sg');
		assert.equal(await round(), 'owner');
	}));

test('one owner fallback per object, then ONE fresh refresh, then nothing: no loop', () =>
	withClock(async () => {
		const net = fakeNet({ [API]: 100, [SG]: 10 });
		const router = new MediaRouter({
			routes: trustedMediaRoutes(serverRoutes, [SG], API),
			fetch: net.fetch,
		});
		await settle(router.probe());
		assert.equal(router.current().id, 'sg');
		const routed = [page(PATH, 0), page(`/media/v1/o/mc_0123/${'b'.repeat(64)}`, 1)].map((p) => {
			const m = router.pageMedia(p)!;
			return { url: router.href(m), media: m };
		});
		assert.equal(routed[0].url, `${SG}${PATH}`);
		let refreshes = 0;
		const moved = `/media/v1/o/mc_0123/${'c'.repeat(64)}`;
		const relayout = mock.fn();
		const recovery = new ChapterMediaRecovery(
			router,
			routed,
			async () => {
				refreshes++;
				// The bytes moved: page 0 has a new hash, page 1 falls back to its locator.
				const m = { kind: 'media', routeId: router.current().id, path: moved, expect: null };
				return [
					{ url: router.href(m), media: m },
					{ url: `${API}/media/v1/c/7/1?cache_policy=2`, media: null },
				];
			},
			relayout,
		);
		const B = `/media/v1/o/mc_0123/${'b'.repeat(64)}`;
		const locator = `${API}/media/v1/c/7/1?cache_policy=2`;
		// 1. A regional object fails: the same path on the owner; the region is down.
		const first = await recovery.recover(0, `${SG}${PATH}`);
		assert.deepEqual([...first.keys()], [0]);
		assert.equal(first.get(0)!.url, `${API}${PATH}`);
		assert.equal(router.current().id, 'owner');
		// A late error for the URL the page already left only repeats its current URL.
		const late = await recovery.recover(0, `${SG}${PATH}`);
		assert.equal(late.get(0)!.url, `${API}${PATH}`);
		// Another regional object of the chapter still gets its own owner retry.
		const other = await recovery.recover(1, `${SG}${B}`);
		assert.equal(other.get(1)!.url, `${API}${B}`);
		// 2. Both owner copies fail at once: one refresh; EACH caller gets its page back,
		// even the one whose refresh was applied by the other caller first.
		const [a, b] = await Promise.all([
			recovery.recover(0, `${API}${PATH}`),
			recovery.recover(1, `${API}${B}`),
		]);
		assert.equal(refreshes, 1, 'concurrent failures share one refresh');
		assert.equal(a.get(0)!.url, `${API}${moved}`);
		assert.equal(b.get(1)!.url, locator);
		// 3. The refreshed pages fail as well: nothing more (the retry button).
		assert.equal((await recovery.recover(0, `${API}${moved}`)).size, 0);
		assert.equal((await recovery.recover(1, locator)).size, 0);
		assert.equal((await recovery.recover(0, `${API}${moved}`)).size, 0);
		assert.equal(refreshes, 1);
		assert.equal(relayout.mock.callCount(), 0, 'same-count refresh only replaces URLs');
		// A legacy page (no exact object) never enters recovery.
		const legacy = new ChapterMediaRecovery(
			router,
			[{ url: `${API}/x`, media: null }],
			async () => {
				throw new Error('must not refresh');
			},
		);
		assert.equal((await legacy.recover(0, `${API}/x`)).size, 0);
	}));

function recoveryFixture() {
	// Owner only: recovery never starts a probe or makes a network request.
	const router = new MediaRouter({ routes: trustedMediaRoutes([], [], API) });
	const routed: RoutedPage[] = [PATH, `/media/v1/o/mc_0123/${'b'.repeat(64)}`].map((path) => {
		const media = router.pageMedia(page(path))!;
		return { url: router.href(media), media };
	});
	return { router, routed };
}

for (const count of [1, 3]) {
	test(`a fresh ${count}-page layout replaces two pages once across concurrent failures`, async () => {
		const { router, routed } = recoveryFixture();
		const fresh: RoutedPage[] = Array.from({ length: count }, (_, i) => ({
			url: `${API}/fresh/${i}`,
			media: null,
		}));
		let finish!: (pages: RoutedPage[]) => void;
		const refresh = mock.fn(() => new Promise<RoutedPage[]>((resolve) => (finish = resolve)));
		const relayout = mock.fn();
		const recovery = new ChapterMediaRecovery(router, routed, refresh, relayout);
		const first = recovery.recover(0, routed[0].url);
		const second = recovery.recover(1, routed[1].url);
		assert.equal(refresh.mock.callCount(), 1);
		assert.equal(relayout.mock.callCount(), 0, 'both failures wait for the shared refresh');
		finish(fresh);
		const changes = await Promise.all([first, second]);
		assert.deepEqual(
			changes.map((c) => c.size),
			[0, 0],
		);
		assert.equal(relayout.mock.callCount(), 1);
		assert.equal(relayout.mock.calls[0].arguments[0], fresh);
		assert.equal((await recovery.recover(0, routed[0].url)).size, 0);
		assert.equal(refresh.mock.callCount(), 1);
		assert.equal(relayout.mock.callCount(), 1, 'later failures cannot relayout again');
	});
}

test('a changed page count without a relayout callback keeps the legacy empty-map result', async () => {
	const { router, routed } = recoveryFixture();
	const refresh = mock.fn(async () => [{ url: `${API}/fresh/0`, media: null }]);
	const recovery = new ChapterMediaRecovery(router, routed, refresh);
	const changes = await Promise.all(routed.map((p, i) => recovery.recover(i, p.url)));
	assert.deepEqual(
		changes.map((c) => c.size),
		[0, 0],
	);
	assert.equal((await recovery.recover(0, routed[0].url)).size, 0);
	assert.equal(refresh.mock.callCount(), 1);
});

for (const outcome of ['empty', 'failed']) {
	test(`${outcome === 'empty' ? 'an empty' : 'a failed'} fresh refresh never relayouts or retries the refresh`, async () => {
		const { router, routed } = recoveryFixture();
		const refresh = mock.fn(async () => {
			if (outcome === 'failed') throw new Error('refresh failed');
			return [];
		});
		const relayout = mock.fn();
		const recovery = new ChapterMediaRecovery(router, routed, refresh, relayout);
		const changes = await Promise.all(routed.map((p, i) => recovery.recover(i, p.url)));
		assert.deepEqual(
			changes.map((c) => c.size),
			[0, 0],
		);
		assert.equal((await recovery.recover(0, routed[0].url)).size, 0);
		assert.equal(refresh.mock.callCount(), 1);
		assert.equal(relayout.mock.callCount(), 0);
	});
}

test('the refresh is a POST that skips a warm anonymous GET cache and sends no token', async () => {
	const requests: { method: string; auth: string | null }[] = [];
	const cached = { data: { pages: [{ index: 0, sourceUrl: 'stale', mediaPath: PATH }] } };
	const fresh = { data: { pages: [{ index: 0, sourceUrl: 'fresh', mediaPath: null }] } };
	const backend = new GraphQLBackend({
		endpoint: 'https://media-refresh.test/graphql',
		fetch: (async (_url: string, init?: RequestInit) => {
			const method = init?.method ?? 'GET';
			const headers = new Headers(init?.headers);
			requests.push({ method, auth: headers.get('authorization') });
			// A GET is answered by the browser/edge cache (stale); a POST reaches the server.
			return Response.json(method === 'GET' ? cached : fresh);
		}) as typeof fetch,
	});
	assert.equal((await backend.pages('7'))[0].sourceUrl, 'stale');
	assert.equal((await backend.pages('7', { fresh: true }))[0].sourceUrl, 'fresh');
	assert.deepEqual(requests, [
		{ method: 'GET', auth: null },
		{ method: 'POST', auth: null },
	]);
});

test('mediaRoutes has its own capability flag: an old server answers once, then never again', async () => {
	let calls = 0;
	const old = new GraphQLBackend({
		endpoint: 'https://media-old.test/graphql',
		fetch: (async () => {
			calls++;
			return Response.json({
				errors: [{ message: 'Unknown field "mediaRoutes" on type "Query"' }],
			});
		}) as typeof fetch,
	});
	assert.equal(await old.mediaRoutes(), undefined);
	assert.equal(await old.mediaRoutes(), undefined);
	assert.equal(calls, 1);
	const modern = new GraphQLBackend({
		endpoint: 'https://media-new.test/graphql',
		fetch: (async () => Response.json({ data: { mediaRoutes: serverRoutes } })) as typeof fetch,
	});
	assert.deepEqual(await modern.mediaRoutes(), serverRoutes);
});

const OBJECT_BYTES = new TextEncoder().encode('verified media object');

async function objectExpectation() {
	const digest = await globalThis.crypto.subtle.digest('SHA-256', OBJECT_BYTES);
	return {
		sha256: Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join(''),
		length: OBJECT_BYTES.byteLength,
		mime: 'image/webp',
	};
}

function objectResponse(bytes = OBJECT_BYTES, headers: Record<string, string> = {}) {
	return new Response(bytes, { headers: { 'content-type': 'image/webp', ...headers } });
}

function blobUrls() {
	const created: { blob: Blob; url: string }[] = [];
	const revoked: string[] = [];
	const evicted: string[] = [];
	return {
		created,
		revoked,
		evicted,
		createUrl: (blob: Blob) => {
			const url = `blob:test-${created.length}`;
			created.push({ blob, url });
			return url;
		},
		revokeUrl: (url: string) => void revoked.push(url),
		onEvict: (url: string) => void evicted.push(url),
	};
}

test('fetchVerifiedObject checks bytes and MIME, omits credentials and headers, and rejects redirects', async () => {
	const expect = await objectExpectation();
	const controller = new AbortController();
	let request: { url: string; init?: RequestInit } | undefined;
	const blob = await fetchVerifiedObject(`${SG}${PATH}`, expect, {
		fetch: (async (url, init) => {
			request = { url: String(url), init };
			return objectResponse(OBJECT_BYTES, {
				'content-type': 'Image/WebP; charset=x',
				'content-length': String(expect.length),
			});
		}) as typeof fetch,
		signal: controller.signal,
		subtle: globalThis.crypto.subtle,
	});
	assert.ok(blob instanceof Blob);
	assert.equal(blob.size, expect.length);
	assert.equal(blob.type, expect.mime);
	assert.deepEqual(new Uint8Array(await blob.arrayBuffer()), OBJECT_BYTES);
	assert.equal(request!.url, `${SG}${PATH}`);
	assert.equal(request!.init!.method, 'GET');
	assert.equal(request!.init!.credentials, 'omit');
	assert.equal(request!.init!.redirect, 'error');
	assert.equal(request!.init!.referrerPolicy, 'no-referrer');
	assert.equal(request!.init!.mode, 'cors');
	assert.equal(request!.init!.signal, controller.signal);
	assert.equal(Object.hasOwn(request!.init!, 'headers'), false);
	assert.equal(Object.hasOwn(request!.init!, 'cache'), false);
});

test('fetchVerifiedObject rejects mismatched hash, length, MIME and HTTP status', async () => {
	const expect = await objectExpectation();
	const cases = [
		{ name: 'hash', response: objectResponse(), expect: { ...expect, sha256: HASH }, reason: 'hash' },
		{ name: 'short body', response: objectResponse(OBJECT_BYTES.slice(1)), reason: 'length' },
		{
			name: 'long body',
			response: objectResponse(new Uint8Array([...OBJECT_BYTES, 0])),
			reason: 'length',
		},
		{
			name: 'MIME',
			response: objectResponse(OBJECT_BYTES, { 'content-type': 'image/png' }),
			reason: 'mime',
		},
		{ name: 'missing MIME', response: new Response(OBJECT_BYTES), reason: 'mime' },
		{ name: 'status', response: new Response(null, { status: 503 }), reason: 'status' },
	];
	for (const entry of cases) {
		await assert.rejects(
			fetchVerifiedObject(`${SG}${PATH}`, entry.expect ?? expect, {
				fetch: (async () => entry.response) as typeof fetch,
			}),
			(error: unknown) => error instanceof MediaCheckError && (error as { reason: string }).reason === entry.reason,
			entry.name,
		);
	}
});

test('fetchVerifiedObject rejects HTTP 206 even when the bytes and metadata match', async () => {
	const expect = await objectExpectation();
	await assert.rejects(
		fetchVerifiedObject(`${SG}${PATH}`, expect, {
			fetch: (async () =>
				new Response(OBJECT_BYTES, {
					status: 206,
					headers: { 'content-type': expect.mime, 'content-length': String(expect.length) },
				})) as typeof fetch,
		}),
		{ name: 'MediaCheckError', reason: 'status' },
	);
});

test('fetchVerifiedObject cancels an oversized stream before reading any more chunks', async () => {
	const expect = await objectExpectation();
	let reads = 0;
	let cancelled = false;
	const body = new ReadableStream<Uint8Array>({
		pull(controller) {
			reads++;
			controller.enqueue(OBJECT_BYTES);
		},
		cancel() {
			cancelled = true;
		},
	}, { highWaterMark: 0 });
	await assert.rejects(
		fetchVerifiedObject(`${SG}${PATH}`, expect, {
			fetch: (async () => new Response(body, { headers: { 'content-type': expect.mime } })) as typeof fetch,
		}),
		(error: unknown) => error instanceof MediaCheckError && (error as { reason: string }).reason === 'length',
	);
	assert.equal(cancelled, true);
	assert.equal(reads, 2);
});

test('fetchVerifiedObject handles a null body through arrayBuffer and reports crypto failures', async () => {
	const expect = await objectExpectation();
	const fetch = (async () => {
		const response = new Response(null, { headers: { 'content-type': expect.mime } });
		response.arrayBuffer = async () => OBJECT_BYTES.slice().buffer;
		return response;
	}) as typeof globalThis.fetch;
	const blob = await fetchVerifiedObject(`${SG}${PATH}`, expect, { fetch });
	assert.deepEqual(new Uint8Array(await blob.arrayBuffer()), OBJECT_BYTES);
	await assert.rejects(
		fetchVerifiedObject(`${SG}${PATH}`, expect, {
			fetch: (async () => new Response(null, { headers: { 'content-type': expect.mime } })) as typeof fetch,
		}),
		(error: unknown) => error instanceof MediaCheckError && (error as { reason: string }).reason === 'length',
	);
	await assert.rejects(
		fetchVerifiedObject(`${SG}${PATH}`, expect, {
			fetch,
			subtle: { digest: async () => { throw new Error('crypto unavailable'); } } as unknown as SubtleCrypto,
		}),
		(error: unknown) => error instanceof MediaCheckError && (error as { reason: string }).reason === 'crypto',
	);
	const crypto = Object.getOwnPropertyDescriptor(globalThis, 'crypto')!;
	try {
		Object.defineProperty(globalThis, 'crypto', { value: undefined, configurable: true });
		await assert.rejects(
			fetchVerifiedObject(`${SG}${PATH}`, expect, { fetch }),
			(error: unknown) => error instanceof MediaCheckError && (error as { reason: string }).reason === 'crypto',
		);
	} finally {
		Object.defineProperty(globalThis, 'crypto', crypto);
	}
});

test('VerifiedObjectUrls enforces its cap and revokes the oldest two objects', async () => {
	const expect = await objectExpectation();
	const urls = blobUrls();
	const cache = new VerifiedObjectUrls({
		cap: 2,
		fetch: (async () => objectResponse()) as typeof fetch,
		subtle: globalThis.crypto.subtle,
		...urls,
	});
	const sources = Array.from({ length: 4 }, (_, i) => `${SG}/object-${i}`);
	const blobs: string[] = [];
	for (const url of sources) blobs.push(await cache.get(url, expect));
	assert.equal(cache.size, 2);
	assert.deepEqual(urls.revoked, blobs.slice(0, 2));
	assert.deepEqual(urls.evicted, sources.slice(0, 2));
	assert.ok(urls.created.every(({ blob }) => blob.size === expect.length && blob.type === expect.mime));
	cache.clear();
	assert.equal(cache.size, 0);
	assert.deepEqual(urls.revoked, blobs);
	assert.deepEqual(urls.evicted, sources.slice(0, 2), 'clear does not report cap evictions');
	cache.clear();
	assert.deepEqual(urls.revoked, blobs, 'a released URL is not revoked twice');
});

test('VerifiedObjectUrls touches cached objects and shares one concurrent fetch per URL', async () => {
	const expect = await objectExpectation();
	const urls = blobUrls();
	let calls = 0;
	const cache = new VerifiedObjectUrls({
		cap: 2,
		fetch: (async () => {
			calls++;
			return objectResponse();
		}) as typeof fetch,
		...urls,
	});
	const [a, same] = await Promise.all([cache.get('a', expect), cache.get('a', expect)]);
	assert.equal(a, same);
	assert.equal(calls, 1);
	assert.equal(urls.created.length, 1);
	const b = await cache.get('b', expect);
	assert.equal(await cache.get('a', expect), a);
	await cache.get('c', expect);
	assert.equal(calls, 3);
	assert.deepEqual(urls.revoked, [b]);
	assert.deepEqual(urls.evicted, ['b']);
	assert.equal(cache.size, 2);
	cache.clear();
});

test('VerifiedObjectUrls keep releases other blobs, aborts fetches and revokes late blobs', async () => {
	const expect = await objectExpectation();
	const urls = blobUrls();
	const pending: { signal: AbortSignal; resolve: (response: Response) => void }[] = [];
	const cache = new VerifiedObjectUrls({
		cap: 3,
		fetch: (async (url, init) => {
			if (url !== 'pending') return objectResponse();
			// This transport ignores abort, so the cache must also discard its late result.
			return new Promise<Response>((resolve) => pending.push({ signal: init!.signal!, resolve }));
		}) as typeof fetch,
		...urls,
	});
	const a = await cache.get('a', expect);
	const b = await cache.get('b', expect);
	const late = cache.get('pending', expect);
	const rejected = assert.rejects(late, { name: 'AbortError' });
	cache.keep(new Set(['b']));
	assert.deepEqual(urls.revoked, [a]);
	assert.equal(cache.size, 1);
	assert.equal(pending[0].signal.aborted, true);
	// A new request for the same URL must survive the old request's late completion.
	const replacement = cache.get('pending', expect);
	pending[0].resolve(objectResponse());
	await rejected;
	assert.deepEqual(urls.revoked, [a, urls.created[2].url]);
	assert.equal(cache.size, 1);
	assert.equal(pending[1].signal.aborted, false);
	assert.equal(cache.get('pending', expect), replacement);
	pending[1].resolve(objectResponse());
	const fresh = await replacement;
	assert.equal(cache.size, 2);
	cache.clear();
	assert.equal(cache.size, 0);
	assert.deepEqual(urls.revoked, [a, urls.created[2].url, b, fresh]);
	assert.deepEqual(urls.evicted, []);
});

test('VerifiedObjectUrls clear aborts in-flight fetches and failed fetches can be retried', async () => {
	const expect = await objectExpectation();
	const urls = blobUrls();
	const networkError = new TypeError('network failed');
	let signal: AbortSignal | undefined;
	let calls = 0;
	const cache = new VerifiedObjectUrls({
		cap: 2,
		fetch: (async (_url, init) => {
			calls++;
			if (calls === 1) throw networkError;
			if (calls === 2) return objectResponse(OBJECT_BYTES, { 'content-type': 'image/png' });
			if (calls === 3) return objectResponse();
			signal = init!.signal!;
			return new Promise<Response>((_, reject) => {
				signal!.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
			});
		}) as typeof fetch,
		...urls,
	});
	await assert.rejects(cache.get('retry', expect), (error: unknown) => error === networkError);
	assert.equal(cache.size, 0);
	await assert.rejects(
		cache.get('retry', expect),
		(error: unknown) => error instanceof MediaCheckError && (error as { reason: string }).reason === 'mime',
	);
	assert.equal(cache.size, 0);
	assert.equal(urls.created.length, 0);
	const blob = await cache.get('retry', expect);
	const pending = cache.get('pending', expect);
	const rejected = assert.rejects(pending, { name: 'AbortError' });
	cache.clear();
	assert.equal(signal!.aborted, true);
	await rejected;
	assert.equal(cache.size, 0);
	assert.deepEqual(urls.revoked, [blob]);
});

for (const route of [serverRoutes[1], helperRoute]) {
	test(`a failed ${route.kind.toLowerCase()} verification recovers to the owner with verify null`, () =>
		withClock(async () => {
			const net = fakeNet({ [API]: 80, [route.base]: 20 });
			const router = new MediaRouter({
				routes: trustedMediaRoutes([serverRoutes[0], route], [route.base], API),
				fetch: net.fetch,
			});
			await settle(router.probe());
			assert.equal(router.current().kind, route.kind);
			const expect = await objectExpectation();
			const mediaPath = `/media/v1/o/mc_0123/${expect.sha256}`;
			const media = router.pageMedia({
				...page(mediaPath),
				mediaSha256: expect.sha256,
				mediaLength: expect.length,
			})!;
			assert.equal(media.routeId, route.id);
			assert.equal(router.verification(media), media.expect);
			const routed = routedMedia(router, media);
			assert.equal(routed.url, `${route.base}${mediaPath}`);
			assert.deepEqual(routed.verify, expect);
			await assert.rejects(
				fetchVerifiedObject(routed.url, routed.verify!, {
					fetch: (async () => objectResponse(new Uint8Array(OBJECT_BYTES.length))) as typeof fetch,
				}),
				(error: unknown) => error instanceof MediaCheckError && (error as { reason: string }).reason === 'hash',
			);
			let refreshes = 0;
			const recovery = new ChapterMediaRecovery(router, [routed], async () => {
				refreshes++;
				return [];
			});
			const result = await recovery.recover(0, routed.url);
			assert.deepEqual(result.get(0), {
				url: `${API}${mediaPath}`,
				media: { ...media, routeId: 'owner' },
				verify: null,
			});
			assert.equal(refreshes, 0);
			assert.equal(router.current().id, 'owner');
		}));
}

test('a refused status or MIME cancels the rest of the body', async () => {
	const expect = await objectExpectation();
	for (const [init, reason] of [
		[{ status: 503, headers: { 'content-type': 'image/webp' } }, 'status'],
		[{ headers: { 'content-type': 'text/html' } }, 'mime'],
	] as const) {
		let cancelled = false;
		const body = new ReadableStream<Uint8Array>({
			pull() {},
			cancel() {
				cancelled = true;
			},
		});
		await assert.rejects(
			fetchVerifiedObject(`${SG}${PATH}`, expect, {
				fetch: (async () => new Response(body, init)) as typeof fetch,
			}),
			(error: unknown) =>
				error instanceof MediaCheckError && (error as { reason: string }).reason === reason,
		);
		assert.equal(cancelled, true, reason);
	}
});

test('VerifiedObjectUrls fails a stalled fetch with reason timeout', async () => {
	const expect = await objectExpectation();
	const urls = blobUrls();
	const cache = new VerifiedObjectUrls({
		cap: 2,
		timeoutMs: 20,
		createUrl: urls.createUrl,
		revokeUrl: urls.revokeUrl,
		// Headers arrive, the body never ends; the abort must reach the body read.
		fetch: (async (_url: string, init?: RequestInit) =>
			new Response(
				new ReadableStream<Uint8Array>({
					start(controller) {
						init?.signal?.addEventListener('abort', () => controller.error(init.signal!.reason));
					},
				}),
				{ headers: { 'content-type': expect.mime } },
			)) as typeof fetch,
	});
	await assert.rejects(
		cache.get('stalled', expect),
		(error: unknown) =>
			error instanceof MediaCheckError && (error as { reason: string }).reason === 'timeout',
	);
	assert.equal(cache.size, 0);
	assert.equal(urls.created.length, 0);
});
