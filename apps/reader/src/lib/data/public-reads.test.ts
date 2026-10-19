import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import { persistedServer } from './node-test-hooks.ts';

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
const endpoint = () => 'https://public-reads-' + ++endpointNumber + '.test/graphql';

interface Sent {
	method: string;
	name: string;
	/** A GET that named a persisted document instead of carrying its text. */
	persisted: boolean;
	/** The shared NSFW pass the GET carried, if any. */
	nsfw: string | null;
	authorization: string | null;
}

type Answer =
	unknown | ((variables: Record<string, unknown>, authorization: string | null) => unknown);

/** A server that answers by operation name and records how each request came. */
function server(answers: Record<string, Answer>) {
	const sent: Sent[] = [];
	const answer = persistedServer(async (input, init) => {
		const url = new URL(String(input));
		const body = init?.body
			? JSON.parse(String(init.body))
			: {
					query: url.searchParams.get('query'),
					variables: JSON.parse(url.searchParams.get('variables') ?? '{}'),
				};
		const name = /^\s*(?:query|mutation)\s+(\w+)/m.exec(body.query)![1];
		sent.at(-1)!.name = name;
		const reply = answers[name];
		if (reply === undefined) assert.fail('unexpected ' + name);
		const authorization = new Headers(init?.headers).get('authorization');
		return Response.json(
			typeof reply === 'function' ? reply(body.variables ?? {}, authorization) : reply,
		);
	});
	const fetch: typeof globalThis.fetch = async (input, init) => {
		sent.push({
			method: init?.method ?? 'GET',
			name: '',
			persisted: new URL(String(input)).searchParams.has('id'),
			nsfw: new URL(String(input)).searchParams.get('nsfw'),
			authorization: new Headers(init?.headers).get('authorization'),
		});
		return answer(input, init);
	};
	return { sent, fetch };
}

/** `name METHOD [token] [id]`, sorted: what went out, whatever the order. */
const shape = (sent: Sent[]) =>
	sent
		.map(
			(s) =>
				`${s.name} ${s.method}` +
				(s.authorization ? ' token' : '') +
				(s.persisted ? ' id' : '') +
				(s.nsfw ? ` nsfw=${s.nsfw}` : ''),
		)
		.sort();

const series = (id: string) => ({
	id,
	title: 'Fixture',
	isMarked: false,
	libraryStatus: null,
	isFavorite: false,
});
const chapter = (id: string) => ({ id, number: 1, read: false, lastPageRead: 0 });
const anonymousPage = {
	workId: 'w_1',
	viewerProgress: null,
	series: series('w_1'),
	spineChapters: [chapter('md-1'), chapter('md-2')],
	sources: [
		{ source: { sourceId: 'src' }, chapters: [chapter('91'), chapter('92')], series: series('9') },
	],
	aggregated: [
		{ entryId: 'e1', key: '100', number: 1, readAnywhere: false, sources: [] },
		{ entryId: 'e2', key: '200', number: 2, readAnywhere: false, sources: [] },
	],
};
const viewer = {
	workId: 'w_1',
	viewerProgress: { revision: 4, throughEntryId: 'e1' },
	library: [
		{ seriesId: 'w_1', isMarked: true, libraryStatus: 'reading', isFavorite: false },
		{ seriesId: '9', isMarked: false, libraryStatus: null, isFavorite: true },
	],
	chapters: [
		{ sourceType: 'mangadex', chapterId: 'md-1', read: true, lastPageRead: 0 },
		{ sourceType: 'suwayomi', chapterId: '92', read: false, lastPageRead: 5 },
		// A copy id names one source type: this one is not the spine chapter `md-2`.
		{ sourceType: 'suwayomi', chapterId: 'md-2', read: true, lastPageRead: 0 },
	],
	aggregated: [{ entryId: 'e1', key: '100' }],
};
const signedPage = {
	...anonymousPage,
	viewerProgress: viewer.viewerProgress,
	series: { ...series('w_1'), isMarked: true, libraryStatus: 'reading' },
	spineChapters: [{ ...chapter('md-1'), read: true }, chapter('md-2')],
	sources: [
		{
			source: { sourceId: 'src' },
			chapters: [chapter('91'), { ...chapter('92'), lastPageRead: 5 }],
			series: { ...series('9'), isFavorite: true },
		},
	],
	aggregated: [{ ...anonymousPage.aggregated[0], readAnywhere: true }, anonymousPage.aggregated[1]],
};

const posture = (showNsfw: boolean, rules: { tags: string[]; ratings: string[] }[] = []) => ({
	data: { session: { user: { showNsfw } }, myBlockRules: rules },
});
const catalogue = {
	NumericEpoch: { data: { numericEpoch: 3 } },
	Discovery: { data: { discovery: [] } },
	Search: { data: { search: { items: [], page: 1, hasNextPage: false, total: 0 } } },
};

test('a signed-in page load sends the catalogue as anonymous GETs and its own data by POST', async () => {
	const { sent, fetch } = server({
		...catalogue,
		ViewerPosture: posture(false),
		SeriesPage: (variables: Record<string, unknown>) => {
			assert.deepEqual(variables, { workId: 'w_1', withAggregated: true });
			return { data: { seriesPage: anonymousPage } };
		},
		SeriesPageViewer: { data: { seriesPageViewer: viewer } },
	});
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		publicReads: true,
		fetch,
	});
	await backend.discovery();
	await backend.search('fixture');
	assert.deepEqual(await backend.seriesPage('w_1', true), signedPage);
	assert.deepEqual(shape(sent), [
		'Discovery GET id',
		'NumericEpoch POST token',
		'Search GET id',
		'SeriesPage GET id',
		'SeriesPageViewer POST token',
		'ViewerPosture POST token',
	]);
	// An admin's includeNsfw is answered for the admin: it keeps the token.
	sent.length = 0;
	await backend.search('fixture', 1, undefined, true);
	assert.deepEqual(shape(sent), ['Search POST token']);
});

const noPass = { data: { nsfwPass: null } };
for (const [name, answer, pass] of [
	['an NSFW opt-in but no pass', posture(true), noPass],
	[
		'an NSFW opt-in on a server without passes',
		posture(true),
		{
			errors: [{ message: 'Unknown field "nsfwPass" on type "QueryRoot".' }],
		},
	],
	['a block rule of their own', posture(false, [{ tags: ['gore'], ratings: [] }]), undefined],
	['a failed posture read', { errors: [{ message: 'boom' }] }, undefined],
] as const) {
	test(`a viewer with ${name} keeps every read signed in`, async () => {
		const { sent, fetch } = server({
			...catalogue,
			ViewerPosture: answer,
			...(pass ? { NsfwPass: pass } : {}),
			SeriesPage: { data: { seriesPage: signedPage } },
		});
		const backend = new GraphQLBackend({
			endpoint: endpoint(),
			token: 'A',
			publicReads: true,
			fetch,
		});
		await backend.discovery();
		assert.deepEqual(await backend.seriesPage('w_1', true), signedPage);
		assert.deepEqual(
			shape(sent),
			[
				'Discovery POST token',
				'NumericEpoch POST token',
				'SeriesPage POST token',
				'ViewerPosture POST token',
				...(pass ? ['NsfwPass POST token'] : []),
			].sort(),
		);
	});
}

const PASS = 'ab'.repeat(32);

test('an NSFW viewer with a pass reads the catalogue as cacheable GETs that carry it', async () => {
	const { sent, fetch } = server({
		...catalogue,
		ViewerPosture: posture(true),
		NsfwPass: { data: { nsfwPass: PASS } },
		SeriesPage: { data: { seriesPage: anonymousPage } },
		SeriesPageViewer: { data: { seriesPageViewer: viewer } },
	});
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		publicReads: true,
		fetch,
	});
	await backend.discovery();
	assert.deepEqual(await backend.seriesPage('w_1', true), signedPage);
	assert.deepEqual(shape(sent), [
		`Discovery GET id nsfw=${PASS}`,
		'NsfwPass POST token',
		'NumericEpoch POST token',
		`SeriesPage GET id nsfw=${PASS}`,
		'SeriesPageViewer POST token',
		'ViewerPosture POST token',
	]);
});

test('an expired pass answers that read signed in and fetches a new pass next time', async () => {
	let passes = 0;
	const { sent, fetch } = server({
		...catalogue,
		ViewerPosture: posture(true),
		NsfwPass: () => ({ data: { nsfwPass: String(++passes).repeat(64) } }),
		Discovery: (_: unknown, authorization: string | null) =>
			authorization || passes > 1
				? { data: { discovery: [] } }
				: {
						data: null,
						errors: [{ message: 'expired', extensions: { code: 'NSFW_PASS_EXPIRED' } }],
					},
	});
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		publicReads: true,
		fetch,
	});
	assert.deepEqual(await backend.discovery(), []);
	assert.deepEqual(shape(sent), [
		'Discovery GET id nsfw=' + '1'.repeat(64),
		'Discovery POST token',
		'NsfwPass POST token',
		'NumericEpoch POST token',
		'ViewerPosture POST token',
	]);
	sent.length = 0;
	await backend.discovery();
	assert.deepEqual(shape(sent), [
		'Discovery GET id nsfw=' + '2'.repeat(64),
		'NsfwPass POST token',
		'ViewerPosture POST token',
	]);
});

test('without publicReads (the admin console) a signed-in viewer never reads anonymously', async () => {
	const { sent, fetch } = server({
		...catalogue,
		SeriesPage: { data: { seriesPage: signedPage } },
	});
	const backend = new GraphQLBackend({ endpoint: endpoint(), token: 'A', fetch });
	await backend.discovery();
	await backend.seriesPage('w_1', true);
	assert.deepEqual(shape(sent), [
		'Discovery POST token',
		'NumericEpoch POST token',
		'SeriesPage POST token',
	]);
});

test('a server without seriesPageViewer gets the signed-in page, and is remembered', async () => {
	const { sent, fetch } = server({
		...catalogue,
		ViewerPosture: posture(false),
		SeriesPage: (_: unknown, authorization: string | null) => ({
			data: { seriesPage: authorization ? signedPage : anonymousPage },
		}),
		SeriesPageViewer: {
			errors: [{ message: 'Unknown field "seriesPageViewer" on type "QueryRoot".' }],
		},
	});
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		publicReads: true,
		fetch,
	});
	assert.deepEqual(await backend.seriesPage('w_1', true), signedPage);
	assert.deepEqual(shape(sent), [
		'NumericEpoch POST token',
		'SeriesPage GET id',
		'SeriesPage POST token',
		'SeriesPageViewer POST token',
		'ViewerPosture POST token',
	]);
	sent.length = 0;
	assert.deepEqual(await backend.seriesPage('w_1', true), signedPage);
	assert.deepEqual(shape(sent), ['SeriesPage POST token']);
});

test('changing the NSFW opt-in here asks for the posture again', async () => {
	let showNsfw = false;
	const { sent, fetch } = server({
		...catalogue,
		ViewerPosture: () => posture(showNsfw),
		NsfwPass: noPass,
		SetShowNsfw: () => ({ data: { setShowNsfw: (showNsfw = true) } }),
	});
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		publicReads: true,
		fetch,
	});
	await backend.discovery();
	await backend.setShowNsfw(true);
	sent.length = 0;
	await backend.discovery();
	assert.deepEqual(shape(sent), [
		'Discovery POST token',
		'NsfwPass POST token',
		'ViewerPosture POST token',
	]);
});

test('a signed-out reader never asks for a posture', async () => {
	const { sent, fetch } = server(catalogue);
	const backend = new GraphQLBackend({ endpoint: endpoint(), publicReads: true, fetch });
	await backend.discovery();
	assert.deepEqual(shape(sent), ['Discovery GET id']);
});

/** A server's GET answers, in order, then the shape of every request. */
async function transport(answers: ((url: URL) => Response | undefined)[], calls = 2) {
	const sent: string[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (input, init) => {
			const url = new URL(String(input));
			if (init?.method === 'POST') {
				sent.push('POST');
			} else {
				sent.push(url.searchParams.has('id') ? 'GET id' : 'GET text');
				const answer = answers.shift()?.(url);
				if (answer) return answer;
			}
			return Response.json({ data: { tags: [] } });
		},
	});
	for (let i = 0; i < calls; i++) assert.deepEqual(await backend.tags(), []);
	return sent;
}

const refused = (code?: string, status = 400) =>
	Response.json(
		{ errors: [{ message: 'refused', ...(code ? { extensions: { code } } : {}) }] },
		{ status },
	);

test('an id the server does not hold is sent as text, and remembered', async () => {
	assert.deepEqual(await transport([() => refused('PERSISTED_QUERY_NOT_FOUND')]), [
		'GET id',
		'GET text',
		'GET text',
	]);
});

test('a server that runs only persisted documents over GET gets the text by POST', async () => {
	assert.deepEqual(
		await transport([
			() => refused('PERSISTED_QUERY_NOT_FOUND'),
			() => refused('PERSISTED_QUERY_REQUIRED', 405),
		]),
		['GET id', 'GET text', 'POST', 'POST'],
	);
});

test('that refusal leaves other documents on persisted GETs', async () => {
	const sent: string[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (input, init) => {
			const url = new URL(String(input));
			const how =
				init?.method === 'POST' ? 'POST' : url.searchParams.has('id') ? 'GET id' : 'GET text';
			sent.push(how);
			if (how === 'GET text') return refused('PERSISTED_QUERY_REQUIRED', 405);
			if (how === 'GET id' && sent.length === 1) return refused('PERSISTED_QUERY_NOT_FOUND');
			return Response.json({ data: { tags: [], genreFacets: [] } });
		},
	});
	await backend.tags();
	await backend.genreFacets();
	assert.deepEqual(sent, ['GET id', 'GET text', 'POST', 'GET id']);
});

test('a plain 405 still means the server has no GET at all', async () => {
	assert.deepEqual(await transport([() => new Response('no', { status: 405 })]), [
		'GET id',
		'POST',
		'POST',
	]);
});

test('a server that predates persisted documents gets the text from then on', async () => {
	const older = () =>
		Response.json({
			errors: [{ message: 'expected executable_definition', locations: [] }],
		});
	assert.deepEqual(await transport([older]), ['GET id', 'GET text', 'GET text']);
});

test('any other 400 fails the read', async () => {
	await assert.rejects(transport([() => refused()], 1), /Backend error 400/);
});
