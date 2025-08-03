import { expect, test } from '@playwright/test';
import { GraphQLBackend } from '../../packages/api/src/graphql-backend';
import type { Series } from '../../packages/types/src';

test('older API rejects metadata once and detail queries retain their other fields', async () => {
	const documents: string[] = [];
	const fixture = { id: '42', title: 'Older API title' } as Series;
	const backend = new GraphQLBackend({
		endpoint: 'https://api.example/graphql',
		token: 'test-token',
		fetch: async (_input, init) => {
			const query = JSON.parse(String(init?.body)).query as string;
			documents.push(query);
			return Response.json(
				documents.length === 1
					? { errors: [{ message: 'Unknown field "metadata" on type "Series".' }] }
					: { data: { series: fixture, canonicalSeries: fixture } },
			);
		},
	});
	const series = await backend.series('42');
	expect(series.title).toBe(fixture.title);
	expect(documents).toHaveLength(2);
	expect(documents[0]).toContain('metadata {');
	expect(documents[1]).not.toContain('metadata {');
	expect(documents[1]).not.toContain('trackers {');
	expect(documents[1]).toContain('...SeriesFields');
	expect(documents[1]).toContain('workId');
	expect(await backend.canonicalSeries('w_old')).toMatchObject(fixture);
	expect(documents[2]).not.toContain('metadata {');
	expect(documents[2]).toContain('localizedDescriptions');
});
