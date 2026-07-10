import assert from 'node:assert/strict';
import { test } from 'node:test';
import { sortLibraryRows } from './library-order.ts';

const rows = [
	{
		title: 'Old release found today',
		latestFoundAt: '2026-10-03T18:00:00Z',
		latestChapterAt: '2020-01-01T00:00:00Z',
		lastReadAt: '',
		read: 0,
		total: 20,
	},
	{
		title: 'Recent release found yesterday',
		latestFoundAt: '2026-10-02T18:00:00Z',
		latestChapterAt: '2026-10-02T18:00:00Z',
		lastReadAt: '2026-10-03T20:00:00Z',
		read: 5,
		total: 20,
	},
	{
		title: 'Unknown',
		latestFoundAt: '',
		latestChapterAt: '2026-10-03T21:00:00Z',
		lastReadAt: '',
		read: 0,
		total: 0,
	},
];

test('Latest update uses found time even when release order disagrees', () => {
	assert.deepEqual(
		sortLibraryRows(rows, 'updated', 'desc').map((row) => row.title),
		rows.map((row) => row.title),
	);
	assert.deepEqual(
		sortLibraryRows(rows, 'updated', 'asc').map((row) => row.title),
		[rows[1].title, rows[0].title, rows[2].title],
	);
});

test('Unknown and invalid dates remain last in either direction', () => {
	const input = [rows[2], { ...rows[2], title: 'Bad date', latestFoundAt: 'invalid' }, rows[0]];
	for (const direction of ['asc', 'desc'] as const) {
		assert.deepEqual(
			sortLibraryRows(input, 'updated', direction).map((row) => row.title),
			[rows[0].title, 'Bad date', 'Unknown'],
		);
	}
});

test('Recent reads and addition ordering retain their separate meanings', () => {
	assert.equal(sortLibraryRows(rows, 'read', 'desc')[0].title, rows[1].title);
	assert.deepEqual(sortLibraryRows(rows, 'added', 'asc'), [...rows].reverse());
	assert.deepEqual(
		rows.map((row) => row.title),
		['Old release found today', 'Recent release found yesterday', 'Unknown'],
	);
});
