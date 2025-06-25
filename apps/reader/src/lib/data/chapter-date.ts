const FOUND_WINDOW_MS = 30 * 24 * 60 * 60 * 1000;

/** Format a chapter timestamp as readable relative prose, then an absolute month/year. */
export function chapterDateLabel(
	iso: string | null | undefined,
	now = Date.now(),
	locales?: Intl.LocalesArgument,
): string {
	if (!iso) return '';
	const then = Date.parse(iso);
	if (Number.isNaN(then)) return '';

	const elapsed = Math.max(0, now - then);
	const minutes = Math.floor(elapsed / 60_000);
	if (minutes < 1) return 'just now';
	if (minutes < 60) return `${minutes} ${minutes === 1 ? 'minute' : 'minutes'} ago`;

	const hours = Math.floor(elapsed / 3_600_000);
	if (hours < 24) return `${hours} ${hours === 1 ? 'hour' : 'hours'} ago`;

	const days = Math.floor(elapsed / 86_400_000);
	if (days < 30) return `${days} ${days === 1 ? 'day' : 'days'} ago`;

	return new Date(then).toLocaleString(locales, { month: 'long', year: 'numeric' });
}

/**
 * Format the discovery time only for a chapter found in the last 30 days or one
 * of the three newest chapters. Missing timestamps never become placeholders.
 */
export function chapterFoundLabel(
	foundAt: string | null | undefined,
	newest: boolean,
	now = Date.now(),
	locales?: Intl.LocalesArgument,
): string {
	if (!foundAt) return '';
	const found = Date.parse(foundAt);
	if (Number.isNaN(found)) return '';
	if (!newest && now - found > FOUND_WINDOW_MS) return '';
	return chapterDateLabel(foundAt, now, locales);
}
