import adapterStatic from '@sveltejs/adapter-static';
import adapterCloudflare from '@sveltejs/adapter-cloudflare';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// Tauri sets this when developing against a physical mobile device.
const host = process.env.TAURI_DEV_HOST;

// --- Build target selection -------------------------------------------------
// The reader ships two ways from ONE codebase:
//   • Tauri (desktop + mobile) and any server-less static host → adapter-static,
//     a pure SPA (`fallback: index.html`, no SSR runtime = no server surface).
//   • Hosted web on Cloudflare → adapter-cloudflare, so the public catalog pages
//     are server-rendered at the edge for fresh content + fast first paint.
// Tauri sets TAURI_ENV_PLATFORM during its build; a plain `pnpm build` targets
// the web (Cloudflare). Set KOMIKA_TARGET=tauri to force the static SPA build
// (e.g. building the web bundle for a static host without Tauri's env present).
const isTauriBuild = !!process.env.TAURI_ENV_PLATFORM || process.env.KOMIKA_TARGET === 'tauri';
// `__KOMIKA_SSR__` gates the per-route `ssr` flags (see $lib/ssr): public pages
// SSR on the web build, but stay client-only in the Tauri/static SPA build.
const ssrEnabled = !isTauriBuild;

export default defineConfig({
	define: {
		__KOMIKA_SSR__: JSON.stringify(ssrEnabled),
	},
	plugins: [
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true,
			},
			// Web build → Cloudflare (edge SSR for public routes). Tauri/static build
			// → static SPA with an index.html fallback so client routing resolves for
			// every path.
			adapter: isTauriBuild ? adapterStatic({ fallback: 'index.html' }) : adapterCloudflare(),
		}),
	],
	// --- Tauri-friendly dev server ---
	clearScreen: false,
	server: {
		// Honor a harness-assigned PORT (e.g. when 5173 is taken by another dev
		// server); fall back to the fixed 5173 the Tauri workflow expects. Only be
		// strict about the port when using that fixed default — an assigned port is
		// already known-free, so allow Vite to fall forward if it somehow isn't.
		port: process.env.PORT ? Number(process.env.PORT) : 5173,
		strictPort: !process.env.PORT,
		host: host || false,
		hmr: host ? { protocol: 'ws', host, port: 5174 } : undefined,
		watch: {
			// Vite shouldn't watch the Rust side.
			ignored: ['**/src-tauri/**'],
		},
	},
});
