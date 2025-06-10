/**
 * Local social store — persistent ratings & comments.
 *
 * Suwayomi has no social layer and Komika's own social service doesn't exist yet,
 * so this makes ratings/comments genuinely work today by persisting to
 * localStorage, keyed by series/chapter. It's deliberately behind a small API so
 * the UI can swap to the real `backend.reviews/comments/postReview/postComment`
 * later without changing screen code — only these functions get reimplemented.
 *
 * Single-device only (localStorage); a real multi-user layer needs the server.
 */

const KEY = 'komika-social-v1';

// 'series' = per-series reviews; 'chapter' = per-chapter comments;
// 'seriesThread' = series-level discussion comments (distinct from reviews so the two
// don't collide in the local store).
type Bucket = 'series' | 'chapter' | 'seriesThread';

interface Store {
	rating: Record<string, number>;
	comments: Record<string, unknown[]>;
}

function safeLoad(): Store {
	if (typeof localStorage === 'undefined') return { rating: {}, comments: {} };
	try {
		const raw = localStorage.getItem(KEY);
		if (!raw) return { rating: {}, comments: {} };
		const parsed = JSON.parse(raw) as Partial<Store>;
		return { rating: parsed.rating ?? {}, comments: parsed.comments ?? {} };
	} catch {
		return { rating: {}, comments: {} };
	}
}

function safeSave(store: Store): void {
	if (typeof localStorage === 'undefined') return;
	try {
		localStorage.setItem(KEY, JSON.stringify(store));
	} catch {
		/* quota / private mode — ignore */
	}
}

function bucketKey(bucket: Bucket, key: string): string {
	return `${bucket}:${key}`;
}

/** Read a persisted rating (0 if unset). */
export function getRating(bucket: Bucket, key: string): number {
	return safeLoad().rating[bucketKey(bucket, key)] ?? 0;
}

/** Persist a rating (0 clears it). */
export function setRating(bucket: Bucket, key: string, score: number): void {
	const store = safeLoad();
	const k = bucketKey(bucket, key);
	if (score > 0) store.rating[k] = score;
	else delete store.rating[k];
	safeSave(store);
}

/**
 * Get persisted comments for a key, seeding from `seed` (the design's sample
 * thread) on first access so a fresh user sees a lively section.
 */
export function getComments<T>(bucket: Bucket, key: string, seed: T[]): T[] {
	const store = safeLoad();
	const k = bucketKey(bucket, key);
	if (!store.comments[k]) {
		store.comments[k] = seed as unknown[];
		safeSave(store);
		return seed.map((c) => ({ ...c }));
	}
	return store.comments[k] as T[];
}

/** Persist the full comment list for a key (after a post or like toggle). */
export function saveComments<T>(bucket: Bucket, key: string, list: T[]): void {
	const store = safeLoad();
	store.comments[bucketKey(bucket, key)] = list as unknown[];
	safeSave(store);
}
