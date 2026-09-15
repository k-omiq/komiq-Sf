import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		sveltekit({
			compilerOptions: {
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true,
			},
			// Every page is prerendered to HTML: the docs are static files with no server.
			// `404.html` is what the edge serves for an unknown path (see wrangler.toml).
			adapter: adapter({ fallback: '404.html' }),
			prerender: {
				// A link to a page that does not exist fails the build instead of shipping.
				handleHttpError: 'fail',
				handleMissingId: 'fail',
			},
			csp: {
				mode: 'hash',
				directives: {
					'default-src': ['self'],
					'script-src': ['self'],
					// Highlighted code carries its token colours as inline style attributes.
					'style-src': ['self', 'unsafe-inline'],
					'img-src': ['self', 'data:'],
					'connect-src': ['self'],
					'font-src': ['self', 'data:'],
					'object-src': ['none'],
					'base-uri': ['none'],
					'form-action': ['none'],
				},
			},
		}),
	],
	server: {
		port: 5410,
		strictPort: true,
	},
});
