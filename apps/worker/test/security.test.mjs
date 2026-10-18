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

const rasterFixtures = [
	['image/png', await readFile(new URL('../../reader/static/favicon-16.png', import.meta.url))],
	[
		'image/jpeg',
		Buffer.from(
			'/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAADAAIDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwAooorYyP/Z',
			'base64',
		),
	],
	[
		'image/webp',
		Buffer.from(
			'UklGRiwAAABXRUJQVlA4ICAAAABwAQCdASoCAAMAAUAmJZQCdAGgAADxOr++p0HxXwAAAA==',
			'base64',
		),
	],
	[
		'image/gif',
		Buffer.from('R0lGODdhAgADAIEAAHiCjAAAAAAAAAAAACwAAAAAAgADAAAIBgABCBwYEAA7', 'base64'),
	],
	// 2x3 images written by Pillow 11.3 (libavif for AVIF); the server's validator
	// tests use the same bytes.
	[
		'image/avif',
		Buffer.from(
			'AAAAIGZ0eXBhdmlmAAAAAGF2aWZtaWYxbWlhZk1BMUIAAADrbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAAcGljdAAAAAAAAAAAAAAAAAAAAAAOcGl0bQAAAAAAAQAAAB5pbG9jAAAAAEQAAAEAAQAAAAEAAAETAAAAGQAAAChpaW5mAAAAAAABAAAAGmluZmUCAAAAAAEAAGF2MDFDb2xvcgAAAABqaXBycAAAAEtpcGNvAAAAFGlzcGUAAAAAAAAAAgAAAAMAAAAQcGl4aQAAAAADCAgIAAAADGF2MUOBAAwAAAAAE2NvbHJuY2x4AAEADQAGgAAAABdpcG1hAAAAAAAAAAEAAQQBAoMEAAAAIW1kYXQSAAoFGABzAhAyDhgACiiihAAAsBOvz87o',
			'base64',
		),
	],
	[
		'image/bmp',
		Buffer.from(
			'Qk1OAAAAAAAAADYAAAAoAAAAAgAAAAMAAAABABgAAAAAABgAAADEDgAAxA4AAAAAAAAAAAAAWijIWijIAABaKMhaKMgAAFooyFooyAAA',
			'base64',
		),
	],
	[
		'image/vnd.microsoft.icon',
		Buffer.from(
			'AAABAAEAAgMAAAAAIABNAAAAFgAAAIlQTkcNChoKAAAADUlIRFIAAAACAAAAAwgCAAAANohJ1gAAABRJREFUeJxjPKERxcDAwMQABlAKABlAAVCRI4itAAAAAElFTkSuQmCC',
			'base64',
		),
	],
];

async function sourceResponse(originResponse) {
	const originalFetch = globalThis.fetch;
	const originalCaches = globalThis.caches;
	const writes = [],
		pending = [];
	globalThis.fetch = async () => originResponse;
	globalThis.caches = {
		default: {
			match: async () => undefined,
			put: async (key, response) => {
				writes.push({ key: key.url, body: Buffer.from(await response.arrayBuffer()) });
			},
		},
	};
	try {
		const response = await worker.default.fetch(
			new Request('https://img.example/img?src=https%3A%2F%2Fcdn.example%2Fa.jpg'),
			{
				ALLOWED_SOURCE_HOSTS: 'cdn.example',
				IMG_RATE_LIMITER: { limit: async () => ({ success: true }) },
			},
			{
				waitUntil(promise) {
					pending.push(promise);
				},
			},
		);
		const body = Buffer.from(await response.arrayBuffer());
		await Promise.all(pending);
		return { response, body, writes };
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.caches = originalCaches;
	}
}

test('valid raster bodies are published unchanged with a bounded mutable cache lifetime', async () => {
	for (const [type, bytes] of rasterFixtures) {
		const result = await sourceResponse(
			new Response(bytes, {
				headers: { 'Content-Type': type, 'Content-Length': String(bytes.length) },
			}),
		);
		assert.equal(result.response.status, 200, type);
		assert.equal(
			result.response.headers.get('Cache-Control'),
			'public, max-age=300, must-revalidate',
		);
		assert.deepEqual(result.body, bytes);
		assert.equal(result.writes.length, 1);
		assert.ok(result.writes[0].key.endsWith('&validated=1'));
		assert.deepEqual(result.writes[0].body, bytes);
	}
});

test('empty, mislabeled, and truncated image successes are refused before cache publication', async () => {
	const invalid = [
		['image/jpeg', Buffer.alloc(0)],
		['image/jpeg', Buffer.from('<html>challenge</html>')],
		['image/jpeg', rasterFixtures[0][1]],
		...rasterFixtures.map(([type, bytes]) => [type, bytes.subarray(0, bytes.length - 1)]),
	];
	for (const [type, bytes] of invalid) {
		const { response, writes } = await sourceResponse(
			new Response(bytes, { headers: { 'Content-Type': type } }),
		);
		assert.equal(response.status, 502, type);
		assert.equal(response.headers.get('Cache-Control'), 'no-store');
		assert.equal(writes.length, 0);
	}
});

test('declared lengths never bypass actual byte bounds or incomplete-stream rejection', async () => {
	const [type, bytes] = rasterFixtures[0];
	for (const body of [bytes, new Uint8Array(32 * 1024 * 1024 + 1)]) {
		const { response, writes } = await sourceResponse(
			new Response(body, { headers: { 'Content-Type': type, 'Content-Length': '1' } }),
		);
		assert.equal(response.status, 502);
		assert.equal(writes.length, 0);
	}
	const body = new ReadableStream({
		start(controller) {
			controller.enqueue(bytes.subarray(0, 12));
			controller.error(new Error('connection lost'));
		},
	});
	const { response, writes } = await sourceResponse(
		new Response(body, { headers: { 'Content-Type': type } }),
	);
	assert.equal(response.status, 502);
	assert.equal(writes.length, 0);
});

test('each raster fixture is refused under another raster type', () => {
	for (const [type, bytes] of rasterFixtures) {
		for (const [other] of rasterFixtures) {
			if (other !== type)
				assert.equal(worker.validateImageBody(bytes, other), false, `${type} as ${other}`);
		}
	}
});

test('edge lookups use only the validated key, so legacy unvalidated entries are never served', async () => {
	const originalFetch = globalThis.fetch;
	const originalCaches = globalThis.caches;
	const [type, bytes] = rasterFixtures[1];
	const legacy = 'https://img.example/img?src=https%3A%2F%2Fcdn.example%2Fa.jpg';
	const store = new Map([
		[legacy, new Response('<html>old</html>', { headers: { 'Content-Type': 'image/jpeg' } })],
	]);
	const matched = [],
		stored = [];
	let upstreamCalls = 0;
	globalThis.caches = {
		default: {
			match: async (key) => {
				matched.push(key.url);
				return store.get(key.url)?.clone();
			},
			put: async (key, response) => {
				stored.push(key.url);
				store.set(key.url, response);
			},
		},
	};
	globalThis.fetch = async () => {
		upstreamCalls += 1;
		return new Response(bytes, { headers: { 'Content-Type': type } });
	};
	const env = {
		ALLOWED_SOURCE_HOSTS: 'cdn.example',
		IMG_RATE_LIMITER: { limit: async () => ({ success: true }) },
	};
	try {
		for (const noise of ['&cache_policy=2', '&cache_policy=2&utm=x']) {
			const pending = [];
			const response = await worker.default.fetch(new Request(`${legacy}${noise}`), env, {
				waitUntil(promise) {
					pending.push(promise);
				},
			});
			assert.equal(response.status, 200);
			assert.deepEqual(Buffer.from(await response.arrayBuffer()), bytes);
			await Promise.all(pending);
		}
		const validated = `${legacy}&validated=1`;
		assert.deepEqual(matched, [validated, validated]);
		assert.deepEqual(stored, [validated]);
		assert.equal(upstreamCalls, 1, 'second request is served from the validated entry');
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.caches = originalCaches;
	}
});

test('proxy rejects active and unknown image media types', () => {
	for (const type of [
		'image/svg+xml',
		'image/svg',
		'image/svg+xml;charset=utf-8',
		'image/unknown',
		'text/html',
	]) {
		assert.equal(worker.isImageContentType(type), false, type);
	}
	for (const type of [
		'image/png',
		'IMAGE/JPEG; charset=binary',
		'image/webp',
		'image/avif',
		'image/gif',
	]) {
		assert.equal(worker.isImageContentType(type), true, type);
	}
});

test('image/jpg is read as image/jpeg for JPEG bodies only', async () => {
	const jpeg = rasterFixtures.find(([type]) => type === 'image/jpeg')[1];
	const png = rasterFixtures.find(([type]) => type === 'image/png')[1];
	assert.equal(worker.isImageContentType('image/jpg'), true);
	assert.equal(worker.imageMediaType('Image/JPG; charset=binary'), 'image/jpeg');
	assert.equal(worker.validateImageBody(jpeg, 'image/jpg'), true);
	assert.equal(worker.validateImageBody(png, 'image/jpg'), false);

	const result = await sourceResponse(
		new Response(jpeg, { headers: { 'Content-Type': 'image/jpg' } }),
	);
	assert.equal(result.response.status, 200);
	assert.equal(result.response.headers.get('Content-Type'), 'image/jpeg');
	assert.deepEqual(result.body, jpeg);
	assert.equal(result.writes.length, 1);
});

test('upstream URL guard rejects credentials and service ports on every hop', () => {
	for (const url of [
		'https://alice:secret@cdn.example/a.jpg',
		'https://cdn.example:8443/a.jpg',
		'http://cdn.example:22/',
		'file:///tmp/a',
	]) {
		assert.equal(worker.safeUpstreamUrl(new URL(url)), false, url);
	}
	assert.equal(worker.safeUpstreamUrl(new URL('https://cdn.example/a.jpg')), true);
	assert.equal(worker.safeUpstreamUrl(new URL('https://cdn.example:443/a.jpg')), true);
});

test('invalid source is refused before contacting cache or upstream', async () => {
	const response = await worker.default.fetch(
		new Request('https://img.example/img?src=https%3A%2F%2Fcdn.example%3A8443%2Fa.jpg'),
		{ ALLOWED_SOURCE_HOSTS: 'cdn.example' },
		{},
	);
	assert.equal(response.status, 400);
});

test('redirect targets are refused before a second fetch', async () => {
	const originalFetch = globalThis.fetch;
	const originalCaches = globalThis.caches;
	let calls = 0;
	globalThis.caches = { default: { match: async () => undefined } };
	globalThis.fetch = async () => {
		calls += 1;
		return new Response(null, {
			status: 302,
			headers: { Location: 'https://cdn.example:8443/private' },
		});
	};
	try {
		const response = await worker.default.fetch(
			new Request('https://img.example/img?src=https%3A%2F%2Fcdn.example%2Fa.jpg'),
			{
				ALLOWED_SOURCE_HOSTS: 'cdn.example',
				IMG_RATE_LIMITER: { limit: async () => ({ success: true }) },
			},
			{},
		);
		assert.equal(response.status, 400);
		assert.equal(calls, 1);
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.caches = originalCaches;
	}
});

test('manual cover recovery bypasses stale cache and still obeys the miss rate limit', async () => {
	const originalFetch = globalThis.fetch;
	const originalCaches = globalThis.caches;
	let cacheReads = 0;
	let upstreamCalls = 0;
	let limits = 0;
	globalThis.caches = {
		default: {
			match: async () => {
				cacheReads += 1;
				return new Response('stale', { headers: { 'Content-Type': 'image/jpeg' } });
			},
		},
	};
	globalThis.fetch = async () => {
		upstreamCalls += 1;
		throw new Error('unexpected upstream request');
	};
	const env = {
		ALLOWED_SOURCE_HOSTS: 'cdn.example',
		IMG_RATE_LIMITER: {
			limit: async () => {
				limits += 1;
				return { success: false };
			},
		},
	};
	try {
		const url = 'https://img.example/img?src=https%3A%2F%2Fcdn.example%2Fa.jpg';
		const cached = await worker.default.fetch(new Request(url), env, {});
		assert.equal(await cached.text(), 'stale');
		const retried = await worker.default.fetch(
			new Request(`${url}&retry=1&_coverRetry=next`),
			env,
			{},
		);
		assert.equal(retried.status, 429);
		assert.equal(cacheReads, 1);
		assert.equal(limits, 1);
		assert.equal(upstreamCalls, 0);
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.caches = originalCaches;
	}
});
