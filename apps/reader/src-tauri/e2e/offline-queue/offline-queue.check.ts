// Headless, deterministic check for the durable offline write-queue (N2.3).
//
// Pure logic: no Tauri, no engine, no network. Drives the REAL CompositeBackend +
// OfflineWriteQueue from @komika/api against a MOCK hosted backend whose writes
// throw while `online=false` and succeed (recording calls) while `online=true`,
// plus an in-memory StorageAdapter. Proves: offline capture + swallow/rethrow
// contract, duplicate-collapse, ordered replay on reconnect, poison-drop after
// MAX_TRIES, and persistence across a simulated restart.

import {
	CompositeBackend,
	MAX_TRIES,
	OfflineWriteQueue,
	createMemoryStorageAdapter,
} from '@komika/api';
import type { Backend, Session, StorageAdapter } from '@komika/api';
import type { Series } from '@komika/types';

let failures = 0;
function check(label: string, cond: boolean): void {
	console.log(`${cond ? 'PASS' : 'FAIL'}: ${label}`);
	if (!cond) failures++;
}

/** A minimal Series stub — only the fields the write path returns/consumes. */
function seriesStub(id: string, marked: boolean): Series {
	return {
		id,
		title: 'stub',
		type: 'MANGA',
		status: 'ONGOING',
		author: null,
		artist: null,
		description: null,
		genres: [],
		coverUrl: '',
		chapterCount: 0,
		rating: { average: 0, count: 0 },
		updatedAt: null,
		isMarked: marked,
	} as unknown as Series;
}

interface Recorded {
	progress: { chapterId: string; lastPageRead: number; read: boolean }[];
	mark: { seriesId: string; marked: boolean }[];
}

/**
 * A mock hosted backend controllable via a mutable `state.online` flag. When
 * offline both writes throw (like a network reject / non-ok status); when online
 * they record the call and resolve. Everything else is unused here.
 */
function mockHosted(state: { online: boolean }, rec: Recorded): Backend {
	const offline = () => new Error('Failed to fetch');
	return {
		session: async () => null as Session | null,
		login: async () => {
			throw offline();
		},
		register: async () => {
			throw offline();
		},
		logout: async () => {},
		discovery: async () => [],
		updates: async () => ({ items: [], total: 0, page: 1, pageSize: 0 }) as never,
		search: async () => ({ items: [], total: 0, page: 1, pageSize: 0 }) as never,
		series: async (id: string) => seriesStub(id, false),
		chapters: async () => [],
		pages: async () => [],
		library: async () => [],
		mark: async (seriesId: string, marked: boolean) => {
			if (!state.online) throw offline();
			rec.mark.push({ seriesId, marked });
			return seriesStub(seriesId, marked);
		},
		setProgress: async (chapterId: string, lastPageRead: number, read: boolean) => {
			if (!state.online) throw offline();
			rec.progress.push({ chapterId, lastPageRead, read });
		},
		reviews: async () => ({ items: [], total: 0, page: 1, pageSize: 0 }) as never,
		postReview: async () => {
			throw offline();
		},
		comments: async () => ({ items: [], total: 0, page: 1, pageSize: 0 }) as never,
		postComment: async () => {
			throw offline();
		},
	} as unknown as Backend;
}

async function main(): Promise<void> {
	// ---- (1) offline setProgress resolves (swallowed) + enqueues + collapses ----
	{
		const state = { online: false };
		const rec: Recorded = { progress: [], mark: [] };
		const queue = new OfflineWriteQueue(createMemoryStorageAdapter());
		const be = new CompositeBackend({ hosted: mockHosted(state, rec), queue });
		be.setCurrentUser('u1'); // C1: ops are stamped with the signed-in user

		let threw = false;
		await be.setProgress('ch1', 3, false).catch(() => (threw = true));
		check('offline setProgress resolves (swallowed, does not reject)', !threw);
		check('offline setProgress enqueues one op', queue.size() === 1);

		// Second progress for the SAME chapter collapses (latest wins, size stays 1).
		await be.setProgress('ch1', 7, true);
		check('duplicate progress for same chapter collapses (size stays 1)', queue.size() === 1);
		const head = queue.peekAll()[0];
		check(
			'collapsed progress keeps latest values (page 7, read true)',
			head.kind === 'progress' && head.lastPageRead === 7 && head.read === true,
		);
		check('hosted saw no successful progress write while offline', rec.progress.length === 0);
	}

	// ---- (2) offline mark rethrows AND enqueues ----
	{
		const state = { online: false };
		const rec: Recorded = { progress: [], mark: [] };
		const queue = new OfflineWriteQueue(createMemoryStorageAdapter());
		const be = new CompositeBackend({ hosted: mockHosted(state, rec), queue });
		be.setCurrentUser('u1'); // C1: ops are stamped with the signed-in user

		let threw = false;
		await be.mark('s1', true).catch(() => (threw = true));
		check('offline mark rethrows (preserves optimistic-UI catch contract)', threw);
		check('offline mark enqueues one op', queue.size() === 1);

		// Duplicate mark for the same series collapses too.
		await be.mark('s1', false).catch(() => {});
		check('duplicate mark for same series collapses (size stays 1)', queue.size() === 1);
		const head = queue.peekAll()[0];
		check('collapsed mark keeps latest value (marked false)', head.kind === 'mark' && head.marked === false);
	}

	// ---- (3) reconnect → flushQueue replays collapsed writes in order, queue empties ----
	{
		const state = { online: false };
		const rec: Recorded = { progress: [], mark: [] };
		const queue = new OfflineWriteQueue(createMemoryStorageAdapter());
		const be = new CompositeBackend({ hosted: mockHosted(state, rec), queue });
		be.setCurrentUser('u1'); // C1: ops are stamped with the signed-in user

		await be.setProgress('chA', 1, false); // enqueue #1 (progress)
		await be.mark('sB', true).catch(() => {}); // enqueue #2 (mark)
		await be.setProgress('chA', 9, true); // collapses onto #1 (latest wins, stays at head)
		check('two distinct targets queued (progress + mark)', queue.size() === 2);

		state.online = true;
		// Drive a drain directly (equivalent to the constructor / 'online' event path).
		// Scoped to the current user (C1).
		await queue.drain(
			(op) =>
				op.kind === 'progress'
					? mockHosted(state, rec).setProgress(op.chapterId, op.lastPageRead, op.read)
					: mockHosted(state, rec).mark(op.seriesId, op.marked).then(() => {}),
			'u1',
		);
		check('queue empties after online drain', queue.size() === 0);
		check(
			'progress replayed exactly once with collapsed latest values',
			rec.progress.length === 1 &&
				rec.progress[0].chapterId === 'chA' &&
				rec.progress[0].lastPageRead === 9 &&
				rec.progress[0].read === true,
		);
		check(
			'mark replayed exactly once',
			rec.mark.length === 1 && rec.mark[0].seriesId === 'sB' && rec.mark[0].marked === true,
		);
	}

	// ---- (4) permanently-throwing hosted drops the op after MAX_TRIES (no unbounded growth) ----
	{
		const alwaysFail: Backend = {
			setProgress: async () => {
				throw new Error('permanent');
			},
		} as unknown as Backend;
		const queue = new OfflineWriteQueue(createMemoryStorageAdapter());
		queue.enqueue({ kind: 'progress', chapterId: 'poison', lastPageRead: 0, read: false, userId: 'u1' });

		// Each drain increments tries by 1 (fails at head, stops). After MAX_TRIES drains
		// the op is dropped. The queue never grows.
		let maxSeen = 0;
		for (let i = 0; i < MAX_TRIES; i++) {
			await queue.drain(
				(op) =>
					alwaysFail.setProgress(
						(op as { chapterId: string }).chapterId,
						(op as { lastPageRead: number }).lastPageRead,
						(op as { read: boolean }).read,
					),
				'u1',
			);
			maxSeen = Math.max(maxSeen, queue.size());
		}
		check('poison op is dropped after MAX_TRIES (queue empty)', queue.size() === 0);
		check('queue never grew beyond one entry (bounded)', maxSeen === 1);
	}

	// ---- (5) persistence: a fresh queue over the SAME adapter sees pending ops (restart) ----
	{
		const adapter: StorageAdapter = createMemoryStorageAdapter();
		const q1 = new OfflineWriteQueue(adapter);
		q1.enqueue({ kind: 'progress', chapterId: 'ch-persist', lastPageRead: 4, read: false });
		q1.enqueue({ kind: 'mark', seriesId: 's-persist', marked: true });
		check('pre-restart queue holds two ops', q1.size() === 2);

		// Simulate a process restart: brand-new instance over the same backing store.
		const q2 = new OfflineWriteQueue(adapter);
		check('restarted queue restores pending ops from storage', q2.size() === 2);
		const ops = q2.peekAll();
		check(
			'restored ops preserve kind/order',
			ops[0].kind === 'progress' &&
				ops[0].chapterId === 'ch-persist' &&
				ops[1].kind === 'mark' &&
				ops[1].seriesId === 's-persist',
		);
	}

	// ---- (6) C1: a foreign user's queued ops NEVER replay under another account ----
	{
		const state = { online: true };
		const rec: Recorded = { progress: [], mark: [] };
		const queue = new OfflineWriteQueue(createMemoryStorageAdapter());
		// User A queues a write (offline capture stamps ownership).
		queue.enqueue({ kind: 'progress', chapterId: 'a-secret', lastPageRead: 5, read: true, userId: 'userA' });
		check('user A op is queued', queue.size() === 1);

		// User B is now signed in and a drain fires (reconnect / flush).
		await queue.drain(
			(op) =>
				op.kind === 'progress'
					? mockHosted(state, rec).setProgress(op.chapterId, op.lastPageRead, op.read)
					: mockHosted(state, rec).mark(op.seriesId, op.marked).then(() => {}),
			'userB',
		);
		check("user A's op is purged, never replayed under user B", queue.size() === 0);
		check('hosted recorded NO write for the foreign op (C1)', rec.progress.length === 0 && rec.mark.length === 0);

		// Identity-unknown drain defers instead of dropping the rightful owner's ops.
		queue.enqueue({ kind: 'mark', seriesId: 's-defer', marked: true, userId: 'userA' });
		await queue.drain(() => Promise.resolve(), null);
		check('drain with unknown identity (null) defers, keeps the op', queue.size() === 1);
	}

	console.log('');
	if (failures === 0) {
		console.log('ALL CHECKS PASSED');
	} else {
		console.log(`${failures} CHECK(S) FAILED`);
		process.exitCode = 1;
	}
}

main().catch((err) => {
	console.error('offline-queue.check crashed:', err);
	process.exitCode = 1;
});
