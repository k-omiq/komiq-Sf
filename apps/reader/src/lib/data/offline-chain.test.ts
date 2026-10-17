import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';
import type {
	EnqueueOp,
	FlushResult,
	ProgressOp,
	StorageAdapter,
	WriteOp,
} from '../../../../../packages/api/src/offline-queue.ts';

registerHooks({
	load(url, context, next) {
		if (/\/packages\/api\/src\/offline-queue\.ts$/.test(url)) {
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

const { OfflineWriteQueue, MAX_TRIES } =
	await import('../../../../../packages/api/src/offline-queue.ts');

function deferred<T = void>() {
	let resolve!: (value: T) => void;
	let reject!: (error: Error) => void;
	const promise = new Promise<T>((yes, no) => {
		resolve = yes;
		reject = no;
	});
	return { promise, resolve, reject };
}

function queue(initial?: WriteOp[]) {
	let stored = initial ? JSON.stringify(initial) : null;
	const saves: WriteOp[][] = [];
	const adapter: StorageAdapter = {
		load: () => stored,
		save(value) {
			stored = value;
			saves.push(JSON.parse(value));
		},
	};
	return { writes: new OfflineWriteQueue(adapter), adapter, saves };
}

type ProgressInput = Extract<EnqueueOp, { kind: 'progress' }>;

function progress(chapterId: string, options: Partial<ProgressInput> = {}): ProgressInput {
	return {
		kind: 'progress',
		chapterId,
		lastPageRead: 1,
		read: false,
		userId: 'A',
		workId: 'work',
		revision: 3,
		...options,
	};
}

function progressOps(writes: InstanceType<typeof OfflineWriteQueue>): ProgressOp[] {
	return writes.peekAll().map((op) => {
		assert.equal(op.kind, 'progress');
		if (op.kind !== 'progress') throw new Error('wrong fixture');
		return op;
	});
}

test('enqueue persists explicit chain fields and a restart preserves the same identities', () => {
	const { writes, adapter, saves } = queue();
	writes.enqueue(progress('one'));
	writes.enqueue(progress('two', { revision: 4 }));
	writes.enqueue(progress('untracked', { workId: undefined, revision: undefined }));
	const [one, two, untracked] = progressOps(writes);
	assert.ok(one.opId);
	assert.ok(two.opId);
	assert.ok(untracked.opId);
	assert.equal(new Set([one.opId, two.opId, untracked.opId]).size, 3);
	assert.equal(one.workId, 'work');
	assert.equal(one.revision, 3);
	assert.equal(one.chainedTo, null);
	assert.equal(two.workId, 'work');
	assert.equal(two.revision, 4);
	assert.equal(two.chainedTo, one.opId);
	assert.equal(untracked.workId, null);
	assert.equal(untracked.revision, null);
	assert.equal(untracked.chainedTo, null);
	assert.deepEqual(saves.at(-1), [one, two, untracked]);
	const restarted = new OfflineWriteQueue(adapter);
	assert.deepEqual(restarted.peekAll(), [one, two, untracked]);
	assert.equal(saves.length, 3, 'loading a queue does not save it');
});

test('enqueue chains to the newest progress of the same work and user only', () => {
	const { writes } = queue();
	writes.enqueue(progress('one'));
	writes.enqueue(progress('two'));
	const [one, two] = progressOps(writes);
	assert.equal(two.chainedTo, one.opId);
	writes.enqueue(progress('untracked', { workId: null }));
	writes.enqueue(progress('other-work', { workId: 'other' }));
	writes.enqueue(progress('other-user', { userId: 'B' }));
	writes.enqueue({ kind: 'mark', seriesId: 'work', marked: true, userId: 'A' });
	writes.enqueue(progress('three'));
	const ops = writes.peekAll();
	for (const op of ops.slice(2, 5)) {
		assert.equal(op.kind === 'progress' && op.chainedTo, null);
	}
	const newest = ops.at(-1);
	assert.equal(newest?.kind === 'progress' && newest.chainedTo, two.opId);
});

test('an applied acknowledgement advances only direct successors in one storage save', async () => {
	for (const revision of [7, 0]) {
		const { writes, saves } = queue();
		writes.enqueue(progress('one'));
		writes.enqueue(progress('two', { revision: 9 }));
		writes.enqueue(progress('three', { revision: 4 }));
		const [, two, three] = progressOps(writes);
		const before = saves.length;
		const entered = deferred(),
			stalled = deferred<FlushResult | void>();
		const sent: WriteOp[] = [];
		const draining = writes.drain(async (op) => {
			sent.push({ ...op });
			if (sent.length === 1) return { appliedRevision: revision };
			if (sent.length === 2) {
				entered.resolve();
				return stalled.promise;
			}
		}, 'A');
		await entered.promise;
		const advanced = { ...two, revision, chainedTo: null };
		assert.deepEqual(writes.peekAll(), [advanced, three]);
		assert.deepEqual(sent[1], advanced, 'the next flush receives the applied revision');
		assert.equal(saves.length, before + 1);
		assert.deepEqual(saves.at(-1), [advanced, three]);
		stalled.resolve({ appliedRevision: 8 });
		await draining;
		assert.deepEqual(sent[2], { ...three, revision: 8, chainedTo: null });
		assert.equal(saves.length, before + 3);
		assert.deepEqual(saves.at(-1), []);
	}
});

test('void and null acknowledgements release successors without changing their base revision', async () => {
	for (const result of [undefined, { appliedRevision: null }] satisfies (FlushResult | void)[]) {
		for (const revision of [5, null]) {
			const { writes, saves } = queue();
			writes.enqueue(progress('one'));
			writes.enqueue(progress('two', { revision }));
			const [, two] = progressOps(writes);
			const before = saves.length;
			const entered = deferred(),
				stalled = deferred();
			let calls = 0;
			const draining = writes.drain(async () => {
				if (++calls === 1) return result;
				entered.resolve();
				return stalled.promise;
			}, 'A');
			await entered.promise;
			const expected = [{ ...two, chainedTo: null }];
			assert.deepEqual(writes.peekAll(), expected);
			assert.equal(saves.length, before + 1);
			assert.deepEqual(saves.at(-1), expected);
			stalled.resolve();
			await draining;
		}
	}
});

test('collapsing a middle op hands its dependency and maximum base revision to its successor', () => {
	for (const [removedRevision, successorRevision, expectedRevision] of [
		[8, 4, 8],
		[4, 8, 8],
		[null, 4, 4],
		[4, null, 4],
		[null, null, null],
		[0, null, 0],
		[-2, null, -2],
	]) {
		const { writes, saves } = queue();
		writes.enqueue(progress('head'));
		writes.enqueue(progress('middle', { revision: removedRevision }));
		writes.enqueue(progress('successor', { revision: successorRevision }));
		const [head, middle, successor] = progressOps(writes);
		assert.equal(successor.chainedTo, middle.opId);
		const before = saves.length;
		writes.enqueue(progress('middle', { revision: 1 }));
		const [retained, handedOver, replacement] = progressOps(writes);
		assert.deepEqual(retained, head);
		assert.deepEqual(handedOver, {
			...successor,
			chainedTo: head.opId,
			revision: expectedRevision,
		});
		assert.equal(replacement.chapterId, 'middle');
		assert.notEqual(replacement.opId, middle.opId);
		assert.equal(replacement.chainedTo, successor.opId);
		assert.equal(replacement.revision, 1);
		assert.equal(saves.length, before + 1);
		assert.deepEqual(saves.at(-1), [retained, handedOver, replacement]);
	}
});

test('collapsing a pending head releases its successor and keeps the greater base revision', () => {
	const { writes, saves } = queue();
	writes.enqueue(progress('head', { revision: 8 }));
	writes.enqueue(progress('successor', { revision: 4 }));
	const [, successor] = progressOps(writes);
	writes.enqueue(progress('head'));
	const [handedOver, replacement] = progressOps(writes);
	assert.deepEqual(handedOver, { ...successor, chainedTo: null, revision: 8 });
	assert.equal(replacement.chainedTo, successor.opId);
	assert.deepEqual(saves.at(-1), [handedOver, replacement]);
});

test('an in-flight work op survives same-chapter collapse and hands over its applied revision', async () => {
	const { writes, saves } = queue();
	writes.enqueue(progress('same-chapter'));
	const [head] = progressOps(writes);
	const entered = deferred(),
		stalled = deferred<FlushResult>(),
		nextEntered = deferred(),
		nextStalled = deferred();
	const sent: WriteOp[] = [];
	const draining = writes.drain(async (op) => {
		sent.push({ ...op });
		if (sent.length === 1) {
			entered.resolve();
			return stalled.promise;
		}
		nextEntered.resolve();
		return nextStalled.promise;
	}, 'A');
	await entered.promise;
	writes.enqueue(progress('same-chapter', { lastPageRead: 5 }));
	const [, superseded] = progressOps(writes);
	writes.enqueue(progress('same-chapter', { lastPageRead: 9, revision: 1 }));
	const [retained, replacement] = progressOps(writes);
	assert.deepEqual(retained, head);
	assert.notEqual(replacement.opId, superseded.opId);
	assert.equal(replacement.chainedTo, head.opId);
	assert.equal(replacement.lastPageRead, 9);
	const before = saves.length;
	stalled.resolve({ appliedRevision: 7 });
	await nextEntered.promise;
	const expected = { ...replacement, revision: 7, chainedTo: null };
	assert.deepEqual(sent, [head, expected]);
	assert.deepEqual(writes.peekAll(), [expected]);
	assert.equal(saves.length, before + 1);
	assert.deepEqual(saves.at(-1), [expected]);
	nextStalled.resolve();
	await draining;
	assert.equal(writes.size(), 0);
});

test('failed flushes preserve the chain and retries resume at the same head', async () => {
	const { writes, saves } = queue();
	writes.enqueue(progress('one'));
	writes.enqueue(progress('two', { revision: 4 }));
	const [one, two] = progressOps(writes);
	const sent: WriteOp[] = [];
	for (let tries = 1; tries <= 2; tries++) {
		const before = saves.length;
		await writes.drain(async (op) => {
			sent.push({ ...op });
			throw new Error('offline');
		}, 'A');
		const expected = [{ ...one, tries }, two];
		assert.deepEqual(writes.peekAll(), expected);
		assert.equal(saves.length, before + 1);
		assert.deepEqual(saves.at(-1), expected);
	}
	await writes.drain(async (op) => {
		sent.push({ ...op });
		return { appliedRevision: 7 };
	}, 'A');
	assert.deepEqual(sent, [
		one,
		{ ...one, tries: 1 },
		{ ...one, tries: 2 },
		{ ...two, revision: 7, chainedTo: null },
	]);
	assert.equal(writes.size(), 0);
});

test('poison drop releases its successor in the same storage save and preserves its revision', async (t) => {
	const warnings: unknown[][] = [];
	t.mock.method(console, 'warn', (...args: unknown[]) => void warnings.push(args));
	const { writes, saves } = queue();
	writes.enqueue(progress('poison'));
	writes.enqueue(progress('successor', { revision: 4 }));
	const [poison, successor] = progressOps(writes);
	for (let tries = 1; tries < MAX_TRIES; tries++) {
		await writes.drain(async () => {
			throw new Error('offline');
		}, 'A');
		assert.deepEqual(writes.peekAll(), [{ ...poison, tries }, successor]);
	}
	const before = saves.length;
	const entered = deferred(),
		stalled = deferred();
	const sent: WriteOp[] = [];
	const draining = writes.drain(async (op) => {
		sent.push({ ...op });
		if (op.kind === 'progress' && op.chapterId === 'poison') throw new Error('offline');
		entered.resolve();
		return stalled.promise;
	}, 'A');
	await entered.promise;
	const released = { ...successor, chainedTo: null };
	assert.deepEqual(sent, [{ ...poison, tries: MAX_TRIES - 1 }, released]);
	assert.deepEqual(writes.peekAll(), [released]);
	assert.equal(saves.length, before + 1);
	assert.deepEqual(saves.at(-1), [released]);
	assert.equal(warnings.length, 1);
	stalled.resolve();
	await draining;
	assert.equal(writes.size(), 0);
});

test('dropForeign and drain filtering clear missing predecessors in the removal save', async () => {
	// Persisted cross-user links exercise cleanup even if an older client wrote them.
	const initial: ProgressOp[] = [
		{ ...progress('A-one'), opId: 'A1', chainedTo: null, ts: 1, tries: 0 },
		{ ...progress('A-two'), opId: 'A2', chainedTo: 'A1', ts: 2, tries: 0 },
		{ ...progress('B-one', { userId: 'B' }), opId: 'B1', chainedTo: 'A2', ts: 3, tries: 0 },
		{ ...progress('B-two', { userId: 'B' }), opId: 'B2', chainedTo: 'B1', ts: 4, tries: 0 },
	];
	for (const method of ['dropForeign', 'drain']) {
		const { writes, saves } = queue(initial);
		const expected = [{ ...initial[2], chainedTo: null }, initial[3]];
		const entered = deferred(),
			stalled = deferred();
		let draining: Promise<void> | undefined;
		if (method === 'dropForeign') writes.dropForeign('B');
		else {
			draining = writes.drain(async () => {
				entered.resolve();
				return stalled.promise;
			}, 'B');
			await entered.promise;
		}
		assert.deepEqual(writes.peekAll(), expected, method);
		assert.equal(saves.length, 1, method);
		assert.deepEqual(saves[0], expected, method);
		writes.dropForeign('B');
		assert.equal(saves.length, 1, 'unchanged filtering does not save');
		writes.dropForeign(null);
		assert.deepEqual(writes.peekAll(), []);
		assert.equal(saves.length, 2);
		stalled.resolve();
		await draining;
		assert.equal(saves.length, 2, 'a late completion after logout cannot save');
	}
});

test('a cancelled replay keeps its late answer and the next drain waits instead of resending', async () => {
	const { writes, saves } = queue();
	writes.enqueue(progress('one'));
	writes.enqueue(progress('two'));
	const [one, two] = progressOps(writes);
	const oldEntered = deferred(),
		oldStalled = deferred<FlushResult>(),
		currentEntered = deferred(),
		currentStalled = deferred<FlushResult>();
	const old = writes.drain(async () => {
		oldEntered.resolve();
		return oldStalled.promise;
	}, 'A');
	await oldEntered.promise;
	writes.cancelReplay();
	const sent: ProgressOp[] = [];
	const current = writes.drain(async (op) => {
		sent.push({ ...(op as ProgressOp) });
		if (sent.length === 1) {
			currentEntered.resolve();
			return currentStalled.promise;
		}
	}, 'A');
	await new Promise<void>((resolve) => setImmediate(resolve));
	assert.deepEqual(sent.slice(), [], 'the op still on the wire is not sent a second time');
	const before = saves.length;
	oldStalled.resolve({ appliedRevision: 99 });
	await old;
	await currentEntered.promise;
	assert.equal(saves.length, before + 1, 'removal and hand-over are one storage write');
	assert.deepEqual(sent.slice(), [{ ...two, chainedTo: null, revision: 99 }]);
	// The new drain's in-flight op survives collapse; the late old drain did not clear it.
	writes.enqueue(progress('two', { lastPageRead: 9 }));
	const [retained, replacement] = progressOps(writes);
	assert.equal(retained.opId, two.opId);
	assert.equal(replacement.chainedTo, two.opId);
	assert.notEqual(retained.opId, one.opId);
	currentStalled.resolve({ appliedRevision: 100 });
	await current;
	assert.equal(sent.length, 2);
	assert.equal(sent[1].revision, 100);
	assert.equal(writes.size(), 0);
});

test('legacy progress gets an identity in memory, replays unchanged and never joins a chain', async () => {
	const legacy: ProgressOp = {
		kind: 'progress',
		chapterId: 'legacy',
		lastPageRead: 5,
		read: true,
		ts: 1,
		tries: 0,
		userId: 'A',
	};
	const { writes, adapter, saves } = queue([legacy]);
	const [loaded] = progressOps(writes);
	assert.ok(loaded.opId);
	assert.deepEqual(loaded, { ...legacy, opId: loaded.opId });
	assert.equal(saves.length, 0);
	assert.deepEqual(JSON.parse(adapter.load()!), [legacy]);
	writes.enqueue(progress('known'));
	writes.enqueue(progress('untracked', { workId: null }));
	const [, known, untracked] = progressOps(writes);
	assert.equal(known.chainedTo, null);
	assert.equal(untracked.chainedTo, null);
	assert.deepEqual(saves[0], [loaded, known]);
	const sent: WriteOp[] = [];
	await writes.drain(async (op) => {
		sent.push({ ...op });
		return { appliedRevision: 7 };
	}, 'A');
	assert.deepEqual(sent, [loaded, known, untracked]);
	assert.equal(writes.size(), 0);
});

test('hasPendingProgress matches queued progress by both user and work', async () => {
	const { writes } = queue();
	assert.equal(writes.hasPendingProgress('A', 'work'), false);
	writes.enqueue({ kind: 'mark', seriesId: 'work', marked: true, userId: 'A' });
	writes.enqueue(progress('untracked', { workId: null }));
	assert.equal(writes.hasPendingProgress('A', 'work'), false);
	writes.enqueue(progress('owned'));
	assert.equal(writes.hasPendingProgress('A', 'work'), true);
	assert.equal(writes.hasPendingProgress('B', 'work'), false);
	assert.equal(writes.hasPendingProgress(null, 'work'), false);
	assert.equal(writes.hasPendingProgress('A', 'other'), false);
	writes.enqueue(progress('anonymous', { userId: null }));
	assert.equal(writes.hasPendingProgress(null, 'work'), true);
	writes.dropForeign('A');
	assert.equal(writes.hasPendingProgress(null, 'work'), false);
	await writes.drain(async () => {}, 'A');
	assert.equal(writes.hasPendingProgress('A', 'work'), false);
});

test('op identities use randomUUID when available and a unique fallback otherwise', (t) => {
	const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'crypto');
	t.after(() => {
		if (descriptor) Object.defineProperty(globalThis, 'crypto', descriptor);
		else Reflect.deleteProperty(globalThis, 'crypto');
	});
	Object.defineProperty(globalThis, 'crypto', {
		configurable: true,
		value: { randomUUID: () => 'generated-uuid' },
	});
	const { writes } = queue();
	writes.enqueue(progress('uuid'));
	assert.equal(progressOps(writes)[0].opId, 'generated-uuid');
	t.mock.method(Date, 'now', () => 1234567890);
	t.mock.method(Math, 'random', () => 0.5);
	for (const crypto of [undefined, {}]) {
		Object.defineProperty(globalThis, 'crypto', { configurable: true, value: crypto });
		writes.enqueue(progress(`fallback-${writes.size()}`));
	}
	const ids = progressOps(writes)
		.slice(1)
		.map((op) => op.opId!);
	assert.notEqual(ids[0], ids[1], 'the counter distinguishes equal timestamps and random values');
	for (const id of ids) assert.match(id, /^kf12oi-\d+-i$/);
});
