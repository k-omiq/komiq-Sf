// See https://svelte.dev/docs/kit/types#app.d.ts
// for information about these interfaces
declare global {
	/** Build target, from `ADMIN_TARGET` (see vite.config.ts). */
	const __ADMIN_TARGET__: 'hosted' | 'embedded' | 'desktop';

	namespace App {
		// interface Error {}
		// interface Locals {}
		// interface PageData {}
		// interface PageState {}
		// interface Platform {}
	}
}

export {};
