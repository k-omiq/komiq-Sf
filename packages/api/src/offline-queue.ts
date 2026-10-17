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
import { createOpId } from './op-id.js';

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
	/**
	 * Stable identity lets queued successors survive collapse and reload. It is also sent
	 * with the save, so a resend after a lost answer is recognised as already applied.
	 */
	opId?: string;
	chapterId: string;
	/** Writes without a known work cannot participate in a revision chain. */
	workId?: string | null;
	/** The base revision is kept until an applied predecessor supplies its replacement. */
	revision?: number | null;
	/** This predecessor must finish before its successor can use an applied revision. */
	chainedTo?: string | null;
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
	/**
	 * The server's numeric epoch known when the write was queued (M5), replayed as is so
	 * the server refuses a write from before an engine change. `null` = unknown (the
	 * server was never reached, or it predates epochs); `undefined` on ops persisted
	 * before this field existed. Both replay with no epoch.
	 */
	numericEpoch?: number | null;
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
	/** See {@link ProgressOp.numericEpoch}. */
	numericEpoch?: number | null;
}

/** The queued write union. Discriminated by `kind`. */
export type WriteOp = ProgressOp | MarkOp;

/** Only an applied save advances the revision its queued successors must send. */
export type FlushResult = { appliedRevision: number | null };

/**
 * A single op minus the bookkeeping fields the queue owns (`ts`/`tries`/`opId`/`chainedTo`), i.e.
 * what a caller passes to {@link OfflineWriteQueue.enqueue}. `ts` may be supplied
 * (the composite can pass one) or omitted (the queue stamps `Date.now()`).
 */
export type EnqueueOp =
	| {
			kind: 'progress';
			chapterId: string;
			/** See {@link ProgressOp.workId}. */
			workId?: string | null;
			/** See {@link ProgressOp.revision}. */
			revision?: number | null;
			/** The id a live attempt of this same save already went out with, if any. */
			opId?: string;
			lastPageRead: number;
			read: boolean;
			ts?: number;
			/** Owning user id (C1); pass `null` when anonymous. */
			userId?: string | null;
			/** See {@link ProgressOp.numericEpoch}. */
			numericEpoch?: number | null;
	  }
	| {
			kind: 'mark';
			seriesId: string;
			marked: boolean;
			ts?: number;
			/** Owning user id (C1); pass `null` when anonymous. */
			userId?: string | null;
			/** See {@link ProgressOp.numericEpoch}. */
			numericEpoch?: number | null;
	  };

/** Drop an op after this many failed flush attempts so a permanently-rejected
 * write can't wedge the queue forever. */
export const MAX_TRIES = 5;

/** Remembered base revisions per user; the oldest works are forgotten first. */
export const MAX_REMEMBERED_REVISIONS = 500;

/**
 * The trailing list element that carries remembered base revisions. It has no owner,
 * so builds that predate it purge it on their next drain instead of misreading the list.
 */
interface RevisionRecord {
	kind: 'revisions';
	revisions: Record<string, Record<string, number>>;
}

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
	private ops: WriteOp[] = [];
	/** Last base revision seen per user and work, kept across restarts (offline start). */
	private revisions = new Map<string, Map<string, number>>();
	private replayGeneration = 0;
	private replay: Promise<void> | undefined;
	/** `settled` resolves after the sending drain has recorded the answer (or failure). */
	private inFlight: { op: WriteOp; settled: Promise<void> } | undefined;

	constructor(private adapter: StorageAdapter) {
		this.read();
	}

	/** Parse the persisted list, tolerating absent/corrupt storage (→ empty). */
	private read(): void {
		const raw = this.adapter.load();
		if (!raw) return;
		try {
			const parsed = JSON.parse(raw);
			if (!Array.isArray(parsed)) return;
			const items = parsed as (WriteOp | RevisionRecord)[];
			for (const item of items) {
				if (item?.kind !== 'revisions') continue;
				for (const [user, works] of Object.entries(item.revisions ?? {})) {
					const known = new Map<string, number>();
					for (const [work, revision] of Object.entries(works ?? {})) {
						if (typeof revision === 'number') known.set(work, revision);
					}
					if (known.size > 0) this.revisions.set(user, known);
				}
			}
			const ops = items.filter((item): item is WriteOp => item?.kind !== 'revisions');
			// Legacy identities stay in memory until the next mutation persists them.
			for (const op of ops) {
				if (op.kind === 'progress') op.opId ??= createOpId();
			}
			this.ops = ops;
		} catch {
			/* corrupt storage: start empty */
		}
	}

	/** Persist the current list. Called after every mutation. */
	private persist(): void {
		const items: (WriteOp | RevisionRecord)[] = [...this.ops];
		if (this.revisions.size > 0) {
			const revisions: RevisionRecord['revisions'] = {};
			for (const [user, works] of this.revisions) revisions[user] = Object.fromEntries(works);
			items.push({ kind: 'revisions', revisions });
		}
		this.adapter.save(JSON.stringify(items));
	}

	/**
	 * Remember the base revision `userId` last saw for `workId`, so a save queued after an
	 * offline restart still carries it. Revisions only rise; a lower one is ignored.
	 */
	rememberRevision(userId: string | null, workId: string, revision: number): void {
		if (userId == null) return;
		let works = this.revisions.get(userId);
		const known = works?.get(workId);
		if (known !== undefined && known >= revision) return;
		if (!works) this.revisions.set(userId, (works = new Map()));
		works.delete(workId);
		works.set(workId, revision);
		while (works.size > MAX_REMEMBERED_REVISIONS) works.delete(works.keys().next().value!);
		this.persist();
	}

	/** Forget a remembered revision, e.g. once the work was loaded fresh with none known. */
	forgetRevision(userId: string | null, workId: string): void {
		if (userId == null) return;
		const works = this.revisions.get(userId);
		if (!works?.delete(workId)) return;
		if (works.size === 0) this.revisions.delete(userId);
		this.persist();
	}

	/** The remembered base revision of `userId` for `workId`, if any. */
	knownRevision(userId: string | null, workId: string): number | undefined {
		return userId == null ? undefined : this.revisions.get(userId)?.get(workId);
	}

	/** A completed predecessor releases successors even when it did not advance the revision. */
	private releaseSuccessors(op: WriteOp, appliedRevision?: number | null): void {
		if (op.kind !== 'progress' || op.workId == null || op.opId == null) return;
		for (const successor of this.ops) {
			if (successor.kind === 'progress' && successor.chainedTo === op.opId) {
				successor.chainedTo = null;
				if (typeof appliedRevision === 'number') successor.revision = appliedRevision;
			}
		}
	}

	/** Identity filtering must not leave a retained write waiting on a removed predecessor. */
	private clearMissingPredecessors(): void {
		const ids = new Set(
			this.ops
				.filter((op): op is ProgressOp => op.kind === 'progress' && op.workId != null)
				.map((op) => op.opId),
		);
		for (const op of this.ops) {
			if (op.kind === 'progress' && op.chainedTo != null && !ids.has(op.chainedTo)) {
				op.chainedTo = null;
			}
		}
	}

	/**
	 * Append a write, collapsing duplicates so only the latest state survives: a
	 * new `progress` for a `chapterId` replaces any pending `progress` for that
	 * same chapter, and a new `mark` for a `seriesId` replaces any pending `mark`
	 * for that series (latest wins). This keeps the queue bounded and correct —
	 * intermediate progress/membership states are irrelevant once superseded.
	 * An in-flight progress write with a work stays because its response may
	 * advance the revision a later write needs. Successors of collapsed writes
	 * inherit their dependencies so that revision hand-over still reaches them.
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
						opId: op.opId ?? createOpId(),
						chapterId: op.chapterId,
						workId: op.workId ?? null,
						revision: op.revision ?? null,
						chainedTo: null,
						lastPageRead: op.lastPageRead,
						read: op.read,
						ts,
						tries: 0,
						userId,
						numericEpoch: op.numericEpoch ?? null,
					}
				: {
						kind: 'mark',
						seriesId: op.seriesId,
						marked: op.marked,
						ts,
						tries: 0,
						userId,
						numericEpoch: op.numericEpoch ?? null,
					};
		this.ops = this.ops.filter((existing) => {
			if (!sameTarget(existing, full)) return true;
			if (existing === this.inFlight?.op && existing.kind === 'progress' && existing.workId != null)
				return true;
			if (existing.kind === 'progress' && existing.workId != null && existing.opId != null) {
				for (const successor of this.ops) {
					if (successor.kind === 'progress' && successor.chainedTo === existing.opId) {
						successor.chainedTo = existing.chainedTo ?? null;
						const revision = Math.max(
							successor.revision ?? -Infinity,
							existing.revision ?? -Infinity,
						);
						successor.revision = revision === -Infinity ? null : revision;
					}
				}
			}
			return false;
		});
		if (full.kind === 'progress' && full.workId != null) {
			for (let i = this.ops.length - 1; i >= 0; i--) {
				const predecessor = this.ops[i];
				if (
					predecessor.kind === 'progress' &&
					predecessor.workId === full.workId &&
					predecessor.userId === userId
				) {
					full.chainedTo = predecessor.opId ?? null;
					break;
				}
			}
		}
		this.ops.push(full);
		this.persist();
	}

	/** Number of pending ops. */
	size(): number {
		return this.ops.length;
	}

	/** Live saves must join an existing backlog for the same user and work. */
	hasPendingProgress(userId: string | null, workId: string): boolean {
		return this.ops.some(
			(op) => op.kind === 'progress' && op.userId === userId && op.workId === workId,
		);
	}

	/** A defensive copy of the pending ops, oldest-first (for tests/inspection). */
	peekAll(): WriteOp[] {
		return this.ops.map((o) => ({ ...o }));
	}

	/**
	 * Drop every op and remembered revision that does NOT belong to `userId` (C1). Called on
	 * login/logout/identity-change so a queued write can never survive into a
	 * different session and replay under the wrong account. A `null` userId
	 * (logout / anonymous) clears the queue entirely — nothing is retained across a
	 * sign-out. No-op when nothing changes.
	 */
	dropForeign(userId: string | null): void {
		this.cancelReplay();
		const before = this.ops.length;
		this.ops = userId == null ? [] : this.ops.filter((op) => op.userId === userId);
		const users = this.revisions.size;
		for (const user of [...this.revisions.keys()]) {
			if (user !== userId) this.revisions.delete(user);
		}
		if (this.ops.length !== before || this.revisions.size !== users) {
			this.clearMissingPredecessors();
			this.persist();
		}
	}

	/** Stop a drain at its next await boundary. Already-sent writes cannot be
	 * recalled, but a new token or identity must never send the remaining backlog. */
	cancelReplay(): void {
		this.replayGeneration += 1;
		// A hung request from an obsolete auth context must not block this account's
		// replay. Its eventual completion still owns only its old head; only that same
		// op waits for it, because it cannot be sent twice.
		this.replay = undefined;
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
		flush: (op: WriteOp) => Promise<FlushResult | void>,
		currentUserId: string | null,
	): Promise<void> {
		// Identity unknown (e.g. token restored but session not yet validated): defer.
		// Never drop here — the owner may still sign in and reclaim these ops.
		if (currentUserId == null) return;
		const generation = this.replayGeneration;
		// Serialize drains before they inspect or send the head. Defer the first
		// callback by one microtask so ownership exists even for reentrant flushes.
		while (this.replay) await this.replay;
		if (generation !== this.replayGeneration) return;
		const replay = Promise.resolve().then(() => this.drainOwned(flush, currentUserId, generation));
		this.replay = replay;
		try {
			await replay;
		} finally {
			if (this.replay === replay) this.replay = undefined;
		}
	}

	private async drainOwned(
		flush: (op: WriteOp) => Promise<FlushResult | void>,
		currentUserId: string,
		generation: number,
	): Promise<void> {
		if (generation !== this.replayGeneration) return;
		// Cross-user safety net: purge anything not owned by the current user before
		// replaying. Redundant with `dropForeign` on identity-change, but guarantees a
		// foreign op is impossible to replay even if an identity transition was missed.
		const before = this.ops.length;
		this.ops = this.ops.filter((op) => op.userId === currentUserId);
		if (this.ops.length !== before) {
			this.clearMissingPredecessors();
			this.persist();
		}
		while (this.ops.length > 0 && generation === this.replayGeneration) {
			const op = this.ops[0];
			if (op.userId !== currentUserId) {
				this.ops.shift();
				this.clearMissingPredecessors();
				this.persist();
				continue;
			}
			// A cancelled drain may still have this op on the wire. A second request for it
			// would carry the same base revision and be refused if the first one applied.
			const pending = this.inFlight;
			if (pending?.op === op) {
				await pending.settled;
				continue;
			}
			let settle!: () => void;
			const inFlight = { op, settled: new Promise<void>((resolve) => (settle = resolve)) };
			this.inFlight = inFlight;
			try {
				const result = await flush(op);
				// Enqueue can replace the head with a newer state while a write waits.
				// A late completion only removes the exact operation it sent. The server
				// answered, so even a cancelled drain records that answer; it only stops.
				if (this.ops[0] === op) {
					this.ops.shift();
					this.releaseSuccessors(op, result?.appliedRevision);
					this.persist();
				}
				if (generation !== this.replayGeneration) return;
			} catch {
				if (generation !== this.replayGeneration) return;
				if (this.ops[0] !== op) continue;
				op.tries += 1;
				if (op.tries >= MAX_TRIES) {
					this.ops.shift();
					this.releaseSuccessors(op);
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
			} finally {
				// A newer drain can await the same op, so only this request may clear its marker.
				if (this.inFlight === inFlight) this.inFlight = undefined;
				settle();
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
