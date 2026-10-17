import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';
import type { Backend, SaveProgressInput } from '../../../../../packages/api/src/backend.ts';
import type { ProgressResult } from '../../../../../packages/types/src/index.ts';
import type { ProgressOp } from '../../../../../packages/api/src/offline-queue.ts';

registerHooks({
	load(url, context, next) {
		if (/\/packages\/api\/src\/(composite-backend|offline-queue)\.ts$/.test(url)) {
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
const { GraphQLRequestError } = await import('../../../../../packages/api/src/partial-errors.ts');

const workId = 'w_fixture';
const input = { chapterId: '42', workId, lastPageRead: 8, read: false };
const settle = () => new Promise<void>((resolve) => setImmediate(resolve));

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (error: Error) => void;
	const promise = new Promise<T>((yes, no) => {
		resolve = yes;
		reject = no;
	});
	return { promise, resolve, reject };
}

async function fixture() {
	const server = {
		online: false,
		known: 4 as number | undefined,
		sent: [] as SaveProgressInput[],
		legacy: [] as unknown[][],
		answer: async (save: SaveProgressInput): Promise<ProgressResult | null> => ({
			status: 'APPLIED',
			revision: (save.revision ?? 4) + 1,
			workId: save.workId,
		}),
	};
	const hosted = {
		setToken: () => {},
		numericEpoch: async () => 3,
		knownRevision: () => server.known,
		saveProgress: async (save: SaveProgressInput) => {
			server.sent.push({ ...save });
			if (!server.online) throw new Error('offline');
			return server.answer(save);
		},
		setProgress: async (...args: unknown[]) => {
			server.legacy.push(args);
		},
	} as unknown as Backend;
	const adapter = createMemoryStorageAdapter();
	const writes = new OfflineWriteQueue(adapter);
	const composite = new CompositeBackend({ hosted, queue: writes });
	composite.setToken('token-A');
	composite.setCurrentUser('A');
	await settle();
	const queued = () => writes.peekAll().filter((op): op is ProgressOp => op.kind === 'progress');
	const reconnect = async () => {
		server.online = true;
		await composite.saveProgress({
			...input,
			chapterId: 'trigger',
			workId: 'w_trigger',
			revision: 0,
		});
		await settle();
	};
	return { server, hosted, adapter, writes, composite, queued, reconnect };
}

test('an offline live save queues its work, known revision, owner and captured epoch', async () => {
	const { composite, server, writes, queued, adapter } = await fixture();
	assert.equal(await composite.saveProgress(input), null);
	const [op] = queued();
	assert.deepEqual(
		server.sent,
		[{ ...input, opId: op.opId }],
		'the live request keeps its epoch implicit and shares its id with the queued retry',
	);
	assert.equal(op.workId, workId);
	assert.equal(op.revision, 4);
	assert.equal(op.numericEpoch, 3);
	assert.equal(op.userId, 'A');
	assert.equal(op.chainedTo, null);
	assert.ok(op.opId);
	assert.deepEqual(JSON.parse(adapter.load()!), writes.peekAll());
});

test('explicit revisions are queued as given and an unknown revision stays null', async () => {
	for (const revision of [0, undefined]) {
		const { composite, server, queued } = await fixture();
		server.known = undefined;
		await composite.saveProgress({ ...input, revision, numericEpoch: null });
		assert.equal(queued()[0].revision, revision ?? null);
		assert.equal(queued()[0].numericEpoch, null);
	}
});

test('two saves of one work replay in order with the first APPLIED revision', async () => {
	const { composite, server, writes, queued, reconnect } = await fixture();
	await composite.saveProgress(input);
	await composite.saveProgress({ ...input, chapterId: '43', read: true });
	await settle();
	const [first, second] = queued();
	assert.equal(second.chainedTo, first.opId);
	server.sent.length = 0;
	server.answer = async (save) => ({
		status: 'APPLIED',
		revision: save.chapterId === '42' ? 17 : 18,
		workId: save.workId,
	});
	await reconnect();
	assert.deepEqual(
		server.sent.filter((save) => save.workId === workId),
		[
			{ ...input, numericEpoch: 3, revision: 4, opId: first.opId },
			{ ...input, chapterId: '43', read: true, numericEpoch: 3, revision: 17, opId: second.opId },
		],
	);
	assert.equal(writes.size(), 0);
});

for (const status of ['REFUSED', 'PAGE_ONLY', 'MOVED', null] as const) {
	test('a ' + status + ' replay releases its successor without replacing its base', async () => {
		const { composite, server, queued, writes, reconnect } = await fixture();
		await composite.saveProgress(input);
		await composite.saveProgress({ ...input, chapterId: '43', revision: 6 });
		await settle();
		assert.equal(queued().length, 2);
		server.sent.length = 0;
		server.answer = async (save) =>
			save.chapterId === '42'
				? status === null
					? null
					: { status, revision: 99, workId: 'w_elsewhere' }
				: { status: 'APPLIED', revision: 7, workId: save.workId };
		await reconnect();
		assert.deepEqual(
			server.sent
				.filter((save) => save.workId === workId)
				.map((save) => [save.chapterId, save.revision]),
			[
				['42', 4],
				['43', 6],
			],
		);
		assert.equal(writes.size(), 0);
	});
}

test('a live save joins a pending work and cannot overtake its in-flight predecessor', async () => {
	const { composite, server, writes, queued } = await fixture();
	await composite.saveProgress(input);
	server.online = true;
	server.sent.length = 0;
	const entered = deferred<void>();
	const stalled = deferred<ProgressResult>();
	server.answer = async (save) => {
		if (save.chapterId === '42') {
			entered.resolve();
			return stalled.promise;
		}
		return { status: 'APPLIED', revision: 11, workId };
	};
	assert.equal(await composite.saveProgress({ ...input, chapterId: '43' }), null);
	await entered.promise;
	assert.equal(server.sent.length, 1);
	assert.equal(server.sent[0].chapterId, '42');
	assert.equal(queued().length, 2);
	assert.equal(await composite.saveProgress({ ...input, chapterId: '44' }), null);
	assert.equal(server.sent.length, 1, 'neither new live save bypasses the queue');
	stalled.resolve({ status: 'APPLIED', revision: 10, workId });
	await settle();
	assert.deepEqual(
		server.sent.map((save) => [save.chapterId, save.revision]),
		[
			['42', 4],
			['43', 10],
			['44', 11],
		],
	);
	assert.equal(writes.size(), 0);
});

test('a live save started while an earlier one fails joins the backlog that failure leaves', async () => {
	const { composite, server, writes } = await fixture();
	server.online = true;
	let failed = false;
	server.answer = async (save) => {
		if (save.chapterId === '42' && !failed) {
			failed = true;
			throw new Error('network');
		}
		return { status: 'APPLIED', revision: (save.revision ?? 4) + 1, workId };
	};
	const first = composite.saveProgress({ ...input, read: true });
	const second = composite.saveProgress({ ...input, chapterId: '43' });
	assert.equal(await first, null);
	assert.equal(await second, null);
	await settle();
	assert.deepEqual(
		server.sent.map((save) => [save.chapterId, save.revision]),
		[
			['42', undefined],
			['42', 4],
			['43', 5],
		],
		'the second save waits and replays after the finish instead of overtaking it',
	);
	assert.equal(writes.size(), 0);
});

test('a chapter toggle sends the queued saves of its work first', async () => {
	const { composite, hosted, server, writes } = await fixture();
	const order: string[] = [];
	(hosted as { saveChapterRead?: unknown }).saveChapterRead = async (tick: SaveProgressInput) => {
		order.push('tick@' + tick.revision);
		return { status: 'APPLIED', revision: 6, workId };
	};
	await composite.saveProgress({ ...input, read: true });
	server.online = true;
	server.answer = async (save) => {
		order.push(save.chapterId + '@' + save.revision);
		return { status: 'APPLIED', revision: 5, workId };
	};
	const result = await composite.saveChapterRead({ chapterId: '2', workId, read: true });
	assert.equal(result?.status, 'APPLIED');
	assert.deepEqual(order, ['42@4', 'tick@undefined']);
	assert.equal(writes.size(), 0);
});

test('a chapter toggle fails while its work still has saves waiting', async () => {
	const { composite, hosted, writes } = await fixture();
	let ticks = 0;
	(hosted as { saveChapterRead?: unknown }).saveChapterRead = async () => {
		ticks += 1;
		return { status: 'APPLIED', revision: 6, workId };
	};
	await composite.saveProgress({ ...input, read: true });
	await assert.rejects(
		composite.saveChapterRead({ chapterId: '2', workId, read: true }),
		/still waiting to sync/,
	);
	assert.equal(ticks, 0);
	assert.equal(writes.size(), 1);
});

test('a save of an earlier sign-in that never answers does not hold up the next one', async () => {
	const { composite, server } = await fixture();
	server.online = true;
	server.answer = async (save) =>
		save.lastPageRead === 1 ? new Promise(() => {}) : { status: 'APPLIED', revision: 5, workId };
	void composite.saveProgress({ ...input, lastPageRead: 1 });
	await settle();
	composite.setToken(null);
	composite.setToken('token-A');
	const result = await composite.saveProgress(input);
	assert.equal(result?.status, 'APPLIED');
});

test('an account switch drops foreign chained ops and ignores the old replay result', async () => {
	const { composite, server, writes } = await fixture();
	await composite.saveProgress(input);
	const entered = deferred<void>();
	const stalled = deferred<ProgressResult>();
	server.online = true;
	server.sent.length = 0;
	server.answer = async () => {
		entered.resolve();
		return stalled.promise;
	};
	await composite.saveProgress({ ...input, chapterId: '43' });
	await entered.promise;
	composite.setToken('token-B');
	composite.setCurrentUser('B');
	assert.equal(writes.size(), 0);
	stalled.resolve({ status: 'APPLIED', revision: 100, workId });
	await settle();
	assert.deepEqual(
		server.sent.map((save) => save.chapterId),
		['42'],
	);
	assert.equal(writes.size(), 0);
});

test('late live failures cannot enqueue under another user or a renewed token', async () => {
	for (const userId of ['A', 'B']) {
		const { composite, server, writes } = await fixture();
		const stalled = deferred<ProgressResult>();
		server.online = true;
		server.answer = () => stalled.promise;
		const pending = composite.saveProgress(input);
		await settle();
		composite.setToken('renewed-token');
		composite.setCurrentUser(userId);
		stalled.reject(new Error('old request failed'));
		assert.equal(await pending, null);
		assert.equal(writes.size(), 0);
	}
});

test('old persisted progress without a work replays through setProgress unchanged', async () => {
	const legacy = {
		kind: 'progress',
		chapterId: '42',
		lastPageRead: 9,
		read: true,
		ts: 1,
		tries: 0,
		userId: 'A',
	};
	const adapter = createMemoryStorageAdapter();
	adapter.save(JSON.stringify([legacy]));
	const writes = new OfflineWriteQueue(adapter);
	const calls: unknown[][] = [];
	const hosted = {
		setProgress: async (...args: unknown[]) => {
			calls.push(args);
		},
		saveProgress: async () => assert.fail('legacy progress has no revision chain'),
	} as unknown as Backend;
	const composite = new CompositeBackend({ hosted, queue: writes });
	composite.setCurrentUser('A');
	await settle();
	assert.deepEqual(calls, [['42', 9, true, null]]);
	assert.equal(writes.size(), 0);
});

test('a stale epoch replay drops the predecessor and releases its successor', async (t) => {
	const { composite, server, writes, reconnect } = await fixture();
	const warnings: unknown[][] = [];
	t.mock.method(console, 'warn', (...args: unknown[]) => {
		warnings.push(args);
	});
	await composite.saveProgress(input);
	await composite.saveProgress({ ...input, chapterId: '43', revision: 6 });
	await settle();
	server.sent.length = 0;
	server.answer = async (save) => {
		if (save.chapterId === '42') {
			throw new GraphQLRequestError([
				{ message: 'Stale epoch', extensions: { code: 'STALE_NUMERIC_EPOCH' } },
			]);
		}
		return { status: 'APPLIED', revision: 7, workId: save.workId };
	};
	await reconnect();
	assert.deepEqual(
		server.sent
			.filter((save) => save.workId === workId)
			.map((save) => [save.chapterId, save.revision]),
		[
			['42', 4],
			['43', 6],
		],
	);
	assert.equal(writes.size(), 0);
	assert.equal(warnings.length, 1);
});

test('a live stale epoch is dropped without queueing it', async (t) => {
	const { composite, server, writes } = await fixture();
	t.mock.method(console, 'warn', () => {});
	server.online = true;
	server.answer = async () => {
		throw new GraphQLRequestError([
			{ message: 'Stale epoch', extensions: { code: 'STALE_NUMERIC_EPOCH' } },
		]);
	};
	assert.equal(await composite.saveProgress(input), null);
	assert.equal(writes.size(), 0);
});

test('without a queue revisioned writes delegate and propagate failures', async () => {
	const { hosted, server } = await fixture();
	const composite = new CompositeBackend({ hosted });
	await assert.rejects(() => composite.saveProgress(input), /offline/);
	server.online = true;
	assert.deepEqual(await composite.saveProgress(input), { status: 'APPLIED', revision: 5, workId });
});

test('composite delegates revision reads, chapter identity, chapter toggles and edits', async () => {
	const calls: unknown[][] = [];
	const progress = { revision: 9, throughEntryId: null };
	const chapter = { workId, entryId: null };
	const answer: ProgressResult = { status: 'APPLIED', revision: 10, workId };
	const hosted = {
		viewerProgress: async (...args: unknown[]) => {
			calls.push(args);
			return progress;
		},
		knownRevision: (...args: unknown[]) => {
			calls.push(args);
			return 9;
		},
		chapterWork: async (...args: unknown[]) => {
			calls.push(args);
			return chapter;
		},
		saveChapterRead: async (...args: unknown[]) => {
			calls.push(args);
			return answer;
		},
		linkChapterCopy: async (...args: unknown[]) => {
			calls.push(args);
			return true;
		},
		mergeWorkChapters: async (...args: unknown[]) => {
			calls.push(args);
			return false;
		},
	} as unknown as Backend;
	const composite = new CompositeBackend({ hosted });
	assert.deepEqual(await composite.viewerProgress(workId), progress);
	assert.equal(composite.knownRevision(workId), 9);
	assert.deepEqual(await composite.chapterWork('42'), chapter);
	assert.deepEqual(await composite.saveChapterRead(input), answer);
	assert.equal(await composite.linkChapterCopy('42', null), true);
	assert.equal(await composite.mergeWorkChapters('keep', 'drop'), false);
	assert.deepEqual(calls, [[workId], [workId], ['42'], [input], ['42', null], ['keep', 'drop']]);
});
