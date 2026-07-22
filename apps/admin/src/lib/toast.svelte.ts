/**
 * Transient feedback for actions that succeeded or failed, rendered by
 * `<Toaster />` in the root layout. Errors the admin must act on (a failed page
 * load) stay inline as a notice; toasts are for "done" and "that didn't work".
 */
export type ToastTone = 'success' | 'danger' | 'info';

export interface Toast {
	id: number;
	tone: ToastTone;
	message: string;
}

export const toasts = $state<{ items: Toast[] }>({ items: [] });

let nextId = 1;

export function dismissToast(id: number): void {
	toasts.items = toasts.items.filter((t) => t.id !== id);
}

function push(tone: ToastTone, message: string): void {
	const id = nextId++;
	toasts.items = [...toasts.items.slice(-3), { id, tone, message }];
	// Errors stay longer: they usually need reading twice.
	setTimeout(() => dismissToast(id), tone === 'danger' ? 8000 : 4500);
}

export const toast = {
	success: (message: string) => push('success', message),
	error: (message: string) => push('danger', message),
	info: (message: string) => push('info', message),
	/** Report a caught error with a fallback for non-Error throws. */
	fromError: (err: unknown, fallback: string) =>
		push('danger', err instanceof Error && err.message ? err.message : fallback),
};
