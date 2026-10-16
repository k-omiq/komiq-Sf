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

test('hosted media route hosts join connect-src for probes; bad entries fail the build', () => {
	assert.deepEqual(
		readerConnectSources({
			PUBLIC_KOMIKA_BACKEND: 'on',
			PUBLIC_KOMIKA_API: 'https://api.example/graphql',
			PUBLIC_KOMIKA_MEDIA_HOSTS: 'https://api.example, https://media-sg.example:8443',
		}),
		['self', 'https://api.example', 'https://media-sg.example:8443'],
	);
	// Media routes need the komiq backend; a direct Suwayomi build never probes.
	assert.deepEqual(
		readerConnectSources({
			PUBLIC_KOMIKA_BACKEND: 'on',
			PUBLIC_KOMIKA_BACKEND_KIND: 'suwayomi',
			PUBLIC_SUWAYOMI_URL: 'http://127.0.0.1:4567',
			PUBLIC_KOMIKA_MEDIA_HOSTS: 'https://media-sg.example',
		}),
		['self', 'http://127.0.0.1:4567'],
	);
	for (const value of [
		'https://media.example/path',
		'https://media.example; connect-src *',
		'https://user:pw@media.example',
		'media.example',
		'https://media.example?x=1',
	]) {
		assert.throws(
			() =>
				readerConnectSources({
					PUBLIC_KOMIKA_BACKEND: 'on',
					PUBLIC_KOMIKA_MEDIA_HOSTS: value,
				}),
			/PUBLIC_KOMIKA_MEDIA_HOSTS/,
		);
	}
});
