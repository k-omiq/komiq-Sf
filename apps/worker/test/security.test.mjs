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
