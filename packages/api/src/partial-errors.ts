/**
 * Which GraphQL errors a caller may read PAST.
 *
 * A GraphQL response can carry `data` AND `errors`: a resolver that fails on a NULLABLE
 * field nulls that one field and reports an error whose `path` names it, while every
 * sibling field resolves normally. The client used to throw on any `errors[]`, so one
 * failing optional enrichment (the reader's cross-source aggregation) discarded a bundle
 * whose series, spine and source lists had all loaded — and the reader rendered "Ch. 0".
 *
 * Tolerance is OPT-IN PER CALL and PER PATH, never global. Most errors are not partial at
 * all (validation, auth, a non-null field nulling `data`), and the ones that are still
 * usually mean the screen is wrong without that field. So a caller lists the exact field
 * paths it can render without, and a response passes only when:
 *   - `data` is present (a non-null failure nulls `data` and is never partial), and
 *   - EVERY error carries a `path` at or under one of those prefixes. An error with no
 *     path (validation, a transport-level failure) or anywhere else still throws;
 *   - the tolerated field's PARENT resolved — a null parent is not a partial result.
 *
 * Dependency-free so it is unit testable on its own; `graphql-backend.ts` owns the I/O.
 */

/** A response path segment: a field name, or an index into a list. */
export type PathSegment = string | number;

export interface GraphQLErrorLike {
	message: string;
	path?: readonly PathSegment[];
}

function startsWith(path: readonly PathSegment[], prefix: readonly PathSegment[]): boolean {
	return prefix.length <= path.length && prefix.every((seg, i) => path[i] === seg);
}

/**
 * Whether the PARENT of a tolerated field resolved. A tolerated field's error can still
 * null its parent — on a schema where the field is non-null but the parent nullable, null
 * propagation stops at the parent — and reading past that would hand the caller a null
 * object (a `seriesPage` of null reads as "not found") instead of reaching its retry.
 */
function parentResolved(data: unknown, prefix: readonly PathSegment[]): boolean {
	let value: unknown = data;
	for (const seg of prefix.slice(0, -1)) {
		if (value == null || typeof value !== 'object') return false;
		value = (value as Record<string | number, unknown>)[seg];
	}
	return value != null && typeof value === 'object';
}

export function tolerablePartialErrors(
	data: unknown,
	errors: readonly GraphQLErrorLike[] | undefined,
	tolerate: readonly (readonly PathSegment[])[] | undefined,
): boolean {
	if (data == null || !errors?.length || !tolerate?.length) return false;
	return errors.every(
		(e) =>
			!!e.path?.length && tolerate.some((p) => startsWith(e.path!, p) && parentResolved(data, p)),
	);
}

/**
 * A GraphQL-level failure that keeps the server's structured errors, so a caller can tell
 * WHICH field failed instead of string-matching a joined message. `message` stays the
 * joined text every existing `msg.includes(…)` check reads.
 */
export class GraphQLRequestError extends Error {
	readonly errors: readonly GraphQLErrorLike[];
	constructor(errors: readonly GraphQLErrorLike[]) {
		super(errors.map((e) => e.message).join('; '));
		this.name = 'GraphQLRequestError';
		this.errors = errors;
	}
}

/**
 * Whether a failure is confined to the given field paths even though it took `data` down
 * with it — the shape a NON-null field produces under null propagation. The error's `path`
 * still names the field that failed, which is what lets a caller retry without it.
 */
export function failedOnlyAt(
	err: unknown,
	paths: readonly (readonly PathSegment[])[],
): err is GraphQLRequestError {
	return (
		err instanceof GraphQLRequestError &&
		err.errors.length > 0 &&
		err.errors.every((e) => !!e.path?.length && paths.some((p) => startsWith(e.path!, p)))
	);
}
