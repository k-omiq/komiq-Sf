import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

registerHooks({
	resolve(specifier, context, next) {
		if (
			context.parentURL?.includes('/packages/api/src/') &&
			specifier.startsWith('./') &&
			specifier.endsWith('.js')
		) {
			return next(specifier.slice(0, -3) + '.ts', context);
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (url.includes('/packages/api/src/') && url.endsWith('.ts')) {
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
const { GraphQLBackend } = await import('../../../../packages/api/src/graphql-backend.ts');
const { operationDocuments } =
	await import('../../../../packages/api/scripts/persisted-operations.ts');
const { persistedId } = await import('../../../../packages/api/src/persisted.ts');
/** A GET names its `operations.ts` document by persisted id. */
const persisted = new Map<string, string>(
	operationDocuments().map((doc: string) => [persistedId(doc), doc]),
);

let endpointId = 0;
function backend(fetch: typeof globalThis.fetch) {
	return new GraphQLBackend({
		endpoint: `https://komiq.example.com/test-${++endpointId}/graphql`,
		fetch,
	});
}

const route = {
	sourceId: 'src',
	engineId: 'sg',
	strict: true,
	active: true,
	versionCheck: 'MATCH',
};

function requestDocument(
	url: RequestInfo | URL,
	init?: RequestInit,
): { query: string; variables: Record<string, unknown> } {
	if (init?.body) return JSON.parse(init.body as string);
	const params = new URL(String(url)).searchParams;
	const id = params.get('id');
	return {
		query: (id === null ? params.get('query') : persisted.get(id)) ?? '',
		variables: JSON.parse(params.get('variables') ?? '{}'),
	};
}

test('old servers return empty/null for engine operations and cache the capability', async () => {
	for (const wording of ['Unknown field', 'Cannot query field']) {
		let calls = 0;
		const old = backend(async (_url, init) => {
			calls++;
			const { query } = requestDocument(_url, init);
			const field = query.includes('mutation SetSourceEngine')
				? 'setSourceEngine'
				: query.includes('query SourceEngineRoute')
					? 'sourceEngineRoute'
					: 'engines';
			return Response.json({ errors: [{ message: `${wording} "${field}" on type "QueryRoot".` }] });
		});
		assert.deepEqual(await old.engines(), []);
		assert.equal(await old.sourceEngineRoute('src'), null);
		assert.equal(await old.setSourceEngine('src', 'sg', true), null);
		const view = old.withFetch(() => assert.fail('capabilities should be shared'));
		assert.deepEqual(await view.engines!(), []);
		assert.equal(await view.sourceEngineRoute!('src'), null);
		assert.equal(await view.setSourceEngine!('src', null), null);
		assert.equal(calls, 3);
	}
});

test('refusals, auth failures, and transport errors do not disable routing', async () => {
	for (const message of [
		'extension versions differ',
		'Admin required',
		'Cannot query field "other"',
	]) {
		let calls = 0;
		const api = backend(async () => {
			calls++;
			return calls === 1
				? Response.json({ errors: [{ message }] })
				: Response.json({ data: { setSourceEngine: route } });
		});
		await assert.rejects(api.setSourceEngine('src', 'sg', true), { message });
		assert.deepEqual(await api.setSourceEngine('src', 'sg', true), route);
		assert.equal(calls, 2);
	}
	const down = backend(async () => new Response('', { status: 503 }));
	await assert.rejects(down.engines(), /503/);
	await assert.rejects(down.sourceEngineRoute('src'), /503/);
});

test('modern operations preserve counter strings and send engine ids and strict defaults', async () => {
	const rows = [{ id: 'sg', counters: { pageBytes: '18446744073709551615' } }];
	const requests: { query: string; variables: unknown }[] = [];
	const api = backend(async (_url, init) => {
		const request = requestDocument(_url, init);
		requests.push(request);
		const field = request.query.includes('mutation SetSourceEngine')
			? 'setSourceEngine'
			: request.query.includes('query SourceEngineRoute')
				? 'sourceEngineRoute'
				: 'engines';
		return Response.json({
			data: {
				[field]:
					field === 'engines'
						? rows
						: field === 'setSourceEngine' && request.variables.engineId === null
							? null
							: route,
			},
		});
	});
	assert.deepEqual(await api.engines(), rows);
	assert.deepEqual(await api.sourceEngineRoute('src'), route);
	assert.deepEqual(await api.setSourceEngine('src', 'sg', true), route);
	assert.equal(await api.setSourceEngine('src', null), null);
	assert.deepEqual(requests[2].variables, { sourceId: 'src', engineId: 'sg', strict: true });
	assert.deepEqual(requests[3].variables, { sourceId: 'src', engineId: null, strict: false });
});

test('a schema upgrade rediscovers regional engines at the same endpoint', async () => {
	const endpoint = `https://komiq.example.com/test-${++endpointId}/graphql`;
	const old = new GraphQLBackend({
		endpoint,
		schemaGeneration: 'old',
		fetch: async () => Response.json({ errors: [{ message: 'Unknown field "engines"' }] }),
	});
	assert.deepEqual(await old.engines(), []);
	const rows = [{ id: 'owner' }, { id: 'sg' }];
	const upgraded = new GraphQLBackend({
		endpoint,
		schemaGeneration: 'new',
		fetch: async () => Response.json({ data: { engines: rows } }),
	});
	assert.deepEqual(await upgraded.engines(), rows);
});
