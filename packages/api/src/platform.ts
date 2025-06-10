/**
 * Runtime platform detection.
 *
 * Tauri v2 injects `__TAURI_INTERNALS__` on the window. When present we are
 * running inside a native shell (desktop or mobile) and can fetch source images
 * directly via a Rust command; otherwise we are a plain web build and must go
 * through the Cloudflare Worker proxy.
 */
export function isTauri(): boolean {
	return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

export type Platform = 'native' | 'web';

export function currentPlatform(): Platform {
	return isTauri() ? 'native' : 'web';
}
