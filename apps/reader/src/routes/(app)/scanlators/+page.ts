import type { PageLoad } from './$types';
import { parseScanlatorUrl } from '$lib/data/scanlators';
import { getScanlator, getScanlators } from '$lib/data/scanlators-repo';

// Client-only (the app's default, see routes/+layout.ts): every row carries the
// viewer's own score, so there is no anonymous page worth rendering at the edge.
//
// Both requests are handed over UNAWAITED, like the library's, so a filter chip or a
// pager step shows its loading state at once instead of holding the old page still.
// Neither rejects.
//
// `s` is the open row (`?s=<scanlator id>`). The page writes it with a shallow
// `replaceState`, which does not re-run this load, so `linked` is fetched for a real
// navigation only: a shared link, a notification, a reload. It is how a link opens a
// scanlator that the first page of the list does not hold.
export const load: PageLoad = ({ url, depends }) => {
	depends('komika:scanlators');
	const state = parseScanlatorUrl(url.searchParams);
	const open = url.searchParams.get('s');
	return {
		state,
		list: getScanlators(state),
		linked: open ? getScanlator(open) : null,
	};
};
