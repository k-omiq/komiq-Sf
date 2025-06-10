import type { PageLoad } from './$types';
import { getReaderChapter } from '$lib/data/source';

export const load: PageLoad = ({ params, url }) =>
	getReaderChapter(params.slug, url.searchParams.get('ch'), url.searchParams.get('src'));
