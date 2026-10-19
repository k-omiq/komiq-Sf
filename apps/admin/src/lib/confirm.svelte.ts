/**
 * Promise-based confirmation, rendered by `<ConfirmHost />` in the root layout.
 * Replaces `window.confirm()` so every destructive action asks the same way.
 */
export interface ConfirmOptions {
	title: string;
	/** What will happen, in plain words. */
	message: string;
	/** Label of the confirming button. Name the action, not "OK". */
	confirmLabel?: string;
	cancelLabel?: string;
	/** Style the confirming button as destructive. */
	danger?: boolean;
	/** Offer "Don't ask again this session". Confirms that share a key are skipped together. */
	skipKey?: string;
}

interface Pending extends ConfirmOptions {
	resolve: (ok: boolean) => void;
}

export const confirmState = $state<{ current: Pending | null }>({ current: null });

/** Skip keys the admin opted out of. Memory only, so a reload or restart asks again. */
const skipped = new Set<string>();

/** True when the admin chose not to be asked again for this key. */
export function confirmSkipped(key: string): boolean {
	return skipped.has(key);
}

/** Stop asking for this key until the app is reloaded. */
export function skipConfirm(key: string): void {
	skipped.add(key);
}

/**
 * Ask the admin to confirm. Resolves `false` on cancel, Escape or scrim click, and
 * `true` at once when the admin already chose not to be asked again for `skipKey`.
 */
export function confirm(options: ConfirmOptions): Promise<boolean> {
	if (options.skipKey && skipped.has(options.skipKey)) return Promise.resolve(true);
	// A second request while one is open cancels the first rather than stacking.
	confirmState.current?.resolve(false);
	return new Promise((resolve) => {
		confirmState.current = { ...options, resolve };
	});
}

/** `dontAskAgain` only counts on a confirm, never on a cancel. */
export function settleConfirm(ok: boolean, dontAskAgain = false): void {
	const pending = confirmState.current;
	confirmState.current = null;
	if (ok && dontAskAgain && pending?.skipKey) skipped.add(pending.skipKey);
	pending?.resolve(ok);
}
