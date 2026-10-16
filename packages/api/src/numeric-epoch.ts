/**
 * M5 numeric epoch: numeric (Suwayomi engine) chapter ids are only meaningful within one
 * engine generation. Progress writes for them carry the server's `numericEpoch`, and the
 * server refuses one from an earlier generation.
 */
import { GraphQLRequestError } from './partial-errors.js';

/** A Suwayomi engine chapter id (all digits); canonical MangaDex ids are uuids. The
 *  server routes progress on the same shape. */
export function isNumericChapterId(id: string): boolean {
	return /^[0-9]+$/.test(id);
}

/**
 * The server refused a numeric progress write because it belongs to an earlier engine
 * generation (`STALE_NUMERIC_EPOCH`). Permanent: the chapter id may now name a different
 * chapter, so the write must be dropped, never retried.
 */
export function isStaleNumericEpoch(err: unknown): boolean {
	return (
		err instanceof GraphQLRequestError &&
		err.errors.some((e) => e.extensions?.code === 'STALE_NUMERIC_EPOCH')
	);
}
