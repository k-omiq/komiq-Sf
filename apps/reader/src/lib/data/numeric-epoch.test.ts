import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';
import type { Backend } from '../../../../../packages/api/src/backend.ts';

registerHooks({
	load(url, context, next) {
		if (/\/packages\/api\/src\/(composite-backend|offline-queue|graphql-backend)\.ts$/.test(url)) {
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

const { CompositeBackend } = await import('../../../../../packages/api/src/composite-backend.ts');
const { OfflineWriteQueue, createMemoryStorageAdapter } =
	await import('../../../../../packages/api/src/offline-queue.ts');
const { GraphQLBackend } = await import('../../../../../packages/api/src/graphql-backend.ts');
const { GraphQLRequestError } = await import('../../../../../packages/api/src/partial-errors.ts');

type Write = { chapterId: string; epoch: number | null | undefined };

/** A server whose engine generation can change: numeric writes from another epoch are
 *  refused the way the real one refuses them (`STALE_NUMERIC_EPOCH`). Its `numericEpoch`
 *  is kept after the first read, like `GraphQLBackend.numericEpoch`. */
function fakeServer() {
	const server = {
		epoch: 3,
		online: false,
		accepted: [] as Write[],
		sent: [] as Write[],
		epochReads: 0,
		known: undefined as number | undefined,
	};
	const hosted = {
		setToken: () => {},
		numericEpoch: async () => {
			if (server.known !== undefined) return server.known;
			if (!server.online) throw new Error('offline');
			server.epochReads++;
			return (server.known = server.epoch);
		},
		setProgress: async (chapterId: string, _page: number, _read: boolean, epoch?: number | null) => {
			server.sent.push({ chapterId, epoch });
			if (!server.online) throw new Error('offline');
			if (/^[0-9]+$/.test(chapterId) && epoch !== server.epoch) {
				throw new GraphQLRequestError([
					{
						message: 'Numeric reading progress belongs to an earlier engine generation',
						extensions: { code: 'STALE_NUMERIC_EPOCH' },
					},
				]);
			}
			server.accepted.push({ chapterId, epoch });
		},
	} as unknown as Backend;
	return { server, hosted };
}

function captureWarnings(t: { after: (fn: () => void) => void }) {
	const lines: string[] = [];
	const original = console.warn;
	console.warn = (...args: unknown[]) => void lines.push(args.map(String).join(' '));
	t.after(() => {
		console.warn = original;
	});
	return lines;
}

function signedIn(hosted: Backend) {
	const writes = new OfflineWriteQueue(createMemoryStorageAdapter());
	const composite = new CompositeBackend({ hosted, queue: writes });
	composite.setToken('token-A');
	composite.setCurrentUser('A');
	return { writes, composite };
}

test('a write queued before an engine change is dropped on replay, never filed on the new chapter', async (t) => {
	const warnings = captureWarnings(t);
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);

	// Read the epoch while online (as the reader does on its first numeric write).
	server.online = true;
	await composite.setProgress('41', 2, false);
	assert.deepEqual(server.accepted, [{ chapterId: '41', epoch: 3 }]);

	// Offline: the write is queued with the epoch known at queue time.
	server.online = false;
	await composite.setProgress('42', 5, true);
	assert.deepEqual(
		writes.peekAll().map((op) => op.kind === 'progress' && [op.chapterId, op.numericEpoch]),
		[['42', 3]],
	);

	// The operator starts a new engine generation; chapter 42 may now be another chapter.
	server.epoch = 4;
	server.online = true;
	await composite.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	// Let the drain the success started finish.
	await new Promise((resolve) => setTimeout(resolve, 0));

	assert.equal(writes.size(), 0, 'the stale write left the queue');
	assert.ok(
		!server.accepted.some((w) => w.chapterId === '42'),
		'the stale write was never accepted',
	);
	assert.deepEqual(
		server.sent.filter((w) => w.chapterId === '42').map((w) => w.epoch),
		[3, 3],
		'replayed once with its own epoch, never re-read and never retried',
	);
	assert.equal(server.epochReads, 1, 'the replay did not read the new epoch');
	assert.ok(warnings.some((line) => line.includes('earlier engine generation') && line.includes('42')));
});

test('a live write refused as stale is dropped, not queued', async (t) => {
	const warnings = captureWarnings(t);
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);
	server.online = true;
	await composite.setProgress('7', 1, false);
	server.epoch = 4; // changed while the app kept its old ids and epoch
	await composite.setProgress('7', 2, false);
	assert.equal(writes.size(), 0);
	assert.deepEqual(server.accepted, [{ chapterId: '7', epoch: 3 }]);
	assert.equal(warnings.length, 1);
});

test('a write queued with no known epoch replays with none, never with the current one', async () => {
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);
	// Offline from the start: the epoch was never read.
	await composite.setProgress('42', 5, true);
	const [op] = writes.peekAll();
	assert.equal(op.kind === 'progress' && op.numericEpoch, null);
	server.online = true;
	await composite.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	await new Promise((resolve) => setTimeout(resolve, 0));
	assert.deepEqual(
		server.sent.filter((w) => w.chapterId === '42').map((w) => w.epoch),
		[null, null],
		'sent without an epoch, so the server decides as it does for an old app',
	);
	assert.equal(writes.size(), 0, 'the server refused it as stale, so it was dropped');
});

test('an ordinary failure still stays queued for retry', async () => {
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);
	server.online = true;
	await composite.setProgress('41', 1, false);
	server.online = false;
	await composite.setProgress('42', 5, true);
	await composite.setProgress('43', 1, false);
	assert.deepEqual(
		writes.peekAll().map((op) => op.kind === 'progress' && [op.chapterId, op.numericEpoch, op.tries]),
		[
			['42', 3, 0],
			['43', 3, 0],
		],
	);
});

test('an op persisted before epochs existed replays with no epoch', async () => {
	const { server, hosted } = fakeServer();
	const storage = createMemoryStorageAdapter();
	storage.save(
		JSON.stringify([
			{ kind: 'progress', chapterId: '42', lastPageRead: 5, read: true, ts: 1, tries: 0, userId: 'A' },
		]),
	);
	const writes = new OfflineWriteQueue(storage);
	const composite = new CompositeBackend({ hosted, queue: writes });
	server.online = true;
	composite.setToken('token-A');
	composite.setCurrentUser('A');
	await composite.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	await new Promise((resolve) => setTimeout(resolve, 0));
	assert.deepEqual(
		server.sent.filter((w) => w.chapterId === '42').map((w) => w.epoch),
		[null],
	);
});

// --- GraphQLBackend: how the epoch reaches the wire ---

let endpointNumber = 0;
const endpoint = () => 'https://epoch-' + ++endpointNumber + '.test/graphql';

function recordingBackend(answer: (body: { query: string; variables?: Record<string, unknown> }) => unknown) {
	const bodies: { query: string; variables?: Record<string, unknown> }[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		fetch: async (_url, init) => {
			const body = JSON.parse(String(init?.body));
			bodies.push(body);
			return Response.json(answer(body));
		},
	});
	return { backend, bodies };
}

const isEpochRead = (b: { query: string }) => /query NumericEpoch/.test(b.query);

test('numeric writes carry one epoch read; uuid writes read and send none', async () => {
	const { backend, bodies } = recordingBackend((b) =>
		isEpochRead(b) ? { data: { numericEpoch: 9 } } : { data: { setProgress: true, setChapterRead: true } },
	);
	await backend.setProgress('12', 3, false);
	await backend.setProgress('13', 1, true);
	await backend.setChapterRead('14', true);
	await backend.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	assert.equal(bodies.filter(isEpochRead).length, 1, 'read once, then kept');
	const writes = bodies.filter((b) => !isEpochRead(b));
	assert.deepEqual(
		writes.map((b) => b.variables?.numericEpoch),
		[9, 9, 9, undefined],
	);
});

test('a replayed write sends exactly the epoch it was queued with, or none for null', async () => {
	const { backend, bodies } = recordingBackend((b) =>
		isEpochRead(b) ? { data: { numericEpoch: 9 } } : { data: { setProgress: true } },
	);
	await backend.setProgress('12', 3, false, 4);
	await backend.setProgress('12', 3, false, null);
	assert.equal(bodies.filter(isEpochRead).length, 0, 'a replay never reads the current epoch');
	assert.deepEqual(
		bodies.map((b) => b.variables?.numericEpoch),
		[4, undefined],
	);
});

test('an old server: the epoch field is unknown and the argument is stripped', async () => {
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) return { errors: [{ message: 'Unknown field "numericEpoch" on type "QueryRoot".' }] };
		if (/numericEpoch/.test(b.query))
			return {
				errors: [{ message: 'Unknown argument "numericEpoch" on field "setProgress" of type "MutationRoot".' }],
			};
		return { data: { setProgress: true } };
	});
	await backend.setProgress('12', 3, false); // epoch unknown here: null, sent as none
	await backend.setProgress('12', 4, false, 2); // a replay with an epoch, on an old server
	const writes = bodies.filter((b) => !isEpochRead(b));
	assert.equal(bodies.filter(isEpochRead).length, 1, 'an old server is asked once');
	assert.equal(writes.at(-1)!.query.includes('numericEpoch'), false, 'stripped for an old server');
	assert.equal(writes.length, 3, 'one strip-and-retry, then the stripped form');
});

test('a stale refusal surfaces with its code', async () => {
	const { isStaleNumericEpoch } = await import('../../../../../packages/api/src/numeric-epoch.ts');
	const { backend } = recordingBackend((b) =>
		isEpochRead(b)
			? { data: { numericEpoch: 1 } }
			: {
					errors: [
						{
							message: 'Numeric reading progress belongs to an earlier engine generation',
							extensions: { code: 'STALE_NUMERIC_EPOCH' },
						},
					],
				},
	);
	await assert.rejects(backend.setProgress('12', 3, false), (e: unknown) => isStaleNumericEpoch(e));
	assert.equal(isStaleNumericEpoch(new Error('offline')), false);
});
