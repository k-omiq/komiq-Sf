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
const { ChapterMediaRecovery, MediaRouter, mediaOrigin, parseMediaHosts, trustedMediaRoutes } =
	await import(api + 'media-routes.ts');
const { WebImageProvider } = await import(api + 'image-provider.ts');
const { GraphQLBackend } = await import(api + 'graphql-backend.ts');

const API = 'https://api.example';
const SG = 'https://media-sg.example';
const EU = 'https://media-eu.example';
const WORKER = 'https://img.example';
const HASH = 'a'.repeat(64);
const PATH = `/media/v1/o/mc_0123/${HASH}`;
const page = (mediaPath: string | null = PATH, i = 0) => ({
	index: i,
	sourceUrl: `${API}/media/v1/c/7/${i}`,
	mediaPath,
});
const serverRoutes = [
	{ id: 'owner', base: '', kind: 'OWNER', region: null },
	{ id: 'sg', base: SG, kind: 'REGIONAL', region: 'sg' },
	{ id: 'eu', base: EU, kind: 'REGIONAL', region: 'eu' },
	{ id: 'evil', base: 'https://evil.example', kind: 'REGIONAL', region: 'x' },
];

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

test('an exact object never reaches the image provider and goes out without credentials', () =>
	withClock(async () => {
		const net = fakeNet({ [API]: 80, [SG]: 20 });
		const router = new MediaRouter({
			routes: trustedMediaRoutes(serverRoutes, [SG], API),
			fetch: net.fetch,
		});
		const media = router.pageMedia(page());
		assert.deepEqual(media, { kind: 'media', routeId: 'owner', path: PATH });
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
			assert.equal(init?.headers, undefined, 'no Authorization or any other header');
			assert.equal(init?.method, 'GET');
		}
	}));

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
				const m = { kind: 'media', routeId: router.current().id, path: moved };
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
