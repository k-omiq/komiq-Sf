import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';

registerHooks({
	load(url, context, next) {
		if (url.endsWith('/packages/api/src/graphql-backend.ts')) {
			return {
				format: 'module',
				source: ts.transpileModule(readFileSync(fileURLToPath(url), 'utf8'), {
					compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
				}).outputText,
				shortCircuit: true,
			};
		}
		return next(url, context);
	},
});
const { GraphQLBackend } = await import('../../../../../packages/api/src/graphql-backend.ts');

let endpointNumber = 0;
const endpoint = () => 'https://fixture-' + ++endpointNumber + '.test/graphql';
const unknown = (field: string) =>
	Response.json({ errors: [{ message: 'Unknown field "' + field + '"' }] });
const tags = [{ id: 'fixture', name: 'Fixture', group: 'genre' }];

test('a negative root capability cannot poison another endpoint', async () => {
	const old = new GraphQLBackend({ endpoint: endpoint(), fetch: async () => unknown('tags') });
	let calls = 0;
	const modern = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return Response.json({ data: { tags } });
		},
	});
	assert.deepEqual(await old.tags(), []);
	assert.deepEqual(await modern.tags(), tags);
	assert.equal(calls, 1);
});

test('withFetch views share discoveries for the same endpoint', async () => {
	let calls = 0;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return unknown('tags');
		},
	});
	assert.deepEqual(await backend.tags(), []);
	const view = backend.withFetch(async () => {
		assert.fail('a fresh fetch view should reuse the endpoint discovery');
	});
	assert.deepEqual(await view.tags!(), []);
	assert.equal(calls, 1);
});

test('an explicit schema generation immediately rediscovers a capable upgraded endpoint', async () => {
	const url = endpoint();
	const old = new GraphQLBackend({
		endpoint: url,
		schemaGeneration: 'v1',
		fetch: async () => unknown('tags'),
	});
	await old.tags();
	const upgraded = new GraphQLBackend({
		endpoint: url,
		schemaGeneration: 'v2',
		fetch: async () => Response.json({ data: { tags } }),
	});
	assert.deepEqual(await upgraded.tags(), tags);
});

test('negative capabilities expire when no schema generation is available', async (t) => {
	let now = 100_000;
	t.mock.method(Date, 'now', () => now);
	let capable = false;
	let calls = 0;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return capable ? Response.json({ data: { tags } }) : unknown('tags');
		},
	});
	assert.deepEqual(await backend.tags(), []);
	capable = true;
	now += 299_999;
	assert.deepEqual(await backend.tags(), []);
	assert.equal(calls, 1);
	now += 1;
	assert.deepEqual(await backend.tags(), tags);
	assert.equal(calls, 2);
});

test('capability capsules are bounded to 64 endpoints, least recently used first', async () => {
	let calls = 0;
	const old = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return unknown('tags');
		},
	});
	const touch = async (count: number) => {
		for (let n = 0; n < count; n++) {
			await new GraphQLBackend({
				endpoint: endpoint(),
				fetch: async () => Response.json({ data: { tags } }),
			}).tags();
		}
	};
	assert.deepEqual(await old.tags(), []);
	await touch(63);
	assert.deepEqual(await old.tags(), [], 'still one of the 64 most recent endpoints');
	assert.equal(calls, 1);
	await touch(64);
	assert.deepEqual(await old.tags(), []);
	assert.equal(calls, 2, 'an evicted endpoint rediscovers instead of growing the registry');
});

test('GET rejection switches only that endpoint to POST', async () => {
	const oldMethods: string[] = [];
	const old = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (_url, init) => {
			const method = init?.method ?? 'GET';
			oldMethods.push(method);
			return method === 'GET'
				? new Response('', { status: 405 })
				: Response.json({ data: { tags } });
		},
	});
	await old.tags();
	await old.tags();
	assert.deepEqual(oldMethods, ['GET', 'POST', 'POST']);
	const modern = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (_url, init) => {
			assert.equal(init?.method, undefined, 'modern endpoint still uses cacheable anonymous GET');
			return Response.json({ data: { tags } });
		},
	});
	await modern.tags();
});

test('field and argument stripping is endpoint scoped', async () => {
	let oldCalls = 0;
	const old = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			oldCalls++;
			return oldCalls === 1
				? Response.json({
						errors: [
							{ message: 'Unknown field "detectedAt"' },
							{ message: 'Unknown argument "filter"' },
						],
					})
				: Response.json({ data: { search: { items: [], page: 1, hasNextPage: false, total: 0 } } });
		},
	});
	await old.search('fixture');
	assert.equal(old.browseFilterSupported(), false);
	const modern = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (input) => {
			const query = new URL(String(input)).searchParams.get('query')!;
			assert.match(query, /\n\s*detectedAt\s*\n/);
			assert.match(query, /filter:\s*\$filter/);
			return Response.json({
				data: { search: { items: [], page: 1, hasNextPage: false, total: 0 } },
			});
		},
	});
	await modern.search('fixture');
	assert.equal(modern.browseFilterSupported(), true);
});

test('chapter-origin, bundle, and explicit read mutation negatives stay on their owner', async () => {
	for (const [field, method] of [
		['chapterOrigin', 'chapterOrigin'],
		['seriesPage', 'seriesPage'],
		['setChapterRead', 'setChapterRead'],
	] as const) {
		const old = new GraphQLBackend({ endpoint: endpoint(), fetch: async () => unknown(field) });
		await old[method]('fixture', false);
		let calls = 0;
		const modern = new GraphQLBackend({
			endpoint: endpoint(),
			fetch: async () => {
				calls++;
				return Response.json({ data: { [field]: field === 'setChapterRead' ? true : null } });
			},
		});
		await modern[method]('fixture', false);
		assert.equal(calls, 1, field);
	}
});

test('transport failures are never treated as schema discoveries', async () => {
	let calls = 0;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return new Response('', { status: 503 });
		},
	});
	await assert.rejects(() => backend.tags(), /503/);
	await assert.rejects(() => backend.tags(), /503/);
	assert.equal(calls, 2);
});

test('a delayed optional-field validation cannot retry an old mutation under a new account', async () => {
	const requests: { token: string | null; query: string }[] = [];
	let finish!: (response: Response) => void;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		fetch: async (_url, init) => {
			requests.push({
				token: new Headers(init?.headers).get('authorization'),
				query: JSON.parse(String(init?.body)).query,
			});
			return requests.length === 1
				? new Promise<Response>((resolve) => (finish = resolve))
				: Response.json({ data: { mark: { id: 'w_fixture', isMarked: true } } });
		},
	});
	const pending = backend.mark('w_fixture', true);
	assert.match(requests[0].query, /\n\s*detectedAt\s*\n/);
	backend.setToken('B');
	finish(unknown('detectedAt'));
	await assert.rejects(pending, /Session changed/);
	assert.deepEqual(
		requests.map((request) => request.token),
		['Bearer A'],
	);
	await backend.mark('w_fixture', true);
	assert.equal(requests[1].token, 'Bearer B');
	assert.match(
		requests[1].query,
		/\n\s*detectedAt\s*\n/,
		'stale validation cannot mutate discovery',
	);
});

test('withFetch compatibility fallbacks share token revisions with their parent', async () => {
	let finish!: (response: Response) => void;
	let calls = 0;
	const backend = new GraphQLBackend({ endpoint: endpoint(), token: 'A' });
	const view = backend.withFetch(async (_url, init) => {
		calls++;
		assert.equal(new Headers(init?.headers).get('authorization'), 'Bearer A');
		return new Promise<Response>((resolve) => (finish = resolve));
	});
	const pending = view.instance!();
	backend.setToken('B');
	backend.setToken('A'); // Returning to the same token still crossed an auth boundary.
	finish(unknown('appsFetchThemselves'));
	await assert.rejects(pending, /Session changed/);
	assert.equal(calls, 1, 'the specialized fallback cannot revive an earlier auth revision');
});

test('same-session optional mutation fields still fall back when setToken repeats its value', async () => {
	const queries: string[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		fetch: async (_url, init) => {
			assert.equal(new Headers(init?.headers).get('authorization'), 'Bearer A');
			queries.push(JSON.parse(String(init?.body)).query);
			backend.setToken('A');
			return queries.length === 1
				? unknown('detectedAt')
				: Response.json({ data: { mark: { id: 'w_fixture', isMarked: true } } });
		},
	});
	assert.equal((await backend.mark('w_fixture', true)).isMarked, true);
	assert.equal(queries.length, 2);
	assert.match(queries[0], /\n\s*detectedAt\s*\n/);
	assert.doesNotMatch(queries[1], /\n\s*detectedAt\s*\n/);
});

test('GET compatibility fallback remains bounded when a slow request outlives discovery TTL', async (t) => {
	let now = 100_000;
	t.mock.method(Date, 'now', () => now);
	const methods: string[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (_url, init) => {
			methods.push(init?.method ?? 'GET');
			now += 300_001;
			assert.ok(methods.length <= 2, 'one slow GET rejection can only produce one POST retry');
			return init?.method === 'POST'
				? Response.json({ data: { tags } })
				: new Response('', { status: 405 });
		},
	});
	assert.deepEqual(await backend.tags(), tags);
	assert.deepEqual(methods, ['GET', 'POST']);
});

test('an older server without Page.media* fields still lists pages', async () => {
	const queries: string[] = [];
	const page = { index: 0, sourceUrl: 'https://fixture.test/p/0', width: null, height: null };
	const old = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (input) => {
			const query = new URL(String(input)).searchParams.get('query')!;
			queries.push(query);
			return queries.length === 1
				? Response.json({
						errors: ['mediaPath', 'mediaSha256', 'mediaLength', 'mediaMime'].map((field) => ({
							message: 'Unknown field "' + field + '"',
						})),
					})
				: Response.json({ data: { pages: [page] } });
		},
	});
	assert.deepEqual(await old.pages('9005'), [page]);
	assert.match(queries[0], /\n\s*mediaPath\s*\n/);
	assert.doesNotMatch(queries[1], /media/);
	assert.match(queries[1], /\n\s*height\s*\n/);
});
