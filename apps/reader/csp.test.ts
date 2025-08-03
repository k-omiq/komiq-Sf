import assert from 'node:assert/strict';
import test from 'node:test';
import { readerConnectSources } from './csp.ts';

test('hosted policy pins configured origins and excludes inactive localhost defaults', () => {
	assert.deepEqual(
		readerConnectSources({
			PUBLIC_KOMIKA_BACKEND: 'on',
			PUBLIC_KOMIKA_API: 'https://api.example/graphql',
			PUBLIC_SUWAYOMI_URL: 'http://localhost:4567',
			PUBLIC_KOMIKA_IMG_MODE: 'proxy',
			PUBLIC_KOMIKA_IMG_WORKER: 'https://images.example/proxy',
		}),
		['self', 'https://api.example', 'https://images.example'],
	);
});

test('self-hosted relative APIs and explicit Suwayomi ports remain supported', () => {
	assert.deepEqual(
		readerConnectSources({
			PUBLIC_KOMIKA_BACKEND: 'on',
			PUBLIC_KOMIKA_BACKEND_KIND: 'suwayomi',
			PUBLIC_KOMIKA_API: '/api/graphql',
			PUBLIC_SUWAYOMI_URL: 'http://127.0.0.1:4567/graphql',
		}),
		['self', 'http://127.0.0.1:4567'],
	);
	assert.deepEqual(readerConnectSources({}, true), ['self', 'ws:', 'wss:']);
});

test('invalid destinations cannot inject policy directives or disclose credentials', () => {
	for (const value of [
		'https://api.example; connect-src *',
		'https://user:secret@api.example',
		'data:text/plain,x',
		'//api.example',
	]) {
		assert.throws(
			() => readerConnectSources({ PUBLIC_KOMIKA_BACKEND: 'on', PUBLIC_KOMIKA_API: value }),
			/PUBLIC_KOMIKA_API must/,
		);
	}
});
