/**
 * M9 revisioned progress: the server refused a save because the reader's view of a work
 * was stale (`REFUSED`), or the chapter copy now belongs to another work (`MOVED`). Either
 * way the reader's optimistic read state for that work is wrong and must be dropped and
 * refetched. Backends raise this for live saves AND for queued replays.
 */
export interface ProgressConflict {
	status: 'REFUSED' | 'MOVED';
	/** The work id the save named. */
	workId: string;
	/** `MOVED`: the work the copy belongs to now (null if it has none). */
	movedTo?: string | null;
	chapterId: string;
}

const listeners = new Set<(c: ProgressConflict) => void>();

export function onProgressConflict(listener: (c: ProgressConflict) => void): () => void {
	listeners.add(listener);
	return () => {
		listeners.delete(listener);
	};
}

export function notifyProgressConflict(conflict: ProgressConflict): void {
	for (const listener of listeners) {
		try {
			listener(conflict);
		} catch (e) {
			console.warn('[komika] progress conflict listener failed:', e);
		}
	}
}
