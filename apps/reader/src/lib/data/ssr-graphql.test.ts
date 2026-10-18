import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ssrGraphqlFetch } from './ssr-graphql.ts';

const ENDPOINT = 'https://api.example/graphql';

/** A fetch that answers each URL from `answers` and records what was asked. */
function upstream(answers: Record<string, () => Response>) {
	const asked: string[] = [];
	const fetch = async (input: RequestInfo | URL) => {
		const request = input instanceof Request ? input : new Request(input);
		asked.push(`${request.method} ${request.url}`);
		const url = new URL(request.url);
		return (answers[url.pathname] ?? (() => new Response('missing', { status: 404 })))();
	};
	return { asked, fetch: fetch as typeof globalThis.fetch };
}

const json = () => Response.json({ data: { tags: [] } });

test('a render reads the API under /graphql/ssr, query string kept', async () => {
	const { asked, fetch } = upstream({ '/graphql/ssr': json });
	const send = ssrGraphqlFetch(ENDPOINT);
	const res = await send(new Request(`${ENDPOINT}?id=abc&variables=%7B%7D`), fetch);
	assert.deepEqual(await res.json(), { data: { tags: [] } });
	assert.deepEqual(asked, ['GET https://api.example/graphql/ssr?id=abc&variables=%7B%7D']);
});

test('POSTs, other paths and other hosts go where they were sent', async () => {
	const { asked, fetch } = upstream({ '/graphql': json, '/covers/a.webp': json });
	const send = ssrGraphqlFetch(ENDPOINT);
	await send(new Request(ENDPOINT, { method: 'POST', body: '{}' }), fetch);
	await send(new Request('https://api.example/covers/a.webp'), fetch);
	await send(new Request('https://other.example/graphql'), fetch);
	assert.deepEqual(asked, [
		'POST https://api.example/graphql',
		'GET https://api.example/covers/a.webp',
		'GET https://other.example/graphql',
	]);
});

test('an answer from the alias is kept, refusals and failures included', async () => {
	for (const status of [400, 500, 503]) {
		const { asked, fetch } = upstream({
			'/graphql/ssr': () => Response.json({ errors: [] }, { status }),
		});
		const res = await ssrGraphqlFetch(ENDPOINT)(new Request(`${ENDPOINT}?id=a`), fetch);
		assert.equal(res.status, status);
		assert.equal(asked.length, 1);
	}
});

for (const [name, alias] of [
	['404', () => new Response('not found', { status: 404 })],
	[
		'web app HTML',
		() => new Response('<!doctype html>', { headers: { 'content-type': 'text/html' } }),
	],
] as const) {
	test(`a server without the alias (${name}) is read at /graphql, and then directly`, async () => {
		const { asked, fetch } = upstream({ '/graphql/ssr': alias, '/graphql': json });
		const send = ssrGraphqlFetch(ENDPOINT);
		assert.deepEqual(await (await send(new Request(`${ENDPOINT}?id=a`), fetch)).json(), {
			data: { tags: [] },
		});
		await send(new Request(`${ENDPOINT}?id=b`), fetch);
		assert.deepEqual(asked, [
			'GET https://api.example/graphql/ssr?id=a',
			'GET https://api.example/graphql?id=a',
			'GET https://api.example/graphql?id=b',
		]);
	});
}
