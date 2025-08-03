import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { Backend } from '@komika/api';
import type { Series } from '@komika/types';
import { homeLatestAdded } from './home-latest-added.ts';

test('Home Latest Added previews Browse page one without filtering or title dedupe', async () => {
	const items = Array.from({ length: 30 }, (_, i) => ({
		id: `w_${i}`,
		title: i < 2 ? 'Same title, distinct works' : `Work ${i}`,
		createdAt: new Date(Date.UTC(2026, 9, 1, 0, 0, 30 - i)).toISOString(),
	})) as Series[];
	const calls: unknown[][] = [];
	const backend: Pick<Backend, 'search'> = {
		async search(...args) {
			calls.push(args);
			return { items, page: 1, hasNextPage: false, total: items.length };
		},
	};
	assert.deepEqual(await homeLatestAdded(backend), items.slice(0, 10));
	assert.deepEqual(calls, [['', 1, { sort: 'LATEST_ADDED' }]]);
});

test('Failed Latest Added search never substitutes a differently ordered discovery rail', async () => {
	const backend: Pick<Backend, 'search'> = {
		async search() {
			throw new Error('search unavailable');
		},
	};
	await assert.rejects(homeLatestAdded(backend), /search unavailable/);
});

test('An empty Latest Added catalogue stays empty', async () => {
	const backend: Pick<Backend, 'search'> = {
		async search() {
			return { items: [], page: 1, hasNextPage: false, total: 0 };
		},
	};
	assert.deepEqual(await homeLatestAdded(backend), []);
});
