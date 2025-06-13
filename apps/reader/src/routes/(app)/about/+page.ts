import type { PageLoad } from './$types';
import { SSR_PUBLIC } from '$lib/ssr';

// Public, entirely static copy — server-rendered at the edge on the web build (it is the
// page rights holders and search engines land on, so it must exist in the HTML), and the
// longest-cached route in the app alongside /support. See $lib/ssr.
export const ssr = SSR_PUBLIC;

export const load: PageLoad = ({ setHeaders }) => {
	setHeaders({ 'cache-control': 'public, max-age=0, s-maxage=600' });
	return {};
};
