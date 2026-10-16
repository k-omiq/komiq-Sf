import assert from 'node:assert/strict';
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
const imageResponse = () => new Response(image, { headers: { 'Content-Type': 'image/png' } });

async function proxyRequest(src, { env = {}, origin, redirects = [], cached } = {}) {
	const originalFetch = globalThis.fetch;
	const originalCaches = globalThis.caches;
	const fetches = [],
		reads = [],
		writes = [],
		pending = [];
	let limits = 0;
	globalThis.fetch = async (url, options) => {
		assert.equal(options.redirect, 'manual');
		fetches.push(url);
		const location = redirects[fetches.length - 1];
		return location
			? new Response(null, { status: 302, headers: { Location: location } })
			: imageResponse();
	};
	globalThis.caches = {
		default: {
			match: async (key) => {
				reads.push(key.url);
				return cached?.clone();
			},
			put: async (key, response) => {
				writes.push({ key: key.url, body: Buffer.from(await response.arrayBuffer()) });
			},
		},
	};
	try {
		const response = await worker.default.fetch(
			new Request(`https://img.example/img?src=${encodeURIComponent(src)}`, {
				headers: origin ? { Origin: origin } : {},
			}),
			{
				ALLOWED_SOURCE_HOSTS: 'api.komiq.cc,media-sg.example.com',
				...env,
				IMG_RATE_LIMITER: {
					limit: async () => {
						limits += 1;
						return { success: true };
					},
				},
			},
			{ waitUntil: (promise) => pending.push(promise) },
		);
		const body = Buffer.from(await response.arrayBuffer());
		await Promise.all(pending);
		return { response, body, fetches, reads, writes, limits };
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.caches = originalCaches;
	}
}

function assertRefused(result, message, fetches = []) {
	assert.equal(result.response.status, 403);
	assert.equal(result.response.headers.get('Cache-Control'), 'no-store');
	assert.equal(result.body.toString(), message);
	assert.deepEqual(result.fetches, fetches);
	assert.deepEqual(result.writes, []);
}

const mediaPaths = [
	'/media/v1/o/mc_x/abc',
	'/MEDIA/v1/o/x',
	'/%6Dedia/v1/o/x',
	'//media/v1/o/x',
	'/media',
	'/a/../media/v1/ready',
	'/%E0%A4%A',
];

for (const path of mediaPaths) {
	test(`media src is refused before cache or upstream: ${path}`, async () => {
		const result = await proxyRequest(`https://api.komiq.cc${path}`, {
			cached: imageResponse(),
		});
		assertRefused(result, 'media routes are not proxied');
		assert.deepEqual(result.reads, []);
		assert.equal(result.limits, 0);
	});
}

test('media refusal takes precedence over source allowlist and hotlink checks', async () => {
	for (const src of ['https://api.komiq.cc/media', 'https://media-sg.example.com/image.png']) {
		const result = await proxyRequest(src, {
			env: {
				ALLOWED_SOURCE_HOSTS: '',
				ALLOWED_ORIGINS: 'https://reader.example',
				MEDIA_ROUTE_HOSTS: 'media-sg.example.com',
			},
			origin: 'https://unapproved.example',
			cached: imageResponse(),
		});
		assertRefused(result, 'media routes are not proxied');
		assert.deepEqual(result.reads, []);
		assert.equal(result.limits, 0);
	}
});

for (const path of ['/mediax/1.png', '/api/v1/manga/1/chapter/2/page/3']) {
	test(`legacy non-media src is still proxied and cached: ${path}`, async () => {
		const src = `https://api.komiq.cc${path}`;
		const result = await proxyRequest(src);
		assert.equal(result.response.status, 200);
		assert.deepEqual(result.body, image);
		assert.deepEqual(result.fetches, [src]);
		assert.equal(result.writes.length, 1);
		assert.deepEqual(result.writes[0].body, image);
	});
}

for (const raw of ['media-sg.example.com', 'https://media-sg.example.com']) {
	test(`configured media host is refused even when allowlisted: ${raw}`, async () => {
		for (const path of ['/', '/image.png', '/api/v1/manga/1/chapter/2/page/3']) {
			const result = await proxyRequest(`https://media-sg.example.com${path}`, {
				env: { MEDIA_ROUTE_HOSTS: raw },
				cached: imageResponse(),
			});
			assertRefused(result, 'media routes are not proxied');
			assert.deepEqual(result.reads, []);
			assert.equal(result.limits, 0);
		}
	});
}

test('media predicate normalizes hosts and paths while matching only exact hosts', () => {
	for (const raw of [undefined, '']) {
		for (const path of [...mediaPaths, '/%2FMEDIA//v1/o/x']) {
			assert.equal(worker.isMediaRouteTarget(new URL(`https://any.example${path}`), raw), true);
		}
		assert.equal(worker.isMediaRouteTarget(new URL('https://any.example/mediax'), raw), false);
		assert.equal(
			worker.isMediaRouteTarget(new URL('https://any.example/image.png?x=%'), raw),
			false,
		);
	}
	for (const raw of [
		' MEDIA-SG.EXAMPLE.COM. ',
		'https://MEDIA-SG.EXAMPLE.COM.',
		'other.example, , https://MEDIA-SG.EXAMPLE.COM., invalid host',
	]) {
		assert.equal(
			worker.isMediaRouteTarget(new URL('https://media-sg.example.com./any'), raw),
			true,
		);
		assert.equal(
			worker.isMediaRouteTarget(new URL('https://x.media-sg.example.com/any'), raw),
			false,
		);
		assert.equal(
			worker.isMediaRouteTarget(new URL('https://media-sg.example.com.evil/any'), raw),
			false,
		);
	}
	assert.equal(
		worker.isMediaRouteTarget(new URL('https://any.example/image.png'), 'invalid host'),
		false,
	);
});

test('subdomains of a configured media host are still proxied when allowlisted', async () => {
	const src = 'https://x.media-sg.example.com/image.png';
	const result = await proxyRequest(src, { env: { MEDIA_ROUTE_HOSTS: 'media-sg.example.com' } });
	assert.equal(result.response.status, 200);
	assert.deepEqual(result.body, image);
	assert.deepEqual(result.fetches, [src]);
	assert.equal(result.writes.length, 1);
});

test('every normalized media redirect is refused before fetching its target', async () => {
	const src = 'https://api.komiq.cc/image.png';
	for (const path of ['/media/v1/o/x', ...mediaPaths]) {
		const target = path.startsWith('//') ? `https://api.komiq.cc${path}` : path;
		const result = await proxyRequest(src, { redirects: [target] });
		assertRefused(result, 'redirect to a media route refused', [src]);
	}
});

test('redirect to a configured media host is refused before fetching its target', async () => {
	const src = 'https://api.komiq.cc/image.png';
	for (const raw of ['media-sg.example.com', 'https://media-sg.example.com']) {
		const result = await proxyRequest(src, {
			env: { MEDIA_ROUTE_HOSTS: raw },
			redirects: ['https://media-sg.example.com/image.png'],
		});
		assertRefused(result, 'redirect to a media route refused', [src]);
	}
});

test('media guard also runs on the third redirect hop', async () => {
	const src = 'https://api.komiq.cc/image.png';
	for (const media of ['/media/v1/o/x', 'https://media-sg.example.com/image.png']) {
		const result = await proxyRequest(src, {
			env: { MEDIA_ROUTE_HOSTS: 'media-sg.example.com' },
			redirects: ['/hop-1.png', '/hop-2.png', media],
		});
		assertRefused(result, 'redirect to a media route refused', [
			src,
			'https://api.komiq.cc/hop-1.png',
			'https://api.komiq.cc/hop-2.png',
		]);
	}
});

test('redirect media refusal takes precedence over the host allowlist', async () => {
	const src = 'https://api.komiq.cc/image.png';
	for (const target of [
		'https://unapproved.example/media',
		'https://media-sg.example.com/image.png',
	]) {
		const result = await proxyRequest(src, {
			env: { ALLOWED_SOURCE_HOSTS: 'api.komiq.cc', MEDIA_ROUTE_HOSTS: 'media-sg.example.com' },
			redirects: [target],
		});
		assertRefused(result, 'redirect to a media route refused', [src]);
	}
});
