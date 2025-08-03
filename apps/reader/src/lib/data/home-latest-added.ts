import type { Backend } from '@komika/api';
import type { Series } from '@komika/types';

/** Home previews the first ten works of Browse's unfiltered Latest Added page.
 * Discovery's RECENTLY_ADDED rail on older APIs uses source intake timestamps,
 * so it cannot substitute for this query without changing the user's ordering.
 */
export async function homeLatestAdded(backend: Pick<Backend, 'search'>): Promise<Series[]> {
	const result = await backend.search('', 1, { sort: 'LATEST_ADDED' });
	// Preserve server identity and order, including distinct works sharing a title.
	return result.items.slice(0, 10);
}
