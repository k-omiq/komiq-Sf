import { backend } from '$lib/context';
import { config, target } from '$lib/config';

/**
 * Whether the server holds no series at all — a new self-hosted install before its
 * owner adds a source — as opposed to feeds that came back empty because something
 * failed (every home feed resolves to empty on error). Only a successful catalogue
 * query with zero results counts; any failure answers false, so the caller keeps its
 * "unavailable" wording.
 *
 * The hosted web build never asks: its catalogue is never empty, and its home page is
 * server-rendered, so it keeps exactly the markup it always had.
 */
export const checksCatalogEmpty = target !== 'web' && config.backendKind === 'komika';

export async function catalogIsEmpty(): Promise<boolean> {
	if (!checksCatalogEmpty) return false;
	try {
		const first = await backend.search('', 1);
		return first.items.length === 0 && !first.hasNextPage;
	} catch {
		return false;
	}
}
