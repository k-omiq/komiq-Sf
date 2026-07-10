import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fetchCoverSource } from './cover-source.ts';

test('source IDs use numeric metadata and canonical IDs use the work bundle', async () => {
	const calls: string[] = [];
	const backend = {
		series: async (id: string) => {
			calls.push(id);
			return { coverUrl: 'numeric.jpg' };
		},
		seriesPage: async (id: string) => {
			calls.push(id);
			return { series: { coverUrl: 'canonical.jpg' }, sources: [] };
		},
	} as unknown as Parameters<typeof fetchCoverSource>[0];
	assert.equal(await fetchCoverSource(backend, '123'), 'numeric.jpg');
	assert.equal(await fetchCoverSource(backend, 'w_work'), 'canonical.jpg');
	assert.deepEqual(calls, ['123', 'w_work']);
});

test('federation-only works recover a cover from their source metadata', async () => {
	const backend = {
		series: async () => {
			throw new Error('work IDs are not numeric');
		},
		seriesPage: async () => ({
			series: null,
			sources: [{ series: null }, { series: { coverUrl: 'source.jpg' } }],
		}),
	} as unknown as Parameters<typeof fetchCoverSource>[0];
	assert.equal(await fetchCoverSource(backend, 'w_federated'), 'source.jpg');
});

test('older adapters can recover federation-only covers without a bundle', async () => {
	const backend = {
		series: async (id: string) => ({ coverUrl: `${id}.jpg` }),
		canonicalSeries: async () => {
			throw new Error('No such work');
		},
		workSources: async () => [{ sourceType: 'suwayomi', sourceKey: '123' }],
	} as unknown as Parameters<typeof fetchCoverSource>[0];
	assert.equal(await fetchCoverSource(backend, 'w_federated'), '123.jpg');
});

test('a gated or absent bundle never bypasses the work visibility check', async () => {
	const backend = {
		series: async () => {
			throw new Error('must not query');
		},
		seriesPage: async () => null,
		workSources: async () => {
			throw new Error('must not query');
		},
	} as unknown as Parameters<typeof fetchCoverSource>[0];
	assert.equal(await fetchCoverSource(backend, 'w_missing'), '');
});
