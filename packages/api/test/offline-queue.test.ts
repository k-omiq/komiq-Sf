import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire, registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import type { Backend, SaveProgressInput } from '../src/backend.ts';
import type { FlushResult, ProgressOp, StorageAdapter, WriteOp } from '../src/offline-queue.ts';

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
		if (/\/packages\/api\/src\/(offline-queue|composite-backend)\.ts$/.test(url)) {
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

const { OfflineWriteQueue } = await import('../src/offline-queue.ts');
const { CompositeBackend } = await import('../src/composite-backend.ts');

const settle = () => new Promise<void>((resolve) => setImmediate(resolve));

function deferred<T = void>() {
	let resolve!: (value: T) => void;
	const promise = new Promise<T>((yes) => (resolve = yes));
	return { promise, resolve };
}

function storage() {
	let stored: string | null = null;
	const adapter: StorageAdapter = {
		load: () => stored,
		save: (value) => void (stored = value),
	};
	return adapter;
}

const progress = (chapterId: string) =>
	({
		kind: 'progress',
		chapterId,
		workId: 'work',
		revision: 5,
		lastPageRead: 0,
		read: true,
		userId: 'A',
	}) as const;

test('a token change while a save is out records its answer and never sends it twice', async () => {
	const writes = new OfflineWriteQueue(storage());
	writes.enqueue(progress('10'));
	writes.enqueue(progress('11'));
	const sent: WriteOp[] = [];
	const first = deferred<FlushResult>();
	const flush = async (op: WriteOp) => {
		sent.push({ ...op });
		return sent.length === 1 ? first.promise : { appliedRevision: 7 };
	};
	const old = writes.drain(flush, 'A');
	await settle();
	writes.cancelReplay();
	const next = writes.drain(flush, 'A');
	await settle();
	assert.deepEqual(
		sent.map((op) => (op as ProgressOp).chapterId),
		['10'],
		'the op still on the wire is not sent again',
	);
	first.resolve({ appliedRevision: 6 });
	await old;
	await next;
	const [, second] = sent as ProgressOp[];
	assert.equal(second.chapterId, '11');
	assert.equal(second.revision, 6, 'the successor takes the late APPLIED revision');
	assert.equal(sent.length, 2);
	assert.equal(writes.size(), 0);
});

test('a save queued after an offline restart carries the revision seen before it', async () => {
	const adapter = storage();
	const server = { online: true, revision: 5 };
	const hostedFor = (known: Map<string, number>) =>
		({
			setToken: () => {},
			numericEpoch: async () => null,
			knownRevision: (workId: string) => known.get(workId),
			saveProgress: async (save: SaveProgressInput) => {
				if (!server.online) throw new Error('offline');
				known.set(save.workId, ++server.revision);
				return { status: 'APPLIED', revision: server.revision, workId: save.workId };
			},
		}) as unknown as Backend;
	const save = { chapterId: '10', workId: 'work', lastPageRead: 3, read: false };

	// First run: an online save teaches the device revision 6.
	const before = new CompositeBackend({
		hosted: hostedFor(new Map()),
		queue: new OfflineWriteQueue(adapter),
	});
	before.setToken('token');
	before.setCurrentUser('A');
	await before.saveProgress(save);
	await settle();

	// Restart offline: the hosted backend knows nothing, the queue's storage does.
	server.online = false;
	server.revision = 9; // another device moved the work meanwhile
	const writes = new OfflineWriteQueue(adapter);
	const after = new CompositeBackend({ hosted: hostedFor(new Map()), queue: writes });
	after.setToken('token');
	after.setCurrentUser('A');
	await after.saveProgress({ ...save, chapterId: '11', read: true });
	const [op] = writes.peekAll() as ProgressOp[];
	assert.equal(op.revision, 6, 'replay can be judged stale against revision 9');

	// A different account's sign-in drops it with that account's foreign writes.
	writes.dropForeign('B');
	assert.equal(writes.knownRevision('A', 'work'), undefined);
	assert.equal(new OfflineWriteQueue(adapter).knownRevision('A', 'work'), undefined);
});

test('remembered revisions survive a reload and older builds can drop their record', () => {
	const adapter = storage();
	const writes = new OfflineWriteQueue(adapter);
	writes.enqueue(progress('10'));
	writes.rememberRevision('A', 'work', 8);
	writes.rememberRevision('A', 'work', 4);
	const reloaded = new OfflineWriteQueue(adapter);
	assert.equal(reloaded.knownRevision('A', 'work'), 8, 'revisions only rise');
	assert.equal(reloaded.size(), 1, 'the record is not a queued write');
	// Builds before the record purge entries without an owner before replaying.
	const stored = JSON.parse(adapter.load()!) as { userId?: unknown }[];
	assert.equal(stored.filter((item) => item.userId === 'A').length, 1);
	reloaded.forgetRevision('A', 'work');
	assert.equal(new OfflineWriteQueue(adapter).knownRevision('A', 'work'), undefined);
});
