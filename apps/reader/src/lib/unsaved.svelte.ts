import { beforeNavigate } from '$app/navigation';

/** Protect drafts during client navigation and browser reload/close. */
export function protectUnsaved(isDirty: () => boolean, message: string) {
	beforeNavigate((navigation) => {
		if (!isDirty() || navigation.willUnload) return;
		if (!window.confirm(message)) navigation.cancel();
	});
	$effect(() => {
		if (!isDirty()) return;
		const unload = (event: BeforeUnloadEvent) => {
			event.preventDefault();
			event.returnValue = '';
		};
		window.addEventListener('beforeunload', unload);
		return () => window.removeEventListener('beforeunload', unload);
	});
}
