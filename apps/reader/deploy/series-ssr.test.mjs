import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import test from 'node:test';

// Run after the web build. Drives the generated Worker, including the adapter's
// `caches.default` layer, against a fake API: no request leaves the process.
registerHooks({
	resolve(specifier, context, next) {
		if (specifier === 'cloudflare:workers') {
			return { shortCircuit: true, url: 'data:text/javascript,export const env = {};' };
		}
		return next(specifier, context);
	},
});

/** What the Worker stored in `caches.default`, by URL. */
const edge = new Map();
globalThis.caches = {
	default: {
		match: async (request) =>
			edge.get(typeof request === 'string' ? request : request.url)?.clone(),
		put: async (request, response) => void edge.set(request.url, response),
	},
};

const API = 'https://api.komiq.cc/graphql';
const series = {
	id: 'w_found',
	title: 'Found Series',
	altTitles: [],
	author: 'An Author',
	artist: null,
	description: 'A series the fake API knows.',
	genres: ['Action'],
	type: 'Manga',
	status: 'ONGOING',
	coverUrl: 'https://api.komiq.cc/covers/w_found.webp?v=1',
	sourceId: 'mangadex',
	chapterCount: 0,
	isMarked: false,
	isNsfw: false,
	rating: { average: 0, count: 0, distribution: [] },
	mangadexRating: null,
	scan: null,
	createdAt: '2026-01-01T00:00:00Z',
	updatedAt: '2026-01-01T00:00:00Z',
	latestChapterAt: null,
	detectedAt: null,
	metadata: null,
	libraryStatus: null,
	isFavorite: false,
	views: { total: 0, last7d: 0, last24h: 0 },
	localizedDescriptions: [],
	credits: [],
	covers: [],
};
/** The documents the API holds by id (`apps/server/src/persisted.rs`). */
const persisted = JSON.parse(
	readFileSync(new URL('../../server/persisted-operations.json', import.meta.url), 'utf8'),
);
/** How each API request arrived: its path, and `id` when it named a persisted document. */
const asked = [];
globalThis.fetch = async (input, init) => {
	const request = new Request(input, init);
	assert.ok(request.url.startsWith(API), `unexpected fetch ${request.url}`);
	const url = new URL(request.url);
	asked.push(`${request.method} ${url.pathname}${url.searchParams.has('id') ? ' id' : ''}`);
	const body =
		request.method === 'POST' ? await request.json() : Object.fromEntries(url.searchParams);
	const query = body.query ?? persisted[body.id];
	const operation = query.match(/\b(?:query|mutation)\s+(\w+)\s*[({]/)?.[1];
	const headers = { 'access-control-allow-origin': '*' };
	if (operation !== 'SeriesPage') return Response.json({ data: {} }, { headers });
	const { workId } = JSON.parse(body.variables);
	if (workId === 'w_down') return new Response('upstream down', { status: 502, headers });
	const seriesPage =
		workId === 'w_found'
			? { workId, viewerProgress: null, series, spineChapters: [], sources: [], aggregated: [] }
			: null;
	return Response.json({ data: { seriesPage } }, { headers });
};

const { default: worker } = await import('../.svelte-kit/cloudflare/_worker.js');

async function get(path) {
	const pending = [];
	const ctx = { waitUntil: (promise) => pending.push(promise), passThroughOnException() {} };
	const env = { ASSETS: { fetch: () => assert.fail(`asset read for ${path}`) } };
	const response = await worker.fetch(new Request(`https://komiq.cc${path}`), env, ctx);
	await Promise.all(pending);
	return response;
}

test('a found series is edge-cached for a minute', async () => {
	asked.length = 0;
	const response = await get('/series/w_found');
	assert.equal(response.status, 200);
	// The render reads the API under its own path, by persisted id.
	assert.ok(asked.length > 0);
	assert.ok(
		asked.every((how) => how === 'GET /graphql/ssr id'),
		asked.join(', '),
	);
	assert.equal(response.headers.get('cache-control'), 'public, max-age=0, s-maxage=60');
	assert.match(await response.text(), /Found Series/);
	assert.ok(edge.has('https://komiq.cc/series/w_found'));
});

test('an unknown series is a 404 that is never cached', async () => {
	const response = await get('/series/w_missing');
	assert.equal(response.status, 404);
	assert.equal(response.headers.get('cache-control'), 'no-store');
	assert.equal(edge.has('https://komiq.cc/series/w_missing'), false);
});

test('a failed load keeps the retry page and is never cached', async () => {
	const response = await get('/series/w_down');
	assert.equal(response.status, 200);
	assert.equal(response.headers.get('cache-control'), 'no-store');
	assert.match(await response.text(), /Couldn’t load this series/);
	assert.equal(edge.has('https://komiq.cc/series/w_down'), false);
});

test('query strings the page does not use redirect to one canonical URL', async () => {
	for (const [query, canonical] of [
		['?utm_source=x', ''],
		['?utm_source=x&order=oldest', '?order=oldest'],
		['?order=newest', ''],
		['?order=OLDEST', ''],
		['?cpage=1', ''],
		['?cpage=abc', ''],
		['?cpage=2.5', '?cpage=2'],
		['?cpage=2&order=oldest', '?order=oldest&cpage=2'],
	]) {
		const response = await get(`/series/w_found${query}`);
		assert.equal(response.status, 301, query);
		assert.equal(response.headers.get('location'), `/series/w_found${canonical}`, query);
		assert.equal(response.headers.get('cache-control'), 'public, max-age=3600, s-maxage=60');
		assert.equal(response.headers.get('x-content-type-options'), 'nosniff');
	}
});

test('parameters the page reads render without a redirect', async () => {
	for (const query of ['?order=oldest', '?cpage=3', '?order=oldest&cpage=2']) {
		const response = await get(`/series/w_found${query}`);
		assert.equal(response.status, 200, query);
		assert.equal(response.headers.get('cache-control'), 'public, max-age=0, s-maxage=60');
	}
});
