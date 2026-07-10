/** Publication metadata only: a translator's language does not determine layout. */
export interface ReaderFormatMetadata {
	type?: string | null;
	genres?: string[];
	metadata?: {
		originalLanguage?: string | null;
		tags?: { id?: string | null; name: string }[];
	} | null;
}

const STRIP_MARKERS = new Set([
	'longstrip',
	'verticalscroll',
	'webtoon',
	'webtoons',
	'manhwa',
	'manhua',
	'korean',
	'chinese',
]);
const STRIP_LANGUAGES = new Set(['ko', 'kor', 'kr', 'korean', 'zh', 'zho', 'chi', 'cn', 'chinese']);
const marker = (value: string) => value.toLowerCase().replace(/[^a-z0-9]/g, '');

/** Explicit long-strip taxonomy takes precedence over a generic MANGA type. */
export function isLongStripSeries(series?: ReaderFormatMetadata | null): boolean {
	if (!series) return false;
	const language = series.metadata?.originalLanguage?.toLowerCase().trim().split(/[-_]/)[0];
	if (language && STRIP_LANGUAGES.has(language)) return true;
	const labels = [
		series.type ?? '',
		...(series.genres ?? []),
		...(series.metadata?.tags ?? []).flatMap((tag) => [tag.name, tag.id ?? '']),
	];
	return labels.some((label) => STRIP_MARKERS.has(marker(label)));
}

/** Fraction through the chapter's scrollable image strip; excludes comments below it. */
export function stripProgress(top: number, height: number, viewportHeight: number): number {
	const distance = Math.max(0, height - viewportHeight);
	if (distance === 0) return top <= 0 ? 1 : 0;
	return Math.max(0, Math.min(1, -top / distance));
}
