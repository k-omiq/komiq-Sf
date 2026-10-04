/**
 * The desktop app's move to a server's new HTTPS address (`followServer`), and a move
 * it could not make. Shared, because the screen that starts a move is not always the
 * one that reports it: the setup wizard is on a later step by then. Kept for this run
 * of the app only. `HttpsMove.svelte` shows it.
 */
import { followServer } from './config';

export const move = $state<{
	/**
	 * The new address did not answer. Another try goes to this address, with the host
	 * the owner typed, and not to an address a later answer names.
	 */
	stuck: { address: string; typed: string } | null;
	trying: boolean;
}>({ stuck: null, trying: false });

let latest = 0;

/**
 * Follow the server to `address`, and keep the move when it did not answer there.
 * `beforeReload`: see `followServer`.
 */
export async function moveTo(
	address: string,
	typed: string,
	beforeReload?: () => void,
): Promise<void> {
	const id = ++latest;
	move.trying = true;
	// A newer move takes over when the owner saves another address meanwhile.
	const answered = await followServer(address, typed, beforeReload, () => id === latest);
	if (id !== latest) return;
	move.trying = false;
	move.stuck = answered ? null : { address, typed };
}
