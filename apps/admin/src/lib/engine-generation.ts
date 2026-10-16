import type { EngineGeneration } from '@komika/api';

export type EngineGenerationRow = EngineGeneration & {
	stateLabel: string;
	sinceLabel: string;
};

type History = { detached: EngineGenerationRow[] };

export type EngineGenerationView = History &
	(
		| { kind: 'unsupported'; current: null; message: string }
		| { kind: 'none'; current: null; message: string }
		| { kind: 'active'; current: EngineGenerationRow; note: string | null }
		| { kind: 'quarantined'; current: EngineGenerationRow }
	);

function parseSince(text: string): Date {
	// PostgreSQL uses a space, microseconds and sometimes an hour-only UTC offset.
	const normalized = text
		.replace(/^(\d{4}-\d{2}-\d{2}) /, '$1T')
		.replace(/(\.\d{3})\d+/, '$1')
		.replace(/([+-]\d{2})$/, '$1:00');
	return new Date(normalized);
}

/** A local date and time, or the untouched server text if it cannot be parsed. */
export function formatSince(text: string): string {
	const date = parseSince(text);
	return Number.isNaN(date.getTime()) ? text : date.toLocaleString();
}

function rowView(row: EngineGeneration): EngineGenerationRow {
	return {
		...row,
		stateLabel: row.state.charAt(0).toUpperCase() + row.state.slice(1),
		sinceLabel: formatSince(row.since),
	};
}

export function engineGenerationView(rows: EngineGeneration[] | null): EngineGenerationView {
	if (rows === null) {
		return {
			kind: 'unsupported',
			current: null,
			detached: [],
			message: 'This server does not report the engine state.',
		};
	}

	// Reverse the server's oldest-first order, preserving that order for invalid dates
	// and ties. Compare timestamps as well so offsets do not change the history order.
	const newestFirst = [...rows].reverse();
	const detached = newestFirst
		.filter((row) => row.state === 'detached')
		.sort((a, b) => parseSince(b.since).getTime() - parseSince(a.since).getTime())
		.map(rowView);
	const current = newestFirst.find((row) => row.state !== 'detached');
	if (!current) {
		return {
			kind: 'none',
			current: null,
			detached,
			message: 'No source engine has been recorded yet.',
		};
	}
	if (current.state === 'quarantined') {
		return { kind: 'quarantined', current: rowView(current), detached };
	}
	// Keep future server states visible, without offering quarantine recovery actions.
	return {
		kind: 'active',
		current: rowView(current),
		detached,
		note: current.unfenced
			? 'This engine cannot store the generation marker, so the server checks only stored source identities.'
			: null,
	};
}

/** Only this schema error means the server predates engine-state reporting. */
export function isUnknownEngineStateField(err: unknown): boolean {
	const message = err instanceof Error ? err.message : typeof err === 'string' ? err : '';
	return message.includes('Unknown field "engineGenerationState"');
}

/** The generation a confirmed "start a new generation" gives up: only the quarantined one
 *  on screen. The server refuses the id if the state changed meanwhile. */
export function generationToReplace(view: EngineGenerationView): string | null {
	return view.kind === 'quarantined' ? view.current.id : null;
}
