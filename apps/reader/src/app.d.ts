// See https://svelte.dev/docs/kit/types#app.d.ts
// for information about these interfaces
declare global {
	// Build-time constant injected by Vite `define` (see vite.config.ts). True on
	// the hosted web (Cloudflare) build where public routes are server-rendered;
	// false on the Tauri/static SPA build.
	const __KOMIKA_SSR__: boolean;
	// Which build this is (vite.config.ts): 'web' = hosted Cloudflare, 'tauri' = the
	// static SPA (native apps, Docker), 'embedded' = the static SPA the API server
	// serves itself from KOMIKA_WEB_DIR.
	const __KOMIKA_TARGET__: 'web' | 'tauri' | 'embedded';

	namespace App {
		// interface Error {}
		// interface Locals {}
		// interface PageData {}
		// interface PageState {}
		// interface Platform {}
	}
}

export {};
