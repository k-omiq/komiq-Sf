import { isTauri } from '@komika/api';

/**
 * The native app locks page zoom (static/theme-init.js): pinching must not scale the
 * whole app. Chapter pages are the exception, where zooming in on small text is part
 * of reading. Call on the reader's mount; the returned function locks zoom again.
 * Zooming out below 100% stays off. A browser is left alone: it keeps its own zoom.
 */
export function allowPageZoom(): () => void {
	if (!isTauri()) return () => {};
	const viewport = document.querySelector<HTMLMetaElement>('meta[name="viewport"]');
	if (!viewport) return () => {};
	const locked = viewport.content;
	viewport.content = locked
		.replace(/maximum-scale=[^,]+/, 'maximum-scale=4')
		.replace(/user-scalable=[^,]+/, 'user-scalable=yes');
	return () => {
		viewport.content = locked;
	};
}

/**
 * Show or hide the system status bar (clock, battery) in the native app. Only iOS acts
 * on it; every other platform, and a browser, does nothing. Failures are ignored: the
 * bar staying visible is not worth an error on screen.
 */
export function setStatusBarHidden(hidden: boolean): void {
	if (!isTauri()) return;
	void import('@tauri-apps/api/core')
		.then(({ invoke }) => invoke('set_status_bar_hidden', { hidden }))
		.catch(() => {});
}
