import type { Backend } from '@komika/api';

type CoverBackend = Pick<Backend, 'series' | 'seriesPage' | 'canonicalSeries' | 'workSources'>;

/** Work IDs and numeric source IDs use different metadata endpoints. */
export async function fetchCoverSource(backend: CoverBackend, id: string): Promise<string> {
	if (!id.startsWith('w_')) return (await backend.series(id))?.coverUrl ?? '';
	const bundle = await backend.seriesPage?.(id, false);
	if (bundle !== undefined) {
		return (
			bundle?.series?.coverUrl ||
			bundle?.sources.find((s) => s.series?.coverUrl)?.series?.coverUrl ||
			''
		);
	}
	// Older adapters lack the bundle. Federation-only works have no MangaDex
	// series, so a missing canonical cover may still be supplied by a source.
	const canonical = await backend.canonicalSeries?.(id).catch(() => null);
	if (canonical?.coverUrl) return canonical.coverUrl;
	for (const source of (await backend.workSources?.(id)) ?? []) {
		if (source.sourceType !== 'suwayomi') continue;
		const series = await backend.series(source.sourceKey).catch(() => null);
		if (series?.coverUrl) return series.coverUrl;
	}
	return '';
}
