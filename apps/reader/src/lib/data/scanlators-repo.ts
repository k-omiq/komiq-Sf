/**
 * The Scanlators page's requests. See `./scanlators` for the page's state and labels.
 *
 * The list needs the Komiq backend: a Suwayomi or mock deployment has no scanlator
 * ratings, and a Komiq server that predates them rejects the field. Both arrive as
 * `status: 'unsupported'`, which the page says in words. Any other failure is
 * `'error'`, with a retry.
 */
import type { RatedScanlator } from '@komika/api';
import { backend } from '$lib/context';
import { auth } from '$lib/auth.svelte';
import { socialLive } from './social-repo';
import { showFilter, type ScanlatorState } from './scanlators';

export interface ScanlatorList {
	status: 'ok' | 'unsupported' | 'error';
	items: RatedScanlator[];
	/** Echoed by the server; the requested page when there is no answer. */
	page: number;
	hasNext: boolean;
	total: number;
}

/** A server that predates the feature rejects the root field by name. */
function predates(err: unknown): boolean {
	const msg = err instanceof Error ? err.message : String(err);
	return /Unknown field "(scanlatorRatings|ratedScanlator|rateScanlator)"/.test(msg);
}

/** One page of the list. Never rejects. */
export async function getScanlators(state: ScanlatorState): Promise<ScanlatorList> {
	const none = { items: [], page: state.page, hasNext: false, total: 0 };
	if (!socialLive() || !backend.scanlatorRatings) return { status: 'unsupported', ...none };
	try {
		const res = await backend.scanlatorRatings({
			search: state.q,
			sort: state.sort,
			page: state.page,
			...showFilter(state.show),
		});
		return {
			status: 'ok',
			items: res.items,
			page: res.page,
			hasNext: res.hasNextPage,
			total: res.total,
		};
	} catch (err) {
		return { status: predates(err) ? 'unsupported' : 'error', ...none };
	}
}

/** One scanlator by id, for a link that opens a row the current page does not hold.
 *  Null when there is none, or when it could not be read. */
export async function getScanlator(id: string): Promise<RatedScanlator | null> {
	if (!socialLive() || !backend.ratedScanlator) return null;
	return backend.ratedScanlator(id).catch(() => null);
}

/** Set, change or (with `null`) withdraw the viewer's rating. Resolves to the row with
 *  the server's new numbers; throws when the write failed. */
export async function rateScanlator(id: string, score: number | null): Promise<RatedScanlator> {
	if (!socialLive() || !backend.rateScanlator) {
		throw new Error('Rating a scanlator needs a Komiq server.');
	}
	if (!auth.user) throw new Error('You must be signed in to rate.');
	return backend.rateScanlator(id, score);
}
