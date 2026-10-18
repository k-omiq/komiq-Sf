/**
 * The Scanlators page: its URL, its links and its numbers.
 *
 * A scanlator is a SITE that translates its own series (a source) or a GROUP that
 * chapters credit (MangaDex credits groups). The server owns the list, the rating and
 * the rank (`scanlatorRatings`); this file only turns the address bar into a request
 * and a row into text.
 *
 * Dependency-free (no backend, no Svelte) so it is unit-testable on its own, like
 * `scanlator-ai.ts`; `./scanlators-repo` owns the fetch.
 */
import type { RatedScanlator, ScanlatorKind, ScanlatorSort } from '@komika/api';

/** Rows per page. Mirrors the server's page size for `scanlatorRatings`; the reader has
 *  no way to ask for it. Used for the pager's range text only. */
export const SCANLATOR_PAGE_SIZE = 30;

/** Ratings a scanlator needs before the server gives it a rank. Mirrors the server's
 *  constant; used for copy only ("Needs 5 to rank"), never to decide a rank. */
export const MIN_RATINGS_TO_RANK = 5;

/** What the list shows: everything, one kind, or what the viewer rated. */
export type ScanlatorShow = 'all' | 'site' | 'group' | 'mine';

export interface ScanlatorState {
	q: string;
	show: ScanlatorShow;
	sort: ScanlatorSort;
	page: number;
}

export const SHOW_LABEL: Record<ScanlatorShow, string> = {
	all: 'All',
	site: 'Sites',
	group: 'MangaDex groups',
	mine: 'Rated by me',
};

export const SORT_LABEL: Record<ScanlatorSort, string> = {
	TOP: 'Top rated',
	VOTES: 'Most ratings',
	NAME: 'A to Z',
};

const SHOWS: readonly ScanlatorShow[] = ['all', 'site', 'group', 'mine'];
const SORTS: readonly ScanlatorSort[] = ['TOP', 'VOTES', 'NAME'];

/**
 * The page's state from its address. Anything unknown is the default rather than an
 * error: the params are user-editable and shareable, like Browse's.
 */
export function parseScanlatorUrl(sp: URLSearchParams): ScanlatorState {
	const show = sp.get('show') as ScanlatorShow | null;
	const sort = sp.get('sort')?.toUpperCase() as ScanlatorSort | undefined;
	const page = Number(sp.get('page'));
	return {
		q: (sp.get('q') ?? '').trim(),
		show: show && SHOWS.includes(show) ? show : 'all',
		sort: sort && SORTS.includes(sort) ? sort : 'TOP',
		page: Number.isInteger(page) && page >= 1 && page <= 100_000 ? page : 1,
	};
}

/**
 * The address of a state, defaults left out so one list has one URL. `open` is the id
 * of the row that is open (`?s=`): a link to one scanlator.
 */
export function scanlatorsHref(state: Partial<ScanlatorState>, open?: string | null): string {
	const sp = new URLSearchParams();
	if (state.q?.trim()) sp.set('q', state.q.trim());
	if (state.show && state.show !== 'all') sp.set('show', state.show);
	if (state.sort && state.sort !== 'TOP') sp.set('sort', state.sort.toLowerCase());
	if (state.page && state.page > 1) sp.set('page', String(state.page));
	if (open) sp.set('s', open);
	const qs = sp.toString();
	return qs ? `/scanlators?${qs}` : '/scanlators';
}

/** The link to one scanlator: the list with that row open. */
export function scanlatorHref(id: string): string {
	return scanlatorsHref({}, id);
}

/** What `show` asks the server for. */
export function showFilter(show: ScanlatorShow): { kind?: ScanlatorKind; mine?: boolean } {
	if (show === 'site') return { kind: 'SITE' };
	if (show === 'group') return { kind: 'GROUP' };
	if (show === 'mine') return { mine: true };
	return {};
}

/** "Site · EN" or "MangaDex group": the kind in words, so colour and shape are never the
 *  only thing that tells the two apart. */
export function kindLabel(s: Pick<RatedScanlator, 'kind' | 'lang'>): string {
	if (s.kind === 'GROUP') return 'MangaDex group';
	return s.lang ? `Site · ${s.lang.toUpperCase()}` : 'Site';
}

/** "1 rating", "1,204 ratings". */
export function countLabel(n: number, one: string, many: string): string {
	return `${n.toLocaleString('en-US')} ${n === 1 ? one : many}`;
}

/** A source id for `AiBadge`, from a site's scanlator id (`site:<source id>`). */
export function siteSourceId(s: Pick<RatedScanlator, 'kind' | 'id'>): string | null {
	return s.kind === 'SITE' ? s.id.replace(/^site:/, '') : null;
}

/**
 * The row after the viewer rated it, before the server answers: their star moves at
 * once and the counts follow it. `score` null withdraws the rating. The average and
 * the rank are the server's to say, so they stay as they were until it answers.
 */
export function withMyScore(s: RatedScanlator, score: number | null): RatedScanlator {
	const distribution = [...s.distribution];
	if (s.myScore) distribution[s.myScore - 1] = Math.max(0, (distribution[s.myScore - 1] ?? 0) - 1);
	if (score) distribution[score - 1] = (distribution[score - 1] ?? 0) + 1;
	return {
		...s,
		myScore: score,
		distribution,
		ratingCount: s.ratingCount - (s.myScore ? 1 : 0) + (score ? 1 : 0),
	};
}
