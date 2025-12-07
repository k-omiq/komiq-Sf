/**
 * Run: node --test --experimental-strip-types src/lib/data/graphql-partial.test.ts
 *
 * Guards the shared client's partial-result rule (`packages/api/src/partial-errors.ts`):
 * a failing NULLABLE field may be read past only where the caller opted in, by path. The
 * regression it exists for: one failing `seriesPage.aggregated` used to throw away a bundle
 * whose series, spine and source lists had all loaded, and the reader rendered "Ch. 0".
 * The test lives here because this is the workspace's node test glob; the module itself is
 * dependency-free.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
	failedOnlyAt,
	GraphQLRequestError,
	tolerablePartialErrors,
} from '../../../../../packages/api/src/partial-errors.ts';

const AGG = ['seriesPage', 'aggregated'] as const;
const bundle = { seriesPage: { workId: 'w_1', aggregated: null } };

test('an error confined to a tolerated field returns the data', () => {
	const errors = [{ message: 'database is locked', path: ['seriesPage', 'aggregated'] }];
	assert.equal(tolerablePartialErrors(bundle, errors, [AGG]), true);
});

test('an error UNDER a tolerated field (a list item inside it) is tolerated too', () => {
	const errors = [{ message: 'x', path: ['seriesPage', 'aggregated', 3, 'title'] }];
	assert.equal(tolerablePartialErrors(bundle, errors, [AGG]), true);
});

test('any error outside the tolerated paths still throws', () => {
	const errors = [
		{ message: 'x', path: ['seriesPage', 'aggregated'] },
		{ message: 'y', path: ['seriesPage', 'spineChapters'] },
	];
	assert.equal(tolerablePartialErrors(bundle, errors, [AGG]), false);
	// A sibling whose name merely STARTS like the tolerated one is not under it.
	const lookalike = [{ message: 'z', path: ['seriesPage', 'aggregatedX'] }];
	assert.equal(tolerablePartialErrors(bundle, lookalike, [AGG]), false);
});

test('validation and transport errors carry no path and always throw', () => {
	const errors = [{ message: 'Unknown argument "filter"' }];
	assert.equal(tolerablePartialErrors(bundle, errors, [AGG]), false);
});

test('a null `data` is never partial, and opting out (no paths) keeps the old throw', () => {
	const errors = [{ message: 'x', path: ['seriesPage', 'aggregated'] }];
	assert.equal(tolerablePartialErrors(null, errors, [AGG]), false);
	assert.equal(tolerablePartialErrors(bundle, errors, undefined), false);
	assert.equal(tolerablePartialErrors(bundle, errors, []), false);
});

test('a tolerated error that nulled the PARENT is not partial (so the caller can retry)', () => {
	// A schema with `seriesPage` nullable but `aggregated` non-null: the aggregation error
	// stops null propagation at `seriesPage`, and `data.seriesPage` is null. Reading past
	// that would hand the reader a null bundle ("not found") instead of its retry.
	const errors = [{ message: 'boom', path: ['seriesPage', 'aggregated'] }];
	assert.equal(tolerablePartialErrors({ seriesPage: null }, errors, [AGG]), false);
	assert.equal(tolerablePartialErrors({}, errors, [AGG]), false);
	assert.equal(tolerablePartialErrors({ seriesPage: { workId: 'w_1' } }, errors, [AGG]), true);
});

test('failedOnlyAt recognises a non-null field that nulled the whole response', () => {
	// What a server that still types `aggregated` non-null answers: data null, but the
	// error's path names the field — enough for the client to retry without it.
	const err = new GraphQLRequestError([{ message: 'boom', path: ['seriesPage', 'aggregated'] }]);
	assert.equal(err.message, 'boom');
	assert.equal(failedOnlyAt(err, [AGG]), true);
	assert.equal(
		failedOnlyAt(new GraphQLRequestError([{ message: 'No such work', path: ['seriesPage'] }]), [
			AGG,
		]),
		false,
	);
	assert.equal(failedOnlyAt(new Error('Backend error 500'), [AGG]), false);
});
