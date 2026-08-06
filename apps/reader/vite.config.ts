import adapterStatic from '@sveltejs/adapter-static';
import adapterCloudflare from '@sveltejs/adapter-cloudflare';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig, loadEnv } from 'vite';
import { readerConnectSources } from './csp';

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
//   • KOMIKA_TARGET=embedded → the same static SPA, built to be served BY the API
//     server itself (`KOMIKA_WEB_DIR`, reader at `/`): it talks to `/graphql` on its
//     own origin and loads images from there, whatever PUBLIC_* the build env holds
//     (see $lib/config). One build works on any address the server is reached at.
const isEmbeddedBuild = process.env.KOMIKA_TARGET === 'embedded';
const isTauriBuild =
	!!process.env.TAURI_ENV_PLATFORM || process.env.KOMIKA_TARGET === 'tauri' || isEmbeddedBuild;
const target = isEmbeddedBuild ? 'embedded' : isTauriBuild ? 'tauri' : 'web';
// `__KOMIKA_SSR__` gates the per-route `ssr` flags (see $lib/ssr): public pages
// SSR on the web build, but stay client-only in the Tauri/static SPA build.
const ssrEnabled = !isTauriBuild;

export default defineConfig(({ command, mode, isPreview }) => {
	const publicEnv = loadEnv(mode, process.cwd(), 'PUBLIC_');
	const connectSources = isTauriBuild
		? []
		: readerConnectSources(publicEnv, command === 'serve' && !isPreview);
	return {
		define: {
			__KOMIKA_SSR__: JSON.stringify(ssrEnabled),
			__KOMIKA_TARGET__: JSON.stringify(target),
		},
		plugins: [
			sveltekit({
				// Inline every stylesheet at or under this size into the <head> instead of
				// linking it. SvelteKit code-splits CSS per component and per route, so the
				// home page shipped EIGHT render-blocking <link rel="stylesheet"> requests
				// (layout, route, MangaCard, Cover, Avatar, FlagBadge, CardRowSkeleton …) —
				// Lighthouse measured 500 ms of blocked render for 14.7 KiB of transfer, i.e.
				// almost all latency and almost no bytes.
				//
				// 12 KiB is chosen off the built output: it covers every file on the public
				// pages' critical path (the largest is the root layout at ~11 KiB raw, which
				// gzips to ~5.5 KiB in the document) while leaving the two heavy route-only
				// sheets — the reader at 26 KiB and one admin-ish route at 13 KiB — external,
				// where they are fetched once and cached across navigations rather than
				// re-sent inside every SSR'd HTML response.
				inlineStyleThreshold: 12 * 1024,
				// Hash bootstrap scripts on SSR and prerendered pages. Public SSR HTML
				// is cached, so per-request nonces would be reused by the cache.
				// Native builds have a separately configured WebView CSP.
				csp: isTauriBuild
					? undefined
					: {
							mode: 'hash',
							directives: {
								'default-src': ['self'],
								'script-src': ['self'],
								'style-src': ['self', 'unsafe-inline'],
								'img-src': ['self', 'https:', 'http:', 'data:', 'blob:'],
								'connect-src': connectSources,
								'font-src': ['self', 'data:'],
								'object-src': ['none'],
								'base-uri': ['self'],
								'form-action': ['self'],
							},
						},
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
	};
});
