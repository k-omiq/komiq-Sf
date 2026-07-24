import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

/** `ADMIN_BASE_PATH`, validated: '' or '/segment' (no trailing slash). */
function adminBasePath(): '' | `/${string}` {
	const raw = (process.env.ADMIN_BASE_PATH ?? '').trim().replace(/\/+$/, '');
	if (raw && !raw.startsWith('/')) throw new Error(`ADMIN_BASE_PATH must start with "/": ${raw}`);
	return raw as '' | `/${string}`;
}

/**
 * `ADMIN_TARGET`: where this build runs. `hosted` (default) is the console at
 * admin.komiq.cc, unchanged. `embedded` is served by the API server itself
 * (`KOMIKA_WEB_DIR`), so the API is same-origin and a fresh server opens the setup
 * wizard. `desktop` runs in the desktop app, which picks its server at runtime
 * (`/start`).
 */
function adminTarget(): 'hosted' | 'embedded' | 'desktop' {
	const raw = (process.env.ADMIN_TARGET ?? 'hosted').trim();
	if (raw !== 'hosted' && raw !== 'embedded' && raw !== 'desktop') {
		throw new Error(`ADMIN_TARGET must be hosted, embedded or desktop: ${raw}`);
	}
	return raw;
}

export default defineConfig({
	define: {
		__ADMIN_TARGET__: JSON.stringify(adminTarget()),
	},
	plugins: [
		sveltekit({
			compilerOptions: {
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true,
			},
			// Admin console is a web-only static SPA; authorization is enforced by the API.
			adapter: adapter({ fallback: 'index.html' }),
			// Self-host builds are served by the API server under `/admin`
			// (`ADMIN_BASE_PATH=/admin`, see apps/server/src/web_dir.rs). Unset keeps the
			// hosted console at the domain root. Internal links go through `$lib/paths`.
			paths: { base: adminBasePath() },
			// The generated fallback HTML contains hashes for its bootstrap script.
			// This meta policy constrains the compatible edge header policy too.
			csp: {
				mode: 'hash',
				directives: {
					'default-src': ['self'],
					'script-src': ['self'],
					'style-src': ['self', 'unsafe-inline'],
					'img-src': ['self', 'https:', 'http:', 'data:'],
					'connect-src': ['self', 'https:', 'http:'],
					'font-src': ['self', 'data:'],
					'object-src': ['none'],
					'base-uri': ['none'],
					'form-action': ['self'],
				},
			},
		}),
	],
	server: {
		port: 5273,
		strictPort: true,
	},
});
