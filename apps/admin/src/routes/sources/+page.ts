import { href } from '$lib/paths';
import { redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';

// The three Sources tools used to be tabs of this one page (`?tab=…`). They are
// separate routes now; old links and bookmarks land on the matching one with the
// rest of their query string intact.
const TAB_ROUTES: Record<string, string> = {
	extensions: '/sources/extensions',
	catalogue: '/sources/browse',
	health: '/sources/health',
};

export const load: PageLoad = ({ url }) => {
	const params = new URLSearchParams(url.searchParams);
	const target = TAB_ROUTES[params.get('tab') ?? ''] ?? '/sources/extensions';
	params.delete('tab');
	const query = params.toString();
	redirect(307, href(target + (query ? `?${query}` : '')));
};
