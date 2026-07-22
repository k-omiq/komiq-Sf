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
}

interface Pending extends ConfirmOptions {
	resolve: (ok: boolean) => void;
}

export const confirmState = $state<{ current: Pending | null }>({ current: null });

/** Ask the admin to confirm. Resolves `false` on cancel, Escape or scrim click. */
export function confirm(options: ConfirmOptions): Promise<boolean> {
	// A second request while one is open cancels the first rather than stacking.
	confirmState.current?.resolve(false);
	return new Promise((resolve) => {
		confirmState.current = { ...options, resolve };
	});
}

export function settleConfirm(ok: boolean): void {
	const pending = confirmState.current;
	confirmState.current = null;
	pending?.resolve(ok);
}
