import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire, registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import type { Backend, SourceMetadataBar, WorkMetadataSource } from '../src/backend.ts';

// Use the admin's existing compiler; the API has no build-time dependencies.
const ts = createRequire(new URL('../../../apps/admin/package.json', import.meta.url))(
	'typescript',
);
// The API uses bundler-style .js imports and constructor parameter properties.
registerHooks({
	resolve(specifier, context, next) {
		if (specifier.startsWith('.') && specifier.endsWith('.js') && context.parentURL) {
			const url = new URL(specifier.slice(0, -3) + '.ts', context.parentURL);
			if (existsSync(fileURLToPath(url))) return { url: url.href, shortCircuit: true };
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (/\/packages\/api\/src\/(graphql|composite)-backend\.ts$/.test(url)) {
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

const { GraphQLBackend } = await import('../src/graphql-backend.ts');
const { CompositeBackend } = await import('../src/composite-backend.ts');
const ops = await import('../src/operations.ts');
let endpointNumber = 0;
const endpoint = () => `http://localhost:8080/graphql?fixture=${++endpointNumber}`;
const unknown = (field: string) =>
	Response.json({ errors: [{ message: `Unknown field "${field}"` }] });
const bars: SourceMetadataBar[] = [
	{ sourceId: '123', fieldGroup: 'CREDITS', createdAt: '2026-10-16T12:00:00Z' },
];
const provenance: WorkMetadataSource[] = [
	{
		fieldGroup: 'CREDITS',
		state: 'SOURCE',
		sourceId: '123',
		sourceName: 'Example source',
		sourceSeriesId: 'ss_example',
		attribution: 'INFERRED',
	},
];

test('metadata operations send the contract fields and variables', async () => {
	const requests: { query: string; variables?: Record<string, unknown> }[] = [];
	const saved = { sourceId: '123', fieldGroup: 'CREDITS', barred: true, works: 7 };
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'fixture-admin',
		fetch: async (_input, init) => {
			assert.equal(init?.method, 'POST');
			const request = JSON.parse(String(init?.body));
			requests.push(request);
			return Response.json({
				data: {
					sourceMetadataBars: bars,
					workMetadataSources: provenance,
					setSourceMetadataBar: saved,
				},
			});
		},
	});
	assert.deepEqual(await backend.sourceMetadataBars(), bars);
	assert.equal(backend.metadataBarsSupported(), true);
	assert.deepEqual(await backend.workMetadataSources('w_example'), provenance);
	assert.deepEqual(await backend.setSourceMetadataBar('123', 'CREDITS', true), saved);
	assert.equal(requests[0].query, ops.SOURCE_METADATA_BARS);
	assert.match(requests[0].query, /sourceId\s+fieldGroup\s+createdAt/);
	assert.equal(requests[1].query, ops.WORK_METADATA_SOURCES);
	assert.deepEqual(requests[1].variables, { workId: 'w_example' });
	assert.match(
		requests[1].query,
		/fieldGroup\s+state\s+sourceId\s+sourceName\s+sourceSeriesId\s+attribution/,
	);
	assert.equal(requests[2].query, ops.SET_SOURCE_METADATA_BAR);
	assert.deepEqual(requests[2].variables, { sourceId: '123', fieldGroup: 'CREDITS', barred: true });
	assert.match(requests[2].query, /\$fieldGroup: MetadataFieldGroup!/);
	assert.match(requests[2].query, /sourceId\s+fieldGroup\s+barred\s+works/);
});

test('old list queries return empty lists and cache only their own field', async () => {
	const calls: string[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'fixture-admin',
		fetch: async (_input, init) => {
			const { query } = JSON.parse(String(init?.body));
			const field = query.includes('query SourceMetadataBars')
				? 'sourceMetadataBars'
				: 'workMetadataSources';
			calls.push(field);
			return unknown(field);
		},
	});
	assert.deepEqual(await backend.sourceMetadataBars(), []);
	assert.equal(backend.metadataBarsSupported(), false);
	assert.deepEqual(await backend.sourceMetadataBars(), []);
	assert.deepEqual(await backend.workMetadataSources('w_one'), []);
	assert.deepEqual(await backend.workMetadataSources('w_two'), []);
	await assert.rejects(backend.setSourceMetadataBar('123', 'TITLE', true), {
		message: 'This server does not support metadata bars',
	});
	assert.deepEqual(calls, ['sourceMetadataBars', 'workMetadataSources']);
});

test('an empty supported bar list still enables toggles', async () => {
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => Response.json({ data: { sourceMetadataBars: [] } }),
	});
	assert.deepEqual(await backend.sourceMetadataBars(), []);
	assert.equal(backend.metadataBarsSupported(), true);
});

test('unknown mutations give a clear error and share the negative with fetch views', async () => {
	let calls = 0;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return unknown('setSourceMetadataBar');
		},
	});
	for (let n = 0; n < 2; n++) {
		await assert.rejects(backend.setSourceMetadataBar('123', 'TAGS', true), {
			message: 'This server does not support metadata bars',
		});
	}
	assert.equal(calls, 1);
	const view = backend.withFetch(async () => assert.fail('must reuse capability discovery'));
	assert.equal(view.metadataBarsSupported!(), false);
	await assert.rejects(
		view.setSourceMetadataBar!('123', 'TAGS', false),
		/does not support metadata bars/,
	);
});

test('negative capabilities stay on their endpoint and schema generation', async () => {
	const url = endpoint();
	const old = new GraphQLBackend({
		endpoint: url,
		schemaGeneration: 'old',
		fetch: async () => unknown('sourceMetadataBars'),
	});
	await old.sourceMetadataBars();
	for (const config of [
		{ endpoint: endpoint(), schemaGeneration: 'old' },
		{ endpoint: url, schemaGeneration: 'new' },
	]) {
		const modern = new GraphQLBackend({
			...config,
			fetch: async () => Response.json({ data: { sourceMetadataBars: bars } }),
		});
		assert.deepEqual(await modern.sourceMetadataBars(), bars);
		assert.equal(modern.metadataBarsSupported(), true);
	}
});

test('metadata mutation negatives expire so an upgraded server can be discovered', async (t) => {
	let now = 100_000;
	t.mock.method(Date, 'now', () => now);
	let capable = false;
	const saved = { sourceId: '123', fieldGroup: 'STATUS', barred: false, works: 1 };
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () =>
			capable
				? Response.json({ data: { setSourceMetadataBar: saved } })
				: unknown('setSourceMetadataBar'),
	});
	await assert.rejects(backend.setSourceMetadataBar('123', 'STATUS', false));
	capable = true;
	now += 300_000;
	assert.deepEqual(await backend.setSourceMetadataBar('123', 'STATUS', false), saved);
});

test('permission, unknown work and transport failures are not capability negatives', async () => {
	for (const failure of [
		() => Response.json({ errors: [{ message: 'Admin only' }] }),
		() => Response.json({ errors: [{ message: 'Unknown work' }] }),
		() => new Response('', { status: 503 }),
	]) {
		let calls = 0;
		const backend = new GraphQLBackend({
			endpoint: endpoint(),
			fetch: async () => {
				calls++;
				return failure();
			},
		});
		for (let n = 0; n < 2; n++) {
			await assert.rejects(backend.sourceMetadataBars());
			await assert.rejects(backend.workMetadataSources('w_missing'));
			await assert.rejects(backend.setSourceMetadataBar('123', 'TITLE', true));
		}
		assert.equal(calls, 6);
		assert.equal(backend.metadataBarsSupported(), true);
	}
});

test('a delayed old-account error does not disable metadata bars for the next account', async () => {
	let reply!: (response: Response) => void;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'first-account',
		fetch: () =>
			new Promise<Response>((resolve) => {
				reply = resolve;
			}),
	});
	const pending = backend.setSourceMetadataBar('123', 'TITLE', true);
	backend.setToken('second-account');
	reply(unknown('setSourceMetadataBar'));
	await assert.rejects(pending);
	assert.equal(backend.metadataBarsSupported(), true);
});

test('composite metadata reads, writes and capability checks forward to the server', async () => {
	const calls: unknown[] = [];
	const saved = { sourceId: '123', fieldGroup: 'TITLE', barred: true, works: 4 };
	const hosted = {
		sourceMetadataBars: async () => bars,
		workMetadataSources: async (workId: string) => {
			calls.push(workId);
			return provenance;
		},
		metadataBarsSupported: () => true,
		setSourceMetadataBar: async (...args: unknown[]) => {
			calls.push(args);
			return saved;
		},
	} as unknown as Backend;
	const composite = new CompositeBackend({ hosted });
	assert.deepEqual(await composite.sourceMetadataBars(), bars);
	assert.deepEqual(await composite.workMetadataSources('w_example'), provenance);
	assert.equal(composite.metadataBarsSupported(), true);
	assert.deepEqual(await composite.setSourceMetadataBar('123', 'TITLE', true), saved);
	assert.deepEqual(calls, ['w_example', ['123', 'TITLE', true]]);
	const old = new CompositeBackend({ hosted: {} as Backend });
	assert.deepEqual(await old.sourceMetadataBars(), []);
	assert.deepEqual(await old.workMetadataSources('w_example'), []);
	assert.equal(old.metadataBarsSupported(), false);
	await assert.rejects(
		old.setSourceMetadataBar('123', 'TITLE', true),
		/does not support metadata bars/,
	);
});
