import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';

registerHooks({
	load(url, context, next) {
		if (
			url.endsWith('/packages/api/src/graphql-backend.ts') ||
			url.endsWith('/packages/api/src/composite-backend.ts')
		) {
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
const { CompositeBackend } = await import('../../../../../packages/api/src/composite-backend.ts');
const ops = await import('../../../../../packages/api/src/operations.ts');

let endpointNumber = 0;
const endpoint = () => 'http://localhost/work-chapter-api-' + ++endpointNumber + '/graphql';
const workId = 'w_fixture';
const entry = {
	id: 'entry-1',
	matchKey: 'n:1',
	sortKey: 1,
	sortKeyText: '1.000000',
	number: '1',
	label: '1',
	special: false,
	specialBy: 'matcher',
	hidden: false,
	titleOverride: null,
	copies: [
		{
			chapterId: 'copy-1',
			sourceSeriesId: 'source-series-1',
			sourceType: 'suwayomi',
			sourceId: 'source-1',
			number: '1',
			title: 'First chapter',
			lang: 'en',
			scanlator: 'Fixture',
			releasedAt: '2026-10-01T00:00:00Z',
			linkedBy: 'matcher',
		},
	],
};
const list = { workId, ready: true, entries: [entry] };

function request(input: Parameters<typeof fetch>[0], init?: Parameters<typeof fetch>[1]) {
	if (init?.body) {
		return JSON.parse(String(init.body)) as { query: string; variables: Record<string, unknown> };
	}
	const params = new URL(String(input)).searchParams;
	return {
		query: params.get('query')!,
		variables: JSON.parse(params.get('variables') ?? '{}') as Record<string, unknown>,
	};
}

function hasFields(selection: string, fields: string[]) {
	for (const field of fields) assert.match(selection, new RegExp('^\\s*' + field + '\\s*$', 'm'));
}

function validSelections(document: string) {
	const source = document.replace(/#[^\n]*/g, '');
	assert.doesNotMatch(source, /\{\s*\}/, 'stripping must not empty a selection set');
	let depth = 0;
	for (const char of source) {
		if (char === '{') depth++;
		if (char === '}') depth--;
		assert.ok(depth >= 0, 'closing braces must have a matching opening brace');
	}
	assert.equal(depth, 0, 'selection braces must balance');
}

test('work chapter documents select optional entry identity and order on their own lines', () => {
	for (const document of [ops.SERIES_PAGE, ops.CANONICAL_CHAPTERS, ops.CHAPTERS]) {
		const fragment = document.match(/fragment ChapterFields on Chapter \{([^}]+)\}/)![1];
		hasFields(fragment, ['id', 'entryId', 'entryOrder', 'number', 'read']);
	}
	assert.match(ops.CHAPTERS, /chapters\(seriesId: \$seriesId\)\s*\{\s*\.\.\.ChapterFields/);
	assert.match(
		ops.CANONICAL_CHAPTERS,
		/canonicalChapters\(workId: \$workId\)\s*\{\s*\.\.\.ChapterFields/,
	);
	assert.match(ops.SERIES_PAGE, /spineChapters\s*\{\s*\.\.\.ChapterFields/);
	assert.match(ops.SERIES_PAGE, /chapters\s*\{\s*\.\.\.ChapterFields/);
	for (const document of [ops.AGGREGATED_CHAPTERS, ops.SERIES_PAGE]) {
		const selection = document.match(
			/(?:aggregatedChapters\([^)]*\)|aggregated @include\([^)]*\))\s*\{([\s\S]*?)sources\s*\{/,
		)![1];
		hasFields(selection, ['entryId', 'entryOrder', 'special', 'number', 'readAnywhere']);
	}
	for (const field of ['entryId', 'entryOrder', 'special'] as const) {
		assert.ok(ops.OPTIONAL_SERIES_FIELDS.includes(field));
	}
});

const chapter = { id: 'copy-1', number: 1, label: '1', read: false };
const aggregated = { number: 1, label: '1', readAnywhere: true, sources: [] };
const bundle = {
	workId,
	series: null,
	spineChapters: [chapter],
	sources: [{ source: { sourceId: 'source-1' }, chapters: [chapter], series: null }],
	aggregated: [aggregated],
};

for (const [name, document, key, value, load] of [
	['SERIES_PAGE', ops.SERIES_PAGE, 'seriesPage', bundle, (b) => b.seriesPage(workId, true)],
	[
		'AGGREGATED_CHAPTERS',
		ops.AGGREGATED_CHAPTERS,
		'aggregatedChapters',
		[aggregated],
		(b) => b.aggregatedChapters(workId),
	],
	[
		'CANONICAL_CHAPTERS',
		ops.CANONICAL_CHAPTERS,
		'canonicalChapters',
		[chapter],
		(b) => b.canonicalChapters(workId),
	],
	['CHAPTERS', ops.CHAPTERS, 'chapters', [chapter], (b) => b.chapters('source-series-1')],
] satisfies [
	string,
	string,
	string,
	unknown,
	(b: InstanceType<typeof GraphQLBackend>) => Promise<unknown>,
][]) {
	test(name + ' retries once without unsupported list fields and remembers them', async () => {
		const queries: string[] = [];
		const fields = ['entryId', 'entryOrder', 'special'].filter((field) =>
			new RegExp('^\\s*' + field + '\\s*$', 'm').test(document),
		);
		const backend = new GraphQLBackend({
			endpoint: endpoint(),
			fetch: async (input, init) => {
				queries.push(request(input, init).query);
				return queries.length === 1
					? Response.json({
							errors: fields.map((field) => ({ message: 'Unknown field "' + field + '"' })),
						})
					: Response.json({ data: { [key]: value } });
			},
		});
		assert.deepEqual(await load(backend), value);
		assert.equal(queries.length, 2, 'all unknown fields share one retry');
		assert.equal(queries[0], document);
		const stripped = document.replace(/^[\t ]*(?:entryId|entryOrder|special)[\t ]*\r?\n/gm, '');
		assert.equal(queries[1], stripped, 'every other selection stays intact');
		validSelections(queries[1]);
		assert.deepEqual(await load(backend), value);
		assert.equal(queries.length, 3);
		assert.equal(queries[2], stripped, 'later calls omit the unsupported fields immediately');
	});
}

test('workChapterList sends the admin document and preserves entries and linked copies', async () => {
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (input, init) => {
			assert.deepEqual(request(input, init), {
				query: ops.WORK_CHAPTER_LIST,
				variables: { workId },
			});
			return Response.json({ data: { workChapterList: list } });
		},
	});
	assert.deepEqual(await backend.workChapterList(workId), list);
});

test('an unknown workChapterList is cached across fetch views without another request', async () => {
	let calls = 0;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return Response.json({ errors: [{ message: 'Unknown field "workChapterList"' }] });
		},
	});
	assert.equal(await backend.workChapterList(workId), undefined);
	assert.equal(await backend.workChapterList('w_other'), undefined);
	const view = backend.withFetch(async () => assert.fail('the negative capability is shared'));
	assert.equal(await view.workChapterList!(workId), undefined);
	assert.equal(calls, 1);
	const modern = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => Response.json({ data: { workChapterList: list } }),
	});
	assert.deepEqual(await modern.workChapterList(workId), list);
});

test('workChapterList propagates other errors without caching them as unsupported', async () => {
	let calls = 0;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => {
			calls++;
			return Response.json({ errors: [{ message: 'Not authenticated' }] });
		},
	});
	await assert.rejects(() => backend.workChapterList(workId), /Not authenticated/);
	await assert.rejects(() => backend.workChapterList(workId), /Not authenticated/);
	assert.equal(calls, 2);
});

test('setWorkChapter sends only defined edit variables and preserves false, empty and zero values', async () => {
	const variables: Record<string, unknown>[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (input, init) => {
			const sent = request(input, init);
			assert.equal(init?.method, 'POST');
			assert.equal(sent.query, ops.SET_WORK_CHAPTER);
			variables.push(sent.variables);
			return Response.json({ data: { setWorkChapter: entry } });
		},
	});
	assert.deepEqual(
		await backend.setWorkChapter(entry.id, { hidden: true, sortKey: undefined }),
		entry,
	);
	assert.deepEqual(variables[0], { entryId: entry.id, hidden: true });
	await backend.setWorkChapter(entry.id, {
		hidden: false,
		titleOverride: '',
		clearTitle: false,
		special: false,
		sortKey: 0,
	});
	assert.deepEqual(variables[1], {
		entryId: entry.id,
		hidden: false,
		titleOverride: '',
		clearTitle: false,
		special: false,
		sortKey: 0,
	});
	await backend.setWorkChapter(entry.id, { clearTitle: true });
	assert.deepEqual(variables[2], { entryId: entry.id, clearTitle: true });
});

test('composite chapter-list methods delegate to hosted and tolerate missing capabilities', async () => {
	const calls: unknown[][] = [];
	const hosted = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () => assert.fail('no request'),
	});
	hosted.workChapterList = async (...args) => {
		calls.push(args);
		return list;
	};
	hosted.setWorkChapter = async (...args) => {
		calls.push(args);
		return entry;
	};
	const backend = new CompositeBackend({ hosted });
	assert.deepEqual(await backend.workChapterList(workId), list);
	assert.deepEqual(await backend.setWorkChapter(entry.id, { hidden: true }), entry);
	assert.deepEqual(calls, [[workId], [entry.id, { hidden: true }]]);
	Object.defineProperties(hosted, {
		workChapterList: { value: undefined },
		setWorkChapter: { value: undefined },
	});
	assert.equal(await backend.workChapterList(workId), undefined);
	await assert.rejects(
		() => backend.setWorkChapter(entry.id, { hidden: true }),
		/Chapter editing is unavailable on this backend\./,
	);
});
