import type { PageLoad } from './$types';
import { getSupport } from '$lib/data/source';
import { SSR_PUBLIC } from '$lib/ssr';

// Public support/donation page — server-rendered at the edge on the web build.
// Content is near-static, so it caches the longest. See $lib/ssr.
export const ssr = SSR_PUBLIC;

export const load: PageLoad = ({ setHeaders }) => {
	setHeaders({ 'cache-control': 'public, max-age=0, s-maxage=600' });
	return getSupport();
};
