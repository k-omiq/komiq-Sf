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
			// Admin console is a web-only static SPA; authorization is enforced by the API.
			adapter: adapter({ fallback: 'index.html' }),
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
