import type { PageLoad } from './$types';
import { SSR_PUBLIC } from '$lib/ssr';

// Public support/donation page — server-rendered at the edge on the web build. There is
// no data load any more: the copy is static module content (`$lib/data/content`), and the
// one interactive thing here — the report form — lives on /support/report. See $lib/ssr.
export const ssr = SSR_PUBLIC;

// Near-static, so it caches the longest of any route.
export const load: PageLoad = ({ setHeaders }) => {
	setHeaders({ 'cache-control': 'public, max-age=0, s-maxage=600' });
	return {};
};
