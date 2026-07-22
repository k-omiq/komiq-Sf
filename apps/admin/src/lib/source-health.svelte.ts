/**
 * How many sources are down right now, for the sidebar badge. Loaded by the
 * shell so the alarm shows on every page, not only after the Health page has
 * been opened. Best-effort: a failed check leaves the last known count.
 */
import { loadSourceScanHealth } from './data';

export const sourceHealth = $state<{ down: number }>({ down: 0 });

export async function refreshSourcesDown(): Promise<void> {
	try {
		const rows = await loadSourceScanHealth();
		sourceHealth.down = rows.filter((h) => h.outageDetectedAt).length;
	} catch {
		/* keep the last known count */
	}
}
