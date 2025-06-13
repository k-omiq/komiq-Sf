/**
 * A durable, dependency-free write-queue for on-device library/progress writes
 * (native only; plan §9). On the hosted server (D2) library membership (`mark`)
 * and reading progress (`setProgress`) are the source of truth, routed there by
 * {@link CompositeBackend}. When the device is offline those writes throw and
 * would be lost; this queue captures them, persists them through an injectable
 * {@link StorageAdapter}, and replays them (reconciled by the canonical id the
 * write already carries) when connectivity returns.
 *
 * No timers, no network, no npm deps: the composite drives draining off its own
 * success signals + the browser `online` event, and the queue only reorders and
 * flushes an ordered list of ops.
 */

/**
 * Storage seam so persistence is injectable. The default binds to
 * `localStorage` when it exists (browser + Tauri webview) and falls back to an
 * in-memory store (Node / SSR / tests). Kept to load/save because the queue
 * persists its entire ordered list as one JSON blob under a single key.
 */
export interface StorageAdapter {
	/** Read the persisted JSON string, or null when nothing is stored. */
	load(): string | null;
	/** Persist the JSON string (overwrites). */
	save(v: string): void;
}

/** A pending on-device reading-progress write, reconciled by canonical `chapterId`. */
export interface ProgressOp {
	kind: 'progress';
	chapterId: string;
	lastPageRead: number;
	read: boolean;
	/** Enqueue timestamp (ms epoch); supplied by the caller / stamped at enqueue. */
	ts: number;
	/** How many failed flush attempts this op has survived (poison-drop guard). */
	tries: number;
	/**
	 * The user this write belongs to (C1): stamped at enqueue so a drain NEVER
	 * replays it under a different signed-in account. `null` = enqueued while
	 * anonymous (never replayed). `undefined` on ops persisted before this field
	 * existed — treated as foreign and dropped rather than mis-attributed.
	 */
	userId?: string | null;
}

/** A pending on-device library-membership write, reconciled by canonical `seriesId`. */
export interface MarkOp {
	kind: 'mark';
	seriesId: string;
	marked: boolean;
	/** Enqueue timestamp (ms epoch); supplied by the caller / stamped at enqueue. */
	ts: number;
	/** How many failed flush attempts this op has survived (poison-drop guard). */
	tries: number;
	/** See {@link ProgressOp.userId} (C1). */
	userId?: string | null;
}

/** The queued write union. Discriminated by `kind`. */
export type WriteOp = ProgressOp | MarkOp;

/**
 * A single op minus the bookkeeping fields the queue owns (`ts`/`tries`), i.e.
 * what a caller passes to {@link OfflineWriteQueue.enqueue}. `ts` may be supplied
 * (the composite can pass one) or omitted (the queue stamps `Date.now()`).
 */
export type EnqueueOp =
	| {
			kind: 'progress';
			chapterId: string;
			lastPageRead: number;
			read: boolean;
			ts?: number;
			/** Owning user id (C1); pass `null` when anonymous. */
			userId?: string | null;
	  }
	| {
			kind: 'mark';
			seriesId: string;
			marked: boolean;
			ts?: number;
			/** Owning user id (C1); pass `null` when anonymous. */
			userId?: string | null;
	  };

/** Drop an op after this many failed flush attempts so a permanently-rejected
 * write can't wedge the queue forever. */
export const MAX_TRIES = 5;

/**
 * A `localStorage`-backed {@link StorageAdapter} keyed by `key`. Falls back to an
 * in-memory store when `localStorage` is absent (Node / SSR) or unusable (private
 * mode / quota) so construction never throws off-browser. Mirrors the repo's
 * guarded `typeof localStorage` access pattern.
 */
export function createLocalStorageAdapter(key: string): StorageAdapter {
	if (typeof localStorage === 'undefined') return createMemoryStorageAdapter();
	return {
		load() {
			try {
				return localStorage.getItem(key);
			} catch {
				return null;
			}
		},
		save(v: string) {
			try {
				localStorage.setItem(key, v);
			} catch {
				/* private mode / quota — the queue just won't survive reload */
			}
		},
	};
}

/** An in-memory {@link StorageAdapter} (Node / SSR / tests). Not durable across
 * process restarts, but keeps the queue's API identical off-browser. */
export function createMemoryStorageAdapter(): StorageAdapter {
	let store: string | null = null;
	return {
		load() {
			return store;
		},
		save(v: string) {
			store = v;
		},
	};
}

/**
 * An ordered, persisted list of pending writes with duplicate-collapse on
 * enqueue and oldest-first draining. Persist happens after every mutation so a
 * crash/reload never loses an accepted write.
 */
export class OfflineWriteQueue {
	private ops: WriteOp[];

	constructor(private adapter: StorageAdapter) {
		this.ops = this.read();
	}

	/** Parse the persisted list, tolerating absent/corrupt storage (→ empty). */
	private read(): WriteOp[] {
		const raw = this.adapter.load();
		if (!raw) return [];
		try {
			const parsed = JSON.parse(raw);
			return Array.isArray(parsed) ? (parsed as WriteOp[]) : [];
		} catch {
			return [];
		}
	}

	/** Persist the current list. Called after every mutation. */
	private persist(): void {
		this.adapter.save(JSON.stringify(this.ops));
	}

	/**
	 * Append a write, collapsing duplicates so only the latest state survives: a
	 * new `progress` for a `chapterId` replaces any pending `progress` for that
	 * same chapter, and a new `mark` for a `seriesId` replaces any pending `mark`
	 * for that series (latest wins). This keeps the queue bounded and correct —
	 * intermediate progress/membership states are irrelevant once superseded.
	 * `tries` resets on collapse: a freshly-observed state deserves a clean retry
	 * budget.
	 */
	enqueue(op: EnqueueOp): void {
		const ts = op.ts ?? Date.now();
		const userId = op.userId ?? null;
		const full: WriteOp =
			op.kind === 'progress'
				? {
						kind: 'progress',
						chapterId: op.chapterId,
						lastPageRead: op.lastPageRead,
						read: op.read,
						ts,
						tries: 0,
						userId,
					}
				: { kind: 'mark', seriesId: op.seriesId, marked: op.marked, ts, tries: 0, userId };
		this.ops = this.ops.filter((existing) => !sameTarget(existing, full));
		this.ops.push(full);
		this.persist();
	}

	/** Number of pending ops. */
	size(): number {
		return this.ops.length;
	}

	/** A defensive copy of the pending ops, oldest-first (for tests/inspection). */
	peekAll(): WriteOp[] {
		return this.ops.map((o) => ({ ...o }));
	}

	/**
	 * Drop every op that does NOT belong to `userId` (C1). Called on
	 * login/logout/identity-change so a queued write can never survive into a
	 * different session and replay under the wrong account. A `null` userId
	 * (logout / anonymous) clears the queue entirely — nothing is retained across a
	 * sign-out. No-op when nothing changes.
	 */
	dropForeign(userId: string | null): void {
		const before = this.ops.length;
		this.ops = userId == null ? [] : this.ops.filter((op) => op.userId === userId);
		if (this.ops.length !== before) this.persist();
	}

	/**
	 * Replay pending ops oldest-first through `flush`. On success the op is
	 * removed; on failure its `tries` is incremented and draining STOPS (order is
	 * preserved and the next drain retries from the same head). An op that has
	 * failed {@link MAX_TRIES} times is dropped (logged) so a permanently-rejected
	 * write can't wedge the queue forever. Persists after every mutation. Never
	 * throws: a rejecting `flush` is caught and ends the drain.
	 *
	 * `currentUserId` scopes the drain to a single account (C1): if it is `null`
	 * the identity isn't known yet and the drain DEFERS entirely (dropping here
	 * would lose the rightful owner's writes). Otherwise any op not stamped with
	 * `currentUserId` is purged up front — a foreign op can never be replayed under
	 * this session — before the oldest-first flush of the remaining owned ops.
	 */
	async drain(
		flush: (op: WriteOp) => Promise<void>,
		currentUserId: string | null,
	): Promise<void> {
		// Identity unknown (e.g. token restored but session not yet validated): defer.
		// Never drop here — the owner may still sign in and reclaim these ops.
		if (currentUserId == null) return;
		// Cross-user safety net: purge anything not owned by the current user before
		// replaying. Redundant with `dropForeign` on identity-change, but guarantees a
		// foreign op is impossible to replay even if an identity transition was missed.
		const before = this.ops.length;
		this.ops = this.ops.filter((op) => op.userId === currentUserId);
		if (this.ops.length !== before) this.persist();
		while (this.ops.length > 0) {
			const op = this.ops[0];
			try {
				await flush(op);
				this.ops.shift();
				this.persist();
			} catch {
				op.tries += 1;
				if (op.tries >= MAX_TRIES) {
					this.ops.shift();
					this.persist();
					console.warn(
						`[offline-queue] dropping poison ${op.kind} op after ${op.tries} tries:`,
						op.kind === 'progress' ? op.chapterId : op.seriesId,
					);
					// The dropped op cleared the head; keep draining the rest.
					continue;
				}
				this.persist();
				// Preserve order — stop and retry from this head on the next drain.
				return;
			}
		}
	}
}

/** Whether two ops address the same reconciliation target (so `b` supersedes `a`). */
function sameTarget(a: WriteOp, b: WriteOp): boolean {
	if (a.kind === 'progress' && b.kind === 'progress') return a.chapterId === b.chapterId;
	if (a.kind === 'mark' && b.kind === 'mark') return a.seriesId === b.seriesId;
	return false;
}
