import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';
import type { Backend } from '../../../../../packages/api/src/backend.ts';
import type { Series } from '@komika/types';

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
const { OfflineWriteQueue, createMemoryStorageAdapter, MAX_TRIES } =
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

function queue() {
	return new OfflineWriteQueue(createMemoryStorageAdapter());
}

test('late A progress and mark failures cannot become B offline writes', async () => {
	for (const kind of ['progress', 'mark']) {
		const writes = queue();
		const stalled = deferred<Series>();
		const error = new Error('offline A request');
		const hosted = {
			setProgress: () => stalled.promise.then(() => {}),
			mark: () => stalled.promise,
			setToken: () => {},
		} as unknown as Backend; // This scenario supplies only the hosted methods it calls.
		const composite = new CompositeBackend({ hosted, queue: writes });
		composite.setToken('token-A');
		composite.setCurrentUser('A');
		const pending =
			kind === 'progress'
				? composite.setProgress('chapter-A', 7, true)
				: composite.mark('series-A', true);
		const result = pending.then(
			() => null,
			(rejection) => rejection,
		);
		composite.setToken('token-B');
		composite.setCurrentUser('B');
		stalled.reject(error);
		assert.equal(await result, kind === 'progress' ? null : error);
		assert.deepEqual(writes.peekAll(), [], kind);
	}
});

test('an old-token failure is discarded even when the user ID is unchanged', async () => {
	const writes = queue(),
		stalled = deferred<void>();
	const composite = new CompositeBackend({
		hosted: { setProgress: () => stalled.promise, setToken: () => {} } as unknown as Backend,
		queue: writes,
	});
	composite.setToken('old-A');
	composite.setCurrentUser('A');
	const pending = composite.setProgress('chapter-A', 7, true);
	composite.setToken('renewed-A');
	stalled.reject(new Error('old session revoked'));
	await pending;
	assert.deepEqual(writes.peekAll(), []);
});

test('a late old drain cannot remove the new account head or alter its retry count', async () => {
	for (const fails of [false, true]) {
		const writes = queue(),
			entered = deferred<void>(),
			stalled = deferred<void>();
		writes.enqueue({
			kind: 'progress',
			chapterId: 'chapter-A',
			lastPageRead: 7,
			read: true,
			userId: 'A',
		});
		const draining = writes.drain(async () => {
			entered.resolve();
			await stalled.promise;
		}, 'A');
		await entered.promise;
		writes.dropForeign('B');
		writes.enqueue({
			kind: 'progress',
			chapterId: 'chapter-B',
			lastPageRead: 1,
			read: false,
			userId: 'B',
		});
		if (fails) stalled.reject(new Error('old A request failed'));
		else stalled.resolve();
		await draining;
		assert.equal(writes.size(), 1);
		assert.equal(writes.peekAll()[0].userId, 'B');
		assert.equal(writes.peekAll()[0].tries, 0);
	}
});

test('the new account replays its own operation while the old request remains stalled', async () => {
	const writes = queue(),
		entered = deferred<void>(),
		stalled = deferred<void>();
	writes.enqueue({
		kind: 'progress',
		chapterId: 'chapter-A',
		lastPageRead: 7,
		read: true,
		userId: 'A',
	});
	const old = writes.drain(async () => {
		entered.resolve();
		await stalled.promise;
	}, 'A');
	await entered.promise;
	writes.dropForeign('B');
	writes.enqueue({
		kind: 'progress',
		chapterId: 'chapter-B',
		lastPageRead: 1,
		read: false,
		userId: 'B',
	});
	const sent: string[] = [];
	const current = writes.drain(async (op) => {
		sent.push(op.userId!);
	}, 'B');
	await current;
	assert.deepEqual(sent, ['B']);
	assert.equal(writes.size(), 0);
	stalled.resolve();
	await old;
	assert.deepEqual(sent, ['B']);
	assert.equal(writes.size(), 0);
});

test('replacing a pending head preserves and sends the newer progress state', async () => {
	const writes = queue(),
		entered = deferred<void>(),
		stalled = deferred<void>();
	writes.enqueue({
		kind: 'progress',
		chapterId: 'same-chapter',
		lastPageRead: 1,
		read: false,
		userId: 'A',
	});
	const sent: number[] = [];
	const draining = writes.drain(async (op) => {
		assert.equal(op.kind, 'progress');
		if (op.kind !== 'progress') throw new Error('wrong fixture');
		sent.push(op.lastPageRead);
		if (sent.length === 1) {
			entered.resolve();
			await stalled.promise;
		}
	}, 'A');
	await entered.promise;
	writes.enqueue({
		kind: 'progress',
		chapterId: 'same-chapter',
		lastPageRead: 9,
		read: true,
		userId: 'A',
	});
	stalled.resolve();
	await draining;
	assert.deepEqual(sent, [1, 9]);
	assert.equal(writes.size(), 0);
});

test('overlapping drains send each queued head once', async () => {
	const writes = queue(),
		entered = deferred<void>(),
		stalled = deferred<void>();
	for (const chapterId of ['one', 'two'])
		writes.enqueue({ kind: 'progress', chapterId, lastPageRead: 0, read: false, userId: 'A' });
	const sent: string[] = [];
	const flush = async (op: ReturnType<typeof writes.peekAll>[number]) => {
		assert.equal(op.kind, 'progress');
		if (op.kind !== 'progress') throw new Error('wrong fixture');
		sent.push(op.chapterId);
		if (sent.length === 1) {
			entered.resolve();
			await stalled.promise;
		}
	};
	const first = writes.drain(flush, 'A');
	await entered.promise;
	const second = writes.drain(flush, 'A');
	await Promise.resolve();
	assert.deepEqual(sent, ['one']);
	stalled.resolve();
	await Promise.all([first, second]);
	assert.deepEqual(sent, ['one', 'two']);
});

test('ordinary offline retries preserve progress resolution, mark rejection and poison bounds', async () => {
	const writes = queue(),
		error = new Error('offline');
	const composite = new CompositeBackend({
		hosted: {
			setProgress: async () => {
				throw error;
			},
			mark: async () => {
				throw error;
			},
		} as unknown as Backend,
		queue: writes,
	});
	composite.setCurrentUser('A');
	// Finish the initial empty replay before the foreground failures.
	await writes.drain(async () => {}, 'A');
	await composite.setProgress('chapter-A', 7, true);
	await assert.rejects(composite.mark('series-A', true), (e) => e === error);
	assert.deepEqual(
		writes.peekAll().map((op) => op.userId),
		['A', 'A'],
	);
	for (let n = 0; n < MAX_TRIES; n++)
		await writes.drain(async () => {
			throw error;
		}, 'A');
	assert.equal(writes.size(), 1, 'only the poisoned oldest operation was removed');
	await writes.drain(async () => {}, 'A');
	assert.equal(writes.size(), 0);
});
