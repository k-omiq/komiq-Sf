import type { WorkChapterEntry } from '@komika/api';

export function moveSortKey(
	entries: readonly WorkChapterEntry[],
	index: number,
	dir: -1 | 1,
): number | null {
	if (!Number.isInteger(index) || !entries[index]) return null;
	const neighbour = entries[index + dir];
	if (!neighbour) return null;
	const beyond = entries[index + dir * 2];
	const key = beyond ? (neighbour.sortKey + beyond.sortKey) / 2 : neighbour.sortKey + dir;
	if (!Number.isFinite(key) || key < -1000000 || key > 1000000) return null;
	// A rounded midpoint must not leave the entry tied with either neighbour.
	if (dir === -1) {
		if (key >= neighbour.sortKey || (beyond && key <= beyond.sortKey)) return null;
	} else if (key <= neighbour.sortKey || (beyond && key >= beyond.sortKey)) return null;
	return key;
}

export function entryLabel(entry: WorkChapterEntry): string {
	const label = entry.label || entry.number;
	return label ? `Ch. ${label}` : 'Special';
}

export function entryTargetLabel(entry: WorkChapterEntry): string {
	const title = entry.titleOverride ?? entry.copies[0]?.title ?? 'Untitled';
	return `${entryLabel(entry)} — ${title}`;
}

export function otherEntries(
	entries: readonly WorkChapterEntry[],
	entryId: string,
): WorkChapterEntry[] {
	return entries.filter((entry) => entry.id !== entryId);
}
