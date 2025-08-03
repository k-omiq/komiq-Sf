import { defineConfig } from '@playwright/test';

/** Production reader regression checks; start the built preview first. */
export default defineConfig({
	testDir: './tests',
	testMatch: [
		'latest-added-consistency.spec.ts',
		'metadata-compatibility.spec.ts',
		'reader-csp.spec.ts',
	],
	timeout: 60_000,
	workers: 1,
	reporter: 'list',
	use: {
		baseURL: process.env.PW_BASE_URL || 'http://127.0.0.1:4173',
		channel: 'chrome',
		screenshot: 'only-on-failure',
	},
});
