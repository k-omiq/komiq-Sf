/** A row of the cover ranking. `installed` is false for a ranked id that is gone. */
export interface RankSource {
	id: string;
	name: string;
	installed: boolean;
}

export const MANGADEX_ID = 'mangadex';

/** The default order: MangaDex first, then the installed sources by name. */
export function defaultSourceOrder(
	installed: readonly { id: string | number; name: string }[],
): RankSource[] {
	const byId = new Map<string, RankSource>();
	for (const s of installed) {
		const id = String(s.id);
		byId.set(id, { id, name: s.name, installed: true });
	}
	byId.delete(MANGADEX_ID);
	return [
		{ id: MANGADEX_ID, name: 'MangaDex', installed: true },
		...[...byId.values()].sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id)),
	];
}

/** The stored ranking first, in order, then every other source in default order. */
export function rankedOrder(
	stored: readonly string[],
	defaults: readonly RankSource[],
): RankSource[] {
	const byId = new Map(defaults.map((s) => [s.id, s]));
	const seen = new Set<string>();
	const out: RankSource[] = [];
	for (const id of stored) {
		if (seen.has(id)) continue;
		seen.add(id);
		out.push(byId.get(id) ?? { id, name: id, installed: false });
	}
	for (const s of defaults) if (!seen.has(s.id)) out.push(s);
	return out;
}

/**
 * The ranking to store for the order `ids`: everything down to the last source that is
 * ranked now or out of default order. The unranked sources after it, in default order,
 * stay unranked, so each series keeps the server's own order for them (MangaDex, then
 * the source it was added from first) instead of a ranking nobody chose.
 */
export function rankToSave(
	ids: readonly string[],
	defaults: readonly RankSource[],
	stored: readonly string[],
): string[] {
	const position = new Map(defaults.map((s, i) => [s.id, i]));
	const ranked = new Set(stored);
	let keep = ids.length;
	while (keep > 0) {
		const id = ids[keep - 1];
		const at = position.get(id);
		if (at === undefined || ranked.has(id)) break;
		const next = keep < ids.length ? position.get(ids[keep]) : undefined;
		if (next !== undefined && at > next) break;
		keep -= 1;
	}
	return ids.slice(0, keep);
}

/** A copy of `list` with the item at `from` moved to `to`. Out-of-range moves return a copy. */
export function moveItem<T>(list: readonly T[], from: number, to: number): T[] {
	const out = [...list];
	if (from < 0 || from >= out.length || to < 0 || to >= out.length || from === to) return out;
	const [item] = out.splice(from, 1);
	out.splice(to, 0, item);
	return out;
}

export function sameOrder(a: readonly string[], b: readonly string[]): boolean {
	return a.length === b.length && a.every((id, i) => id === b[i]);
}

const count = new Intl.NumberFormat('en-US');
const decimal = new Intl.NumberFormat('en-US', { maximumFractionDigits: 1 });

export function formatCount(n: number): string {
	return count.format(n);
}

/** Bytes in decimal units: "512 B", "38.2 MB". */
export function formatBytes(bytes: number): string {
	if (!Number.isFinite(bytes) || bytes < 1000) return `${Math.max(0, Math.round(bytes || 0))} B`;
	const units = ['kB', 'MB', 'GB', 'TB'];
	let value = bytes;
	let unit = -1;
	while (value >= 1000 && unit < units.length - 1) {
		value /= 1000;
		unit += 1;
	}
	return `${decimal.format(value)} ${units[unit]}`;
}

/** "1,204 covers, 38.2 MB". */
export function coverAmount(covers: number, bytes: number): string {
	return `${formatCount(covers)} cover${covers === 1 ? '' : 's'}, ${formatBytes(bytes)}`;
}
