export type LibrarySortKey = 'updated' | 'read' | 'added' | 'alpha' | 'unread';
interface LibrarySortRow {
	title: string;
	read: number;
	total: number;
	latestFoundAt: string;
	lastReadAt: string;
}

const collator = new Intl.Collator(undefined, { sensitivity: 'base', numeric: true });

/** Latest update follows our found clock; missing dates remain last in both directions. */
export function sortLibraryRows<T extends LibrarySortRow>(
	rows: readonly T[],
	key: LibrarySortKey,
	direction: 'asc' | 'desc',
): T[] {
	const sign = direction === 'asc' ? 1 : -1;
	const byTitle = (a: T, b: T) => collator.compare(a.title, b.title);
	if (key === 'added') return direction === 'desc' ? [...rows] : [...rows].reverse();
	if (key === 'alpha') return [...rows].sort((a, b) => sign * byTitle(a, b));
	if (key === 'unread') {
		const unread = (row: T) => Math.max(0, row.total - row.read);
		return [...rows].sort((a, b) => sign * (unread(a) - unread(b)) || byTitle(a, b));
	}
	const field = key === 'updated' ? 'latestFoundAt' : 'lastReadAt';
	return [...rows].sort((a, b) => {
		const x = Date.parse(a[field]);
		const y = Date.parse(b[field]);
		const xKnown = Number.isFinite(x);
		const yKnown = Number.isFinite(y);
		if (xKnown !== yKnown) return xKnown ? -1 : 1;
		return (xKnown && yKnown ? sign * (x - y) : 0) || byTitle(a, b);
	});
}
