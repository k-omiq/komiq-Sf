import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { EngineGeneration } from '@komika/api';
import {
	engineGenerationView,
	formatSince,
	generationToReplace,
	isUnknownEngineStateField,
} from './engine-generation.ts';

function generation(overrides: Partial<EngineGeneration> = {}): EngineGeneration {
	return {
		id: 'current',
		state: 'active',
		reason: null,
		since: '2026-10-16 12:11:06.123456+00',
		unfenced: false,
		...overrides,
	};
}

test('null means this server does not report the state', () => {
	assert.deepEqual(engineGenerationView(null), {
		kind: 'unsupported',
		current: null,
		detached: [],
		message: 'This server does not report the engine state.',
	});
});

test('an empty list means no engine has been recorded', () => {
	assert.deepEqual(engineGenerationView([]), {
		kind: 'none',
		current: null,
		detached: [],
		message: 'No source engine has been recorded yet.',
	});
});

test('active retains the current row and formats its state and since', () => {
	const row = generation({ reason: 'Reattached after marker loss' });
	const view = engineGenerationView([row]);
	assert.equal(view.kind, 'active');
	if (view.kind !== 'active') throw new Error('Expected active');
	assert.deepEqual(view.current, {
		...row,
		stateLabel: 'Active',
		sinceLabel: formatSince(row.since),
	});
	assert.equal(view.note, null);
	assert.deepEqual(view.detached, []);
});

test('active without marker support explains the remaining identity check', () => {
	const view = engineGenerationView([generation({ unfenced: true })]);
	assert.equal(view.kind, 'active');
	if (view.kind !== 'active') throw new Error('Expected active');
	assert.equal(view.current.unfenced, true);
	assert.equal(
		view.note,
		'This engine cannot store the generation marker, so the server checks only stored source identities.',
	);
});

test('quarantined retains the exact server reason', () => {
	for (const reason of [
		'Stored numeric ids answer different source identities',
		'The engine is empty but PostgreSQL still has Suwayomi references',
		'The engine generation marker is missing or different',
	]) {
		const view = engineGenerationView([generation({ state: 'quarantined', reason })]);
		assert.equal(view.kind, 'quarantined');
		assert.equal(view.current?.stateLabel, 'Quarantined');
		assert.equal(view.current?.reason, reason);
	}
});

test('quarantine history is detached only, newest first, without changing the input', () => {
	const old = generation({
		id: 'old',
		state: 'detached',
		since: '2026-10-15 14:00:00+02',
		reason: 'Operator started a new engine generation',
	});
	const newer = generation({
		id: 'newer',
		state: 'detached',
		since: '2026-10-15 13:00:00+00',
	});
	const current = generation({ state: 'quarantined' });
	const rows = [old, newer, current];
	const snapshot = structuredClone(rows);
	const view = engineGenerationView(rows);
	assert.equal(view.kind, 'quarantined');
	assert.equal(view.current?.id, 'current');
	assert.deepEqual(
		view.detached.map((row) => row.id),
		['newer', 'old'],
	);
	assert.equal(view.detached[1].reason, old.reason);
	assert.equal(view.detached[0].stateLabel, 'Detached');
	assert.equal(view.detached[0].sinceLabel, formatSince(newer.since));
	assert.deepEqual(rows, snapshot);
	// History sorting also holds if the supplied rows are not in server order.
	assert.deepEqual(engineGenerationView([newer, old, current]).detached, view.detached);
});

test('detached-only rows mean no current engine while retaining history', () => {
	const view = engineGenerationView([
		generation({ id: 'old', state: 'detached', since: '2026-10-14 12:00:00+00' }),
		generation({ id: 'newer', state: 'detached' }),
	]);
	assert.equal(view.kind, 'none');
	assert.equal(view.current, null);
	assert.deepEqual(
		view.detached.map((row) => row.id),
		['newer', 'old'],
	);
});

test('future server states remain visible without quarantine recovery actions', () => {
	const view = engineGenerationView([generation({ state: 'checking' })]);
	assert.equal(view.kind, 'active');
	assert.equal(view.current?.state, 'checking');
	assert.equal(view.current?.stateLabel, 'Checking');
});

test('formatSince normalizes PostgreSQL timestamps to local date-time', () => {
	const expected = new Date('2026-10-16T12:11:06.123Z').toLocaleString();
	assert.equal(formatSince('2026-10-16 12:11:06.123456+00'), expected);
	assert.equal(formatSince('2026-10-16 14:11:06.123456+02'), expected);
	assert.equal(formatSince('2026-10-16 07:11:06.123456-05:00'), expected);
	assert.equal(formatSince('2026-10-16T12:11:06.123Z'), expected);
});

test('formatSince falls back to the untouched server text', () => {
	assert.equal(formatSince('not a timestamp'), 'not a timestamp');
	assert.equal(formatSince('infinity'), 'infinity');
	assert.equal(formatSince(''), '');
});

test('only the unknown engine-state field is treated as unsupported', () => {
	const message = 'Unknown field "engineGenerationState" on type "Query"';
	assert.equal(isUnknownEngineStateField(new Error(message)), true);
	assert.equal(isUnknownEngineStateField(message), true);
	for (const err of [
		new Error('Unknown field "startNewEngineGeneration"'),
		new Error('Unknown field "engineGenerationStateExtra"'),
		new Error('Unauthorized'),
		new Error('The source engine is not reachable.'),
		'connection failed',
		null,
		undefined,
		{},
		42,
	]) {
		assert.equal(isUnknownEngineStateField(err), false);
	}
});

test('only the quarantined generation on screen can be replaced', () => {
	assert.equal(
		generationToReplace(engineGenerationView([generation({ id: 'shown', state: 'quarantined' })])),
		'shown',
	);
	assert.equal(generationToReplace(engineGenerationView([generation({ id: 'shown' })])), null);
	assert.equal(generationToReplace(engineGenerationView([])), null);
	assert.equal(generationToReplace(engineGenerationView(null)), null);
});
