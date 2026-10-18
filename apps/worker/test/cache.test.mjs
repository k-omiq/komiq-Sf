import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import ts from 'typescript';

const source = await readFile(new URL('../src/index.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
	compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 },
});
const worker = await import(
	`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

const image = Buffer.from(
	'iVBORw0KGgoAAAANSUhEUgAAAAIAAAADCAIAAAA2iEnWAAAAFElEQVR4nGM8oRHFwMDAxAAGUAoAGUABUJEjiK0AAAAASUVORK5CYII=',
	'base64',
);
const sha = createHash('sha256').update(image).digest('hex');
const chapter = '7c07a7fecb2fe3868aa22aae2edf0e5a';
const file = `1-${sha}.png`;
const pageUrl = (host, folder = 'data') => `https://${host}/${folder}/${chapter}/${file}`;
const canonicalKey = (folder = 'data') =>
	`https://img.komiq.cc/img/md/${folder}/${chapter}/${file}`;

const STORED = 'public, max-age=2592000';
const BROWSER = 'public, max-age=604800, immutable';
const MUTABLE = 'public, max-age=300, must-revalidate';
const ENV_HOSTS = 'uploads.mangadex.org,mangadex.network,cdn.example';

/**
 * Run requests through the Worker with one shared in-memory edge cache.
 * `upstream(url, options)` answers upstream fetches (default: the PNG).
 */
async function session(
	requests,
	{ upstream = async () => imageResponse(), limit = async () => ({ success: true }) } = {},
) {
	const originalFetch = globalThis.fetch;
	const originalCaches = globalThis.caches;
	const store = new Map();
	const log = { fetches: [], reads: [], writes: [], limits: 0, responses: [] };
	globalThis.fetch = async (url, options) => {
		log.fetches.push({ url, options });
		return upstream(url, options);
	};
	globalThis.caches = {
		default: {
			match: async (key) => {
				log.reads.push(key.url);
				return store.get(key.url)?.clone();
			},
			put: async (key, response) => {
				log.writes.push({ key: key.url, cacheControl: response.headers.get('Cache-Control') });
				store.set(key.url, response);
			},
		},
	};
	const env = {
		ALLOWED_SOURCE_HOSTS: ENV_HOSTS,
		IMG_RATE_LIMITER: {
			limit: async (input) => {
				log.limits += 1;
				return limit(input);
			},
		},
	};
	try {
		for (const { src, method = 'GET', query = '' } of requests) {
			const pending = [];
			const response = await worker.default.fetch(
				new Request(`https://img.komiq.cc/img?src=${encodeURIComponent(src)}${query}`, { method }),
				env,
				{ waitUntil: (promise) => pending.push(promise) },
			);
			const body = Buffer.from(await response.arrayBuffer());
			await Promise.all(pending);
			log.responses.push({ response, body });
		}
		return log;
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.caches = originalCaches;
	}
}

const imageResponse = () => new Response(image, { headers: { 'Content-Type': 'image/png' } });

test('two MangaDex@Home hosts with the same file share one content key', async () => {
	const log = await session([
		{ src: pageUrl('cmdxd98sb0x3yprd.mangadex.network') },
		{ src: pageUrl('a1b2c3d4e5f6g7h8.mangadex.network') },
	]);
	assert.deepEqual(log.reads, [canonicalKey(), canonicalKey()]);
	assert.deepEqual(
		log.writes.map((write) => write.key),
		[canonicalKey()],
	);
	assert.equal(log.fetches.length, 1, 'the second host is served from the shared entry');
	for (const { response, body } of log.responses) {
		assert.equal(response.status, 200);
		assert.deepEqual(body, image);
	}
});

test('a hit costs no rate-limiter call', async () => {
	const log = await session([
		{ src: pageUrl('node-a.mangadex.network') },
		{ src: pageUrl('node-b.mangadex.network') },
		{ src: 'https://cdn.example/a.png' },
		{ src: 'https://cdn.example/a.png' },
	]);
	assert.equal(log.fetches.length, 2);
	assert.equal(log.limits, 2, 'only the two misses ask the limiter');
});

test('token path segments, uploads.mangadex.org and data-saver map to content keys', () => {
	const cases = [
		[`https://node.mangadex.network/sometoken/data/${chapter}/${file}`, 'data'],
		[pageUrl('uploads.mangadex.org'), 'data'],
		[pageUrl('node.mangadex.network', 'data-saver'), 'data-saver'],
	];
	for (const [url, folder] of cases) {
		assert.deepEqual(
			worker.mangadexPage(new URL(url)),
			{ folder, chapter, file, sha256: sha },
			url,
		);
	}
	for (const url of [
		'https://cdn.example/data/' + chapter + '/' + file,
		'https://evilmangadex.network/data/' + chapter + '/' + file,
		`${pageUrl('node.mangadex.network')}?x=1`,
		`https://node.mangadex.network/data/${chapter}/1-short.png`,
		`https://uploads.mangadex.org/covers/${chapter}/cover.jpg`,
	]) {
		assert.equal(worker.mangadexPage(new URL(url)), null, url);
	}
});

test('a non-MangaDex upstream keeps its own URL key and short lifetime', async () => {
	const src = 'https://cdn.example/a.png';
	const log = await session([{ src: `${src}#frag` }, { src }]);
	const key = `https://img.komiq.cc/img?src=${encodeURIComponent(src)}&validated=1`;
	assert.deepEqual(log.reads, [key, key], 'the fragment is not part of the key');
	assert.deepEqual(log.writes, [{ key, cacheControl: MUTABLE }]);
	assert.equal(log.fetches[0].options.cf, undefined, 'no long zone-cache TTL for mutable URLs');
	for (const { response } of log.responses) {
		assert.equal(response.headers.get('Cache-Control'), MUTABLE);
	}
});

test('MangaDex pages are stored for 30 days, sent to browsers for 7 days, fetched through the tiers', async () => {
	const log = await session([
		{ src: pageUrl('node-a.mangadex.network') },
		{ src: pageUrl('node-a.mangadex.network') },
	]);
	assert.deepEqual(log.writes, [{ key: canonicalKey(), cacheControl: STORED }]);
	const [miss, hit] = log.responses;
	assert.equal(miss.response.headers.get('Cache-Control'), BROWSER);
	assert.equal(hit.response.headers.get('Cache-Control'), BROWSER);
	assert.equal(hit.response.headers.get('Access-Control-Allow-Origin'), '*');
	assert.deepEqual(log.fetches[0].options.cf, {
		cacheEverything: true,
		cacheTtlByStatus: { '200-299': 2592000, '300-599': -1 },
	});
});

test('a page whose bytes do not match the hash in its name is refused and not stored', async () => {
	const other = Buffer.from(image);
	other[other.length - 1] ^= 1; // change the IEND CRC: still a framed PNG
	const log = await session([{ src: pageUrl('bad.mangadex.network') }], {
		upstream: async () => new Response(other, { headers: { 'Content-Type': 'image/png' } }),
	});
	const [{ response, body }] = log.responses;
	assert.equal(response.status, 502);
	assert.match(body.toString(), /content hash/);
	assert.equal(response.headers.get('Cache-Control'), 'no-store');
	assert.equal(log.writes.length, 0);
});

test('upstream errors are never stored', async () => {
	const log = await session([{ src: pageUrl('node-a.mangadex.network') }], {
		upstream: async () => new Response('gone', { status: 404 }),
	});
	assert.equal(log.responses[0].response.status, 502);
	assert.equal(log.responses[0].response.headers.get('Cache-Control'), 'no-store');
	assert.equal(log.writes.length, 0);
});

test('a rate-limited miss answers 429 with Retry-After: 10', async () => {
	const log = await session([{ src: pageUrl('node-a.mangadex.network') }], {
		limit: async () => ({ success: false }),
	});
	const [{ response }] = log.responses;
	assert.equal(response.status, 429);
	assert.equal(response.headers.get('Retry-After'), '10');
	assert.equal(response.headers.get('Cache-Control'), 'no-store');
	assert.equal(log.fetches.length, 0);
});

test('one 25 s deadline covers every redirect hop', async () => {
	const originalTimeout = AbortSignal.timeout;
	const deadlines = [];
	let controller;
	AbortSignal.timeout = (ms) => {
		deadlines.push(ms);
		controller = new AbortController();
		return controller.signal;
	};
	try {
		const log = await session([{ src: 'https://cdn.example/a.png' }], {
			upstream: async (url, options) => {
				if (url === 'https://cdn.example/a.png') {
					return new Response(null, {
						status: 302,
						headers: { Location: 'https://cdn.example/b.png' },
					});
				}
				// The second hop hangs until the shared deadline fires.
				controller.abort(new DOMException('deadline', 'TimeoutError'));
				return new Promise((_, reject) => {
					if (options.signal.aborted) reject(options.signal.reason);
					options.signal.addEventListener('abort', () => reject(options.signal.reason));
				});
			},
		});
		assert.deepEqual(deadlines, [25_000], 'one deadline, not one per hop');
		assert.equal(log.fetches.length, 2);
		assert.equal(log.fetches[0].options.signal, log.fetches[1].options.signal);
		const [{ response }] = log.responses;
		assert.equal(response.status, 504);
		assert.equal(response.headers.get('Cache-Control'), 'no-store');
		assert.equal(log.writes.length, 0);
	} finally {
		AbortSignal.timeout = originalTimeout;
	}
});

test('a HEAD miss answers headers only and stores nothing; a later HEAD hit is headers only', async () => {
	const log = await session([
		{ src: pageUrl('node-a.mangadex.network'), method: 'HEAD' },
		{ src: pageUrl('node-a.mangadex.network') },
		{ src: pageUrl('node-a.mangadex.network'), method: 'HEAD' },
	]);
	const [headMiss, get, headHit] = log.responses;
	assert.equal(headMiss.response.status, 200);
	assert.equal(headMiss.body.length, 0);
	assert.equal(headMiss.response.headers.get('Content-Type'), 'image/png');
	assert.equal(headMiss.response.headers.get('Cache-Control'), BROWSER);
	assert.deepEqual(get.body, image);
	assert.equal(headHit.body.length, 0);
	assert.deepEqual(
		log.writes.map((write) => write.key),
		[canonicalKey()],
		'only the GET stored the page',
	);
	assert.equal(log.fetches.length, 2, 'the HEAD miss did not fill the cache');
});
