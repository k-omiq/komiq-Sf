import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';
import type { ProgressResult, ProgressStatus } from '../../../../../packages/types/src/index.ts';
import type { ProgressConflict } from '../../../../../packages/api/src/progress-conflict.ts';

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
const { onProgressConflict } = await import('../../../../../packages/api/src/progress-conflict.ts');
const { onStaleNumericEpoch } = await import('../../../../../packages/api/src/numeric-epoch.ts');
const ops = await import('../../../../../packages/api/src/operations.ts');

type Request = { query: string; variables: Record<string, unknown> };
let endpointNumber = 0;
const workId = 'w_fixture';
const progress = { revision: 7, throughEntryId: 'entry-1' };
const bundle = {
	workId: 'w_effective',
	series: null,
	spineChapters: [],
	sources: [],
	viewerProgress: progress,
};
const input = { chapterId: '42', workId, lastPageRead: 8, read: false, numericEpoch: 3 };

function client(respond: (request: Request) => unknown | Promise<unknown>) {
	const requests: Request[] = [];
	const backend = new GraphQLBackend({
		endpoint: 'http://localhost/progress-revision-' + ++endpointNumber + '/graphql',
		token: 'token-A',
		fetch: async (url, init) => {
			const params = new URL(String(url)).searchParams;
			const sent: Request = init?.body
				? JSON.parse(String(init.body))
				: { query: params.get('query')!, variables: JSON.parse(params.get('variables') ?? '{}') };
			if (sent.query === ops.NUMERIC_EPOCH) return Response.json({ data: { numericEpoch: 3 } });
			requests.push(sent);
			return Response.json(await respond(sent));
		},
	});
	return { backend, requests };
}

test('seriesPage seeds both work ids and saveProgress sends every revisioned variable', async () => {
	const answer: ProgressResult = { status: 'APPLIED', revision: 8, workId: bundle.workId };
	const { backend, requests } = client(({ query }) => {
		if (query === ops.SERIES_PAGE) return { data: { seriesPage: bundle } };
		assert.equal(query, ops.SAVE_PROGRESS);
		return { data: { saveProgress: answer } };
	});
	assert.deepEqual(await backend.seriesPage(workId), bundle);
	assert.equal(backend.knownRevision(workId), 7);
	assert.equal(backend.knownRevision(bundle.workId), 7);
	assert.deepEqual(await backend.saveProgress(input), answer);
	assert.deepEqual(requests[1], { query: ops.SAVE_PROGRESS, variables: { ...input, revision: 7 } });
	assert.equal(backend.knownRevision(workId), 8);
	assert.equal(backend.knownRevision(bundle.workId), 8);
});

test('an unknown base fetches viewerProgress first and an explicit revision wins', async () => {
	const { backend, requests } = client(({ query, variables }) => {
		if (query === ops.VIEWER_PROGRESS) return { data: { viewerProgress: progress } };
		return {
			data: {
				saveProgress: { status: 'APPLIED', revision: Number(variables.revision) + 1, workId },
			},
		};
	});
	await backend.saveProgress(input);
	assert.deepEqual(
		requests.map((r) => r.query),
		[ops.VIEWER_PROGRESS, ops.SAVE_PROGRESS],
	);
	assert.deepEqual(requests[0].variables, { workId });
	assert.equal(requests[1].variables.revision, 7);
	await backend.saveProgress({ ...input, revision: 2 });
	assert.equal(requests[2].variables.revision, 2);
	assert.equal(
		backend.knownRevision(workId),
		8,
		'an older APPLIED answer cannot lower the known base',
	);
});

for (const method of ['saveProgress', 'saveChapterRead'] as const) {
	for (const status of ['APPLIED', 'PAGE_ONLY', 'REFUSED', 'MOVED'] satisfies ProgressStatus[]) {
		test(method + ' handles ' + status + ' revisions and conflicts', async (t) => {
			const conflicts: ProgressConflict[] = [];
			t.after(onProgressConflict((conflict) => conflicts.push(conflict)));
			const answer = { status, revision: 20, workId: 'w_destination' };
			const { backend } = client(({ query }) =>
				query === ops.VIEWER_PROGRESS
					? { data: { viewerProgress: progress } }
					: { data: { [method]: answer } },
			);
			await backend.viewerProgress(workId);
			assert.deepEqual(await backend[method](input), answer);
			// A conflict forgets the base, so the next save reads it again after the refresh.
			const known = { APPLIED: 20, PAGE_ONLY: 7, REFUSED: undefined, MOVED: undefined }[status];
			assert.equal(backend.knownRevision(workId), known);
			assert.equal(backend.knownRevision('w_destination'), status === 'APPLIED' ? 20 : undefined);
			assert.deepEqual(
				conflicts,
				status === 'REFUSED' || status === 'MOVED'
					? [{ status, workId, movedTo: 'w_destination', chapterId: input.chapterId }]
					: [],
			);
		});
	}
}

test('saveChapterRead sends its exact mutation and obtains the live numeric epoch', async () => {
	const answer = { status: 'APPLIED', revision: 0, workId: null };
	const { backend, requests } = client(() => ({ data: { saveChapterRead: answer } }));
	const tick = { chapterId: '42', workId, read: true, revision: 0 };
	assert.deepEqual(await backend.saveChapterRead(tick), answer);
	assert.deepEqual(requests, [
		{ query: ops.SAVE_CHAPTER_READ, variables: { ...tick, numericEpoch: 3 } },
	]);
	assert.equal(backend.knownRevision(workId), 0);
});

for (const method of ['saveProgress', 'saveChapterRead'] as const) {
	test(
		method + ' falls back to the legacy mutation and remembers the shared capability',
		async () => {
			const legacy = method === 'saveProgress' ? 'setProgress' : 'setChapterRead';
			const { backend, requests } = client(({ query }) => {
				if (query === ops.SAVE_PROGRESS || query === ops.SAVE_CHAPTER_READ) {
					return { errors: [{ message: 'Unknown field "' + method + '"' }] };
				}
				assert.ok(query === ops.SET_PROGRESS || query === ops.SET_CHAPTER_READ);
				return { data: { setProgress: true, setChapterRead: true } };
			});
			const save = () =>
				method === 'saveProgress'
					? backend.saveProgress({ ...input, revision: 4 })
					: backend.saveChapterRead({ chapterId: '42', workId, read: false, revision: 4 });
			assert.equal(await save(), null);
			assert.equal(await save(), null);
			assert.equal(requests.length, 3);
			assert.equal(
				requests[1].query,
				legacy === 'setProgress' ? ops.SET_PROGRESS : ops.SET_CHAPTER_READ,
			);
			assert.deepEqual(
				requests.slice(1).map((r) => r.variables),
				Array(2).fill(
					method === 'saveProgress'
						? { chapterId: '42', lastPageRead: 8, read: false, numericEpoch: 3 }
						: { chapterId: '42', read: false, numericEpoch: 3 },
				),
			);
			const other = method === 'saveProgress' ? 'saveChapterRead' : 'saveProgress';
			assert.equal(await backend[other]({ ...input, revision: 4 }), null);
			assert.equal(requests.length, 4);
			assert.equal(
				requests.filter((r) => r.query === ops.SAVE_PROGRESS || r.query === ops.SAVE_CHAPTER_READ)
					.length,
				1,
			);
		},
	);
}

test('legacy chapter-read unavailability throws so the optimistic toggle can be undone', async () => {
	const { backend } = client(({ query }) => ({
		errors: [
			{
				message:
					'Unknown field "' +
					(query === ops.SAVE_CHAPTER_READ ? 'saveChapterRead' : 'setChapterRead') +
					'"',
			},
		],
	}));
	await assert.rejects(
		() => backend.saveChapterRead({ chapterId: 'copy', workId, read: false, revision: 0 }),
		/Chapter read state is unavailable on this server\./,
	);
});

test('viewerProgress missing or null uses base zero and missing capability is remembered', async () => {
	for (const unsupported of [false, true]) {
		const { backend, requests } = client(({ query }) => {
			if (query === ops.VIEWER_PROGRESS) {
				return unsupported
					? { errors: [{ message: 'Unknown field "viewerProgress"' }] }
					: { data: { viewerProgress: null } };
			}
			return { data: { saveProgress: { status: 'REFUSED', revision: 9, workId } } };
		});
		await backend.saveProgress(input);
		assert.equal(requests[1].variables.revision, 0);
		assert.equal(backend.knownRevision(workId), undefined);
		if (unsupported) {
			assert.equal(await backend.viewerProgress(workId), undefined);
			assert.equal(requests.length, 2);
		}
	}
});

test('fetch views share revisions, token changes clear them, and new auth cells start empty', async () => {
	const { backend } = client(() => ({ data: { viewerProgress: progress } }));
	const view = backend.withFetch(async () => Response.json({ data: { viewerProgress: progress } }));
	await view.viewerProgress!(workId);
	assert.equal(backend.knownRevision(workId), 7);
	backend.setToken('token-A');
	assert.equal(view.knownRevision!(workId), 7);
	backend.setToken('token-B');
	assert.equal(backend.knownRevision(workId), undefined);
	assert.equal(view.knownRevision!(workId), undefined);
	await backend.viewerProgress(workId);
	view.setToken!(null);
	assert.equal(backend.knownRevision(workId), undefined);
	const fresh = client(() => assert.fail('no request')).backend;
	assert.equal(fresh.knownRevision(workId), undefined);
});

test('a delayed response from another account cannot seed the revision cache', async () => {
	let resolve!: (value: unknown) => void;
	const { backend } = client(
		() =>
			new Promise((done) => {
				resolve = done;
			}),
	);
	const pending = backend.viewerProgress(workId);
	backend.setToken('token-B');
	resolve({ data: { viewerProgress: progress } });
	await assert.rejects(pending, /Session changed/);
	assert.equal(backend.knownRevision(workId), undefined);
});

for (const fields of [['viewerProgress'], ['metadata'], ['metadata', 'viewerProgress']]) {
	test('nested optional stripping preserves the document for ' + fields.join(' and '), async () => {
		const { backend, requests } = client(() =>
			requests.length === 1
				? { errors: fields.map((field) => ({ message: 'Unknown field "' + field + '"' })) }
				: { data: { seriesPage: { ...bundle, viewerProgress: undefined } } },
		);
		await backend.seriesPage(workId);
		assert.equal(requests.length, 2);
		let expected = ops.SERIES_PAGE;
		for (const field of fields) {
			expected = expected.replace(
				new RegExp(`^[\\t ]*${field}[\\t ]*\\{(?:[^{}]|\\{[^{}]*\\})*\\}[\\t ]*\\r?\\n`, 'gm'),
				'',
			);
		}
		assert.equal(requests[1].query, expected, 'all other fields remain byte-for-byte intact');
		const source = expected.replace(/#[^\n]*/g, '');
		assert.doesNotMatch(source, /\{\s*\}/);
		let depth = 0;
		for (const char of source) {
			if (char === '{') depth++;
			if (char === '}') depth--;
			assert.ok(depth >= 0);
		}
		assert.equal(depth, 0);
		await backend.seriesPage(workId);
		assert.equal(requests[2].query, expected, 'later calls omit the same blocks without a retry');
	});
}

test('chapterWork preserves nullable results and remembers an unknown field', async () => {
	const current = { workId, entryId: 'entry-1' };
	const { backend, requests } = client(({ variables }) => ({
		data: { chapterWork: variables.chapterId === 'copy' ? current : null },
	}));
	assert.deepEqual(await backend.chapterWork('copy'), current);
	assert.equal(await backend.chapterWork('missing'), null);
	assert.deepEqual(requests[0], { query: ops.CHAPTER_WORK, variables: { chapterId: 'copy' } });
	const old = client(() => ({ errors: [{ message: 'Unknown field "chapterWork"' }] }));
	assert.equal(await old.backend.chapterWork('copy'), undefined);
	assert.equal(await old.backend.chapterWork('copy'), undefined);
	assert.equal(old.requests.length, 1);
});

test('chapter relinking and merging send their exact ids, including a null entry', async () => {
	const { backend, requests } = client(({ query }) => ({
		data:
			query === ops.LINK_CHAPTER_COPY ? { linkChapterCopy: true } : { mergeWorkChapters: false },
	}));
	assert.equal(await backend.linkChapterCopy('copy', null), true);
	assert.equal(await backend.mergeWorkChapters('keep', 'drop'), false);
	assert.deepEqual(requests, [
		{ query: ops.LINK_CHAPTER_COPY, variables: { chapterId: 'copy', entryId: null } },
		{ query: ops.MERGE_WORK_CHAPTERS, variables: { keepId: 'keep', dropId: 'drop' } },
	]);
});

test('only live numeric revisioned saves notify about a stale epoch', async (t) => {
	let notifications = 0;
	t.after(
		onStaleNumericEpoch(() => {
			notifications++;
		}),
	);
	for (const method of ['saveProgress', 'saveChapterRead'] as const) {
		const { backend } = client(() => ({
			errors: [{ message: 'Stale epoch', extensions: { code: 'STALE_NUMERIC_EPOCH' } }],
		}));
		await assert.rejects(() => backend[method]({ ...input, revision: 0 }));
		const before = notifications;
		await assert.rejects(() => backend[method]({ ...input, numericEpoch: undefined, revision: 0 }));
		assert.equal(notifications, before + 1);
		await assert.rejects(() => backend[method]({ ...input, numericEpoch: undefined, revision: 0 }));
		assert.equal(notifications, before + 1, 'one notification per auth cell');
	}
});

test('overlapping saves of one work go out in turn and the second sends the first answer', async () => {
	const conflicts: ProgressConflict[] = [];
	const stop = onProgressConflict((conflict) => conflicts.push(conflict));
	let stored = 7;
	let inFlight = 0;
	let maxInFlight = 0;
	const { backend, requests } = client(async ({ query, variables }) => {
		if (query === ops.VIEWER_PROGRESS) return { data: { viewerProgress: progress } };
		inFlight += 1;
		maxInFlight = Math.max(maxInFlight, inFlight);
		await new Promise((resolve) => setTimeout(resolve, 5));
		inFlight -= 1;
		// The server's rule: an equal revision applies and moves it; a stale finish is refused.
		const status = variables.revision === stored ? 'APPLIED' : 'REFUSED';
		if (status === 'APPLIED') stored += 1;
		return { data: { saveProgress: { status, revision: stored, workId } } };
	});
	await backend.viewerProgress(workId);
	const [finish, leave] = await Promise.all([
		backend.saveProgress({ ...input, lastPageRead: 19, read: true }),
		backend.saveProgress({ ...input, lastPageRead: 19, read: true }),
	]);
	stop();
	assert.equal(maxInFlight, 1);
	assert.equal(finish?.status, 'APPLIED');
	assert.equal(leave?.status, 'APPLIED');
	assert.deepEqual(
		requests.filter((r) => r.query === ops.SAVE_PROGRESS).map((r) => r.variables.revision),
		[7, 8],
	);
	assert.deepEqual(conflicts, []);
});

test('a save waiting behind another never goes out after the sign-in changed', async () => {
	let release!: () => void;
	const gate = new Promise<void>((resolve) => (release = resolve));
	const { backend, requests } = client(async ({ query }) => {
		if (query === ops.VIEWER_PROGRESS) return { data: { viewerProgress: progress } };
		await gate;
		return { data: { saveProgress: { status: 'APPLIED', revision: 8, workId } } };
	});
	await backend.viewerProgress(workId);
	const first = backend.saveProgress(input);
	const second = backend.saveProgress(input);
	await new Promise((resolve) => setTimeout(resolve, 5));
	backend.setToken('token-B');
	release();
	await first.catch(() => {});
	await assert.rejects(second, /Session changed/);
	assert.equal(requests.filter((r) => r.query === ops.SAVE_PROGRESS).length, 1);
});

test('a save of the old account that never answers does not hold up the next account', async () => {
	const { backend, requests } = client(async ({ query, variables }) => {
		if (query === ops.VIEWER_PROGRESS) return { data: { viewerProgress: progress } };
		if (variables.lastPageRead === 1) return new Promise(() => {});
		return { data: { saveProgress: { status: 'APPLIED', revision: 8, workId } } };
	});
	await backend.viewerProgress(workId);
	void backend.saveProgress({ ...input, lastPageRead: 1 });
	await new Promise((resolve) => setTimeout(resolve, 5));
	backend.setToken('token-B');
	const result = await backend.saveProgress(input);
	assert.equal(result?.status, 'APPLIED');
	assert.equal(requests.filter((r) => r.query === ops.SAVE_PROGRESS).length, 2);
});
